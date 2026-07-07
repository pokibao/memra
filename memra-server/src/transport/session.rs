//! HTTP session-id pass-through middleware.
//!
//! Reads the `X-MA-Session-Id` request header, validates it, and inserts
//! a `SessionId` extension so downstream handlers can correlate writes to a
//! caller-supplied session token without touching the authentication layer.
//!
//! Design decisions (D3 / D4 / D5 from TODO-IMPL-01):
//! - Valid header   -> `SessionId("http:<value>")`
//! - Missing header -> `SessionId("http-fallback:<actor_id>-<nanos>")`
//! - Invalid header -> audit event + same fallback (NO 400 - session is an
//!   identity hint, not a hard auth gate)
//! - No AuthenticatedActor present (defensive) -> actor_id = "unknown"

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::audit::{AuditEvent, AuditLogger};
use crate::transport::auth::AuthenticatedActor;

pub const SESSION_ID_HEADER: &str = "x-ma-session-id";

/// Newtype wrapper inserted into request extensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionId(pub String);

/// State carried by the session middleware (mirrors `AuthState` pattern).
#[derive(Debug, Clone)]
pub struct SessionState {
    pub audit: AuditLogger,
}

impl SessionState {
    pub fn new(audit: AuditLogger) -> Self {
        Self { audit }
    }
}

/// Validation error for session IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionIdError {
    Empty,
    TooLong,
    InvalidChar,
}

impl std::fmt::Display for SessionIdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionIdError::Empty => write!(f, "session_id is empty"),
            SessionIdError::TooLong => write!(f, "session_id exceeds 128 characters"),
            SessionIdError::InvalidChar => {
                write!(
                    f,
                    "session_id contains invalid characters (allowed: [a-zA-Z0-9_:-])"
                )
            }
        }
    }
}

/// Validate a raw session-id value.
///
/// Allowed charset: `[a-zA-Z0-9_:-]`, max 128 characters.
/// Returns the validated value unchanged on success.
pub fn validate_session_id(value: &str) -> Result<&str, SessionIdError> {
    if value.is_empty() {
        return Err(SessionIdError::Empty);
    }
    if value.len() > 128 {
        return Err(SessionIdError::TooLong);
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ':')
    {
        return Err(SessionIdError::InvalidChar);
    }
    Ok(value)
}

fn fallback_session_id(actor_id: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("http-fallback:{actor_id}-{nanos}")
}

/// Axum middleware: extract and validate `X-MA-Session-Id`, insert `SessionId`
/// extension for downstream handlers.
///
/// Must run AFTER `auth_middleware` so `AuthenticatedActor` is already present
/// in extensions (axum layers run in REVERSE addition order — see `http.rs`).
pub async fn session_middleware(
    State(state): State<Arc<SessionState>>,
    mut request: Request,
    next: Next,
) -> Response {
    // Resolve actor_id for fallback construction. auth_middleware runs before
    // us in the request path, so AuthenticatedActor should be present on the
    // HTTP path. Defensive fallback to "unknown" for edge cases.
    let actor_id: String = request
        .extensions()
        .get::<AuthenticatedActor>()
        .map(|a| a.actor_id.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let session_id_value = match request.headers().get(SESSION_ID_HEADER) {
        None => {
            // No header provided - use fallback silently.
            fallback_session_id(&actor_id)
        }
        Some(raw) => {
            match raw.to_str() {
                Ok(s) => match validate_session_id(s) {
                    Ok(valid) => format!("http:{valid}"),
                    Err(err) => {
                        // Invalid header - audit and fall back.
                        let fallback = fallback_session_id(&actor_id);
                        let _ = state.audit.append(
                            AuditEvent::new("session_id_invalid", "fallback")
                                .with_session_id(fallback.clone())
                                .with_field("got", s)
                                .with_field("error", err.to_string()),
                        );
                        fallback
                    }
                },
                Err(_) => {
                    // Non-UTF-8 header bytes.
                    let fallback = fallback_session_id(&actor_id);
                    let _ = state.audit.append(
                        AuditEvent::new("session_id_invalid", "fallback")
                            .with_session_id(fallback.clone())
                            .with_field("got", "<non-utf8>")
                            .with_field("error", "header value is not valid UTF-8"),
                    );
                    fallback
                }
            }
        }
    };

    request.extensions_mut().insert(SessionId(session_id_value));
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- validate_session_id unit tests ---

    #[test]
    fn validate_session_id_accepts_alphanumeric_and_separators() {
        assert!(validate_session_id("claude-code-12345").is_ok());
        assert!(validate_session_id("codex_456:abc").is_ok());
        assert!(validate_session_id("a").is_ok());
        assert!(validate_session_id("ABC123_foo-bar:baz").is_ok());
    }

    #[test]
    fn validate_session_id_rejects_too_long() {
        let long = "a".repeat(129);
        assert_eq!(validate_session_id(&long), Err(SessionIdError::TooLong));
        // 128 chars is still valid.
        let exact = "a".repeat(128);
        assert!(validate_session_id(&exact).is_ok());
    }

    #[test]
    fn validate_session_id_rejects_empty() {
        assert_eq!(validate_session_id(""), Err(SessionIdError::Empty));
    }

    #[test]
    fn validate_session_id_rejects_invalid_char() {
        // space
        assert_eq!(
            validate_session_id("claude code"),
            Err(SessionIdError::InvalidChar)
        );
        // @
        assert_eq!(
            validate_session_id("bad@bytes"),
            Err(SessionIdError::InvalidChar)
        );
        // newline
        assert_eq!(
            validate_session_id("foo\nbar"),
            Err(SessionIdError::InvalidChar)
        );
        // dot
        assert_eq!(
            validate_session_id("foo.bar"),
            Err(SessionIdError::InvalidChar)
        );
    }

    // --- session_middleware async unit tests ---

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;
    use tower::ServiceExt;

    fn test_state() -> Arc<SessionState> {
        Arc::new(SessionState::new(AuditLogger::new(
            std::env::temp_dir().join("ma-session-test-audit"),
        )))
    }

    /// Build a minimal test router that applies session_middleware and returns
    /// the SessionId extension value in the response body.
    fn test_app(state: Arc<SessionState>) -> Router {
        Router::new()
            .route(
                "/test",
                get(|req: axum::extract::Request| async move {
                    let sid = req
                        .extensions()
                        .get::<SessionId>()
                        .cloned()
                        .map(|s| s.0)
                        .unwrap_or_else(|| "MISSING".to_string());
                    (StatusCode::OK, sid)
                }),
            )
            .layer(from_fn_with_state(state, session_middleware))
    }

    #[tokio::test]
    async fn session_middleware_extracts_valid_header() -> Result<(), String> {
        let app = test_app(test_state());
        let req = HttpRequest::builder()
            .method("GET")
            .uri("/test")
            .header(SESSION_ID_HEADER, "claude-code-12345")
            .body(Body::empty())
            .map_err(|e| e.to_string())?;

        let resp = app.oneshot(req).await.map_err(|e| e.to_string())?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .map_err(|e| e.to_string())?;
        let body_str = std::str::from_utf8(&body).map_err(|e| e.to_string())?;
        assert_eq!(body_str, "http:claude-code-12345");
        Ok(())
    }

    #[tokio::test]
    async fn session_middleware_falls_back_when_header_missing() -> Result<(), String> {
        // Inject actor into extensions before the middleware runs.
        let actor = AuthenticatedActor {
            actor_id: "test-actor".to_string(),
            key_name: None,
            key_hash: String::new(),
        };

        let inner = test_app(test_state());
        let req = {
            let mut r = HttpRequest::builder()
                .method("GET")
                .uri("/test")
                .body(Body::empty())
                .map_err(|e| e.to_string())?;
            r.extensions_mut().insert(actor);
            r
        };

        let resp = inner.oneshot(req).await.map_err(|e| e.to_string())?;
        let body = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .map_err(|e| e.to_string())?;
        let body_str = std::str::from_utf8(&body).map_err(|e| e.to_string())?;
        // Fallback must start with "http-fallback:test-actor-"
        assert!(
            body_str.starts_with("http-fallback:test-actor-"),
            "expected http-fallback:test-actor-<nanos>, got: {body_str}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_middleware_audits_invalid_header_then_falls_back() -> Result<(), String> {
        let dir = std::env::temp_dir().join(format!(
            "ma-session-audit-invalid-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let state = Arc::new(SessionState::new(AuditLogger::new(dir.clone())));
        let app = test_app(state);

        let req = HttpRequest::builder()
            .method("GET")
            .uri("/test")
            // Space is an invalid character.
            .header(SESSION_ID_HEADER, "invalid header value")
            .body(Body::empty())
            .map_err(|e| e.to_string())?;

        let resp = app.oneshot(req).await.map_err(|e| e.to_string())?;
        let body = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .map_err(|e| e.to_string())?;
        let body_str = std::str::from_utf8(&body).map_err(|e| e.to_string())?;

        // Response must be a fallback (not the invalid value, not a 400).
        assert!(
            body_str.starts_with("http-fallback:"),
            "expected fallback, got: {body_str}"
        );

        // Audit log must record the invalid event.
        let logger = AuditLogger::new(dir);
        let audit_content =
            std::fs::read_to_string(logger.current_path()).map_err(|e| e.to_string())?;
        assert!(
            audit_content.contains("session_id_invalid"),
            "audit log missing session_id_invalid event"
        );
        // The fallback session_id must be attached to the audit event.
        assert!(
            audit_content.contains(r#""session_id":"http-fallback:"#),
            "audit event missing session_id field with http-fallback prefix, got: {audit_content}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_middleware_handles_missing_actor_defensively() -> Result<(), String> {
        // No actor inserted -> should use "unknown" in fallback.
        let app = test_app(test_state());
        let req = HttpRequest::builder()
            .method("GET")
            .uri("/test")
            .body(Body::empty())
            .map_err(|e| e.to_string())?;

        let resp = app.oneshot(req).await.map_err(|e| e.to_string())?;
        let body = axum::body::to_bytes(resp.into_body(), 1024)
            .await
            .map_err(|e| e.to_string())?;
        let body_str = std::str::from_utf8(&body).map_err(|e| e.to_string())?;
        assert!(
            body_str.starts_with("http-fallback:unknown-"),
            "expected http-fallback:unknown-<nanos>, got: {body_str}"
        );
        Ok(())
    }
}
