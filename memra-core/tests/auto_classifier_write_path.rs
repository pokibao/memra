//! Rust write-path coverage for auto-classification opt-in semantics.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use memra_core::core::write_orchestrator::{AddMemoryParams, AddMemoryResult, WriteOrchestrator};
use memra_core::storage::cold_storage::ColdStorageWriter;
use memra_core::storage::db::DbPool;
use rusqlite::Connection;

fn temp_db_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("ma-auto-classifier-write-{nanos}-{label}.sqlite3"))
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

fn cleanup(db_path: &Path) {
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(db_path.with_extension("sqlite3-wal"));
    let _ = fs::remove_file(db_path.with_extension("sqlite3-shm"));
}

fn build_orchestrator(db_path: &Path) -> WriteOrchestrator {
    let write_pool = DbPool::open(db_path).expect("open write pool");
    let cold = ColdStorageWriter::disabled();
    WriteOrchestrator::new(write_pool, cold, "test-project".to_string())
}

fn read_layer_category(conn: &Connection, note_id: &str) -> (Option<String>, Option<String>) {
    conn.query_row(
        "SELECT layer, category FROM notes WHERE id = ?1",
        rusqlite::params![note_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .expect("note should be readable")
}

fn saved_id(result: AddMemoryResult) -> String {
    match result {
        AddMemoryResult::Saved { id, .. } => id,
        other => panic!("expected Saved, got {other:?}"),
    }
}

#[test]
fn auto_classify_fills_missing_category_without_changing_supplied_layer() {
    let db_path = temp_db_path("category");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_full_schema(&conn);
    }

    let orchestrator = build_orchestrator(&db_path);
    let note_id = saved_id(orchestrator.add_memory(&AddMemoryParams {
        content: "We decided to go with SQLite after trade-off review.".to_string(),
        layer: Some("verified_fact".to_string()),
        source: Some("user".to_string()),
        auto_classify: true,
        ..Default::default()
    }));

    let conn = Connection::open(&db_path).expect("read temp db");
    assert_eq!(
        read_layer_category(&conn, &note_id),
        (
            Some("verified_fact".to_string()),
            Some("decision".to_string())
        )
    );

    cleanup(&db_path);
}

#[test]
fn auto_classify_layer_can_select_layer_when_original_layer_missing() {
    let db_path = temp_db_path("layer");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_full_schema(&conn);
    }

    let orchestrator = build_orchestrator(&db_path);
    let note_id = saved_id(orchestrator.add_memory(&AddMemoryParams {
        content: "今天下午完成会议并发布上线。".to_string(),
        source: Some("user".to_string()),
        auto_classify: true,
        auto_classify_layer: true,
        ..Default::default()
    }));

    let conn = Connection::open(&db_path).expect("read temp db");
    assert_eq!(
        read_layer_category(&conn, &note_id),
        (Some("event_log".to_string()), Some("event".to_string()))
    );

    cleanup(&db_path);
}

#[test]
fn auto_classify_layer_does_not_override_default_when_disabled() {
    let db_path = temp_db_path("disabled");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_full_schema(&conn);
    }

    let orchestrator = build_orchestrator(&db_path);
    let note_id = saved_id(orchestrator.add_memory(&AddMemoryParams {
        content: "今天下午完成会议并发布上线。".to_string(),
        source: Some("user".to_string()),
        auto_classify: true,
        auto_classify_layer: false,
        ..Default::default()
    }));

    let conn = Connection::open(&db_path).expect("read temp db");
    assert_eq!(
        read_layer_category(&conn, &note_id),
        (Some("verified_fact".to_string()), Some("event".to_string()))
    );

    cleanup(&db_path);
}
