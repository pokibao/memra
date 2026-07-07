//! Regression tests for Rust add_memory cold-storage transactionality.
//!
//! The write path must not return success for an active SQL note when the
//! required cold-storage append/ref update failed.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use memra_core::core::write_orchestrator::{AddMemoryParams, AddMemoryResult, WriteOrchestrator};
use memra_core::storage::cold_storage::ColdStorageWriter;
use memra_core::storage::db::DbPool;
use rusqlite::Connection;

fn unique_suffix(label: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos}-{label}")
}

fn temp_db_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ma-cold-txn-{}.sqlite3", unique_suffix(label)))
}

fn temp_dir_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ma-cold-txn-{}", unique_suffix(label)))
}

fn create_schema(conn: &Connection, include_cold_storage_ref: bool) {
    let cold_ref_column = if include_cold_storage_ref {
        "cold_storage_ref  TEXT,"
    } else {
        ""
    };
    conn.execute_batch(&format!(
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
            {cold_ref_column}
            recall_count      INTEGER DEFAULT 0,
            event_when        TEXT,
            event_when_ts     REAL
        );
        CREATE VIRTUAL TABLE notes_fts USING fts5(note_id UNINDEXED, content);",
    ))
    .expect("schema creation failed");
}

fn setup_orchestrator(
    label: &str,
    cold_storage: ColdStorageWriter,
    include_cold_storage_ref: bool,
) -> (WriteOrchestrator, PathBuf) {
    let db_path = temp_db_path(label);
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn, include_cold_storage_ref);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let orchestrator = WriteOrchestrator::new(write_pool, cold_storage, "test-project".to_string());
    (orchestrator, db_path)
}

fn count_notes(db_path: &PathBuf) -> i64 {
    let conn = Connection::open(db_path).expect("open db for count");
    conn.query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
        .expect("count notes")
}

fn add_event(orchestrator: &WriteOrchestrator, label: &str) -> AddMemoryResult {
    orchestrator.add_memory(&AddMemoryParams {
        content: format!("cold storage transaction event {label}"),
        layer: Some("event_log".to_string()),
        category: Some("event".to_string()),
        memory_kind: Some("event".to_string()),
        confidence: Some(0.9),
        source: Some("user".to_string()),
        created_by: Some("user".to_string()),
        ..Default::default()
    })
}

#[test]
fn successful_add_memory_stores_cold_storage_ref() {
    let cold_dir = temp_dir_path("success-cold");
    let cold = ColdStorageWriter::new(cold_dir.clone(), "test-project".to_string());
    let (orchestrator, db_path) = setup_orchestrator("success", cold, true);

    let note_id = match add_event(&orchestrator, "success") {
        AddMemoryResult::Saved {
            id,
            cold_storage_ref,
            ..
        } => {
            assert!(
                cold_storage_ref.is_some(),
                "successful required cold storage write must return a ref"
            );
            id
        }
        other => panic!("expected Saved, got {other:?}"),
    };

    let conn = Connection::open(&db_path).expect("open db");
    let stored_ref: Option<String> = conn
        .query_row(
            "SELECT cold_storage_ref FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |row| row.get(0),
        )
        .expect("stored note ref");

    assert!(
        stored_ref.is_some(),
        "cold_storage_ref must be persisted on the note row"
    );

    let cold_file = fs::read_dir(cold_dir.join("test-project"))
        .expect("cold project dir")
        .next()
        .expect("cold jsonl file")
        .expect("cold dir entry")
        .path();
    let line = fs::read_to_string(cold_file).expect("read cold JSONL");
    let record: serde_json::Value =
        serde_json::from_str(line.trim()).expect("cold JSONL should be valid JSON");
    assert_eq!(record["metadata"]["category"], "event");
    assert_eq!(record["metadata"]["confidence"], 0.9);
    assert_eq!(record["metadata"]["source"], "user");
    assert_eq!(record["metadata"]["created_by"], "user");
    assert!(
        record["metadata"].get("memory_type").is_none(),
        "cold storage metadata should mirror Python source metadata, not policy metadata"
    );

    let _ = fs::remove_dir_all(cold_dir);
    let _ = fs::remove_file(db_path);
}

#[test]
fn cold_storage_append_failure_rolls_back_note() {
    let blocked_base = temp_dir_path("blocked-base-file");
    fs::write(&blocked_base, "not a directory").expect("create blocking file");
    let cold = ColdStorageWriter::new(blocked_base.clone(), "test-project".to_string());
    let (orchestrator, db_path) = setup_orchestrator("append-fail", cold, true);

    let result = add_event(&orchestrator, "append-fail");
    match result {
        AddMemoryResult::Error(message) => {
            assert!(
                message.contains("Cold storage append failed"),
                "unexpected error: {message}"
            );
        }
        other => panic!("expected cold-storage error, got {other:?}"),
    }

    assert_eq!(
        count_notes(&db_path),
        0,
        "append failure must roll back the SQL note"
    );

    let _ = fs::remove_file(blocked_base);
    let _ = fs::remove_file(db_path);
}

#[test]
fn cold_storage_ref_update_failure_rolls_back_note() {
    let cold_dir = temp_dir_path("missing-column-cold");
    let cold = ColdStorageWriter::new(cold_dir.clone(), "test-project".to_string());
    let (orchestrator, db_path) = setup_orchestrator("ref-update-fail", cold, false);

    let result = add_event(&orchestrator, "ref-update-fail");
    match result {
        AddMemoryResult::Error(message) => {
            assert!(
                message.contains("Cold storage ref update failed"),
                "unexpected error: {message}"
            );
        }
        other => panic!("expected cold-storage ref update error, got {other:?}"),
    }

    assert_eq!(
        count_notes(&db_path),
        0,
        "cold_storage_ref update failure must roll back the SQL note"
    );

    let _ = fs::remove_dir_all(cold_dir);
    let _ = fs::remove_file(db_path);
}

#[test]
fn disabled_cold_storage_preserves_existing_test_behavior() {
    let cold = ColdStorageWriter::disabled();
    let (orchestrator, db_path) = setup_orchestrator("disabled", cold, true);

    match add_event(&orchestrator, "disabled") {
        AddMemoryResult::Saved {
            cold_storage_ref, ..
        } => assert!(
            cold_storage_ref.is_none(),
            "disabled cold storage should not produce a ref"
        ),
        other => panic!("expected Saved with disabled cold storage, got {other:?}"),
    }

    assert_eq!(count_notes(&db_path), 1);
    let _ = fs::remove_file(db_path);
}
