//! Public-build coverage for the commercial-number detector.

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
    std::env::temp_dir().join(format!("ma-commercial-detector-{nanos}-{label}.sqlite3"))
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

fn read_note(conn: &Connection, note_id: &str) -> (Option<String>, Option<f64>, serde_json::Value) {
    let (source, confidence, metadata_json): (Option<String>, Option<f64>, Option<String>) = conn
        .query_row(
            "SELECT source, confidence, metadata_json FROM notes WHERE id = ?1",
            rusqlite::params![note_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("note should be readable");
    let metadata = metadata_json
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(serde_json::Value::Null);
    (source, confidence, metadata)
}

fn build_orchestrator(db_path: &Path) -> WriteOrchestrator {
    let write_pool = DbPool::open(db_path).expect("open write pool");
    let cold = ColdStorageWriter::disabled();
    WriteOrchestrator::new(write_pool, cold, "test-project".to_string())
}

#[test]
fn write_orchestrator_keeps_ai_commercial_number_when_public_entity_list_is_empty() {
    let db_path = temp_db_path("flagged");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_full_schema(&conn);
    }

    let orchestrator = build_orchestrator(&db_path);
    let result = orchestrator.add_memory(&AddMemoryParams {
        content: "MarketLab ¥600K Riley续签".to_string(),
        layer: Some("event_log".to_string()),
        source: Some("ai".to_string()),
        confidence: Some(0.95),
        ..Default::default()
    });
    let note_id = match result {
        AddMemoryResult::Saved { id, .. } => id,
        other => panic!("expected Saved, got {other:?}"),
    };

    let conn = Connection::open(&db_path).expect("read temp db");
    let (source, confidence, metadata) = read_note(&conn, &note_id);
    assert_eq!(source.as_deref(), Some("ai"));
    assert_eq!(confidence, Some(0.95));
    assert!(metadata.get("outdated_pending").is_none());
    assert!(metadata.get("detector_reason").is_none());

    cleanup(&db_path);
}

#[test]
fn write_orchestrator_respects_related_user_anchor() {
    let db_path = temp_db_path("user-anchor");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_full_schema(&conn);
        conn.execute(
            "INSERT INTO notes (id, content, layer, source, project_id, is_active)
             VALUES ('mem-user', 'alice confirmed Riley renewal price', 'event_log', 'user', 'test-project', 1)",
            [],
        )
        .expect("seed user anchor");
    }

    let orchestrator = build_orchestrator(&db_path);
    let result = orchestrator.add_memory(&AddMemoryParams {
        content: "订单金额¥12K Riley".to_string(),
        layer: Some("event_log".to_string()),
        source: Some("ai".to_string()),
        confidence: Some(0.95),
        related_ids: Some(vec!["mem-user".to_string()]),
        ..Default::default()
    });
    let note_id = match result {
        AddMemoryResult::Saved { id, .. } => id,
        other => panic!("expected Saved, got {other:?}"),
    };

    let conn = Connection::open(&db_path).expect("read temp db");
    let (source, confidence, metadata) = read_note(&conn, &note_id);
    assert_eq!(source.as_deref(), Some("ai"));
    assert_eq!(confidence, Some(0.95));
    assert!(metadata.get("outdated_pending").is_none());
    assert!(metadata.get("detector_reason").is_none());

    cleanup(&db_path);
}
