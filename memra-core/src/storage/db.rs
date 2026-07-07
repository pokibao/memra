//! SQLite connection setup and row types.
//!
//! Opens the existing Memra SQLite database (created by Python)
//! in read-only mode for Phase 1 (Read Path).

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, Result as SqlResult};
use serde::{Deserialize, Serialize};

use crate::personal::AI_PROVISIONAL_SOURCES;
use crate::storage::session_tokens_writer::ensure_session_tokens_table;
use crate::storage::sessions_writer::ensure_sessions_table;

/// Wrapper around rusqlite::Connection with interior mutability.
///
/// rusqlite::Connection is `Send` but not `Sync`, so we wrap in Mutex
/// to allow shared access from the MCP tool handlers.
pub struct DbPool {
    conn: Mutex<Connection>,
}

impl DbPool {
    /// Open an existing Memra SQLite database.
    ///
    /// Phase 1 opens read-write (needed for FTS5 MATCH queries with WAL).
    /// The path should point to the `.sqlite3` file created by Python.
    pub fn open(path: &Path) -> SqlResult<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        // Match Python's pragmas
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 30000;
             PRAGMA cache_size = -8000;",
        )?;
        // Ensure sessions table exists (idempotent CREATE IF NOT EXISTS).
        ensure_sessions_table(&conn)?;
        // Ensure session_tokens table exists (new in IMPL-01b).
        ensure_session_tokens_table(&conn)?;
        // Trust Gate v1: AI-origin notes remain candidates until confirmed.
        ensure_notes_trust_columns(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open an in-memory pool with the full notes + sessions schema.
    ///
    /// Intended for tests (unit + integration). Marked `#[doc(hidden)]` because
    /// it bypasses the normal "open the production DB" entrypoint, but kept
    /// `pub` so integration tests under `tests/` can build a `SearchEngine`
    /// against a real schema without touching the user's `~/.memra`
    /// state. Production code should call `open` or `open_readonly`.
    #[doc(hidden)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA busy_timeout = 30000;",
        )?;
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
                source           TEXT DEFAULT 'user',
                created_by       TEXT,
                version          INTEGER DEFAULT 1,
                root_id          TEXT,
                cold_storage_ref TEXT,
                event_when       TEXT,
                event_when_ts    REAL,
                confirmed_at     INTEGER,
                confirmed_by     TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_notes_source_confirmed
                ON notes(source, confirmed_at);
            CREATE TABLE IF NOT EXISTS checkpoints (
                id          TEXT PRIMARY KEY,
                content     TEXT NOT NULL,
                is_active   INTEGER NOT NULL DEFAULT 1,
                metadata_json TEXT,
                created_at  TEXT,
                project_id  TEXT
            );
            CREATE TABLE IF NOT EXISTS note_relations (
                from_note_id TEXT NOT NULL,
                to_note_id TEXT NOT NULL,
                relation_type TEXT NOT NULL,
                strength REAL NOT NULL DEFAULT 0.5,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (from_note_id, to_note_id, relation_type)
            );
            CREATE TABLE IF NOT EXISTS dream_candidates (
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
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts
                USING fts5(note_id UNINDEXED, content);",
        )?;
        ensure_sessions_table(&conn)?;
        ensure_session_tokens_table(&conn)?;
        ensure_notes_trust_columns(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Open a read-only connection (for tests against snapshot DBs).
    pub fn open_readonly(path: &Path) -> SqlResult<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.execute_batch("PRAGMA busy_timeout = 30000;")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Execute a closure with the connection lock held.
    ///
    /// If a previous call panicked while holding the lock, the mutex is
    /// "poisoned". We recover from that by calling `into_inner()` — the
    /// `Connection` itself is still valid for independent SQLite statements,
    /// so cascading the failure would be worse than continuing.
    pub fn with_conn<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&Connection) -> T,
    {
        let conn = match self.conn.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::warn!("DB mutex was poisoned; recovering inner guard");
                poisoned.into_inner()
            }
        };
        f(&conn)
    }
}

/// Ensure Trust Gate v1 confirmation columns exist on legacy notes tables.
pub fn ensure_notes_trust_columns(conn: &Connection) -> rusqlite::Result<()> {
    if !table_exists(conn, "notes")? {
        return Ok(());
    }
    let needs_confirmed_at = !column_exists(conn, "notes", "confirmed_at")?;
    let needs_confirmed_by = !column_exists(conn, "notes", "confirmed_by")?;
    let has_source = column_exists(conn, "notes", "source")?;
    let needs_index = has_source && !trust_index_exists(conn)?;

    // Skip the write lock entirely once every artifact is already present.
    // `DbPool::open` calls this on every startup; taking BEGIN IMMEDIATE on
    // already-migrated databases regresses availability (concurrent opens
    // can fail with `database is locked`). The previous condition still ran
    // the transaction whenever `has_source` was true.
    if !needs_confirmed_at && !needs_confirmed_by && !needs_index {
        return Ok(());
    }

    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> rusqlite::Result<()> {
        if needs_confirmed_at {
            conn.execute("ALTER TABLE notes ADD COLUMN confirmed_at INTEGER NULL", [])?;
        }
        if needs_confirmed_by {
            conn.execute("ALTER TABLE notes ADD COLUMN confirmed_by TEXT NULL", [])?;
        }
        if needs_index {
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_notes_source_confirmed
                    ON notes(source, confirmed_at)",
                [],
            )?;
        }
        Ok(())
    })();

    if let Err(err) = result {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(err);
    }

    if let Err(err) = conn.execute_batch("COMMIT") {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(err);
    }

    Ok(())
}

/// Deactivate stale, unconfirmed AI candidates that were never recalled.
///
/// The provisional source list is derived from `personal::AI_PROVISIONAL_SOURCES`
/// so adding a new source variant there automatically extends TTL coverage —
/// guards against the source-list drift class of bug (see Trust Gate v1 Gap 4).
pub fn expire_unconfirmed_candidates(conn: &Connection, now_epoch: i64) -> rusqlite::Result<usize> {
    ensure_notes_trust_columns(conn)?;
    let cutoff = now_epoch - 7 * 86_400;
    let in_list = AI_PROVISIONAL_SOURCES
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "UPDATE notes
         SET is_active = 0
         WHERE source IN ({in_list})
           AND confirmed_at IS NULL
           AND COALESCE(recall_count, 0) = 0
           AND COALESCE(
                CASE
                    WHEN typeof(created_at) IN ('integer', 'real') THEN CAST(created_at AS INTEGER)
                    WHEN created_at NOT GLOB '*[^0-9]*' THEN CAST(created_at AS INTEGER)
                    ELSE CAST(strftime('%s', created_at) AS INTEGER)
                END,
                0
           ) < ?1"
    );
    conn.execute(&sql, [cutoff])
}

fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name.eq_ignore_ascii_case(column) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn trust_index_exists(conn: &Connection) -> rusqlite::Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type = 'index' AND name = 'idx_notes_source_confirmed'",
        [],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Helper: create an in-memory DbPool with a minimal fixture table.
    ///
    /// We construct DbPool directly (same module = access to private field)
    /// rather than going through `DbPool::open` so we avoid needing a temp
    /// file on disk and the `tempfile` crate dev-dependency.
    fn make_pool() -> DbPool {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE fixture (id TEXT PRIMARY KEY, val INTEGER NOT NULL);
             INSERT INTO fixture VALUES ('a', 1), ('b', 2);",
        )
        .expect("setup");
        DbPool {
            conn: Mutex::new(conn),
        }
    }

    #[test]
    fn with_conn_normal_path_succeeds() {
        let pool = make_pool();

        let v1: i64 = pool.with_conn(|c| {
            c.query_row("SELECT val FROM fixture WHERE id = 'a'", [], |r| r.get(0))
                .expect("query a")
        });
        assert_eq!(v1, 1, "first with_conn should return 1");

        let v2: i64 = pool.with_conn(|c| {
            c.query_row("SELECT val FROM fixture WHERE id = 'b'", [], |r| r.get(0))
                .expect("query b")
        });
        assert_eq!(v2, 2, "second with_conn should return 2");
    }

    #[test]
    fn with_conn_recovers_from_poisoned_mutex() {
        let pool = Arc::new(make_pool());
        let pool_clone = Arc::clone(&pool);

        // Spawn a thread that panics inside with_conn, poisoning the mutex.
        let handle = std::thread::spawn(move || {
            // Use catch_unwind so the thread's panic doesn't abort the process.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pool_clone.with_conn(|_c| {
                    panic!("intentional panic to poison the mutex");
                })
            }));
            // Propagate the panic so the JoinHandle carries Err.
            result.unwrap();
        });

        // The spawned thread should have panicked.
        assert!(handle.join().is_err(), "thread should have panicked");

        // The main thread must still be able to use with_conn after the poison.
        let v: i64 = pool.with_conn(|c| {
            c.query_row("SELECT val FROM fixture WHERE id = 'a'", [], |r| r.get(0))
                .expect("query after poison")
        });
        assert_eq!(v, 1, "with_conn must still work after mutex was poisoned");
    }
}

// --- Row types matching Python `notes` table schema ---

/// A row from the `notes` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoteRow {
    pub id: String,
    pub content: String,
    pub layer: String,
    pub category: Option<String>,
    pub vector_json: Option<String>,
    pub vector_blob: Option<Vec<u8>>,
    pub metadata_json: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub is_active: bool,
    pub agent_id: Option<String>,
    pub evolution_state: Option<String>,
    pub is_head: bool,
    pub topic_key: Option<String>,
    pub root_id: Option<String>,
    pub version: Option<i64>,
    pub review_after: Option<String>,
    pub superseded_by: Option<String>,
    pub room: Option<String>,
    pub project_id: Option<String>,
    pub confidence: Option<f64>,
    pub recall_count: i64,
}

impl NoteRow {
    /// Read a NoteRow from a rusqlite Row.
    ///
    /// Expects columns by name (use `SELECT n.*` or explicit column list).
    pub fn from_row(row: &rusqlite::Row<'_>) -> SqlResult<Self> {
        Ok(Self {
            id: row.get("id")?,
            content: row.get("content")?,
            layer: row.get("layer")?,
            category: row.get("category")?,
            vector_json: row.get("vector_json").ok().flatten(),
            vector_blob: row.get("vector_blob").ok().flatten(),
            metadata_json: row.get("metadata_json").ok().flatten(),
            created_at: row.get("created_at").ok().flatten(),
            updated_at: row.get("updated_at").ok().flatten(),
            is_active: row
                .get::<_, i64>("is_active")
                .map(|v| v != 0)
                .unwrap_or(true),
            agent_id: row.get("agent_id").ok().flatten(),
            evolution_state: row.get("evolution_state").ok().flatten(),
            is_head: row.get::<_, i64>("is_head").map(|v| v != 0).unwrap_or(true),
            topic_key: row.get("topic_key").ok().flatten(),
            root_id: row.get("root_id").ok().flatten(),
            version: row.get::<_, Option<i64>>("version").ok().flatten(),
            review_after: row.get("review_after").ok().flatten(),
            // superseded_by is in metadata_json, not a direct column
            superseded_by: None,
            room: row.get("room").ok().flatten(),
            project_id: row.get("project_id").ok().flatten(),
            confidence: row.get("confidence").ok(),
            recall_count: row.get::<_, i64>("recall_count").unwrap_or(0),
        })
    }
}

/// A candidate from FTS5 or vector retrieval, with optional BM25 score.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub note: NoteRow,
    pub bm25_score: Option<f64>,
}

/// A scored search result ready for output.
#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub id: String,
    pub content: String,
    pub layer: String,
    pub category: Option<String>,
    pub score: f64,
    pub created_at: Option<String>,
    pub topic_key: Option<String>,
    pub root_id: Option<String>,
    pub version: Option<i64>,
    pub channel: Option<String>,
    pub temperature: Option<String>,
    pub is_head: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub recall_count: i64,
    pub last_recalled_at: Option<String>,
}

/// Diagnostic counters emitted alongside a search result batch. Used by the
/// MCP layer to surface "silent skip" conditions back to the user (REL-02).
#[derive(Debug, Clone, Default, Serialize)]
pub struct SearchDiagnostics {
    /// Number of vector-candidate rows skipped because their stored vector
    /// dimensionality did not match the query vector's. In practice this is
    /// the legacy 384-dim MiniLM rows that predate the bge-m3 1024-dim
    /// migration; they would otherwise silently hit the cosine-zero path and
    /// never surface to the user.
    pub dim_mismatch_skipped: usize,
    /// Number of results added by neural spreading activation.
    pub activation_result_count: usize,
    /// Deepest activation hop that contributed a result in this batch.
    pub activation_max_depth: usize,
    /// Activation relation types used by the returned association results.
    pub activation_relation_type_counts: std::collections::BTreeMap<String, usize>,
    /// Number of returned activation results that came from dream evidence.
    pub activation_dream_evidence_count: usize,
    /// Wall-clock time spent in the activation stage.
    pub activation_latency_ms: u128,
}

/// Parameters for the search pipeline.
#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub query: String,
    pub limit: usize,
    pub layer: Option<String>,
    pub category: Option<String>,
    pub only_active: bool,
    pub agent_id: Option<String>,
    pub project_id: Option<String>,
    pub as_of: Option<String>,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub include_expired: bool,
    pub cross_project: bool,
    pub search_mode: Option<String>,
    pub room: Option<String>,
    pub min_score: f64,
    pub boost_categories: Option<Vec<String>>,
    pub include_constitution: bool,
}

/// Parse an ISO 8601 datetime string into chrono DateTime.
pub fn parse_datetime(s: &str) -> Option<DateTime<Utc>> {
    // Handle "Z" suffix and various ISO formats
    let cleaned = s.trim().replace("Z", "+00:00");
    DateTime::parse_from_rfc3339(&cleaned)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| {
            // Try without timezone (assume UTC)
            chrono::NaiveDateTime::parse_from_str(&cleaned, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|ndt| ndt.and_utc())
        })
}

// --- Row types for Dreaming Cognitive Lifecycle (TODO-R4, 2026-04-13) ---

/// A row from the `dream_candidates` table.
/// Mirrors the Python schema in storage_schema.py.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DreamCandidateRow {
    pub id: String,
    pub project_id: String,
    pub source_type: String,
    pub source_id: Option<String>,
    pub summary: String,
    pub hypothesis: Option<String>,
    pub confidence: f64,
    pub frequency: i64,
    pub evidence_ids: Option<String>, // JSON array of event_log IDs
    pub verdict: String,              // pending|promoted|held|discarded
    pub promoted_to: Option<String>,
    pub evaluator_notes: Option<String>,
    pub writer_status: Option<String>,
    pub writer_reason: Option<String>,
    pub created_at: String,
    pub evaluated_at: Option<String>,
    pub promoted_at: Option<String>,
    pub discarded_at: Option<String>,
}

impl DreamCandidateRow {
    /// Read a DreamCandidateRow from a rusqlite Row.
    pub fn from_row(row: &rusqlite::Row<'_>) -> SqlResult<Self> {
        Ok(Self {
            id: row.get("id")?,
            project_id: row.get("project_id")?,
            source_type: row.get("source_type")?,
            source_id: row.get("source_id").ok().flatten(),
            summary: row.get("summary")?,
            hypothesis: row.get("hypothesis").ok().flatten(),
            confidence: row.get::<_, f64>("confidence").unwrap_or(0.0),
            frequency: row.get::<_, i64>("frequency").unwrap_or(1),
            evidence_ids: row.get("evidence_ids").ok().flatten(),
            verdict: row
                .get::<_, String>("verdict")
                .unwrap_or_else(|_| "pending".to_string()),
            promoted_to: row.get("promoted_to").ok().flatten(),
            evaluator_notes: row.get("evaluator_notes").ok().flatten(),
            writer_status: row.get("writer_status").ok().flatten(),
            writer_reason: row.get("writer_reason").ok().flatten(),
            created_at: row.get("created_at")?,
            evaluated_at: row.get("evaluated_at").ok().flatten(),
            promoted_at: row.get("promoted_at").ok().flatten(),
            discarded_at: row.get("discarded_at").ok().flatten(),
        })
    }
}

/// A row from the `dream_evolution_log` table.
/// Empty in v6.0; populated by Evolver in v6.1 (TODO-R5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DreamEvolutionLogRow {
    pub id: String,
    pub project_id: String,
    pub run_at: String,
    pub period_start: String,
    pub period_end: String,
    pub promoted_count: Option<i64>,
    pub confirmed_count: Option<i64>,
    pub corrected_count: Option<i64>,
    pub outdated_count: Option<i64>,
    pub precision_rate: Option<f64>,
    pub weight_adjustments: Option<String>,     // JSON
    pub discard_patterns_added: Option<String>, // JSON
    pub llm_model: Option<String>,
    pub llm_tokens_used: Option<i64>,
    pub created_at: String,
}

impl DreamEvolutionLogRow {
    /// Read a DreamEvolutionLogRow from a rusqlite Row.
    pub fn from_row(row: &rusqlite::Row<'_>) -> SqlResult<Self> {
        Ok(Self {
            id: row.get("id")?,
            project_id: row.get("project_id")?,
            run_at: row.get("run_at")?,
            period_start: row.get("period_start")?,
            period_end: row.get("period_end")?,
            promoted_count: row.get("promoted_count").ok().flatten(),
            confirmed_count: row.get("confirmed_count").ok().flatten(),
            corrected_count: row.get("corrected_count").ok().flatten(),
            outdated_count: row.get("outdated_count").ok().flatten(),
            precision_rate: row.get("precision_rate").ok().flatten(),
            weight_adjustments: row.get("weight_adjustments").ok().flatten(),
            discard_patterns_added: row.get("discard_patterns_added").ok().flatten(),
            llm_model: row.get("llm_model").ok().flatten(),
            llm_tokens_used: row.get("llm_tokens_used").ok().flatten(),
            created_at: row.get("created_at")?,
        })
    }
}
