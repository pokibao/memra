use memra_core::storage::db::{ensure_notes_trust_columns, expire_unconfirmed_candidates};
use rusqlite::{Connection, params};

const NOW: i64 = 1_700_000_000;
const OLD: i64 = NOW - 8 * 86_400;

fn setup_conn() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    conn.execute_batch(
        "CREATE TABLE notes (
            id TEXT PRIMARY KEY,
            content TEXT NOT NULL,
            source TEXT,
            created_at INTEGER,
            recall_count INTEGER DEFAULT 0,
            is_active INTEGER DEFAULT 1
        );",
    )
    .expect("create notes");
    ensure_notes_trust_columns(&conn).expect("ensure trust columns");
    conn
}

fn insert_note(
    conn: &Connection,
    id: &str,
    source: &str,
    created_at: i64,
    recall_count: i64,
    confirmed_at: Option<i64>,
) {
    conn.execute(
        "INSERT INTO notes (id, content, source, created_at, recall_count, is_active, confirmed_at)
         VALUES (?1, 'candidate', ?2, ?3, ?4, 1, ?5)",
        params![id, source, created_at, recall_count, confirmed_at],
    )
    .expect("insert note");
}

fn active(conn: &Connection, id: &str) -> bool {
    conn.query_row("SELECT is_active FROM notes WHERE id = ?1", [id], |row| {
        row.get::<_, i64>(0)
    })
    .expect("read is_active")
        == 1
}

#[test]
fn trust_gate_ttl_expires_old_unrecalled_ai_candidate() {
    let conn = setup_conn();
    insert_note(&conn, "old-ai", "ai", OLD, 0, None);

    let expired = expire_unconfirmed_candidates(&conn, NOW).expect("expire candidates");

    assert_eq!(expired, 1);
    assert!(!active(&conn, "old-ai"));
}

#[test]
fn trust_gate_ttl_keeps_recalled_ai_candidate() {
    let conn = setup_conn();
    insert_note(&conn, "recalled-ai", "ai", OLD, 3, None);

    let expired = expire_unconfirmed_candidates(&conn, NOW).expect("expire candidates");

    assert_eq!(expired, 0);
    assert!(active(&conn, "recalled-ai"));
}

#[test]
fn trust_gate_ttl_keeps_user_note() {
    let conn = setup_conn();
    insert_note(&conn, "user-note", "user", OLD, 0, None);

    let expired = expire_unconfirmed_candidates(&conn, NOW).expect("expire candidates");

    assert_eq!(expired, 0);
    assert!(active(&conn, "user-note"));
}

#[test]
fn trust_gate_ttl_keeps_confirmed_ai_candidate() {
    let conn = setup_conn();
    insert_note(&conn, "confirmed-ai", "ai", OLD, 0, Some(NOW - 60));

    let expired = expire_unconfirmed_candidates(&conn, NOW).expect("expire candidates");

    assert_eq!(expired, 0);
    assert!(active(&conn, "confirmed-ai"));
}

#[test]
fn trust_gate_ttl_migration_adds_columns_and_index() {
    let conn = setup_conn();

    let confirmed_at_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name = 'confirmed_at'",
            [],
            |row| row.get(0),
        )
        .expect("confirmed_at column");
    let confirmed_by_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name = 'confirmed_by'",
            [],
            |row| row.get(0),
        )
        .expect("confirmed_by column");
    let index_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_notes_source_confirmed'",
            [],
            |row| row.get(0),
        )
        .expect("trust index");

    assert_eq!(confirmed_at_count, 1);
    assert_eq!(confirmed_by_count, 1);
    assert_eq!(index_count, 1);
}

#[test]
fn trust_gate_ttl_migration_works_on_existing_db_with_source_column() {
    let conn = Connection::open_in_memory().expect("open in-memory db");
    conn.execute_batch(
        "CREATE TABLE notes (id TEXT, source TEXT);
         INSERT INTO notes (id, source) VALUES ('n1', 'user'), ('n2', 'ai');",
    )
    .expect("create legacy notes");

    ensure_notes_trust_columns(&conn).expect("ensure trust columns");

    let confirmed_at_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name = 'confirmed_at'",
            [],
            |row| row.get(0),
        )
        .expect("confirmed_at column");
    let confirmed_by_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name = 'confirmed_by'",
            [],
            |row| row.get(0),
        )
        .expect("confirmed_by column");
    let index_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_notes_source_confirmed'",
            [],
            |row| row.get(0),
        )
        .expect("trust index");

    assert_eq!(confirmed_at_count, 1);
    assert_eq!(confirmed_by_count, 1);
    assert_eq!(index_count, 1);
}

#[test]
fn trust_gate_ttl_expires_every_ai_provisional_source() {
    // Regression guard for Gap 4 (code-reviewer HIGH #1):
    // Every variant in personal::AI_PROVISIONAL_SOURCES must be subject to TTL.
    // Hardcoding the SQL IN-list lets sources drift silently. This test inserts
    // one stale unrecalled candidate per provisional source and asserts every
    // one of them gets deactivated by `expire_unconfirmed_candidates`.
    let conn = setup_conn();

    for source in memra_core::personal::AI_PROVISIONAL_SOURCES {
        let id = format!("old-{source}");
        insert_note(&conn, &id, source, OLD, 0, None);
    }

    let expired = expire_unconfirmed_candidates(&conn, NOW).expect("expire candidates");

    assert_eq!(
        expired,
        memra_core::personal::AI_PROVISIONAL_SOURCES.len(),
        "every AI provisional source variant must be subject to TTL"
    );

    for source in memra_core::personal::AI_PROVISIONAL_SOURCES {
        let id = format!("old-{source}");
        assert!(
            !active(&conn, &id),
            "source variant {source} was not expired",
        );
    }
}

#[test]
fn trust_gate_ttl_migration_skips_write_lock_when_fully_migrated() {
    // P1 regression guard (Codex bot finding on PR #246): DbPool::open
    // invokes ensure_notes_trust_columns on every startup. Once the schema
    // is fully migrated, the helper must NOT open a write transaction —
    // otherwise concurrent opens deadlock with `database is locked`.
    //
    // Strategy: after a clean migration, flip the connection into
    // `PRAGMA query_only = 1`. A correctly-implemented early return must
    // keep the second invocation green; any attempt to BEGIN IMMEDIATE /
    // ALTER / CREATE INDEX under query_only fails with a runtime error.
    let conn = Connection::open_in_memory().expect("open in-memory db");
    conn.execute_batch("CREATE TABLE notes (id TEXT, source TEXT);")
        .expect("create legacy notes");

    ensure_notes_trust_columns(&conn).expect("first migration");

    conn.execute_batch("PRAGMA query_only = 1;")
        .expect("set query_only");

    let result = ensure_notes_trust_columns(&conn);

    conn.execute_batch("PRAGMA query_only = 0;")
        .expect("reset query_only");

    result.expect("second invocation must not require a write lock");
}
