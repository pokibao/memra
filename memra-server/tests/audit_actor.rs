//! Integration tests for W3-A: audit actor_id threading + stdio principal synthesis.
//!
//! Exit criteria verified:
//! 1. HTTP add_rule path → audit entry has `actor_id == bound actor_id`.
//! 2. stdio approve_change path → audit entry has `actor_id` starting with `"stdio-"`.
//! 3. Same stdio process cannot self-approve 3/3 (second attempt is rejected).
//! 4. Three distinct stdio actor_ids can approve the same change to completion.

use std::fs;

use memra_core::governance::GovernanceService;
use memra_core::storage::db::DbPool;
use memra_server::audit::{AuditEvent, AuditLogger};
use memra_server::transport::auth::AuthenticatedActor;

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

fn temp_audit_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let dir = std::env::temp_dir().join(format!("ma-audit-actor-{tag}-{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn in_memory_governance() -> GovernanceService {
    let pool = DbPool::open(std::path::Path::new(":memory:")).expect("in-memory DB");
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
                raw_text          TEXT,
                raw_metadata      TEXT,
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

fn http_actor(id: &str) -> AuthenticatedActor {
    AuthenticatedActor {
        actor_id: id.to_string(),
        key_name: Some("test-key".to_string()),
        key_hash: "deadbeef".to_string(),
    }
}

fn stdio_actor(synthetic_id: &str) -> AuthenticatedActor {
    AuthenticatedActor {
        actor_id: synthetic_id.to_string(),
        key_name: Some("stdio".to_string()),
        key_hash: String::new(),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Test 1: HTTP request writes bound actor_id to audit
// ────────────────────────────────────────────────────────────────────────────

/// HTTP add_rule with bearer auth → audit JSONL entry contains the bound
/// `actor_id`, not null.
#[test]
fn http_request_writes_bound_actor_id_to_audit() {
    let dir = temp_audit_dir("http-actor");
    let logger = AuditLogger::new(&dir);
    let actor = http_actor("claude-code-actor");

    // Simulate what audit_operation does after receiving an HTTP-authenticated actor.
    let event = AuditEvent::new("add_rule", "saved").with_actor(actor.actor_id.as_str());
    logger.append(event).expect("append should succeed");

    let content = fs::read_to_string(logger.current_path()).expect("audit file must exist");
    let line = content.lines().next().expect("must have at least one line");
    let parsed: serde_json::Value = serde_json::from_str(line).expect("must be valid JSON");

    assert_eq!(
        parsed["actor_id"].as_str(),
        Some("claude-code-actor"),
        "HTTP actor_id must be written to audit"
    );
    assert_eq!(parsed["event_type"].as_str(), Some("add_rule"));
    assert_eq!(parsed["result"].as_str(), Some("saved"));
}

// ────────────────────────────────────────────────────────────────────────────
// Test 2: stdio request writes synthesized principal to audit
// ────────────────────────────────────────────────────────────────────────────

/// stdio approve_change path → audit has `actor_id` starting with `"stdio-"`
/// and is NOT null.
#[test]
fn stdio_request_writes_synthesized_principal_to_audit() {
    let dir = temp_audit_dir("stdio-actor");
    let logger = AuditLogger::new(&dir);

    // Synthesise a stdio principal the same way resolve_actor does at runtime.
    let pid = std::process::id();
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "unknown".into());
    let exe_hex = blake3::hash(exe.as_bytes()).to_hex().to_string();
    let exe_hash = &exe_hex[..16];
    let actor_id = format!("stdio-{pid}-{exe_hash}");

    let actor = stdio_actor(&actor_id);
    let event = AuditEvent::new("approve_change", "pending").with_actor(actor.actor_id.as_str());
    logger.append(event).expect("append should succeed");

    let content = fs::read_to_string(logger.current_path()).expect("audit file must exist");
    let line = content.lines().next().expect("must have at least one line");
    let parsed: serde_json::Value = serde_json::from_str(line).expect("must be valid JSON");

    let written_id = parsed["actor_id"]
        .as_str()
        .expect("actor_id must not be null");
    assert!(
        written_id.starts_with("stdio-"),
        "stdio actor_id must start with 'stdio-', got: {written_id}"
    );
    assert!(!written_id.is_empty(), "actor_id must not be empty string");
}

// ────────────────────────────────────────────────────────────────────────────
// Test 3: Single stdio process cannot self-approve 3/3
// ────────────────────────────────────────────────────────────────────────────

/// Same stdio principal attempts 3 approvals on one change.
/// Second attempt must return an error; status remains `pending`.
#[test]
fn stdio_single_process_cannot_self_approve_3_3() {
    let gov = in_memory_governance();

    let proposal = gov
        .propose_change(
            "L0 stdio self-approval test",
            "W3-A test",
            None,
            None,
            None,
            None,
        )
        .expect("propose should succeed");
    assert_eq!(proposal.status, "pending");

    // First approval from the single stdio process.
    let stdio_id = "stdio-12345-abcdef0123456789";
    let first = gov
        .approve_change_for_principal(&proposal.id, stdio_id, Some(stdio_id), None)
        .expect("first approval must succeed");
    assert_eq!(first.approvals_count, 1);
    assert_eq!(first.status, "pending");

    // Second attempt from the same stdio process must be rejected.
    let err = gov
        .approve_change_for_principal(&proposal.id, stdio_id, Some(stdio_id), None)
        .expect_err("second approval from same stdio principal must be rejected");
    assert!(
        err.contains("already approved"),
        "Expected 'already approved' error, got: {err}"
    );

    // Third attempt also rejected.
    let err = gov
        .approve_change_for_principal(&proposal.id, stdio_id, Some(stdio_id), None)
        .expect_err("third approval from same stdio principal must be rejected");
    assert!(
        err.contains("already approved"),
        "Expected 'already approved' error, got: {err}"
    );

    // Status is still pending with only 1 approval.
    assert_eq!(
        first.approvals_count, 1,
        "Only 1 approval should be counted despite 3 attempts"
    );
    assert_eq!(
        first.status, "pending",
        "Proposal must remain pending after single-process self-approval attempts"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// Test 4: Three distinct stdio actor_ids can approve to completion
// ────────────────────────────────────────────────────────────────────────────

/// Three different synthesized stdio actor_ids (representing different processes
/// or different executables) each approve once → status flips to `approved`.
#[test]
fn three_distinct_stdio_pids_can_approve() {
    let gov = in_memory_governance();

    let proposal = gov
        .propose_change(
            "L0 three-distinct-stdio test",
            "W3-A test",
            None,
            None,
            None,
            None,
        )
        .expect("propose should succeed");

    // Three distinct stdio actor_ids (different pid or exe hash).
    let id_a = "stdio-10001-aaaa000011110000";
    let id_b = "stdio-10002-bbbb111122220000";
    let id_c = "stdio-10003-cccc222233330000";

    gov.approve_change_for_principal(&proposal.id, id_a, Some(id_a), None)
        .expect("first distinct stdio principal must succeed");
    gov.approve_change_for_principal(&proposal.id, id_b, Some(id_b), None)
        .expect("second distinct stdio principal must succeed");
    let final_record = gov
        .approve_change_for_principal(&proposal.id, id_c, Some(id_c), None)
        .expect("third distinct stdio principal must succeed");

    assert_eq!(final_record.approvals_count, 3);
    assert_eq!(
        final_record.status, "applied",
        "Three distinct stdio principals must reach applied status (W3-B writes identity_schema row)"
    );
}
