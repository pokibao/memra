//! Session UPSERT write operations.
//!
//! Maintains the `sessions` table that tracks per-session metadata for
//! replay attribution (TODO-IMPL-01).  All writes are designed to run inside
//! an existing `BEGIN IMMEDIATE` transaction so they can be made atomic with
//! the note insert that triggered them.

use rusqlite::Connection;

/// Upsert a session row.
///
/// - On first appearance: inserts with both `first_seen` and `last_seen` set
///   to `now`.
/// - On conflict: updates only `last_seen`, leaving `first_seen` unchanged.
///
/// `agent_label`, `actor_id`, and `project_id` are optional observability
/// hints; they are allowed to be NULL in the schema. `project_id` is the
/// per-project namespace used by `get_context.active_sessions` to enforce
/// multi-project isolation (PR #232 Codex P1).
pub fn upsert_session(
    conn: &Connection,
    session_id: &str,
    agent_label: Option<&str>,
    actor_id: Option<&str>,
    project_id: Option<&str>,
    now: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sessions (session_id, agent_label, actor_id, project_id, first_seen, last_seen)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)
         ON CONFLICT(session_id) DO UPDATE SET
             last_seen = excluded.last_seen,
             -- Keep first non-null project_id; same session_id should not
             -- legitimately switch projects, but if it does, retain the
             -- earliest attribution rather than silently overwriting.
             project_id = COALESCE(sessions.project_id, excluded.project_id)",
        rusqlite::params![session_id, agent_label, actor_id, project_id, now],
    )?;
    Ok(())
}

/// Ensure the `sessions` table and its index exist.
///
/// Called from `DbPool::open` (same path as the `notes` table creation) so
/// the table is always present even on a fresh database.
///
/// Migration safety (PR1 → PR3): existing PR1-era DBs have a `sessions`
/// table without the `project_id` column. SQLite has no `ADD COLUMN IF NOT
/// EXISTS`, AND `CREATE INDEX ... ON sessions(project_id, ...)` errors with
/// "no such column" when the column is missing — even with `IF NOT EXISTS`,
/// because column references are validated at parse time, not at index-name
/// dedup time.
///
/// Order MUST be: CREATE TABLE (declares full target schema for fresh DBs)
/// → migrate (ALTER TABLE for legacy DBs missing the column) → CREATE INDEX
/// referencing the migrated column. Doing the index up front breaks legacy
/// upgrades (verified via sqlite3 CLI 2026-04-26 self-review).
pub fn ensure_sessions_table(conn: &Connection) -> rusqlite::Result<()> {
    // Step 1: ensure the table exists with the up-to-date schema for fresh DBs.
    // The CREATE TABLE IF NOT EXISTS does NOT alter existing tables, so legacy
    // PR1-era DBs still need explicit migration in step 2.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            session_id  TEXT PRIMARY KEY,
            agent_label TEXT,
            actor_id    TEXT,
            project_id  TEXT,
            first_seen  TEXT NOT NULL,
            last_seen   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_sessions_last_seen
            ON sessions(last_seen DESC);",
    )?;

    // Step 2: legacy migration — add project_id column if missing. Probe via
    // PRAGMA table_info because SQLite has no ADD COLUMN IF NOT EXISTS.
    if !column_exists(conn, "sessions", "project_id")? {
        conn.execute("ALTER TABLE sessions ADD COLUMN project_id TEXT", [])?;
    }

    // Step 3: only NOW is project_id guaranteed to exist on every DB shape,
    // so the project-scoped index can be created safely on both fresh and
    // migrated databases.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_sessions_project_last_seen
            ON sessions(project_id, last_seen DESC)",
        [],
    )?;
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_test_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        ensure_sessions_table(&conn).expect("ensure_sessions_table");
        conn
    }

    fn table_exists(conn: &Connection, table: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    fn index_exists(conn: &Connection, index: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
            [index],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    fn row_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap_or(0)
    }

    fn get_row(conn: &Connection, session_id: &str) -> Option<(String, String)> {
        conn.query_row(
            "SELECT first_seen, last_seen FROM sessions WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()
    }

    fn get_agent_label(conn: &Connection, session_id: &str) -> Option<String> {
        conn.query_row(
            "SELECT agent_label FROM sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    }

    #[test]
    fn sessions_table_created_on_db_init() {
        let conn = open_test_conn();
        assert!(table_exists(&conn, "sessions"), "sessions table must exist");
        assert!(
            index_exists(&conn, "idx_sessions_last_seen"),
            "idx_sessions_last_seen index must exist"
        );
        assert!(
            index_exists(&conn, "idx_sessions_project_last_seen"),
            "idx_sessions_project_last_seen index must exist"
        );
    }

    #[test]
    fn ensure_sessions_table_migrates_legacy_pr1_schema() {
        // Self-review regression: simulate a PR1-era DB whose `sessions`
        // table was created BEFORE the `project_id` column existed. The
        // first version of `ensure_sessions_table` (PR3 / commit f5817da)
        // ran `CREATE INDEX ... ON sessions(project_id, ...)` inside the
        // up-front execute_batch, which fails on legacy schemas with
        // "no such column: project_id". The fixed version must:
        //   - leave existing rows intact
        //   - add the project_id column (NULL for pre-migration rows)
        //   - create both indexes (last_seen, project_last_seen)
        let conn = Connection::open_in_memory().expect("in-memory db");

        // Set up legacy PR1-era schema (no project_id column, only last_seen index).
        conn.execute_batch(
            "CREATE TABLE sessions (
                session_id  TEXT PRIMARY KEY,
                agent_label TEXT,
                actor_id    TEXT,
                first_seen  TEXT NOT NULL,
                last_seen   TEXT NOT NULL
            );
            CREATE INDEX idx_sessions_last_seen ON sessions(last_seen DESC);",
        )
        .expect("legacy schema setup");

        // Insert a row into the legacy schema (5 columns, no project_id).
        conn.execute(
            "INSERT INTO sessions (session_id, agent_label, actor_id, first_seen, last_seen)
             VALUES ('http:legacy-1', 'claude-code', 'actor-1', '2026-04-20T00:00:00Z', '2026-04-20T00:00:00Z')",
            [],
        ).expect("legacy row");

        // Run the migration. Must NOT error.
        ensure_sessions_table(&conn).expect("migration must succeed on legacy schema");

        // The legacy row must still be there.
        assert_eq!(row_count(&conn), 1, "legacy row must survive migration");

        // The project_id column must now exist (NULL for pre-migration rows).
        let project_id_after_migration: Option<String> = conn
            .query_row(
                "SELECT project_id FROM sessions WHERE session_id = 'http:legacy-1'",
                [],
                |row| row.get(0),
            )
            .expect("project_id column must be queryable post-migration");
        assert_eq!(
            project_id_after_migration, None,
            "pre-migration rows must have NULL project_id"
        );

        // Both indexes must now exist (the project-scoped one is the one that
        // would have crashed the up-front execute_batch on legacy DBs).
        assert!(
            index_exists(&conn, "idx_sessions_last_seen"),
            "idx_sessions_last_seen must exist post-migration"
        );
        assert!(
            index_exists(&conn, "idx_sessions_project_last_seen"),
            "idx_sessions_project_last_seen must exist post-migration"
        );

        // New writes through the migrated table must accept project_id.
        upsert_session(
            &conn,
            "http:post-migration",
            Some("codex"),
            Some("actor-2"),
            Some("memra"),
            "2026-04-26T00:00:00Z",
        )
        .expect("post-migration upsert must succeed with project_id");
        assert_eq!(row_count(&conn), 2);

        // Re-running ensure_sessions_table must be idempotent (no-op).
        ensure_sessions_table(&conn).expect("re-running migration must be idempotent");
        assert_eq!(
            row_count(&conn),
            2,
            "idempotent re-run must not change rows"
        );
    }

    #[test]
    fn sessions_upsert_inserts_new_row() {
        let conn = open_test_conn();
        upsert_session(
            &conn,
            "http:sess-abc",
            Some("claude-code"),
            Some("actor-1"),
            Some("memra"),
            "2026-04-26T10:00:00Z",
        )
        .expect("upsert");

        assert_eq!(row_count(&conn), 1);
        let (first_seen, last_seen) = get_row(&conn, "http:sess-abc").expect("row must exist");
        assert_eq!(first_seen, "2026-04-26T10:00:00Z");
        assert_eq!(last_seen, "2026-04-26T10:00:00Z");
    }

    #[test]
    fn sessions_upsert_updates_existing_row_last_seen() {
        let conn = open_test_conn();
        // Insert 5 times with advancing timestamps.
        for i in 0..5u32 {
            let ts = format!("2026-04-26T10:00:{i:02}Z");
            upsert_session(&conn, "http:sess-multi", None, None, None, &ts).expect("upsert");
        }

        // Only 1 row.
        assert_eq!(row_count(&conn), 1);
        let (first_seen, last_seen) = get_row(&conn, "http:sess-multi").expect("row");
        assert_eq!(
            first_seen, "2026-04-26T10:00:00Z",
            "first_seen must not advance"
        );
        assert_eq!(
            last_seen, "2026-04-26T10:00:04Z",
            "last_seen must reflect latest write"
        );
    }

    // Helpers for atomicity tests that need the full notes+fts schema.
    fn open_full_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS notes (
                id               TEXT PRIMARY KEY,
                content          TEXT NOT NULL,
                layer            TEXT,
                category         TEXT,
                is_active        INTEGER NOT NULL DEFAULT 1,
                confidence       REAL,
                agent_id         TEXT,
                project_id       TEXT,
                created_at       TEXT,
                updated_at       TEXT,
                valid_at         REAL,
                metadata_json    TEXT,
                vector_json      TEXT,
                vector_blob      BLOB,
                evolution_state  TEXT DEFAULT 'active',
                topic_key        TEXT,
                is_head          INTEGER DEFAULT 1,
                review_after     TEXT,
                room             TEXT,
                agent            TEXT,
                difficulty       INTEGER,
                time_cost_hint   TEXT,
                related_ids_json TEXT,
                role             TEXT,
                session_id       TEXT,
                source           TEXT,
                created_by       TEXT,
                version          INTEGER DEFAULT 1,
                root_id          TEXT,
                cold_storage_ref TEXT,
                event_when       TEXT,
                event_when_ts    REAL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(note_id UNINDEXED, content);",
        )
        .expect("notes schema");
        ensure_sessions_table(&conn).expect("sessions schema");
        conn
    }

    fn notes_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
            .unwrap_or(0)
    }

    #[test]
    fn note_insert_and_session_upsert_atomic_on_success() {
        use crate::storage::writer::{NoteInsert, upsert_note};

        let conn = open_full_conn();

        // This test asserts atomicity of (note insert + session upsert) inside
        // the same transaction. The agent_label-source regression is covered
        // separately by `agent_label_uses_agent_field_not_agent_id` to keep
        // this test focused on the transactional contract.
        conn.execute_batch("BEGIN IMMEDIATE").expect("begin");
        let note = NoteInsert {
            id: "test-note-1".to_string(),
            content: "atomic test content".to_string(),
            layer: "verified_fact".to_string(),
            is_active: true,
            session_id: Some("http:sess-atomic".to_string()),
            ..Default::default()
        };
        upsert_note(&conn, &note).expect("upsert_note");
        conn.execute_batch("COMMIT").expect("commit");

        // Both rows must be present after commit.
        assert_eq!(notes_count(&conn), 1, "note must be persisted");
        assert_eq!(row_count(&conn), 1, "session row must be persisted");
        let (first_seen, _) = get_row(&conn, "http:sess-atomic").expect("session row");
        assert!(!first_seen.is_empty());
    }

    #[test]
    fn agent_label_uses_agent_field_not_agent_id() {
        // Codex P2 regression test (PR #230): write_orchestrator populates
        // NoteInsert.agent and leaves agent_id as None. If upsert_note read
        // agent_id, every session row would have NULL agent_label.
        use crate::storage::writer::{NoteInsert, upsert_note};

        let conn = open_full_conn();

        conn.execute_batch("BEGIN IMMEDIATE").expect("begin");
        let note = NoteInsert {
            id: "agent-label-regression".to_string(),
            content: "regression for Codex P2".to_string(),
            layer: "verified_fact".to_string(),
            is_active: true,
            session_id: Some("http:agent-source-check".to_string()),
            // Mirror the add-memory path exactly: agent populated, agent_id None.
            agent: Some("codex".to_string()),
            agent_id: None,
            ..Default::default()
        };
        upsert_note(&conn, &note).expect("upsert_note");
        conn.execute_batch("COMMIT").expect("commit");

        assert_eq!(
            get_agent_label(&conn, "http:agent-source-check"),
            Some("codex".to_string()),
            "agent_label must reflect note.agent (the actually-populated field)"
        );
    }

    #[test]
    fn note_insert_and_session_upsert_atomic_on_failure() {
        use crate::storage::writer::{NoteInsert, upsert_note};

        let conn = open_full_conn();

        // First write a note successfully so we know the schema works.
        conn.execute_batch("BEGIN IMMEDIATE").expect("begin");
        let note = NoteInsert {
            id: "note-atomic-fail".to_string(),
            content: "first write".to_string(),
            layer: "verified_fact".to_string(),
            is_active: true,
            session_id: Some("http:sess-atomic-fail".to_string()),
            ..Default::default()
        };
        upsert_note(&conn, &note).expect("first upsert");
        conn.execute_batch("COMMIT").expect("first commit");

        assert_eq!(notes_count(&conn), 1);
        assert_eq!(row_count(&conn), 1);

        // Now simulate a rollback: begin a second transaction, write a new
        // note + session, then roll back. Neither should appear.
        conn.execute_batch("BEGIN IMMEDIATE").expect("begin 2");
        let note2 = NoteInsert {
            id: "note-atomic-fail-2".to_string(),
            content: "second write that will be rolled back".to_string(),
            layer: "verified_fact".to_string(),
            is_active: true,
            session_id: Some("http:sess-atomic-fail-2".to_string()),
            ..Default::default()
        };
        upsert_note(&conn, &note2).expect("second upsert (pre-rollback)");
        conn.execute_batch("ROLLBACK").expect("rollback");

        // After rollback only the first note/session must survive.
        assert_eq!(notes_count(&conn), 1, "rollback must undo second note");
        assert_eq!(row_count(&conn), 1, "rollback must undo second session");
        assert!(get_row(&conn, "http:sess-atomic-fail-2").is_none());
    }
}
