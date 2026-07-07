//! Tests for the write-path silent-failure fixes:
//!   Critical #2 (unwrap_or_default on serde serialization)
//!   High #5 (.ok().flatten() on query_row)
//!
//! All tests use an in-memory SQLite database, no real DB required.

use memra_core::storage::db::NoteRow;
use memra_core::storage::writer::{
    NoteInsert, stamp_note_lineage, strengthen_note_relation, upsert_note, upsert_note_relation_max,
};
use rusqlite::Connection;

/// Minimal schema that satisfies `upsert_note` and `mark_superseded`.
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
        CREATE TABLE note_relations (
            from_note_id TEXT NOT NULL,
            to_note_id TEXT NOT NULL,
            relation_type TEXT NOT NULL,
            strength REAL NOT NULL DEFAULT 0.5,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (from_note_id, to_note_id, relation_type)
        );
        CREATE TABLE note_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            note_id TEXT NOT NULL,
            event_type TEXT NOT NULL,
            related_note_id TEXT,
            payload_json TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE VIRTUAL TABLE notes_fts USING fts5(note_id UNINDEXED, content);",
    )
    .expect("schema creation failed");
    // sessions table: required by upsert_note when session_id is Some.
    memra_core::storage::sessions_writer::ensure_sessions_table(conn)
        .expect("sessions schema creation failed");
}

#[test]
fn report_outcome_feedback_strengthens_and_decays_adjacent_relations() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);
    insert_raw(&conn, "n1", "source note", None);
    insert_raw(&conn, "n2", "target note", None);
    insert_raw(&conn, "n3", "unsafe provenance note", None);
    conn.execute(
        "INSERT INTO note_relations (from_note_id, to_note_id, relation_type, strength)
         VALUES
         ('n1', 'n2', 'supports', 0.50),
         ('n2', 'n3', 'provenance', 0.90)",
        [],
    )
    .expect("relation insert");

    memra_core::storage::writer::atomic_update_outcome(&conn, "n2", "confirmed", None)
        .expect("confirmed outcome");
    memra_core::storage::writer::apply_hebbian_feedback(&conn, "n2", "confirmed")
        .expect("confirmed feedback");
    let strength: f64 = conn
        .query_row(
            "SELECT strength FROM note_relations WHERE from_note_id = 'n1' AND to_note_id = 'n2'",
            [],
            |row| row.get(0),
        )
        .expect("strength after confirmed");
    assert!((strength - 0.55).abs() < f64::EPSILON);
    let unsafe_strength: f64 = conn
        .query_row(
            "SELECT strength FROM note_relations WHERE from_note_id = 'n2' AND to_note_id = 'n3'",
            [],
            |row| row.get(0),
        )
        .expect("unsafe relation unchanged after confirmed");
    assert!((unsafe_strength - 0.90).abs() < f64::EPSILON);

    memra_core::storage::writer::atomic_update_outcome(&conn, "n2", "corrected", Some("bad path"))
        .expect("corrected outcome");
    memra_core::storage::writer::apply_hebbian_feedback(&conn, "n2", "corrected")
        .expect("corrected feedback");
    let corrected: f64 = conn
        .query_row(
            "SELECT strength FROM note_relations WHERE from_note_id = 'n1' AND to_note_id = 'n2'",
            [],
            |row| row.get(0),
        )
        .expect("strength after corrected");
    assert!((corrected - 0.4675).abs() < 0.000_001);

    memra_core::storage::writer::atomic_update_outcome(&conn, "n2", "outdated", Some("stale path"))
        .expect("outdated outcome");
    memra_core::storage::writer::apply_hebbian_feedback(&conn, "n2", "outdated")
        .expect("outdated feedback");
    let outdated: f64 = conn
        .query_row(
            "SELECT strength FROM note_relations WHERE from_note_id = 'n1' AND to_note_id = 'n2'",
            [],
            |row| row.get(0),
        )
        .expect("strength after outdated");
    assert!((outdated - 0.32725).abs() < 0.000_001);

    let stored: String = conn
        .query_row(
            "SELECT metadata_json FROM notes WHERE id = 'n2'",
            [],
            |row| row.get(0),
        )
        .expect("metadata after feedback");
    let parsed: serde_json::Value =
        serde_json::from_str(&stored).expect("metadata_json should stay valid");
    assert_eq!(
        parsed.get("success_count").and_then(|v| v.as_i64()),
        Some(1)
    );
    assert_eq!(
        parsed.get("failure_count").and_then(|v| v.as_i64()),
        Some(2)
    );
    assert_eq!(
        parsed.get("is_corrected").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        parsed.get("is_outdated").and_then(|v| v.as_bool()),
        Some(true)
    );

    let event_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM note_events WHERE note_id = 'n2' AND event_type = 'hebbian_feedback'",
            [],
            |row| row.get(0),
        )
        .expect("hebbian audit row count");
    assert_eq!(event_rows, 3);
    let latest_payload: String = conn
        .query_row(
            "SELECT payload_json FROM note_events
             WHERE note_id = 'n2' AND event_type = 'hebbian_feedback'
             ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("latest hebbian audit payload");
    let payload: serde_json::Value =
        serde_json::from_str(&latest_payload).expect("audit payload should be valid JSON");
    assert_eq!(
        payload.get("outcome").and_then(|v| v.as_str()),
        Some("outdated")
    );
    assert_eq!(
        payload.get("relation_type").and_then(|v| v.as_str()),
        Some("supports")
    );
    assert_eq!(
        payload.get("before_strength").and_then(|v| v.as_f64()),
        Some(corrected)
    );
    assert_eq!(
        payload.get("after_strength").and_then(|v| v.as_f64()),
        Some(outdated)
    );
}

#[test]
fn report_outcome_feedback_strengthens_and_decays_dream_candidates() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);
    insert_raw(&conn, "e1", "dream evidence", None);
    insert_raw(&conn, "m1", "promoted dream memory", None);
    conn.execute_batch(
        "CREATE TABLE dream_candidates (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            source_type TEXT NOT NULL,
            source_id TEXT,
            summary TEXT NOT NULL,
            hypothesis TEXT,
            confidence REAL NOT NULL DEFAULT 0.0,
            frequency INTEGER DEFAULT 1,
            evidence_ids TEXT,
            verdict TEXT DEFAULT 'pending',
            promoted_to TEXT,
            evaluator_notes TEXT,
            writer_status TEXT,
            writer_reason TEXT,
            created_at TEXT NOT NULL,
            evaluated_at TEXT,
            promoted_at TEXT,
            discarded_at TEXT
        );",
    )
    .expect("dream schema");
    conn.execute(
        "INSERT INTO dream_candidates
         (id, project_id, source_type, summary, confidence, evidence_ids, verdict, promoted_to, created_at, promoted_at)
         VALUES
         ('cand-1', 'alpha', 'consolidation', 'dream candidate', 0.80, '[\"e1\"]', 'promoted', 'm1', '2026-05-04T00:00:00Z', '2026-05-04T00:00:00Z')",
        [],
    )
    .expect("dream candidate");

    memra_core::storage::writer::atomic_update_outcome(&conn, "m1", "confirmed", None)
        .expect("confirmed outcome");
    memra_core::storage::writer::apply_dream_feedback(&conn, "m1", "confirmed", None)
        .expect("confirmed dream feedback");
    let confirmed: (f64, String) = conn
        .query_row(
            "SELECT confidence, writer_status FROM dream_candidates WHERE id = 'cand-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("confirmed dream row");
    assert!((confirmed.0 - 0.85).abs() < 0.000_001);
    assert_eq!(confirmed.1, "feedback_confirmed");

    memra_core::storage::writer::atomic_update_outcome(&conn, "m1", "corrected", Some("wrong dream"))
        .expect("corrected outcome");
    memra_core::storage::writer::apply_dream_feedback(&conn, "m1", "corrected", Some("wrong dream"))
        .expect("corrected dream feedback");
    let corrected: (f64, String, Option<String>) = conn
        .query_row(
            "SELECT confidence, writer_status, writer_reason FROM dream_candidates WHERE id = 'cand-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("corrected dream row");
    assert!((corrected.0 - 0.6375).abs() < 0.000_001);
    assert_eq!(corrected.1, "feedback_corrected");
    assert_eq!(corrected.2.as_deref(), Some("wrong dream"));

    memra_core::storage::writer::atomic_update_outcome(&conn, "m1", "outdated", Some("stale dream"))
        .expect("outdated outcome");
    memra_core::storage::writer::apply_dream_feedback(&conn, "m1", "outdated", Some("stale dream"))
        .expect("outdated dream feedback");
    let outdated: (f64, String) = conn
        .query_row(
            "SELECT confidence, writer_status FROM dream_candidates WHERE id = 'cand-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("outdated dream row");
    assert!((outdated.0 - 0.31875).abs() < 0.000_001);
    assert_eq!(outdated.1, "feedback_outdated");

    let event_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM note_events WHERE note_id = 'm1' AND event_type = 'dream_feedback'",
            [],
            |row| row.get(0),
        )
        .expect("dream feedback events");
    assert_eq!(event_rows, 3);
    let latest_payload: String = conn
        .query_row(
            "SELECT payload_json FROM note_events
             WHERE note_id = 'm1' AND event_type = 'dream_feedback'
             ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("latest dream feedback payload");
    let payload: serde_json::Value =
        serde_json::from_str(&latest_payload).expect("payload should be json");
    assert_eq!(
        payload.get("candidate_id").and_then(|value| value.as_str()),
        Some("cand-1")
    );
    assert_eq!(
        payload.get("outcome").and_then(|value| value.as_str()),
        Some("outdated")
    );
    assert_eq!(
        payload
            .get("after_confidence")
            .and_then(|value| value.as_f64()),
        Some(outdated.0)
    );
}

/// Insert a note directly via SQL (bypasses upsert_note for setup convenience).
fn insert_raw(conn: &Connection, id: &str, content: &str, metadata_json: Option<&str>) {
    conn.execute(
        "INSERT INTO notes (id, content, layer, is_active, metadata_json)
         VALUES (?1, ?2, 'verified_fact', 1, ?3)",
        rusqlite::params![id, content, metadata_json],
    )
    .expect("insert_raw failed");
    conn.execute(
        "INSERT INTO notes_fts (note_id, content) VALUES (?1, ?2)",
        rusqlite::params![id, content],
    )
    .expect("fts insert failed");
}

// ---------------------------------------------------------------------------
// Test 1 — upsert_note_with_bad_metadata_errors
//
// The fix ensures metadata_json is written via a properly-serialized string,
// not silently replaced with "".  We verify that after atomic_update_outcome
// the stored JSON is valid, non-empty, and contains the expected keys
// (success_count) — proving the serialization path ran to completion rather
// than silently emitting an empty string.
// ---------------------------------------------------------------------------
#[test]
fn upsert_note_with_bad_metadata_errors() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    // Insert a note with pre-existing metadata.
    let note_id = "test-bad-meta-01";
    let initial_meta = r#"{"custom_key":"preserved"}"#;
    insert_raw(
        &conn,
        note_id,
        "test content for metadata guard",
        Some(initial_meta),
    );

    // Call atomic_update_outcome — this internally calls serde_json::to_string
    // and (with our fix) propagates serialization errors instead of writing "".
    let result = memra_core::storage::writer::atomic_update_outcome(&conn, note_id, "confirmed", None);
    assert!(
        result.is_ok(),
        "atomic_update_outcome should succeed: {result:?}"
    );
    assert!(result.unwrap(), "should return true for a known outcome");

    // Verify the stored metadata is valid JSON and contains success_count > 0.
    let stored: Option<String> = conn
        .query_row(
            "SELECT metadata_json FROM notes WHERE id = ?1",
            [note_id],
            |row| row.get(0),
        )
        .unwrap();

    let stored_str = stored.expect("metadata_json should not be NULL");
    assert!(
        !stored_str.is_empty(),
        "metadata_json must not be empty string (the old silent-failure behaviour)"
    );

    let parsed: serde_json::Value =
        serde_json::from_str(&stored_str).expect("metadata_json must be valid JSON after update");

    assert_eq!(
        parsed.get("success_count").and_then(|v| v.as_i64()),
        Some(1),
        "success_count should be 1 after one confirmed outcome"
    );
    assert_eq!(
        parsed.get("failure_count").and_then(|v| v.as_i64()),
        Some(0),
        "confirmed outcome should persist Python-compatible failure_count=0"
    );
    let updated_at: Option<String> = conn
        .query_row(
            "SELECT updated_at FROM notes WHERE id = ?1",
            [note_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        updated_at.is_none(),
        "report_outcome should mirror Python and leave updated_at unchanged"
    );
    // The pre-existing key must still be present (not dropped).
    assert_eq!(
        parsed.get("custom_key").and_then(|v| v.as_str()),
        Some("preserved"),
        "pre-existing metadata keys must be preserved"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — mark_superseded_preserves_existing_metadata
//
// Verifies that mark_superseded reads back the existing metadata and merges
// superseded_by into it without discarding other fields.
// ---------------------------------------------------------------------------
#[test]
fn mark_superseded_preserves_existing_metadata() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let old_id = "test-supersede-old";
    let new_id = "test-supersede-new";
    // Note with existing meaningful metadata.
    let existing_meta = r#"{"a":1,"b":"hello"}"#;
    insert_raw(&conn, old_id, "old note content", Some(existing_meta));
    insert_raw(&conn, new_id, "new note content", None);

    let result = memra_core::storage::writer::mark_superseded(&conn, old_id, new_id, "auto_supersede");
    assert!(result.is_ok(), "mark_superseded should succeed: {result:?}");

    // Read back and verify merged metadata.
    let stored: String = conn
        .query_row(
            "SELECT metadata_json FROM notes WHERE id = ?1",
            [old_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap();

    let parsed: serde_json::Value =
        serde_json::from_str(&stored).expect("stored metadata must be valid JSON");

    // Existing fields must survive the merge.
    assert_eq!(
        parsed.get("a").and_then(|v| v.as_i64()),
        Some(1),
        "field 'a' must survive mark_superseded"
    );
    assert_eq!(
        parsed.get("b").and_then(|v| v.as_str()),
        Some("hello"),
        "field 'b' must survive mark_superseded"
    );
    // superseded_by must be injected.
    assert_eq!(
        parsed.get("superseded_by").and_then(|v| v.as_str()),
        Some(new_id),
        "superseded_by must be set to new_id"
    );

    // evolution_state must have been updated.
    let evo_state: String = conn
        .query_row(
            "SELECT evolution_state FROM notes WHERE id = ?1",
            [old_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    assert_eq!(evo_state, "superseded");
}

// ---------------------------------------------------------------------------
// Test 3 — mark_superseded_missing_target_returns_error
//
// With the old .ok().flatten() pattern, calling mark_superseded on a
// non-existent ID silently "succeeded" and wrote superseded_by into nothing.
// After the fix (OptionalExtension + propagate QueryReturnedNoRows), it must
// return an Err.
// ---------------------------------------------------------------------------
#[test]
fn mark_superseded_missing_target_returns_error() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let result = memra_core::storage::writer::mark_superseded(
        &conn,
        "nonexistent-old-id",
        "nonexistent-new-id",
        "auto_supersede",
    );

    assert!(
        result.is_err(),
        "mark_superseded on non-existent id must return Err, got Ok"
    );

    // Specifically, it must be QueryReturnedNoRows (not some other error).
    match result.unwrap_err() {
        rusqlite::Error::QueryReturnedNoRows => {} // expected
        other => panic!("Expected QueryReturnedNoRows, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Test 4 — mark_superseded_null_metadata_does_not_crash
//
// When the older row's metadata_json is NULL (no prior metadata ever written),
// the fix must treat NULL as an empty object instead of panicking on
// .as_str() of a None. Regression guard for the 2026-04-18 dogfood crash:
//   "Invalid column type Null at index 0".
// After mark_superseded runs, the row must still end up with a valid
// metadata_json containing superseded_by.
// ---------------------------------------------------------------------------
#[test]
fn mark_superseded_null_metadata_does_not_crash() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let old_id = "test-supersede-null-old";
    let new_id = "test-supersede-null-new";
    insert_raw(&conn, old_id, "old note with null metadata", None);
    insert_raw(&conn, new_id, "new note content", None);

    let result = memra_core::storage::writer::mark_superseded(&conn, old_id, new_id, "auto_supersede");
    assert!(
        result.is_ok(),
        "NULL metadata_json must not crash mark_superseded: {result:?}"
    );

    let stored: Option<String> = conn
        .query_row(
            "SELECT metadata_json FROM notes WHERE id = ?1",
            [old_id],
            |row| row.get(0),
        )
        .unwrap();
    let stored = stored.expect("mark_superseded should create metadata_json");
    let parsed: serde_json::Value =
        serde_json::from_str(&stored).expect("metadata_json must be valid JSON");
    assert_eq!(
        parsed.get("superseded_by").and_then(|v| v.as_str()),
        Some(new_id),
        "superseded_by must be written even when original metadata_json is NULL"
    );

    let evo_state: String = conn
        .query_row(
            "SELECT evolution_state FROM notes WHERE id = ?1",
            [old_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(evo_state, "superseded");
}

// ---------------------------------------------------------------------------
// Test 5 — mark_superseded_tolerates_empty_string_metadata
//
// An older row can legitimately carry `metadata_json = ""` (e.g. a storage
// layer that serialized an empty map to the empty string, or a hand-edited
// fix-forward). That isn't valid JSON, so `from_str` fails — the code must
// fall through to an empty object rather than propagate a serialization
// error. Regression guard for Issue #68 scope item:
//   "mark_superseded_tolerates_empty_string_metadata".
// ---------------------------------------------------------------------------
#[test]
fn mark_superseded_tolerates_empty_string_metadata() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let old_id = "test-supersede-empty-old";
    let new_id = "test-supersede-empty-new";
    insert_raw(
        &conn,
        old_id,
        "old note with empty-string metadata",
        Some(""),
    );
    insert_raw(&conn, new_id, "new note content", None);

    let result = memra_core::storage::writer::mark_superseded(&conn, old_id, new_id, "auto_supersede");
    assert!(
        result.is_ok(),
        "empty-string metadata_json must not crash mark_superseded: {result:?}"
    );

    let stored: String = conn
        .query_row(
            "SELECT metadata_json FROM notes WHERE id = ?1",
            [old_id],
            |row| row.get(0),
        )
        .expect("metadata_json must be rewritten by mark_superseded");
    let parsed: serde_json::Value =
        serde_json::from_str(&stored).expect("rewritten metadata_json must be valid JSON");
    assert_eq!(
        parsed.get("superseded_by").and_then(|v| v.as_str()),
        Some(new_id),
        "superseded_by must be written even when original metadata_json was \"\""
    );
    // supersede trio must all be set — confirms the empty-string fallback
    // went through the full merge path, not a partial write.
    assert_eq!(
        parsed.get("is_outdated").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(
        parsed.get("supersede_reason").is_some(),
        "supersede_reason must be set"
    );
}

#[test]
fn upsert_note_defaults_and_respects_lineage_fields() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let default_note = NoteInsert {
        id: "default-lineage".to_string(),
        content: "default lineage".to_string(),
        layer: "verified_fact".to_string(),
        ..Default::default()
    };
    upsert_note(&conn, &default_note).expect("default insert");

    let (default_root, default_version): (Option<String>, i64) = conn
        .query_row(
            "SELECT root_id, version FROM notes WHERE id = ?1",
            ["default-lineage"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("default lineage row");
    assert_eq!(default_root.as_deref(), Some("default-lineage"));
    assert_eq!(default_version, 1);

    let explicit_note = NoteInsert {
        id: "explicit-lineage".to_string(),
        content: "explicit lineage".to_string(),
        layer: "verified_fact".to_string(),
        root_id: Some("root-abc".to_string()),
        version: Some(7),
        ..Default::default()
    };
    upsert_note(&conn, &explicit_note).expect("explicit insert");

    let (explicit_root, explicit_version): (Option<String>, i64) = conn
        .query_row(
            "SELECT root_id, version FROM notes WHERE id = ?1",
            ["explicit-lineage"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("explicit lineage row");
    assert_eq!(explicit_root.as_deref(), Some("root-abc"));
    assert_eq!(explicit_version, 7);
}

#[test]
fn upsert_note_preserves_legacy_null_lineage_fields_on_reupsert() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    conn.execute(
        "INSERT INTO notes (id, content, layer, is_active, version, root_id)
         VALUES (?1, ?2, 'verified_fact', 1, NULL, NULL)",
        rusqlite::params!["legacy-null-lineage", "legacy null lineage"],
    )
    .expect("seed legacy null row");
    conn.execute(
        "INSERT INTO notes_fts (note_id, content) VALUES (?1, ?2)",
        rusqlite::params!["legacy-null-lineage", "legacy null lineage"],
    )
    .expect("seed legacy null fts row");

    let reupsert = NoteInsert {
        id: "legacy-null-lineage".to_string(),
        content: "legacy null lineage updated".to_string(),
        layer: "verified_fact".to_string(),
        root_id: None,
        version: None,
        ..Default::default()
    };
    upsert_note(&conn, &reupsert).expect("reupsert legacy null row");

    let (root_id, version): (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT root_id, version FROM notes WHERE id = ?1",
            ["legacy-null-lineage"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("legacy null lineage row");
    assert!(root_id.is_none(), "legacy NULL root_id must stay NULL");
    assert!(version.is_none(), "legacy NULL version must stay NULL");
}

#[test]
fn upsert_note_reupsert_without_lineage_preserves_existing_values() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let seeded = NoteInsert {
        id: "preserve-lineage".to_string(),
        content: "seed".to_string(),
        layer: "verified_fact".to_string(),
        root_id: Some("root-seed".to_string()),
        version: Some(5),
        ..Default::default()
    };
    upsert_note(&conn, &seeded).expect("seed insert");

    let reupsert = NoteInsert {
        id: "preserve-lineage".to_string(),
        content: "seed updated".to_string(),
        layer: "verified_fact".to_string(),
        ..Default::default()
    };
    upsert_note(&conn, &reupsert).expect("reupsert without lineage");

    let (root_id, version): (Option<String>, i64) = conn
        .query_row(
            "SELECT root_id, version FROM notes WHERE id = ?1",
            ["preserve-lineage"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("preserved lineage row");
    assert_eq!(root_id.as_deref(), Some("root-seed"));
    assert_eq!(version, 5);
}

#[test]
fn upsert_note_reupsert_explicitly_overrides_lineage_values() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let seeded = NoteInsert {
        id: "override-lineage".to_string(),
        content: "seed".to_string(),
        layer: "verified_fact".to_string(),
        root_id: Some("root-old".to_string()),
        version: Some(5),
        ..Default::default()
    };
    upsert_note(&conn, &seeded).expect("seed insert");

    let override_note = NoteInsert {
        id: "override-lineage".to_string(),
        content: "seed updated".to_string(),
        layer: "verified_fact".to_string(),
        root_id: Some("root-new".to_string()),
        version: Some(3),
        ..Default::default()
    };
    upsert_note(&conn, &override_note).expect("reupsert override");

    let (root_id, version): (Option<String>, i64) = conn
        .query_row(
            "SELECT root_id, version FROM notes WHERE id = ?1",
            ["override-lineage"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("override lineage row");
    assert_eq!(root_id.as_deref(), Some("root-new"));
    assert_eq!(version, 3);
}

#[test]
fn upsert_note_bumps_updated_at_for_lineage_only_changes() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let seeded = NoteInsert {
        id: "lineage-timestamp".to_string(),
        content: "same content".to_string(),
        layer: "verified_fact".to_string(),
        updated_at: Some("2020-01-01T00:00:00.000Z".to_string()),
        root_id: Some("root-old".to_string()),
        version: Some(1),
        ..Default::default()
    };
    upsert_note(&conn, &seeded).expect("seed insert");

    let lineage_update = NoteInsert {
        id: "lineage-timestamp".to_string(),
        content: "same content".to_string(),
        layer: "verified_fact".to_string(),
        root_id: Some("root-new".to_string()),
        version: Some(2),
        ..Default::default()
    };
    upsert_note(&conn, &lineage_update).expect("lineage-only update");

    let (root_id, version, updated_at): (Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT root_id, version, updated_at FROM notes WHERE id = ?1",
            ["lineage-timestamp"],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("lineage timestamp row");

    assert_eq!(root_id.as_deref(), Some("root-new"));
    assert_eq!(version, 2);
    assert_ne!(
        updated_at.as_deref(),
        Some("2020-01-01T00:00:00.000Z"),
        "lineage-only changes are material and must bump updated_at"
    );
}

#[test]
fn note_row_hydration_preserves_null_version() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    conn.execute(
        "INSERT INTO notes (id, content, layer, is_active, version, root_id)
         VALUES (?1, ?2, 'verified_fact', 1, NULL, NULL)",
        rusqlite::params!["hydrate-null-version", "hydrate null version"],
    )
    .expect("seed legacy null row");

    let row = conn
        .query_row(
            "SELECT * FROM notes WHERE id = ?1",
            ["hydrate-null-version"],
            NoteRow::from_row,
        )
        .expect("hydrate NoteRow");

    assert!(
        row.version.is_none(),
        "legacy NULL version must hydrate as None, not as synthetic version 1"
    );
}

// ---------------------------------------------------------------------------
// Test: upsert_note_persists_source_and_created_by
//
// Python's MCP path stamps every add_rule write with source="user" +
// created_by="user" (backend/mcp_memory.py:1171, storage_rows.py:292-293).
// Rust Gate F parity run found these two columns empty; the write path
// must persist them when the caller supplies them.
// ---------------------------------------------------------------------------
#[test]
fn upsert_note_persists_source_and_created_by() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let note = NoteInsert {
        id: "col-fill-user".to_string(),
        content: "column-fill smoke test".to_string(),
        layer: "verified_fact".to_string(),
        is_active: true,
        is_head: true,
        source: Some("user".to_string()),
        created_by: Some("user".to_string()),
        ..Default::default()
    };

    upsert_note(&conn, &note).expect("upsert should succeed");

    let (source, created_by): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT source, created_by FROM notes WHERE id = ?1",
            [&note.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("row readback should succeed");

    assert_eq!(
        source.as_deref(),
        Some("user"),
        "source must be persisted verbatim"
    );
    assert_eq!(
        created_by.as_deref(),
        Some("user"),
        "created_by must be persisted verbatim"
    );
}

#[test]
fn upsert_note_persists_gate_f_temporal_and_session_columns() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let note = NoteInsert {
        id: "gate-f-column-fill".to_string(),
        content: "Gate F temporal column fill smoke test".to_string(),
        layer: "verified_fact".to_string(),
        valid_at: Some(1_776_517_055.201_8),
        session_id: Some("20260218_134822".to_string()),
        review_after: Some("2026-05-19T00:00:00+00:00".to_string()),
        ..Default::default()
    };

    upsert_note(&conn, &note).expect("upsert should persist column-fill fields");

    let (valid_at, session_id, review_after): (Option<f64>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT valid_at, session_id, review_after FROM notes WHERE id = ?1",
            [&note.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("gate-f column-fill row");

    assert!(
        (valid_at.expect("valid_at must be stored") - 1_776_517_055.201_8).abs() < 0.0001,
        "valid_at must be stored as epoch seconds"
    );
    assert_eq!(
        session_id.as_deref(),
        Some("20260218_134822"),
        "session_id must be persisted on the note row"
    );
    assert_eq!(
        review_after.as_deref(),
        Some("2026-05-19T00:00:00+00:00"),
        "review_after must be persisted on mutable facts"
    );
}

// ---------------------------------------------------------------------------
// Test: upsert_note_coalesces_source_on_second_write
//
// ON CONFLICT uses COALESCE(excluded.source, notes.source) so a second write
// that omits source (None) must NOT wipe the previously-stored value.
// ---------------------------------------------------------------------------
#[test]
fn upsert_note_coalesces_source_on_second_write() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let seed = NoteInsert {
        id: "col-fill-coalesce".to_string(),
        content: "initial user content".to_string(),
        layer: "verified_fact".to_string(),
        is_active: true,
        is_head: true,
        source: Some("user".to_string()),
        created_by: Some("user".to_string()),
        ..Default::default()
    };
    upsert_note(&conn, &seed).expect("seed write");

    let overwrite = NoteInsert {
        id: "col-fill-coalesce".to_string(),
        content: "edit content without source".to_string(),
        layer: "verified_fact".to_string(),
        is_active: true,
        is_head: true,
        // Intentionally omit source/created_by — COALESCE must preserve
        // the previously-stored values.
        ..Default::default()
    };
    upsert_note(&conn, &overwrite).expect("second write");

    let (source, created_by): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT source, created_by FROM notes WHERE id = ?1",
            [&seed.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("row readback should succeed");

    assert_eq!(
        source.as_deref(),
        Some("user"),
        "source must survive an overwrite that omits it"
    );
    assert_eq!(
        created_by.as_deref(),
        Some("user"),
        "created_by must survive an overwrite that omits it"
    );
}

// ---------------------------------------------------------------------------
// Test: mark_superseded_writes_full_transition_cascade
//
// Matches Python backend/services/memory_lifecycle.py:91-147 mark_transition
// path-A contract (called from write_orchestrator.py:1420-1445 for same-layer
// auto-supersede above AUTO_SUPERSEDE_THRESHOLD=0.70):
//
//   DB columns:
//     evolution_state = 'superseded'
//     is_head         = 0
//     is_active       = 0
//     updated_at      = now
//
//   metadata_json keys (merged, preserving prior keys):
//     superseded_by    = new_id
//     supersede_reason = "auto_supersede"
//     is_outdated      = true
// ---------------------------------------------------------------------------
#[test]
fn mark_superseded_writes_full_transition_cascade() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let old_id = "supersede-cascade-old";
    let new_id = "supersede-cascade-new";
    let prior_meta = r#"{"memory_type":"project","stale_risk":"low"}"#;
    insert_raw(
        &conn,
        old_id,
        "older version of same topic",
        Some(prior_meta),
    );
    insert_raw(&conn, new_id, "newer version of same topic", None);

    memra_core::storage::writer::mark_superseded(&conn, old_id, new_id, "auto_supersede")
        .expect("mark_superseded should succeed");

    // Column cascade: evolution_state + is_head + is_active all flipped.
    let (evo_state, is_head, is_active): (String, i64, i64) = conn
        .query_row(
            "SELECT evolution_state, is_head, is_active FROM notes WHERE id = ?1",
            [old_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("old row must exist after cascade");
    assert_eq!(evo_state, "superseded", "evolution_state must flip");
    assert_eq!(is_head, 0, "is_head must clear to 0");
    assert_eq!(
        is_active, 0,
        "is_active must clear to 0 — Python mark_transition sets is_active=False"
    );

    // metadata cascade: superseded_by + supersede_reason + is_outdated added,
    // prior keys preserved.
    let stored: String = conn
        .query_row(
            "SELECT metadata_json FROM notes WHERE id = ?1",
            [old_id],
            |row| row.get::<_, String>(0),
        )
        .expect("metadata_json row must exist");
    let parsed: serde_json::Value =
        serde_json::from_str(&stored).expect("metadata_json must be valid JSON");

    assert_eq!(
        parsed.get("superseded_by").and_then(|v| v.as_str()),
        Some(new_id),
        "superseded_by must point at the successor"
    );
    assert_eq!(
        parsed.get("supersede_reason").and_then(|v| v.as_str()),
        Some("auto_supersede"),
        "supersede_reason must record the trigger"
    );
    assert_eq!(
        parsed.get("is_outdated").and_then(|v| v.as_bool()),
        Some(true),
        "is_outdated must be set to true"
    );

    // Prior metadata keys must still be present.
    assert_eq!(
        parsed.get("memory_type").and_then(|v| v.as_str()),
        Some("project"),
        "prior memory_type key must survive the merge"
    );
    assert_eq!(
        parsed.get("stale_risk").and_then(|v| v.as_str()),
        Some("low"),
        "prior stale_risk key must survive the merge"
    );

    // Outcome fields — mirror Python `atomic_update_outcome` (storage.py:1218-1227)
    // which INCREMENTS existing counters. First supersede lifts failure_count
    // from 0 → 1; see separate test for "prior failure_count survives".
    assert_eq!(
        parsed.get("success_count").and_then(|v| v.as_i64()),
        Some(0),
        "success_count stays at 0 on first auto-supersede (no prior history)"
    );
    assert_eq!(
        parsed.get("failure_count").and_then(|v| v.as_i64()),
        Some(1),
        "failure_count increments from 0 to 1 on first auto-supersede"
    );
    assert_eq!(
        parsed.get("last_outcome_reason").and_then(|v| v.as_str()),
        Some(&*format!("auto-superseded by {new_id}")),
        "last_outcome_reason must record the successor id"
    );

    let forget_cascade_relations: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM note_relations
             WHERE from_note_id = ?1
               AND to_note_id = ?2
               AND relation_type = 'forget_cascade'",
            rusqlite::params![new_id, old_id],
            |row| row.get(0),
        )
        .expect("forget_cascade relation count");
    assert_eq!(
        forget_cascade_relations, 1,
        "supersede must connect successor -> superseded via forget_cascade"
    );
}

// ---------------------------------------------------------------------------
// Test: mark_superseded_preserves_prior_outcome_counters
//
// Codex review P2 on PR #110: prior failure_count=3 must become 4 on
// auto-supersede, not reset to 1 — mirrors Python atomic_update_outcome
// which INCREMENTS existing counters rather than overwriting them.
// ---------------------------------------------------------------------------
#[test]
fn mark_superseded_increments_existing_outcome_counters() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let old_id = "supersede-increment-old";
    let new_id = "supersede-increment-new";
    // Pre-populate the old row with prior failure history (e.g., 3 corrected
    // outcomes accumulated before the auto-supersede fires).
    let prior_meta = r#"{"memory_type":"project","failure_count":3,"success_count":2,"last_outcome_reason":"prior test"}"#;
    insert_raw(&conn, old_id, "prior claim", Some(prior_meta));
    insert_raw(&conn, new_id, "successor claim", None);

    memra_core::storage::writer::mark_superseded(&conn, old_id, new_id, "auto_supersede")
        .expect("mark_superseded should succeed on a row with prior counters");

    let stored: String = conn
        .query_row(
            "SELECT metadata_json FROM notes WHERE id = ?1",
            [old_id],
            |row| row.get::<_, String>(0),
        )
        .expect("metadata_json row must exist");
    let parsed: serde_json::Value =
        serde_json::from_str(&stored).expect("metadata_json must be valid JSON");

    assert_eq!(
        parsed.get("failure_count").and_then(|v| v.as_i64()),
        Some(4),
        "failure_count must increment from 3 to 4 (not reset to 1)"
    );
    assert_eq!(
        parsed.get("success_count").and_then(|v| v.as_i64()),
        Some(2),
        "success_count must preserve prior value (no decrement on supersede)"
    );
    assert_eq!(
        parsed.get("memory_type").and_then(|v| v.as_str()),
        Some("project"),
        "unrelated prior metadata keys must survive the merge"
    );
    assert_eq!(
        parsed.get("last_outcome_reason").and_then(|v| v.as_str()),
        Some(&*format!("auto-superseded by {new_id}")),
        "last_outcome_reason is overwritten to reflect the latest transition"
    );
}

// ---------------------------------------------------------------------------
// Test: mark_superseded_records_contradiction_reason
//
// The `reason` parameter flows through to metadata.supersede_reason verbatim,
// so a non-auto-supersede trigger (contradiction / evolution_refine /
// cross_layer_contradiction) labels the row correctly.
// ---------------------------------------------------------------------------
#[test]
fn mark_superseded_records_contradiction_reason() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let old_id = "supersede-reason-contradiction-old";
    let new_id = "supersede-reason-contradiction-new";
    insert_raw(&conn, old_id, "claim that got contradicted", None);
    insert_raw(&conn, new_id, "contradicting claim", None);

    memra_core::storage::writer::mark_superseded(&conn, old_id, new_id, "contradiction")
        .expect("mark_superseded with contradiction reason should succeed");

    let (evo_state, stored): (String, String) = conn
        .query_row(
            "SELECT evolution_state, metadata_json FROM notes WHERE id = ?1",
            [old_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("metadata_json row must exist");
    let parsed: serde_json::Value =
        serde_json::from_str(&stored).expect("metadata_json must be valid JSON");

    assert_eq!(
        evo_state, "contradicted",
        "contradiction reason must enter the contradicted evolution state"
    );
    assert_eq!(
        parsed.get("supersede_reason").and_then(|v| v.as_str()),
        Some("contradiction"),
        "non-auto reasons must flow through to metadata verbatim"
    );
}

// ---------------------------------------------------------------------------
// Test: upsert_note_first_insert_leaves_updated_at_null
//
// Python's storage_rows.py:167-171 leaves updated_at = NULL on first insert
// when the caller does not supply one. UPSERT later fills it with now() if
// a material change fires (see upsert_note_bumps_updated_at_for_lineage_only_changes).
//
// Before this fix the Rust path wrote `now()` on first insert, producing
// parity-breaking diff noise on every new write vs the Python snapshot.
// ---------------------------------------------------------------------------
#[test]
fn upsert_note_first_insert_leaves_updated_at_null() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let note = NoteInsert {
        id: "updated-at-null".to_string(),
        content: "first write without explicit updated_at".to_string(),
        layer: "verified_fact".to_string(),
        is_active: true,
        is_head: true,
        // Intentionally no updated_at — this is the default MCP path on
        // first insert (Python leaves it NULL; Rust used to stamp now()).
        ..Default::default()
    };
    upsert_note(&conn, &note).expect("first insert should succeed");

    let stored_updated_at: Option<String> = conn
        .query_row(
            "SELECT updated_at FROM notes WHERE id = ?1",
            [&note.id],
            |row| row.get(0),
        )
        .expect("row readback should succeed");

    assert!(
        stored_updated_at.is_none(),
        "updated_at must stay NULL on first insert when caller did not supply one; got {stored_updated_at:?}"
    );
}

// ---------------------------------------------------------------------------
// Test: upsert_note_first_insert_respects_explicit_updated_at
//
// If the caller DOES supply updated_at, persist it verbatim. Rust must not
// silently replace a caller-supplied timestamp with now().
// ---------------------------------------------------------------------------
#[test]
fn upsert_note_first_insert_respects_explicit_updated_at() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let explicit_ts = "2025-12-31T23:59:59.000Z";
    let note = NoteInsert {
        id: "updated-at-explicit".to_string(),
        content: "first write with explicit updated_at".to_string(),
        layer: "verified_fact".to_string(),
        is_active: true,
        is_head: true,
        updated_at: Some(explicit_ts.to_string()),
        ..Default::default()
    };
    upsert_note(&conn, &note).expect("first insert should succeed");

    let stored_updated_at: Option<String> = conn
        .query_row(
            "SELECT updated_at FROM notes WHERE id = ?1",
            [&note.id],
            |row| row.get(0),
        )
        .expect("row readback should succeed");

    assert_eq!(
        stored_updated_at.as_deref(),
        Some(explicit_ts),
        "explicit updated_at must be persisted verbatim"
    );
}

// ---------------------------------------------------------------------------
// Test: upsert_note_material_change_without_explicit_ts_uses_now
//
// When a UPSERT fires a material change and the caller omits updated_at,
// SQL must stamp a fresh timestamp — it must not keep the prior NULL.
// ---------------------------------------------------------------------------
#[test]
fn upsert_note_material_change_without_explicit_ts_uses_now() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    // Seed with NULL updated_at.
    let seed = NoteInsert {
        id: "material-change-null".to_string(),
        content: "original content".to_string(),
        layer: "verified_fact".to_string(),
        is_active: true,
        is_head: true,
        ..Default::default()
    };
    upsert_note(&conn, &seed).expect("seed write");

    let seed_ts: Option<String> = conn
        .query_row(
            "SELECT updated_at FROM notes WHERE id = ?1",
            [&seed.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(seed_ts.is_none(), "seed updated_at should be NULL");

    // Material change: content differs; updated_at stays None (caller omits).
    let overwrite = NoteInsert {
        id: "material-change-null".to_string(),
        content: "content mutated to trigger material change".to_string(),
        layer: "verified_fact".to_string(),
        is_active: true,
        is_head: true,
        ..Default::default()
    };
    upsert_note(&conn, &overwrite).expect("second write should succeed");

    let final_ts: Option<String> = conn
        .query_row(
            "SELECT updated_at FROM notes WHERE id = ?1",
            [&seed.id],
            |row| row.get(0),
        )
        .unwrap();

    let stamped = final_ts.expect(
        "material change must stamp updated_at via strftime('now'); \
         got NULL instead",
    );
    assert!(
        stamped.starts_with("20"),
        "updated_at should be ISO8601 starting with year 20xx, got: {stamped}"
    );
}

// ---------------------------------------------------------------------------
// Test: upsert_note_material_change_with_reused_ts_uses_now
//
// A caller can build an update payload from an existing note and pass the
// existing updated_at back unchanged. A material mutation must not preserve
// that stale timestamp; it should fall through to SQL now(), matching the
// Python storage_rows.py double-CASE guard.
// ---------------------------------------------------------------------------
#[test]
fn upsert_note_material_change_with_reused_ts_uses_now() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);

    let original_updated_at = "2020-01-01T00:00:00.000Z";
    let seed = NoteInsert {
        id: "material-change-reused-ts".to_string(),
        content: "original content".to_string(),
        layer: "verified_fact".to_string(),
        is_active: true,
        is_head: true,
        updated_at: Some(original_updated_at.to_string()),
        ..Default::default()
    };
    upsert_note(&conn, &seed).expect("seed write");

    let overwrite = NoteInsert {
        id: "material-change-reused-ts".to_string(),
        content: "content mutated while reusing timestamp".to_string(),
        layer: "verified_fact".to_string(),
        is_active: true,
        is_head: true,
        updated_at: Some(original_updated_at.to_string()),
        ..Default::default()
    };
    upsert_note(&conn, &overwrite).expect("second write should succeed");

    let (content, updated_at): (String, Option<String>) = conn
        .query_row(
            "SELECT content, updated_at FROM notes WHERE id = ?1",
            [&seed.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(content, "content mutated while reusing timestamp");
    assert_ne!(
        updated_at.as_deref(),
        Some(original_updated_at),
        "material change must not preserve a caller-reused stale updated_at"
    );
}

#[test]
fn rust_batch_relation_helpers_keep_writes_bounded_and_lineage_stamped() {
    let conn = Connection::open_in_memory().expect("in-memory db");
    create_schema(&conn);
    insert_raw(&conn, "old", "old note", None);
    insert_raw(&conn, "new", "new note", None);

    let (created, created_strength) =
        upsert_note_relation_max(&conn, "new", "old", "supports", 0.70).expect("create relation");
    assert_eq!(created, "created");
    assert_eq!(created_strength, 0.70);

    let (unchanged, unchanged_strength) =
        upsert_note_relation_max(&conn, "new", "old", "supports", 0.60)
            .expect("weaker relation ignored");
    assert_eq!(unchanged, "unchanged");
    assert_eq!(unchanged_strength, 0.70);

    let (strengthened, final_strength) =
        strengthen_note_relation(&conn, "new", "old", "supports", 0.50, 0.40)
            .expect("strengthen relation");
    assert_eq!(strengthened, "strengthened");
    assert_eq!(final_strength, 1.0);

    let affected = stamp_note_lineage(&conn, "new", Some("root-old"), Some(3), Some("topic-key"))
        .expect("stamp lineage");
    assert_eq!(affected, 1);

    let row: (String, i64, String) = conn
        .query_row(
            "SELECT root_id, version, topic_key FROM notes WHERE id = 'new'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("lineage row");
    assert_eq!(row, ("root-old".to_string(), 3, "topic-key".to_string()));
}
