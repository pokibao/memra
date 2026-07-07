//! Low-level writer tests for `insert_note_relation`.
//!
//! Covers ONLY the storage helper that writes a single row to `note_relations`:
//! basic insert + field round-trip, `INSERT OR IGNORE` dedup semantics,
//! `relation_type` distinction, and f64 `strength` precision.
//!
//! NOT covered here: the full auto-link pipeline in
//! `WriteOrchestrator::link_related` — similarity-window selection,
//! project / layer scoping, candidate truncation, 4-decimal rounding.
//! A separate integration test against the orchestrator is needed to
//! gate regressions in auto-link *selection* (Codex P2 on PR #179).
//!
//! Uses in-memory SQLite — no real DB required.

use memra_core::storage::writer::insert_note_relation;
use rusqlite::Connection;

/// Full schema matching production (notes + FTS + note_relations).
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
            metadata_json     TEXT,
            vector_json       TEXT,
            vector_blob       BLOB,
            evolution_state   TEXT DEFAULT 'active',
            topic_key         TEXT,
            is_head           INTEGER DEFAULT 1,
            room              TEXT,
            agent             TEXT,
            difficulty        INTEGER,
            time_cost_hint    TEXT,
            related_ids_json  TEXT,
            role              TEXT,
            source            TEXT,
            created_by        TEXT,
            version           INTEGER DEFAULT 1,
            root_id           TEXT,
            cold_storage_ref  TEXT,
            event_when        TEXT,
            event_when_ts     REAL
        );
        CREATE VIRTUAL TABLE notes_fts USING fts5(note_id UNINDEXED, content);
        CREATE TABLE IF NOT EXISTS note_relations (
            from_note_id   TEXT NOT NULL,
            to_note_id     TEXT NOT NULL,
            relation_type  TEXT NOT NULL,
            strength       REAL NOT NULL DEFAULT 0.5,
            created_at     TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (from_note_id, to_note_id, relation_type)
        );",
    )
    .expect("schema creation failed");
}

/// Helper: count rows in note_relations.
fn count_relations(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM note_relations", [], |row| row.get(0))
        .unwrap_or(0)
}

/// Helper: fetch the first relation row.
fn first_relation(conn: &Connection) -> Option<(String, String, String, f64)> {
    conn.query_row(
        "SELECT from_note_id, to_note_id, relation_type, strength FROM note_relations LIMIT 1",
        [],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, f64>(3)?,
            ))
        },
    )
    .ok()
}

/// Core assertion: insert_note_relation creates the expected row.
#[test]
fn test_insert_note_relation_creates_row() {
    let conn = Connection::open_in_memory().unwrap();
    create_schema(&conn);

    insert_note_relation(&conn, "note-a", "note-b", "supports", 0.72)
        .expect("insert_note_relation should succeed");

    assert_eq!(
        count_relations(&conn),
        1,
        "expected 1 note_relations row after insert"
    );

    let (from, to, kind, strength) = first_relation(&conn).expect("row should exist");
    assert_eq!(from, "note-a");
    assert_eq!(to, "note-b");
    assert_eq!(kind, "supports");
    assert!(
        (strength - 0.72).abs() < 1e-6,
        "strength mismatch: {strength}"
    );
}

/// Idempotency: INSERT OR IGNORE on the same PK must not duplicate.
#[test]
fn test_insert_note_relation_ignore_duplicate() {
    let conn = Connection::open_in_memory().unwrap();
    create_schema(&conn);

    insert_note_relation(&conn, "note-a", "note-b", "supports", 0.72).unwrap();
    insert_note_relation(&conn, "note-a", "note-b", "supports", 0.80).unwrap();

    assert_eq!(
        count_relations(&conn),
        1,
        "INSERT OR IGNORE should not duplicate identical PK"
    );
}

/// Two notes with different relation_type can coexist on the same (from, to) pair.
#[test]
fn test_insert_note_relation_different_type() {
    let conn = Connection::open_in_memory().unwrap();
    create_schema(&conn);

    insert_note_relation(&conn, "note-a", "note-b", "supports", 0.72).unwrap();
    insert_note_relation(&conn, "note-a", "note-b", "generalizes", 0.65).unwrap();

    assert_eq!(
        count_relations(&conn),
        2,
        "different relation_type should create two rows"
    );
}

/// Verify that the storage helper correctly stores and retrieves strength values.
#[test]
fn test_insert_note_relation_strength_precision() {
    let conn = Connection::open_in_memory().unwrap();
    create_schema(&conn);

    let strength = 0.7312;
    insert_note_relation(&conn, "source-id", "target-id", "supports", strength)
        .expect("insert should succeed");

    let stored: f64 = conn
        .query_row(
            "SELECT strength FROM note_relations WHERE from_note_id = 'source-id'",
            [],
            |row| row.get(0),
        )
        .expect("row should exist");

    assert!(
        (stored - strength).abs() < 1e-6,
        "stored strength {stored} should be close to {strength}"
    );
}
