use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use memra_core::retrieval::search::SearchEngine;
use memra_core::storage::db::{DbPool, SearchParams};
use rusqlite::Connection;

fn temp_db_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("ma-include-constitution-{nanos}-{label}.sqlite3"))
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
            session_id        TEXT,
            recall_count      INTEGER DEFAULT 0,
            event_when        TEXT,
            event_when_ts     REAL
        );
        CREATE VIRTUAL TABLE notes_fts USING fts5(note_id UNINDEXED, content);
        CREATE TABLE recall_log (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            note_id TEXT NOT NULL,
            recalled_at TEXT NOT NULL,
            session_id TEXT,
            query_text TEXT
        );",
    )
    .expect("schema creation failed");
}

fn insert_note(
    conn: &Connection,
    id: &str,
    layer: &str,
    category: &str,
    confidence: f64,
    created_at: &str,
    content: &str,
) {
    conn.execute(
        "INSERT INTO notes (
            id, content, layer, category, is_active, confidence, project_id,
            created_at, updated_at, metadata_json, evolution_state, recall_count
         ) VALUES (?1, ?2, ?3, ?4, 1, ?5, 'test-project', ?6, ?6, '{}', 'active', 0)",
        rusqlite::params![id, content, layer, category, confidence, created_at],
    )
    .expect("insert note");
    conn.execute(
        "INSERT INTO notes_fts (note_id, content) VALUES (?1, ?2)",
        rusqlite::params![id, content],
    )
    .expect("insert fts");
}

#[test]
fn include_constitution_false_excludes_identity_before_limiting() {
    let db_path = temp_db_path("identity-heavy");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn);

        for idx in 0..8 {
            insert_note(
                &conn,
                &format!("identity-{idx}"),
                "identity_schema",
                "person",
                1.0,
                "2026-04-14T00:00:00Z",
                &format!("release workflow identity priority exact match {idx}"),
            );
        }

        for idx in 0..3 {
            insert_note(
                &conn,
                &format!("fact-{idx}"),
                "verified_fact",
                "decision",
                0.2,
                "2020-01-01T00:00:00Z",
                &format!("release workflow fallback verified fact {idx}"),
            );
        }
    }

    let pool = DbPool::open_readonly(&db_path).expect("open read pool");
    let engine = SearchEngine::new(pool);
    let included = engine.search(&SearchParams {
        query: "release workflow".to_string(),
        limit: 3,
        only_active: true,
        project_id: Some("test-project".to_string()),
        search_mode: Some("lexical".to_string()),
        min_score: 0.0,
        include_constitution: true,
        ..Default::default()
    });
    assert!(
        included
            .iter()
            .any(|result| result.layer == "identity_schema"),
        "control query should prove identity rows can dominate before exclusion: {included:?}"
    );

    let excluded = engine.search(&SearchParams {
        query: "release workflow".to_string(),
        limit: 3,
        only_active: true,
        project_id: Some("test-project".to_string()),
        search_mode: Some("lexical".to_string()),
        min_score: 0.0,
        include_constitution: false,
        ..Default::default()
    });

    assert_eq!(
        excluded.len(),
        3,
        "identity rows must not consume candidate slots before filtering: {excluded:?}"
    );
    assert!(
        excluded
            .iter()
            .all(|result| result.layer != "identity_schema"),
        "identity_schema rows must be excluded in the search engine: {excluded:?}"
    );

    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(db_path.with_extension("sqlite3-wal"));
    let _ = fs::remove_file(db_path.with_extension("sqlite3-shm"));
}

#[test]
fn explicit_identity_layer_overrides_constitution_exclusion() {
    // Regression guard for Codex PR #48 review (P1): when the caller explicitly asks for
    // `layer="identity_schema"`, the default `include_constitution=false` would previously
    // stack `layer = 'identity_schema' AND layer != 'identity_schema'` and return zero rows.
    let db_path = temp_db_path("explicit-identity");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn);
        for idx in 0..4 {
            insert_note(
                &conn,
                &format!("identity-{idx}"),
                "identity_schema",
                "person",
                1.0,
                "2026-04-14T00:00:00Z",
                &format!("identity payload {idx}"),
            );
        }
        insert_note(
            &conn,
            "fact-1",
            "verified_fact",
            "decision",
            0.7,
            "2026-04-14T00:00:00Z",
            "unrelated verified fact",
        );
    }

    let pool = DbPool::open_readonly(&db_path).expect("open read pool");
    let engine = SearchEngine::new(pool);
    let got = engine.search(&SearchParams {
        query: "identity payload".to_string(),
        limit: 5,
        only_active: true,
        project_id: Some("test-project".to_string()),
        search_mode: Some("lexical".to_string()),
        min_score: 0.0,
        layer: Some("identity_schema".to_string()),
        include_constitution: false,
        ..Default::default()
    });

    assert!(
        !got.is_empty(),
        "explicit layer=identity_schema must not be zeroed by default constitution exclusion: {got:?}"
    );
    assert!(
        got.iter().all(|r| r.layer == "identity_schema"),
        "caller asked for identity layer; result must stay on-layer: {got:?}"
    );

    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(db_path.with_extension("sqlite3-wal"));
    let _ = fs::remove_file(db_path.with_extension("sqlite3-shm"));
}
