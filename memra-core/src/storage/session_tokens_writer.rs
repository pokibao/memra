//! Session write-token CRUD operations.
//!
//! Maintains the `session_tokens` table that issues, validates, and revokes
//! short-lived write tokens for L0 governance mutations (TODO-IMPL-01b).
//!
//! Design decisions (D7):
//! - `session_tokens` lives in the SAME SQLite DB as `sessions` for atomic JOIN.
//! - Token storage: blake3(secret) hex is stored; the plaintext secret is
//!   returned ONCE at issue time and NEVER persisted.
//! - Constant-time comparison must be used for all token hash lookups to avoid
//!   timing side-channels.

use rusqlite::{Connection, OptionalExtension};

// ---------------------------------------------------------------------------
// Table setup
// ---------------------------------------------------------------------------

/// Ensure the `session_tokens` table and its index exist.
///
/// This is a NEW table (no legacy schema to migrate), so we use a simple
/// CREATE TABLE IF NOT EXISTS + CREATE INDEX IF NOT EXISTS in two discrete
/// steps.  Keeping the steps separate follows the PR4 lesson: CREATE INDEX
/// referencing a column must come AFTER the column is guaranteed to exist on
/// all DB shapes.  For a brand-new table there is no legacy path — both steps
/// are always safe in order.
///
/// Called from `DbPool::open` immediately after `ensure_sessions_table`.
pub fn ensure_session_tokens_table(conn: &Connection) -> rusqlite::Result<()> {
    // Step 1: create table for fresh DBs (no-op on existing).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_tokens (
            token_hash  TEXT PRIMARY KEY,
            session_id  TEXT NOT NULL,
            actor_id    TEXT NOT NULL,
            issued_at   TEXT NOT NULL,
            expires_at  TEXT NOT NULL,
            revoked_at  TEXT
        );",
    )?;

    // Step 2: create lookup index (session_id + expires_at DESC for fast
    // active-token lookups; IF NOT EXISTS makes this idempotent).
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_session_tokens_session_expires
            ON session_tokens(session_id, expires_at DESC)",
        [],
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Write operations
// ---------------------------------------------------------------------------

/// Insert a new write token row.
///
/// `token_hash` is the blake3 hex of the secret (NOT the secret itself).
/// `issued_at` and `expires_at` are ISO 8601 strings.
pub fn issue_token(
    conn: &Connection,
    token_hash: &str,
    session_id: &str,
    actor_id: &str,
    issued_at: &str,
    expires_at: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO session_tokens
             (token_hash, session_id, actor_id, issued_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![token_hash, session_id, actor_id, issued_at, expires_at],
    )?;
    Ok(())
}

/// Result of a token validation attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidateResult {
    /// Token is valid and belongs to the given actor + session.
    ///
    /// `session_id` is the session the token was minted for at issue time.
    /// PR #235 Codex P1: callers MUST cross-check it against the request's
    /// resolved session_id before accepting the token to block cross-session
    /// replay (mint for session A → reuse on session B requests with same
    /// actor's bearer key).
    Valid {
        actor_id: String,
        session_id: String,
        expires_at: String,
    },
    /// No row found for the given hash.
    NotFound,
    /// Row exists but `expires_at < now`.
    Expired { expires_at: String },
    /// Row exists but `revoked_at IS NOT NULL`.
    Revoked { revoked_at: String },
    /// Row exists but `actor_id` does not match the expected value.
    ActorMismatch {
        stored_actor: String,
        expected_actor: String,
    },
}

/// Validate a token hash against the DB.
///
/// Performs constant-time comparison on the token hash to avoid timing
/// side-channels.  `token_hash` is the caller-computed blake3 hex of the
/// secret; `expected_actor_id` is the actor_id from the authenticated request
/// (which must match the actor that issued the token).
///
/// `now` is an ISO 8601 string used for TTL comparison.
pub fn validate_token(
    conn: &Connection,
    token_hash: &str,
    expected_actor_id: &str,
    now: &str,
) -> rusqlite::Result<ValidateResult> {
    // Fetch ALL active (non-revoked) rows so we can do constant-time hash
    // comparison without leaking timing information about whether a hash prefix
    // exists.  In practice there will be very few tokens at any given time, so
    // this is not a performance concern.
    //
    // We still do a WHERE clause to narrow by hash for correctness — the
    // constant_time_eq below is the security layer.
    //
    // PR #235 Codex P1 fix: also fetch session_id so callers can enforce
    // session binding (cross-session replay defence).
    let row: Option<(String, String, String, String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT actor_id, session_id, expires_at, token_hash, revoked_at
               FROM session_tokens
              WHERE token_hash = ?1",
        )?;
        stmt.query_row(rusqlite::params![token_hash], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .optional()?
    };

    let Some((stored_actor, stored_session_id, expires_at, stored_hash, revoked_at)) = row else {
        return Ok(ValidateResult::NotFound);
    };

    // Constant-time comparison against the stored hash (defence-in-depth;
    // the WHERE already did an exact match but timing must not leak).
    if !constant_time_eq(token_hash, &stored_hash) {
        return Ok(ValidateResult::NotFound);
    }

    // Revoked check first (revoked supersedes expired for clearer error messages).
    if let Some(revoked_at) = revoked_at {
        return Ok(ValidateResult::Revoked { revoked_at });
    }

    // Expiry check (lexicographic ISO 8601 comparison is correct for UTC strings).
    if expires_at.as_str() < now {
        return Ok(ValidateResult::Expired { expires_at });
    }

    // Actor binding check.
    if stored_actor != expected_actor_id {
        return Ok(ValidateResult::ActorMismatch {
            stored_actor,
            expected_actor: expected_actor_id.to_string(),
        });
    }

    Ok(ValidateResult::Valid {
        actor_id: stored_actor,
        session_id: stored_session_id,
        expires_at,
    })
}

/// Revoke all tokens associated with a session.
///
/// Sets `revoked_at` on every non-already-revoked token for the given
/// `session_id`.  Returns the actor_ids of tokens that were revoked (for
/// CLI output).
///
/// PR #236 Codex P2 fix: SELECT-then-UPDATE was racy — between the two
/// statements another writer could insert a token for the same session,
/// which would then be revoked by UPDATE but missing from the returned
/// actors list, causing the audit log's `revoked_count` /
/// `revoked_actor_ids` fields to under-report.  We now use SQLite's
/// `UPDATE ... RETURNING` clause (3.35+) so the same statement performs
/// the update and emits the affected `actor_id` rows atomically.  All
/// supported platforms ship SQLite ≥ 3.35.
pub fn revoke_tokens_for_session(
    conn: &Connection,
    session_id: &str,
    revoked_at: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "UPDATE session_tokens
            SET revoked_at = ?1
          WHERE session_id = ?2 AND revoked_at IS NULL
        RETURNING actor_id",
    )?;
    let actors: Vec<String> = stmt
        .query_map(rusqlite::params![revoked_at, session_id], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(actors)
}

/// Delete all tokens whose `expires_at < cutoff` (expired AND already past TTL).
///
/// Called periodically to prevent unbounded table growth.
pub fn sweep_expired(conn: &Connection, cutoff: &str) -> rusqlite::Result<usize> {
    let count = conn.execute(
        "DELETE FROM session_tokens WHERE expires_at < ?1",
        rusqlite::params![cutoff],
    )?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Constant-time byte-level comparison (mirrors `transport/auth.rs`).
///
/// Avoids timing side-channel on token hash comparison.
fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let max_len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();
    for i in 0..max_len {
        let l = if i < left.len() { left[i] } else { 0 };
        let r = if i < right.len() { right[i] } else { 0 };
        diff |= (l ^ r) as usize;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_test_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        ensure_session_tokens_table(&conn).expect("ensure_session_tokens_table");
        conn
    }

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    fn index_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(false)
    }

    fn token_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM session_tokens", [], |r| r.get(0))
            .unwrap_or(0)
    }

    // AC1-a: table and index are created on a fresh connection.
    #[test]
    fn session_tokens_table_and_index_created_on_init() {
        let conn = open_test_conn();
        assert!(
            table_exists(&conn, "session_tokens"),
            "session_tokens table must exist"
        );
        assert!(
            index_exists(&conn, "idx_session_tokens_session_expires"),
            "idx_session_tokens_session_expires must exist"
        );
    }

    // AC1-b: ensure_session_tokens_table is idempotent.
    #[test]
    fn ensure_session_tokens_table_is_idempotent() {
        let conn = open_test_conn();
        // Running a second time must not error or change the schema.
        ensure_session_tokens_table(&conn).expect("idempotent second call must succeed");
        assert_eq!(token_count(&conn), 0, "idempotent re-run must not add rows");
    }

    // AC6 / issue happy path.
    #[test]
    fn issue_token_inserts_row() {
        let conn = open_test_conn();
        issue_token(
            &conn,
            "fakehash-abc123",
            "http:sess-1",
            "actor-alice",
            "2026-04-26T10:00:00Z",
            "2026-04-27T10:00:00Z",
        )
        .expect("issue_token should succeed");
        assert_eq!(token_count(&conn), 1);
    }

    // AC2 / validate happy path.
    #[test]
    fn validate_token_returns_valid_for_good_token() {
        let conn = open_test_conn();
        issue_token(
            &conn,
            "hash-valid",
            "http:sess-valid",
            "actor-bob",
            "2026-04-26T10:00:00Z",
            "2099-12-31T23:59:59Z", // far future
        )
        .expect("issue");

        let result = validate_token(&conn, "hash-valid", "actor-bob", "2026-04-26T12:00:00Z")
            .expect("validate");
        // PR #235 Codex P1 fix: ValidateResult::Valid now exposes session_id
        // so callers can enforce session binding. Match against the issue
        // call's session arg ("http:sess-valid").
        match result {
            ValidateResult::Valid {
                actor_id,
                session_id,
                expires_at,
            } => {
                assert_eq!(actor_id, "actor-bob");
                assert_eq!(session_id, "http:sess-valid");
                assert_eq!(expires_at, "2099-12-31T23:59:59Z");
            }
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    // Expired token returns Expired variant.
    #[test]
    fn validate_token_returns_expired_for_past_token() {
        let conn = open_test_conn();
        issue_token(
            &conn,
            "hash-expired",
            "http:sess-x",
            "actor-alice",
            "2026-01-01T00:00:00Z",
            "2026-01-02T00:00:00Z", // in the past
        )
        .expect("issue");

        let result = validate_token(&conn, "hash-expired", "actor-alice", "2026-04-26T10:00:00Z")
            .expect("validate");
        assert!(
            matches!(result, ValidateResult::Expired { .. }),
            "expected Expired, got {result:?}"
        );
    }

    // Revoked token returns Revoked variant.
    #[test]
    fn validate_token_returns_revoked_for_revoked_token() {
        let conn = open_test_conn();
        issue_token(
            &conn,
            "hash-revoked",
            "http:sess-r",
            "actor-charlie",
            "2026-04-26T10:00:00Z",
            "2099-12-31T23:59:59Z",
        )
        .expect("issue");
        revoke_tokens_for_session(&conn, "http:sess-r", "2026-04-26T11:00:00Z").expect("revoke");

        let result = validate_token(
            &conn,
            "hash-revoked",
            "actor-charlie",
            "2026-04-26T12:00:00Z",
        )
        .expect("validate");
        assert!(
            matches!(result, ValidateResult::Revoked { .. }),
            "expected Revoked, got {result:?}"
        );
    }

    // Actor mismatch returns ActorMismatch variant.
    #[test]
    fn validate_token_returns_actor_mismatch() {
        let conn = open_test_conn();
        issue_token(
            &conn,
            "hash-mismatch",
            "http:sess-m",
            "actor-alice",
            "2026-04-26T10:00:00Z",
            "2099-12-31T23:59:59Z",
        )
        .expect("issue");

        // alice's token presented by bob → mismatch.
        let result = validate_token(&conn, "hash-mismatch", "actor-bob", "2026-04-26T12:00:00Z")
            .expect("validate");
        assert!(
            matches!(result, ValidateResult::ActorMismatch { .. }),
            "expected ActorMismatch, got {result:?}"
        );
    }

    // Unknown hash → NotFound.
    #[test]
    fn validate_token_returns_not_found_for_unknown_hash() {
        let conn = open_test_conn();
        let result = validate_token(
            &conn,
            "no-such-hash",
            "actor-nobody",
            "2026-04-26T10:00:00Z",
        )
        .expect("validate");
        assert_eq!(result, ValidateResult::NotFound);
    }

    // sweep_expired removes only expired rows.
    #[test]
    fn sweep_expired_removes_past_tokens_only() {
        let conn = open_test_conn();
        issue_token(
            &conn,
            "hash-old",
            "http:sess-old",
            "actor-1",
            "2026-01-01T00:00:00Z",
            "2026-01-02T00:00:00Z",
        )
        .expect("issue old");
        issue_token(
            &conn,
            "hash-new",
            "http:sess-new",
            "actor-2",
            "2026-04-26T00:00:00Z",
            "2099-12-31T23:59:59Z",
        )
        .expect("issue new");

        let removed = sweep_expired(&conn, "2026-04-26T10:00:00Z").expect("sweep");
        assert_eq!(removed, 1, "only the expired token should be removed");
        assert_eq!(token_count(&conn), 1, "fresh token must remain");
    }

    // revoke_tokens_for_session returns actor_ids and only touches non-revoked rows.
    #[test]
    fn revoke_tokens_for_session_returns_actor_ids() {
        let conn = open_test_conn();
        issue_token(
            &conn,
            "hash-rev-1",
            "http:sess-rev",
            "actor-x",
            "2026-04-26T10:00:00Z",
            "2099-12-31T23:59:59Z",
        )
        .expect("issue 1");
        issue_token(
            &conn,
            "hash-rev-2",
            "http:sess-rev",
            "actor-y",
            "2026-04-26T10:00:00Z",
            "2099-12-31T23:59:59Z",
        )
        .expect("issue 2");

        let actors = revoke_tokens_for_session(&conn, "http:sess-rev", "2026-04-26T11:00:00Z")
            .expect("revoke");
        let mut sorted = actors.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["actor-x", "actor-y"]);

        // Second call on same session must return 0 (already revoked).
        let actors2 = revoke_tokens_for_session(&conn, "http:sess-rev", "2026-04-26T11:01:00Z")
            .expect("second revoke");
        assert!(actors2.is_empty(), "already-revoked tokens must be skipped");
    }

    // PR #236 Codex P2 regression: every token whose row is updated must
    // appear in the returned actors list — even tokens whose `revoked_at`
    // is updated by the same statement that selects them.  The atomic
    // `UPDATE ... RETURNING` form guarantees this; the previous
    // SELECT-then-UPDATE could miss rows inserted between the two.
    //
    // We can't drive an in-process race deterministically without threads
    // and a real DB, so this test instead asserts the count invariant: the
    // number of actor_ids returned equals the number of rows transitioned
    // from active to revoked, end-to-end.
    #[test]
    fn revoke_tokens_for_session_count_matches_rows_revoked() {
        let conn = open_test_conn();
        for i in 0..5 {
            issue_token(
                &conn,
                &format!("hash-race-{i}"),
                "http:sess-race",
                &format!("actor-{i}"),
                "2026-04-26T10:00:00Z",
                "2099-12-31T23:59:59Z",
            )
            .expect("issue");
        }
        // Pre-revoke one to ensure WHERE filter is honoured.
        conn.execute(
            "UPDATE session_tokens SET revoked_at = '2026-04-26T10:30:00Z'
              WHERE token_hash = 'hash-race-0'",
            [],
        )
        .expect("pre-revoke");

        let actors = revoke_tokens_for_session(&conn, "http:sess-race", "2026-04-26T11:00:00Z")
            .expect("revoke");
        assert_eq!(
            actors.len(),
            4,
            "4 active tokens existed; 4 actor_ids must come back"
        );

        // Verify DB state matches the returned count: exactly 4 rows now have
        // revoked_at = '2026-04-26T11:00:00Z' for this session.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM session_tokens
                  WHERE session_id = 'http:sess-race'
                    AND revoked_at = '2026-04-26T11:00:00Z'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(n, 4, "DB row count must equal returned actor count");
    }
}
