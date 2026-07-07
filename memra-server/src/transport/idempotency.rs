use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::transport::admission::is_write_tool_call_body;
use crate::transport::auth::AuthenticatedActor;

pub const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
pub const IDEMPOTENCY_TTL_SECONDS: i64 = 24 * 60 * 60;
const LEDGER_FILE_NAME: &str = "ledger.sqlite3";
const IDEMPOTENCY_BODY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
pub struct IdempotencyLedger {
    db_path: PathBuf,
    conn: Mutex<Connection>,
}

#[derive(Debug)]
pub struct IdempotencyState {
    pub ledger: Arc<IdempotencyLedger>,
    pub project_id: String,
}

impl IdempotencyLedger {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, IdempotencyError> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir).map_err(|source| IdempotencyError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let db_path = dir.join(LEDGER_FILE_NAME);
        let conn = Connection::open(&db_path).map_err(IdempotencyError::Sqlite)?;
        let ledger = Self {
            db_path,
            conn: Mutex::new(conn),
        };
        ledger.init()?;
        Ok(ledger)
    }

    pub fn default_dir() -> PathBuf {
        home_dir().join(".memra").join("idempotency")
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn begin_request(
        &self,
        key: &str,
        request_hash: &str,
        now_unix: i64,
    ) -> Result<IdempotencyOutcome, IdempotencyError> {
        let conn = self.lock_conn()?;
        sweep_expired_inner(&conn, now_unix)?;

        let existing = conn
            .query_row(
                "SELECT request_hash, response_status, response_headers_json, response_body
                 FROM idempotency_ledger
                 WHERE key = ?1",
                [key],
                |row| {
                    Ok(ExistingEntry {
                        request_hash: row.get(0)?,
                        response_status: row.get(1)?,
                        response_headers_json: row.get(2)?,
                        response_body: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(IdempotencyError::Sqlite)?;

        if let Some(existing) = existing {
            if existing.request_hash != request_hash {
                return Err(IdempotencyError::KeyConflict);
            }
            return match (
                existing.response_status,
                existing.response_headers_json,
                existing.response_body,
            ) {
                (Some(status), Some(headers_json), Some(body)) => {
                    Ok(IdempotencyOutcome::Replay(CachedResponse {
                        status,
                        headers_json,
                        body,
                    }))
                }
                _ => Ok(IdempotencyOutcome::InProgress),
            };
        }

        conn.execute(
            "INSERT INTO idempotency_ledger
             (key, request_hash, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                key,
                request_hash,
                now_unix,
                now_unix + IDEMPOTENCY_TTL_SECONDS
            ],
        )
        .map_err(IdempotencyError::Sqlite)?;

        Ok(IdempotencyOutcome::New)
    }

    pub fn store_response(
        &self,
        key: &str,
        response: CachedResponse,
    ) -> Result<(), IdempotencyError> {
        let conn = self.lock_conn()?;
        let updated = conn
            .execute(
                "UPDATE idempotency_ledger
                 SET response_status = ?2,
                     response_headers_json = ?3,
                     response_body = ?4
                 WHERE key = ?1",
                params![key, response.status, response.headers_json, response.body],
            )
            .map_err(IdempotencyError::Sqlite)?;

        if updated == 0 {
            return Err(IdempotencyError::KeyNotFound);
        }
        Ok(())
    }

    /// Delete the in-progress slot for `key` so that a retry with the same
    /// Idempotency-Key is treated as a brand-new request rather than
    /// replaying a cached error.  Called when the upstream handler returns
    /// a non-2xx status (RFC-draft §2.5: non-2xx MUST NOT be replayed).
    pub fn release_failed(&self, key: &str) -> Result<(), IdempotencyError> {
        let conn = self.lock_conn()?;
        conn.execute("DELETE FROM idempotency_ledger WHERE key = ?1", [key])
            .map_err(IdempotencyError::Sqlite)?;
        Ok(())
    }

    pub fn sweep_expired(&self, now_unix: i64) -> Result<usize, IdempotencyError> {
        let conn = self.lock_conn()?;
        sweep_expired_inner(&conn, now_unix)
    }

    fn init(&self) -> Result<(), IdempotencyError> {
        let conn = self.lock_conn()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS idempotency_ledger (
                key TEXT PRIMARY KEY,
                request_hash TEXT NOT NULL,
                response_status INTEGER,
                response_headers_json TEXT,
                response_body BLOB,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_idempotency_expires_at
                ON idempotency_ledger(expires_at);",
        )
        .map_err(IdempotencyError::Sqlite)?;
        Ok(())
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, IdempotencyError> {
        self.conn.lock().map_err(|_| IdempotencyError::Poisoned)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedResponse {
    pub status: u16,
    pub headers_json: String,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyOutcome {
    New,
    Replay(CachedResponse),
    InProgress,
}

#[derive(Debug, thiserror::Error)]
pub enum IdempotencyError {
    #[error("missing Idempotency-Key header")]
    MissingKey,
    #[error("invalid Idempotency-Key header")]
    InvalidKey,
    #[error("Idempotency-Key reused with a different request body")]
    KeyConflict,
    #[error("idempotency key has no pending ledger row")]
    KeyNotFound,
    #[error("idempotency ledger mutex poisoned")]
    Poisoned,
    #[error("idempotency ledger I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("idempotency ledger SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Debug)]
struct ExistingEntry {
    request_hash: String,
    response_status: Option<u16>,
    response_headers_json: Option<String>,
    response_body: Option<Vec<u8>>,
}

pub fn require_idempotency_key(
    headers: &HeaderMap,
    is_write: bool,
) -> Result<Option<String>, IdempotencyError> {
    if !is_write {
        return Ok(None);
    }

    let Some(value) = headers.get(IDEMPOTENCY_KEY_HEADER) else {
        return Err(IdempotencyError::MissingKey);
    };
    let key = value
        .to_str()
        .map_err(|_| IdempotencyError::InvalidKey)?
        .trim();
    if key.is_empty() {
        return Err(IdempotencyError::MissingKey);
    }
    Ok(Some(key.to_string()))
}

pub fn request_hash(body: &[u8]) -> String {
    blake3::hash(body).to_hex().to_string()
}

pub async fn idempotency_middleware(
    State(state): State<Arc<IdempotencyState>>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let body_bytes = match to_bytes(body, IDEMPOTENCY_BODY_LIMIT_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return text_response(
                StatusCode::BAD_REQUEST,
                format!("failed to read request body: {error}"),
            );
        }
    };

    let is_write = is_write_tool_call_body(&body_bytes);
    let key = match require_idempotency_key(&parts.headers, is_write) {
        Ok(key) => key,
        Err(error) => return idempotency_error_response(error),
    };

    let Some(key) = key else {
        let request = Request::from_parts(parts, Body::from(body_bytes));
        return next.run(request).await;
    };

    let scoped_key = scoped_idempotency_key(&parts.extensions, &state.project_id, &key);
    let hash = request_hash(&body_bytes);
    match state.ledger.begin_request(&scoped_key, &hash, now_unix()) {
        Ok(IdempotencyOutcome::New) => {
            let request = Request::from_parts(parts, Body::from(body_bytes));
            let response = next.run(request).await;
            cache_response(&state.ledger, &scoped_key, response).await
        }
        Ok(IdempotencyOutcome::Replay(response)) => cached_response(response),
        Ok(IdempotencyOutcome::InProgress) => text_response(
            StatusCode::CONFLICT,
            "Idempotency-Key request is already in progress",
        ),
        Err(error) => idempotency_error_response(error),
    }
}

fn scoped_idempotency_key(
    extensions: &axum::http::Extensions,
    project_id: &str,
    raw_key: &str,
) -> String {
    let actor_id = extensions
        .get::<AuthenticatedActor>()
        .map(|actor| actor.actor_id.as_str())
        .unwrap_or("anonymous");
    let project_hash = blake3::hash(project_id.as_bytes()).to_hex().to_string();
    let actor_hash = blake3::hash(actor_id.as_bytes()).to_hex().to_string();
    format!("v2:{project_hash}:{actor_hash}:{raw_key}")
}

async fn cache_response(ledger: &IdempotencyLedger, key: &str, response: Response) -> Response {
    let (parts, body) = response.into_parts();
    let body_bytes = match to_bytes(body, IDEMPOTENCY_BODY_LIMIT_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => {
            // Release the in-progress slot before bailing out so a retry with
            // the same Idempotency-Key isn't frozen as InProgress for the
            // 24h TTL (e.g. when the handler body exceeds the body limit).
            if let Err(release_err) = ledger.release_failed(key) {
                tracing::warn!(
                    "failed to release idempotency slot after body read error: {release_err}"
                );
            }
            return text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to read response body: {error}"),
            );
        }
    };

    let status = parts.status;
    if status.is_success() {
        // RFC-draft §2.5: only 2xx responses are eligible for replay.
        let cached = CachedResponse {
            status: status.as_u16(),
            headers_json: headers_to_json(&parts.headers),
            body: body_bytes.to_vec(),
        };
        if let Err(error) = ledger.store_response(key, cached) {
            tracing::warn!("failed to store idempotency response: {error}");
            // Caching the 2xx failed (e.g. SQLite locked). Release the slot so
            // a retry with the same key isn't frozen as InProgress for 24h.
            // The original 200 still flows back to this caller below.
            if let Err(release_err) = ledger.release_failed(key) {
                tracing::warn!(
                    "failed to release idempotency slot after store failure: {release_err}"
                );
            }
        }
    } else {
        // Non-2xx — release the in-progress slot so the next retry with
        // the same Idempotency-Key is treated as New (not InProgress).
        if let Err(e) = ledger.release_failed(key) {
            tracing::warn!("failed to release idempotency slot on non-2xx: {e}");
        }
    }
    Response::from_parts(parts, Body::from(body_bytes))
}

fn cached_response(cached: CachedResponse) -> Response {
    let status = match StatusCode::from_u16(cached.status) {
        Ok(status) => status,
        Err(_) => StatusCode::OK,
    };
    let mut response = (status, Body::from(cached.body)).into_response();
    restore_headers(response.headers_mut(), &cached.headers_json);
    response
}

fn idempotency_error_response(error: IdempotencyError) -> Response {
    let status = match error {
        IdempotencyError::MissingKey | IdempotencyError::InvalidKey => StatusCode::BAD_REQUEST,
        IdempotencyError::KeyConflict | IdempotencyError::KeyNotFound => StatusCode::CONFLICT,
        IdempotencyError::Poisoned | IdempotencyError::Io { .. } | IdempotencyError::Sqlite(_) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    text_response(status, error.to_string())
}

fn text_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, message.into()).into_response()
}

fn headers_to_json(headers: &HeaderMap) -> String {
    let pairs: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    serde_json::to_string(&pairs).unwrap_or_else(|_| "[]".to_string())
}

fn restore_headers(headers: &mut HeaderMap, headers_json: &str) {
    let Ok(Value::Array(pairs)) = serde_json::from_str::<Value>(headers_json) else {
        return;
    };
    for pair in pairs {
        let Some(name) = pair.get(0).and_then(Value::as_str) else {
            continue;
        };
        let Some(value) = pair.get(1).and_then(Value::as_str) else {
            continue;
        };
        let Ok(name) = name.parse::<axum::http::HeaderName>() else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(value) else {
            continue;
        };
        headers.insert(name, value);
    }
}

fn now_unix() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(_) => 0,
    }
}

fn sweep_expired_inner(conn: &Connection, now_unix: i64) -> Result<usize, IdempotencyError> {
    conn.execute(
        "DELETE FROM idempotency_ledger WHERE expires_at <= ?1",
        [now_unix],
    )
    .map_err(IdempotencyError::Sqlite)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
    use axum::middleware::from_fn_with_state;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use tower::ServiceExt;

    use super::IDEMPOTENCY_BODY_LIMIT_BYTES;

    use crate::transport::idempotency::{
        CachedResponse, IdempotencyError, IdempotencyLedger, IdempotencyOutcome, IdempotencyState,
        require_idempotency_key,
    };

    fn temp_dir(name: &str) -> Result<PathBuf, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ma-idempotency-test-{now}-{name}"));
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        Ok(dir)
    }

    fn state(ledger: Arc<IdempotencyLedger>) -> Arc<IdempotencyState> {
        Arc::new(IdempotencyState {
            ledger,
            project_id: "test-project".to_string(),
        })
    }

    #[test]
    fn write_requests_require_idempotency_key() -> Result<(), String> {
        let headers = HeaderMap::new();
        assert!(matches!(
            require_idempotency_key(&headers, true),
            Err(IdempotencyError::MissingKey)
        ));
        assert!(matches!(require_idempotency_key(&headers, false), Ok(None)));

        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", HeaderValue::from_static("abc"));
        assert_eq!(
            require_idempotency_key(&headers, true).map_err(|error| error.to_string())?,
            Some("abc".to_string())
        );
        Ok(())
    }

    #[test]
    fn ledger_replays_stored_response_for_same_key_and_body() -> Result<(), String> {
        let ledger =
            IdempotencyLedger::open(temp_dir("replay")?).map_err(|error| error.to_string())?;
        let request_hash = "hash-a";
        assert!(matches!(
            ledger
                .begin_request("key-a", request_hash, 1_000)
                .map_err(|error| error.to_string())?,
            IdempotencyOutcome::New
        ));

        ledger
            .store_response(
                "key-a",
                CachedResponse {
                    status: 200,
                    headers_json: "{}".to_string(),
                    body: b"ok".to_vec(),
                },
            )
            .map_err(|error| error.to_string())?;

        match ledger
            .begin_request("key-a", request_hash, 1_001)
            .map_err(|error| error.to_string())?
        {
            IdempotencyOutcome::Replay(response) => {
                assert_eq!(response.status, 200);
                assert_eq!(response.body, b"ok");
            }
            other => return Err(format!("expected replay, got {other:?}")),
        }
        Ok(())
    }

    #[test]
    fn ledger_rejects_same_key_with_different_body() -> Result<(), String> {
        let ledger =
            IdempotencyLedger::open(temp_dir("conflict")?).map_err(|error| error.to_string())?;
        ledger
            .begin_request("key-a", "hash-a", 1_000)
            .map_err(|error| error.to_string())?;

        assert!(matches!(
            ledger.begin_request("key-a", "hash-b", 1_001),
            Err(IdempotencyError::KeyConflict)
        ));
        Ok(())
    }

    #[test]
    fn expired_keys_are_swept_after_ttl() -> Result<(), String> {
        let ledger =
            IdempotencyLedger::open(temp_dir("expired")?).map_err(|error| error.to_string())?;
        ledger
            .begin_request("key-a", "hash-a", 1_000)
            .map_err(|error| error.to_string())?;

        let removed = ledger
            .sweep_expired(1_000 + 24 * 60 * 60 + 1)
            .map_err(|error| error.to_string())?;

        assert_eq!(removed, 1);
        assert!(matches!(
            ledger
                .begin_request("key-a", "hash-b", 1_000 + 24 * 60 * 60 + 2)
                .map_err(|error| error.to_string())?,
            IdempotencyOutcome::New
        ));
        Ok(())
    }

    #[tokio::test]
    async fn middleware_replays_cached_write_response() -> Result<(), String> {
        let ledger = Arc::new(
            IdempotencyLedger::open(temp_dir("middleware")?).map_err(|error| error.to_string())?,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let app = Router::new()
            .route(
                "/",
                post(move || {
                    let calls = calls_for_handler.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        "created"
                    }
                }),
            )
            .layer(from_fn_with_state(
                state(ledger),
                super::idempotency_middleware,
            ));

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add_rule","arguments":{"content":"hello"}}}"#;
        let first = app
            .clone()
            .oneshot(write_request(body)?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(first.status(), StatusCode::OK);
        let second = app
            .oneshot(write_request(body)?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(second.status(), StatusCode::OK);
        let second_body = to_bytes(second.into_body(), 1024)
            .await
            .map_err(|error| error.to_string())?;

        assert_eq!(second_body.as_ref(), b"created");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    fn write_request(body: &str) -> Result<Request<Body>, String> {
        Request::builder()
            .method("POST")
            .uri("/")
            .header("idempotency-key", "abc")
            .body(Body::from(body.to_string()))
            .map_err(|error| error.to_string())
    }

    fn write_request_with_key(body: &str, key: &str) -> Result<Request<Body>, String> {
        Request::builder()
            .method("POST")
            .uri("/")
            .header("idempotency-key", key)
            .body(Body::from(body.to_string()))
            .map_err(|error| error.to_string())
    }

    /// A 200 response must be cached: the second call with the same
    /// Idempotency-Key must return the cached bytes without invoking the
    /// handler a second time.
    #[tokio::test]
    async fn successful_response_is_cached() -> Result<(), String> {
        let ledger = Arc::new(
            IdempotencyLedger::open(temp_dir("cached_ok")?).map_err(|error| error.to_string())?,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let app = Router::new()
            .route(
                "/",
                post(move || {
                    let calls = calls_for_handler.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        (StatusCode::OK, "ok-body")
                    }
                }),
            )
            .layer(from_fn_with_state(
                state(ledger),
                super::idempotency_middleware,
            ));

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add_rule","arguments":{"content":"cached-ok"}}}"#;

        let first = app
            .clone()
            .oneshot(write_request_with_key(body, "key-cached-ok")?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(first.status(), StatusCode::OK);

        let second = app
            .oneshot(write_request_with_key(body, "key-cached-ok")?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(second.status(), StatusCode::OK);

        let second_body = to_bytes(second.into_body(), 1024)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(second_body.as_ref(), b"ok-body");
        // Handler must have been called exactly once (second was a replay).
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        Ok(())
    }

    /// A 500 response must NOT be cached: the second call with the same
    /// Idempotency-Key must re-invoke the handler, not replay the error.
    #[tokio::test]
    async fn server_error_is_not_cached() -> Result<(), String> {
        let ledger = Arc::new(
            IdempotencyLedger::open(temp_dir("not_cached_500")?)
                .map_err(|error| error.to_string())?,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let app = Router::new()
            .route(
                "/",
                post(move || {
                    let calls = calls_for_handler.clone();
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            (StatusCode::INTERNAL_SERVER_ERROR, "first-error")
                        } else {
                            (StatusCode::OK, "second-ok")
                        }
                    }
                }),
            )
            .layer(from_fn_with_state(
                state(ledger),
                super::idempotency_middleware,
            ));

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add_rule","arguments":{"content":"not-cached-500"}}}"#;

        // First call: handler returns 500 — should NOT be cached.
        let first = app
            .clone()
            .oneshot(write_request_with_key(body, "key-not-cached-500")?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // Second call: slot was released, so handler is invoked again.
        let second = app
            .oneshot(write_request_with_key(body, "key-not-cached-500")?)
            .await
            .map_err(|error| error.to_string())?;
        // Handler was re-invoked (call count == 2) and returned 200 this time.
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        Ok(())
    }

    /// After a non-2xx response the in-progress slot must be fully released:
    /// a THIRD call with the same key must also be treated as New (handler
    /// invoked), not InProgress, and once the 200 is cached, a fourth call
    /// gets a replay without hitting the handler again.
    #[tokio::test]
    async fn in_progress_slot_released_after_failure() -> Result<(), String> {
        let ledger = Arc::new(
            IdempotencyLedger::open(temp_dir("slot_released")?)
                .map_err(|error| error.to_string())?,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let app = Router::new()
            .route(
                "/",
                post(move || {
                    let calls = calls_for_handler.clone();
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            (StatusCode::INTERNAL_SERVER_ERROR, "error-on-first")
                        } else {
                            (StatusCode::OK, "ok-on-retry")
                        }
                    }
                }),
            )
            .layer(from_fn_with_state(
                state(ledger),
                super::idempotency_middleware,
            ));

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add_rule","arguments":{"content":"slot-released"}}}"#;

        // Call 1: 500, slot released.
        let r1 = app
            .clone()
            .oneshot(write_request_with_key(body, "key-slot-released")?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(r1.status(), StatusCode::INTERNAL_SERVER_ERROR);
        // Handler invoked once.
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Call 2: handler re-invoked (New, not InProgress), returns 200.
        let r2 = app
            .clone()
            .oneshot(write_request_with_key(body, "key-slot-released")?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(r2.status(), StatusCode::OK);
        // Handler invoked a second time.
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // Call 3: slot now has a 200 cached, so it should be replayed
        // (handler NOT invoked again — call count stays at 2).
        let r3 = app
            .oneshot(write_request_with_key(body, "key-slot-released")?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(r3.status(), StatusCode::OK);
        // Still 2: call 3 was a replay, handler not invoked.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        Ok(())
    }

    /// When the handler returns a body that exceeds
    /// `IDEMPOTENCY_BODY_LIMIT_BYTES`, the middleware short-circuits with a
    /// 500. The in-progress slot must still be released so the next retry is
    /// treated as New, otherwise the key stays InProgress for 24h (Codex P1).
    #[tokio::test]
    async fn slot_released_when_response_body_exceeds_limit() -> Result<(), String> {
        let ledger = Arc::new(
            IdempotencyLedger::open(temp_dir("body_limit_release")?)
                .map_err(|error| error.to_string())?,
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_handler = calls.clone();
        let app = Router::new()
            .route(
                "/",
                post(move || {
                    let calls = calls_for_handler.clone();
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            // Oversized body: triggers the to_bytes length-limit
                            // failure inside cache_response.
                            let huge = vec![b'x'; IDEMPOTENCY_BODY_LIMIT_BYTES + 1];
                            (StatusCode::OK, huge).into_response()
                        } else {
                            (StatusCode::OK, "small-ok").into_response()
                        }
                    }
                }),
            )
            .layer(from_fn_with_state(
                state(ledger),
                super::idempotency_middleware,
            ));

        let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"add_rule","arguments":{"content":"body-limit"}}}"#;

        // Call 1: middleware fails to read the oversized response body and
        // returns 500. Without the fix, the slot is left InProgress.
        let r1: Response = app
            .clone()
            .oneshot(write_request_with_key(body, "key-body-limit")?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(r1.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Call 2: must NOT be 409 InProgress. Handler must be re-invoked and
        // succeed, proving the slot was released.
        let r2 = app
            .oneshot(write_request_with_key(body, "key-body-limit")?)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(r2.status(), StatusCode::OK);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let r2_body = to_bytes(r2.into_body(), 1024)
            .await
            .map_err(|error| error.to_string())?;
        assert_eq!(r2_body.as_ref(), b"small-ok");
        Ok(())
    }

    #[test]
    fn scoped_keys_separate_projects_and_actors() {
        let mut extensions_a = axum::http::Extensions::new();
        extensions_a.insert(crate::transport::auth::AuthenticatedActor {
            actor_id: "actor-a".to_string(),
            key_name: None,
            key_hash: "hash-a".to_string(),
        });
        let mut extensions_b = axum::http::Extensions::new();
        extensions_b.insert(crate::transport::auth::AuthenticatedActor {
            actor_id: "actor-b".to_string(),
            key_name: None,
            key_hash: "hash-b".to_string(),
        });

        let key_a = super::scoped_idempotency_key(&extensions_a, "project-a", "client-key");
        let key_b = super::scoped_idempotency_key(&extensions_b, "project-a", "client-key");
        let key_c = super::scoped_idempotency_key(&extensions_a, "project-b", "client-key");

        assert_ne!(key_a, key_b);
        assert_ne!(key_a, key_c);
        assert!(key_a.ends_with(":client-key"));
    }
}
