use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use memra_core::retrieval::search::SearchEngine;
use memra_core::storage::db::DbPool;
use rusqlite::Connection;

fn temp_dir(name: &str) -> Result<PathBuf, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("ma-project-isolation-{now}-{name}"));
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    Ok(dir)
}

fn bootstrap_db(path: &PathBuf) -> Result<(), String> {
    let conn = Connection::open(path).map_err(|error| error.to_string())?;
    conn.execute_batch(
        r#"
        CREATE TABLE notes (
            id TEXT PRIMARY KEY,
            content TEXT NOT NULL,
            layer TEXT,
            category TEXT,
            vector_json TEXT,
            vector_blob BLOB,
            metadata_json TEXT,
            created_at TEXT,
            updated_at TEXT,
            valid_at REAL,
            is_active INTEGER NOT NULL DEFAULT 1,
            agent_id TEXT,
            evolution_state TEXT,
            topic_key TEXT,
            review_after TEXT,
            room TEXT,
            project_id TEXT,
            confidence REAL,
            session_id TEXT,
            recall_count INTEGER DEFAULT 0,
            event_when TEXT,
            event_when_ts REAL
        );

        CREATE TABLE recall_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            note_id TEXT NOT NULL,
            recalled_at TEXT NOT NULL,
            session_id TEXT,
            query_text TEXT
        );
        "#,
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn insert_checkpoint(
    conn: &Connection,
    id: &str,
    project_id: &str,
    task_id: &str,
    status: &str,
) -> Result<(), String> {
    let metadata = format!(
        r#"{{"record_type":"checkpoint","task_id":"{task_id}","task_status":"{status}","next_step":"next-{task_id}"}}"#
    );
    conn.execute(
        "INSERT INTO notes (
            id, content, layer, category, metadata_json, created_at, updated_at,
            is_active, project_id, confidence, recall_count
         ) VALUES (?1, ?2, 'event_log', 'event', ?3, ?4, ?4, 1, ?5, 0.9, 0)",
        rusqlite::params![
            id,
            format!("Checkpoint for {task_id} in {project_id}"),
            metadata,
            "2026-04-14T00:00:00Z",
            project_id,
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn insert_note(conn: &Connection, id: &str, project_id: &str, content: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO notes (
            id, content, layer, category, created_at, updated_at, is_active, project_id, confidence, recall_count
         ) VALUES (?1, ?2, 'verified_fact', 'architecture', ?3, ?3, 1, ?4, 0.9, 0)",
        rusqlite::params![id, content, "2026-04-14T00:00:00Z", project_id],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn insert_recall(conn: &Connection, note_id: &str, recalled_at: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO recall_log (note_id, recalled_at, session_id, query_text)
         VALUES (?1, ?2, 'test-session', 'test query')",
        rusqlite::params![note_id, recalled_at],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

#[test]
fn scoped_checkpoint_search_does_not_leak_across_projects() -> Result<(), String> {
    let dir = temp_dir("checkpoints")?;
    let db_path = dir.join("memory_anchor.sqlite3");
    bootstrap_db(&db_path)?;
    let conn = Connection::open(&db_path).map_err(|error| error.to_string())?;
    insert_checkpoint(&conn, "cp-a", "alpha", "task-alpha", "in_progress")?;
    insert_checkpoint(&conn, "cp-b", "beta", "task-beta", "in_progress")?;
    drop(conn);

    let pool = DbPool::open_readonly(&db_path).map_err(|error| error.to_string())?;
    let engine = SearchEngine::new(pool);

    let results =
        engine.search_checkpoints_for_project(None, None, Some("active"), Some("alpha"), 10);
    assert_eq!(results.len(), 1);
    let task_id = results[0]
        .metadata
        .as_ref()
        .and_then(|meta| meta.get("task_id"))
        .and_then(|value| value.as_str());
    assert_eq!(task_id, Some("task-alpha"));
    Ok(())
}

#[test]
fn scoped_wake_context_only_counts_project_rows() -> Result<(), String> {
    let dir = temp_dir("wake")?;
    let db_path = dir.join("memory_anchor.sqlite3");
    bootstrap_db(&db_path)?;
    let conn = Connection::open(&db_path).map_err(|error| error.to_string())?;
    insert_checkpoint(&conn, "cp-a", "alpha", "task-alpha", "in_progress")?;
    insert_checkpoint(&conn, "cp-b", "beta", "task-beta", "in_progress")?;
    insert_note(&conn, "note-a1", "alpha", "alpha note 1")?;
    insert_note(&conn, "note-a2", "alpha", "alpha note 2")?;
    insert_note(&conn, "note-b1", "beta", "beta note 1")?;
    drop(conn);

    let pool = DbPool::open_readonly(&db_path).map_err(|error| error.to_string())?;
    let engine = SearchEngine::new(pool);

    let snapshot = engine.get_context_wake_for_project(Some("alpha"));
    assert_eq!(snapshot["active_checkpoints"].as_i64(), Some(1));

    // 2026-05-04: recent_memories migrated from a count to a list of preview
    // objects (wake_search_drift_red bug 3). Cross-project isolation now
    // means: alpha sees its 2 alpha notes, beta's 1 row stays out.
    // Checkpoint rows are excluded from recent_memories (they already
    // appear under `checkpoints`).
    let recent = snapshot["recent_memories"]
        .as_array()
        .expect("recent_memories must be a list (post-drift fix)");
    assert_eq!(
        recent.len(),
        2,
        "alpha should see 2 alpha notes; got {recent:?}"
    );
    let recent_ids: Vec<&str> = recent
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    assert!(
        recent_ids.contains(&"note-a1") && recent_ids.contains(&"note-a2"),
        "expected note-a1 and note-a2 in alpha-scoped recent_memories; got {recent_ids:?}"
    );
    assert!(
        !recent_ids.contains(&"note-b1"),
        "beta's note-b1 leaked into alpha-scoped recent_memories: {recent_ids:?}"
    );

    let first_task = snapshot["checkpoints"][0]["task_id"].as_str();
    assert_eq!(first_task, Some("task-alpha"));
    Ok(())
}

#[test]
fn search_results_include_latest_recall_log_timestamp() -> Result<(), String> {
    let dir = temp_dir("last-recalled")?;
    let db_path = dir.join("memory_anchor.sqlite3");
    bootstrap_db(&db_path)?;
    let conn = Connection::open(&db_path).map_err(|error| error.to_string())?;
    insert_note(&conn, "note-a1", "alpha", "alpha note 1")?;
    insert_recall(&conn, "note-a1", "2026-04-13T00:00:00Z")?;
    insert_recall(&conn, "note-a1", "2026-04-14T00:00:00Z")?;
    drop(conn);

    let pool = DbPool::open_readonly(&db_path).map_err(|error| error.to_string())?;
    let engine = SearchEngine::new(pool);

    let results = engine.search(&memra_core::storage::db::SearchParams {
        query: String::new(),
        limit: 5,
        only_active: true,
        project_id: Some("alpha".to_string()),
        ..Default::default()
    });

    assert_eq!(results.len(), 1);
    // RFC3339 input is forwarded verbatim (`Z` stays `Z`) to preserve parity
    // with Python `get_recall_stats` which returns `MAX(recalled_at)` as-is.
    assert_eq!(
        results[0].last_recalled_at.as_deref(),
        Some("2026-04-14T00:00:00Z")
    );
    Ok(())
}
