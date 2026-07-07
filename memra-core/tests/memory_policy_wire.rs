//! Integration test for MemoryPolicy wired into WriteOrchestrator.
//!
//! Verifies:
//!   1. verified_fact + ai_extraction + transient marker → PolicySkipped short-circuit
//!      (returns BEFORE the embed/dedup path).
//!   2. verified_fact + manual user write with unclassified content → Saved, with
//!      policy-injected metadata (memory_type=project fallback + policy_warning).
//!   3. event_log layer bypasses policy entirely (no injected metadata).

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use memra_core::core::write_orchestrator::{AddMemoryParams, AddMemoryResult, WriteOrchestrator};
use memra_core::storage::cold_storage::ColdStorageWriter;
use memra_core::storage::db::DbPool;
use rusqlite::Connection;

fn temp_db_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    std::env::temp_dir().join(format!("ma-policy-wire-{nanos}-{label}.sqlite3"))
}

fn create_full_schema(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE notes (
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
        CREATE VIRTUAL TABLE notes_fts USING fts5(note_id UNINDEXED, content);",
    )
    .expect("schema creation failed");
}

fn cleanup(db_path: &PathBuf) {
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(db_path.with_extension("sqlite3-wal"));
    let _ = fs::remove_file(db_path.with_extension("sqlite3-shm"));
}

fn metadata_of(conn: &Connection, note_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT metadata_json FROM notes WHERE id = ?1",
        rusqlite::params![note_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

// Transient markers (Python memory_policy.py:87-94): 今天心情, 正在调试, etc.
// Classified as source_kind="extraction" ⇒ allow_durable=false.
#[test]
fn ai_extraction_transient_content_is_policy_skipped() {
    let db_path = temp_db_path("transient-skip");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_full_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let cold = ColdStorageWriter::disabled();
    let orchestrator = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    // "这轮" hits TRANSIENT_MARKERS (memory_policy.rs:67) but no memory_type
    // marker — so classify_memory_type=None, transient=true, source_kind=extraction
    // ⇒ allow_durable=false, reason=transient_session_state.
    let result = orchestrator.add_memory(&AddMemoryParams {
        content: "这轮调试我们推进了 XYZ 任务".to_string(),
        layer: Some("verified_fact".to_string()),
        source: Some("ai_extraction".to_string()),
        confidence: Some(0.9),
        ..Default::default()
    });

    match result {
        AddMemoryResult::PolicySkipped {
            layer,
            reason,
            memory_type,
        } => {
            assert_eq!(layer, "verified_fact");
            assert_eq!(reason, "transient_session_state");
            assert!(memory_type.is_none() || memory_type.as_deref() == Some("project"));
        }
        other => panic!("Expected PolicySkipped, got {other:?}"),
    }

    cleanup(&db_path);
}

// Manual user write with unclassified English content → falls back to
// memory_type=project + policy_warning=fallback_to_project.
#[test]
fn manual_write_unclassified_injects_fallback_metadata() {
    let db_path = temp_db_path("fallback-project");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_full_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let read_conn = Connection::open(&db_path).expect("read conn");
    let cold = ColdStorageWriter::disabled();
    let orchestrator = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    // Neutral free-form English that misses every marker (user / feedback /
    // project / reference / transient / repo-derivable). Manual write
    // (source="user") ⇒ source_kind=manual, classify=None, fallback branch
    // stamps project + policy_warning=fallback_to_project.
    let result = orchestrator.add_memory(&AddMemoryParams {
        content: "the green ball rolled down a quiet shallow hill one afternoon".to_string(),
        layer: Some("verified_fact".to_string()),
        source: Some("user".to_string()),
        confidence: Some(0.9),
        ..Default::default()
    });

    let note_id = match result {
        AddMemoryResult::Saved { id, .. } => id,
        other => panic!("Expected Saved, got {other:?}"),
    };

    let meta_json = metadata_of(&read_conn, &note_id).expect("metadata should exist");
    let meta: serde_json::Value = serde_json::from_str(&meta_json).expect("metadata parses");

    // Fallback stamped memory_type + policy_warning.
    assert_eq!(meta["memory_type"].as_str(), Some("project"));
    assert_eq!(meta["policy_warning"].as_str(), Some("fallback_to_project"));
    // 7-key typed metadata always present.
    assert_eq!(meta["stale_risk"].as_str(), Some("low"));
    assert!(meta["why"].is_string());
    assert!(meta["how_to_apply"].is_string());
    assert_eq!(meta["source_kind"].as_str(), Some("manual"));

    cleanup(&db_path);
}

// event_log layer bypasses policy entirely — no typed metadata keys injected.
#[test]
fn event_layer_bypasses_policy_evaluation() {
    let db_path = temp_db_path("event-bypass");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_full_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let read_conn = Connection::open(&db_path).expect("read conn");
    let cold = ColdStorageWriter::disabled();
    let orchestrator = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    let result = orchestrator.add_memory(&AddMemoryParams {
        content: "event parity wire — today's checkpoint 20260419".to_string(),
        layer: Some("event_log".to_string()),
        memory_kind: Some("event".to_string()),
        confidence: Some(0.9),
        when: Some("2026-04-19T00:00:00Z".to_string()),
        ..Default::default()
    });

    let note_id = match result {
        AddMemoryResult::Saved { id, layer, .. } => {
            assert_eq!(layer, "event_log");
            id
        }
        other => panic!("Expected Saved event_log, got {other:?}"),
    };

    // Policy did NOT run ⇒ no memory_type / policy_warning keys.
    let meta = metadata_of(&read_conn, &note_id);
    if let Some(meta_json) = meta {
        let parsed: serde_json::Value = serde_json::from_str(&meta_json).expect("metadata parses");
        assert!(
            parsed.get("memory_type").is_none(),
            "event_log must not carry memory_type: {meta_json}"
        );
        assert!(
            parsed.get("policy_warning").is_none(),
            "event_log must not carry policy_warning: {meta_json}"
        );
    }

    cleanup(&db_path);
}

// Issue #115: Python leaves `updated_at` NULL on first insert; Rust used to
// stamp `now()` unconditionally because add_memory_inner passed `Some(now)`
// to NoteInsert. Verify the first-insert path now persists NULL so the
// upsert_note() ON CONFLICT CASE branch can fill it in only on a real
// material-change update later. Mirrors backend/core/storage_rows.py.
#[test]
fn first_insert_leaves_updated_at_null_for_python_parity() {
    let db_path = temp_db_path("first-insert-updated-at");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_full_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let read_conn = Connection::open(&db_path).expect("read conn");
    let cold = ColdStorageWriter::disabled();
    let orchestrator = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    // Use procedure_schema to bypass MemoryPolicy gate cleanly. Test the
    // structural NULL semantics, not policy.
    let result = orchestrator.add_memory(&AddMemoryParams {
        content: "first-insert NULL parity probe content".to_string(),
        layer: Some("procedure_schema".to_string()),
        source: Some("user".to_string()),
        confidence: Some(0.9),
        ..Default::default()
    });

    let note_id = match result {
        AddMemoryResult::Saved { id, .. } => id,
        other => panic!("Expected Saved, got {other:?}"),
    };

    let updated_at: Option<String> = read_conn
        .query_row(
            "SELECT updated_at FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |row| row.get(0),
        )
        .expect("read updated_at");

    assert_eq!(
        updated_at, None,
        "issue #115: first-insert must leave updated_at NULL for Python parity"
    );

    // created_at must still be stamped — only updated_at is the parity gap.
    let created_at: Option<String> = read_conn
        .query_row(
            "SELECT created_at FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |row| row.get(0),
        )
        .expect("read created_at");
    assert!(
        created_at.is_some(),
        "first-insert must still record created_at"
    );

    cleanup(&db_path);
}

// Issue #118: event_log writes must persist `event_when` (RFC3339 string)
// and `event_when_ts` (epoch seconds) so Rust matches Python's
// storage_rows.py columns. Pre-fix: NoteInsert lacked these fields and
// the INSERT statement omitted both columns, so event rows came back
// as NULL on both. RED panic looked like:
//     left: (None, None)
//    right: (Some("2026-04-15..."), Some(1776556800.0))
#[test]
fn event_log_persists_event_when_and_event_when_ts_for_python_parity() {
    let db_path = temp_db_path("event-when-persist");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_full_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let read_conn = Connection::open(&db_path).expect("read conn");
    let cold = ColdStorageWriter::disabled();
    let orchestrator = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    let result = orchestrator.add_memory(&AddMemoryParams {
        content: "S02-03 event probe — checkpoint timestamp".to_string(),
        layer: Some("event_log".to_string()),
        memory_kind: Some("event".to_string()),
        confidence: Some(0.9),
        when: Some("2026-04-15T00:00:00Z".to_string()),
        ..Default::default()
    });

    let note_id = match result {
        AddMemoryResult::Saved { id, .. } => id,
        other => panic!("Expected Saved, got {other:?}"),
    };

    let (event_when, event_when_ts): (Option<String>, Option<f64>) = read_conn
        .query_row(
            "SELECT event_when, event_when_ts FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read event timing columns");

    assert_eq!(
        event_when.as_deref(),
        Some("2026-04-15T00:00:00Z"),
        "issue #118: event_when must persist the raw RFC3339 string"
    );
    let ts = event_when_ts.expect("event_when_ts must persist");
    // 2026-04-15T00:00:00Z = 1776211200 epoch seconds.
    assert!(
        (ts - 1_776_211_200.0).abs() < 1e-3,
        "issue #118: event_when_ts must be the parsed epoch seconds, got {ts}"
    );

    cleanup(&db_path);
}

// Issue #118 PR #215 Codex P2-2 regression guard: an UPSERT that omits
// event_when / event_when_ts must CLEAR the existing values, not preserve
// them via COALESCE. Mirrors Python `storage_rows.py` direct-assignment
// semantics so replay/update paths can drop stale event timing metadata.
#[test]
fn upsert_clears_event_when_columns_when_caller_omits_them() {
    use memra_core::storage::writer::{NoteInsert, upsert_note};

    let db_path = temp_db_path("upsert-clear-event-when");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_full_schema(&conn);
    }

    let conn = Connection::open(&db_path).expect("rw conn");

    // Phase 1: insert with both event timing columns set.
    let initial = NoteInsert {
        id: "note-118-p2-2".to_string(),
        content: "event probe".to_string(),
        layer: "event_log".to_string(),
        is_active: true,
        is_head: true,
        event_when: Some("2026-04-15T00:00:00Z".to_string()),
        event_when_ts: Some(1_776_211_200.0),
        ..Default::default()
    };
    upsert_note(&conn, &initial).expect("initial insert");

    let (ew1, ets1): (Option<String>, Option<f64>) = conn
        .query_row(
            "SELECT event_when, event_when_ts FROM notes WHERE id = ?1",
            rusqlite::params!["note-118-p2-2"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read row");
    assert_eq!(ew1.as_deref(), Some("2026-04-15T00:00:00Z"));
    assert!((ets1.unwrap() - 1_776_211_200.0).abs() < 1e-3);

    // Phase 2: UPSERT same id with content change but omitted event timing.
    // Python `storage_rows.py` would clear both columns. Pre-fix Rust used
    // COALESCE and silently kept the old values forever.
    let omit = NoteInsert {
        id: "note-118-p2-2".to_string(),
        content: "event probe — replay path that drops timing".to_string(),
        layer: "event_log".to_string(),
        is_active: true,
        is_head: true,
        event_when: None,
        event_when_ts: None,
        ..Default::default()
    };
    upsert_note(&conn, &omit).expect("upsert omit");

    let (ew2, ets2): (Option<String>, Option<f64>) = conn
        .query_row(
            "SELECT event_when, event_when_ts FROM notes WHERE id = ?1",
            rusqlite::params!["note-118-p2-2"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read row");
    assert!(
        ew2.is_none(),
        "Issue #118 P2-2: UPSERT must clear event_when when omitted, got {ew2:?}"
    );
    assert!(
        ets2.is_none(),
        "Issue #118 P2-2: UPSERT must clear event_when_ts when omitted, got {ets2:?}"
    );

    cleanup(&db_path);
}

#[test]
fn non_event_writes_leave_event_when_columns_null() {
    let db_path = temp_db_path("non-event-null");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_full_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let read_conn = Connection::open(&db_path).expect("read conn");
    let cold = ColdStorageWriter::disabled();
    let orchestrator = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    let result = orchestrator.add_memory(&AddMemoryParams {
        content: "non-event content has no event_when".to_string(),
        layer: Some("procedure_schema".to_string()),
        confidence: Some(0.9),
        ..Default::default()
    });

    let note_id = match result {
        AddMemoryResult::Saved { id, .. } => id,
        other => panic!("Expected Saved, got {other:?}"),
    };

    let (event_when, event_when_ts): (Option<String>, Option<f64>) = read_conn
        .query_row(
            "SELECT event_when, event_when_ts FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read event timing columns");

    assert!(
        event_when.is_none(),
        "non-event writes must leave event_when NULL, got {event_when:?}"
    );
    assert!(
        event_when_ts.is_none(),
        "non-event writes must leave event_when_ts NULL, got {event_when_ts:?}"
    );

    cleanup(&db_path);
}
