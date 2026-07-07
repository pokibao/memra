//! MCP service definition for Memra.
//!
//! Hosts the full MCP tool surface backed by [`SearchEngine`],
//! [`WriteOrchestrator`], [`GovernanceService`], and [`ExperienceStore`]:
//! read tools (search_rules, search_checkpoints, get_context),
//! write tools (add_rule, save_checkpoint, propose_change, approve_change),
//! and experience tools (search_experiences, review_experience, ...).

use std::path::PathBuf;
use std::sync::Arc;

/// Safe UTF-8 truncation by byte budget (used for raw byte-bounded outputs).
fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Char-based truncation with "..." marker, matching Python
/// `normalize_checkpoint_text` / `normalize_checkpoint_list` semantics.
/// One CJK character counts as one char (not 3 bytes).
fn safe_truncate_chars(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    if max_chars <= 3 {
        return s.chars().take(max_chars).collect();
    }
    let kept: String = s.chars().take(max_chars - 3).collect();
    format!("{kept}...")
}

use chrono::DateTime;
use chrono::{SecondsFormat, Utc};
use memra_core::core::write_orchestrator::{AddMemoryParams, AddMemoryResult, WriteOrchestrator};
use memra_core::experience::ExperienceStore;
use memra_core::governance::GovernanceService;
use memra_core::retrieval::search::SearchEngine;
use memra_core::storage::cold_storage::ColdStorageWriter;
use memra_core::storage::db::{DbPool, SearchDiagnostics, SearchParams, SearchResult};
use memra_core::storage::session_tokens_writer::{ValidateResult, validate_token};
use rmcp::{
    RoleServer, handler::server::wrapper::Parameters, schemars, service::RequestContext, tool,
};

use crate::audit::{AuditEvent, AuditLogger};
use crate::transport::admission::{WriteAdmission, write_admission_error_json};
use crate::transport::auth::{AuthenticatedActor, hash_bearer_token};

/// Header name for the write token.
pub const WRITE_TOKEN_HEADER: &str = "x-ma-write-token";

const DEFAULT_MIN_SCORE: f64 = 0.5;

/// Error returned by `require_write_token`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteTokenError {
    /// Header absent on HTTP path.
    Missing,
    /// Token exists but has expired.
    Expired { expires_at: String },
    /// Token has been revoked.
    Revoked { revoked_at: String },
    /// Token does not bind to the requesting actor.
    ActorMismatch { stored_actor: String },
    /// Token was minted for a different session than the current request
    /// claims (PR #235 Codex P1 — cross-session replay defence).
    SessionMismatch {
        stored_session: String,
        request_session: String,
    },
    /// Internal DB error.
    DbError(String),
}

fn format_process_session_id(now: DateTime<Utc>) -> String {
    now.format("%Y%m%d_%H%M%S").to_string()
}

/// Serialize a `WriteTokenError` to the JSON error string returned to callers.
fn write_token_error_json(err: WriteTokenError) -> String {
    match err {
        WriteTokenError::Missing => {
            r#"{"error":"write_token_required","hint":"POST /session/open to obtain a write token","status":401}"#.to_string()
        }
        WriteTokenError::Expired { expires_at } => {
            format!(
                r#"{{"error":"write_token_expired","expires_at":"{expires_at}","status":401}}"#
            )
        }
        WriteTokenError::Revoked { revoked_at } => {
            format!(
                r#"{{"error":"write_token_revoked","revoked_at":"{revoked_at}","status":403}}"#
            )
        }
        WriteTokenError::ActorMismatch { stored_actor } => {
            // Do NOT echo the stored_actor in the response — that would leak
            // information about other sessions. Just indicate mismatch.
            let _ = stored_actor; // suppress unused warning
            r#"{"error":"write_token_actor_mismatch","status":403}"#.to_string()
        }
        WriteTokenError::SessionMismatch {
            stored_session,
            request_session,
        } => {
            // PR #235 Codex P1: do NOT echo stored_session_id in the response —
            // exposing other sessions' ids could enable target-finding for
            // replay attacks. The audit log captures both for forensics.
            let _ = stored_session;
            let _ = request_session;
            r#"{"error":"write_token_session_mismatch","status":403}"#.to_string()
        }
        WriteTokenError::DbError(msg) => {
            tracing::error!("write_token DB error: {msg}");
            r#"{"error":"write_token_internal_error","status":500}"#.to_string()
        }
    }
}

#[derive(Clone)]
pub struct MemraService {
    project_id: String,
    engine: Arc<SearchEngine>,
    writer: Arc<WriteOrchestrator>,
    governance: Arc<GovernanceService>,
    experience: Arc<ExperienceStore>,
    write_admission: WriteAdmission,
    audit: AuditLogger,
    session_id: Arc<String>,
    /// Dedicated DB pool for session_tokens lookups (separate from write pool
    /// to avoid lock contention during governance operations).
    token_pool: Arc<DbPool>,
}

impl std::fmt::Debug for MemraService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemraService").finish()
    }
}

impl MemraService {
    /// Create a new service backed by the given DB path.
    ///
    /// Opens two connections: one for reads (SearchEngine), one for writes (WriteOrchestrator).
    /// SQLite WAL mode allows concurrent read + write.
    pub fn with_db(db_path: PathBuf, project_id: String) -> anyhow::Result<Self> {
        // Read connection
        let read_pool = DbPool::open(&db_path)?;
        let engine = SearchEngine::new(read_pool);

        // Write connection
        let write_pool = DbPool::open(&db_path)?;
        let cold_dir = std::env::var("MA_COLD_STORAGE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| dirs_or_home().join(".memra").join("cold_storage"));
        let cold_enabled = std::env::var("MA_COLD_STORAGE_ENABLED")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);
        let cold_storage = if cold_enabled {
            ColdStorageWriter::new(cold_dir, project_id.clone())
        } else {
            ColdStorageWriter::disabled()
        };
        let writer = WriteOrchestrator::new(write_pool, cold_storage, project_id.clone());

        // Governance uses its own connection
        let gov_pool = DbPool::open(&db_path)?;
        let governance = GovernanceService::with_project(gov_pool, project_id.clone());
        governance.init_table();

        let experience_pool = DbPool::open(&db_path)?;
        let experience = ExperienceStore::with_db(experience_pool, project_id.clone());

        // Token validation pool (read-only queries against session_tokens).
        let token_pool = DbPool::open(&db_path)?;

        Ok(Self {
            project_id,
            engine: Arc::new(engine),
            writer: Arc::new(writer),
            governance: Arc::new(governance),
            experience: Arc::new(experience),
            write_admission: WriteAdmission::default(),
            audit: AuditLogger::default(),
            session_id: Arc::new(format_process_session_id(Utc::now())),
            token_pool: Arc::new(token_pool),
        })
    }

    /// Create a stub service (no DB, all tools return stubs).
    pub fn stub() -> Self {
        let read_pool = DbPool::open_readonly(std::path::Path::new(":memory:"))
            .expect("in-memory DB should open");
        let write_pool =
            DbPool::open(std::path::Path::new(":memory:")).expect("in-memory DB should open");
        let gov_pool =
            DbPool::open(std::path::Path::new(":memory:")).expect("in-memory DB should open");
        let exp_pool =
            DbPool::open(std::path::Path::new(":memory:")).expect("in-memory DB should open");
        let token_pool =
            DbPool::open(std::path::Path::new(":memory:")).expect("in-memory DB should open");
        let cold_storage = ColdStorageWriter::disabled();
        let writer = WriteOrchestrator::new(write_pool, cold_storage, "stub".into());
        let governance = GovernanceService::with_project(gov_pool, "stub");
        governance.init_table();
        let experience = ExperienceStore::with_db(exp_pool, "stub");
        Self {
            project_id: "stub".to_string(),
            engine: Arc::new(SearchEngine::new(read_pool)),
            writer: Arc::new(writer),
            governance: Arc::new(governance),
            experience: Arc::new(experience),
            write_admission: WriteAdmission::default(),
            audit: AuditLogger::default(),
            session_id: Arc::new(format_process_session_id(Utc::now())),
            token_pool: Arc::new(token_pool),
        }
    }

    pub fn with_write_admission(mut self, write_admission: WriteAdmission) -> Self {
        self.write_admission = write_admission;
        self
    }

    /// Return the token pool for use in the HTTP session/open endpoint.
    pub fn token_pool(&self) -> Arc<DbPool> {
        Arc::clone(&self.token_pool)
    }

    /// Resolve the session_id for a write operation.
    ///
    /// HTTP path: if the `SessionId` extension was inserted by `session_middleware`,
    /// return that value (already prefixed "http:" or "http-fallback:").
    /// stdio path: return `"stdio:<process_session_id>"` using the Arc<String>
    /// minted at service construction time.
    pub fn resolve_session_id(&self, ctx: &RequestContext<RoleServer>) -> String {
        use crate::transport::session::SessionId;
        if let Some(parts) = ctx.extensions.get::<axum::http::request::Parts>() {
            if let Some(session_id) = parts.extensions.get::<SessionId>() {
                return session_id.0.clone();
            }
        }
        self.stdio_session_id()
    }

    /// Return the stdio-prefixed process session id.
    ///
    /// Used as fallback by `resolve_session_id` and exposed for unit tests.
    pub fn stdio_session_id(&self) -> String {
        format!("stdio:{}", *self.session_id)
    }

    fn audit_operation(
        &self,
        event_type: &'static str,
        result: &str,
        actor: Option<&AuthenticatedActor>,
    ) {
        let mut event = AuditEvent::new(event_type, result);
        if let Some(a) = actor {
            event = event.with_actor(a.actor_id.as_str());
        }
        if let Err(error) = self.audit.append(event) {
            tracing::warn!("failed to append audit event {event_type}: {error}");
        }
    }

    /// Like `audit_operation` but also attaches the resolved `session_id` from
    /// the request context. Use this for tool handlers that receive a
    /// `RequestContext<RoleServer>` (e.g. `add_rule`, `report_outcome`).
    pub(crate) fn audit_operation_with_session(
        &self,
        event_type: &'static str,
        result: &str,
        ctx: &RequestContext<RoleServer>,
        actor: Option<&AuthenticatedActor>,
    ) {
        let sid = self.resolve_session_id(ctx);
        let mut event = AuditEvent::new(event_type, result).with_session_id(sid);
        if let Some(a) = actor {
            event = event.with_actor(a.actor_id.as_str());
        }
        if let Err(error) = self.audit.append(event) {
            tracing::warn!("failed to append audit event {event_type}: {error}");
        }
    }

    /// Validate an `X-MA-Write-Token` header for L0 governance mutations.
    ///
    /// Only enforced on the HTTP path (when axum HTTP request parts are present
    /// in the `RequestContext` extensions). On the stdio path there are no HTTP
    /// headers and this check is a no-op (stdio callers are assumed to be
    /// trusted local processes).
    ///
    /// Returns `Ok(())` if:
    /// - We are on the stdio path (no HTTP parts), OR
    /// - The token is present, valid, non-expired, non-revoked, and binds to
    ///   the requesting actor.
    pub(crate) fn require_write_token(
        &self,
        ctx: &RequestContext<RoleServer>,
        actor: &AuthenticatedActor,
    ) -> Result<(), WriteTokenError> {
        // Only enforce on the HTTP path.
        let parts = match ctx.extensions.get::<axum::http::request::Parts>() {
            Some(p) => p,
            None => {
                // stdio path — no token enforcement.
                return Ok(());
            }
        };
        self.require_write_token_from_parts(parts, actor)
    }

    /// Inner validation accepting pre-extracted `Parts` so unit tests can
    /// exercise the logic without constructing a `RequestContext` (which is
    /// non-exhaustive in rmcp and cannot be built from test code).
    pub(crate) fn require_write_token_from_parts(
        &self,
        parts: &axum::http::request::Parts,
        actor: &AuthenticatedActor,
    ) -> Result<(), WriteTokenError> {
        // Extract the header value.
        let token_secret = match parts.headers.get(WRITE_TOKEN_HEADER) {
            Some(v) => match v.to_str() {
                Ok(s) if !s.is_empty() => s.to_string(),
                _ => return Err(WriteTokenError::Missing),
            },
            None => return Err(WriteTokenError::Missing),
        };

        // PR #235 Codex P1 fix — cross-session replay defence:
        // resolve the request's bound session_id (set by session_middleware
        // from X-MA-Session-Id header, or http-fallback when missing). If a
        // valid token's stored session_id does not match the request's
        // session_id, reject — even when actor_id matches. This blocks the
        // attack where an actor mints a token under session A then replays
        // it under session B with the same bearer key.
        let request_session_id = parts
            .extensions
            .get::<crate::transport::session::SessionId>()
            .map(|s| s.0.clone())
            .unwrap_or_else(|| self.stdio_session_id());

        // Hash the secret and validate against the DB.
        let token_hash = hash_bearer_token(&token_secret);
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);

        let result = self
            .token_pool
            .with_conn(|conn| validate_token(conn, &token_hash, &actor.actor_id, &now));

        let validate_result = match result {
            Ok(r) => r,
            Err(e) => return Err(WriteTokenError::DbError(e.to_string())),
        };

        match validate_result {
            ValidateResult::Valid {
                session_id: stored_session_id,
                ..
            } => {
                // PR #235 Codex P1: enforce session binding.
                if stored_session_id != request_session_id {
                    let _ = self.audit.append(
                        AuditEvent::new("session_token_validation_failed", "session_mismatch")
                            .with_actor(&actor.actor_id)
                            .with_session_id(request_session_id.clone())
                            .with_field("stored_session_id", stored_session_id.as_str()),
                    );
                    return Err(WriteTokenError::SessionMismatch {
                        stored_session: stored_session_id,
                        request_session: request_session_id,
                    });
                }
                // Audit successful validation.
                let _ = self.audit.append(
                    AuditEvent::new("session_token_validated", "ok")
                        .with_actor(&actor.actor_id)
                        .with_session_id(stored_session_id),
                );
                Ok(())
            }
            ValidateResult::NotFound => {
                let _ = self.audit.append(
                    AuditEvent::new("session_token_validation_failed", "not_found")
                        .with_actor(&actor.actor_id),
                );
                Err(WriteTokenError::Missing)
            }
            ValidateResult::Expired { expires_at } => {
                let _ = self.audit.append(
                    AuditEvent::new("session_token_validation_failed", "expired")
                        .with_actor(&actor.actor_id)
                        .with_field("expires_at", expires_at.as_str()),
                );
                Err(WriteTokenError::Expired { expires_at })
            }
            ValidateResult::Revoked { revoked_at } => {
                let _ = self.audit.append(
                    AuditEvent::new("session_token_validation_failed", "revoked")
                        .with_actor(&actor.actor_id)
                        .with_field("revoked_at", revoked_at.as_str()),
                );
                Err(WriteTokenError::Revoked { revoked_at })
            }
            ValidateResult::ActorMismatch { stored_actor, .. } => {
                let _ = self.audit.append(
                    AuditEvent::new("session_token_validation_failed", "actor_mismatch")
                        .with_actor(&actor.actor_id)
                        .with_field("stored_actor", stored_actor.as_str()),
                );
                Err(WriteTokenError::ActorMismatch { stored_actor })
            }
        }
    }

    fn propose_change_with_actor(
        &self,
        actor: &AuthenticatedActor,
        params: ProposeChangeParams,
    ) -> String {
        let _permit = match self.write_admission.try_enter() {
            Ok(permit) => permit,
            Err(error) => {
                self.audit_operation("propose_change", "admission_rejected", Some(actor));
                return write_admission_error_json(error);
            }
        };

        match self.governance.propose_change(
            &params.proposed_content,
            &params.reason,
            params.change_type.as_deref(),
            params.target_id.as_deref(),
            params.category.as_deref(),
            Some(actor.actor_id.as_str()),
        ) {
            Ok(record) => {
                self.audit_operation("propose_change", "pending", Some(actor));
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "pending",
                    "change_id": record.id,
                    "change_type": record.change_type,
                    "target_id": record.target_id,
                    "category": record.category,
                    "approvals_needed": record.approvals_needed,
                    "message": format!(
                        "变更已提议，需要 {} 次审批才能生效。",
                        record.approvals_needed
                    ),
                }))
                .unwrap_or_else(|_| r#"{"error":"serialize failed"}"#.to_string())
            }
            Err(e) => {
                self.audit_operation("propose_change", "error", Some(actor));
                format!(r#"{{"error":"{e}"}}"#)
            }
        }
    }

    fn approve_change_with_actor(
        &self,
        actor: &AuthenticatedActor,
        params: ApproveChangeParams,
    ) -> String {
        let _permit = match self.write_admission.try_enter() {
            Ok(permit) => permit,
            Err(error) => {
                self.audit_operation("approve_change", "admission_rejected", Some(actor));
                return write_admission_error_json(error);
            }
        };

        // D3.1: Use the stable actor_id from resolve_actor (either HTTP bearer or
        // stdio-{pid}-{exe_hash}). Governance dedup in approve_change_for_principal
        // ensures the same principal cannot approve more than once.
        let display_approver = params
            .approver
            .as_deref()
            .unwrap_or(actor.actor_id.as_str());
        let result = self.governance.approve_change_for_principal(
            &params.change_id,
            &actor.actor_id,
            Some(display_approver),
            params.comment.as_deref(),
        );

        match result {
            Ok(record) => {
                self.audit_operation("approve_change", &record.status, Some(actor));
                // Status is 'applied' (not 'approved') when threshold is reached —
                // apply_change_inner runs inside the same transaction and sets
                // status='applied' directly.  Any apply failure returns Err and
                // is surfaced to the client in the branch below.
                let applied = record.status == "applied";
                let emoji = if applied { "✅" } else { "🔄" };
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": record.status,
                    "change_id": record.id,
                    "approvals_count": record.approvals_count,
                    "approvals_needed": record.approvals_needed,
                    "applied_at": record.applied_at,
                    "message": format!(
                        "{emoji} 审批 {}/{}{}",
                        record.approvals_count,
                        record.approvals_needed,
                        if applied { " — 变更已应用！" } else { "" }
                    ),
                }))
                .unwrap_or_else(|_| r#"{"error":"serialize failed"}"#.to_string())
            }
            Err(e) => {
                self.audit_operation("approve_change", "error", Some(actor));
                format!(r#"{{"error":"{e}"}}"#)
            }
        }
    }

    fn save_checkpoint_with_actor(
        &self,
        actor: &AuthenticatedActor,
        params: SaveCheckpointParams,
    ) -> String {
        if let Err(msg) = validate_checkpoint_params(&params) {
            self.audit_operation("save_checkpoint", "invalid_input", Some(actor));
            return msg;
        }

        let _permit = match self.write_admission.try_enter() {
            Ok(permit) => permit,
            Err(error) => {
                self.audit_operation("save_checkpoint", "admission_rejected", Some(actor));
                return write_admission_error_json(error);
            }
        };

        let task_status = params.task_status.as_deref().unwrap_or("blocked");
        let meta = build_checkpoint_metadata(&params, task_status);

        match self
            .writer
            .save_checkpoint(&params.task_id, &params.summary, meta)
        {
            Ok(id) => {
                self.audit_operation("save_checkpoint", task_status, Some(actor));
                let status_icon = match task_status {
                    "blocked" => "🔴",
                    "in_progress" => "🟡",
                    "completed" => "✅",
                    "abandoned" => "⚫",
                    _ => "⚪",
                };
                format!(
                    "{status_icon} 断点已保存\n- 任务: {}\n- 状态: {task_status}\n- ID: {id}",
                    params.task_id
                )
            }
            Err(e) => {
                self.audit_operation("save_checkpoint", "error", Some(actor));
                format!("❌ 断点保存失败: {e}")
            }
        }
    }

    pub fn save_checkpoint_from_trusted_cli(&self, params: SaveCheckpointParams) -> String {
        let actor = trusted_stdio_actor();
        self.save_checkpoint_with_actor(&actor, params)
    }
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Mirror Python `_handle_save_checkpoint` input validation
/// (mcp_memory.py:1646-1654): reject empty task_id/summary and task_status
/// values outside the canonical whitelist.
fn validate_checkpoint_params(params: &SaveCheckpointParams) -> Result<(), String> {
    if params.task_id.trim().is_empty() || params.summary.trim().is_empty() {
        return Err("❌ 错误：task_id 和 summary 是必填项".to_string());
    }
    if let Some(status) = params.task_status.as_deref() {
        const VALID: [&str; 4] = ["blocked", "in_progress", "completed", "abandoned"];
        if !VALID.contains(&status) {
            return Err(format!(
                "❌ 错误：task_status 必须是 abandoned, blocked, completed, in_progress 之一，收到: {status}"
            ));
        }
    }
    Ok(())
}

fn build_checkpoint_metadata(
    params: &SaveCheckpointParams,
    task_status: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let mut meta = serde_json::Map::new();
    meta.insert("record_type".to_string(), "checkpoint".into());
    meta.insert("task_id".to_string(), params.task_id.clone().into());
    meta.insert("task_status".to_string(), task_status.into());

    insert_text(&mut meta, "blocker", params.blocker.as_deref());
    insert_text(&mut meta, "next_step", params.next_step.as_deref());
    insert_text(&mut meta, "task_context", params.task_context.as_deref());
    if let Some(trigger_patterns) = normalized_list(params.trigger_patterns.as_deref()) {
        meta.insert("trigger_patterns".to_string(), trigger_patterns.into());
    }

    let compact_sections = build_checkpoint_sections(params);
    if !compact_sections.is_empty() {
        meta.insert("compact_schema_version".to_string(), 1.into());
        meta.insert(
            "compact_sections".to_string(),
            serde_json::Value::Object(compact_sections),
        );
    }

    meta
}

fn build_checkpoint_sections(
    params: &SaveCheckpointParams,
) -> serde_json::Map<String, serde_json::Value> {
    let mut sections = serde_json::Map::new();
    insert_text(
        &mut sections,
        "task_specification",
        params.task_specification.as_deref(),
    );
    insert_list(
        &mut sections,
        "files_and_functions",
        params.files_and_functions.as_deref(),
    );
    insert_list(&mut sections, "workflow", params.workflow.as_deref());
    insert_list(
        &mut sections,
        "errors_and_corrections",
        params.errors_and_corrections.as_deref(),
    );
    insert_list(&mut sections, "decisions", params.decisions.as_deref());
    insert_list(&mut sections, "living_docs", params.living_docs.as_deref());
    sections
}

fn insert_text(
    target: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: Option<&str>,
) {
    if let Some(value) = value.and_then(normalized_text) {
        target.insert(key.to_string(), value.into());
    }
}

fn insert_list(
    target: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    values: Option<&[String]>,
) {
    if let Some(values) = normalized_list(values) {
        target.insert(key.to_string(), values.into());
    }
}

fn normalized_text(value: &str) -> Option<String> {
    let text = value.trim();
    if text.is_empty() {
        return None;
    }
    Some(safe_truncate_chars(text, 500))
}

fn normalized_list(values: Option<&[String]>) -> Option<Vec<String>> {
    let mut items = Vec::new();
    for raw in values.unwrap_or_default() {
        let item = raw.trim().trim_start_matches(['-', '*', '•']).trim();
        if item.is_empty() {
            continue;
        }
        let item = safe_truncate_chars(item, 140);
        if !items.contains(&item) {
            items.push(item);
        }
        if items.len() >= 8 {
            break;
        }
    }
    if items.is_empty() { None } else { Some(items) }
}

#[derive(Debug, Clone)]
struct AssociationTrace {
    associated_from: String,
    association_root: Option<String>,
    relation_type: String,
    relation_strength: Option<f64>,
    activation_depth: Option<u64>,
    activation_path: Vec<String>,
    dream_candidate_id: Option<String>,
}

fn association_trace(metadata: Option<&serde_json::Value>) -> Option<AssociationTrace> {
    let meta = metadata?;
    if meta
        .get("is_association")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return None;
    }
    let associated_from = meta
        .get("associated_from")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();
    let relation_type = meta
        .get("relation_type")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("association")
        .to_string();
    let relation_strength = meta
        .get("relation_strength")
        .and_then(serde_json::Value::as_f64);
    let association_root = meta
        .get("association_root")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let activation_depth = meta
        .get("activation_depth")
        .and_then(serde_json::Value::as_u64);
    let activation_path = meta
        .get("activation_path")
        .and_then(serde_json::Value::as_array)
        .map(|path| {
            path.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let dream_candidate_id = meta
        .get("dream_candidate_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    Some(AssociationTrace {
        associated_from,
        association_root,
        relation_type,
        relation_strength,
        activation_depth,
        activation_path,
        dream_candidate_id,
    })
}

fn association_selected_reason(trace: &AssociationTrace) -> String {
    let mut reason = format!(
        "Rust neural activation via {} from {}",
        trace.relation_type, trace.associated_from
    );
    if let Some(strength) = trace.relation_strength {
        reason.push_str(&format!(" (strength={strength:.3})"));
    }
    if let Some(depth) = trace.activation_depth {
        reason.push_str(&format!(" (depth={depth})"));
    }
    if let Some(root) = trace.association_root.as_deref() {
        reason.push_str(&format!(" (root={root})"));
    }
    if let Some(candidate_id) = trace.dream_candidate_id.as_deref() {
        reason.push_str(&format!(" (dream_candidate_id={candidate_id})"));
    }
    reason
}

fn association_manifest(trace: &AssociationTrace) -> serde_json::Value {
    serde_json::json!({
        "associated_from": trace.associated_from,
        "association_root": trace.association_root,
        "relation_type": trace.relation_type,
        "relation_strength": trace.relation_strength,
        "activation_depth": trace.activation_depth,
        "activation_path": trace.activation_path,
        "dream_candidate_id": trace.dream_candidate_id,
    })
}

fn association_output_line(trace: &AssociationTrace) -> String {
    let mut line = format!(
        "   🔗 association: {} from {}",
        trace.relation_type, trace.associated_from
    );
    if let Some(strength) = trace.relation_strength {
        line.push_str(&format!(" strength={strength:.3}"));
    }
    if let Some(depth) = trace.activation_depth {
        line.push_str(&format!(" depth={depth}"));
    }
    if !trace.activation_path.is_empty() {
        line.push_str(&format!(" path={}", trace.activation_path.join(" -> ")));
    }
    if let Some(candidate_id) = trace.dream_candidate_id.as_deref() {
        line.push_str(&format!(" dream_candidate_id={candidate_id}"));
    }
    line.push('\n');
    line
}

fn ensure_search_observability_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS query_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            query TEXT NOT NULL,
            top_result_ids TEXT,
            top_scores TEXT,
            result_count INTEGER DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS recall_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            note_id TEXT NOT NULL,
            recalled_at TEXT NOT NULL DEFAULT (datetime('now')),
            session_id TEXT,
            query_text TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_recall_log_note ON recall_log(note_id);
        CREATE TABLE IF NOT EXISTS activation_trace_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            trace_id TEXT NOT NULL,
            project_id TEXT NOT NULL,
            tool TEXT NOT NULL,
            query TEXT NOT NULL,
            emitted_at TEXT NOT NULL,
            result_count INTEGER NOT NULL DEFAULT 0,
            activation_result_count INTEGER NOT NULL DEFAULT 0,
            activation_max_depth INTEGER NOT NULL DEFAULT 0,
            activation_dream_evidence_count INTEGER NOT NULL DEFAULT 0,
            activation_latency_ms INTEGER NOT NULL DEFAULT 0,
            activation_trace_json TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_activation_trace_project_time
            ON activation_trace_log(project_id, emitted_at);
        CREATE INDEX IF NOT EXISTS idx_activation_trace_depth
            ON activation_trace_log(project_id, activation_max_depth);",
    )
}

fn search_trace_payload(
    trace_id: &str,
    project_id: &str,
    tool: &str,
    query: &str,
    emitted_at: &str,
    visible_results: &[&SearchResult],
    diagnostics: &SearchDiagnostics,
) -> serde_json::Value {
    let final_results = visible_results
        .iter()
        .map(|result| {
            let association = association_trace(result.metadata.as_ref());
            serde_json::json!({
                "id": &result.id,
                "score": result.score,
                "layer": &result.layer,
                "channel": &result.channel,
                "activation": association.as_ref().map(association_manifest),
            })
        })
        .collect::<Vec<_>>();
    let activation_paths = visible_results
        .iter()
        .filter_map(|result| association_trace(result.metadata.as_ref()))
        .map(|trace| association_manifest(&trace))
        .collect::<Vec<_>>();

    serde_json::json!({
        "schema_version": "rust-search-activation-trace/v1",
        "trace_id": trace_id,
        "emitted_at": emitted_at,
        "project_id": project_id,
        "tool": tool,
        "query": query,
        "summary": {
            "result_count": visible_results.len(),
            "activation_result_count": diagnostics.activation_result_count,
            "activation_max_depth": diagnostics.activation_max_depth,
            "activation_dream_evidence_count": diagnostics.activation_dream_evidence_count,
            "activation_latency_ms": diagnostics.activation_latency_ms,
            "relation_type_counts": &diagnostics.activation_relation_type_counts,
        },
        "final_results": final_results,
        "activation_paths": activation_paths,
    })
}

fn manifest_item(query: &str, result: &SearchResult, include_content: bool) -> serde_json::Value {
    let stale_risk = if result.layer == "event_log" {
        "medium"
    } else {
        "low"
    };
    let needs_verification = result.layer != "identity_schema";
    let association = association_trace(result.metadata.as_ref());
    let hook = format!(
        "相关记忆: {}{}",
        safe_truncate(&result.content, 90),
        if needs_verification {
            " | 需验证"
        } else {
            ""
        }
    );
    let mut item = serde_json::json!({
        "id": result.id.clone(),
        "hook": hook,
        "memory_type": result.category.clone(),
        "feedback_kind": result.metadata.as_ref()
            .and_then(|meta| meta.get("feedback_kind"))
            .and_then(serde_json::Value::as_str),
        "living_doc": false,
        "living_docs": [],
        "layer": result.layer.clone(),
        "score": (result.score * 1000.0).round() / 1000.0,
        "selection_score": (result.score * 1000.0).round() / 1000.0,
        "stale_risk": stale_risk,
        "needs_verification": needs_verification,
        "verification_hint": if needs_verification {
            "Rust manifest uses memory-search evidence; verify repo-derived claims before relying on them."
        } else {
            ""
        },
        "selected_reason": association
            .as_ref()
            .map(association_selected_reason)
            .unwrap_or_else(|| "Rust memory search result".to_string()),
        "source_type": if association.is_some() { "memory_association" } else { "memory" },
        "experience_id": null,
        "experience_kind": null,
        "experience_status": null,
        "evidence_refs": [],
    });
    if let Some(trace) = association.as_ref() {
        item["association"] = association_manifest(trace);
        item["evidence_refs"] = serde_json::json!([
            {
                "kind": "associated_from",
                "id": trace.associated_from,
                "relation_type": trace.relation_type,
            }
        ]);
        if let Some(candidate_id) = trace.dream_candidate_id.as_deref() {
            item["evidence_refs"]
                .as_array_mut()
                .expect("evidence_refs set to array")
                .push(serde_json::json!({
                    "kind": "dream_candidate",
                    "id": candidate_id,
                }));
        }
    }
    if include_content {
        item["content"] = serde_json::Value::String(result.content.clone());
        item["why"] = serde_json::Value::String(format!("Matched query: {query}"));
        item["how_to_apply"] = serde_json::Value::String(
            "Use as context after checking freshness and scope.".to_string(),
        );
    }
    item
}

fn unsupported_in_rust_json(tool: &str) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "error": "unsupported_in_rust",
        "tool": tool,
        "message": "Rust MCP received an empty or incomplete probe payload; retry with a complete payload or use the documented rollback path explicitly.",
    }))
    .unwrap_or_else(|_| r#"{"error":"unsupported_in_rust"}"#.to_string())
}

fn missing_or_empty(value: Option<&str>) -> bool {
    value.map(str::is_empty).unwrap_or(true)
}

/// Issue #116: pick the actor defaults for `add_rule` based on whether the
/// payload routes into the event_log layer. Mirrors the Python MCP path:
/// `handle_add_memory` dispatches event payloads to `handle_log_event`,
/// which calls `kernel.log_event(...)` whose `source` default is "ai"
/// (`backend/core/memory_kernel.py:1093`). All non-event writes stay
/// `("user", "user")` matching `backend/mcp_memory.py:1171`.
///
/// Layer inference here mirrors `add_memory_inner` in `memra-core`:
/// `memory_kind == "event"` OR `when` set ⇒ event_log path.
fn infer_add_rule_actor_defaults(
    memory_kind: Option<&str>,
    when: Option<&str>,
) -> (&'static str, &'static str) {
    let is_event_path = memory_kind == Some("event") || when.is_some();
    if is_event_path {
        ("ai", "ai")
    } else {
        ("user", "user")
    }
}

/// Python `handle_add_rule` defaults `memory_kind` to `fact` and injects
/// `layer=memory` before delegating to `handle_add_memory`, so fact writes get
/// category auto-classification but do not let the classifier replace layer.
fn infer_add_rule_auto_classification(memory_kind: Option<&str>) -> (bool, bool) {
    let is_fact_path = memory_kind.unwrap_or("fact") == "fact";
    (is_fact_path, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEMP_DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_actor(id: &str) -> AuthenticatedActor {
        AuthenticatedActor {
            actor_id: id.to_string(),
            key_name: Some("test".to_string()),
            key_hash: "test-hash".to_string(),
        }
    }

    // Issue #116 — Python MCP routes event_log writes through log_event
    // which defaults source="ai" / created_by="ai". Non-event writes use
    // ("user", "user"). Rust's add_rule used to stamp ("user", "user")
    // unconditionally, drifting on every event row. Helper unit tests:

    #[test]
    fn infer_actor_defaults_event_kind_returns_ai() {
        assert_eq!(
            infer_add_rule_actor_defaults(Some("event"), None),
            ("ai", "ai")
        );
    }

    #[test]
    fn infer_actor_defaults_when_field_routes_to_event_log() {
        assert_eq!(
            infer_add_rule_actor_defaults(None, Some("2026-04-15T00:00:00Z")),
            ("ai", "ai")
        );
    }

    #[test]
    fn infer_actor_defaults_fact_kind_returns_user() {
        assert_eq!(
            infer_add_rule_actor_defaults(Some("fact"), None),
            ("user", "user")
        );
    }

    #[test]
    fn infer_actor_defaults_procedure_kind_returns_user() {
        assert_eq!(
            infer_add_rule_actor_defaults(Some("procedure"), None),
            ("user", "user")
        );
    }

    #[test]
    fn infer_actor_defaults_unspecified_returns_user() {
        assert_eq!(infer_add_rule_actor_defaults(None, None), ("user", "user"));
    }

    #[test]
    fn infer_add_rule_auto_classification_fact_matches_python_add_rule() {
        assert_eq!(
            infer_add_rule_auto_classification(Some("fact")),
            (true, false)
        );
        assert_eq!(infer_add_rule_auto_classification(None), (true, false));
    }

    #[test]
    fn infer_add_rule_auto_classification_non_fact_is_disabled() {
        assert_eq!(
            infer_add_rule_auto_classification(Some("event")),
            (false, false)
        );
        assert_eq!(
            infer_add_rule_auto_classification(Some("procedure")),
            (false, false)
        );
    }

    #[test]
    fn process_session_id_matches_python_state_manager_shape() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-04-19T12:34:56Z")
            .expect("fixed timestamp")
            .with_timezone(&chrono::Utc);

        assert_eq!(format_process_session_id(now), "20260419_123456");
    }

    #[test]
    fn mcp_search_default_min_score_is_recalibrated() {
        assert_eq!(DEFAULT_MIN_SCORE, 0.5);
    }

    fn service_with_checkpoint_schema(tag: &str) -> MemraService {
        let db_path = temp_db_path(tag);
        init_notes_schema(&db_path);
        MemraService::with_db(db_path, "test-project".to_string()).expect("service")
    }

    fn temp_db_path(tag: &str) -> PathBuf {
        let n = TEMP_DB_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("ma-service-{tag}-{pid}-{n}-{nanos}.sqlite3"))
    }

    fn init_notes_schema(db_path: &std::path::Path) {
        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_file(db_path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("sqlite3-shm"));
        let conn = rusqlite::Connection::open(db_path).expect("create temp DB");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS notes (
                    id                TEXT PRIMARY KEY,
                    content           TEXT NOT NULL,
                    layer             TEXT,
                    category          TEXT,
                    is_active         INTEGER NOT NULL DEFAULT 1,
                    confidence        REAL,
                    agent_id          TEXT,
                    project_id        TEXT,
                    created_at        TEXT,
                    updated_at        TEXT,
                    valid_at          REAL,
                    metadata_json     TEXT,
                    vector_json       TEXT,
                    vector_blob       BLOB,
                    evolution_state   TEXT DEFAULT 'active',
                    topic_key         TEXT,
                    is_head           INTEGER DEFAULT 1,
                    review_after      TEXT,
                    room              TEXT,
                    agent             TEXT,
                    difficulty        INTEGER,
                    time_cost_hint    TEXT,
                    related_ids_json  TEXT,
                    role              TEXT,
                    session_id        TEXT,
                    source            TEXT,
                    created_by        TEXT,
                    version           INTEGER DEFAULT 1,
                    root_id           TEXT,
                    cold_storage_ref  TEXT,
            event_when        TEXT,
            event_when_ts     REAL
                );
                CREATE INDEX IF NOT EXISTS idx_notes_root_id ON notes(root_id);
                CREATE INDEX IF NOT EXISTS idx_notes_root_version ON notes(root_id, version);
                CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts
                    USING fts5(note_id UNINDEXED, content);",
        )
        .expect("notes schema");
    }

    fn service_with_seeded_memories() -> MemraService {
        let db_path = temp_db_path("seeded");
        init_notes_schema(&db_path);
        {
            let conn = rusqlite::Connection::open(&db_path).expect("open temp DB");
            conn.execute(
                "INSERT INTO notes
                 (id, content, layer, category, is_active, confidence, project_id,
                  created_at, updated_at, room, metadata_json)
                 VALUES
                 ('m1', 'Rust search_manifest remembers release workflow decisions',
                  'verified_fact', 'decision', 1, 0.91, 'test-project',
                  '2026-04-14T00:00:00Z', '2026-04-14T00:00:00Z',
                  'release', '{}')",
                [],
            )
            .expect("insert m1");
            conn.execute(
                "INSERT INTO notes_fts (note_id, content)
                 VALUES ('m1', 'Rust search_manifest remembers release workflow decisions')",
                [],
            )
            .expect("fts m1");
            conn.execute(
                "INSERT INTO notes
                 (id, content, layer, category, is_active, confidence, project_id,
                  created_at, updated_at, room, metadata_json)
                 VALUES
                 ('m2', 'Rust list_rooms counts engineering memories',
                  'event_log', 'event', 1, 0.8, 'test-project',
                  '2026-04-13T00:00:00Z', '2026-04-13T00:00:00Z',
                  'engineering', '{}')",
                [],
            )
            .expect("insert m2");
            conn.execute(
                "INSERT INTO notes_fts (note_id, content)
                 VALUES ('m2', 'Rust list_rooms counts engineering memories')",
                [],
            )
            .expect("fts m2");
        }
        let service = MemraService::with_db(db_path.clone(), "test-project".to_string())
            .expect("service");
        {
            let conn = rusqlite::Connection::open(&db_path).expect("open temp DB");
            conn.execute(
                "INSERT INTO experience_records
                 (id, title, summary, kind, status, origin, project_id, source_note_id,
                  topic_key, confidence, evidence_note_ids_json, metadata_json,
                  created_at, updated_at)
                 VALUES
                 ('exp-1', 'Release workflow SOP',
                  'Pin release actions and verify generated artifacts.',
                  'procedure', 'stable', 'rust_test', 'test-project', 'm1',
                  'verified_fact:release', 0.88, '[\"m1\"]',
                  '{\"activation_cues\":[\"release\",\"artifact\"],\"steps_count\":3}',
                  '2026-04-14T00:00:00Z', '2026-04-14T00:00:00Z')",
                [],
            )
            .expect("insert experience");
        }
        service
    }

    fn seed_note_relation(service: &MemraService, from_id: &str, to_id: &str) {
        service.engine.pool().with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS note_relations (
                    from_note_id TEXT NOT NULL,
                    to_note_id TEXT NOT NULL,
                    relation_type TEXT NOT NULL,
                    strength REAL NOT NULL DEFAULT 0.5,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    PRIMARY KEY (from_note_id, to_note_id, relation_type)
                );",
            )
            .expect("note_relations schema");
            conn.execute(
                "INSERT INTO note_relations
                 (from_note_id, to_note_id, relation_type, strength)
                 VALUES (?1, ?2, 'supports', 0.91)",
                rusqlite::params![from_id, to_id],
            )
            .expect("insert note relation");
        });
    }

    #[test]
    fn full_context_mode_uses_rust_snapshot() {
        let response = MemraService::stub().get_context(Parameters(GetContextParams {
            mode: Some("full".to_string()),
        }));

        let payload: serde_json::Value = serde_json::from_str(&response).expect("json response");
        assert_eq!(payload["mode"], serde_json::json!("full"));
        assert_eq!(payload["project_id"], serde_json::json!("stub"));
        assert!(payload["stats"].is_object());
        assert!(payload["recent_memories"].is_array());
    }

    #[test]
    fn propose_change_forwards_governance_parameters() {
        let service = MemraService::stub();
        let actor = test_actor("actor-proposer");
        let response = service.propose_change_with_actor(
            &actor,
            ProposeChangeParams {
                proposed_content: "updated identity".to_string(),
                reason: "test reason".to_string(),
                change_type: Some("update".to_string()),
                target_id: Some("target-123".to_string()),
                category: Some("preference".to_string()),
            },
        );
        let payload: serde_json::Value =
            serde_json::from_str(&response).expect("propose response JSON");
        let change_id = payload["change_id"]
            .as_str()
            .expect("change_id must be present");

        let row: (String, Option<String>, Option<String>, String) =
            service.governance.pool().with_conn(|conn| {
                conn.query_row(
                    "SELECT change_type, target_id, category, proposer
                     FROM constitution_changes WHERE id = ?1",
                    [change_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("constitution change row")
            });

        assert_eq!(row.0, "update");
        assert_eq!(row.1.as_deref(), Some("target-123"));
        assert_eq!(row.2.as_deref(), Some("preference"));
        assert_eq!(row.3, "actor-proposer");
    }

    #[test]
    fn approve_change_forwards_display_approver_and_comment() {
        let service = MemraService::stub();
        let actor = test_actor("actor-approver");
        let proposal = service
            .governance
            .propose_change("new identity", "test", None, None, None, Some("seed"))
            .expect("proposal");

        let response = service.approve_change_with_actor(
            &actor,
            ApproveChangeParams {
                change_id: proposal.id.clone(),
                approver: Some("Alex display".to_string()),
                comment: Some("looks good".to_string()),
            },
        );
        assert!(
            response.contains(r#""status": "pending""#),
            "first approval should keep change pending: {response}"
        );

        let approvals_json: String = service.governance.pool().with_conn(|conn| {
            conn.query_row(
                "SELECT approvals FROM constitution_changes WHERE id = ?1",
                [&proposal.id],
                |row| row.get(0),
            )
            .expect("approvals row")
        });
        let approvals: Vec<memra_core::governance::ApprovalEntry> =
            serde_json::from_str(&approvals_json).expect("approval JSON");

        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].actor_id.as_deref(), Some("actor-approver"));
        assert_eq!(approvals[0].approver, "Alex display");
        assert_eq!(approvals[0].comment.as_deref(), Some("looks good"));
    }

    #[test]
    fn l0_governance_service_propose_approve_x3_applies_identity_schema() {
        let service = service_with_checkpoint_schema("l0-e2e");
        let content = "R2 service L0 governance e2e identity rule";
        let proposer = test_actor("actor-proposer");

        let proposal = service.propose_change_with_actor(
            &proposer,
            ProposeChangeParams {
                proposed_content: content.to_string(),
                reason: "exercise server tool path through L0 apply".to_string(),
                change_type: Some("create".to_string()),
                target_id: None,
                category: Some("identity".to_string()),
            },
        );
        let proposed: serde_json::Value =
            serde_json::from_str(&proposal).expect("proposal response JSON");
        assert_eq!(proposed["status"].as_str(), Some("pending"));
        let change_id = proposed["change_id"]
            .as_str()
            .expect("proposal response should include change_id")
            .to_string();

        for (idx, actor_id) in ["actor-alice", "actor-bob", "actor-charlie"]
            .iter()
            .enumerate()
        {
            let response = service.approve_change_with_actor(
                &test_actor(actor_id),
                ApproveChangeParams {
                    change_id: change_id.clone(),
                    approver: None,
                    comment: Some(format!("approval {}", idx + 1)),
                },
            );
            let payload: serde_json::Value =
                serde_json::from_str(&response).expect("approval response JSON");
            assert_eq!(
                payload["approvals_count"].as_i64(),
                Some((idx + 1) as i64),
                "approval count should advance once for {actor_id}: {response}"
            );

            if idx < 2 {
                assert_eq!(
                    payload["status"].as_str(),
                    Some("pending"),
                    "first two approvals should stay pending: {response}"
                );
            } else {
                assert_eq!(
                    payload["status"].as_str(),
                    Some("applied"),
                    "third distinct approval should apply the L0 change: {response}"
                );
                assert!(
                    payload["applied_at"].as_str().is_some(),
                    "applied response should carry applied_at: {response}"
                );
            }
        }

        #[derive(Debug)]
        struct IdentityRow {
            category: Option<String>,
            project_id: Option<String>,
            is_active: i64,
            evolution_state: Option<String>,
            agent: Option<String>,
            metadata_json: Option<String>,
        }

        let row = service.governance.pool().with_conn(|conn| {
            conn.query_row(
                "SELECT category, project_id, is_active, evolution_state, agent, metadata_json
                 FROM notes
                 WHERE layer = 'identity_schema' AND content = ?1",
                rusqlite::params![content],
                |row| {
                    Ok(IdentityRow {
                        category: row.get(0)?,
                        project_id: row.get(1)?,
                        is_active: row.get(2)?,
                        evolution_state: row.get(3)?,
                        agent: row.get(4)?,
                        metadata_json: row.get(5)?,
                    })
                },
            )
            .expect("identity_schema row written by L0 apply")
        });

        assert_eq!(row.category.as_deref(), Some("identity"));
        assert_eq!(row.project_id.as_deref(), Some("test-project"));
        assert_eq!(row.is_active, 1);
        assert_eq!(row.evolution_state.as_deref(), Some("active"));
        assert_eq!(row.agent.as_deref(), Some("actor-charlie"));

        let metadata: serde_json::Value = serde_json::from_str(
            row.metadata_json
                .as_deref()
                .expect("applied identity row should include metadata_json"),
        )
        .expect("metadata_json should be valid JSON");
        assert_eq!(metadata["change_id"].as_str(), Some(change_id.as_str()));
        assert_eq!(metadata["applied_by"].as_str(), Some("actor-charlie"));
    }

    #[test]
    fn save_checkpoint_persists_compact_sections() {
        let service = service_with_checkpoint_schema("checkpoint");
        let actor = test_actor("actor-checkpoint");

        let response = service.save_checkpoint_with_actor(
            &actor,
            SaveCheckpointParams {
                task_id: "t2-checkpoint".to_string(),
                summary: "normalize checkpoint metadata".to_string(),
                task_specification: Some("把 Rust checkpoint 对齐 compact schema".to_string()),
                task_status: Some("blocked".to_string()),
                blocker: Some("waiting on test".to_string()),
                next_step: Some("run search_checkpoints".to_string()),
                task_context: Some("issue #22".to_string()),
                decisions: Some(vec!["compact_sections wins".to_string()]),
                files_and_functions: Some(vec![
                    "memra-server/src/service.rs".to_string(),
                    "memra-core/src/core/write_orchestrator.rs".to_string(),
                ]),
                workflow: Some(vec!["audit Python".to_string(), "patch Rust".to_string()]),
                errors_and_corrections: Some(vec!["trim blank items".to_string()]),
                living_docs: Some(vec!["docs/MIGRATION-rust-mcp.md".to_string()]),
                trigger_patterns: Some(vec!["checkpoint parity".to_string()]),
            },
        );
        assert!(
            response.contains("t2-checkpoint"),
            "save response should mention task_id: {response}"
        );

        let results = service.engine.search_checkpoints_for_project(
            None,
            Some("t2-checkpoint"),
            Some("blocked"),
            Some("test-project"),
            5,
        );
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("任务说明"));
        assert!(results[0].content.contains("关键决策"));

        let metadata = results[0].metadata.as_ref().expect("checkpoint metadata");
        assert_eq!(metadata["compact_schema_version"].as_i64(), Some(1));
        assert_eq!(metadata["blocker"].as_str(), Some("waiting on test"));
        assert_eq!(
            metadata["trigger_patterns"][0].as_str(),
            Some("checkpoint parity")
        );

        let sections = metadata["compact_sections"]
            .as_object()
            .expect("compact_sections object");
        assert_eq!(
            sections["task_specification"].as_str(),
            Some("把 Rust checkpoint 对齐 compact schema")
        );
        assert_eq!(
            sections["files_and_functions"][0].as_str(),
            Some("memra-server/src/service.rs")
        );
        assert_eq!(sections["workflow"][1].as_str(), Some("patch Rust"));
        assert_eq!(
            sections["errors_and_corrections"][0].as_str(),
            Some("trim blank items")
        );
        assert_eq!(
            sections["decisions"][0].as_str(),
            Some("compact_sections wins")
        );
        assert_eq!(
            sections["living_docs"][0].as_str(),
            Some("docs/MIGRATION-rust-mcp.md")
        );
    }

    fn minimal_checkpoint_params(task_id: &str, summary: &str) -> SaveCheckpointParams {
        SaveCheckpointParams {
            task_id: task_id.to_string(),
            summary: summary.to_string(),
            task_specification: None,
            task_status: None,
            blocker: None,
            next_step: None,
            task_context: None,
            decisions: None,
            files_and_functions: None,
            workflow: None,
            errors_and_corrections: None,
            living_docs: None,
            trigger_patterns: None,
        }
    }

    #[test]
    fn save_checkpoint_rejects_invalid_status() {
        let service = service_with_checkpoint_schema("invalid-status");
        let actor = test_actor("actor-invalid-status");

        let mut params = minimal_checkpoint_params("t8-bad-status", "bad");
        params.task_status = Some("done".to_string());

        let response = service.save_checkpoint_with_actor(&actor, params);
        assert!(response.contains("task_status 必须是"), "got: {response}");
        assert!(
            response.contains("done"),
            "should echo offending value: {response}"
        );

        let results = service.engine.search_checkpoints_for_project(
            None,
            Some("t8-bad-status"),
            None,
            Some("test-project"),
            5,
        );
        assert!(results.is_empty(), "invalid status must not persist a row");
    }

    #[test]
    fn save_checkpoint_rejects_empty_required_fields() {
        let service = service_with_checkpoint_schema("empty-required");
        let actor = test_actor("actor-empty-required");

        let blank_id =
            service.save_checkpoint_with_actor(&actor, minimal_checkpoint_params("   ", "summary"));
        assert!(blank_id.contains("必填项"), "blank task_id: {blank_id}");

        let blank_summary =
            service.save_checkpoint_with_actor(&actor, minimal_checkpoint_params("task", "\t\n"));
        assert!(
            blank_summary.contains("必填项"),
            "blank summary: {blank_summary}"
        );
    }

    #[test]
    fn save_checkpoint_preserves_cjk_under_char_budget() {
        // 200 个汉字 = 600 bytes UTF-8。旧的 byte-based 截断会在 ~166 字处砍掉；
        // char-based 应该完整保留（500 char 预算之内）。
        let cjk_spec: String = "中".repeat(200);
        let service = service_with_checkpoint_schema("cjk");
        let actor = test_actor("actor-cjk");

        let mut params = minimal_checkpoint_params("t8-cjk", "cjk preservation");
        params.task_specification = Some(cjk_spec.clone());

        let _ = service.save_checkpoint_with_actor(&actor, params);

        let results = service.engine.search_checkpoints_for_project(
            None,
            Some("t8-cjk"),
            None,
            Some("test-project"),
            5,
        );
        assert_eq!(results.len(), 1);
        let metadata = results[0].metadata.as_ref().expect("metadata");
        let stored = metadata["compact_sections"]["task_specification"]
            .as_str()
            .expect("task_specification stored");
        assert_eq!(
            stored.chars().count(),
            200,
            "CJK string must survive intact"
        );
        assert_eq!(stored, cjk_spec);
        assert!(!stored.ends_with("..."), "no truncation marker expected");
    }

    #[test]
    fn save_checkpoint_truncates_with_marker_beyond_char_budget() {
        // 600 个汉字 > 500 char 预算 → 应保留 497 char + "..."
        let oversized: String = "字".repeat(600);
        let service = service_with_checkpoint_schema("cjk-truncate");
        let actor = test_actor("actor-cjk-truncate");

        let mut params = minimal_checkpoint_params("t8-cjk-trunc", "truncate");
        params.task_specification = Some(oversized);

        let _ = service.save_checkpoint_with_actor(&actor, params);

        let results = service.engine.search_checkpoints_for_project(
            None,
            Some("t8-cjk-trunc"),
            None,
            Some("test-project"),
            5,
        );
        let metadata = results[0].metadata.as_ref().expect("metadata");
        let stored = metadata["compact_sections"]["task_specification"]
            .as_str()
            .expect("task_specification stored");
        assert_eq!(stored.chars().count(), 500, "must equal char budget");
        assert!(
            stored.ends_with("..."),
            "truncation marker required: {stored}"
        );
    }

    #[test]
    fn save_checkpoint_completed_uses_check_icon() {
        let service = service_with_checkpoint_schema("completed-icon");
        let actor = test_actor("actor-completed-icon");

        let mut params = minimal_checkpoint_params("t8-icon", "icon");
        params.task_status = Some("completed".to_string());

        let response = service.save_checkpoint_with_actor(&actor, params);
        assert!(
            response.starts_with("✅"),
            "expected ✅ icon, got: {response}"
        );
    }

    #[test]
    fn list_rooms_reports_project_room_counts() {
        let service = service_with_seeded_memories();
        let response = service.list_rooms(Parameters(ListRoomsParams {
            project_id: None,
            include_null: Some(false),
        }));

        assert!(response.contains("release: 1 条"), "got: {response}");
        assert!(response.contains("engineering: 1 条"), "got: {response}");
    }

    #[test]
    fn search_manifest_returns_json_payload() {
        let service = service_with_seeded_memories();
        let response = service.search_manifest(Parameters(SearchManifestParams {
            query: "release workflow".to_string(),
            candidate_limit: Some(5),
            selected_limit: Some(2),
            min_score: Some(0.0),
            include_constitution: Some(false),
            cross_project: Some(false),
            search_mode: Some("lexical".to_string()),
            as_of: None,
            start_time: None,
            end_time: None,
            include_expired: Some(false),
        }));
        let payload: serde_json::Value =
            serde_json::from_str(&response).expect("manifest JSON payload");

        assert_eq!(payload["query"].as_str(), Some("release workflow"));
        assert!(
            payload["candidate_count"].as_i64().unwrap_or(0) >= 1,
            "expected at least one manifest candidate: {payload}"
        );
        assert!(
            payload["selected"]
                .as_array()
                .and_then(|items| items.first())
                .and_then(|item| item.get("content"))
                .is_some(),
            "selected entries should include expanded content: {payload}"
        );

        // REL-02 contract: diagnostics field always present (even when zero)
        // so agents can rely on its shape without existence checks.
        assert!(
            payload["diagnostics"]["dim_mismatch_skipped"]
                .as_u64()
                .is_some(),
            "manifest payload must expose diagnostics.dim_mismatch_skipped (REL-02): {payload}"
        );
    }

    #[test]
    fn search_rules_surfaces_association_explanation() {
        let service = service_with_seeded_memories();
        seed_note_relation(&service, "m1", "m2");

        let response = service.search_rules(Parameters(SearchRulesParams {
            query: "release workflow".to_string(),
            limit: Some(5),
            min_score: Some(0.0),
            search_mode: Some("lexical".to_string()),
            layer: None,
            category: None,
            include_constitution: Some(false),
            start_time: None,
            end_time: None,
            include_expired: Some(false),
            cross_project: Some(false),
            room: None,
        }));

        assert!(
            response.contains("association: supports from m1"),
            "search_rules should explain neural activation edge: {response}"
        );
        assert!(
            response.contains("strength=0.910"),
            "search_rules should expose association strength: {response}"
        );
        assert!(
            response.contains("depth=1"),
            "search_rules should expose neural activation depth: {response}"
        );
        assert!(
            response.contains("path=m1 -> m2"),
            "search_rules should expose neural activation path: {response}"
        );
        assert!(
            response.contains("activation diagnostics: results=1 max_depth=1"),
            "search_rules should summarize activation diagnostics: {response}"
        );
    }

    #[test]
    fn search_rules_surfaces_multi_hop_activation_path() {
        let service = service_with_seeded_memories();
        service.engine.pool().with_conn(|conn| {
            conn.execute(
                "INSERT INTO notes
                 (id, content, layer, category, is_active, confidence, project_id,
                  created_at, updated_at, room, metadata_json)
                 VALUES
                 ('m3', 'Deep chained memory reached only through neural activation',
                  'verified_fact', 'fact', 1, 0.79, 'test-project',
                  '2026-04-12T00:00:00Z', '2026-04-12T00:00:00Z',
                  'engineering', '{}')",
                [],
            )
            .expect("insert m3");
        });
        seed_note_relation(&service, "m1", "m2");
        seed_note_relation(&service, "m2", "m3");

        let response = service.search_rules(Parameters(SearchRulesParams {
            query: "release workflow".to_string(),
            limit: Some(5),
            min_score: Some(0.0),
            search_mode: Some("lexical".to_string()),
            layer: None,
            category: None,
            include_constitution: Some(false),
            start_time: None,
            end_time: None,
            include_expired: Some(false),
            cross_project: Some(false),
            room: None,
        }));

        assert!(
            response.contains("m3") || response.contains("Deep chained memory"),
            "search_rules should include the second-hop memory: {response}"
        );
        assert!(
            response.contains("depth=2"),
            "search_rules should expose second-hop activation depth: {response}"
        );
        assert!(
            response.contains("path=m1 -> m2 -> m3"),
            "search_rules should expose full activation chain: {response}"
        );
    }

    #[test]
    fn search_manifest_surfaces_association_trace() {
        let service = service_with_seeded_memories();
        seed_note_relation(&service, "m1", "m2");

        let response = service.search_manifest(Parameters(SearchManifestParams {
            query: "release workflow".to_string(),
            candidate_limit: Some(5),
            selected_limit: Some(5),
            min_score: Some(0.0),
            include_constitution: Some(false),
            cross_project: Some(false),
            search_mode: Some("lexical".to_string()),
            as_of: None,
            start_time: None,
            end_time: None,
            include_expired: Some(false),
        }));
        let payload: serde_json::Value =
            serde_json::from_str(&response).expect("manifest JSON payload");
        let selected = payload["selected"]
            .as_array()
            .expect("selected must be an array");
        let associated = selected
            .iter()
            .find(|item| item["id"].as_str() == Some("m2"))
            .expect("m2 should be selected through association");

        assert_eq!(
            associated["source_type"].as_str(),
            Some("memory_association")
        );
        assert_eq!(
            associated["association"]["associated_from"].as_str(),
            Some("m1")
        );
        assert_eq!(
            associated["association"]["relation_type"].as_str(),
            Some("supports")
        );
        assert_eq!(
            associated["association"]["association_root"].as_str(),
            Some("m1")
        );
        assert_eq!(
            associated["association"]["activation_depth"].as_u64(),
            Some(1)
        );
        assert_eq!(
            associated["association"]["activation_path"]
                .as_array()
                .expect("activation_path should be an array")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["m1", "m2"]
        );
        assert!(
            associated["selected_reason"]
                .as_str()
                .unwrap_or_default()
                .contains("Rust neural activation via supports from m1"),
            "selected_reason should explain association trace: {associated}"
        );
        assert!(
            associated["evidence_refs"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .any(|item| item["kind"].as_str() == Some("associated_from")
                    && item["id"].as_str() == Some("m1")),
            "manifest should expose associated_from as evidence ref: {associated}"
        );
        assert_eq!(
            payload["diagnostics"]["activation_result_count"].as_u64(),
            Some(1)
        );
        assert_eq!(
            payload["diagnostics"]["activation_max_depth"].as_u64(),
            Some(1)
        );
        assert_eq!(
            payload["diagnostics"]["activation_relation_type_counts"]["supports"].as_u64(),
            Some(1)
        );
    }

    #[test]
    fn search_manifest_persists_activation_trace_log() {
        let service = service_with_seeded_memories();
        seed_note_relation(&service, "m1", "m2");

        let response = service.search_manifest(Parameters(SearchManifestParams {
            query: "release workflow".to_string(),
            candidate_limit: Some(5),
            selected_limit: Some(5),
            min_score: Some(0.0),
            include_constitution: Some(false),
            cross_project: Some(false),
            search_mode: Some("lexical".to_string()),
            as_of: None,
            start_time: None,
            end_time: None,
            include_expired: Some(false),
        }));
        let payload: serde_json::Value =
            serde_json::from_str(&response).expect("manifest JSON payload");
        assert_eq!(
            payload["diagnostics"]["activation_result_count"].as_u64(),
            Some(1)
        );

        service.engine.pool().with_conn(|conn| {
            let query_log_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM query_log WHERE query = 'release workflow'",
                    [],
                    |row| row.get(0),
                )
                .expect("query_log count");
            assert_eq!(query_log_count, 1);

            let recall_log_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM recall_log WHERE query_text = 'release workflow'",
                    [],
                    |row| row.get(0),
                )
                .expect("recall_log count");
            assert!(
                recall_log_count >= 2,
                "direct + associated visible results should be recall-logged"
            );

            let (tool, activation_result_count, activation_max_depth, trace_json): (
                String,
                i64,
                i64,
                String,
            ) = conn
                .query_row(
                    "SELECT tool, activation_result_count, activation_max_depth, activation_trace_json
                     FROM activation_trace_log
                     WHERE project_id = 'test-project'
                     ORDER BY id DESC
                     LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("activation trace row");
            assert_eq!(tool, "search_manifest");
            assert_eq!(activation_result_count, 1);
            assert_eq!(activation_max_depth, 1);

            let trace: serde_json::Value =
                serde_json::from_str(&trace_json).expect("trace json");
            assert_eq!(
                trace["summary"]["activation_result_count"].as_u64(),
                Some(1)
            );
            assert!(
                trace["activation_paths"]
                    .as_array()
                    .unwrap_or(&Vec::new())
                    .iter()
                    .any(|item| item["associated_from"].as_str() == Some("m1")
                        && item["activation_depth"].as_u64() == Some(1)),
                "trace should persist the association path: {trace}"
            );
        });
    }

    #[test]
    fn experience_tool_params_tolerate_empty_objects() {
        let service = service_with_seeded_memories();

        let search_params: SearchExperiencesParams =
            serde_json::from_value(serde_json::json!({})).expect("search_experiences params");
        let search_response: serde_json::Value =
            serde_json::from_str(&service.search_experiences(Parameters(search_params)))
                .expect("search_experiences JSON");
        assert_eq!(
            search_response["error"].as_str(),
            Some("unsupported_in_rust"),
            "{search_response}"
        );

        let review_params: ReviewExperienceParams =
            serde_json::from_value(serde_json::json!({})).expect("review params");
        let review_response: serde_json::Value =
            serde_json::from_str(&service.review_experience(Parameters(review_params)))
                .expect("review JSON");
        assert_eq!(
            review_response["error"].as_str(),
            Some("unsupported_in_rust"),
            "{review_response}"
        );

        let build_params: BuildExperienceArtifactsParams =
            serde_json::from_value(serde_json::json!({})).expect("build params");
        let build_response: serde_json::Value =
            serde_json::from_str(&service.build_experience_artifacts(Parameters(build_params)))
                .expect("build JSON");
        assert_eq!(
            build_response["error"].as_str(),
            Some("unsupported_in_rust"),
            "{build_response}"
        );

        let topic_params: ReportTopicFeedbackParams =
            serde_json::from_value(serde_json::json!({})).expect("topic feedback params");
        let topic_response: serde_json::Value =
            serde_json::from_str(&service.report_topic_feedback(Parameters(topic_params)))
                .expect("topic feedback JSON");
        assert_eq!(
            topic_response["error"].as_str(),
            Some("unsupported_in_rust"),
            "{topic_response}"
        );

        let artifact_params: ReportArtifactFeedbackParams =
            serde_json::from_value(serde_json::json!({})).expect("artifact feedback params");
        let artifact_response: serde_json::Value =
            serde_json::from_str(&service.report_artifact_feedback(Parameters(artifact_params)))
                .expect("artifact feedback JSON");
        assert_eq!(
            artifact_response["error"].as_str(),
            Some("unsupported_in_rust"),
            "{artifact_response}"
        );
    }

    #[test]
    fn experience_tools_are_db_backed() {
        let service = service_with_seeded_memories();

        let search_payload: serde_json::Value = serde_json::from_str(&service.search_experiences(
            Parameters(SearchExperiencesParams {
                query: Some("release artifact".to_string()),
                limit: Some(5),
                statuses: Some(vec!["stable".to_string()]),
                kind: Some("procedure".to_string()),
            }),
        ))
        .expect("search_experiences JSON");
        assert_eq!(search_payload["count"].as_i64(), Some(1));
        assert_eq!(search_payload["results"][0]["id"].as_str(), Some("exp-1"));

        let review_payload: serde_json::Value = serde_json::from_str(&service.review_experience(
            Parameters(ReviewExperienceParams {
                experience_id: Some("exp-1".to_string()),
                topic: None,
                force: Some(true),
            }),
        ))
        .expect("review_experience JSON");
        assert_eq!(
            review_payload["reviews"]
                .as_array()
                .map(|items| items.len()),
            Some(4)
        );

        let surface_payload: serde_json::Value = serde_json::from_str(
            &service.get_experience_surfaces(Parameters(GetExperienceSurfacesParams {
                target: Some("hermes".to_string()),
                limit: Some(5),
                refresh: Some(false),
                topic: Some("release".to_string()),
                auto_topic: Some(false),
            })),
        )
        .expect("get_experience_surfaces JSON");
        assert_eq!(surface_payload["target"].as_str(), Some("hermes"));
        assert!(
            surface_payload["surface"]["stable_playbooks"]
                .as_array()
                .map(|items| !items.is_empty())
                .unwrap_or(false)
        );
    }

    #[test]
    fn artifact_feedback_records_topic_feedback() {
        let service = service_with_seeded_memories();
        let artifact_payload: serde_json::Value = serde_json::from_str(
            &service.build_experience_artifacts(Parameters(BuildExperienceArtifactsParams {
                target: Some("hermes".to_string()),
                limit: Some(5),
                save: Some(false),
                topic: Some("release".to_string()),
                auto_topic: Some(false),
            })),
        )
        .expect("artifact JSON");
        let feedback_token = artifact_payload["hermes_skill_bundle"]["feedback_token"]
            .as_str()
            .expect("feedback token")
            .to_string();

        let feedback_payload: serde_json::Value = serde_json::from_str(
            &service.report_artifact_feedback(Parameters(ReportArtifactFeedbackParams {
                feedback_token: Some(feedback_token.clone()),
                artifact_run_id: None,
                verdict: Some("helpful".to_string()),
                source: Some("test".to_string()),
                reason: Some("worked".to_string()),
            })),
        )
        .expect("artifact feedback JSON");
        assert_eq!(
            feedback_payload["feedback_token"].as_str(),
            Some(feedback_token.as_str())
        );
        assert_eq!(
            feedback_payload["verdict"].as_str(),
            Some("artifact_helpful")
        );

        let topic_payload: serde_json::Value = serde_json::from_str(
            &service.report_topic_feedback(Parameters(ReportTopicFeedbackParams {
                topic: Some("release".to_string()),
                verdict: Some("artifact_miss".to_string()),
                artifact_target: Some("hermes".to_string()),
                source: Some("test".to_string()),
                reason: Some("manual".to_string()),
            })),
        )
        .expect("topic feedback JSON");
        assert_eq!(topic_payload["topic_norm"].as_str(), Some("release"));
    }

    // --- session_id resolution: shape tests + integration coverage map ---
    //
    // `RequestContext<RoleServer>` requires `RequestId + Peer<R>` at construction
    // time and has no Default impl, so we cannot drive the full
    // `resolve_session_id(&self, ctx: &RequestContext<RoleServer>)` signature
    // from a unit test. Coverage is split as follows:
    //
    // 1. `stdio_session_id()` direct call — tested below
    // 2. `Parts.extensions.get::<SessionId>()` extraction shape — tested below
    //    via `session_id_from_parts_shape` (a hand-written copy of the inner
    //    logic; this is a SHAPE test, not a real call into resolve_session_id;
    //    its job is to lock the extension key + newtype unwrap, NOT to verify
    //    that rmcp wraps `Parts` inside `RequestContext.extensions`)
    // 3. The end-to-end path "axum middleware inserts SessionId → service.rs
    //    reads it via Parts unwrap" — tested integration-style by
    //    `transport/session.rs::session_middleware_extracts_valid_header`
    //    + the existing `resolve_actor` helper at service.rs:1997 which uses
    //    the IDENTICAL `ctx.extensions.get::<Parts>()` unwrap pattern and is
    //    exercised by every authenticated HTTP request in the suite.
    //
    // If rmcp ever stops wrapping Parts in RequestContext.extensions, the
    // shape test below WILL still pass while production breaks. The integration
    // path in (3) is the safety net.

    use crate::transport::session::SessionId;

    /// SHAPE test only — operates on a raw `Parts` to lock the extension-key +
    /// newtype-unwrap contract. Does NOT exercise the rmcp `RequestContext`
    /// outer unwrap; see comment above for why and where the real path is
    /// covered.
    fn session_id_from_parts_shape(
        svc: &MemraService,
        parts: axum::http::request::Parts,
    ) -> String {
        if let Some(session_id) = parts.extensions.get::<SessionId>() {
            return session_id.0.clone();
        }
        svc.stdio_session_id()
    }

    #[test]
    fn session_id_extension_key_unwrap_returns_inner() {
        // Locks: SessionId is the right extension type, .0 is the inner String.
        let svc = MemraService::stub();
        let (mut parts, _) = axum::http::Request::builder()
            .body(())
            .unwrap()
            .into_parts();
        parts
            .extensions
            .insert(SessionId("http:my-session-123".to_string()));
        assert_eq!(
            session_id_from_parts_shape(&svc, parts),
            "http:my-session-123"
        );
    }

    #[test]
    fn session_id_extension_absent_falls_back_to_stdio() {
        // Locks: missing SessionId in Parts.extensions → stdio fallback path.
        let svc = MemraService::stub();
        let (parts, _) = axum::http::Request::builder()
            .body(())
            .unwrap()
            .into_parts();
        let result = session_id_from_parts_shape(&svc, parts);
        assert!(
            result.starts_with("stdio:"),
            "expected stdio: prefix, got: {result}"
        );
    }

    #[test]
    fn stdio_session_id_returns_prefixed_process_id() {
        // Direct test of stdio_session_id — the actual function called by
        // resolve_session_id on the stdio fallback branch.
        let svc = MemraService::stub();
        let result = svc.stdio_session_id();
        assert!(
            result.starts_with("stdio:"),
            "expected stdio: prefix, got: {result}"
        );
    }

    // -----------------------------------------------------------------------
    // require_write_token tests (IMPL-01b)
    //
    // NOTE: `RequestContext<RoleServer>` is non-exhaustive and cannot be
    // constructed directly in tests. We therefore test the inner
    // `require_write_token_from_parts` method which contains all the
    // logic. The thin outer `require_write_token` wrapper is covered by:
    //   - stdio path: absence of Parts in extensions → Ok (shape test below)
    //   - HTTP path: covered end-to-end in integration tests and by the
    //     governance_spoof.rs tests which exercise the full tool path.
    // -----------------------------------------------------------------------

    /// Helper: build axum Parts with an optional write token header.
    fn parts_with_token(token: Option<&str>) -> axum::http::request::Parts {
        parts_with_token_and_session(token, None)
    }

    /// Helper: build axum Parts with an optional write token header and an
    /// optional bound session id (mimics `session_middleware` having inserted
    /// `SessionId(...)` into the request extensions). PR #235 Codex P1
    /// regression: write_token validation now enforces session binding, so
    /// happy-path tests must inject the matching SessionId.
    fn parts_with_token_and_session(
        token: Option<&str>,
        session_id: Option<&str>,
    ) -> axum::http::request::Parts {
        let mut builder = axum::http::Request::builder().method("POST").uri("/mcp");
        if let Some(t) = token {
            builder = builder.header(WRITE_TOKEN_HEADER, t);
        }
        let (mut parts, _) = builder.body(()).unwrap().into_parts();
        if let Some(sid) = session_id {
            parts
                .extensions
                .insert(crate::transport::session::SessionId(sid.to_string()));
        }
        parts
    }

    // AC3: stdio path — require_write_token_from_parts with no header → Missing.
    // (The outer require_write_token returns Ok on stdio — tested by the
    //  absence-of-Parts short-circuit which we verify with a unit test.)
    #[test]
    fn require_write_token_missing_header_returns_missing() {
        let svc = MemraService::stub();
        let actor = test_actor("actor-http");
        let parts = parts_with_token(None);
        assert_eq!(
            svc.require_write_token_from_parts(&parts, &actor),
            Err(WriteTokenError::Missing)
        );
    }

    // AC3: expired token → Expired.
    #[test]
    fn require_write_token_returns_expired_for_past_token() {
        use memra_core::storage::session_tokens_writer::issue_token;

        let svc = MemraService::stub();
        let actor = test_actor("actor-expired");

        let secret = "expired-secret-hex-64-chars-padded-000000000000000000000000000";
        let token_hash = hash_bearer_token(secret);
        svc.token_pool.with_conn(|conn| {
            issue_token(
                conn,
                &token_hash,
                "http:test-sess",
                "actor-expired",
                "2026-01-01T00:00:00Z",
                "2026-01-02T00:00:00Z", // expired
            )
            .expect("issue expired token");
        });

        let parts = parts_with_token(Some(secret));
        assert!(
            matches!(
                svc.require_write_token_from_parts(&parts, &actor),
                Err(WriteTokenError::Expired { .. })
            ),
            "expected Expired error"
        );
    }

    // AC3: revoked token → Revoked.
    #[test]
    fn require_write_token_returns_revoked_for_revoked_token() {
        use memra_core::storage::session_tokens_writer::{issue_token, revoke_tokens_for_session};

        let svc = MemraService::stub();
        let actor = test_actor("actor-revoked");

        let secret = "revoked-secret-hex-64-chars-padded-00000000000000000000000000";
        let token_hash = hash_bearer_token(secret);
        svc.token_pool.with_conn(|conn| {
            issue_token(
                conn,
                &token_hash,
                "http:revoke-sess",
                "actor-revoked",
                "2026-04-26T10:00:00Z",
                "2099-12-31T23:59:59Z",
            )
            .expect("issue revoked token");
            revoke_tokens_for_session(conn, "http:revoke-sess", "2026-04-26T11:00:00Z")
                .expect("revoke token");
        });

        let parts = parts_with_token(Some(secret));
        assert!(
            matches!(
                svc.require_write_token_from_parts(&parts, &actor),
                Err(WriteTokenError::Revoked { .. })
            ),
            "expected Revoked error"
        );
    }

    // AC3: token exists but actor_id does not match → ActorMismatch.
    #[test]
    fn require_write_token_returns_actor_mismatch() {
        use memra_core::storage::session_tokens_writer::issue_token;

        let svc = MemraService::stub();
        let secret = "mismatch-secret-hex-64-chars-padded-00000000000000000000000000";
        let token_hash = hash_bearer_token(secret);
        svc.token_pool.with_conn(|conn| {
            issue_token(
                conn,
                &token_hash,
                "http:mismatch-sess",
                "actor-alice", // token belongs to alice
                "2026-04-26T10:00:00Z",
                "2099-12-31T23:59:59Z",
            )
            .expect("issue token for alice");
        });

        // Bob presents alice's token → mismatch.
        let actor_bob = test_actor("actor-bob");
        let parts = parts_with_token(Some(secret));
        assert!(
            matches!(
                svc.require_write_token_from_parts(&parts, &actor_bob),
                Err(WriteTokenError::ActorMismatch { .. })
            ),
            "expected ActorMismatch error"
        );
    }

    // AC3: valid token matching the actor → Ok.
    #[test]
    fn require_write_token_valid_passes() {
        use memra_core::storage::session_tokens_writer::issue_token;

        let svc = MemraService::stub();
        let actor = test_actor("actor-valid");

        let secret = "valid-secret-hex-64-chars-padded-000000000000000000000000000000";
        let token_hash = hash_bearer_token(secret);
        svc.token_pool.with_conn(|conn| {
            issue_token(
                conn,
                &token_hash,
                "http:valid-sess",
                "actor-valid",
                "2026-04-26T10:00:00Z",
                "2099-12-31T23:59:59Z",
            )
            .expect("issue valid token");
        });

        // PR #235 Codex P1: request must claim the same session_id the token
        // was minted for, otherwise SessionMismatch.
        let parts = parts_with_token_and_session(Some(secret), Some("http:valid-sess"));
        assert_eq!(
            svc.require_write_token_from_parts(&parts, &actor),
            Ok(()),
            "valid token matching actor + session must pass"
        );
    }

    // PR #235 Codex P1 regression: token minted for session A cannot be
    // replayed on requests claiming session B even when actor matches.
    #[test]
    fn require_write_token_returns_session_mismatch_on_replay() {
        use memra_core::storage::session_tokens_writer::issue_token;

        let svc = MemraService::stub();
        let actor = test_actor("actor-replayer");

        // Issue token bound to session A.
        let secret = "replay-secret-hex-64-chars-padded-00000000000000000000000000000";
        let token_hash = hash_bearer_token(secret);
        svc.token_pool.with_conn(|conn| {
            issue_token(
                conn,
                &token_hash,
                "http:session-A",
                "actor-replayer",
                "2026-04-26T10:00:00Z",
                "2099-12-31T23:59:59Z",
            )
            .expect("issue token bound to session A");
        });

        // Replay attempt: same actor, same token, but request claims session B.
        let parts = parts_with_token_and_session(Some(secret), Some("http:session-B"));
        match svc.require_write_token_from_parts(&parts, &actor) {
            Err(WriteTokenError::SessionMismatch {
                stored_session,
                request_session,
            }) => {
                assert_eq!(stored_session, "http:session-A");
                assert_eq!(request_session, "http:session-B");
            }
            other => panic!("expected SessionMismatch, got {other:?}"),
        }
    }

    // Defensive: stdio path (no SessionId extension) must NOT validate against
    // a token bound to an http: session — otherwise an attacker who somehow
    // got both an HTTP key and stdio access could replay across the boundary.
    // Stored session "http:foo" vs computed "stdio:..." → SessionMismatch.
    #[test]
    fn require_write_token_blocks_http_token_on_stdio_fallback_session() {
        use memra_core::storage::session_tokens_writer::issue_token;

        let svc = MemraService::stub();
        let actor = test_actor("actor-cross-transport");
        let secret = "stdio-replay-secret-hex-64-padded-000000000000000000000000000000";
        let token_hash = hash_bearer_token(secret);
        svc.token_pool.with_conn(|conn| {
            issue_token(
                conn,
                &token_hash,
                "http:cross-transport-sess",
                "actor-cross-transport",
                "2026-04-26T10:00:00Z",
                "2099-12-31T23:59:59Z",
            )
            .expect("issue http-bound token");
        });

        // No SessionId extension → falls back to stdio_session_id which starts
        // with "stdio:" and cannot match an "http:" bound token.
        let parts = parts_with_token(Some(secret));
        assert!(
            matches!(
                svc.require_write_token_from_parts(&parts, &actor),
                Err(WriteTokenError::SessionMismatch { .. })
            ),
            "http-bound token must NOT validate on stdio fallback session"
        );
    }
}

// --- Parameter types ---

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchRulesParams {
    #[schemars(description = "Search query")]
    pub query: String,
    #[schemars(description = "Max results (default 5)")]
    pub limit: Option<usize>,
    #[schemars(description = "Min score threshold (default 0.5)")]
    pub min_score: Option<f64>,
    #[schemars(description = "Search mode: auto, semantic, recent, lexical, rrf")]
    pub search_mode: Option<String>,
    #[schemars(description = "Filter by layer")]
    pub layer: Option<String>,
    #[schemars(description = "Filter by category")]
    pub category: Option<String>,
    #[schemars(description = "Include L0 constitution/identity memories")]
    pub include_constitution: Option<bool>,
    #[schemars(description = "Start time filter (ISO 8601)")]
    pub start_time: Option<String>,
    #[schemars(description = "End time filter (ISO 8601)")]
    pub end_time: Option<String>,
    #[schemars(description = "Include expired memories")]
    pub include_expired: Option<bool>,
    #[schemars(description = "Cross-project search")]
    pub cross_project: Option<bool>,
    #[schemars(description = "Palace room filter")]
    pub room: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct AddRuleParams {
    #[schemars(description = "Memory content")]
    pub content: String,
    #[schemars(description = "Category")]
    pub category: Option<String>,
    #[schemars(description = "Confidence (0.0-1.0, default 0.9)")]
    pub confidence: Option<f64>,
    #[schemars(description = "Agent identifier")]
    pub agent: Option<String>,
    #[schemars(description = "Palace room")]
    pub room: Option<String>,
    #[schemars(description = "Memory kind: fact, event, procedure")]
    pub memory_kind: Option<String>,
    #[schemars(description = "Role: user, assistant, system")]
    pub role: Option<String>,
    #[schemars(description = "Event time (ISO 8601)")]
    pub when: Option<String>,
    #[schemars(description = "Event location")]
    pub r#where: Option<String>,
    #[schemars(description = "People involved")]
    pub who: Option<Vec<String>>,
    #[schemars(description = "TTL in days (event_log only)")]
    pub ttl_days: Option<i64>,
    #[schemars(description = "Difficulty (1-5)")]
    pub difficulty: Option<i64>,
    #[schemars(description = "Time cost hint")]
    pub time_cost_hint: Option<String>,
    #[schemars(description = "Related memory IDs")]
    pub related_ids: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetContextParams {
    #[schemars(description = "Mode: wake (compact) or full")]
    pub mode: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ProposeChangeParams {
    #[schemars(description = "Proposed content")]
    pub proposed_content: String,
    #[schemars(description = "Reason for change")]
    pub reason: String,
    #[schemars(description = "Change type: create, update, or delete")]
    pub change_type: Option<String>,
    #[schemars(description = "Target memory ID for update/delete changes")]
    pub target_id: Option<String>,
    #[schemars(description = "Memory category for applied identity row")]
    pub category: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ApproveChangeParams {
    #[schemars(description = "Change ID to approve")]
    pub change_id: String,
    #[schemars(description = "Display approver label; actor_id remains auth-bound")]
    pub approver: Option<String>,
    #[schemars(description = "Optional approval comment")]
    pub comment: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SaveCheckpointParams {
    #[schemars(description = "Task ID")]
    pub task_id: String,
    #[schemars(description = "Checkpoint summary")]
    pub summary: String,
    #[schemars(description = "Structured task specification")]
    pub task_specification: Option<String>,
    #[schemars(description = "Task status: blocked, in_progress, completed, abandoned")]
    pub task_status: Option<String>,
    #[schemars(description = "Blocker description")]
    pub blocker: Option<String>,
    #[schemars(description = "Next step")]
    pub next_step: Option<String>,
    #[schemars(description = "Task context (URLs, file paths)")]
    pub task_context: Option<String>,
    #[schemars(description = "Key decisions")]
    pub decisions: Option<Vec<String>>,
    #[schemars(description = "Files and functions involved")]
    pub files_and_functions: Option<Vec<String>>,
    #[schemars(description = "Workflow steps taken")]
    pub workflow: Option<Vec<String>>,
    #[schemars(description = "Errors and corrections")]
    pub errors_and_corrections: Option<Vec<String>>,
    #[schemars(description = "Living docs paths")]
    pub living_docs: Option<Vec<String>>,
    #[schemars(description = "Trigger patterns for auto-wakeup")]
    pub trigger_patterns: Option<Vec<String>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchCheckpointsParams {
    #[schemars(description = "Search query (optional)")]
    pub query: Option<String>,
    #[schemars(description = "Exact task_id match")]
    pub task_id: Option<String>,
    #[schemars(description = "Status filter: active, blocked, in_progress, completed, abandoned")]
    pub task_status: Option<String>,
    #[schemars(description = "Max results (default 5)")]
    pub limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReportOutcomeParams {
    #[schemars(description = "Memory ID")]
    pub memory_id: String,
    #[schemars(description = "Outcome: confirmed, corrected, or outdated")]
    pub outcome: String,
    #[schemars(description = "Optional explanation")]
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchManifestParams {
    #[schemars(description = "Search query")]
    pub query: String,
    #[schemars(description = "Manifest candidate count (default 12)")]
    pub candidate_limit: Option<usize>,
    #[schemars(description = "Selected result count (default 5)")]
    pub selected_limit: Option<usize>,
    #[schemars(description = "Min score threshold (default 0.5)")]
    pub min_score: Option<f64>,
    #[schemars(description = "Include L0 constitution/identity memories")]
    pub include_constitution: Option<bool>,
    #[schemars(description = "Cross-project search")]
    pub cross_project: Option<bool>,
    #[schemars(description = "Search mode: auto, semantic, recent, lexical, rrf")]
    pub search_mode: Option<String>,
    #[schemars(description = "Bi-temporal as-of timestamp")]
    pub as_of: Option<String>,
    #[schemars(description = "Start time filter")]
    pub start_time: Option<String>,
    #[schemars(description = "End time filter")]
    pub end_time: Option<String>,
    #[schemars(description = "Include expired memories")]
    pub include_expired: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListRoomsParams {
    #[schemars(description = "Project namespace; defaults to current project")]
    pub project_id: Option<String>,
    #[schemars(description = "Include NULL/unclassified room bucket")]
    pub include_null: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchExperiencesParams {
    pub query: Option<String>,
    pub limit: Option<usize>,
    pub statuses: Option<Vec<String>>,
    pub kind: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReviewExperienceParams {
    pub experience_id: Option<String>,
    pub topic: Option<String>,
    pub force: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetExperienceSurfacesParams {
    pub target: Option<String>,
    pub limit: Option<usize>,
    pub refresh: Option<bool>,
    pub topic: Option<String>,
    pub auto_topic: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct BuildExperienceArtifactsParams {
    pub target: Option<String>,
    pub limit: Option<usize>,
    pub save: Option<bool>,
    pub topic: Option<String>,
    pub auto_topic: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReportTopicFeedbackParams {
    pub topic: Option<String>,
    pub verdict: Option<String>,
    pub artifact_target: Option<String>,
    pub source: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReportArtifactFeedbackParams {
    pub feedback_token: Option<String>,
    pub artifact_run_id: Option<String>,
    pub verdict: Option<String>,
    pub source: Option<String>,
    pub reason: Option<String>,
}

// --- Tool router ---

#[rmcp::tool_router(server_handler)]
impl MemraService {
    fn record_search_observability(
        &self,
        tool: &str,
        query: &str,
        results: &[SearchResult],
        visible_limit: usize,
        diagnostics: &SearchDiagnostics,
    ) {
        if let Err(error) =
            self.try_record_search_observability(tool, query, results, visible_limit, diagnostics)
        {
            tracing::warn!("search observability write failed for {tool}: {error}");
        }
    }

    fn try_record_search_observability(
        &self,
        tool: &str,
        query: &str,
        results: &[SearchResult],
        visible_limit: usize,
        diagnostics: &SearchDiagnostics,
    ) -> rusqlite::Result<()> {
        self.engine.pool().with_conn(|conn| {
            ensure_search_observability_schema(conn)?;
            let visible_results = results.iter().take(visible_limit).collect::<Vec<_>>();
            let result_ids = visible_results
                .iter()
                .map(|result| result.id.clone())
                .collect::<Vec<_>>();
            let scores = visible_results
                .iter()
                .map(|result| (result.score * 10_000.0).round() / 10_000.0)
                .collect::<Vec<_>>();
            let emitted_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
            let trace_id = format!(
                "rust-search-{}-{}",
                Utc::now().timestamp_nanos_opt().unwrap_or_default(),
                result_ids.len()
            );
            let trace_payload = search_trace_payload(
                &trace_id,
                &self.project_id,
                tool,
                query,
                &emitted_at,
                &visible_results,
                diagnostics,
            );
            let result_ids_json =
                serde_json::to_string(&result_ids).unwrap_or_else(|_| "[]".to_string());
            let scores_json = serde_json::to_string(&scores).unwrap_or_else(|_| "[]".to_string());
            let trace_json =
                serde_json::to_string(&trace_payload).unwrap_or_else(|_| "{}".to_string());

            conn.execute(
                "INSERT INTO query_log (timestamp, query, top_result_ids, top_scores, result_count)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    emitted_at,
                    query,
                    result_ids_json,
                    scores_json,
                    visible_results.len() as i64
                ],
            )?;
            for result in &visible_results {
                conn.execute(
                    "INSERT INTO recall_log (note_id, recalled_at, query_text)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![&result.id, emitted_at, query],
                )?;
            }
            conn.execute(
                "INSERT INTO activation_trace_log
                 (trace_id, project_id, tool, query, emitted_at, result_count,
                  activation_result_count, activation_max_depth,
                  activation_dream_evidence_count, activation_latency_ms,
                  activation_trace_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    trace_id,
                    self.project_id,
                    tool,
                    query,
                    emitted_at,
                    visible_results.len() as i64,
                    diagnostics.activation_result_count as i64,
                    diagnostics.activation_max_depth as i64,
                    diagnostics.activation_dream_evidence_count as i64,
                    diagnostics.activation_latency_ms as i64,
                    trace_json,
                ],
            )?;
            Ok(())
        })
    }

    #[tool(description = "Semantic+keyword hybrid search across all memory layers")]
    fn search_rules(&self, Parameters(params): Parameters<SearchRulesParams>) -> String {
        let query_text = params.query.clone();
        let search_params = SearchParams {
            query: params.query,
            limit: params.limit.unwrap_or(5),
            layer: params.layer,
            category: params.category,
            only_active: true,
            agent_id: None,
            project_id: Some(self.project_id.clone()),
            as_of: None,
            start_time: params.start_time,
            end_time: params.end_time,
            include_expired: params.include_expired.unwrap_or(false),
            cross_project: params.cross_project.unwrap_or(false),
            search_mode: params.search_mode,
            room: params.room,
            min_score: params.min_score.unwrap_or(DEFAULT_MIN_SCORE),
            boost_categories: None,
            include_constitution: params.include_constitution.unwrap_or(false),
        };

        let (results, diagnostics) = self.engine.search_with_diagnostics(&search_params);
        self.record_search_observability(
            "search_rules",
            &query_text,
            &results,
            results.len(),
            &diagnostics,
        );

        if results.is_empty() {
            if diagnostics.dim_mismatch_skipped > 0 {
                // Zero results AND silent skips → user needs this signal the
                // most. Do not return the boilerplate "No results found."
                return format!(
                    "No results found. ⚠️ {} note(s) skipped due to embedding dim mismatch \
                     (legacy 384-dim rows predating the bge-m3 1024-dim migration). \
                     Run scripts/reembed_legacy.py to restore them. See TODO-REL-02.",
                    diagnostics.dim_mismatch_skipped
                );
            }
            return "No results found.".to_string();
        }

        // Format results as the Python version does
        let mut output = String::new();
        for (i, r) in results.iter().enumerate() {
            output.push_str(&format!(
                "{}. [{}] (score: {:.3}) {}\n",
                i + 1,
                r.layer,
                r.score,
                safe_truncate(&r.content, 300)
            ));
            if let Some(ref cat) = r.category {
                output.push_str(&format!("   category: {cat}\n"));
            }
            if let Some(ref temp) = r.temperature {
                output.push_str(&format!("   temperature: {temp}\n"));
            }
            if let Some(trace) = association_trace(r.metadata.as_ref()) {
                output.push_str(&association_output_line(&trace));
            }
            output.push('\n');
        }

        if diagnostics.dim_mismatch_skipped > 0 {
            output.push_str(&format!(
                "⚠️ {} additional note(s) skipped due to embedding dim mismatch \
                 (legacy 384-dim rows predating bge-m3 1024-dim). See TODO-REL-02.\n",
                diagnostics.dim_mismatch_skipped
            ));
        }
        if diagnostics.activation_result_count > 0 {
            let relation_counts = diagnostics
                .activation_relation_type_counts
                .iter()
                .map(|(relation, count)| format!("{relation}:{count}"))
                .collect::<Vec<_>>()
                .join(", ");
            output.push_str(&format!(
                "🧠 activation diagnostics: results={} max_depth={} dream_evidence={} latency_ms={} relation_types=[{}]\n",
                diagnostics.activation_result_count,
                diagnostics.activation_max_depth,
                diagnostics.activation_dream_evidence_count,
                diagnostics.activation_latency_ms,
                relation_counts
            ));
        }

        output
    }

    #[tool(description = "Build a prompt-facing manifest from memory search results")]
    fn search_manifest(&self, Parameters(params): Parameters<SearchManifestParams>) -> String {
        let candidate_limit = params.candidate_limit.unwrap_or(12).clamp(1, 20);
        let selected_limit = params.selected_limit.unwrap_or(5).clamp(1, 5);
        let include_constitution = params.include_constitution.unwrap_or(false);
        let search_params = SearchParams {
            query: params.query.clone(),
            limit: candidate_limit,
            layer: None,
            category: None,
            only_active: true,
            agent_id: None,
            project_id: Some(self.project_id.clone()),
            as_of: params.as_of,
            start_time: params.start_time,
            end_time: params.end_time,
            include_expired: params.include_expired.unwrap_or(false),
            cross_project: params.cross_project.unwrap_or(false),
            search_mode: params.search_mode,
            room: None,
            min_score: params.min_score.unwrap_or(DEFAULT_MIN_SCORE),
            boost_categories: None,
            include_constitution,
        };
        let (results, diagnostics) = self.engine.search_with_diagnostics(&search_params);
        self.record_search_observability(
            "search_manifest",
            &params.query,
            &results,
            selected_limit,
            &diagnostics,
        );

        let manifest: Vec<serde_json::Value> = results
            .iter()
            .map(|result| manifest_item(&params.query, result, false))
            .collect();
        let selected: Vec<serde_json::Value> = results
            .iter()
            .take(selected_limit)
            .map(|result| manifest_item(&params.query, result, true))
            .collect();
        let selected_ids: Vec<String> = results
            .iter()
            .take(selected_limit)
            .map(|result| result.id.clone())
            .collect();

        // REL-02: surface dim-mismatch skip count in the JSON envelope.
        // search_rules renders this as a warning footnote, but search_manifest
        // returns structured JSON — agents consuming the manifest need the
        // count as a field, not as prose. (Contract review on PR #188.)
        serde_json::to_string_pretty(&serde_json::json!({
            "query": params.query,
            "candidate_count": manifest.len(),
            "selected_count": selected.len(),
            "manifest": manifest,
            "selected_ids": selected_ids,
            "selected": selected,
            "diagnostics": {
                "dim_mismatch_skipped": diagnostics.dim_mismatch_skipped,
                "activation_result_count": diagnostics.activation_result_count,
                "activation_max_depth": diagnostics.activation_max_depth,
                "activation_relation_type_counts": diagnostics.activation_relation_type_counts,
                "activation_dream_evidence_count": diagnostics.activation_dream_evidence_count,
                "activation_latency_ms": diagnostics.activation_latency_ms,
            },
            "rust_notes": {
                "experience_manifest_items": "not yet ported; memory results only",
            },
        }))
        .unwrap_or_else(|_| r#"{"error":"serialize failed"}"#.to_string())
    }

    #[tool(description = "List palace rooms used in the current project")]
    fn list_rooms(&self, Parameters(params): Parameters<ListRoomsParams>) -> String {
        let project_id = params.project_id.unwrap_or_else(|| self.project_id.clone());
        let rooms = self
            .engine
            .list_rooms(Some(&project_id), params.include_null.unwrap_or(false));
        if rooms.is_empty() {
            return "🏠 当前项目没有找到任何 room（尝试 include_null=true 查看未分类计数）"
                .to_string();
        }

        let mut lines = vec!["🏠 Palace Rooms：".to_string()];
        for entry in rooms {
            let label = entry.room.unwrap_or_else(|| "(未分类)".to_string());
            let last_used = entry.last_used_at.unwrap_or_else(|| "-".to_string());
            lines.push(format!(
                "  - {label}: {} 条 (最近使用: {last_used})",
                entry.count
            ));
        }
        lines.push(String::new());
        lines.push("💡 AI 在 add_rule 前可先调 list_rooms 避免 room 命名漂移。".to_string());
        lines.join("\n")
    }

    #[tool(description = "Search experience substrate")]
    fn search_experiences(
        &self,
        Parameters(params): Parameters<SearchExperiencesParams>,
    ) -> String {
        let Some(query) = params
            .query
            .as_deref()
            .filter(|query| !query.trim().is_empty())
        else {
            return unsupported_in_rust_json("search_experiences");
        };
        serde_json::to_string_pretty(&self.experience.search_experiences(
            query,
            params.limit.unwrap_or(5),
            params.statuses.as_deref(),
            params.kind.as_deref(),
        ))
        .unwrap_or_else(|_| r#"{"error":"serialize failed"}"#.to_string())
    }

    #[tool(description = "Review an experience with expert lenses")]
    fn review_experience(&self, Parameters(params): Parameters<ReviewExperienceParams>) -> String {
        if missing_or_empty(params.experience_id.as_deref())
            && missing_or_empty(params.topic.as_deref())
        {
            return unsupported_in_rust_json("review_experience");
        }
        serde_json::to_string_pretty(&self.experience.review_experience(
            params.experience_id.as_deref(),
            params.topic.as_deref(),
            params.force.unwrap_or(false),
        ))
        .unwrap_or_else(|_| r#"{"error":"serialize failed"}"#.to_string())
    }

    #[tool(description = "Read Hermes/Harness/AutoResearch experience surfaces")]
    fn get_experience_surfaces(
        &self,
        Parameters(params): Parameters<GetExperienceSurfacesParams>,
    ) -> String {
        serde_json::to_string_pretty(&self.experience.get_surfaces(
            params.target.as_deref().unwrap_or("all"),
            params.limit.unwrap_or(6),
            params.topic.as_deref(),
            params.auto_topic.unwrap_or(false),
        ))
        .unwrap_or_else(|_| r#"{"error":"serialize failed"}"#.to_string())
    }

    #[tool(description = "Build experience artifacts")]
    fn build_experience_artifacts(
        &self,
        Parameters(params): Parameters<BuildExperienceArtifactsParams>,
    ) -> String {
        if params.target.is_none()
            && params.limit.is_none()
            && params.save.is_none()
            && params.topic.is_none()
            && params.auto_topic.is_none()
        {
            return unsupported_in_rust_json("build_experience_artifacts");
        }
        serde_json::to_string_pretty(&self.experience.build_artifacts(
            params.target.as_deref().unwrap_or("all"),
            params.limit.unwrap_or(8),
            params.topic.as_deref(),
            params.auto_topic.unwrap_or(false),
            params.save.unwrap_or(true),
        ))
        .unwrap_or_else(|_| r#"{"error":"serialize failed"}"#.to_string())
    }

    #[tool(description = "Record topic feedback for experience routing")]
    fn report_topic_feedback(
        &self,
        Parameters(params): Parameters<ReportTopicFeedbackParams>,
    ) -> String {
        let (Some(topic), Some(verdict)) = (
            params
                .topic
                .as_deref()
                .filter(|topic| !topic.trim().is_empty()),
            params
                .verdict
                .as_deref()
                .filter(|verdict| !verdict.trim().is_empty()),
        ) else {
            return unsupported_in_rust_json("report_topic_feedback");
        };
        serde_json::to_string_pretty(&self.experience.record_topic_feedback(
            topic,
            verdict,
            params.artifact_target.as_deref(),
            params.source.as_deref().unwrap_or("user"),
            params.reason.as_deref(),
            None,
        ))
        .unwrap_or_else(|_| r#"{"error":"serialize failed"}"#.to_string())
    }

    #[tool(description = "Record artifact feedback")]
    fn report_artifact_feedback(
        &self,
        Parameters(params): Parameters<ReportArtifactFeedbackParams>,
    ) -> String {
        let Some(verdict) = params
            .verdict
            .as_deref()
            .filter(|verdict| !verdict.trim().is_empty())
        else {
            return unsupported_in_rust_json("report_artifact_feedback");
        };
        serde_json::to_string_pretty(&self.experience.report_artifact_feedback(
            params.feedback_token.as_deref(),
            params.artifact_run_id.as_deref(),
            verdict,
            params.reason.as_deref(),
            params.source.as_deref().unwrap_or("artifact"),
        ))
        .unwrap_or_else(|_| r#"{"error":"serialize failed"}"#.to_string())
    }

    #[tool(description = "Write a new memory (with dedup)")]
    fn add_rule(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(params): Parameters<AddRuleParams>,
    ) -> String {
        let actor = resolve_actor(&ctx);
        let _permit = match self.write_admission.try_enter() {
            Ok(permit) => permit,
            Err(error) => {
                self.audit_operation_with_session(
                    "add_rule",
                    "admission_rejected",
                    &ctx,
                    Some(&actor),
                );
                return write_admission_error_json(error);
            }
        };

        // Issue #116: event_log writes get ("ai", "ai"); regular fact /
        // procedure writes stay ("user", "user"). Mirrors Python's
        // handle_add_memory → handle_log_event branch.
        let (default_source, default_created_by) =
            infer_add_rule_actor_defaults(params.memory_kind.as_deref(), params.when.as_deref());
        let (auto_classify, auto_classify_layer) =
            infer_add_rule_auto_classification(params.memory_kind.as_deref());

        let add_params = AddMemoryParams {
            content: params.content,
            category: params.category,
            confidence: params.confidence,
            agent: params.agent,
            room: params.room,
            role: params.role,
            difficulty: params.difficulty,
            time_cost_hint: params.time_cost_hint,
            related_ids: params.related_ids,
            when: params.when,
            where_: params.r#where,
            who: params.who,
            ttl_days: params.ttl_days,
            memory_kind: params.memory_kind,
            session_id: Some(self.resolve_session_id(&ctx)),
            source: Some(default_source.to_string()),
            created_by: Some(default_created_by.to_string()),
            auto_classify,
            auto_classify_layer,
            ..Default::default()
        };

        match self.writer.add_memory(&add_params) {
            AddMemoryResult::Saved {
                id,
                layer,
                cold_storage_ref,
                superseded_ids,
                warnings,
            } => {
                self.audit_operation_with_session("add_rule", "saved", &ctx, Some(&actor));
                let mut msg = format!("✅ 记忆已保存\n- ID: {id}\n- 层级: {layer}\n");
                if let Some(ref cref) = cold_storage_ref {
                    msg.push_str(&format!("- 冷存储: {cref}\n"));
                }
                if !superseded_ids.is_empty() {
                    msg.push_str(&format!("- 自动替代: {}\n", superseded_ids.join(", ")));
                }
                for w in &warnings {
                    msg.push_str(&format!("⚠️ {w}\n"));
                }
                msg
            }
            AddMemoryResult::Duplicate {
                existing_id,
                similarity,
            } => {
                self.audit_operation_with_session("add_rule", "duplicate", &ctx, Some(&actor));
                format!(
                    "⚠️ 重复检测：与已有记忆相似度 {:.1}%，超过阈值 88%\n- 已有 ID: {existing_id}\n- 状态: duplicate_detected",
                    similarity * 100.0
                )
            }
            AddMemoryResult::PolicySkipped {
                layer,
                reason,
                memory_type,
            } => {
                self.audit_operation_with_session("add_rule", "policy_skipped", &ctx, Some(&actor));
                let mtype = memory_type.as_deref().unwrap_or("unclassified");
                format!(
                    "⚠️ 写入被 MemoryPolicy 拒绝\n- 层级: {layer}\n- 类型: {mtype}\n- 原因: {reason}\n- 状态: policy_skipped"
                )
            }
            AddMemoryResult::Error(e) => {
                self.audit_operation_with_session("add_rule", "error", &ctx, Some(&actor));
                format!("❌ 写入失败: {e}")
            }
        }
    }

    #[tool(description = "Get context snapshot (wake or full mode)")]
    fn get_context(&self, Parameters(params): Parameters<GetContextParams>) -> String {
        let mode = params.mode.as_deref().unwrap_or("wake");

        match mode {
            "wake" => {
                let snapshot = self
                    .engine
                    .get_context_wake_for_project(Some(&self.project_id));
                serde_json::to_string_pretty(&snapshot).unwrap_or_else(|_| "{}".to_string())
            }
            "full" => {
                let snapshot = self
                    .engine
                    .get_context_full_for_project(Some(&self.project_id));
                serde_json::to_string_pretty(&snapshot).unwrap_or_else(|_| "{}".to_string())
            }
            _ => format!(r#"{{"error":"unknown mode: {mode}"}}"#),
        }
    }

    #[tool(description = "Propose L0 change (3x approval required)")]
    fn propose_change(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(params): Parameters<ProposeChangeParams>,
    ) -> String {
        let actor = resolve_actor(&ctx);
        // D-phase (IMPL-01b): require a valid write token for L0 mutations on
        // the HTTP path. stdio path is exempt (trusted local process).
        if let Err(err) = self.require_write_token(&ctx, &actor) {
            self.audit_operation("propose_change", "write_token_rejected", Some(&actor));
            return write_token_error_json(err);
        }
        self.propose_change_with_actor(&actor, params)
    }

    #[tool(description = "Approve L0 change")]
    fn approve_change(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(params): Parameters<ApproveChangeParams>,
    ) -> String {
        let actor = resolve_actor(&ctx);
        // D-phase (IMPL-01b): require a valid write token for L0 mutations on
        // the HTTP path.
        if let Err(err) = self.require_write_token(&ctx, &actor) {
            self.audit_operation("approve_change", "write_token_rejected", Some(&actor));
            return write_token_error_json(err);
        }
        self.approve_change_with_actor(&actor, params)
    }

    #[tool(description = "Save task breakpoint")]
    fn save_checkpoint(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(params): Parameters<SaveCheckpointParams>,
    ) -> String {
        let actor = resolve_actor(&ctx);
        self.save_checkpoint_with_actor(&actor, params)
    }

    #[tool(description = "Find saved breakpoints")]
    fn search_checkpoints(
        &self,
        Parameters(params): Parameters<SearchCheckpointsParams>,
    ) -> String {
        let results = self.engine.search_checkpoints_for_project(
            params.query.as_deref(),
            params.task_id.as_deref(),
            params.task_status.as_deref(),
            Some(&self.project_id),
            params.limit.unwrap_or(5),
        );

        if results.is_empty() {
            return "No checkpoints found.".to_string();
        }

        let mut output = String::new();
        for (i, r) in results.iter().enumerate() {
            let meta = r.metadata.as_ref();
            let task_id = meta
                .and_then(|m| m.get("task_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let status = meta
                .and_then(|m| m.get("task_status"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let next_step = meta
                .and_then(|m| m.get("next_step"))
                .and_then(|v| v.as_str());
            let blocker = meta.and_then(|m| m.get("blocker")).and_then(|v| v.as_str());

            let status_icon = match status {
                "blocked" => "🔴",
                "in_progress" => "🟡",
                "completed" => "🟢",
                _ => "⚪",
            };

            output.push_str(&format!(
                "{i}. {status_icon} {task_id}\n   {}\n",
                safe_truncate(&r.content, 200)
            ));
            if let Some(ns) = next_step {
                output.push_str(&format!("   ➡️ 下一步: {}\n", safe_truncate(ns, 200)));
            }
            if let Some(b) = blocker {
                output.push_str(&format!("   ❌ 阻塞: {}\n", safe_truncate(b, 200)));
            }
            output.push('\n');
        }

        output
    }

    #[tool(description = "Feedback: confirmed/corrected/outdated")]
    fn report_outcome(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(params): Parameters<ReportOutcomeParams>,
    ) -> String {
        let actor = resolve_actor(&ctx);
        let _permit = match self.write_admission.try_enter() {
            Ok(permit) => permit,
            Err(error) => {
                self.audit_operation_with_session(
                    "report_outcome",
                    "admission_rejected",
                    &ctx,
                    Some(&actor),
                );
                return write_admission_error_json(error);
            }
        };

        let reason = params.reason.as_deref();

        match self
            .writer
            .report_outcome(&params.memory_id, &params.outcome, reason)
        {
            Ok(true) => {
                self.audit_operation_with_session(
                    "report_outcome",
                    &params.outcome,
                    &ctx,
                    Some(&actor),
                );
                let emoji = match params.outcome.as_str() {
                    "confirmed" => "✅",
                    "corrected" => "🔄",
                    "outdated" => "📦",
                    _ => "❓",
                };
                format!(
                    "{emoji} 反馈已记录\n- 记忆 ID: {}\n- 结果: {}",
                    params.memory_id, params.outcome
                )
            }
            Ok(false) => {
                self.audit_operation_with_session(
                    "report_outcome",
                    "invalid_outcome",
                    &ctx,
                    Some(&actor),
                );
                format!("⚠️ 未知的 outcome 类型: {}", params.outcome)
            }
            Err(e) => {
                self.audit_operation_with_session("report_outcome", "error", &ctx, Some(&actor));
                format!("❌ 反馈记录失败: {e}")
            }
        }
    }
}

/// Resolve the authenticated actor for a request.
///
/// HTTP path: extracts the `AuthenticatedActor` inserted by `auth_middleware`.
/// stdio path: synthesises a per-process principal with a stable identity derived
/// from the current PID and the blake3 hash of the process executable path.
/// This ensures that a single stdio process can approve a change only once
/// (governance dedup check in `approve_change_for_principal` handles the rest).
pub fn resolve_actor(ctx: &RequestContext<RoleServer>) -> AuthenticatedActor {
    // HTTP path: actor was inserted into extensions by auth_middleware
    if let Some(parts) = ctx.extensions.get::<axum::http::request::Parts>() {
        if let Some(actor) = parts.extensions.get::<AuthenticatedActor>() {
            return actor.clone();
        }
    }
    // stdio path: synthesise a stable per-process principal
    trusted_stdio_actor()
}

fn trusted_stdio_actor() -> AuthenticatedActor {
    let pid = std::process::id();
    let exe = match std::env::current_exe().and_then(|p| p.canonicalize()) {
        Ok(p) => p.display().to_string(),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "current_exe()/canonicalize() failed; stdio principal falls back to 'unknown'. \
                 Audit trail for this process may coalesce with other unknown-exe processes."
            );
            "unknown".into()
        }
    };
    let exe_hex = blake3::hash(exe.as_bytes()).to_hex().to_string();
    debug_assert_eq!(
        exe_hex.len(),
        64,
        "blake3 hex must be 64 chars; library swap may have broken this invariant"
    );
    let exe_hash = exe_hex.get(..16).unwrap_or(&exe_hex);
    let actor_id = format!("stdio-{pid}-{exe_hash}");
    AuthenticatedActor {
        actor_id,
        key_name: Some("stdio".to_string()),
        key_hash: String::new(),
    }
}
