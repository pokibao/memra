use crate::audit::{AuditEvent, AuditLogger};
use crate::config::{ApiKeyConfig, AppConfig};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

pub const ACTOR_HINT_HEADER: &str = "x-ma-actor-id";

/// Combined auth middleware state: authenticator + audit logger.
#[derive(Debug, Clone)]
pub struct AuthState {
    pub authenticator: Arc<Authenticator>,
    pub audit: AuditLogger,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedActor {
    pub actor_id: String,
    pub key_name: Option<String>,
    pub key_hash: String,
}

#[derive(Debug, Clone)]
pub struct Authenticator {
    keys: Vec<ApiKeyEntry>,
}

impl Authenticator {
    pub fn empty() -> Self {
        Self { keys: Vec::new() }
    }

    pub fn from_config(config: &AppConfig) -> Self {
        let keys = config
            .auth
            .api_keys
            .iter()
            .map(ApiKeyEntry::from_config)
            .collect();
        Self { keys }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn authenticate_authorization_header(
        &self,
        authorization: &str,
    ) -> Result<AuthenticatedActor, AuthError> {
        let token = bearer_token_from_header(authorization)?;
        self.authenticate_token(token)
    }

    pub fn authenticate_token(&self, token: &str) -> Result<AuthenticatedActor, AuthError> {
        let token_hash = hash_bearer_token(token);
        let mut matched_key: Option<&ApiKeyEntry> = None;
        let mut duplicate = false;
        for key in &self.keys {
            if constant_time_eq(&key.key_hash, &token_hash) {
                if matched_key.is_some() {
                    // Keep walking all remaining entries to preserve constant-time
                    // behaviour — do not return early here.
                    duplicate = true;
                } else {
                    matched_key = Some(key);
                }
            }
        }

        if duplicate {
            tracing::error!("duplicate api key hash in config — refusing ambiguous auth");
            return Err(AuthError::InvalidApiKey);
        }

        let Some(key) = matched_key else {
            return Err(AuthError::InvalidApiKey);
        };

        if key.revoked {
            return Err(AuthError::InvalidApiKey);
        }

        Ok(AuthenticatedActor {
            actor_id: key.actor_id.clone(),
            key_name: key.name.clone(),
            key_hash: key.key_hash.clone(),
        })
    }

    pub fn verify_actor_hint(
        &self,
        actor: &AuthenticatedActor,
        actor_hint: Option<&str>,
    ) -> Result<(), AuthError> {
        let Some(actor_hint) = actor_hint else {
            return Ok(());
        };

        if actor_hint == actor.actor_id {
            return Ok(());
        }

        Err(AuthError::ActorMismatch {
            expected: actor.actor_id.clone(),
            got: actor_hint.to_string(),
        })
    }
}

pub async fn auth_middleware(
    State(state): State<Arc<AuthState>>,
    mut request: Request,
    next: Next,
) -> Response {
    if state.authenticator.is_empty() {
        return auth_error_response(
            StatusCode::UNAUTHORIZED,
            "HTTP auth is not configured; run ma admin add-key first",
        );
    }

    let Some(value) = request.headers().get(AUTHORIZATION) else {
        return auth_error_response(StatusCode::UNAUTHORIZED, "missing Authorization header");
    };
    let authorization = match value.to_str() {
        Ok(value) => value,
        Err(_) => {
            return auth_error_response(
                StatusCode::UNAUTHORIZED,
                "invalid Authorization header encoding",
            );
        }
    };

    let actor = match state
        .authenticator
        .authenticate_authorization_header(authorization)
    {
        Ok(actor) => actor,
        Err(error) => return auth_error_response(StatusCode::UNAUTHORIZED, error.to_string()),
    };
    let actor_hint = request
        .headers()
        .get(ACTOR_HINT_HEADER)
        .and_then(|value| value.to_str().ok());
    if let Err(error) = state.authenticator.verify_actor_hint(&actor, actor_hint) {
        // D3.1/D3.1b: ActorMismatch is a security event — log to audit JSONL
        let _ = state.audit.append(
            AuditEvent::new("actor_mismatch", "rejected")
                .with_field("actor_id", actor.actor_id.as_str())
                .with_field("hint", actor_hint.unwrap_or(""))
                .with_field("error", error.to_string()),
        );
        return auth_error_response(StatusCode::FORBIDDEN, error.to_string());
    }

    request.extensions_mut().insert(actor);
    next.run(request).await
}

fn auth_error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, message.into()).into_response()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApiKeyEntry {
    name: Option<String>,
    key_hash: String,
    actor_id: String,
    revoked: bool,
}

impl ApiKeyEntry {
    fn from_config(config: &ApiKeyConfig) -> Self {
        Self {
            name: config.name.clone(),
            key_hash: normalize_key_hash(&config.key_hash),
            actor_id: config.actor_id.clone(),
            revoked: config.revoked_at.is_some(),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("invalid authorization header")]
    InvalidAuthorizationHeader,
    #[error("missing bearer token")]
    MissingBearerToken,
    #[error("invalid API key")]
    InvalidApiKey,
    #[error("actor hint mismatch: expected {expected}, got {got}")]
    ActorMismatch { expected: String, got: String },
}

pub fn hash_bearer_token(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

pub fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let max_len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();

    for index in 0..max_len {
        let left_byte = byte_at_or_zero(left, index);
        let right_byte = byte_at_or_zero(right, index);
        diff |= (left_byte ^ right_byte) as usize;
    }

    diff == 0
}

fn byte_at_or_zero(bytes: &[u8], index: usize) -> u8 {
    if index < bytes.len() { bytes[index] } else { 0 }
}

fn bearer_token_from_header(authorization: &str) -> Result<&str, AuthError> {
    let Some((scheme, token)) = authorization.split_once(' ') else {
        return Err(AuthError::InvalidAuthorizationHeader);
    };

    if !scheme.eq_ignore_ascii_case("Bearer") {
        return Err(AuthError::InvalidAuthorizationHeader);
    }

    let token = token.trim();
    if token.is_empty() {
        return Err(AuthError::MissingBearerToken);
    }

    Ok(token)
}

fn normalize_key_hash(key_hash: &str) -> String {
    match key_hash.strip_prefix("blake3:") {
        Some(hash) => hash.to_ascii_lowercase(),
        None => key_hash.to_ascii_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{ApiKeyConfig, AppConfig, AuthConfig, ServerConfig};
    use crate::transport::auth::{AuthError, Authenticator, constant_time_eq, hash_bearer_token};

    fn config_with_keys(api_keys: Vec<ApiKeyConfig>) -> AppConfig {
        AppConfig {
            auth: AuthConfig { api_keys },
            server: ServerConfig::default(),
        }
    }

    fn api_key(
        name: Option<&str>,
        key_hash: String,
        actor_id: &str,
        revoked_at: Option<&str>,
    ) -> ApiKeyConfig {
        ApiKeyConfig {
            name: name.map(str::to_string),
            key_hash,
            actor_id: actor_id.to_string(),
            created_at: "2026-04-12T00:00:00Z".to_string(),
            revoked_at: revoked_at.map(str::to_string),
        }
    }

    #[test]
    fn resolves_bearer_token_to_bound_actor() -> Result<(), String> {
        let key_hash = format!("blake3:{}", hash_bearer_token("secret-token"));
        let auth = Authenticator::from_config(&config_with_keys(vec![api_key(
            Some("claude-code-local"),
            key_hash,
            "claude-code",
            None,
        )]));

        let actor = auth
            .authenticate_authorization_header("Bearer secret-token")
            .map_err(|error| error.to_string())?;

        assert_eq!(actor.actor_id, "claude-code");
        assert_eq!(actor.key_name.as_deref(), Some("claude-code-local"));
        auth.verify_actor_hint(&actor, Some("claude-code"))
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    #[test]
    fn supports_raw_hash_and_rejects_revoked_key() -> Result<(), String> {
        let key_hash = hash_bearer_token("revoked-token");
        let auth = Authenticator::from_config(&config_with_keys(vec![api_key(
            None,
            key_hash,
            "cursor",
            Some("2026-04-13T00:00:00Z"),
        )]));

        let error = match auth.authenticate_authorization_header("Bearer revoked-token") {
            Ok(actor) => return Err(format!("expected revoked key rejection, got {actor:?}")),
            Err(error) => error,
        };

        assert!(matches!(error, AuthError::InvalidApiKey));
        Ok(())
    }

    #[test]
    fn distinct_hashes_match_correct_actor() -> Result<(), String> {
        let hash_a = format!("blake3:{}", hash_bearer_token("token-a"));
        let hash_b = format!("blake3:{}", hash_bearer_token("token-b"));
        let auth = Authenticator::from_config(&config_with_keys(vec![
            api_key(Some("actor-one"), hash_a, "actor-1", None),
            api_key(Some("actor-two"), hash_b, "actor-2", None),
        ]));

        let actor = auth
            .authenticate_authorization_header("Bearer token-a")
            .map_err(|e| e.to_string())?;
        assert_eq!(actor.actor_id, "actor-1");

        let actor = auth
            .authenticate_authorization_header("Bearer token-b")
            .map_err(|e| e.to_string())?;
        assert_eq!(actor.actor_id, "actor-2");

        Ok(())
    }

    #[test]
    fn rejects_duplicate_key_hash_config() {
        let shared_hash = format!("blake3:{}", hash_bearer_token("shared-secret"));
        let auth = Authenticator::from_config(&config_with_keys(vec![
            api_key(Some("key-one"), shared_hash.clone(), "actor-1", None),
            api_key(Some("key-two"), shared_hash, "actor-2", None),
        ]));

        assert!(matches!(
            auth.authenticate_authorization_header("Bearer shared-secret"),
            Err(AuthError::InvalidApiKey)
        ));
    }

    #[test]
    fn rejects_malformed_authorization_headers() {
        assert!(matches!(
            Authenticator::empty().authenticate_authorization_header("Basic secret"),
            Err(AuthError::InvalidAuthorizationHeader)
        ));
        assert!(matches!(
            Authenticator::empty().authenticate_authorization_header("Bearer   "),
            Err(AuthError::MissingBearerToken)
        ));
    }

    #[test]
    fn rejects_actor_hint_mismatch() -> Result<(), String> {
        let key_hash = format!("blake3:{}", hash_bearer_token("secret-token"));
        let auth = Authenticator::from_config(&config_with_keys(vec![api_key(
            Some("human"),
            key_hash,
            "human-cli",
            None,
        )]));
        let actor = auth
            .authenticate_authorization_header("Bearer secret-token")
            .map_err(|error| error.to_string())?;

        assert!(matches!(
            auth.verify_actor_hint(&actor, Some("spoofed-actor")),
            Err(AuthError::ActorMismatch { .. })
        ));
        Ok(())
    }

    #[test]
    fn constant_time_compare_handles_length_mismatch() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("abc", "abx"));
    }
}
