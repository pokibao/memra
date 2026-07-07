use memra_core::retrieval::search::SearchEngine;
use memra_core::storage::db::{DbPool, SearchParams};

fn make_engine() -> SearchEngine {
    SearchEngine::new(DbPool::open_in_memory().expect("in-memory db"))
}

fn insert_note(engine: &SearchEngine, id: &str, content: &str, source: &str) {
    engine.pool().with_conn(|conn| {
        conn.execute(
            "INSERT INTO notes
             (id, content, layer, category, is_active, confidence, project_id,
              created_at, updated_at, source, is_head, evolution_state)
             VALUES (?1, ?2, 'verified_fact', 'trust-gate', 1, 0.95, 'alpha',
                     '2026-05-19T00:00:00Z', '2026-05-19T00:00:00Z', ?3, 1, 'active')",
            rusqlite::params![id, content, source],
        )
        .expect("insert note");
        conn.execute(
            "INSERT INTO notes_fts (note_id, content) VALUES (?1, ?2)",
            rusqlite::params![id, content],
        )
        .expect("insert fts");
    });
}

fn insert_relation(engine: &SearchEngine, from: &str, to: &str) {
    engine.pool().with_conn(|conn| {
        conn.execute(
            "INSERT INTO note_relations (from_note_id, to_note_id, relation_type, strength)
             VALUES (?1, ?2, 'supports', 0.95)",
            rusqlite::params![from, to],
        )
        .expect("insert relation");
    });
}

fn search_ids(engine: &SearchEngine, query: &str) -> Vec<String> {
    let (results, _diagnostics) = engine.search_with_diagnostics(&SearchParams {
        query: query.to_string(),
        limit: 5,
        only_active: true,
        project_id: Some("alpha".to_string()),
        search_mode: Some("lexical".to_string()),
        min_score: 0.0,
        ..Default::default()
    });
    results.into_iter().map(|result| result.id).collect()
}

#[test]
fn trust_gate_unconfirmed_ai_candidate_is_not_activation_endpoint() {
    let engine = make_engine();
    insert_note(
        &engine,
        "trusted-source",
        "trusted anchor source memory",
        "user",
    );
    insert_note(
        &engine,
        "ai-target",
        "contagious hallucination should stay isolated",
        "ai",
    );
    insert_relation(&engine, "trusted-source", "ai-target");

    let ids = search_ids(&engine, "trusted anchor");

    assert!(ids.iter().any(|id| id == "trusted-source"));
    assert!(
        !ids.iter().any(|id| id == "ai-target"),
        "unconfirmed AI target must not be returned through activation"
    );
}

#[test]
fn trust_gate_unconfirmed_ai_candidate_is_not_activation_source() {
    let engine = make_engine();
    insert_note(&engine, "ai-source", "candidate anchor source memory", "ai");
    insert_note(
        &engine,
        "trusted-target",
        "trusted neighbor reachable only through activation",
        "user",
    );
    insert_relation(&engine, "ai-source", "trusted-target");

    let ids = search_ids(&engine, "candidate anchor");

    assert!(ids.iter().any(|id| id == "ai-source"));
    assert!(
        !ids.iter().any(|id| id == "trusted-target"),
        "unconfirmed AI source must not spread activation"
    );
}
