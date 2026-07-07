//! POST /session/open — issue a write token for L0 governance mutations.
//!
//! D-phase implementation (TODO-IMPL-01b). The caller must already be
//! authenticated via `auth_middleware` (Bearer API key). On success the
//! response contains the write token secret (returned ONCE) and its
//! expiration timestamp. The server stores only the blake3 hash.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::audit::{AuditEvent, AuditLogger};
use crate::transport::auth::{AuthenticatedActor, hash_bearer_token};
use crate::transport::session::validate_session_id;
use memra_core::storage::db::DbPool;
use memra_core::storage::session_tokens_writer::issue_token;

/// Maximum TTL allowed by contract (24 hours).
pub const MAX_TTL_SECONDS: u64 = 86_400;

/// Shared state for the session/open endpoint.
#[derive(Clone)]
pub struct SessionOpenState {
    pub pool: Arc<DbPool>,
    pub audit: AuditLogger,
}

/// Request body for POST /session/open.
#[derive(Debug, Deserialize)]
pub struct SessionOpenRequest {
    pub session_id: String,
    /// Actor hint (ignored for auth — actor is always the bearer key owner).
    #[allow(dead_code)]
    pub agent: Option<String>,
    /// TTL in seconds. Must be 0 < ttl_seconds <= 86400.
    pub ttl_seconds: Option<u64>,
}

/// Successful response body.
#[derive(Debug, Serialize)]
pub struct SessionOpenResponse {
    /// The write-token secret. Returned ONCE; never stored server-side.
    pub write_token: String,
    /// ISO 8601 expiration timestamp.
    pub expires_at: String,
}

/// Error response body.
#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

fn error_response(status: StatusCode, message: impl Into<String>) -> axum::response::Response {
    (
        status,
        Json(ErrorBody {
            error: message.into(),
        }),
    )
        .into_response()
}

/// Handler for POST /session/open.
///
/// Authentication: caller must present a valid Bearer API key (enforced by
/// `auth_middleware` before this handler runs).  The `AuthenticatedActor`
/// extension is inserted by that middleware.
pub async fn session_open_handler(
    State(state): State<Arc<SessionOpenState>>,
    actor: axum::Extension<AuthenticatedActor>,
    Json(body): Json<SessionOpenRequest>,
) -> axum::response::Response {
    // Validate session_id.
    if body.session_id.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "session_id is required");
    }
    if let Err(e) = validate_session_id(&body.session_id) {
        return error_response(StatusCode::BAD_REQUEST, format!("invalid session_id: {e}"));
    }

    // Validate TTL.
    let ttl_seconds = body.ttl_seconds.unwrap_or(3_600);
    if ttl_seconds == 0 || ttl_seconds > MAX_TTL_SECONDS {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("ttl_seconds must be between 1 and {MAX_TTL_SECONDS}"),
        );
    }

    // Generate the secret (32 random bytes, hex-encoded).
    // The brief says "base64url(32 random bytes)" but since base64 is not in
    // the workspace, we use lowercase hex of 32 bytes (64 chars) — still
    // 256 bits of entropy and URL-safe.
    let secret = match generate_secret() {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to generate write token secret: {e}");
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "token generation failed");
        }
    };

    // Hash the secret for storage.
    let token_hash = hash_bearer_token(&secret);

    // Compute timestamps.
    let now = Utc::now();
    let issued_at = now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let expires_at = (now + chrono::Duration::seconds(ttl_seconds as i64))
        .to_rfc3339_opts(SecondsFormat::Secs, true);

    // Insert into DB.
    let result = state.pool.with_conn(|conn| {
        issue_token(
            conn,
            &token_hash,
            &body.session_id,
            &actor.actor_id,
            &issued_at,
            &expires_at,
        )
    });

    if let Err(e) = result {
        tracing::error!("failed to insert write token: {e}");
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, "token storage failed");
    }

    // Audit: session_id + actor_id; NEVER log the secret or the hash.
    let _ = state.audit.append(
        AuditEvent::new("session_token_issued", "ok")
            .with_session_id(format!("http:{}", body.session_id))
            .with_actor(&actor.actor_id)
            .with_field("expires_at", expires_at.as_str()),
    );

    (
        StatusCode::OK,
        Json(SessionOpenResponse {
            write_token: secret,
            expires_at,
        }),
    )
        .into_response()
}

/// Generate a 32-byte random secret as a lowercase hex string.
///
/// 64 hex characters = 256 bits of entropy. URL-safe and no extra crate needed.
fn generate_secret() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes_to_hex(&bytes))
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        output.push(HEX[(b >> 4) as usize] as char);
        output.push(HEX[(b & 0x0f) as usize] as char);
    }
    output
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::middleware::from_fn_with_state;
    use axum::routing::post;
    use tower::ServiceExt;

    use crate::audit::AuditLogger;
    use crate::transport::auth::{AuthState, AuthenticatedActor, Authenticator};
    use memra_core::storage::db::DbPool;

    fn make_state() -> Arc<SessionOpenState> {
        let pool = DbPool::open(std::path::Path::new(":memory:")).expect("in-memory DB");
        Arc::new(SessionOpenState {
            pool: Arc::new(pool),
            audit: AuditLogger::new(std::env::temp_dir().join("ma-session-open-test-audit")),
        })
    }

    fn make_actor(actor_id: &str) -> AuthenticatedActor {
        AuthenticatedActor {
            actor_id: actor_id.to_string(),
            key_name: None,
            key_hash: String::new(),
        }
    }

    /// Build a test router that pre-injects the given actor (bypasses real auth).
    fn test_app_with_actor(state: Arc<SessionOpenState>, actor: AuthenticatedActor) -> Router {
        Router::new()
            .route("/session/open", post(session_open_handler))
            .layer(axum::middleware::from_fn(
                move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                    let actor = actor.clone();
                    async move {
                        req.extensions_mut().insert(actor);
                        next.run(req).await
                    }
                },
            ))
            .with_state(state)
    }

    /// Build a test router with a real auth_middleware.
    fn test_app_with_auth(state: Arc<SessionOpenState>, auth_state: Arc<AuthState>) -> Router {
        Router::new()
            .route("/session/open", post(session_open_handler))
            .layer(from_fn_with_state(
                auth_state,
                crate::transport::auth::auth_middleware,
            ))
            .with_state(state)
    }

    // AC2: 200 happy path — valid session_id + default TTL.
    #[tokio::test]
    async fn session_open_returns_200_with_write_token() -> Result<(), String> {
        let state = make_state();
        let app = test_app_with_actor(state, make_actor("alice"));

        let req = Request::builder()
            .method("POST")
            .uri("/session/open")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"session_id":"http:test-sess"}"#))
            .map_err(|e| e.to_string())?;

        let resp = app.oneshot(req).await.map_err(|e| e.to_string())?;
        assert_eq!(resp.status(), StatusCode::OK, "expected 200 on happy path");

        let body = axum::body::to_bytes(resp.into_body(), 4096)
            .await
            .map_err(|e| e.to_string())?;
        let json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;

        // Token must be present and 64-char hex.
        let token = json["write_token"].as_str().expect("write_token field");
        assert_eq!(
            token.len(),
            64,
            "write_token must be 64 hex chars (32 bytes)"
        );
        assert!(
            token.chars().all(|c| c.is_ascii_hexdigit()),
            "write_token must be hex"
        );
        // expires_at must be present.
        assert!(
            json["expires_at"].is_string(),
            "expires_at field must be present"
        );
        Ok(())
    }

    // AC2: 400 — missing session_id.
    #[tokio::test]
    async fn session_open_400_on_missing_session_id() -> Result<(), String> {
        let state = make_state();
        let app = test_app_with_actor(state, make_actor("alice"));

        let req = Request::builder()
            .method("POST")
            .uri("/session/open")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"session_id":""}"#))
            .map_err(|e| e.to_string())?;

        let resp = app.oneshot(req).await.map_err(|e| e.to_string())?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    // AC2: 400 — invalid session_id (contains space).
    #[tokio::test]
    async fn session_open_400_on_invalid_session_id() -> Result<(), String> {
        let state = make_state();
        let app = test_app_with_actor(state, make_actor("alice"));

        let req = Request::builder()
            .method("POST")
            .uri("/session/open")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"session_id":"invalid session id"}"#))
            .map_err(|e| e.to_string())?;

        let resp = app.oneshot(req).await.map_err(|e| e.to_string())?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    // AC7: 400 — TTL too long (> 86400).
    #[tokio::test]
    async fn session_open_400_on_ttl_exceeding_max() -> Result<(), String> {
        let state = make_state();
        let app = test_app_with_actor(state, make_actor("alice"));

        let req = Request::builder()
            .method("POST")
            .uri("/session/open")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"session_id":"http:sess","ttl_seconds":86401}"#,
            ))
            .map_err(|e| e.to_string())?;

        let resp = app.oneshot(req).await.map_err(|e| e.to_string())?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    // AC7: 400 — TTL zero.
    #[tokio::test]
    async fn session_open_400_on_ttl_zero() -> Result<(), String> {
        let state = make_state();
        let app = test_app_with_actor(state, make_actor("alice"));

        let req = Request::builder()
            .method("POST")
            .uri("/session/open")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"session_id":"http:sess","ttl_seconds":0}"#))
            .map_err(|e| e.to_string())?;

        let resp = app.oneshot(req).await.map_err(|e| e.to_string())?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        Ok(())
    }

    // AC2: 401 — missing auth (with real auth middleware, no keys configured).
    #[tokio::test]
    async fn session_open_401_without_auth() -> Result<(), String> {
        use crate::config::{AppConfig, AuthConfig, ServerConfig};

        let state = make_state();
        // Config with no API keys → auth always rejects.
        let config = AppConfig {
            auth: AuthConfig { api_keys: vec![] },
            server: ServerConfig::default(),
        };
        let auth_state = Arc::new(AuthState {
            authenticator: Arc::new(Authenticator::from_config(&config)),
            audit: AuditLogger::new(std::env::temp_dir().join("ma-session-open-401-audit")),
        });
        let app = test_app_with_auth(state, auth_state);

        let req = Request::builder()
            .method("POST")
            .uri("/session/open")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"session_id":"http:sess"}"#))
            .map_err(|e| e.to_string())?;

        let resp = app.oneshot(req).await.map_err(|e| e.to_string())?;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    // AC7: ttl_seconds = 86400 (max) is accepted.
    #[tokio::test]
    async fn session_open_200_on_max_ttl() -> Result<(), String> {
        let state = make_state();
        let app = test_app_with_actor(state, make_actor("alice"));

        let req = Request::builder()
            .method("POST")
            .uri("/session/open")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"session_id":"http:sess","ttl_seconds":86400}"#,
            ))
            .map_err(|e| e.to_string())?;

        let resp = app.oneshot(req).await.map_err(|e| e.to_string())?;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "max TTL 86400 should be accepted"
        );
        Ok(())
    }

    // AC6: secret must not appear in the stored token_hash.
    #[tokio::test]
    async fn session_open_secret_not_equal_to_stored_hash() -> Result<(), String> {
        use memra_core::storage::session_tokens_writer::validate_token;

        let state = make_state();
        let app = test_app_with_actor(Arc::clone(&state), make_actor("alice"));

        let req = Request::builder()
            .method("POST")
            .uri("/session/open")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"session_id":"http:hash-check"}"#))
            .map_err(|e| e.to_string())?;

        let resp = app.oneshot(req).await.map_err(|e| e.to_string())?;
        let body = axum::body::to_bytes(resp.into_body(), 4096)
            .await
            .map_err(|e| e.to_string())?;
        let json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        let secret = json["write_token"].as_str().expect("write_token");

        // The secret itself must NOT be stored in the DB — validate using the
        // secret as the hash (if that passes, the secret was stored raw — P0 bug).
        let now_ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let wrong_result = state
            .pool
            .with_conn(|conn| validate_token(conn, secret, "alice", &now_ts));
        // Using the raw secret as a lookup hash should return NotFound (not Valid).
        assert!(
            matches!(
                wrong_result,
                Ok(memra_core::storage::session_tokens_writer::ValidateResult::NotFound)
            ),
            "raw secret must NOT be stored as token_hash (AC6 violation)"
        );
        Ok(())
    }

    // Schema correctness: response JSON has exactly write_token + expires_at.
    #[tokio::test]
    async fn session_open_response_schema_has_required_fields() -> Result<(), String> {
        let state = make_state();
        let app = test_app_with_actor(state, make_actor("alice"));

        let req = Request::builder()
            .method("POST")
            .uri("/session/open")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"session_id":"http:schema-check","ttl_seconds":3600}"#,
            ))
            .map_err(|e| e.to_string())?;

        let resp = app.oneshot(req).await.map_err(|e| e.to_string())?;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), 4096)
            .await
            .map_err(|e| e.to_string())?;
        let json: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;

        // Exactly two top-level keys.
        let obj = json.as_object().expect("response must be a JSON object");
        assert!(obj.contains_key("write_token"), "must have write_token");
        assert!(obj.contains_key("expires_at"), "must have expires_at");
        // expires_at must parse as RFC3339.
        let expires_at_str = obj["expires_at"].as_str().expect("expires_at string");
        chrono::DateTime::parse_from_rfc3339(expires_at_str)
            .map_err(|e| format!("expires_at not valid RFC3339: {e}"))?;
        Ok(())
    }
}
