//! Write operations for the notes table.
//!
//! All writes use `BEGIN IMMEDIATE` transactions for serialization.
//! Ported from `backend/services/storage_rows.py` upsert_note().

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value as JsonValue;
use tracing::warn;

use crate::personal::allows_strengthening;
use crate::storage::sessions_writer::upsert_session;

const HEBBIAN_CONFIRM_DELTA: f64 = 0.05;
const HEBBIAN_CORRECT_DECAY: f64 = 0.85;
const HEBBIAN_OUTDATED_DECAY: f64 = 0.70;
const DREAM_CONFIRM_DELTA: f64 = 0.05;
const DREAM_CORRECT_DECAY: f64 = 0.75;
const DREAM_OUTDATED_DECAY: f64 = 0.50;

/// Parameters for inserting/upserting a note.
#[derive(Debug, Clone, Default)]
pub struct NoteInsert {
    pub id: String,
    pub content: String,
    pub layer: String,
    pub category: Option<String>,
    pub is_active: bool,
    pub confidence: Option<f64>,
    pub agent_id: Option<String>,
    pub project_id: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub valid_at: Option<f64>,
    pub session_id: Option<String>,
    pub review_after: Option<String>,
    pub metadata_json: Option<String>,
    pub vector_json: Option<String>,
    pub vector_blob: Option<Vec<u8>>,
    pub evolution_state: Option<String>,
    pub topic_key: Option<String>,
    pub is_head: bool,
    pub room: Option<String>,
    pub agent: Option<String>,
    pub difficulty: Option<i64>,
    pub time_cost_hint: Option<String>,
    pub related_ids_json: Option<String>,
    pub role: Option<String>,
    pub version: Option<i64>,
    pub root_id: Option<String>,
    /// Origin of this write: "user" for MCP writes, other values for
    /// programmatic writers (autoresearch, consolidation, etc.). Mirrors
    /// Python storage_rows.py `source` column; default in MCP path is "user".
    pub source: Option<String>,
    /// Identity that initiated this write. Python defaults to `source` when
    /// not supplied explicitly; Rust mirrors the pattern at the caller site.
    pub created_by: Option<String>,
    /// Issue #118: event_log time columns. Python persists both the raw
    /// RFC3339 string (`event_when`) and its derived epoch seconds
    /// (`event_when_ts`) for query/sort efficiency. Non-event writes leave
    /// both NULL.
    pub event_when: Option<String>,
    pub event_when_ts: Option<f64>,
}

/// Insert a note with UPSERT semantics (ON CONFLICT DO UPDATE).
///
/// Also maintains the FTS5 index (DELETE + INSERT into notes_fts).
/// Runs within the caller's transaction context.
pub fn upsert_note(conn: &Connection, note: &NoteInsert) -> rusqlite::Result<()> {
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let created_at = note.created_at.as_deref().unwrap_or(&now);
    // Mirror Python storage_rows.py semantics: when the caller does not pass
    // an explicit updated_at, leave it NULL on first insert. The UPSERT
    // branch below uses an IS-NOT-NULL-guarded CASE so a subsequent
    // material-change write fills in `now()` instead of writing back the
    // NULL or the caller's stale supplied value (PR #86 review).
    let updated_at = note.updated_at.as_deref();
    let is_active: i64 = if note.is_active { 1 } else { 0 };
    let is_head: i64 = if note.is_head { 1 } else { 0 };
    let evolution_state = note.evolution_state.as_deref().unwrap_or("active");

    conn.execute(
        // strftime() OK here: this only generates a fresh UTC write timestamp;
        // it does not parse, filter, or order existing timestamp text.
        "INSERT INTO notes (
            id, content, layer, category, is_active, confidence,
            agent_id, project_id, created_at, updated_at,
            valid_at, session_id,
            metadata_json, vector_json, vector_blob,
            evolution_state, topic_key, is_head, review_after, room,
            agent, difficulty, time_cost_hint, related_ids_json, role,
            version, root_id, source, created_by,
            event_when, event_when_ts
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            ?7, ?8, ?9, ?10,
            ?11, ?12,
            ?13, ?14, ?15,
            ?16, ?17, ?18, ?19, ?20,
            ?21, ?22, ?23, ?24, ?25,
            COALESCE(?26, 1), COALESCE(?27, ?1), ?28, ?29,
            ?30, ?31
        )
        ON CONFLICT(id) DO UPDATE SET
            content = excluded.content,
            layer = excluded.layer,
            category = excluded.category,
            is_active = excluded.is_active,
            confidence = excluded.confidence,
            agent_id = excluded.agent_id,
            project_id = COALESCE(excluded.project_id, notes.project_id),
            updated_at = CASE
                WHEN notes.content IS NOT excluded.content
                  OR notes.layer IS NOT excluded.layer
                  OR notes.category IS NOT excluded.category
                  OR notes.confidence IS NOT excluded.confidence
                  OR notes.is_active IS NOT excluded.is_active
                  OR notes.agent_id IS NOT excluded.agent_id
                  OR notes.evolution_state IS NOT excluded.evolution_state
                  OR notes.version IS NOT COALESCE(?32, notes.version)
                  OR notes.root_id IS NOT COALESCE(?33, notes.root_id)
                  OR notes.topic_key IS NOT excluded.topic_key
                  OR notes.is_head IS NOT excluded.is_head
                  OR (excluded.project_id IS NOT NULL AND notes.project_id IS NOT excluded.project_id)
                  OR (excluded.agent IS NOT NULL AND notes.agent IS NOT excluded.agent)
                  OR (excluded.difficulty IS NOT NULL AND notes.difficulty IS NOT excluded.difficulty)
                  OR (excluded.time_cost_hint IS NOT NULL AND notes.time_cost_hint IS NOT excluded.time_cost_hint)
                  OR (excluded.related_ids_json IS NOT NULL AND notes.related_ids_json IS NOT excluded.related_ids_json)
                  OR (excluded.role IS NOT NULL AND notes.role IS NOT excluded.role)
                -- Material change detected. Mirror Python's stale-timestamp
                -- guard while preserving Rust's NULL-on-omission insert path:
                --   if caller supplied a different updated_at → use it
                --   else (omitted NULL or re-used current value) → stamp now()
                THEN CASE
                    WHEN excluded.updated_at IS NOT NULL
                      AND excluded.updated_at IS NOT notes.updated_at
                    THEN excluded.updated_at
                    ELSE strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                END
                ELSE notes.updated_at
            END,
            valid_at = COALESCE(excluded.valid_at, notes.valid_at),
            session_id = COALESCE(excluded.session_id, notes.session_id),
            metadata_json = excluded.metadata_json,
            vector_json = excluded.vector_json,
            vector_blob = excluded.vector_blob,
            evolution_state = CASE
                WHEN excluded.evolution_state != 'active' THEN excluded.evolution_state
                ELSE COALESCE(notes.evolution_state, 'active')
            END,
            topic_key = COALESCE(excluded.topic_key, notes.topic_key),
            is_head = COALESCE(excluded.is_head, notes.is_head, 1),
            review_after = COALESCE(excluded.review_after, notes.review_after),
            room = COALESCE(excluded.room, notes.room),
            agent = COALESCE(excluded.agent, notes.agent),
            difficulty = COALESCE(excluded.difficulty, notes.difficulty),
            time_cost_hint = COALESCE(excluded.time_cost_hint, notes.time_cost_hint),
            related_ids_json = COALESCE(excluded.related_ids_json, notes.related_ids_json),
            role = COALESCE(excluded.role, notes.role),
            source = COALESCE(excluded.source, notes.source),
            created_by = COALESCE(excluded.created_by, notes.created_by),
            -- Issue #118 PR #215 Codex P2-2: direct overwrite, not
            -- COALESCE. Python `storage_rows.py` writes
            -- `event_when = excluded.event_when` so a replay/update path
            -- that explicitly omits these columns clears stale event
            -- timing metadata instead of welding it onto the row forever.
            event_when = excluded.event_when,
            event_when_ts = excluded.event_when_ts,
            version = COALESCE(?32, notes.version),
            root_id = COALESCE(?33, notes.root_id)",
        rusqlite::params![
            note.id,
            note.content,
            note.layer,
            note.category,
            is_active,
            note.confidence,
            note.agent_id,
            note.project_id,
            created_at,
            updated_at,
            note.valid_at,
            note.session_id,
            note.metadata_json,
            note.vector_json,
            note.vector_blob,
            evolution_state,
            note.topic_key,
            is_head,
            note.review_after,
            note.room,
            note.agent,
            note.difficulty,
            note.time_cost_hint,
            note.related_ids_json,
            note.role,
            note.version,
            note.root_id,
            note.source,
            note.created_by,
            note.event_when,
            note.event_when_ts,
            note.version,
            note.root_id,
        ],
    )?;

    // Maintain FTS5 index
    conn.execute("DELETE FROM notes_fts WHERE note_id = ?1", [&note.id])?;
    conn.execute(
        "INSERT INTO notes_fts (note_id, content) VALUES (?1, ?2)",
        rusqlite::params![note.id, note.content],
    )?;

    // Sessions UPSERT: track session attribution inside the same transaction.
    // Fast path: skip when no session_id is present (stdio writes without HTTP context).
    //
    // P2 fix (PR #230 Codex): pass `note.agent` not `note.agent_id`. The
    // add-memory path through `write_orchestrator.rs` populates `agent` and
    // leaves `agent_id` as None, so using `agent_id` would write NULL into
    // every `sessions.agent_label` row and break observability.
    //
    // P1 fix (PR #232 Codex): pass `note.project_id` so `get_context`
    // active_sessions can scope to the requesting project. Multi-project
    // DBs without this leak session_ids across project boundaries.
    if let Some(ref sid) = note.session_id {
        upsert_session(
            conn,
            sid,
            note.agent.as_deref(),
            None,
            note.project_id.as_deref(),
            &now,
        )?;
    }

    Ok(())
}

/// Deactivate all checkpoints with the given task_id (UPSERT phase 1).
pub fn deactivate_checkpoints_by_task_id(
    conn: &Connection,
    task_id: &str,
    exclude_id: Option<&str>,
) -> rusqlite::Result<usize> {
    let mut sql = String::from(
        "UPDATE notes SET is_active = 0
         WHERE layer = 'event_log'
           AND is_active = 1
           AND json_valid(metadata_json)
           AND json_extract(metadata_json, '$.record_type') = 'checkpoint'
           AND json_extract(metadata_json, '$.task_id') = ?1",
    );
    if exclude_id.is_some() {
        sql.push_str(" AND id != ?2");
    }
    let affected = match exclude_id {
        Some(exclude_id) => conn.execute(&sql, (task_id, exclude_id))?,
        None => conn.execute(&sql, [task_id])?,
    };
    Ok(affected)
}

/// Atomically update a memory's outcome (confirmed/corrected/outdated).
///
/// Reads current metadata + confidence, applies the outcome delta,
/// writes back in a single statement.
pub fn atomic_update_outcome(
    conn: &Connection,
    note_id: &str,
    outcome: &str,
    reason: Option<&str>,
) -> rusqlite::Result<bool> {
    // Read current state
    let (metadata_str, confidence): (Option<String>, Option<f64>) = conn.query_row(
        "SELECT metadata_json, confidence FROM notes WHERE id = ?1 AND is_active = 1",
        [note_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let mut meta: serde_json::Map<String, JsonValue> = metadata_str
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    let conf = confidence.unwrap_or(0.9);
    let mut success_count = meta
        .get("success_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let mut failure_count = meta
        .get("failure_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    let new_conf = match outcome {
        "confirmed" => {
            success_count += 1;
            (conf + 0.01).min(0.99)
        }
        "corrected" => {
            failure_count += 1;
            meta.insert("is_corrected".to_string(), JsonValue::Bool(true));
            if let Some(r) = reason {
                meta.insert("last_outcome_reason".to_string(), JsonValue::from(r));
            }
            (conf - 0.05).max(0.1)
        }
        "outdated" => {
            failure_count += 1;
            meta.insert("is_outdated".to_string(), JsonValue::Bool(true));
            if let Some(r) = reason {
                meta.insert("last_outcome_reason".to_string(), JsonValue::from(r));
            }
            (conf - 0.10).max(0.1)
        }
        _ => {
            warn!("Unknown outcome '{outcome}', ignoring");
            return Ok(false);
        }
    };
    meta.insert("success_count".to_string(), JsonValue::from(success_count));
    meta.insert("failure_count".to_string(), JsonValue::from(failure_count));

    let new_meta_str = match serde_json::to_string(&meta) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("metadata serialization failed for note {note_id}: {e}");
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "metadata serialize: {e}"
            )));
        }
    };

    conn.execute(
        "UPDATE notes SET
            metadata_json = ?1,
            confidence = ?2
         WHERE id = ?3",
        rusqlite::params![new_meta_str, new_conf, note_id],
    )?;

    Ok(true)
}

/// Apply the smallest Rust-side Hebbian feedback loop after `report_outcome`.
///
/// `report_outcome` is the user/agent signal that a recalled memory helped,
/// misled, or went stale. Rust retrieval now follows `note_relations`; this
/// feedback keeps those neural edges from being static:
/// - confirmed: strengthen adjacent activation edges a little, capped at 1.0
/// - corrected/outdated: decay adjacent activation edges without deleting them
///
/// The update is intentionally local to edges touching the reported note. It
/// does not infer which multi-hop path was used because current MCP calls do
/// not yet persist per-query activation traces.
pub fn apply_hebbian_feedback(
    conn: &Connection,
    note_id: &str,
    outcome: &str,
) -> rusqlite::Result<usize> {
    if outcome == "confirmed" && !is_trusted_for_strengthening(conn, note_id)? {
        return Ok(0);
    }

    let sql = match outcome {
        "confirmed" => {
            "UPDATE note_relations
             SET strength = min(1.0, strength + ?1)
             WHERE (from_note_id = ?2 OR to_note_id = ?2)
               AND relation_type IN ('supports', 'refines', 'overlaps', 'co_activated')"
        }
        "corrected" => {
            "UPDATE note_relations
             SET strength = max(0.0, strength * ?1)
             WHERE (from_note_id = ?2 OR to_note_id = ?2)
               AND relation_type IN ('supports', 'refines', 'overlaps', 'co_activated')"
        }
        "outdated" => {
            "UPDATE note_relations
             SET strength = max(0.0, strength * ?1)
             WHERE (from_note_id = ?2 OR to_note_id = ?2)
               AND relation_type IN ('supports', 'refines', 'overlaps', 'co_activated')"
        }
        _ => return Ok(0),
    };
    let factor = match outcome {
        "confirmed" => HEBBIAN_CONFIRM_DELTA,
        "corrected" => HEBBIAN_CORRECT_DECAY,
        "outdated" => HEBBIAN_OUTDATED_DECAY,
        _ => unreachable!(),
    };
    let before_edges = hebbian_feedback_edges(conn, note_id)?;
    let affected = conn.execute(sql, rusqlite::params![factor, note_id])?;
    for edge in before_edges {
        let after_strength = match outcome {
            "confirmed" => (edge.strength + HEBBIAN_CONFIRM_DELTA).min(1.0),
            "corrected" => (edge.strength * HEBBIAN_CORRECT_DECAY).max(0.0),
            "outdated" => (edge.strength * HEBBIAN_OUTDATED_DECAY).max(0.0),
            _ => edge.strength,
        };
        let related_note_id = if edge.from_note_id == note_id {
            edge.to_note_id.as_str()
        } else {
            edge.from_note_id.as_str()
        };
        let payload = serde_json::json!({
            "outcome": outcome,
            "from_note_id": edge.from_note_id,
            "to_note_id": edge.to_note_id,
            "relation_type": edge.relation_type,
            "before_strength": edge.strength,
            "after_strength": after_strength,
        });
        insert_note_event(
            conn,
            note_id,
            "hebbian_feedback",
            Some(related_note_id),
            Some(&payload.to_string()),
        )?;
    }
    Ok(affected)
}

/// Trust Gate v1: only user/checkpoint memories, or explicitly confirmed
/// candidate memories, can strengthen long-term Hebbian edges.
pub fn is_trusted_for_strengthening(conn: &Connection, note_id: &str) -> rusqlite::Result<bool> {
    let source_expr = if sqlite_column_exists(conn, "notes", "source")? {
        "source"
    } else {
        "NULL"
    };
    let confirmed_expr = if sqlite_column_exists(conn, "notes", "confirmed_at")? {
        "confirmed_at"
    } else {
        "NULL"
    };
    let sql = format!("SELECT {source_expr}, {confirmed_expr} FROM notes WHERE id = ?1 LIMIT 1");
    conn.query_row(&sql, [note_id], |row| {
        let source: Option<String> = row.get(0)?;
        let confirmed_at: Option<i64> = row.get(1)?;
        Ok(allows_strengthening(
            source.as_deref(),
            confirmed_at.is_some(),
            true,
        ))
    })
    .optional()
    .map(|trusted| trusted.unwrap_or(false))
}

/// Apply report_outcome feedback to promoted dream candidates.
///
/// Dream activation uses promoted candidates as evidence edges. A reported
/// outcome against the promoted memory should therefore reinforce or decay the
/// candidate confidence as well as ordinary note_relations:
/// - confirmed: strengthen the candidate confidence a little, capped at 1.0
/// - corrected: decay confidence so the evidence path becomes less dominant
/// - outdated: stronger decay for stale promoted memories
///
/// The candidate row stays auditable instead of being deleted; search already
/// ignores dream evidence whose confidence falls below the activation
/// threshold.
pub fn apply_dream_feedback(
    conn: &Connection,
    note_id: &str,
    outcome: &str,
    reason: Option<&str>,
) -> rusqlite::Result<usize> {
    if !sqlite_table_exists(conn, "dream_candidates")? {
        return Ok(0);
    }
    let mut stmt = conn.prepare(
        "SELECT id, confidence, evidence_ids
         FROM dream_candidates
         WHERE promoted_to = ?1
           AND verdict = 'promoted'",
    )?;
    let rows = stmt.query_map([note_id], |row| {
        Ok(DreamFeedbackCandidate {
            id: row.get(0)?,
            confidence: row.get::<_, f64>(1).unwrap_or(0.0),
            evidence_ids: row.get::<_, Option<String>>(2).ok().flatten(),
        })
    })?;
    let candidates = rows.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut affected = 0;
    for candidate in candidates {
        let after_confidence = match outcome {
            "confirmed" => (candidate.confidence + DREAM_CONFIRM_DELTA).min(1.0),
            "corrected" => (candidate.confidence * DREAM_CORRECT_DECAY).max(0.0),
            "outdated" => (candidate.confidence * DREAM_OUTDATED_DECAY).max(0.0),
            _ => continue,
        };
        let writer_status = match outcome {
            "confirmed" => "feedback_confirmed",
            "corrected" => "feedback_corrected",
            "outdated" => "feedback_outdated",
            _ => unreachable!(),
        };
        affected += conn.execute(
            "UPDATE dream_candidates
             SET confidence = ?1,
                 writer_status = ?2,
                 writer_reason = ?3,
                 evaluated_at = COALESCE(evaluated_at, ?4)
             WHERE id = ?5",
            rusqlite::params![
                after_confidence,
                writer_status,
                reason,
                Utc::now().to_rfc3339(),
                candidate.id
            ],
        )?;
        let payload = serde_json::json!({
            "candidate_id": candidate.id,
            "outcome": outcome,
            "before_confidence": candidate.confidence,
            "after_confidence": after_confidence,
            "evidence_ids": candidate.evidence_ids,
            "reason": reason,
        });
        insert_note_event(
            conn,
            note_id,
            "dream_feedback",
            None,
            Some(&payload.to_string()),
        )?;
    }
    Ok(affected)
}

#[derive(Debug)]
struct DreamFeedbackCandidate {
    id: String,
    confidence: f64,
    evidence_ids: Option<String>,
}

#[derive(Debug)]
struct HebbianFeedbackEdge {
    from_note_id: String,
    to_note_id: String,
    relation_type: String,
    strength: f64,
}

fn hebbian_feedback_edges(
    conn: &Connection,
    note_id: &str,
) -> rusqlite::Result<Vec<HebbianFeedbackEdge>> {
    let mut stmt = conn.prepare(
        "SELECT from_note_id, to_note_id, relation_type, strength
         FROM note_relations
         WHERE (from_note_id = ?1 OR to_note_id = ?1)
           AND relation_type IN ('supports', 'refines', 'overlaps', 'co_activated')
         ORDER BY from_note_id, to_note_id, relation_type",
    )?;
    let rows = stmt.query_map([note_id], |row| {
        Ok(HebbianFeedbackEdge {
            from_note_id: row.get(0)?,
            to_note_id: row.get(1)?,
            relation_type: row.get(2)?,
            strength: row.get(3)?,
        })
    })?;
    rows.collect()
}

fn sqlite_table_exists(conn: &Connection, table_name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = ?1
        )",
        [table_name],
        |row| row.get::<_, i64>(0).map(|value| value != 0),
    )
}

fn sqlite_column_exists(
    conn: &Connection,
    table_name: &str,
    column_name: &str,
) -> rusqlite::Result<bool> {
    let sql = format!("PRAGMA table_info({table_name})");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row?.eq_ignore_ascii_case(column_name) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Mark a note as superseded by another note — full cascade matching Python
/// `memory_lifecycle.mark_transition` (path A, auto-supersede).
///
/// Column updates (single UPDATE so the transition is atomic):
/// - `evolution_state = 'superseded'` or, for logical contradiction,
///   `evolution_state = 'contradicted'`
/// - `is_head = 0`
/// - `is_active = 0`
/// - `updated_at = now()`
///
/// metadata_json updates (merged with any pre-existing keys):
/// - `superseded_by = new_id`
/// - `supersede_reason = reason` (e.g. "auto_supersede", "contradiction",
///   "evolution_refine", "cross_layer_contradiction" — match Python values)
/// - `is_outdated = true`
///
/// Mirrors:
/// - backend/core/write_orchestrator.py:1420-1445 (call site)
/// - backend/services/memory_lifecycle.py:91-147 (field contract)
pub fn mark_superseded(
    conn: &Connection,
    old_id: &str,
    new_id: &str,
    reason: &str,
) -> rusqlite::Result<()> {
    let evolution_state = if reason == "contradiction" {
        "contradicted"
    } else {
        "superseded"
    };

    // Update evolution / head / active flags. Detect "note does not exist"
    // via affected rows rather than via the later metadata SELECT, so a NULL
    // metadata_json on an existing row does not get misread as a missing row.
    let affected = conn.execute(
        // strftime() OK here: write-side timestamp bump only, no comparison
        // or ordering over stored timestamp text.
        // Confidence decay -0.1 (floor 0.1) mirrors Python
        // `storage.py::report_outcome` at line 1238 when outcome="outdated":
        // `new_confidence = max(0.1, old_confidence - 0.1)`.
        "UPDATE notes SET
            evolution_state = ?2,
            is_head = 0,
            is_active = 0,
            confidence = max(0.1, COALESCE(confidence, 0.9) - 0.1),
            updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE id = ?1",
        rusqlite::params![old_id, evolution_state],
    )?;
    if affected == 0 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }

    // Merge metadata: preserve any pre-existing keys, stamp supersede trio.
    // metadata_json may be NULL for rows inserted without prior policy
    // classifier output — treat NULL as an empty object rather than
    // panicking inside .as_str() on a None value.
    let meta_str: Option<String> = conn
        .query_row(
            "SELECT metadata_json FROM notes WHERE id = ?1",
            [old_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();

    let mut meta: serde_json::Map<String, JsonValue> = meta_str
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    meta.insert("superseded_by".to_string(), JsonValue::from(new_id));
    meta.insert("supersede_reason".to_string(), JsonValue::from(reason));
    meta.insert("is_outdated".to_string(), JsonValue::Bool(true));

    // Outcome fields mirror Python `atomic_update_outcome` at
    // backend/services/storage.py:1218-1227 — increment the existing counters
    // instead of resetting. A note with prior failure_count=3 becomes 4 on
    // auto-supersede, not 1. Preserves outcome history across the transition.
    let prior_success = meta
        .get("success_count")
        .and_then(JsonValue::as_i64)
        .unwrap_or(0);
    let prior_failure = meta
        .get("failure_count")
        .and_then(JsonValue::as_i64)
        .unwrap_or(0);
    meta.insert("success_count".to_string(), JsonValue::from(prior_success));
    meta.insert(
        "failure_count".to_string(),
        JsonValue::from(prior_failure + 1),
    );
    meta.insert(
        "last_outcome_reason".to_string(),
        JsonValue::from(format!("auto-superseded by {new_id}")),
    );

    let new_meta = match serde_json::to_string(&meta) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("metadata serialization failed for superseded note {old_id}: {e}");
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "metadata serialize: {e}"
            )));
        }
    };
    conn.execute(
        "UPDATE notes SET metadata_json = ?1 WHERE id = ?2",
        rusqlite::params![new_meta, old_id],
    )?;

    if evolution_state == "superseded" {
        insert_note_relation(conn, new_id, old_id, "forget_cascade", 1.0)?;
    }

    Ok(())
}

/// Convert a f32 vector to a binary blob (little-endian f32 array).
pub fn vector_to_blob(vec: &[f32]) -> Vec<u8> {
    vec.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Insert a row into `note_events`.
///
/// Mirrors `backend/services/storage.py` `add_note_event()`.
/// Runs within the caller's transaction context (no internal BEGIN/COMMIT).
///
/// Schema:
///   id INTEGER PK, note_id TEXT, event_type TEXT,
///   related_note_id TEXT, payload_json TEXT,
///   created_at TEXT DEFAULT (datetime('now'))
pub fn insert_note_event(
    conn: &Connection,
    note_id: &str,
    event_type: &str,
    related_note_id: Option<&str>,
    payload_json: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO note_events (note_id, event_type, related_note_id, payload_json, created_at)
         VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        rusqlite::params![note_id, event_type, related_note_id, payload_json],
    )?;
    Ok(())
}

/// Insert a row into `note_relations`.
///
/// Uses INSERT OR IGNORE (PRIMARY KEY = from+to+kind) to match Python's
/// `add_relation` semantics (`INSERT OR IGNORE INTO note_relations …`).
/// Called outside the main write transaction so a failure here is non-fatal.
pub fn insert_note_relation(
    conn: &Connection,
    from_note_id: &str,
    to_note_id: &str,
    relation_type: &str,
    strength: f64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO note_relations (from_note_id, to_note_id, relation_type, strength)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![from_note_id, to_note_id, relation_type, strength],
    )?;
    Ok(())
}

/// Upsert a relation by keeping the stronger of the current and proposed
/// strengths. This is the Rust-owned replacement for Python consolidation's
/// direct `note_relations` max-strength update path.
pub fn upsert_note_relation_max(
    conn: &Connection,
    from_note_id: &str,
    to_note_id: &str,
    relation_type: &str,
    strength: f64,
) -> rusqlite::Result<(String, f64)> {
    let existing: Option<f64> = conn
        .query_row(
            "SELECT strength
             FROM note_relations
             WHERE from_note_id = ?1 AND to_note_id = ?2 AND relation_type = ?3",
            rusqlite::params![from_note_id, to_note_id, relation_type],
            |row| row.get(0),
        )
        .optional()?;
    match existing {
        None => {
            conn.execute(
                "INSERT INTO note_relations (from_note_id, to_note_id, relation_type, strength)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![from_note_id, to_note_id, relation_type, strength],
            )?;
            Ok(("created".to_string(), strength))
        }
        Some(current) => {
            let next = current.max(strength);
            if next != current {
                conn.execute(
                    "UPDATE note_relations
                     SET strength = ?1
                     WHERE from_note_id = ?2 AND to_note_id = ?3 AND relation_type = ?4",
                    rusqlite::params![next, from_note_id, to_note_id, relation_type],
                )?;
                Ok(("updated".to_string(), next))
            } else {
                Ok(("unchanged".to_string(), current))
            }
        }
    }
}

/// Insert-or-strengthen a co-activation style relation, capped at 1.0.
pub fn strengthen_note_relation(
    conn: &Connection,
    from_note_id: &str,
    to_note_id: &str,
    relation_type: &str,
    initial_strength: f64,
    delta: f64,
) -> rusqlite::Result<(String, f64)> {
    let existing: Option<f64> = conn
        .query_row(
            "SELECT strength
             FROM note_relations
             WHERE from_note_id = ?1 AND to_note_id = ?2 AND relation_type = ?3",
            rusqlite::params![from_note_id, to_note_id, relation_type],
            |row| row.get(0),
        )
        .optional()?;
    match existing {
        None => {
            conn.execute(
                "INSERT INTO note_relations (from_note_id, to_note_id, relation_type, strength)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![from_note_id, to_note_id, relation_type, initial_strength],
            )?;
            Ok(("created".to_string(), initial_strength))
        }
        Some(current) => {
            let next = (current + delta).min(1.0);
            if next != current {
                conn.execute(
                    "UPDATE note_relations
                     SET strength = ?1
                     WHERE from_note_id = ?2 AND to_note_id = ?3 AND relation_type = ?4",
                    rusqlite::params![next, from_note_id, to_note_id, relation_type],
                )?;
                Ok(("strengthened".to_string(), next))
            } else {
                Ok(("unchanged".to_string(), current))
            }
        }
    }
}

/// Stamp canonical lineage fields after a batch write when the caller already
/// resolved the source root/topic. Kept in Rust so Python consolidation does
/// not need to issue direct `notes` updates for root/version/topic metadata.
pub fn stamp_note_lineage(
    conn: &Connection,
    note_id: &str,
    root_id: Option<&str>,
    version: Option<i64>,
    topic_key: Option<&str>,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE notes
         SET root_id = COALESCE(?2, root_id),
             version = COALESCE(?3, version),
             topic_key = COALESCE(?4, topic_key)
         WHERE id = ?1",
        rusqlite::params![note_id, root_id, version, topic_key],
    )
}
