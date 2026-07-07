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
    std::env::temp_dir().join(format!("ma-exact-self-recall-{nanos}-{label}.sqlite3"))
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

fn insert_note(conn: &Connection, id: &str, content: &str, created_at: &str) {
    conn.execute(
        "INSERT INTO notes (
            id, content, layer, category, is_active, confidence, project_id,
            created_at, updated_at, metadata_json, evolution_state, recall_count
         ) VALUES (?1, ?2, 'verified_fact', 'preference', 1, 0.95, 'memra',
                   ?3, ?3, '{}', 'active', 0)",
        rusqlite::params![id, content, created_at],
    )
    .expect("insert note");
    conn.execute(
        "INSERT INTO notes_fts (note_id, content) VALUES (?1, ?2)",
        rusqlite::params![id, content],
    )
    .expect("insert fts");
}

#[test]
fn exact_content_prefix_self_recall_survives_bm25_crowdout() {
    let db_path = temp_db_path("bm25-crowdout");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn);

        let shared_terms = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        for idx in 0..80 {
            insert_note(
                &conn,
                &format!("short-distractor-{idx:02}"),
                shared_terms,
                "2026-05-05T00:00:00Z",
            );
        }

        let exact = format!(
            "{shared_terms} durable operator preference: when a memory fact is queried \
             by its own content prefix, the exact active note must enter the Rust \
             candidate pool before BM25 short-document crowdout can hide it."
        );
        insert_note(&conn, "exact-self", &exact, "2026-04-01T00:00:00Z");
    }

    let pool = DbPool::open_readonly(&db_path).expect("open read pool");
    let engine = SearchEngine::new(pool);
    let results = engine.search(&SearchParams {
        query:
            "alpha beta gamma delta epsilon zeta eta theta iota kappa durable operator preference"
                .to_string(),
        limit: 10,
        only_active: true,
        project_id: Some("memra".to_string()),
        search_mode: Some("lexical".to_string()),
        min_score: 0.0,
        ..Default::default()
    });
    let ids: Vec<&str> = results.iter().map(|result| result.id.as_str()).collect();
    assert!(
        ids.contains(&"exact-self"),
        "exact self row must survive candidate collection; got {ids:?}"
    );

    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(db_path.with_extension("sqlite3-wal"));
    let _ = fs::remove_file(db_path.with_extension("sqlite3-shm"));
}

#[test]
fn exact_content_self_recall_uses_original_query_before_marker_stripping() {
    let db_path = temp_db_path("marker-stripping");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn);

        for idx in 0..80 {
            insert_note(
                &conn,
                &format!("marker-distractor-{idx:02}"),
                "durable marker exact fallback candidate recall",
                "2026-05-05T00:00:00Z",
            );
        }

        let exact = "durable current marker latest exact active now fallback candidate recall: \
             temporal marker stripping must not prevent literal self-recall from finding \
             the active source note.";
        insert_note(&conn, "exact-marker-self", exact, "2026-04-01T00:00:00Z");
    }

    let pool = DbPool::open_readonly(&db_path).expect("open read pool");
    let engine = SearchEngine::new(pool);
    let results = engine.search(&SearchParams {
        query: "durable current marker latest exact active now fallback candidate recall"
            .to_string(),
        limit: 10,
        only_active: true,
        project_id: Some("memra".to_string()),
        search_mode: Some("lexical".to_string()),
        min_score: 0.0,
        ..Default::default()
    });
    let ids: Vec<&str> = results.iter().map(|result| result.id.as_str()).collect();
    assert!(
        ids.contains(&"exact-marker-self"),
        "exact self row must use the original query before marker stripping; got {ids:?}"
    );

    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(db_path.with_extension("sqlite3-wal"));
    let _ = fs::remove_file(db_path.with_extension("sqlite3-shm"));
}

#[test]
fn exact_content_self_recall_handles_operational_log_punctuation() {
    let db_path = temp_db_path("operational-punctuation");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_schema(&conn);

        for idx in 0..80 {
            insert_note(
                &conn,
                &format!("openclaw-distractor-{idx:02}"),
                "[SUCCESS] OpenClaw OAuth refresh failed refresh_token_reused telegram timeline",
                "2026-05-05T00:00:00Z",
            );
        }

        let exact = "[CORRECTION] OpenClaw 诊断：err.log 满屏报错 != broken，必须先看\"功能信号时间线\"\n\n\
             trigger: openclaw 误诊, OAuth refresh failed 当作 broken, OpenClaw err log 噪音, \
             refresh_token_reused noise, telegram sendMessage 时间线, codex access_token";
        insert_note(
            &conn,
            "exact-operational-self",
            exact,
            "2026-05-02T00:00:00Z",
        );
    }

    let pool = DbPool::open_readonly(&db_path).expect("open read pool");
    let engine = SearchEngine::new(pool);
    let results = engine.search(&SearchParams {
        query:
            "[CORRECTION] OpenClaw 诊断：err.log 满屏报错 != broken，必须先看\"功能信号时间线\"\n\n\
             trigger: openclaw 误诊, OAuth refresh failed 当作 broken, OpenClaw err log 噪音, \
             refresh_token_reused noise, telegram sendMessage 时间线, codex access_token"
                .to_string(),
        limit: 10,
        only_active: true,
        project_id: Some("memra".to_string()),
        search_mode: Some("lexical".to_string()),
        min_score: 0.0,
        ..Default::default()
    });
    let ids: Vec<&str> = results.iter().map(|result| result.id.as_str()).collect();
    assert!(
        ids.contains(&"exact-operational-self"),
        "exact self row must survive punctuation, underscores, quotes, and newlines; got {ids:?}"
    );

    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(db_path.with_extension("sqlite3-wal"));
    let _ = fs::remove_file(db_path.with_extension("sqlite3-shm"));
}
