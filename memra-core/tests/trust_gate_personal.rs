use memra_core::personal::{allows_strengthening, is_ai_provisional_source, is_trusted_memory_source};

#[test]
fn trust_gate_personal_trusts_user_and_checkpoint_sources() {
    assert!(is_trusted_memory_source("user"));
    assert!(is_trusted_memory_source(" checkpoint "));
    assert!(is_trusted_memory_source("USER"));
}

#[test]
fn trust_gate_personal_marks_ai_sources_as_provisional() {
    for source in [
        "ai",
        "ai_extraction",
        "ai_proposal",
        "dream_promoter",
        "external_ai",
    ] {
        assert!(
            is_ai_provisional_source(source),
            "{source} should remain provisional"
        );
    }
}

#[test]
fn trust_gate_personal_blocks_unconfirmed_ai_strengthening() {
    assert!(!allows_strengthening(Some("ai"), false, true));
    assert!(!allows_strengthening(Some("dream_promoter"), false, true));
    assert!(!allows_strengthening(Some("external_ai"), false, true));
}

#[test]
fn trust_gate_personal_allows_confirmed_candidates() {
    assert!(allows_strengthening(Some("ai"), true, true));
    assert!(allows_strengthening(Some("ai_proposal"), true, true));
}

#[test]
fn trust_gate_personal_preserves_legacy_missing_source_policy() {
    assert!(allows_strengthening(None, false, true));
    assert!(!allows_strengthening(None, false, false));
}
