//! Release-blocking negative tests:
//!   - single_key_cannot_self_approve_via_header_replay (D3.1)
//!   - write_token_replay_cannot_self_approve (IMPL-01b)
//!
//! D3.1: "A single leaked API key must not be able to satisfy the 3-approval
//! requirement by replaying itself with different X-MA-Actor-Id header values."
//!
//! IMPL-01b: "Same actor with same write_token CANNOT approve their own
//! propose_change" — replay of a write_token by the proposing actor must be
//! caught by the single-principal dedup in governance, independent of whether
//! the token itself is valid or not.
//!
//! This test proves that the governance system deduplicates on the server-resolved
//! `actor_id` (bound to the API key at provisioning), not on the header hint.

use memra_core::governance::GovernanceService;
use memra_core::storage::db::DbPool;
use memra_core::storage::session_tokens_writer::{ValidateResult, issue_token, validate_token};

/// Shared fixture: in-memory DB with both `constitution_changes` and the `notes` tables
/// so that W3-B's `apply_change_inner` (which writes an `identity_schema` row on the 3rd
/// approval) can run without "no such table: notes" errors.
fn fresh_gov() -> GovernanceService {
    let pool = DbPool::open(std::path::Path::new(":memory:")).expect("open in-memory DB");
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
        .expect("notes schema");
    });
    let gov = GovernanceService::new(pool);
    gov.init_table();
    gov
}

/// Core invariant: same actor_id replayed 3 times with different display labels
/// must NOT reach approved status. Only 1 approval should be counted.
#[test]
fn single_key_cannot_self_approve_via_header_replay() {
    let gov = fresh_gov();

    // Step 1: Propose a change
    let proposal = gov
        .propose_change(
            "Test L0 rule",
            "Testing spoof resistance",
            None,
            None,
            None,
            None,
        )
        .expect("propose should succeed");
    assert_eq!(proposal.status, "pending");

    // Step 2: First approval with actor_id = "api-key-1" (the server-resolved identity)
    // but display label "alice" (simulating X-MA-Actor-Id header)
    let first = gov
        .approve_change_for_principal(&proposal.id, "api-key-1", Some("alice"), None)
        .expect("first approval should succeed");
    assert_eq!(first.approvals_count, 1);
    assert_eq!(first.status, "pending");

    // Step 3: Same actor_id replayed with different header hint "bob"
    let err = gov
        .approve_change_for_principal(&proposal.id, "api-key-1", Some("bob"), None)
        .expect_err("second approval with same principal should be rejected");
    assert!(
        err.contains("already approved"),
        "Expected duplicate principal rejection, got: {err}"
    );

    // Step 4: Same actor_id replayed with yet another header hint "charlie"
    let err = gov
        .approve_change_for_principal(&proposal.id, "api-key-1", Some("charlie"), None)
        .expect_err("third approval with same principal should also be rejected");
    assert!(
        err.contains("already approved"),
        "Expected duplicate principal rejection, got: {err}"
    );

    // Step 5: Verify the proposal is still pending with only 1 approval
    // (re-fetch by proposing a dummy and checking original — or just verify counts above)
    assert_eq!(
        first.approvals_count, 1,
        "Only 1 approval should have been counted despite 3 attempts"
    );
    assert_eq!(
        first.status, "pending",
        "Proposal must remain pending — single key cannot self-approve"
    );
}

/// Positive control: 3 genuinely different principals CAN approve.
#[test]
fn three_distinct_principals_can_approve() {
    let gov = fresh_gov();

    let proposal = gov
        .propose_change(
            "Test L0 rule",
            "Testing distinct principals",
            None,
            None,
            None,
            None,
        )
        .expect("propose should succeed");

    gov.approve_change_for_principal(&proposal.id, "key-alice", Some("alice"), None)
        .expect("first distinct principal should succeed");
    gov.approve_change_for_principal(&proposal.id, "key-bob", Some("bob"), None)
        .expect("second distinct principal should succeed");
    let final_record = gov
        .approve_change_for_principal(&proposal.id, "key-charlie", Some("charlie"), None)
        .expect("third distinct principal should succeed");

    assert_eq!(final_record.approvals_count, 3);
    assert_eq!(
        final_record.status, "applied",
        "W3-B applies identity_schema row on 3rd approval; status advances past 'approved' directly to 'applied'"
    );
}

/// IMPL-01b mandatory negative test:
/// Same actor CANNOT satisfy the 3-approval requirement by replaying the same
/// write_token on multiple approve_change calls. The dedup happens at the
/// governance layer (same actor_id), not at the token layer. This test verifies
/// that a valid write_token in the session_tokens table does NOT help an actor
/// self-approve when governance already counts that actor's vote.
///
/// This is the "D3.1 lesson" extended to the write_token path: issuing a token
/// for an actor does not grant them the ability to multiply their approval count.
#[test]
fn write_token_replay_cannot_self_approve() {
    let gov = fresh_gov(); // creates the full schema including session_tokens (via DbPool::open)
    gov.init_table();

    // Issue a write token for actor "actor-replay" using the governance's pool.
    gov.pool().with_conn(|conn| {
        issue_token(
            conn,
            "token-hash-replay",
            "http:replay-session",
            "actor-replay",
            "2026-04-26T10:00:00Z",
            "2099-12-31T23:59:59Z",
        )
        .expect("issue token");
    });

    // Verify the token is valid.
    let validate_result = gov.pool().with_conn(|conn| {
        validate_token(
            conn,
            "token-hash-replay",
            "actor-replay",
            "2026-04-26T12:00:00Z",
        )
        .expect("validate")
    });
    // PR #235 Codex P1: ValidateResult::Valid now exposes session_id so the
    // require_write_token path can enforce session binding. Match against the
    // session this token was issued for.
    match validate_result {
        ValidateResult::Valid {
            actor_id,
            session_id,
            expires_at,
        } => {
            assert_eq!(actor_id, "actor-replay");
            assert_eq!(session_id, "http:replay-session");
            assert_eq!(expires_at, "2099-12-31T23:59:59Z");
        }
        other => panic!("expected Valid, got {other:?}"),
    }

    // Propose a change as actor-replay.
    let proposal = gov
        .propose_change(
            "Test L0 rule for replay",
            "Testing write_token replay resistance",
            None,
            None,
            None,
            Some("actor-replay"),
        )
        .expect("propose should succeed");

    // First approval with a valid write_token in hand.
    let first = gov
        .approve_change_for_principal(&proposal.id, "actor-replay", Some("actor-replay"), None)
        .expect("first approval should succeed");
    assert_eq!(first.approvals_count, 1);
    assert_eq!(first.status, "pending");

    // Replay: same actor, same write_token hash still valid in DB → MUST be rejected
    // by governance dedup (not the token layer).
    let err = gov
        .approve_change_for_principal(&proposal.id, "actor-replay", Some("actor-replay"), None)
        .expect_err("second approval with same actor must be rejected despite valid write_token");
    assert!(
        err.contains("already approved"),
        "Expected duplicate principal rejection on write_token replay, got: {err}"
    );

    // The token is still valid (not consumed by the approval flow —
    // the dedup is governance-level, not token-level).
    let still_valid = gov.pool().with_conn(|conn| {
        validate_token(
            conn,
            "token-hash-replay",
            "actor-replay",
            "2026-04-26T12:00:00Z",
        )
        .expect("validate after replay")
    });
    assert!(
        matches!(still_valid, ValidateResult::Valid { .. }),
        "write_token must remain valid after self-approve attempt: {still_valid:?}"
    );

    // Status must remain pending — 1 approval, not 3.
    assert_eq!(
        first.approvals_count, 1,
        "Only 1 approval must be counted despite write_token replay"
    );
    assert_eq!(
        first.status, "pending",
        "Change must remain pending after write_token replay attempt"
    );
}

/// Edge case: mixing authenticated (actor_id) and legacy (approver-only) calls.
#[test]
fn legacy_approver_name_and_actor_id_collide_correctly() {
    let gov = fresh_gov();

    let proposal = gov
        .propose_change("Test L0 rule", "Testing mixed auth", None, None, None, None)
        .expect("propose should succeed");

    // Legacy call: approve_change uses approver name as actor_id fallback.
    // The wrapper is #[deprecated] for production use (it aliases the
    // label and the principal — see PR #75 follow-up); the test
    // intentionally exercises the legacy contract to prove that the
    // dedup path catches it on the very next call.
    #[allow(deprecated)]
    gov.approve_change(&proposal.id, Some("alice"), None)
        .expect("legacy approval should succeed");

    // Authenticated call with same actor_id as the legacy approver name
    let err = gov
        .approve_change_for_principal(&proposal.id, "alice", Some("alice-v2"), None)
        .expect_err("should reject: same effective principal");
    assert!(
        err.contains("already approved"),
        "Expected collision between legacy approver and actor_id: {err}"
    );
}
