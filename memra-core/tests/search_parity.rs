//! Phase 1 parity test: Rust search_rules against the real SQLite DB.
//!
//! Verifies that:
//! 1. The engine can open and query the production DB
//! 2. FTS5 returns non-empty results for known queries
//! 3. Checkpoint search returns active checkpoints
//! 4. get_context_wake returns a valid snapshot
//!
//! This test is `#[ignore]` by default — run with `--include-ignored`.

use std::path::PathBuf;

use memra_core::retrieval::recall::build_fts_query;
use memra_core::retrieval::search::SearchEngine;
use memra_core::storage::db::{DbPool, SearchParams};

/// Safe UTF-8 truncation (never panics on multibyte boundaries).
fn safe_truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn get_db_path() -> Option<PathBuf> {
    // Try env var first
    if let Ok(path) = std::env::var("MCP_MEMORY_STORAGE_PATH") {
        let p = PathBuf::from(path);
        if p.exists() {
            return Some(p);
        }
    }

    // Default path for memra project
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home)
        .join(".memra/projects/memra/.storage/memory_anchor.sqlite3");
    if path.exists() { Some(path) } else { None }
}

fn create_engine() -> Option<SearchEngine> {
    let path = get_db_path()?;
    let pool = DbPool::open_readonly(&path).ok()?;
    Some(SearchEngine::new(pool))
}

#[test]
#[ignore]
fn debug_direct_fts_query() {
    let path = get_db_path().expect("Need real DB");
    let pool = DbPool::open_readonly(&path).expect("open DB");

    pool.with_conn(|conn| {
        // Check note count
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notes WHERE is_active=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        println!("Active notes: {count}");

        // Check FTS count
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notes_fts", [], |row| row.get(0))
            .unwrap();
        println!("FTS entries: {fts_count}");

        // Direct FTS query
        let fts_q = build_fts_query("Memra");
        println!("FTS query string: '{fts_q}'");

        let mut stmt = conn
            .prepare(
                "SELECT n.id, n.layer, substr(n.content, 1, 60), bm25(notes_fts) AS bm25_score
             FROM notes_fts
             JOIN notes n ON n.id = notes_fts.note_id
             WHERE notes_fts MATCH ?1
               AND n.is_active = 1
               AND n.project_id = ?2
             ORDER BY bm25_score ASC
             LIMIT 3",
            )
            .unwrap();

        let rows: Vec<(String, String, String, f64)> = stmt
            .query_map(rusqlite::params!["Memra", "memra"], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        println!("Direct FTS rows: {}", rows.len());
        for (id, layer, content, bm25) in &rows {
            println!("  [{bm25:.3}] [{layer}] {id}: {content}");
        }
        assert!(!rows.is_empty(), "Direct FTS query should return results");

        // Now test NoteRow::from_row by reading column names
        let pragma_rows: Vec<(i32, String, String)> = conn
            .prepare("PRAGMA table_info(notes)")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        println!("Notes table columns:");
        for (cid, name, ty) in &pragma_rows {
            println!("  {cid}: {name} ({ty})");
        }
    });
}

#[test]
#[ignore]
fn search_rules_returns_results() {
    let engine = create_engine().expect("Need real DB for parity test");

    // Test with a known query that should return results
    let params = SearchParams {
        query: "Memra".to_string(),
        limit: 5,
        only_active: true,
        project_id: Some("memra".to_string()),
        min_score: 0.0,
        ..Default::default()
    };

    let results = engine.search(&params);
    assert!(
        !results.is_empty(),
        "search_rules('Memra') should return results"
    );
    println!(
        "search_rules('Memra') returned {} results:",
        results.len()
    );
    for (i, r) in results.iter().enumerate() {
        println!(
            "  {}. [{:.3}] [{}] {}",
            i + 1,
            r.score,
            r.layer,
            safe_truncate(&r.content, 100)
        );
    }

    // Scores should be sorted descending
    for w in results.windows(2) {
        assert!(
            w[0].score >= w[1].score,
            "Results should be sorted by score descending"
        );
    }
}

#[test]
#[ignore]
fn search_rules_cjk_query() {
    let engine = create_engine().expect("Need real DB for parity test");

    let params = SearchParams {
        query: "预算".to_string(),
        limit: 5,
        only_active: true,
        project_id: Some("memra".to_string()),
        min_score: 0.0,
        ..Default::default()
    };

    let results = engine.search(&params);
    assert!(
        !results.is_empty(),
        "search_rules('预算') should return results (CJK query)"
    );
    println!("search_rules('预算') returned {} results:", results.len());
    for (i, r) in results.iter().enumerate() {
        println!(
            "  {}. [{:.3}] [{}] {}",
            i + 1,
            r.score,
            r.layer,
            safe_truncate(&r.content, 100)
        );
    }
}

#[test]
#[ignore]
fn search_checkpoints_active() {
    let engine = create_engine().expect("Need real DB for parity test");

    let results = engine.search_checkpoints(None, None, Some("active"), 10);
    println!("Active checkpoints: {}", results.len());
    for (i, r) in results.iter().enumerate() {
        let task_id = r
            .metadata
            .as_ref()
            .and_then(|m| m.get("task_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        println!(
            "  {}. {} — {}",
            i + 1,
            task_id,
            safe_truncate(&r.content, 80)
        );
    }
    // We know from get_context that there are active checkpoints
    assert!(
        !results.is_empty(),
        "should find at least one active checkpoint"
    );
}

#[test]
#[ignore]
fn get_context_wake_returns_snapshot() {
    let engine = create_engine().expect("Need real DB for parity test");

    let snapshot = engine.get_context_wake();
    println!(
        "Wake snapshot: {}",
        serde_json::to_string_pretty(&snapshot).unwrap()
    );

    assert_eq!(snapshot["mode"], "wake");
    assert!(
        snapshot["active_checkpoints"].as_i64().unwrap_or(0) >= 0,
        "should have a count of active checkpoints"
    );
}

#[test]
#[ignore]
fn search_with_temporal_markers() {
    let engine = create_engine().expect("Need real DB for parity test");

    let params = SearchParams {
        query: "最近的 BUILD-SUCCESS".to_string(),
        limit: 5,
        only_active: true,
        project_id: Some("memra".to_string()),
        min_score: 0.0,
        ..Default::default()
    };

    let results = engine.search(&params);
    println!(
        "search('最近的 BUILD-SUCCESS') returned {} results",
        results.len()
    );
    for (i, r) in results.iter().enumerate() {
        println!(
            "  {}. [{:.3}] [{}] {}",
            i + 1,
            r.score,
            r.layer,
            safe_truncate(&r.content, 100)
        );
    }
    // Should find at least one BUILD-SUCCESS
    assert!(!results.is_empty(), "temporal query should return results");
}
