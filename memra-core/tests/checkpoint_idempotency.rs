use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use memra_core::core::write_orchestrator::{AddMemoryParams, AddMemoryResult, WriteOrchestrator};
use memra_core::storage::cold_storage::ColdStorageWriter;
use memra_core::storage::db::DbPool;
use rusqlite::Connection;

type CheckpointPhysicalRow = (
    Option<String>,
    Option<String>,
    Option<f64>,
    Option<String>,
    Option<String>,
    Option<Vec<u8>>,
    Option<String>,
);

fn temp_db_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    std::env::temp_dir().join(format!("ma-checkpoint-idem-{nanos}-{label}.sqlite3"))
}

fn create_schema(conn: &Connection) {
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
    .expect("schema");
}

fn cleanup(db_path: &PathBuf) {
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(db_path.with_extension("sqlite3-wal"));
    let _ = fs::remove_file(db_path.with_extension("sqlite3-shm"));
}

fn checkpoint_meta(task_id: &str, status: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut metadata = serde_json::Map::new();
    metadata.insert("record_type".to_string(), "checkpoint".into());
    metadata.insert("task_id".to_string(), task_id.into());
    metadata.insert("task_status".to_string(), status.into());
    metadata
}

#[test]
fn save_checkpoint_reuses_identical_active_checkpoint() {
    let db_path = temp_db_path("reuse");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let cold = ColdStorageWriter::disabled();
    let writer = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    let task_id = "checkpoint-idempotent-task";
    let metadata = checkpoint_meta(task_id, "blocked");
    let first_id = writer
        .save_checkpoint(task_id, "same summary", metadata.clone())
        .expect("first checkpoint");

    let conn = Connection::open(&db_path).expect("read db");
    conn.execute(
        "INSERT INTO notes
          (id, content, layer, category, is_active, project_id, metadata_json, created_at)
         VALUES (?1, '[断点] checkpoint-idempotent-task: stale', 'event_log', 'event', 1, 'test-project', ?2, '2026-04-20T00:00:00Z')",
        (
            "stale-checkpoint-row",
            r#"{"record_type":"checkpoint","task_id":"checkpoint-idempotent-task","task_status":"blocked"}"#,
        ),
    )
    .expect("insert stale checkpoint head");
    drop(conn);

    let second_id = writer
        .save_checkpoint(task_id, "same summary", metadata)
        .expect("same checkpoint");

    assert_eq!(first_id, second_id);

    let conn = Connection::open(&db_path).expect("read db");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
        .expect("count");
    assert_eq!(count, 2);
    let active_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE is_active = 1",
            [],
            |row| row.get(0),
        )
        .expect("active count");
    assert_eq!(active_count, 1);
    let stale_updated_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE is_active = 0 AND updated_at IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("stale updated_at count");
    assert_eq!(
        stale_updated_count, 0,
        "checkpoint deactivation should mirror Python and leave updated_at unchanged"
    );

    cleanup(&db_path);
}

#[test]
fn save_checkpoint_reuses_checkpoint_with_pretty_metadata_json() {
    let db_path = temp_db_path("reuse-pretty-json");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let cold = ColdStorageWriter::disabled();
    let writer = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    let task_id = "checkpoint-pretty-json-task";
    let metadata = checkpoint_meta(task_id, "blocked");
    let first_id = writer
        .save_checkpoint(task_id, "same summary", metadata.clone())
        .expect("first checkpoint");

    let conn = Connection::open(&db_path).expect("read db");
    conn.execute(
        "UPDATE notes SET metadata_json = ?1 WHERE id = ?2",
        (
            format!(
                "{{\n  \"task_status\": \"blocked\",\n  \"task_id\": \"{task_id}\",\n  \"record_type\": \"checkpoint\"\n}}"
            ),
            &first_id,
        ),
    )
    .expect("rewrite metadata formatting");
    drop(conn);

    let second_id = writer
        .save_checkpoint(task_id, "same summary", metadata)
        .expect("same checkpoint");

    assert_eq!(first_id, second_id);

    let conn = Connection::open(&db_path).expect("read db");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
        .expect("count");
    assert_eq!(count, 1);

    cleanup(&db_path);
}

#[test]
fn save_checkpoint_skips_malformed_metadata_json_during_reuse_probe() {
    let db_path = temp_db_path("reuse-malformed-json");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let cold = ColdStorageWriter::disabled();
    let writer = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    let task_id = "checkpoint-malformed-json-task";
    let metadata = checkpoint_meta(task_id, "blocked");
    let first_id = writer
        .save_checkpoint(task_id, "same summary", metadata.clone())
        .expect("first checkpoint");

    let conn = Connection::open(&db_path).expect("read db");
    let content: String = conn
        .query_row(
            "SELECT content FROM notes WHERE id = ?1",
            (&first_id,),
            |row| row.get(0),
        )
        .expect("checkpoint content");
    conn.execute(
        "INSERT INTO notes
          (id, content, layer, category, is_active, project_id, metadata_json, created_at)
         VALUES (?1, ?2, 'event_log', 'event', 1, 'test-project', '{not-json', '2026-04-20T00:00:00Z')",
        ("corrupt-checkpoint-row", content),
    )
    .expect("insert malformed metadata row");
    drop(conn);

    let second_id = writer
        .save_checkpoint(task_id, "same summary", metadata)
        .expect("same checkpoint");

    assert_eq!(first_id, second_id);

    cleanup(&db_path);
}

#[test]
fn save_checkpoint_writes_new_row_when_summary_changes() {
    let db_path = temp_db_path("new-summary");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let cold = ColdStorageWriter::disabled();
    let writer = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    let task_id = "checkpoint-upsert-task";
    let first_id = writer
        .save_checkpoint(
            task_id,
            "first summary",
            checkpoint_meta(task_id, "blocked"),
        )
        .expect("first checkpoint");
    let second_id = writer
        .save_checkpoint(
            task_id,
            "second summary",
            checkpoint_meta(task_id, "blocked"),
        )
        .expect("second checkpoint");

    assert_ne!(first_id, second_id);

    let conn = Connection::open(&db_path).expect("read db");
    let active_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM notes WHERE is_active = 1",
            [],
            |row| row.get(0),
        )
        .expect("active count");
    assert_eq!(active_count, 1);

    cleanup(&db_path);
}

#[test]
fn save_checkpoint_writes_python_physical_fields() {
    let db_path = temp_db_path("physical-fields");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn);
    }

    let cold_dir = std::env::temp_dir().join(format!(
        "ma-checkpoint-cold-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let cold = ColdStorageWriter::new(cold_dir.clone(), "test-project".to_string());
    let writer = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    let task_id = "checkpoint-physical-fields";
    let metadata = checkpoint_meta(task_id, "blocked");
    let checkpoint_id = writer
        .save_checkpoint(task_id, "physical summary", metadata)
        .expect("checkpoint save");

    let conn = Connection::open(&db_path).expect("read db");
    let (
        source,
        created_by,
        valid_at,
        updated_at,
        vector_json,
        vector_blob,
        cold_storage_ref,
    ): CheckpointPhysicalRow = conn
        .query_row(
            "SELECT source, created_by, valid_at, updated_at, vector_json, vector_blob, cold_storage_ref
             FROM notes WHERE id = ?1",
            [&checkpoint_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .expect("checkpoint row");

    assert_eq!(source.as_deref(), Some("user"));
    assert_eq!(created_by.as_deref(), Some("user"));
    assert!(
        valid_at.is_some(),
        "checkpoint valid_at should be populated"
    );
    assert!(
        updated_at.is_none(),
        "first checkpoint insert should mirror Python and leave updated_at NULL"
    );
    assert!(
        vector_json.is_some(),
        "checkpoint should persist vector_json"
    );
    assert!(
        vector_blob.is_some(),
        "checkpoint should persist vector_blob"
    );
    assert!(
        cold_storage_ref.is_some(),
        "checkpoint should persist cold_storage_ref"
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
    assert_eq!(record["metadata"]["confidence"], 0.95);
    assert_eq!(record["metadata"]["source"], "user");
    assert_eq!(record["metadata"]["created_by"], "user");

    let _ = fs::remove_dir_all(cold_dir);
    cleanup(&db_path);
}

#[test]
fn add_memory_rejects_exact_duplicate_without_embedding() {
    let db_path = temp_db_path("exact-duplicate");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let cold = ColdStorageWriter::disabled();
    let writer = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    let params = AddMemoryParams {
        content: "exact duplicate memory".to_string(),
        layer: Some("verified_fact".to_string()),
        category: Some("bug".to_string()),
        confidence: Some(1.0),
        ..Default::default()
    };
    let first_id = match writer.add_memory(&params) {
        AddMemoryResult::Saved { id, .. } => id,
        other => panic!("expected first save, got {other:?}"),
    };

    match writer.add_memory(&params) {
        AddMemoryResult::Duplicate {
            existing_id,
            similarity,
        } => {
            assert_eq!(existing_id, first_id);
            assert_eq!(similarity, 1.0);
        }
        other => panic!("expected duplicate, got {other:?}"),
    }

    let conn = Connection::open(&db_path).expect("read db");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
        .expect("count");
    assert_eq!(count, 1);

    cleanup(&db_path);
}

#[test]
fn add_memory_allows_exact_duplicate_retry_to_fill_missing_vector() {
    let db_path = temp_db_path("exact-duplicate-backfill");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let cold = ColdStorageWriter::disabled();
    let writer = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    let params = AddMemoryParams {
        content: "exact duplicate vector backfill".to_string(),
        layer: Some("verified_fact".to_string()),
        category: Some("bug".to_string()),
        confidence: Some(1.0),
        ..Default::default()
    };
    let first_id = match writer.add_memory(&params) {
        AddMemoryResult::Saved { id, .. } => id,
        other => panic!("expected first save, got {other:?}"),
    };

    let conn = Connection::open(&db_path).expect("read db");
    conn.execute(
        "UPDATE notes SET vector_json = NULL, vector_blob = NULL WHERE id = ?1",
        (&first_id,),
    )
    .expect("clear vector fields");
    drop(conn);

    match writer.add_memory(&params) {
        AddMemoryResult::Saved { id, .. } => assert_ne!(id, first_id),
        other => panic!("expected retry with vector to save, got {other:?}"),
    }

    cleanup(&db_path);
}

#[test]
fn add_memory_allows_exact_duplicate_event_log_episode() {
    let db_path = temp_db_path("event-duplicate");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let cold = ColdStorageWriter::disabled();
    let writer = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    let params = AddMemoryParams {
        content: "same event episode".to_string(),
        layer: Some("event_log".to_string()),
        category: Some("event".to_string()),
        confidence: Some(1.0),
        ..Default::default()
    };
    let first_id = match writer.add_memory(&params) {
        AddMemoryResult::Saved { id, .. } => id,
        other => panic!("expected first save, got {other:?}"),
    };
    let second_id = match writer.add_memory(&params) {
        AddMemoryResult::Saved { id, .. } => id,
        other => panic!("expected second event save, got {other:?}"),
    };

    assert_ne!(first_id, second_id);

    let conn = Connection::open(&db_path).expect("read db");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
        .expect("count");
    assert_eq!(count, 2);

    cleanup(&db_path);
}
