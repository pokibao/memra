//! Cross-backend parity test for L0 approver canonicalization.
//!
//! Mirror of the archived v6 `backend/tests/test_approver_parity.py`. R4 keeps
//! the parity cases as Rust-owned fixture data under `memra-core/tests/fixtures/`
//! so future changes to `canonicalize_approver` still preserve the old attack
//! scenario coverage without keeping active Python tests.
//!
//! `canonicalize_approver` is module-private; this integration test
//! exposes it via the existing `GovernanceService::approve_change_for_principal`
//! semantics: two inputs produce the same canonical key iff registering
//! the second after the first is rejected as a duplicate. We use that
//! contract instead of a direct function call to avoid widening the
//! crate's public surface area for tests.

use memra_core::governance::GovernanceService;
use memra_core::storage::db::DbPool;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct SharedCase {
    id: String,
    input: String,
    canonical: String,
}

#[derive(Debug, Deserialize)]
struct DivergenceCase {
    id: String,
    input: String,
    python_canonical: String,
    rust_canonical: String,
    #[allow(dead_code)]
    rationale: String,
}

#[derive(Debug, Deserialize)]
struct Fixture {
    schema_version: u32,
    shared_cases: Vec<SharedCase>,
    known_divergences: Vec<DivergenceCase>,
}

fn load_fixture() -> Fixture {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("fixtures");
    path.push("approver_canonicalization.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture at {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("parse fixture json")
}

fn fresh_gov() -> GovernanceService {
    let pool = DbPool::open(std::path::Path::new(":memory:")).expect("open in-memory db");
    pool.with_conn(|conn| {
        // The governance fixture only needs constitution_changes; apply_change_inner
        // never runs because we never reach the 3rd approval here.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS notes (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                layer TEXT NOT NULL,
                category TEXT,
                is_active INTEGER NOT NULL DEFAULT 1,
                confidence REAL,
                project_id TEXT,
                created_at TEXT,
                updated_at TEXT,
                valid_at REAL,
                metadata_json TEXT,
                is_head INTEGER NOT NULL DEFAULT 1,
                evolution_state TEXT,
                review_after TEXT,
                room TEXT,
                agent TEXT,
                difficulty INTEGER,
                time_cost_hint TEXT,
                related_ids_json TEXT,
                role TEXT,
                session_id TEXT,
                version INTEGER DEFAULT 1,
                root_id TEXT,
                cold_storage_ref TEXT,
            event_when        TEXT,
            event_when_ts     REAL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(note_id UNINDEXED, content);",
        )
        .expect("notes schema");
    });
    let gov = GovernanceService::new(pool);
    gov.init_table();
    gov
}

/// Probe whether two inputs canonicalize to the same key by trying to
/// register both as approvers on a fresh proposal. Returns true iff
/// the second registration is rejected as a duplicate (= same key).
fn canonicalizes_equal(first: &str, second: &str) -> bool {
    let gov = fresh_gov();
    let proposal = gov
        .propose_change("parity probe", "test", None, None, None, None)
        .expect("propose");
    gov.approve_change_for_principal(&proposal.id, "actor-1", Some(first), None)
        .expect("first approval");
    let result = gov.approve_change_for_principal(&proposal.id, "actor-2", Some(second), None);
    match result {
        Err(e) if e.contains("already approved by this approver") => true,
        Err(e) if e.contains("must not be empty") => {
            // Both inputs canonicalize to empty — treat as "equal" but
            // surface the empty case explicitly so callers can spot it.
            first_canonicalizes_empty(first) && first_canonicalizes_empty(second)
        }
        Err(e) => panic!("unexpected error from second approval: {e}"),
        Ok(_) => false,
    }
}

fn first_canonicalizes_empty(input: &str) -> bool {
    let gov = fresh_gov();
    let proposal = gov
        .propose_change("empty probe", "test", None, None, None, None)
        .expect("propose");
    matches!(
        gov.approve_change_for_principal(&proposal.id, "actor-1", Some(input), None),
        Err(ref e) if e.contains("must not be empty")
    )
}

#[test]
fn fixture_is_well_formed() {
    let fx = load_fixture();
    assert_eq!(fx.schema_version, 1);
    assert!(
        !fx.shared_cases.is_empty(),
        "fixture must have shared cases"
    );
    let mut seen = std::collections::HashSet::new();
    for case in fx
        .shared_cases
        .iter()
        .map(|c| &c.id)
        .chain(fx.known_divergences.iter().map(|c| &c.id))
    {
        assert!(seen.insert(case.clone()), "duplicate fixture id: {case}");
    }
}

#[test]
fn rust_matches_shared_canonical() {
    let fx = load_fixture();
    let mut failures: Vec<String> = Vec::new();
    for case in &fx.shared_cases {
        // Compare via the equivalence-class probe: every shared case's
        // input MUST canonicalize equal to its `canonical` value.
        if !canonicalizes_equal(&case.input, &case.canonical) {
            failures.push(format!(
                "id={:?} input={:?} canonical={:?} — Rust did not collapse them",
                case.id, case.input, case.canonical
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} shared-case parity failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn rust_known_divergences_still_diverge() {
    let fx = load_fixture();
    let mut failures: Vec<String> = Vec::new();
    for case in &fx.known_divergences {
        // The Rust expectation must hold (input canonicalizes equal to
        // its rust_canonical value).
        if !canonicalizes_equal(&case.input, &case.rust_canonical) {
            failures.push(format!(
                "id={:?} input={:?} rust_canonical={:?} — Rust does not match its own expected key",
                case.id, case.input, case.rust_canonical
            ));
        }
        // The Python and Rust expectations must still be distinct;
        // otherwise the case has converged and should move to
        // shared_cases.
        if case.python_canonical == case.rust_canonical {
            failures.push(format!(
                "id={:?}: python_canonical and rust_canonical now match ({:?}); move to shared_cases",
                case.id, case.python_canonical
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} divergence-case failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}
