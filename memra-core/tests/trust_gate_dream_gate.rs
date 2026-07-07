use memra_core::personal::{allows_strengthening, is_ai_provisional_source};

#[test]
fn trust_gate_dream_gate_treats_dream_promoter_as_provisional_ai() {
    assert!(is_ai_provisional_source("dream_promoter"));
    assert!(!allows_strengthening(Some("dream_promoter"), false, true));
    assert!(allows_strengthening(Some("dream_promoter"), true, true));
}
