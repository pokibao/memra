//! Write orchestrator: dedup, auto-supersede, embed, write.
//!
//! Ported from `backend/core/write_orchestrator.py`.

use std::collections::HashSet;

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension};
use tracing::{info, warn};
use uuid::Uuid;

use crate::core::auto_classifier::AutoClassifier;
use crate::core::commercial_number_detector::detect_with_neighbor_sources;
use crate::core::memory_policy::{EvaluateInput, MemoryPolicy};
use crate::core::safety_filter::safe_text_prefix;
use crate::embedding::EMBEDDING_DIM;
use crate::retrieval::scoring::cosine_similarity;
use crate::storage::cold_storage::ColdStorageWriter;
use crate::storage::db::DbPool;
use crate::storage::writer::{
    NoteInsert, insert_note_relation, mark_superseded, upsert_note, vector_to_blob,
};

/// Dedup thresholds (matching Python).
const DEDUP_SIMILARITY_THRESHOLD: f64 = 0.88;
const AUTO_SUPERSEDE_THRESHOLD: f64 = 0.70;
/// Mirrors Python `TOPIC_INHERIT_THRESHOLD` in write_orchestrator.py: a
/// best-match similarity >= 0.78 makes the new note inherit topic_key from
/// the matched note even when no auto-supersede fires.
const TOPIC_INHERIT_THRESHOLD: f64 = 0.78;
const DEDUP_CANDIDATE_LIMIT: usize = 50;
const REVIEW_AFTER_DAYS: i64 = 30;
const MUTABLE_CATEGORIES: &[&str] = &["person", "event"];
const MUTABLE_CONTENT_MARKERS: &[&str] =
    &["月支出", "月收入", "团队", "员工", "现状", "目前", "当前"];
const CROSS_LAYER_SOFT_THRESHOLD: f64 = 0.62;
const CROSS_LAYER_HARD_THRESHOLD: f64 = 0.85;
const CROSS_LAYER_SCAN_LIMIT: i64 = 100;
const CROSS_LAYER_MUTABLE_KEYWORDS: &[&str] = &[
    "团队",
    "员工",
    "人员",
    "staff",
    "team",
    "成员",
    "联系人",
    "月支出",
    "月收入",
    "开销",
    "费用",
    "收入",
    "支出",
    "财务",
    "debt",
    "expense",
    "budget",
    "预算",
    "报价",
    "汇率",
    "状态",
    "现状",
    "目前",
    "当前",
    "进度",
    "阶段",
    "status",
    "current",
    "项目",
    "project",
    "plan",
    "计划",
    "版本",
    "version",
    "v1",
    "v2",
    "v3",
    "v4",
    "v5",
];

/// Auto-link thresholds (Sprint 5 P0-1, matching Python write_orchestrator.py).
/// Lower bound = 0.50 (Python `AUTO_LINK_THRESHOLD`).
/// Upper bound exclusive = DEDUP_SIMILARITY_THRESHOLD — above that is dedup territory.
const AUTO_LINK_LOWER: f64 = 0.50;
const AUTO_LINK_MAX_LINKS: usize = 3;

enum StoredVectorState {
    Missing,
    Present(usize),
    Invalid,
}

fn stored_vector_state(blob: Option<&[u8]>, json: Option<&str>) -> StoredVectorState {
    if let Some(blob) = blob {
        if blob.len() >= 4 && blob.len() % 4 == 0 {
            return StoredVectorState::Present(blob.len() / 4);
        }
        return StoredVectorState::Invalid;
    }

    if let Some(json) = json {
        return match serde_json::from_str::<Vec<f32>>(json) {
            Ok(vector) => StoredVectorState::Present(vector.len()),
            Err(_) => StoredVectorState::Invalid,
        };
    }

    StoredVectorState::Missing
}

fn exact_duplicate_vector_compatible(
    blob: Option<&[u8]>,
    json: Option<&str>,
    query_dim: Option<usize>,
) -> bool {
    match stored_vector_state(blob, json) {
        // No stored vector: compatible iff we also have no query vector.
        StoredVectorState::Missing => query_dim.is_none(),
        // Stored vector exists: matching dimension is compatible. When we
        // cannot embed the current query (embedding outage, e.g. Ollama
        // offline), trust the prior successful embedding instead of
        // skipping duplicate protection — otherwise an outage would let
        // identical content be re-written unchecked. Bot P1 on PR #138
        // HEAD 3e7cc00.
        StoredVectorState::Present(stored_dim) => match query_dim {
            Some(dim) => dim == stored_dim,
            None => true,
        },
        StoredVectorState::Invalid => false,
    }
}

fn stored_vector_from_parts(blob: Option<&[u8]>, json: Option<&str>) -> Option<Vec<f32>> {
    if let Some(blob) = blob {
        if blob.len() < 4 || blob.len() % 4 != 0 {
            return None;
        }
        return Some(
            blob.chunks_exact(4)
                .filter_map(|chunk| chunk.try_into().ok().map(f32::from_le_bytes))
                .collect(),
        );
    }

    json.and_then(|raw| serde_json::from_str::<Vec<f32>>(raw).ok())
}

/// Result of an add_memory operation.
#[derive(Debug)]
pub enum AddMemoryResult {
    /// Successfully written.
    Saved {
        id: String,
        layer: String,
        cold_storage_ref: Option<String>,
        superseded_ids: Vec<String>,
        /// Warnings (e.g., embedding failed — note won't appear in semantic search).
        warnings: Vec<String>,
    },
    /// Rejected as duplicate.
    Duplicate {
        existing_id: String,
        similarity: f64,
    },
    /// Rejected by MemoryPolicy (transient / repo-derivable / unclassified).
    /// Mirrors Python `{"status": "policy_skipped", ...}` at
    /// `backend/core/write_orchestrator.py:1180-1186`.
    PolicySkipped {
        layer: String,
        reason: String,
        memory_type: Option<String>,
    },
    /// Failed to write.
    Error(String),
}

/// Parameters for add_memory.
#[derive(Debug, Clone, Default)]
pub struct AddMemoryParams {
    pub content: String,
    pub layer: Option<String>,
    pub category: Option<String>,
    pub confidence: Option<f64>,
    pub agent: Option<String>,
    pub room: Option<String>,
    pub role: Option<String>,
    pub difficulty: Option<i64>,
    pub time_cost_hint: Option<String>,
    pub related_ids: Option<Vec<String>>,
    pub when: Option<String>,
    pub where_: Option<String>,
    pub who: Option<Vec<String>>,
    pub ttl_days: Option<i64>,
    pub memory_kind: Option<String>,
    /// Session that initiated this write. Python MCP fills this from
    /// StateManager; Rust service fills a per-process session id.
    pub session_id: Option<String>,
    /// Origin of this write. MCP handler should set to Some("user"). Non-MCP
    /// callers (autoresearch seeders, consolidation) may pass other values
    /// like "ai_extraction" to mirror Python storage_rows.py behaviour.
    pub source: Option<String>,
    /// Identity of the caller that initiated this write. MCP handler defaults
    /// to "user" to match Python MCP path (`backend/mcp_memory.py:1171`).
    pub created_by: Option<String>,
    /// Extra metadata fields to include.
    pub extra_metadata: Option<serde_json::Map<String, serde_json::Value>>,
    /// Optional lineage root to stamp on the inserted note.
    pub root_id: Option<String>,
    /// Optional lineage version to stamp on the inserted note.
    pub version: Option<i64>,
    /// Optional topic key override for batch/import callers that already own
    /// the lineage decision.
    pub topic_key: Option<String>,
    /// Enable deterministic rule classification when category/layer are not
    /// already supplied by the caller. Defaults to false to preserve current
    /// Rust write-path behavior until each caller opts in.
    pub auto_classify: bool,
    /// Allow auto-classification to select the layer, but only when the caller
    /// omitted layer. Mirrors Python `auto_classify_layer`.
    pub auto_classify_layer: bool,
}

/// The write orchestrator manages the full add_memory pipeline.
pub struct WriteOrchestrator {
    db: DbPool,
    cold_storage: ColdStorageWriter,
    project_id: String,
}

impl WriteOrchestrator {
    pub fn new(db: DbPool, cold_storage: ColdStorageWriter, project_id: String) -> Self {
        Self {
            db,
            cold_storage,
            project_id,
        }
    }

    /// Add a new memory with dedup, auto-supersede, and cold storage.
    pub fn add_memory(&self, params: &AddMemoryParams) -> AddMemoryResult {
        self.db
            .with_conn(|conn| self.add_memory_inner(conn, params))
    }

    fn add_memory_inner(&self, conn: &Connection, params: &AddMemoryParams) -> AddMemoryResult {
        let content = params.content.trim();
        if content.is_empty() {
            return AddMemoryResult::Error("Content is empty".to_string());
        }

        let original_layer_missing = params.layer.as_deref().is_none_or(|layer| layer.is_empty());

        // Determine layer
        let mut layer = match params.layer.as_deref() {
            Some(l) => l.to_string(),
            None => {
                if params.when.is_some() {
                    "event_log".to_string()
                } else {
                    match params.memory_kind.as_deref() {
                        Some("event") => "event_log".to_string(),
                        Some("procedure") => "procedure_schema".to_string(),
                        _ => "verified_fact".to_string(),
                    }
                }
            }
        };
        let mut category = params.category.clone();

        if layer == "procedure_schema" {
            category.get_or_insert_with(|| "routine".to_string());
        } else if params.auto_classify {
            let classification = AutoClassifier::default().classify(content, Some(&layer));
            if category.is_none() && !classification.category.is_empty() {
                category = Some(classification.category);
            }
            if params.auto_classify_layer
                && original_layer_missing
                && is_auto_classified_layer_allowed(&classification.layer)
            {
                layer = classification.layer;
            }
        }

        // L0 write guard: identity_schema requires propose_change + 3x approve_change
        if layer == "identity_schema" {
            return AddMemoryResult::Error(
                "Cannot write to identity_schema directly. Use propose_change + approve_change."
                    .to_string(),
            );
        }

        let mut confidence = params.confidence.unwrap_or(0.9);
        let mut source = params.source.clone();
        let is_event_layer = layer == "event_log";

        // MemoryPolicy gate: verified_fact layer gets governance + 7-key
        // typed metadata injection. Mirrors Python write_orchestrator.py:1166-1186.
        // Run BEFORE embed/dedup so transient or derivable content short-circuits
        // without burning the embedding call.
        let policy_decision = if layer == "verified_fact" {
            let decision = MemoryPolicy::new().evaluate(EvaluateInput {
                content,
                layer: Some(&layer),
                category: category.as_deref(),
                source: source.as_deref(),
                metadata: params.extra_metadata.as_ref(),
                agent: params.agent.as_deref(),
            });
            if !decision.allow_durable {
                return AddMemoryResult::PolicySkipped {
                    layer: layer.clone(),
                    reason: decision.reason,
                    memory_type: decision.memory_type,
                };
            }
            Some(decision)
        } else {
            None
        };

        // Embed content for dedup and storage
        let embedding = self.embed_content(content);

        // Runtime dim gate: reject write if embedding has wrong dimension.
        // embed_text already logs a warn on mismatch; this is the write-path hard guard.
        if let Some(ref emb) = embedding {
            if emb.len() != EMBEDDING_DIM {
                return AddMemoryResult::Error(format!(
                    "Embedding dimension mismatch: got {} expected {}. \
                     Refusing write to avoid corrupting vector index.",
                    emb.len(),
                    EMBEDDING_DIM
                ));
            }
        }

        // Dedup check (skip for events — they're episodic, duplicates are expected)
        let mut superseded_ids = Vec::new();
        // Captured for issue #112 topic_key inheritance when sim ∈ [0.78, AUTO_SUPERSEDE_THRESHOLD).
        let mut dedup_best_match_id: Option<String> = None;
        let mut dedup_best_sim: f64 = 0.0;
        if !is_event_layer {
            let query_dim = embedding.as_ref().map(Vec::len);
            if let Some(existing_id) = self.find_exact_duplicate(conn, content, &layer, query_dim) {
                return AddMemoryResult::Duplicate {
                    existing_id,
                    similarity: 1.0,
                };
            }
            if let Some(ref emb) = embedding {
                let dedup = self.check_duplicate(conn, emb, &layer);
                if dedup.duplicate {
                    if let Some(ref best) = dedup.best_match {
                        return AddMemoryResult::Duplicate {
                            existing_id: best.id.clone(),
                            similarity: dedup.best_sim,
                        };
                    }
                }

                dedup_best_sim = dedup.best_sim;
                if let Some(ref best) = dedup.best_match {
                    dedup_best_match_id = Some(best.id.clone());
                }

                // Auto-supersede: 0.70 <= sim <= 0.88
                // Note: actual mark_superseded happens INSIDE the transaction below
                if dedup.best_sim >= AUTO_SUPERSEDE_THRESHOLD {
                    if let Some(ref best) = dedup.best_match {
                        info!(
                            "Will auto-supersede {} (sim={:.4})",
                            best.id, dedup.best_sim
                        );
                        superseded_ids.push(best.id.clone());
                    }
                }
            }
        }

        // Generate note ID
        let note_id = Uuid::new_v4().to_string();
        let now_dt = Utc::now();
        let now = now_dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let valid_at = valid_at_epoch_seconds(params.when.as_deref(), &now_dt);
        // Issue #118: persist event_when (raw RFC3339) + event_when_ts
        // (epoch seconds) so Rust matches Python's storage_rows.py columns.
        let event_when = params.when.clone();
        let event_when_ts = event_when_epoch_seconds(event_when.as_deref());
        let review_after = compute_review_after(
            &layer,
            category.as_deref(),
            params.who.as_deref(),
            content,
            &now_dt,
        );

        let mut metadata = params.extra_metadata.clone().unwrap_or_default();
        let detector_trigger = commercial_detector_trigger(&metadata);
        let detector_result = detect_with_neighbor_sources(
            content,
            detector_trigger.as_deref(),
            source.as_deref().unwrap_or(""),
            || {
                commercial_detector_neighbor_sources(
                    conn,
                    params.related_ids.as_deref(),
                    embedding.as_deref(),
                    &layer,
                    &self.project_id,
                )
            },
        );
        if detector_result.flagged {
            metadata.insert(
                "outdated_pending".to_string(),
                serde_json::Value::Bool(true),
            );
            metadata.insert(
                "detector_reason".to_string(),
                serde_json::Value::String(detector_result.reason),
            );
            source = Some("ai_proposal".to_string());
            confidence = confidence.min(0.7);
        }

        // Build metadata
        // Merge MemoryPolicy typed metadata (7 base keys + optional policy_warning
        // / feedback_kind / living_doc keys). Python dict.update skips None values;
        // we mirror that by filtering JSON null.
        if let Some(ref decision) = policy_decision {
            for (k, v) in &decision.metadata {
                if !v.is_null() {
                    metadata.insert(k.clone(), v.clone());
                }
            }
        }
        if let Some(ref kind) = params.memory_kind {
            metadata.insert(
                "memory_kind".to_string(),
                serde_json::Value::from(kind.as_str()),
            );
        }
        if let Some(ref who) = params.who {
            metadata.insert("who".to_string(), serde_json::Value::from(who.clone()));
        }
        if let Some(ttl) = params.ttl_days {
            metadata.insert("ttl_days".to_string(), serde_json::Value::from(ttl));
        }
        if let Some(ref role) = params.role {
            metadata.insert("role".to_string(), serde_json::Value::from(role.as_str()));
        }
        // Issue #114: do NOT stamp `supersedes` into the new note's metadata.
        // Python (backend/core/write_orchestrator.py) does not emit this key —
        // the supersede link lives only on the OLD row (`superseded_by` +
        // `is_outdated`) plus the SQL evolution_state flip. Mirror that.

        let metadata_json = if metadata.is_empty() {
            None
        } else {
            match serde_json::to_string(&metadata) {
                Ok(s) => Some(s),
                Err(e) => {
                    return AddMemoryResult::Error(format!("metadata serialize failed: {e}"));
                }
            }
        };

        // Build vector storage
        let (vector_json, vector_blob) = match embedding {
            Some(ref vec) => {
                let json = match serde_json::to_string(vec) {
                    Ok(s) => s,
                    Err(e) => {
                        return AddMemoryResult::Error(format!("vector serialize failed: {e}"));
                    }
                };
                (Some(json), Some(vector_to_blob(vec)))
            }
            None => (None, None),
        };

        let related_ids_json = match params.related_ids.as_ref() {
            Some(ids) => match serde_json::to_string(ids) {
                Ok(s) => Some(s),
                Err(e) => {
                    return AddMemoryResult::Error(format!("related_ids serialize failed: {e}"));
                }
            },
            None => None,
        };

        // Topic key: inherit from superseded note (issue #112 parity with
        // backend/core/write_orchestrator.py:1976-1985), else high-similarity
        // best_match, else build fresh from content.
        let topic_key = params.topic_key.clone().or_else(|| {
            decide_topic_key(
                conn,
                &layer,
                content,
                is_event_layer,
                &superseded_ids,
                dedup_best_match_id.as_deref(),
                dedup_best_sim,
            )
        });

        let note = NoteInsert {
            id: note_id.clone(),
            content: content.to_string(),
            layer: layer.clone(),
            category: category.clone(),
            is_active: true,
            confidence: Some(confidence),
            agent_id: None,
            project_id: Some(self.project_id.clone()),
            created_at: Some(now),
            // Issue #115: leave NULL on first insert to mirror Python
            // (backend/core/storage_rows.py). The upsert_note() ON CONFLICT
            // CASE branch fills it in only when a real material-change update
            // happens later (PR #93 covered the UPSERT path).
            updated_at: None,
            valid_at,
            session_id: if is_event_layer {
                None
            } else {
                params.session_id.clone()
            },
            review_after,
            metadata_json,
            vector_json,
            vector_blob,
            evolution_state: Some("active".to_string()),
            topic_key,
            is_head: true,
            room: params.room.clone(),
            agent: params.agent.clone(),
            difficulty: params.difficulty,
            time_cost_hint: params.time_cost_hint.clone(),
            related_ids_json,
            role: params.role.clone(),
            version: params.version,
            root_id: params.root_id.clone(),
            source,
            created_by: params.created_by.clone(),
            event_when: event_when.clone(),
            event_when_ts,
        };

        // Write within transaction
        if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
            return AddMemoryResult::Error(format!("Transaction start failed: {e}"));
        }

        if let Err(e) = upsert_note(conn, &note) {
            let _ = conn.execute_batch("ROLLBACK");
            return AddMemoryResult::Error(format!("Upsert failed: {e}"));
        }

        // Update superseded notes with new ID reference (inside transaction)
        for sid in &superseded_ids {
            // "auto_supersede" matches Python write_orchestrator.py:1429 for
            // the same-layer AUTO_SUPERSEDE_THRESHOLD (≥ 0.70) cascade.
            if let Err(e) = mark_superseded(conn, sid, &note_id, "auto_supersede") {
                let _ = conn.execute_batch("ROLLBACK");
                return AddMemoryResult::Error(format!("Failed to mark {sid} as superseded: {e}"));
            }

            // mark_superseded owns the forget_cascade edge so every Rust
            // supersede path, including batch-write mark-superseded, gets the
            // same successor -> superseded neural relation exactly once.
        }

        // Cold storage dual-write is part of the write contract when enabled:
        // append + cold_storage_ref backfill must happen before COMMIT so a
        // caller cannot observe a successful hot SQLite row with missing cold
        // archive provenance. A disabled writer returns Ok(None) for test and
        // opt-out paths.
        let cold_meta = cold_storage_metadata(params, &layer, category.as_deref());
        let cold_meta_val = if cold_meta.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(cold_meta))
        };
        let cold_ref =
            match self
                .cold_storage
                .append(&note_id, content, &layer, cold_meta_val.as_ref())
            {
                Ok(cold_ref) => cold_ref,
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return AddMemoryResult::Error(format!("Cold storage append failed: {e}"));
                }
            };

        if let Some(ref cref) = cold_ref {
            if let Err(e) = conn.execute(
                "UPDATE notes SET cold_storage_ref = ?1 WHERE id = ?2",
                rusqlite::params![cref, note_id],
            ) {
                let _ = conn.execute_batch("ROLLBACK");
                return AddMemoryResult::Error(format!("Cold storage ref update failed: {e}"));
            }
        }

        if let Err(e) = conn.execute_batch("COMMIT") {
            return AddMemoryResult::Error(format!("Commit failed: {e}"));
        }

        if !is_event_layer {
            if let Some(ref emb) = embedding {
                self.cross_layer_contradiction_scan(
                    conn,
                    &note_id,
                    content,
                    category.as_deref(),
                    emb,
                    params.who.as_deref(),
                );
            }
        }

        // Sprint 5 P0-1: Auto-link related notes (matching Python _auto_link_related).
        // Runs AFTER commit so a link failure never rolls back the main write.
        // Only for non-event layers (Python: `not is_event_layer`).
        if !is_event_layer {
            if let Some(ref emb) = embedding {
                self.link_related(conn, &note_id, emb, &layer, &superseded_ids);
            }
        }

        self.store_extracted_entities(conn, &note_id, content);

        // Collect warnings
        let mut warnings = Vec::new();
        if embedding.is_none() {
            warnings.push(
                "Embedding failed: note saved without vector, will not appear in semantic search"
                    .to_string(),
            );
        }

        AddMemoryResult::Saved {
            id: note_id,
            layer,
            cold_storage_ref: cold_ref,
            superseded_ids,
            warnings,
        }
    }

    /// Save or update a checkpoint (UPSERT by task_id).
    pub fn save_checkpoint(
        &self,
        task_id: &str,
        summary: &str,
        metadata: serde_json::Map<String, serde_json::Value>,
    ) -> Result<String, String> {
        self.db.with_conn(|conn| {
            if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
                return Err(format!("Transaction start failed: {e}"));
            }

            let content = checkpoint_searchable_content(task_id, summary, &metadata);
            let metadata_value = serde_json::Value::Object(metadata.clone());

            let mut existing_stmt = match conn.prepare(
                "SELECT id, metadata_json FROM notes
                 WHERE is_active = 1
                   AND layer = 'event_log'
                   AND content = ?1
                   AND json_valid(metadata_json)
                   AND json_extract(metadata_json, '$.record_type') = 'checkpoint'
                   AND json_extract(metadata_json, '$.task_id') = ?2
                   AND project_id = ?3
                 ORDER BY COALESCE(updated_at, created_at) DESC, id ASC
                 LIMIT 5",
            ) {
                Ok(stmt) => stmt,
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(format!("Checkpoint idempotency probe failed: {e}"));
                }
            };
            let existing_rows = match existing_stmt
                .query_map((&content, task_id, &self.project_id), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                }) {
                Ok(rows) => rows,
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(format!("Checkpoint idempotency probe failed: {e}"));
                }
            };
            for row in existing_rows {
                let (existing_id, existing_metadata_json) = match row {
                    Ok(row) => row,
                    Err(e) => {
                        let _ = conn.execute_batch("ROLLBACK");
                        return Err(format!("Checkpoint idempotency probe failed: {e}"));
                    }
                };
                let existing_metadata_value = existing_metadata_json
                    .as_deref()
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
                if existing_metadata_value.as_ref() == Some(&metadata_value) {
                    match crate::storage::writer::deactivate_checkpoints_by_task_id(
                        conn,
                        task_id,
                        Some(&existing_id),
                    ) {
                        Ok(n) => {
                            if n > 0 {
                                info!("Deactivated {n} stale checkpoints for task '{task_id}'");
                            }
                        }
                        Err(e) => {
                            let _ = conn.execute_batch("ROLLBACK");
                            return Err(format!("Deactivate failed: {e}"));
                        }
                    }
                    if let Err(e) = conn.execute_batch("COMMIT") {
                        return Err(format!("Commit failed: {e}"));
                    }
                    return Ok(existing_id);
                }
            }

            let metadata_json =
                serde_json::to_string(&metadata).unwrap_or_else(|_| "{}".to_string());

            // Phase 1: deactivate old checkpoints with same task_id
            match crate::storage::writer::deactivate_checkpoints_by_task_id(conn, task_id, None) {
                Ok(n) => {
                    if n > 0 {
                        info!("Deactivated {n} old checkpoints for task '{task_id}'");
                    }
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(format!("Deactivate failed: {e}"));
                }
            }

            // Phase 2: write new checkpoint
            let note_id = Uuid::new_v4().to_string();
            let now_dt = Utc::now();
            let now = now_dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
            let embedding = self.embed_content(&content);
            let (vector_json, vector_blob) = match embedding {
                Some(ref emb) => (serde_json::to_string(emb).ok(), Some(vector_to_blob(emb))),
                None => (None, None),
            };

            let note = NoteInsert {
                id: note_id.clone(),
                content,
                layer: "event_log".to_string(),
                category: Some("event".to_string()),
                is_active: true,
                confidence: Some(0.95),
                project_id: Some(self.project_id.clone()),
                created_at: Some(now.clone()),
                valid_at: Some(timestamp_seconds(&now_dt)),
                metadata_json: Some(metadata_json),
                vector_json,
                vector_blob,
                is_head: true,
                evolution_state: Some("active".to_string()),
                source: Some("user".to_string()),
                created_by: Some("user".to_string()),
                ..Default::default()
            };

            if let Err(e) = upsert_note(conn, &note) {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(format!("Checkpoint write failed: {e}"));
            }

            let cold_meta = serde_json::json!({
                "category": "event",
                "confidence": 0.95,
                "source": "user",
                "created_by": "user",
            });
            let cold_ref = match self.cold_storage.append(
                &note_id,
                &note.content,
                &note.layer,
                Some(&cold_meta),
            ) {
                Ok(cold_ref) => cold_ref,
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(format!("Checkpoint cold storage append failed: {e}"));
                }
            };
            if let Some(ref cref) = cold_ref {
                if let Err(e) = conn.execute(
                    "UPDATE notes SET cold_storage_ref = ?1 WHERE id = ?2",
                    rusqlite::params![cref, note_id],
                ) {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(format!("Checkpoint cold storage ref update failed: {e}"));
                }
            }

            if let Err(e) = conn.execute_batch("COMMIT") {
                return Err(format!("Commit failed: {e}"));
            }

            Ok(note_id)
        })
    }

    /// Report outcome for a memory (confirmed/corrected/outdated).
    pub fn report_outcome(
        &self,
        note_id: &str,
        outcome: &str,
        reason: Option<&str>,
    ) -> Result<bool, String> {
        self.db.with_conn(|conn| {
            if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
                return Err(format!("Transaction start failed: {e}"));
            }

            match crate::storage::writer::atomic_update_outcome(conn, note_id, outcome, reason) {
                Ok(updated) => {
                    if updated {
                        crate::storage::writer::apply_hebbian_feedback(conn, note_id, outcome)
                            .map_err(|e| format!("Hebbian feedback update failed: {e}"))?;
                        crate::storage::writer::apply_dream_feedback(
                            conn, note_id, outcome, reason,
                        )
                        .map_err(|e| format!("Dream feedback update failed: {e}"))?;
                    }
                    if let Err(e) = conn.execute_batch("COMMIT") {
                        return Err(format!("Commit failed: {e}"));
                    }
                    Ok(updated)
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(format!("Outcome update failed: {e}"))
                }
            }
        })
    }

    /// Embed content using the shared fastembed singleton.
    fn embed_content(&self, content: &str) -> Option<Vec<f32>> {
        crate::embedding::embed_text(content)
    }

    /// Check if content is a duplicate of an existing memory.
    fn find_exact_duplicate(
        &self,
        conn: &Connection,
        content: &str,
        layer: &str,
        query_dim: Option<usize>,
    ) -> Option<String> {
        let mut stmt = conn
            .prepare(
                "SELECT id, vector_blob, vector_json
	             FROM notes
	             WHERE is_active = 1
	               AND layer = ?1
	               AND content = ?2
	               AND project_id = ?3
	             ORDER BY COALESCE(updated_at, created_at) DESC, id ASC
	             LIMIT ?4",
            )
            .ok()?;

        let rows = stmt
            .query_map(
                (
                    layer,
                    content,
                    &self.project_id,
                    DEDUP_CANDIDATE_LIMIT as i64,
                ),
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .ok()?;

        for row in rows.flatten() {
            let (id, blob, json) = row;
            if exact_duplicate_vector_compatible(blob.as_deref(), json.as_deref(), query_dim) {
                return Some(id);
            }
        }

        None
    }

    /// Check if content is a duplicate of an existing memory.
    fn check_duplicate(&self, conn: &Connection, query_vector: &[f32], layer: &str) -> DedupResult {
        let mut result = DedupResult::default();

        // Scan all active notes in the same layer for vector similarity
        let sql = "SELECT id, content, vector_blob, vector_json
                   FROM notes
                   WHERE is_active = 1 AND layer = ?1
                   ORDER BY created_at DESC
                   LIMIT ?2";

        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(e) => {
                warn!("Dedup query failed: {e}");
                return result;
            }
        };

        let limit = DEDUP_CANDIDATE_LIMIT as i64;
        let rows = match stmt.query_map(rusqlite::params![layer, limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!("Dedup iteration failed: {e}");
                return result;
            }
        };

        for row_result in rows {
            let (id, content, blob, json_str) = match row_result {
                Ok(r) => r,
                Err(_) => continue,
            };

            // Extract stored vector
            let stored_vec = if let Some(ref b) = blob {
                if b.len() >= 4 {
                    Some(
                        b.chunks_exact(4)
                            .filter_map(|c| c.try_into().ok().map(f32::from_le_bytes))
                            .collect::<Vec<f32>>(),
                    )
                } else {
                    None
                }
            } else {
                json_str
                    .as_deref()
                    .and_then(|s| serde_json::from_str(s).ok())
            };

            let Some(stored) = stored_vec else {
                continue;
            };

            // Dim-mismatch guard: skip legacy rows with wrong vector dimension.
            // This handles the 384-dim vs 1024-dim co-existence period without panicking.
            if stored.len() != query_vector.len() {
                warn!(
                    "check_duplicate: skipping row '{}' — dim mismatch (stored={} query={})",
                    id,
                    stored.len(),
                    query_vector.len()
                );
                continue;
            }

            let sim = cosine_similarity(query_vector, &stored);
            if sim > result.best_sim {
                result.best_sim = sim;
                result.best_match = Some(DedupMatch {
                    id,
                    content,
                    similarity: sim,
                });
            }
        }

        if result.best_sim > DEDUP_SIMILARITY_THRESHOLD {
            result.duplicate = true;
            info!(
                "Dedup hit: sim={:.4} > threshold={:.2}",
                result.best_sim, DEDUP_SIMILARITY_THRESHOLD
            );
        }

        result
    }

    /// Auto-link: find similar notes and write `note_relations` rows.
    ///
    /// Matches Python `_auto_link_related` (write_orchestrator.py:875).
    /// Trigger: non-event layer, cosine similarity in [AUTO_LINK_LOWER, DEDUP_SIMILARITY_THRESHOLD).
    /// Up to AUTO_LINK_MAX_LINKS relations are written with type "supports".
    ///
    /// Candidate query scopes to the current project and active evolution state
    /// to match Python's `find_similar_notes` (see issue #106): otherwise a
    /// multi-project DB can leak cross-project auto-link edges.
    fn cross_layer_contradiction_scan(
        &self,
        conn: &Connection,
        new_note_id: &str,
        content: &str,
        category: Option<&str>,
        embedding: &[f32],
        event_who: Option<&[String]>,
    ) -> Vec<String> {
        let mut search_terms = Vec::new();
        if let Some(who) = event_who {
            search_terms.extend(who.iter().filter(|item| !item.is_empty()).cloned());
        }

        let content_lower = content.to_ascii_lowercase();
        for keyword in CROSS_LAYER_MUTABLE_KEYWORDS {
            if content.contains(keyword) || content_lower.contains(keyword) {
                search_terms.push((*keyword).to_string());
            }
        }
        search_terms.sort();
        search_terms.dedup();
        search_terms.truncate(5);
        if search_terms.is_empty() {
            return Vec::new();
        }

        let mut stmt = match conn.prepare(
            "SELECT id, content, layer, category, metadata_json, vector_blob, vector_json
             FROM notes
             WHERE id != ?1
               AND is_active = 1
               AND project_id = ?2
             LIMIT ?3",
        ) {
            Ok(stmt) => stmt,
            Err(e) => {
                warn!("cross-layer contradiction scan prepare failed: {e}");
                return Vec::new();
            }
        };

        let rows = match stmt.query_map(
            rusqlite::params![new_note_id, self.project_id, CROSS_LAYER_SCAN_LIMIT],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        ) {
            Ok(rows) => rows,
            Err(e) => {
                warn!("cross-layer contradiction scan query failed: {e}");
                return Vec::new();
            }
        };

        let mut superseded = Vec::new();
        for row in rows.flatten() {
            let (row_id, row_content, row_layer, old_category, metadata_json, blob, vector_json) =
                row;
            let row_lower = row_content.to_ascii_lowercase();
            if !search_terms
                .iter()
                .any(|term| row_content.contains(term) || row_lower.contains(term))
            {
                continue;
            }

            let old_meta: serde_json::Map<String, serde_json::Value> = metadata_json
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or_default();
            if old_meta
                .get("record_type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|record_type| {
                    record_type == "checkpoint" || record_type == "procedure"
                })
            {
                continue;
            }

            let Some(stored_vec) =
                stored_vector_from_parts(blob.as_deref(), vector_json.as_deref())
            else {
                continue;
            };
            if stored_vec.len() != embedding.len() {
                continue;
            }

            let sim = cosine_similarity(embedding, &stored_vec);
            if sim < CROSS_LAYER_SOFT_THRESHOLD {
                continue;
            }
            if let (Some(new_cat), Some(old_cat)) = (category, old_category.as_deref()) {
                if new_cat != old_cat && sim < CROSS_LAYER_HARD_THRESHOLD {
                    continue;
                }
            }

            let hard = sim >= CROSS_LAYER_HARD_THRESHOLD;
            match self.mark_cross_layer_contradiction(conn, &row_id, new_note_id, sim, hard) {
                Ok(()) => {
                    superseded.push(row_id.clone());
                    info!(
                        "Cross-layer contradiction: {} {} (sim={:.3}, layer={}) -> {}",
                        if hard { "DEACTIVATED" } else { "OUTDATED" },
                        row_id,
                        sim,
                        row_layer.as_deref().unwrap_or(""),
                        new_note_id
                    );
                }
                Err(e) => warn!("cross-layer contradiction update failed for {row_id}: {e}"),
            }
        }

        superseded
    }

    fn mark_cross_layer_contradiction(
        &self,
        conn: &Connection,
        old_id: &str,
        new_id: &str,
        sim: f64,
        hard: bool,
    ) -> rusqlite::Result<()> {
        let outcome = if hard { "corrected" } else { "outdated" };
        let confidence_decay = if hard { 0.05 } else { 0.10 };
        let reason = format!("cross-layer contradiction with {new_id} (sim={sim:.3})");

        if hard {
            conn.execute(
                "UPDATE notes SET
                    evolution_state = 'superseded',
                    is_head = 0,
                    is_active = 0,
                    confidence = max(0.1, COALESCE(confidence, 0.9) - ?2),
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE id = ?1",
                rusqlite::params![old_id, confidence_decay],
            )?;
        } else {
            conn.execute(
                "UPDATE notes SET
                    confidence = max(0.1, COALESCE(confidence, 0.9) - ?2),
                    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE id = ?1",
                rusqlite::params![old_id, confidence_decay],
            )?;
        }

        let meta_str: Option<String> = conn
            .query_row(
                "SELECT metadata_json FROM notes WHERE id = ?1",
                [old_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let mut meta: serde_json::Map<String, serde_json::Value> = meta_str
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_default();
        meta.insert("superseded_by".to_string(), serde_json::Value::from(new_id));
        meta.insert(
            "supersede_reason".to_string(),
            serde_json::Value::from("cross_layer_contradiction"),
        );
        if hard {
            meta.insert("is_corrected".to_string(), serde_json::Value::Bool(true));
        } else {
            meta.insert("is_outdated".to_string(), serde_json::Value::Bool(true));
        }
        let prior_success = meta
            .get("success_count")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let prior_failure = meta
            .get("failure_count")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        meta.insert(
            "success_count".to_string(),
            serde_json::Value::from(prior_success),
        );
        meta.insert(
            "failure_count".to_string(),
            serde_json::Value::from(prior_failure + 1),
        );
        meta.insert(
            "last_outcome_reason".to_string(),
            serde_json::Value::from(reason),
        );
        let metadata_json = serde_json::to_string(&meta).map_err(|e| {
            rusqlite::Error::InvalidParameterName(format!("metadata serialize: {e}"))
        })?;
        conn.execute(
            "UPDATE notes SET metadata_json = ?1 WHERE id = ?2",
            rusqlite::params![metadata_json, old_id],
        )?;

        if hard {
            insert_note_relation(conn, new_id, old_id, "forget_cascade", 1.0)?;
        }
        info!("cross-layer contradiction outcome={outcome} old={old_id} new={new_id}");
        Ok(())
    }

    fn link_related(
        &self,
        conn: &Connection,
        new_note_id: &str,
        embedding: &[f32],
        layer: &str,
        exclude_ids: &[String],
    ) {
        let sql = "SELECT id, vector_blob, vector_json
                   FROM notes
                   WHERE is_active = 1
                     AND layer = ?1
                     AND project_id = ?2
                     AND COALESCE(evolution_state, 'active') = 'active'
                   ORDER BY created_at DESC
                   LIMIT ?3";

        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(e) => {
                warn!("link_related: prepare failed: {e}");
                return;
            }
        };

        let limit = DEDUP_CANDIDATE_LIMIT as i64;
        let rows = match stmt.query_map(rusqlite::params![layer, self.project_id, limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        }) {
            Ok(r) => r,
            Err(e) => {
                warn!("link_related: query failed: {e}");
                return;
            }
        };

        // Collect candidates ordered by descending similarity
        let mut candidates: Vec<(String, f64)> = Vec::new();
        for row_result in rows {
            let (id, blob, json_str) = match row_result {
                Ok(r) => r,
                Err(_) => continue,
            };

            // Skip self and superseded notes
            if id == new_note_id || exclude_ids.contains(&id) {
                continue;
            }

            let stored_vec = stored_vector_from_parts(blob.as_deref(), json_str.as_deref());

            let Some(stored) = stored_vec else { continue };

            // Dim-mismatch guard
            if stored.len() != embedding.len() {
                continue;
            }

            let sim = cosine_similarity(embedding, &stored);

            // Only link in the [0.50, 0.88) window — above 0.88 is dedup territory
            if (AUTO_LINK_LOWER..DEDUP_SIMILARITY_THRESHOLD).contains(&sim) {
                candidates.push((id, sim));
            }
        }

        // Sort descending by similarity, take top N
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        candidates.truncate(AUTO_LINK_MAX_LINKS);

        let mut linked_ids = Vec::new();
        for (target_id, sim) in candidates {
            // Issue #107: match Python `_auto_link_related` which writes
            // round(sim, 4). Keeps note_relations.strength deterministic
            // across runtimes for parity_check.
            let strength = (sim * 10_000.0).round() / 10_000.0;
            match insert_note_relation(conn, new_note_id, &target_id, "supports", strength) {
                Ok(()) => {
                    info!(
                        "Auto-link: {} → {} (sim={:.4})",
                        &new_note_id[..8.min(new_note_id.len())],
                        &target_id[..8.min(target_id.len())],
                        strength,
                    );
                    linked_ids.push(target_id);
                }
                Err(e) => {
                    warn!("Auto-link relation write failed: {e}");
                }
            }
        }

        if !linked_ids.is_empty() {
            self.backfill_related_ids(conn, new_note_id, &linked_ids);
        }
    }

    fn backfill_related_ids(&self, conn: &Connection, note_id: &str, linked_ids: &[String]) {
        let existing_json = match conn
            .query_row(
                "SELECT related_ids_json FROM notes WHERE id = ?1",
                [note_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
        {
            Ok(value) => value.flatten(),
            Err(e) => {
                warn!("Auto-link related_ids read failed: {e}");
                return;
            }
        };

        let mut merged = existing_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
            .unwrap_or_default();
        for linked_id in linked_ids {
            if !merged.iter().any(|existing| existing == linked_id) {
                merged.push(linked_id.clone());
            }
        }

        let Ok(related_json) = serde_json::to_string(&merged) else {
            warn!("Auto-link related_ids serialization failed");
            return;
        };
        if let Err(e) = conn.execute(
            "UPDATE notes SET related_ids_json = ?1 WHERE id = ?2",
            rusqlite::params![related_json, note_id],
        ) {
            warn!("Auto-link related_ids backfill failed: {e}");
        }
    }

    fn store_extracted_entities(&self, conn: &Connection, note_id: &str, content: &str) -> usize {
        let legacy_tables_ready = match conn.query_row(
            "SELECT COUNT(*)
             FROM sqlite_master
             WHERE type = 'table'
               AND name IN ('entities', 'memory_entities')",
            [],
            |row| row.get::<_, i64>(0),
        ) {
            Ok(count) => count == 2,
            Err(e) => {
                warn!("entity table availability check failed: {e}");
                false
            }
        };
        if !legacy_tables_ready {
            return 0;
        }

        let entities = extract_entities_for_parity(content);
        if entities.is_empty() {
            return 0;
        }

        let mut stored = 0;
        for (name, entity_type) in entities {
            if let Err(e) = conn.execute(
                "INSERT OR IGNORE INTO entities (name, entity_type) VALUES (?1, ?2)",
                rusqlite::params![name, entity_type],
            ) {
                warn!("entity insert failed: {e}");
                return stored;
            }

            let entity_id = match conn
                .query_row(
                    "SELECT id FROM entities WHERE name = ?1 AND entity_type = ?2",
                    rusqlite::params![name, entity_type],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
            {
                Ok(Some(id)) => id,
                Ok(None) => continue,
                Err(e) => {
                    warn!("entity lookup failed: {e}");
                    continue;
                }
            };

            match conn.execute(
                "INSERT OR IGNORE INTO memory_entities (memory_id, entity_id) VALUES (?1, ?2)",
                rusqlite::params![note_id, entity_id],
            ) {
                Ok(affected) => stored += affected,
                Err(e) => warn!("memory_entities link failed: {e}"),
            }
        }

        stored
    }
}

fn extract_entities_for_parity(content: &str) -> Vec<(String, String)> {
    if content.len() < 5 {
        return Vec::new();
    }

    let proper_name_re = match Regex::new(r"\b([A-Z][a-z]+(?:\s+[A-Z][a-z]+)+)\b") {
        Ok(re) => re,
        Err(e) => {
            warn!("entity regex compile failed: {e}");
            return Vec::new();
        }
    };

    let mut entities = Vec::new();
    for capture in proper_name_re.captures_iter(content) {
        let Some(match_) = capture.get(1) else {
            continue;
        };
        let name = match_.as_str().trim();
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "the" | "this" | "that" | "what" | "when" | "where" | "how"
        ) {
            continue;
        }
        if name.split_whitespace().count() <= 3
            && !entities
                .iter()
                .any(|(existing, _): &(String, String)| existing == name)
        {
            // Mirrors Python's standalone proper-name fallback, which labels
            // "Project Apollo" as a person-class entity in the legacy tables.
            entities.push((name.to_string(), "person".to_string()));
        }
    }

    entities
}

fn timestamp_seconds(dt: &DateTime<Utc>) -> f64 {
    dt.timestamp() as f64 + f64::from(dt.timestamp_subsec_nanos()) / 1_000_000_000.0
}

fn valid_at_epoch_seconds(event_when: Option<&str>, created_at: &DateTime<Utc>) -> Option<f64> {
    event_when_epoch_seconds(event_when).or_else(|| Some(timestamp_seconds(created_at)))
}

fn commercial_detector_trigger(
    metadata: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    for key in ["trigger", "source_trigger", "memory_trigger", "query"] {
        if let Some(value) = metadata.get(key).and_then(serde_json::Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn commercial_detector_neighbor_sources(
    conn: &Connection,
    related_ids: Option<&[String]>,
    embedding: Option<&[f32]>,
    layer: &str,
    project_id: &str,
) -> Vec<Option<String>> {
    let mut seen = HashSet::new();
    let mut sources = Vec::new();

    for related_id in related_ids.unwrap_or(&[]) {
        add_commercial_neighbor_source(conn, related_id, None, &mut seen, &mut sources);
    }

    let Some(embedding) = embedding else {
        return sources;
    };

    let sql = "SELECT id, source, metadata_json, vector_blob, vector_json
               FROM notes
               WHERE is_active = 1
                 AND layer = ?1
                 AND project_id = ?2
               ORDER BY created_at DESC
               LIMIT ?3";
    let mut stmt = match conn.prepare(sql) {
        Ok(stmt) => stmt,
        Err(error) => {
            warn!("commercial detector neighbor query prepare failed: {error}");
            return sources;
        }
    };
    let rows = match stmt.query_map(
        rusqlite::params![layer, project_id, DEDUP_CANDIDATE_LIMIT as i64],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        },
    ) {
        Ok(rows) => rows,
        Err(error) => {
            warn!("commercial detector neighbor query failed: {error}");
            return sources;
        }
    };

    let mut candidates = Vec::new();
    for row in rows.flatten() {
        let (id, source, metadata_json, vector_blob, vector_json) = row;
        let Some(stored) = decode_stored_vector(vector_blob.as_deref(), vector_json.as_deref())
        else {
            continue;
        };
        if stored.len() != embedding.len() {
            continue;
        }
        let sim = cosine_similarity(embedding, &stored);
        if sim >= 0.0 {
            candidates.push((id, source, metadata_json, sim));
        }
    }

    candidates.sort_by(|left, right| {
        right
            .3
            .partial_cmp(&left.3)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for (id, source, metadata_json, _) in candidates.into_iter().take(10) {
        add_commercial_neighbor_source(
            conn,
            &id,
            source.or_else(|| metadata_source(metadata_json.as_deref())),
            &mut seen,
            &mut sources,
        );
    }

    sources
}

fn add_commercial_neighbor_source(
    conn: &Connection,
    note_id: &str,
    candidate_source: Option<String>,
    seen: &mut HashSet<String>,
    sources: &mut Vec<Option<String>>,
) {
    let note_id = note_id.trim();
    if note_id.is_empty() || !seen.insert(note_id.to_string()) {
        return;
    }
    sources.push(commercial_detector_note_source(conn, note_id).or(candidate_source));
}

fn commercial_detector_note_source(conn: &Connection, note_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT source, metadata_json FROM notes WHERE id = ?1",
        rusqlite::params![note_id],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        },
    )
    .ok()
    .and_then(|(source, metadata_json)| {
        source.or_else(|| metadata_source(metadata_json.as_deref()))
    })
    .map(|source| source.trim().to_string())
    .filter(|source| !source.is_empty())
}

fn metadata_source(metadata_json: Option<&str>) -> Option<String> {
    let raw = metadata_json?.trim();
    if raw.is_empty() {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .get("source")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
}

fn decode_stored_vector(blob: Option<&[u8]>, json: Option<&str>) -> Option<Vec<f32>> {
    if let Some(blob) = blob {
        if blob.len() >= 4 && blob.len() % 4 == 0 {
            return Some(
                blob.chunks_exact(4)
                    .filter_map(|chunk| chunk.try_into().ok().map(f32::from_le_bytes))
                    .collect(),
            );
        }
        return None;
    }
    json.and_then(|raw| serde_json::from_str(raw).ok())
}

fn is_auto_classified_layer_allowed(layer: &str) -> bool {
    matches!(layer, "verified_fact" | "event_log")
}

fn cold_storage_metadata(
    params: &AddMemoryParams,
    layer: &str,
    category: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut metadata = serde_json::Map::new();
    if let Some(category) = category {
        metadata.insert("category".to_string(), serde_json::Value::from(category));
    }
    if let Some(confidence) = params.confidence {
        metadata.insert(
            "confidence".to_string(),
            serde_json::Value::from(confidence),
        );
    }
    if let Some(ref source) = params.source {
        metadata.insert(
            "source".to_string(),
            serde_json::Value::from(source.as_str()),
        );
    }
    if let Some(ref created_by) = params.created_by {
        metadata.insert(
            "created_by".to_string(),
            serde_json::Value::from(created_by.as_str()),
        );
    }
    if layer != "event_log"
        && let Some(ref session_id) = params.session_id
    {
        metadata.insert(
            "session_id".to_string(),
            serde_json::Value::from(session_id.as_str()),
        );
    }
    if let Some(difficulty) = params.difficulty {
        metadata.insert(
            "difficulty".to_string(),
            serde_json::Value::from(difficulty),
        );
    }
    if let Some(ref time_cost_hint) = params.time_cost_hint {
        metadata.insert(
            "time_cost_hint".to_string(),
            serde_json::Value::from(time_cost_hint.as_str()),
        );
    }
    if let Some(ref agent) = params.agent {
        metadata.insert("agent".to_string(), serde_json::Value::from(agent.as_str()));
    }
    metadata
}

/// Issue #118: parse `event_when` ISO 8601 → epoch seconds.
///
/// Mirrors Python `backend/services/storage_rows.py::to_unix_timestamp`:
///   1. RFC3339 with offset (e.g. `2026-04-15T00:00:00Z`, `...+08:00`)
///   2. Naive ISO 8601 without offset (e.g. `2026-04-15T00:00:00`) —
///      assume UTC, matching Python's `parsed.replace(tzinfo=timezone.utc)`
///      branch (PR #215 Codex P2 finding).
///
/// Returns None when input is None, empty, or doesn't parse under either
/// shape. Distinct from `valid_at_epoch_seconds` which falls back to
/// created_at instead of returning None.
fn event_when_epoch_seconds(event_when: Option<&str>) -> Option<f64> {
    let value = event_when?;
    if value.is_empty() {
        return None;
    }

    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(timestamp_seconds(&dt.with_timezone(&Utc)));
    }

    // Fallback: tz-less ISO 8601. Python's `datetime.fromisoformat` accepts
    // both `T` and ` ` separators and either fractional seconds or none, so
    // try the common shapes Python writes to match Rust↔Python parity.
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, fmt) {
            let utc = naive.and_utc();
            return Some(timestamp_seconds(&utc));
        }
    }

    None
}

fn compute_review_after(
    layer: &str,
    category: Option<&str>,
    who: Option<&[String]>,
    content: &str,
    created_at: &DateTime<Utc>,
) -> Option<String> {
    if layer == "event_log" {
        return None;
    }
    let has_who = who.is_some_and(|items| !items.is_empty());
    let mutable = category.is_some_and(|cat| MUTABLE_CATEGORIES.contains(&cat))
        || has_who
        || MUTABLE_CONTENT_MARKERS
            .iter()
            .any(|marker| content.contains(marker));
    if mutable {
        Some((*created_at + Duration::days(REVIEW_AFTER_DAYS)).to_rfc3339())
    } else {
        None
    }
}

fn checkpoint_searchable_content(
    task_id: &str,
    summary: &str,
    metadata: &serde_json::Map<String, serde_json::Value>,
) -> String {
    let mut parts = vec![format!("[断点] {task_id}: {summary}")];

    if let Some(blocker) = metadata.get("blocker").and_then(serde_json::Value::as_str) {
        parts.push(format!("阻塞: {blocker}"));
    }
    if let Some(next_step) = metadata
        .get("next_step")
        .and_then(serde_json::Value::as_str)
    {
        parts.push(format!("下一步: {next_step}"));
    }

    let Some(sections) = metadata
        .get("compact_sections")
        .and_then(serde_json::Value::as_object)
    else {
        return parts.join(" | ");
    };

    if let Some(spec) = sections
        .get("task_specification")
        .and_then(serde_json::Value::as_str)
    {
        parts.push(format!("任务说明: {spec}"));
    }
    for (key, label) in [
        ("files_and_functions", "文件/函数"),
        ("workflow", "工作流"),
        ("errors_and_corrections", "错误/修正"),
        ("decisions", "关键决策"),
        ("living_docs", "活文档"),
    ] {
        let Some(items) = sections.get(key).and_then(serde_json::Value::as_array) else {
            continue;
        };
        let preview: Vec<&str> = items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .take(4)
            .collect();
        if !preview.is_empty() {
            parts.push(format!("{label}: {}", preview.join("、")));
        }
    }

    parts.join(" | ")
}

#[derive(Debug, Default)]
struct DedupResult {
    duplicate: bool,
    best_match: Option<DedupMatch>,
    best_sim: f64,
}

#[derive(Debug)]
struct DedupMatch {
    id: String,
    #[allow(dead_code)]
    content: String,
    #[allow(dead_code)]
    similarity: f64,
}

/// Read `topic_key` of an existing note. Returns None when row missing or
/// topic_key is NULL.
pub(crate) fn read_note_topic_key(conn: &Connection, note_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT topic_key FROM notes WHERE id = ?1",
        rusqlite::params![note_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

/// Build a topic_key from content (first 100 UTF-8 boundary-snapped bytes).
fn build_topic_key_from_content(layer: &str, content: &str) -> String {
    let key_content = safe_text_prefix(content, 100);
    format!("{}:{}", layer, key_content.replace('\n', " "))
}

/// Issue #112: pick topic_key for a new note. Mirrors Python
/// write_orchestrator.py:1976-1985. Order: supersede → best_match≥0.78 → fresh.
pub(crate) fn decide_topic_key(
    conn: &Connection,
    layer: &str,
    content: &str,
    is_event_layer: bool,
    superseded_ids: &[String],
    best_match_id: Option<&str>,
    best_sim: f64,
) -> Option<String> {
    if is_event_layer {
        return None;
    }
    if let Some(first) = superseded_ids.first() {
        if let Some(tk) = read_note_topic_key(conn, first) {
            return Some(tk);
        }
    }
    if let Some(id) = best_match_id {
        if best_sim >= TOPIC_INHERIT_THRESHOLD {
            if let Some(tk) = read_note_topic_key(conn, id) {
                return Some(tk);
            }
        }
    }
    Some(build_topic_key_from_content(layer, content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    struct TopicKeyFixture {
        layer: String,
        content: String,
        expected_topic_key: String,
    }

    /// Helper: simulate the topic_key computation exactly as written in add_memory_inner.
    fn compute_topic_key(layer: &str, content: &str) -> Option<String> {
        let is_event_layer = layer == "event_log";
        if !is_event_layer {
            let key_content = safe_text_prefix(content, 100);
            Some(format!("{}:{}", layer, key_content.replace('\n', " ")))
        } else {
            None
        }
    }

    #[test]
    fn gate_f_valid_at_uses_event_when_or_created_at_epoch_seconds() {
        let created_at = chrono::DateTime::parse_from_rfc3339("2026-04-19T12:00:00Z")
            .expect("created_at")
            .with_timezone(&chrono::Utc);

        let event_valid_at = valid_at_epoch_seconds(Some("2026-04-15T00:00:00Z"), &created_at)
            .expect("event_when should parse");
        let created_valid_at = valid_at_epoch_seconds(None, &created_at)
            .expect("created_at fallback should always exist");

        assert_eq!(event_valid_at, 1_776_211_200.0);
        assert_eq!(created_valid_at, 1_776_600_000.0);
    }

    /// Issue #118 PR #215 Codex P2-1: tz-less ISO must be accepted as
    /// UTC, matching Python `to_unix_timestamp`. Pre-fix returned None
    /// for these shapes, dropping `event_when_ts` to NULL even when
    /// `event_when` was stored.
    #[test]
    fn event_when_epoch_seconds_accepts_timezone_less_iso_as_utc() {
        // Same instant expressed 4 ways. All must yield the same epoch.
        let expected = 1_776_211_200.0; // 2026-04-15T00:00:00Z
        for variant in [
            "2026-04-15T00:00:00Z",
            "2026-04-15T00:00:00+00:00",
            "2026-04-15T00:00:00", // tz-less T-separator
            "2026-04-15 00:00:00", // tz-less space-separator
        ] {
            let got = event_when_epoch_seconds(Some(variant))
                .unwrap_or_else(|| panic!("Issue #118 P2-1: variant {variant:?} must parse"));
            assert!(
                (got - expected).abs() < 1e-3,
                "Issue #118 P2-1: variant {variant} produced {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn event_when_epoch_seconds_accepts_fractional_seconds_tz_less() {
        let got = event_when_epoch_seconds(Some("2026-04-15T00:00:00.500"))
            .expect("fractional tz-less must parse");
        assert!((got - 1_776_211_200.5).abs() < 1e-3);
    }

    #[test]
    fn event_when_epoch_seconds_returns_none_for_garbage() {
        assert_eq!(event_when_epoch_seconds(Some("not-a-date")), None);
        assert_eq!(event_when_epoch_seconds(Some("")), None);
        assert_eq!(event_when_epoch_seconds(None), None);
    }

    #[test]
    fn gate_f_review_after_matches_python_mutable_fact_rules() {
        let created_at = chrono::DateTime::parse_from_rfc3339("2026-04-19T12:00:00Z")
            .expect("created_at")
            .with_timezone(&chrono::Utc);
        let who = vec!["alice".to_string()];

        assert_eq!(
            compute_review_after(
                "verified_fact",
                Some("person"),
                None,
                "stable profile",
                &created_at
            )
            .as_deref(),
            Some("2026-05-19T12:00:00+00:00")
        );
        assert_eq!(
            compute_review_after(
                "verified_fact",
                Some("research"),
                Some(&who),
                "team note",
                &created_at
            )
            .as_deref(),
            Some("2026-05-19T12:00:00+00:00")
        );
        assert_eq!(
            compute_review_after(
                "verified_fact",
                Some("research"),
                None,
                "当前团队变化",
                &created_at
            )
            .as_deref(),
            Some("2026-05-19T12:00:00+00:00")
        );
        assert!(
            compute_review_after(
                "event_log",
                Some("event"),
                Some(&who),
                "event rows skip review_after",
                &created_at
            )
            .is_none(),
            "event_log rows must not receive mutable-fact review_after"
        );
    }

    /// 34 Chinese chars = 102 bytes; used to trigger the original byte-slice panic.
    #[test]
    fn topic_key_handles_cjk_boundary() {
        // "记忆锚点是AI的外挂海马体" = 13 chars = 39 bytes; repeat 3 → 117 bytes > 100
        let long_cjk = "记忆锚点是AI的外挂海马体".repeat(3);
        assert!(
            long_cjk.len() > 100,
            "precondition: input exceeds 100 bytes"
        );

        let result = compute_topic_key("verified_fact", &long_cjk);
        let key = result.expect("topic_key must be Some for non-event layer");

        // Must be valid UTF-8 (String is always valid UTF-8 in Rust)
        assert!(
            key.starts_with("verified_fact:"),
            "key must start with layer prefix"
        );

        // The content portion must itself be valid UTF-8 — verify by checking char boundaries
        let content_part = key.trim_start_matches("verified_fact:");
        assert!(
            std::str::from_utf8(content_part.as_bytes()).is_ok(),
            "topic_key content portion must be valid UTF-8"
        );
        // Content portion must be at most 100 bytes
        assert!(
            content_part.len() <= 100,
            "content portion must be at most 100 bytes, got {}",
            content_part.len()
        );
    }

    #[test]
    fn topic_key_matches_phase0_python_fixtures() {
        let long_ascii = "A".repeat(200);
        let expected_long_ascii = format!("verified_fact:{}", "A".repeat(100));
        let cases = [
            (
                "verified_fact",
                "alpha\nbeta".to_string(),
                "verified_fact:alpha beta".to_string(),
            ),
            (
                "verified_fact",
                "short content".to_string(),
                "verified_fact:short content".to_string(),
            ),
            ("verified_fact", long_ascii, expected_long_ascii),
        ];

        for (layer, content, expected) in cases {
            assert_eq!(
                compute_topic_key(layer, &content).as_deref(),
                Some(expected.as_str())
            );
        }
    }

    #[test]
    fn topic_key_matches_shared_fixture_json() {
        let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/topic_key_parity.json");
        let raw =
            std::fs::read_to_string(&fixture_path).expect("shared topic_key fixture must load");
        let fixtures: Vec<TopicKeyFixture> =
            serde_json::from_str(&raw).expect("shared topic_key fixture must parse");

        assert_eq!(fixtures.len(), 100);
        for item in fixtures {
            assert_eq!(
                compute_topic_key(&item.layer, &item.content).as_deref(),
                Some(item.expected_topic_key.as_str())
            );
        }
    }

    /// Property test: 100 deterministic mixed-script strings, none panic, all valid UTF-8.
    #[test]
    fn topic_key_property_no_panic() {
        // Seeded pseudo-random via LCG (no external dep)
        let mut state: u64 = 0xDEAD_BEEF_CAFE_1337;
        let lcg_next = |s: &mut u64| -> u64 {
            *s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *s
        };

        let scripts: &[&str] = &[
            "hello world ",
            "记忆锚点",
            "αβγδε",
            "🎉🦀🧠",
            "\u{200B}\u{FEFF}", // zero-width chars
            "日本語テスト",
            "한국어테스트",
            "العربية",
            "Ünïcödé",
            "mixed混合🔥test",
        ];

        for i in 0..100u64 {
            // Pick length 0..500
            let len_seed = lcg_next(&mut state);
            let target_len = (len_seed % 500) as usize;

            // Build string by concatenating script chunks until we reach target_len bytes
            let mut content = String::new();
            let script_seed = lcg_next(&mut state);
            let chunk = scripts[(script_seed as usize) % scripts.len()];
            while content.len() < target_len {
                content.push_str(chunk);
            }
            // Truncate to exactly target_len bytes safely
            let content = safe_text_prefix(&content, target_len).to_string();

            // This must not panic
            let result = compute_topic_key("verified_fact", &content);
            let key = result.expect("must return Some for non-event layer");

            // Verify valid UTF-8 (implicit in Rust String, but be explicit)
            assert!(
                std::str::from_utf8(key.as_bytes()).is_ok(),
                "iteration {i}: topic_key must be valid UTF-8"
            );

            // Content portion must be ≤100 bytes
            let content_part = key.trim_start_matches("verified_fact:");
            assert!(
                content_part.len() <= 100,
                "iteration {i}: content portion must be ≤100 bytes, got {}",
                content_part.len()
            );
        }

        // Event layer must return None regardless of content
        let result = compute_topic_key("event_log", "任意内容");
        assert!(result.is_none(), "event_log must return None topic_key");
    }

    /// Exactly 100 bytes of ASCII input: prefix must equal the input exactly.
    #[test]
    fn topic_key_exact_100_ascii() {
        let input: String = "a".repeat(100);
        assert_eq!(input.len(), 100, "precondition: exactly 100 bytes");

        let key =
            compute_topic_key("verified_fact", &input).expect("must be Some for non-event layer");

        let expected = format!("verified_fact:{input}");
        assert_eq!(key, expected, "100-byte ASCII input must be kept intact");
    }

    // ---------------------------------------------------------------
    // Issue #112: topic_key inheritance (parity with Python
    // backend/core/write_orchestrator.py:1976-1985).
    // ---------------------------------------------------------------

    fn temp_db_with_minimal_notes_table() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE notes (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                layer TEXT,
                topic_key TEXT,
                is_active INTEGER NOT NULL DEFAULT 1
            );",
        )
        .expect("schema");
        conn
    }

    fn seed_note(conn: &Connection, id: &str, layer: &str, topic_key: Option<&str>) {
        conn.execute(
            "INSERT INTO notes (id, content, layer, topic_key, is_active) VALUES (?1, ?2, ?3, ?4, 1)",
            rusqlite::params![id, "seed", layer, topic_key],
        )
        .expect("seed insert");
    }

    #[test]
    fn read_note_topic_key_returns_existing_value() {
        let conn = temp_db_with_minimal_notes_table();
        seed_note(&conn, "n1", "verified_fact", Some("verified_fact:custom"));
        assert_eq!(
            read_note_topic_key(&conn, "n1"),
            Some("verified_fact:custom".to_string())
        );
    }

    #[test]
    fn read_note_topic_key_returns_none_for_missing_or_null() {
        let conn = temp_db_with_minimal_notes_table();
        seed_note(&conn, "n1", "verified_fact", None);
        assert_eq!(read_note_topic_key(&conn, "n1"), None);
        assert_eq!(read_note_topic_key(&conn, "missing"), None);
    }

    #[test]
    fn decide_topic_key_inherits_from_superseded_note() {
        let conn = temp_db_with_minimal_notes_table();
        seed_note(
            &conn,
            "old-note",
            "verified_fact",
            Some("verified_fact:Architecture parity sample"),
        );

        let result = decide_topic_key(
            &conn,
            "verified_fact",
            "totally different content of the new note",
            false,
            &["old-note".to_string()],
            None,
            0.0,
        );

        assert_eq!(
            result.as_deref(),
            Some("verified_fact:Architecture parity sample"),
            "supersede must inherit topic_key from old note (issue #112)"
        );
    }

    #[test]
    fn decide_topic_key_inherits_from_best_match_when_sim_above_threshold() {
        let conn = temp_db_with_minimal_notes_table();
        seed_note(
            &conn,
            "best-1",
            "verified_fact",
            Some("verified_fact:Person parity sample"),
        );

        let result = decide_topic_key(
            &conn,
            "verified_fact",
            "fresh content",
            false,
            &[],
            Some("best-1"),
            0.80,
        );

        assert_eq!(
            result.as_deref(),
            Some("verified_fact:Person parity sample")
        );
    }

    #[test]
    fn decide_topic_key_skips_best_match_when_sim_below_threshold() {
        let conn = temp_db_with_minimal_notes_table();
        seed_note(
            &conn,
            "best-1",
            "verified_fact",
            Some("verified_fact:Should not inherit"),
        );

        let result = decide_topic_key(
            &conn,
            "verified_fact",
            "fresh independent content",
            false,
            &[],
            Some("best-1"),
            0.77,
        );

        assert_eq!(
            result.as_deref(),
            Some("verified_fact:fresh independent content"),
            "below TOPIC_INHERIT_THRESHOLD must build fresh"
        );
    }

    #[test]
    fn decide_topic_key_builds_fresh_when_no_signals() {
        let conn = temp_db_with_minimal_notes_table();
        let result = decide_topic_key(
            &conn,
            "procedure_schema",
            "step list content",
            false,
            &[],
            None,
            0.0,
        );

        assert_eq!(
            result.as_deref(),
            Some("procedure_schema:step list content")
        );
    }

    #[test]
    fn decide_topic_key_returns_none_for_event_layer() {
        let conn = temp_db_with_minimal_notes_table();
        seed_note(
            &conn,
            "old-note",
            "event_log",
            Some("verified_fact:should be ignored"),
        );

        let result = decide_topic_key(
            &conn,
            "event_log",
            "any event content",
            true,
            &["old-note".to_string()],
            None,
            0.0,
        );

        assert!(
            result.is_none(),
            "event_log must always return None topic_key"
        );
    }

    #[test]
    fn decide_topic_key_supersede_takes_priority_over_best_match() {
        let conn = temp_db_with_minimal_notes_table();
        seed_note(
            &conn,
            "supersede-target",
            "verified_fact",
            Some("verified_fact:from supersede"),
        );
        seed_note(
            &conn,
            "best-match-target",
            "verified_fact",
            Some("verified_fact:from best match"),
        );

        let result = decide_topic_key(
            &conn,
            "verified_fact",
            "new content",
            false,
            &["supersede-target".to_string()],
            Some("best-match-target"),
            0.95,
        );

        assert_eq!(
            result.as_deref(),
            Some("verified_fact:from supersede"),
            "supersede precedence > best_match precedence"
        );
    }

    #[test]
    fn exact_dup_compat_missing_query_vector_against_present_row_is_compatible() {
        // Regression guard for Bot P1 on PR #138 HEAD 3e7cc00:
        // During embedding outages (e.g. Ollama offline) the query
        // has no vector. If we skip duplicate protection in that case,
        // identical content is silently re-written. Treat any present
        // stored vector as compatible when the query side is missing.
        let stored_blob = vec![0u8; 1024 * 4]; // 1024 f32
        assert!(exact_duplicate_vector_compatible(
            Some(stored_blob.as_slice()),
            None,
            None,
        ));
    }

    #[test]
    fn exact_dup_compat_missing_query_and_missing_store_is_compatible() {
        assert!(exact_duplicate_vector_compatible(None, None, None));
    }

    #[test]
    fn exact_dup_compat_missing_query_and_invalid_store_is_not_compatible() {
        // Garbage blob length must not false-match.
        let bad = vec![0u8; 3];
        assert!(!exact_duplicate_vector_compatible(
            Some(bad.as_slice()),
            None,
            None,
        ));
    }

    #[test]
    fn exact_dup_compat_dim_mismatch_still_rejected() {
        // With a real query vector, dimension mismatch must still reject.
        let stored_blob = vec![0u8; 768 * 4];
        assert!(!exact_duplicate_vector_compatible(
            Some(stored_blob.as_slice()),
            None,
            Some(1024),
        ));
    }
}
