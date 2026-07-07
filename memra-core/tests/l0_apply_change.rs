//! Integration tests for W3-B: L0 `apply_change_inner` (Critical #4).
//!
//! Verifies that the 3rd approval writes an `identity_schema` row to the
//! `notes` table within the same transaction, and that failures roll back.

use memra_core::governance::GovernanceService;
use memra_core::storage::db::DbPool;
use rusqlite::Connection;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Build an in-memory GovernanceService with the full schema needed by
/// `apply_change_inner` (notes + notes_fts + constitution_changes).
fn make_pool() -> DbPool {
    let pool = DbPool::open(std::path::Path::new(":memory:")).unwrap();
    pool.with_conn(|conn| {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS notes (
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
            CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts
                USING fts5(note_id UNINDEXED, content);",
        )
        .expect("notes schema creation failed");
    });
    pool
}

fn make_service() -> GovernanceService {
    let pool = make_pool();
    let svc = GovernanceService::new(pool);
    svc.init_table();
    svc
}

fn make_service_with_project(project_id: &str) -> GovernanceService {
    let pool = make_pool();
    let svc = GovernanceService::with_project(pool, project_id);
    svc.init_table();
    svc
}

/// Query a single (content, is_active, metadata_json, evolution_state) tuple
/// from the `notes` table by id.  Returns None when not found.
fn fetch_note(pool: &DbPool, id: &str) -> Option<(String, bool, Option<String>, Option<String>)> {
    pool.with_conn(|conn| {
        conn.query_row(
            "SELECT content, is_active, metadata_json, evolution_state
             FROM notes WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1).map(|v| v != 0)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .ok()
    })
}

/// Query the first identity_schema row whose content matches.  Returns (id,
/// is_active, metadata_json).
fn find_identity_row(pool: &DbPool, content: &str) -> Option<(String, bool, Option<String>)> {
    pool.with_conn(|conn| {
        conn.query_row(
            "SELECT id, is_active, metadata_json
             FROM notes WHERE layer = 'identity_schema' AND content = ?1
             LIMIT 1",
            [content],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1).map(|v| v != 0)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .ok()
    })
}

/// Read the status of a constitution_change record.
fn change_status(pool: &DbPool, id: &str) -> String {
    pool.with_conn(|conn| {
        conn.query_row(
            "SELECT status FROM constitution_changes WHERE id = ?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "not_found".to_string())
    })
}

// ── test 1 ───────────────────────────────────────────────────────────────────

/// Propose a `create` change and give it 3 distinct approvals.
/// After the 3rd approval:
/// - A row must exist in `notes` with `layer='identity_schema'` and the
///   proposed content.
/// - `metadata_json` must contain `applied_by` and `change_id` keys.
/// - `constitution_changes.status` must be `'applied'`.
/// - The returned `ChangeRecord` has `status = "applied"` and a non-None
///   `applied_at`.
#[test]
fn create_then_3_approvals_writes_identity_row() {
    let svc = make_service();

    let content = "I am a memory anchor for AI assistants.";
    let record = svc
        .propose_change(content, "initial identity", None, None, None, None)
        .unwrap();
    assert_eq!(record.status, "pending");

    svc.approve_change_for_principal(&record.id, "actor-alice", Some("alice"), None)
        .unwrap();
    svc.approve_change_for_principal(&record.id, "actor-bob", Some("bob"), None)
        .unwrap();
    let final_rec = svc
        .approve_change_for_principal(&record.id, "actor-charlie", Some("charlie"), None)
        .unwrap();

    // Returned record must be 'applied'.
    assert_eq!(final_rec.status, "applied", "status should be applied");
    assert_eq!(final_rec.approvals_count, 3);
    assert!(
        final_rec.applied_at.is_some(),
        "applied_at must be set on return"
    );

    // DB status must also be 'applied'.
    assert_eq!(
        change_status(svc.pool(), &record.id),
        "applied",
        "DB status should be applied"
    );

    // An identity_schema row must exist.
    let (_, is_active, meta_json) = find_identity_row(svc.pool(), content)
        .expect("identity_schema row must exist after 3 approvals");

    assert!(is_active, "identity row must be active");

    // Metadata must carry applied_by and change_id.
    let meta_str = meta_json.expect("metadata_json must be present");
    let meta: serde_json::Value =
        serde_json::from_str(&meta_str).expect("metadata_json must be valid JSON");
    assert_eq!(
        meta["applied_by"].as_str().unwrap_or(""),
        "actor-charlie",
        "applied_by should be the 3rd approver's actor_id"
    );
    assert_eq!(
        meta["change_id"].as_str().unwrap_or(""),
        record.id,
        "change_id should match the constitution_change id"
    );
}

// ── test 2 ───────────────────────────────────────────────────────────────────

/// After 3 approvals the change is in `'applied'` state.
/// A 4th approve call must return an error containing "not pending".
#[test]
fn fourth_approve_rejected() {
    let svc = make_service();

    let record = svc
        .propose_change("Fourth-approval test rule", "test", None, None, None, None)
        .unwrap();

    svc.approve_change_for_principal(&record.id, "p1", None, None)
        .unwrap();
    svc.approve_change_for_principal(&record.id, "p2", None, None)
        .unwrap();
    svc.approve_change_for_principal(&record.id, "p3", None, None)
        .unwrap();

    let err = svc
        .approve_change_for_principal(&record.id, "p4", None, None)
        .unwrap_err();

    assert!(
        err.contains("not pending"),
        "4th approval must fail with 'not pending'; got: {err}"
    );
}

// ── test 3 ───────────────────────────────────────────────────────────────────

/// Propose an `update` change targeting an existing identity row.
/// After 3 approvals the target row's content must be replaced and
/// `updated_at` advanced (upsert_note handles that via ON CONFLICT DO UPDATE).
#[test]
fn update_replaces_existing_identity_row() {
    let svc = make_service();

    // Seed an existing identity_schema row directly.
    let target_id = uuid::Uuid::new_v4().to_string();
    svc.pool().with_conn(|conn: &Connection| {
        conn.execute(
            "INSERT INTO notes (id, content, layer, is_active, created_at, updated_at)
             VALUES (?1, 'old content', 'identity_schema', 1, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
            rusqlite::params![target_id],
        )
        .expect("seed identity row");
        conn.execute(
            "INSERT INTO notes_fts (note_id, content) VALUES (?1, 'old content')",
            rusqlite::params![target_id],
        )
        .expect("seed fts row");
    });

    let new_content = "updated identity content";
    let record = svc
        .propose_change(
            new_content,
            "update test",
            Some("update"),
            Some(&target_id),
            None,
            None,
        )
        .unwrap();

    svc.approve_change_for_principal(&record.id, "u1", None, None)
        .unwrap();
    svc.approve_change_for_principal(&record.id, "u2", None, None)
        .unwrap();
    svc.approve_change_for_principal(&record.id, "u3", None, None)
        .unwrap();

    // The target row must now have the new content.
    let (content, is_active, _meta, _evo) =
        fetch_note(svc.pool(), &target_id).expect("target row must still exist");

    assert_eq!(content, new_content, "content must be replaced");
    assert!(is_active, "updated row must remain active");

    // updated_at must have advanced past the seed value.
    let updated_at: String = svc.pool().with_conn(|conn| {
        conn.query_row(
            "SELECT updated_at FROM notes WHERE id = ?1",
            [&target_id],
            |row| row.get(0),
        )
        .expect("updated_at query")
    });
    assert_ne!(
        updated_at, "2020-01-01T00:00:00Z",
        "updated_at must have advanced"
    );
}

#[test]
fn update_requires_target_id_and_rolls_back() {
    let svc = make_service();

    let record = svc
        .propose_change(
            "missing target update",
            "update without target_id must fail",
            Some("update"),
            None,
            None,
            None,
        )
        .unwrap();

    svc.approve_change_for_principal(&record.id, "u1", None, None)
        .unwrap();
    svc.approve_change_for_principal(&record.id, "u2", None, None)
        .unwrap();
    let err = svc
        .approve_change_for_principal(&record.id, "u3", None, None)
        .unwrap_err();

    assert!(
        err.contains("update requires target_id"),
        "expected missing target_id error; got: {err}"
    );
    assert_eq!(
        change_status(svc.pool(), &record.id),
        "pending",
        "status must stay pending after missing target_id rollback"
    );
    assert!(
        find_identity_row(svc.pool(), "missing target update").is_none(),
        "update without target_id must not create a new identity row"
    );
}

#[test]
fn update_stamps_project_id_on_existing_identity_row() {
    let svc = make_service_with_project("project-visible");

    let target_id = uuid::Uuid::new_v4().to_string();
    svc.pool().with_conn(|conn: &Connection| {
        conn.execute(
            "INSERT INTO notes (id, content, layer, is_active, project_id, created_at, updated_at)
             VALUES (?1, 'old projectless content', 'identity_schema', 1, NULL, '2020-01-01T00:00:00Z', '2020-01-01T00:00:00Z')",
            rusqlite::params![target_id],
        )
        .expect("seed identity row");
        conn.execute(
            "INSERT INTO notes_fts (note_id, content) VALUES (?1, 'old projectless content')",
            rusqlite::params![target_id],
        )
        .expect("seed fts row");
    });

    let record = svc
        .propose_change(
            "project-visible content",
            "project stamp update",
            Some("update"),
            Some(&target_id),
            None,
            None,
        )
        .unwrap();

    svc.approve_change_for_principal(&record.id, "p1", None, None)
        .unwrap();
    svc.approve_change_for_principal(&record.id, "p2", None, None)
        .unwrap();
    svc.approve_change_for_principal(&record.id, "p3", None, None)
        .unwrap();

    let project_id: Option<String> = svc.pool().with_conn(|conn| {
        conn.query_row(
            "SELECT project_id FROM notes WHERE id = ?1",
            [&target_id],
            |row| row.get(0),
        )
        .expect("project_id query")
    });
    assert_eq!(project_id.as_deref(), Some("project-visible"));

    let visible_count: i64 = svc.pool().with_conn(|conn| {
        conn.query_row(
            "SELECT COUNT(*) FROM notes
             WHERE id = ?1 AND project_id = 'project-visible' AND layer = 'identity_schema'",
            [&target_id],
            |row| row.get(0),
        )
        .expect("project-filter visibility query")
    });
    assert_eq!(visible_count, 1);
}

// ── test 4 ───────────────────────────────────────────────────────────────────

/// Propose a `delete` change targeting an existing identity row.
/// After 3 approvals the target row must have `is_active=0` and
/// `evolution_state='deprecated'`.
#[test]
fn delete_marks_identity_row_inactive() {
    let svc = make_service();

    // Seed an identity_schema row to delete.
    let target_id = uuid::Uuid::new_v4().to_string();
    svc.pool().with_conn(|conn: &Connection| {
        conn.execute(
            "INSERT INTO notes (id, content, layer, is_active, evolution_state)
             VALUES (?1, 'to be deleted', 'identity_schema', 1, 'active')",
            rusqlite::params![target_id],
        )
        .expect("seed row");
        conn.execute(
            "INSERT INTO notes_fts (note_id, content) VALUES (?1, 'to be deleted')",
            rusqlite::params![target_id],
        )
        .expect("seed fts");
    });

    let record = svc
        .propose_change(
            "to be deleted",
            "delete test",
            Some("delete"),
            Some(&target_id),
            None,
            None,
        )
        .unwrap();

    svc.approve_change_for_principal(&record.id, "d1", None, None)
        .unwrap();
    svc.approve_change_for_principal(&record.id, "d2", None, None)
        .unwrap();
    svc.approve_change_for_principal(&record.id, "d3", None, None)
        .unwrap();

    let (_, is_active, _, evolution_state) =
        fetch_note(svc.pool(), &target_id).expect("target row must still exist (soft-delete)");

    assert!(!is_active, "is_active must be 0 after delete");
    assert_eq!(
        evolution_state.as_deref(),
        Some("deprecated"),
        "evolution_state must be 'deprecated'"
    );
}

// ── test 5 ───────────────────────────────────────────────────────────────────

/// If `apply_change_inner` fails (e.g., `delete` with a target_id that has
/// no matching identity_schema row), the entire transaction must roll back:
/// - `constitution_changes.status` stays `'pending'`
/// - No notes row is created or modified
#[test]
fn apply_failure_rolls_back() {
    let svc = make_service();

    // Target a non-existent row → delete will affect 0 rows → error.
    let ghost_id = uuid::Uuid::new_v4().to_string();

    let record = svc
        .propose_change(
            "ghost content",
            "rollback test",
            Some("delete"),
            Some(&ghost_id),
            None,
            None,
        )
        .unwrap();

    svc.approve_change_for_principal(&record.id, "r1", None, None)
        .unwrap();
    svc.approve_change_for_principal(&record.id, "r2", None, None)
        .unwrap();

    // 3rd approval triggers apply_change_inner which will fail because ghost_id
    // has no identity_schema row.
    let err = svc
        .approve_change_for_principal(&record.id, "r3", None, None)
        .unwrap_err();

    assert!(
        err.contains("no identity_schema row") || err.contains("apply_change"),
        "expected apply error; got: {err}"
    );

    // Status must remain 'pending' (transaction rolled back).
    assert_eq!(
        change_status(svc.pool(), &record.id),
        "pending",
        "status must stay pending after rollback"
    );

    // No identity_schema row for ghost_id must have been created.
    let row = fetch_note(svc.pool(), &ghost_id);
    assert!(
        row.is_none(),
        "no notes row must exist for ghost_id after rollback"
    );
}
