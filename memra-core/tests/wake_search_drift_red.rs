//! RED tests for the Python ↔ Rust wake/search drift discovered 2026-05-04.
//!
//! Background:
//!   The Python backend got 49 RED tests fixed in build2-ma-search-recall-fix-q1q3-v2
//!   (commits c224aab/0d4393e/0627b25 on 2026-04-28), but the Rust backend never
//!   adopted the same fixes. Since MCP clients call `target/release/memra serve`
//!   (the Rust binary), the same recall bug recurred 5+ times under the
//!   appearance of "we already fixed this" — see alice's complaint trail
//!   (4-13 / 4-20 / 4-28 / 5-03 / 5-04).
//!
//! This file encodes the four bugs as failing tests, to be flipped GREEN once
//! the Rust algorithm reaches parity with Python.
//!
//! North star (alice 2026-04-20 [CONSTITUTION] cd5d6535):
//!   "让 AI 越用越懂 alice，不需要 alice 反复教".
//!   Every time alice re-explains the same thing = MA product failure.

use std::collections::HashSet;

use memra_core::retrieval::scoring::{
    ScoreInput, ScoringMetadata, ScoringPayload, has_temporal_markers, score_result_debug,
};
use memra_core::retrieval::search::SearchEngine;
use memra_core::storage::db::{DbPool, SearchParams};

fn make_engine() -> SearchEngine {
    let pool = DbPool::open_in_memory().expect("open in-memory pool");
    SearchEngine::new(pool)
}

fn insert_search_note(engine: &SearchEngine, id: &str, content: &str, project_id: &str) {
    insert_search_note_at(engine, id, content, project_id, 0, None, None);
}

fn insert_search_note_at(
    engine: &SearchEngine,
    id: &str,
    content: &str,
    project_id: &str,
    days_ago: i64,
    agent: Option<&str>,
    task_id: Option<&str>,
) {
    engine.pool().with_conn(|conn| {
        let now_iso = (chrono::Utc::now() - chrono::Duration::days(days_ago))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let metadata_json = serde_json::json!({
            "agent": agent,
            "task_id": task_id,
        })
        .to_string();
        conn.execute(
            "INSERT INTO notes (
                id, content, layer, category, created_at, updated_at,
                is_active, project_id, confidence, metadata_json, agent
             ) VALUES (?1, ?2, 'verified_fact', 'fact', ?3, ?3, 1, ?4, 0.9, ?5, ?6)",
            rusqlite::params![id, content, now_iso, project_id, metadata_json, agent],
        )
        .expect("insert search note");
        conn.execute(
            "INSERT INTO notes_fts (note_id, content) VALUES (?1, ?2)",
            rusqlite::params![id, content],
        )
        .expect("insert search note fts");
    });
}

fn assert_result_has_activation_path(
    result: &memra_core::storage::db::SearchResult,
    relation_type: &str,
    expected_path: &[&str],
) {
    let meta = result
        .metadata
        .as_ref()
        .expect("activated result should carry metadata");
    assert_eq!(meta["is_association"].as_bool(), Some(true));
    assert_eq!(meta["relation_type"].as_str(), Some(relation_type));
    assert_eq!(
        meta["activation_path"]
            .as_array()
            .expect("activation_path should be array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>(),
        expected_path
    );
}

fn insert_relation(
    engine: &SearchEngine,
    from_id: &str,
    to_id: &str,
    relation_type: &str,
    strength: f64,
) {
    engine.pool().with_conn(|conn| {
        conn.execute(
            "INSERT INTO note_relations (
                from_note_id, to_note_id, relation_type, strength
             ) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![from_id, to_id, relation_type, strength],
        )
        .expect("insert note relation");
    });
}

fn insert_promoted_dream_candidate(
    engine: &SearchEngine,
    id: &str,
    project_id: &str,
    promoted_to: &str,
    evidence_ids: &[&str],
    confidence: f64,
) {
    engine.pool().with_conn(|conn| {
        let now_iso = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let evidence_json = serde_json::to_string(evidence_ids).expect("serialize evidence ids");
        conn.execute(
            "INSERT INTO dream_candidates (
                id, project_id, source_type, summary, confidence, frequency,
                evidence_ids, verdict, promoted_to, created_at, evaluated_at, promoted_at
             ) VALUES (
                ?1, ?2, 'consolidation', 'dream candidate connects evidence to promoted memory',
                ?3, 2, ?4, 'promoted', ?5, ?6, ?6, ?6
             )",
            rusqlite::params![
                id,
                project_id,
                confidence,
                evidence_json,
                promoted_to,
                now_iso
            ],
        )
        .expect("insert promoted dream candidate");
    });
}

fn lexical_search(
    engine: &SearchEngine,
    query: &str,
    limit: usize,
) -> Vec<memra_core::storage::db::SearchResult> {
    engine.search(&SearchParams {
        query: query.to_string(),
        limit,
        only_active: true,
        project_id: Some("memra".to_string()),
        search_mode: Some("lexical".to_string()),
        ..SearchParams::default()
    })
}

fn empty_metadata() -> ScoringMetadata {
    ScoringMetadata::default()
}

fn payload(content: &str, layer: &str, days_ago: i64) -> ScoringPayload {
    let created_at = (chrono::Utc::now() - chrono::Duration::days(days_ago))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    ScoringPayload {
        content: content.to_string(),
        layer: layer.to_string(),
        confidence: Some(0.9),
        created_at: Some(created_at),
        evolution_state: None,
        category: None,
        source: None,
    }
}

// ---------------------------------------------------------------------------
// BUG 1: generic queries get freshness ≈ 0% — 25-day-old memos beat fresh work.
//
// Python build2 fix (apr-28): keep generic at 0.90/0.02/0.08 BUT expanded the
// temporal-markers whitelist so most alice queries route to temporal weights
// (0.45/0.40/0.15). However, alice's actual day-to-day language ("回忆一下" /
// "今天" / "Hermes 在执行" / "回一下上下文") still falls outside the whitelist
// AND the generic weights stay 90/2/8 — so the dictionary game is unwinnable.
//
// The real fix: rebalance generic to give freshness real weight.
// ---------------------------------------------------------------------------

#[test]
fn red_bug1_generic_query_freshness_must_be_meaningful() {
    let pl = payload("test memory content", "verified_fact", 0);
    let meta = empty_metadata();
    let boost: HashSet<String> = HashSet::new();
    let input = ScoreInput {
        query: "how does scoring work", // generic, no temporal markers
        query_vector: None,
        payload: &pl,
        stored_vector: None,
        bm25_score: None,
        metadata: &meta,
        boost_categories: Some(&boost),
        recall_count: 0,
        last_recalled_at: None,
    };
    let dbg = score_result_debug(&input);

    // Sanity: this query should NOT be classified as temporal.
    assert!(
        !dbg.has_temporal,
        "'how does scoring work' must not be temporal; got has_temporal=true"
    );

    // BUG: generic queries currently weigh freshness at 0.02 (2%), so a fresh
    // memory's recency advantage is essentially noise. Set the floor at 0.20
    // (20%) — anything below that means recent work continues to lose to
    // 25-day-old strategy memos in cosine-saturated queries.
    assert!(
        dbg.w_fresh >= 0.20,
        "generic query freshness weight must be >= 0.20; got w_fresh={} \
         (this is the 90/2/8 reverse-north-star bug — freshness can never \
         overcome cosine relevance)",
        dbg.w_fresh
    );
    // Trust can stay modest; ensure the rebalance doesn't gut relevance.
    assert!(
        dbg.w_rel >= 0.50 && dbg.w_rel <= 0.80,
        "generic relevance weight must stay 0.50..=0.80; got {}",
        dbg.w_rel
    );
}

// ---------------------------------------------------------------------------
// BUG 2: alice's actual spoken phrases are NOT in CURRENT_STATE_MARKERS_ZH.
//
// The Rust whitelist has 14 ZH phrases, none of which match how alice
// actually asks for status. Each query below was observed in real sessions
// (transcripts 4-13 / 4-20 / 5-03 / 5-04) and routed to generic 90/2/8.
// ---------------------------------------------------------------------------

#[test]
fn red_bug2_alice_real_phrasings_must_route_to_temporal() {
    let phrases = [
        "回忆一下这个项目的状态", // 5-04 mission-control session opener
        "回一下上下文",           // 5-04 MarketLab session opener
        "今天做了什么",           // generic daily check
        "昨晚的进度",             // 5-04 morning resume
        "这两天什么情况",         // weekend check-in
        "刚做完的事",             // immediate after action
        "Hermes 在执行",          // 5-04 mission-control specific
        "agent 一直在执行",       // generic agent-status check
        "最新的记忆",             // 5-04 alice explicit ask
        "最新的几条",             // 5-04 alice explicit ask
    ];

    let mut missing: Vec<&str> = Vec::new();
    for q in phrases {
        if !has_temporal_markers(q) {
            missing.push(q);
        }
    }
    assert!(
        missing.is_empty(),
        "the following alice-style queries must route to temporal channel \
         but currently fall through to generic 90/2/8 weights: {missing:#?}\n\n\
         Add the missing markers to CURRENT_STATE_MARKERS_ZH/EN in \
         memra-core/src/retrieval/scoring.rs."
    );
}

// ---------------------------------------------------------------------------
// BUG 3: wake snapshot returns recent_memories as a count, not a list.
//
// The AI that opens a session sees `recent_memories: 15` (a number) — the
// last 15 add_rule writes are completely invisible. North-star反方向: the AI
// has no idea what alice did this week.
// ---------------------------------------------------------------------------

#[test]
fn red_bug3_wake_snapshot_recent_memories_must_be_a_list() {
    let engine = make_engine();
    let snap = engine.get_context_wake();
    let value = &snap["recent_memories"];

    // Current bug: recent_memories is a number (count), not an array of recent
    // memory previews. AI sees "15" with no titles/content — completely blind
    // to what alice actually wrote last session.
    assert!(
        value.is_array(),
        "wake.recent_memories must be a list of recent memory previews \
         (with title/layer/created_at), not a count. Got: {value:?}\n\n\
         Fix: change get_context_wake_for_project in \
         memra-core/src/retrieval/search.rs:700 to embed the actual rows, not \
         just the count."
    );
}

// ---------------------------------------------------------------------------
// BUG 4: wake snapshot has no active_workstream field.
//
// The Python backend exposes `l1_active_workstream` — top-K most-active
// task_id/branch by event_log writes in the last 7d (Q3 of build2). Rust
// has no equivalent. AI starts every session blind to "what alice is
// actively working on right now" except via the active_checkpoints dump,
// which is contaminated by stale in_progress tasks (e.g. 5 demo-money
// checkpoints from 5-03 that nobody closed).
// ---------------------------------------------------------------------------

#[test]
fn red_bug4_wake_snapshot_must_include_active_workstream() {
    let engine = make_engine();
    let snap = engine.get_context_wake();
    let value = &snap["active_workstream"];

    assert!(
        value.is_array(),
        "wake.active_workstream must exist as a list of recent active \
         task_ids/branches by write count in last 7d. Got: {value:?}\n\n\
         Fix: add active_workstream to get_context_wake_for_project in \
         memra-core/src/retrieval/search.rs:700, mirroring Python \
         backend/services/context_snapshot.py:_list_active_workstreams (Q3)."
    );
}

// ---------------------------------------------------------------------------
// CODEX P1-1: list_active_workstreams must return rows on the project-scoped
// path (the *only* path real MCP traffic hits via service.rs:2450).
//
// Background: bug4 above only checked `is_array()`. Codex review surfaced
// that the Some(project_id) branch of list_active_workstreams interleaves
// `?1` with bare `?` placeholders. SQLite re-uses index 1 for both the bare
// `?` (n_at.project_id, position-bound by rusqlite) and the explicit `?1`
// (cutoff_seconds), so the project_id filter ends up comparing a string
// column to an integer — the prepared statement either errors out or
// silently returns zero rows. `unwrap_or_default()` then masks both, and
// production wake.active_workstream is forever `[]`.
//
// This test inserts 3 task_id-tagged rows scoped to project "alpha", and
// asserts the project-scoped wake snapshot surfaces that workstream with
// the right count.
// ---------------------------------------------------------------------------

#[test]
fn red_codex_p1_active_workstream_returns_data_when_project_scoped() {
    use uuid::Uuid;

    let engine = make_engine();

    // Reach into the engine's pool to insert tagged rows directly.
    engine.pool().with_conn(|conn| {
        let now_iso = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        for i in 0..3 {
            let id = Uuid::new_v4().to_string();
            let metadata = format!(r#"{{"task_id":"alpha-task","branch":"feat/alpha-{i}"}}"#);
            conn.execute(
                "INSERT INTO notes (
                    id, content, layer, category, metadata_json, created_at, updated_at,
                    is_active, project_id, confidence
                 ) VALUES (?1, ?2, 'event_log', 'event', ?3, ?4, ?4, 1, 'alpha', 0.9)",
                rusqlite::params![id, format!("alpha event {i}"), metadata, now_iso],
            )
            .expect("insert tagged row");
        }
        // One unrelated project row to ensure scoping is real.
        let other_id = Uuid::new_v4().to_string();
        let other_meta = r#"{"task_id":"beta-task"}"#;
        conn.execute(
            "INSERT INTO notes (
                id, content, layer, category, metadata_json, created_at, updated_at,
                is_active, project_id, confidence
             ) VALUES (?1, 'beta event', 'event_log', 'event', ?2, ?3, ?3, 1, 'beta', 0.9)",
            rusqlite::params![other_id, other_meta, now_iso],
        )
        .expect("insert beta row");
    });

    let snap = engine.get_context_wake_for_project(Some("alpha"));
    let workstream = snap["active_workstream"]
        .as_array()
        .expect("active_workstream must be an array");

    assert!(
        !workstream.is_empty(),
        "alpha has 3 task_id-tagged rows; project-scoped active_workstream \
         must surface them but came back empty: {snap}\n\n\
         Likely cause: list_active_workstreams Some(pid) branch mixes ?1 \
         and bare ? placeholders, prepared statement errors out, \
         unwrap_or_default() returns [].\n\n\
         Fix: collapse all placeholders to anonymous ? and bind in source \
         order [pid, pid, cutoff_seconds, pid, limit]."
    );

    let alpha = workstream
        .iter()
        .find(|w| w["key"].as_str() == Some("alpha-task"))
        .expect("alpha-task workstream missing");
    assert_eq!(
        alpha["count"].as_i64(),
        Some(3),
        "expected count=3 for alpha-task; got {alpha:?}"
    );

    // Cross-project leak guard: alpha-scoped wake must NOT surface beta-task.
    assert!(
        workstream
            .iter()
            .all(|w| w["key"].as_str() != Some("beta-task")),
        "alpha-scoped wake leaked beta-task: {workstream:?}"
    );
}

// ---------------------------------------------------------------------------
// CODEX P1-2: completed checkpoints must be visible somewhere in wake.
//
// search_checkpoints_inner(status="active") only emits blocked/in_progress
// rows into wake.checkpoints[]. list_recent_memories then filters out
// every record_type=checkpoint row regardless of task_status. So a
// `save_checkpoint(task_status="completed")` written 30 seconds ago is
// triple-invisible: not in checkpoints[] (filtered out as not active), not
// in recent_memories[] (filtered out as a checkpoint). This was one of the
// original 5-04 failure modes (MarketLab-v2-riley-response-2026-05-04 was a
// completed checkpoint that vanished from wake) and the existing fix did
// nothing for it.
//
// Decision encoded by this test: completed checkpoints surface in
// recent_memories[] (because they are "what just got finished"), while
// active checkpoints stay in checkpoints[] only (so they don't double up).
// ---------------------------------------------------------------------------

#[test]
fn red_codex_p1_completed_checkpoint_visible_in_wake() {
    use uuid::Uuid;

    let engine = make_engine();

    engine.pool().with_conn(|conn| {
        let now_iso = chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let completed_id = Uuid::new_v4().to_string();
        let completed_meta = r#"{"record_type":"checkpoint","task_id":"just-finished","task_status":"completed","next_step":"none"}"#;
        conn.execute(
            "INSERT INTO notes (
                id, content, layer, category, metadata_json, created_at, updated_at,
                is_active, project_id, confidence
             ) VALUES (?1, ?2, 'event_log', 'event', ?3, ?4, ?4, 1, 'memra', 0.9)",
            rusqlite::params![
                completed_id,
                "[断点] just-finished: closed out the work cleanly",
                completed_meta,
                now_iso
            ],
        )
        .expect("insert completed checkpoint");

        let active_id = Uuid::new_v4().to_string();
        let active_meta = r#"{"record_type":"checkpoint","task_id":"still-going","task_status":"in_progress","next_step":"keep working"}"#;
        conn.execute(
            "INSERT INTO notes (
                id, content, layer, category, metadata_json, created_at, updated_at,
                is_active, project_id, confidence
             ) VALUES (?1, ?2, 'event_log', 'event', ?3, ?4, ?4, 1, 'memra', 0.9)",
            rusqlite::params![
                active_id,
                "[断点] still-going: more to do",
                active_meta,
                now_iso
            ],
        )
        .expect("insert active checkpoint");
    });

    let snap = engine.get_context_wake_for_project(Some("memra"));

    // Active checkpoint must appear in checkpoints[].
    let checkpoints = snap["checkpoints"]
        .as_array()
        .expect("checkpoints must be array");
    assert!(
        checkpoints
            .iter()
            .any(|c| c["task_id"].as_str() == Some("still-going")),
        "active checkpoint should still appear in checkpoints[]; got {checkpoints:?}"
    );
    // Active checkpoint should NOT also be in recent_memories (no double-up).
    let recent = snap["recent_memories"]
        .as_array()
        .expect("recent_memories must be array");
    assert!(
        recent
            .iter()
            .all(|r| r["task_id"].as_str() != Some("still-going")),
        "active checkpoint must NOT also appear in recent_memories \
         (would double up with checkpoints[]). Got: {recent:?}"
    );
    // Completed checkpoint must surface SOMEWHERE — recent_memories is the
    // chosen location so the AI sees "just-finished" at session start.
    assert!(
        recent
            .iter()
            .any(|r| r["task_id"].as_str() == Some("just-finished")),
        "completed checkpoint must surface in recent_memories so AI sees \
         freshly-closed work at session start; got {recent:?}\n\n\
         Fix: list_recent_memories filter must keep checkpoint rows whose \
         task_status == 'completed' (active rows already appear in \
        checkpoints[])."
    );
}

// ---------------------------------------------------------------------------
// CODEX GOAL-1: wake must expose relation signals for recent memories.
//
// alice's product requirement is not merely "search newest rows". Memory
// Anchor promises a neural memory graph: new notes should connect to older
// notes through note_relations, and the session-start wake payload must make
// that graph visible enough for the agent to continue the chain.
// ---------------------------------------------------------------------------

#[test]
fn red_codex_goal_recent_memories_surface_relation_signals() {
    let engine = make_engine();

    engine.pool().with_conn(|conn| {
        let now_iso = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        conn.execute(
            "INSERT INTO notes (
                id, content, layer, category, created_at, updated_at,
                is_active, project_id, confidence
             ) VALUES ('fresh-neuron', 'fresh neural memory', 'event_log', 'event',
                       ?1, ?1, 1, 'memra', 0.9)",
            rusqlite::params![now_iso],
        )
        .expect("insert fresh note");
        conn.execute(
            "INSERT INTO notes (
                id, content, layer, category, created_at, updated_at,
                is_active, project_id, confidence
             ) VALUES ('old-neuron', 'older linked memory', 'verified_fact', 'fact',
                       ?1, ?1, 1, 'memra', 0.9)",
            rusqlite::params![now_iso],
        )
        .expect("insert old note");
        conn.execute(
            "INSERT INTO note_relations (
                from_note_id, to_note_id, relation_type, strength, created_at
             ) VALUES ('fresh-neuron', 'old-neuron', 'supports', 0.88, ?1)",
            rusqlite::params![now_iso],
        )
        .expect("insert relation");
    });

    let snap = engine.get_context_wake_for_project(Some("memra"));
    let recent = snap["recent_memories"]
        .as_array()
        .expect("recent_memories must be array");
    let fresh = recent
        .iter()
        .find(|r| r["id"].as_str() == Some("fresh-neuron"))
        .expect("fresh-neuron missing from wake recent_memories");

    assert_eq!(
        fresh["relation_count"].as_i64(),
        Some(1),
        "wake must expose relation_count for recent memories; got {fresh:?}"
    );
    assert!(
        fresh["relation_types"]
            .as_array()
            .expect("relation_types must be array")
            .iter()
            .any(|t| t.as_str() == Some("supports")),
        "wake must expose relation_types so agents can see neural edges; got {fresh:?}"
    );
}

// ---------------------------------------------------------------------------
// CODEX GOAL-2: wake must expose recently promoted dreaming candidates.
//
// If old/new memories are consolidated in dreaming but session wake never
// returns the promoted candidates, the loop is not closed for the agent.
// ---------------------------------------------------------------------------

#[test]
fn red_codex_goal_wake_surfaces_recent_dreaming_promotions() {
    let engine = make_engine();

    engine.pool().with_conn(|conn| {
        let now_iso = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        conn.execute(
            "INSERT INTO dream_candidates (
                id, project_id, source_type, source_id, summary, hypothesis,
                confidence, frequency, evidence_ids, verdict, promoted_to,
                evaluator_notes, created_at, evaluated_at, promoted_at, discarded_at
             ) VALUES (
                'dream-alpha', 'memra', 'dream', 'source-alpha',
                'old and new recall failures consolidate into one Rust parity rule',
                NULL, 0.91, 3, '[\"fresh-neuron\",\"old-neuron\"]',
                'promoted', 'fresh-neuron', NULL, ?1, ?1, ?1, NULL
             )",
            rusqlite::params![now_iso],
        )
        .expect("insert promoted dream candidate");
        conn.execute(
            "INSERT INTO dream_candidates (
                id, project_id, source_type, summary, confidence, verdict,
                promoted_to, created_at, promoted_at
             ) VALUES (
                'dream-beta', 'other-project', 'dream',
                'other project promotion must not leak', 0.8, 'promoted',
                'other-note', ?1, ?1
             )",
            rusqlite::params![now_iso],
        )
        .expect("insert other project dream candidate");
    });

    let snap = engine.get_context_wake_for_project(Some("memra"));
    let promoted = snap["recently_promoted_from_dreaming"]
        .as_array()
        .expect("recently_promoted_from_dreaming must be array");

    let alpha = promoted
        .iter()
        .find(|d| d["id"].as_str() == Some("dream-alpha"))
        .expect("dream-alpha promotion missing from wake");
    assert_eq!(
        alpha["promoted_to"].as_str(),
        Some("fresh-neuron"),
        "wake must surface the promoted note id; got {alpha:?}"
    );
    assert!(
        promoted
            .iter()
            .all(|d| d["id"].as_str() != Some("dream-beta")),
        "project-scoped wake leaked another project's dream promotion: {promoted:?}"
    );
}

// ---------------------------------------------------------------------------
// CODEX GOAL-3: Rust search itself must walk the neural graph.
//
// Wake visibility is necessary but not sufficient. Once a direct search result
// is found, Rust retrieval must do the same one-hop spreading activation that
// Python has: strong semantic relation edges activate neighbors that do not
// lexically match the query.
// ---------------------------------------------------------------------------

#[test]
fn red_codex_goal_search_spreads_activation_over_strong_relation_edges() {
    let engine = make_engine();
    insert_search_note(
        &engine,
        "direct-sqlite",
        "SQLite is a lightweight relational database engine",
        "memra",
    );
    insert_search_note(
        &engine,
        "neighbor-graph",
        "This memory is only reachable through the neural relation graph",
        "memra",
    );
    insert_relation(&engine, "direct-sqlite", "neighbor-graph", "supports", 0.8);

    let results = lexical_search(&engine, "SQLite database", 10);
    let direct = results
        .iter()
        .find(|r| r.id == "direct-sqlite")
        .expect("direct lexical result missing");
    let neighbor = results
        .iter()
        .find(|r| r.id == "neighbor-graph")
        .expect("strong relation neighbor must be activated into Rust search results");

    let meta = neighbor
        .metadata
        .as_ref()
        .expect("activated neighbor should carry metadata");
    assert_eq!(meta["is_association"].as_bool(), Some(true));
    assert_eq!(meta["associated_from"].as_str(), Some("direct-sqlite"));
    assert_eq!(meta["relation_type"].as_str(), Some("supports"));
    assert!(
        neighbor.score < direct.score,
        "activated neighbor should be a derivative score below source score; \
         direct={direct:?} neighbor={neighbor:?}"
    );
}

#[test]
fn red_codex_goal_search_spreads_activation_across_neural_chain() {
    let engine = make_engine();
    insert_search_note(
        &engine,
        "chain-direct",
        "SQLite database neural chain seed",
        "memra",
    );
    insert_search_note(
        &engine,
        "chain-hop-one",
        "Intermediate memory only reachable through first neural edge",
        "memra",
    );
    insert_search_note(
        &engine,
        "chain-hop-two",
        "Second hop memory only reachable through chained activation",
        "memra",
    );
    insert_relation(&engine, "chain-direct", "chain-hop-one", "supports", 0.9);
    insert_relation(&engine, "chain-hop-one", "chain-hop-two", "refines", 0.8);

    let results = lexical_search(&engine, "SQLite database", 10);
    let direct = results
        .iter()
        .find(|r| r.id == "chain-direct")
        .expect("direct lexical seed missing");
    let hop_one = results
        .iter()
        .find(|r| r.id == "chain-hop-one")
        .expect("first hop memory should activate");
    let hop_two = results
        .iter()
        .find(|r| r.id == "chain-hop-two")
        .expect("second hop memory should activate through neural chain");

    assert!(
        hop_one.score < direct.score && hop_two.score < hop_one.score,
        "activation should decay with each hop; direct={direct:?} hop_one={hop_one:?} hop_two={hop_two:?}"
    );
    let meta = hop_two
        .metadata
        .as_ref()
        .expect("second hop activation should carry metadata");
    assert_eq!(meta["is_association"].as_bool(), Some(true));
    assert_eq!(meta["associated_from"].as_str(), Some("chain-hop-one"));
    assert_eq!(meta["association_root"].as_str(), Some("chain-direct"));
    assert_eq!(meta["activation_depth"].as_u64(), Some(2));
    assert_eq!(
        meta["activation_path"]
            .as_array()
            .expect("activation_path should be array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>(),
        vec!["chain-direct", "chain-hop-one", "chain-hop-two"]
    );
}

#[test]
fn red_codex_goal_search_respects_activation_strength_threshold() {
    let engine = make_engine();
    insert_search_note(
        &engine,
        "direct-threshold",
        "SQLite database threshold seed",
        "memra",
    );
    insert_search_note(
        &engine,
        "weak-neighbor",
        "Weakly related memory should not activate",
        "memra",
    );
    insert_relation(
        &engine,
        "direct-threshold",
        "weak-neighbor",
        "supports",
        0.2,
    );

    let results = lexical_search(&engine, "SQLite database", 10);
    assert!(
        results.iter().any(|r| r.id == "direct-threshold"),
        "direct seed missing: {results:?}"
    );
    assert!(
        results.iter().all(|r| r.id != "weak-neighbor"),
        "weak relation below threshold must not activate: {results:?}"
    );
}

#[test]
fn red_r1_activation_strength_threshold_boundary_is_strict() {
    let engine = make_engine();
    insert_search_note(
        &engine,
        "direct-boundary",
        "SQLite database boundary threshold seed",
        "memra",
    );
    insert_search_note(
        &engine,
        "at-threshold-neighbor",
        "Exactly threshold relation must not activate",
        "memra",
    );
    insert_search_note(
        &engine,
        "above-threshold-neighbor",
        "Barely above threshold relation should activate",
        "memra",
    );
    insert_relation(
        &engine,
        "direct-boundary",
        "at-threshold-neighbor",
        "supports",
        0.4,
    );
    insert_relation(
        &engine,
        "direct-boundary",
        "above-threshold-neighbor",
        "supports",
        0.4001,
    );

    let results = lexical_search(&engine, "SQLite database", 10);
    assert!(
        results.iter().any(|r| r.id == "direct-boundary"),
        "direct seed missing: {results:?}"
    );
    assert!(
        results.iter().all(|r| r.id != "at-threshold-neighbor"),
        "relation strength exactly at the 0.4 threshold must not activate; results={results:?}"
    );
    let above = results
        .iter()
        .find(|r| r.id == "above-threshold-neighbor")
        .expect("relation strength above threshold should activate");
    assert_eq!(
        above
            .metadata
            .as_ref()
            .and_then(|meta| meta["relation_strength"].as_f64()),
        Some(0.4001)
    );
}

#[test]
fn red_codex_goal_search_ignores_provenance_relation_types() {
    let engine = make_engine();
    insert_search_note(
        &engine,
        "direct-provenance",
        "SQLite database provenance seed",
        "memra",
    );
    insert_search_note(
        &engine,
        "provenance-neighbor",
        "Procedure provenance should not appear in ordinary search",
        "memra",
    );
    insert_relation(
        &engine,
        "direct-provenance",
        "provenance-neighbor",
        "generalized_from",
        0.9,
    );

    let results = lexical_search(&engine, "SQLite database", 10);
    assert!(
        results.iter().any(|r| r.id == "direct-provenance"),
        "direct seed missing: {results:?}"
    );
    assert!(
        results.iter().all(|r| r.id != "provenance-neighbor"),
        "provenance relation type must not activate generic search: {results:?}"
    );
}

#[test]
fn red_r1_wake_recent_memory_items_keep_schema_contract() {
    let engine = make_engine();
    insert_search_note(
        &engine,
        "schema-recent",
        "wake schema recent memory preview contract",
        "memra",
    );

    let wake = engine.get_context_wake_for_project(Some("memra"));
    let recent = wake["recent_memories"]
        .as_array()
        .expect("wake.recent_memories must be an array");
    let item = recent
        .iter()
        .find(|item| item["id"].as_str() == Some("schema-recent"))
        .expect("schema-recent must appear in wake recent_memories");

    for key in [
        "id",
        "layer",
        "category",
        "created_at",
        "preview",
        "task_id",
        "task_status",
        "is_checkpoint",
        "relation_count",
        "relation_types",
    ] {
        assert!(
            item.get(key).is_some(),
            "wake recent memory item missing key {key}: {item:?}"
        );
    }
    assert_eq!(item["layer"].as_str(), Some("verified_fact"));
    assert_eq!(
        item["preview"].as_str(),
        Some("wake schema recent memory preview contract")
    );
    assert_eq!(item["relation_count"].as_i64(), Some(0));
    assert!(
        item["relation_types"]
            .as_array()
            .expect("relation_types must be array")
            .is_empty()
    );
}

#[test]
fn red_r1_governance_identity_schema_is_hidden_unless_explicitly_requested() {
    let engine = make_engine();
    insert_search_note(
        &engine,
        "governance-direct",
        "SQLite database governance seed",
        "memra",
    );
    insert_search_note(
        &engine,
        "governance-l0",
        "alice immutable identity rule reachable only through L0 edge",
        "memra",
    );
    engine.pool().with_conn(|conn| {
        conn.execute(
            "UPDATE notes SET layer = 'identity_schema', category = 'person' WHERE id = 'governance-l0'",
            [],
        )
        .expect("promote fixture to identity_schema");
    });
    insert_relation(
        &engine,
        "governance-direct",
        "governance-l0",
        "supports",
        0.95,
    );

    let default_results = lexical_search(&engine, "SQLite database governance", 10);
    assert!(
        default_results.iter().any(|r| r.id == "governance-direct"),
        "direct seed missing: {default_results:?}"
    );
    assert!(
        default_results.iter().all(|r| r.id != "governance-l0"),
        "identity_schema must stay hidden from ordinary recall unless include_constitution=true: {default_results:?}"
    );

    let explicit_results = engine.search(&SearchParams {
        query: "SQLite database governance".to_string(),
        limit: 10,
        only_active: true,
        project_id: Some("memra".to_string()),
        search_mode: Some("lexical".to_string()),
        include_constitution: true,
        ..SearchParams::default()
    });
    let l0 = explicit_results
        .iter()
        .find(|r| r.id == "governance-l0")
        .expect("include_constitution=true should allow L0 activation");
    assert_eq!(l0.layer, "identity_schema");
    assert_eq!(
        l0.metadata
            .as_ref()
            .and_then(|meta| meta["relation_type"].as_str()),
        Some("supports")
    );
}

#[test]
fn red_codex_goal_search_activation_stays_project_scoped() {
    let engine = make_engine();
    insert_search_note(
        &engine,
        "direct-project",
        "SQLite database project-scoped seed",
        "memra",
    );
    insert_search_note(
        &engine,
        "other-project-neighbor",
        "Other project neighbor must not cross the relation graph",
        "other-project",
    );
    insert_relation(
        &engine,
        "direct-project",
        "other-project-neighbor",
        "supports",
        0.95,
    );

    let results = lexical_search(&engine, "SQLite database", 10);
    assert!(
        results.iter().any(|r| r.id == "direct-project"),
        "direct seed missing: {results:?}"
    );
    assert!(
        results.iter().all(|r| r.id != "other-project-neighbor"),
        "project-scoped search must not activate neighbors from another project: {results:?}"
    );
}

#[test]
fn red_codex_goal_search_walks_dreaming_evidence_edges_from_promotion() {
    let engine = make_engine();
    insert_search_note(
        &engine,
        "dream-promoted",
        "Rust retrieval closure rule from dreaming",
        "memra",
    );
    insert_search_note(
        &engine,
        "dream-evidence",
        "Original old recall failure evidence only linked through dreaming",
        "memra",
    );
    insert_promoted_dream_candidate(
        &engine,
        "dream-candidate",
        "memra",
        "dream-promoted",
        &["dream-evidence"],
        0.92,
    );

    let results = lexical_search(&engine, "Rust retrieval closure", 10);
    let promoted = results
        .iter()
        .find(|r| r.id == "dream-promoted")
        .expect("direct promoted memory missing");
    let evidence = results
        .iter()
        .find(|r| r.id == "dream-evidence")
        .expect("dream evidence memory must activate from promoted memory");

    let meta = evidence
        .metadata
        .as_ref()
        .expect("dream evidence activation should carry metadata");
    assert_eq!(meta["is_association"].as_bool(), Some(true));
    assert_eq!(meta["associated_from"].as_str(), Some("dream-promoted"));
    assert_eq!(meta["relation_type"].as_str(), Some("dreaming_evidence"));
    assert_eq!(meta["dream_candidate_id"].as_str(), Some("dream-candidate"));
    assert!(
        evidence.score < promoted.score,
        "dream evidence should be a derivative score below promoted source; \
         promoted={promoted:?} evidence={evidence:?}"
    );
}

#[test]
fn red_codex_goal_search_walks_dreaming_edges_back_to_promotion() {
    let engine = make_engine();
    insert_search_note(
        &engine,
        "dream-promoted-back",
        "Promoted dream synthesis only reachable through evidence",
        "memra",
    );
    insert_search_note(
        &engine,
        "dream-evidence-back",
        "Hermes recall failure evidence seed",
        "memra",
    );
    insert_promoted_dream_candidate(
        &engine,
        "dream-candidate-back",
        "memra",
        "dream-promoted-back",
        &["dream-evidence-back"],
        0.91,
    );

    let results = lexical_search(&engine, "Hermes recall failure", 10);
    assert!(
        results.iter().any(|r| r.id == "dream-evidence-back"),
        "direct evidence seed missing: {results:?}"
    );
    let promoted = results
        .iter()
        .find(|r| r.id == "dream-promoted-back")
        .expect("promoted dream memory must activate back from evidence memory");

    let meta = promoted
        .metadata
        .as_ref()
        .expect("dream promotion activation should carry metadata");
    assert_eq!(meta["is_association"].as_bool(), Some(true));
    assert_eq!(
        meta["associated_from"].as_str(),
        Some("dream-evidence-back")
    );
    assert_eq!(meta["relation_type"].as_str(), Some("dreaming_evidence"));
    assert_eq!(
        meta["dream_candidate_id"].as_str(),
        Some("dream-candidate-back")
    );
}

#[test]
fn red_codex_goal_search_dreaming_edges_stay_project_scoped() {
    let engine = make_engine();
    insert_search_note(
        &engine,
        "dream-project-seed",
        "Rust retrieval project scoped dream seed",
        "memra",
    );
    insert_search_note(
        &engine,
        "dream-other-evidence",
        "Other project evidence must not activate",
        "other-project",
    );
    insert_promoted_dream_candidate(
        &engine,
        "dream-other-candidate",
        "other-project",
        "dream-project-seed",
        &["dream-other-evidence"],
        0.99,
    );

    let results = lexical_search(&engine, "Rust retrieval project scoped", 10);
    assert!(
        results.iter().any(|r| r.id == "dream-project-seed"),
        "direct project seed missing: {results:?}"
    );
    assert!(
        results.iter().all(|r| r.id != "dream-other-evidence"),
        "dream evidence from another project must not activate: {results:?}"
    );
}

#[test]
fn red_codex_goal_memory_continuity_links_day1_day7_and_dream_chain() {
    let engine = make_engine();
    insert_search_note_at(
        &engine,
        "continuity-day1-source",
        "continuity audit day one original product contract from Claude: agents must recover project memory by reading the book",
        "memra",
        6,
        Some("claude"),
        Some("continuity-day1"),
    );
    insert_search_note_at(
        &engine,
        "continuity-day3-bridge",
        "continuity audit day three Codex bridge implementation links the original product contract to Rust recall",
        "memra",
        4,
        Some("codex"),
        Some("continuity-day3"),
    );
    insert_search_note_at(
        &engine,
        "continuity-day5-dream",
        "continuity audit day five dream synthesis compresses the source contract and bridge implementation into one promoted memory",
        "memra",
        2,
        Some("dream"),
        Some("continuity-day5"),
    );
    insert_search_note_at(
        &engine,
        "continuity-day7-latest",
        "continuity audit day seven latest project state: new agents must first see this current status then follow links backward",
        "memra",
        0,
        Some("claude"),
        Some("continuity-day7"),
    );
    insert_search_note_at(
        &engine,
        "continuity-other-project",
        "continuity audit day seven latest project state must not leak from other project",
        "other-project",
        0,
        Some("codex"),
        Some("continuity-other"),
    );
    insert_relation(
        &engine,
        "continuity-day3-bridge",
        "continuity-day1-source",
        "supports",
        0.95,
    );
    insert_relation(
        &engine,
        "continuity-day7-latest",
        "continuity-day5-dream",
        "refines",
        0.92,
    );
    insert_promoted_dream_candidate(
        &engine,
        "continuity-dream-candidate",
        "memra",
        "continuity-day5-dream",
        &["continuity-day1-source", "continuity-day3-bridge"],
        0.94,
    );

    let wake = engine.get_context_wake_for_project(Some("memra"));
    let recent = wake["recent_memories"]
        .as_array()
        .expect("wake.recent_memories must be an array");
    assert!(
        recent
            .iter()
            .any(|item| item["id"].as_str() == Some("continuity-day7-latest")),
        "wake must surface the Day 7 latest state for a fresh agent: {wake:?}"
    );
    assert!(
        recent
            .iter()
            .all(|item| item["id"].as_str() != Some("continuity-other-project")),
        "wake must stay project-scoped: {wake:?}"
    );
    let promoted = wake["recently_promoted_from_dreaming"]
        .as_array()
        .expect("wake.recently_promoted_from_dreaming must be an array");
    assert!(
        promoted
            .iter()
            .any(|item| item["promoted_to"].as_str() == Some("continuity-day5-dream")),
        "wake must expose the Day 5 promoted dream chain: {wake:?}"
    );

    let latest_results = lexical_search(
        &engine,
        "continuity audit day seven latest project state",
        10,
    );
    assert!(
        latest_results
            .iter()
            .any(|result| result.id == "continuity-day7-latest"),
        "fresh latest-state query must find Day 7 status: {latest_results:?}"
    );
    assert!(
        latest_results
            .iter()
            .all(|result| result.id != "continuity-other-project"),
        "latest-state query must not leak another project: {latest_results:?}"
    );
    let day5_from_latest = latest_results
        .iter()
        .find(|result| result.id == "continuity-day5-dream")
        .expect("Day 7 latest state should activate Day 5 dream summary");
    assert_result_has_activation_path(
        day5_from_latest,
        "refines",
        &["continuity-day7-latest", "continuity-day5-dream"],
    );

    let bridge_results = lexical_search(
        &engine,
        "continuity audit day three bridge implementation",
        10,
    );
    assert!(
        bridge_results
            .iter()
            .any(|result| result.id == "continuity-day3-bridge"),
        "Day 3 bridge should be a direct recall result: {bridge_results:?}"
    );
    let day1_from_bridge = bridge_results
        .iter()
        .find(|result| result.id == "continuity-day1-source")
        .expect("Day 3 bridge should activate Day 1 original source");
    assert_result_has_activation_path(
        day1_from_bridge,
        "supports",
        &["continuity-day3-bridge", "continuity-day1-source"],
    );

    let dream_results = lexical_search(&engine, "continuity audit day five dream synthesis", 10);
    assert!(
        dream_results
            .iter()
            .any(|result| result.id == "continuity-day5-dream"),
        "Day 5 promoted dream should be a direct recall result: {dream_results:?}"
    );
    for evidence_id in ["continuity-day1-source", "continuity-day3-bridge"] {
        let evidence = dream_results
            .iter()
            .find(|result| result.id == evidence_id)
            .unwrap_or_else(|| panic!("dream query should activate evidence {evidence_id}"));
        assert_eq!(
            evidence
                .metadata
                .as_ref()
                .and_then(|meta| meta["relation_type"].as_str()),
            Some("dreaming_evidence"),
            "dream evidence result should explain its dreaming edge: {evidence:?}"
        );
        assert_eq!(
            evidence
                .metadata
                .as_ref()
                .and_then(|meta| meta["dream_candidate_id"].as_str()),
            Some("continuity-dream-candidate")
        );
    }
}
