//! Runtime null-safety smoke for the real Rust write path.
//!
//! Goal:
//! - exercise `WriteOrchestrator.add_memory()` with many nullable field combos
//! - verify auto-supersede against a legacy row whose `metadata_json` is NULL
//! - verify post-write `report_outcome()` still works on the saved row
//!
//! This stays in test-only surface: no business code changes.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use memra_core::core::write_orchestrator::{AddMemoryParams, AddMemoryResult, WriteOrchestrator};
use memra_core::embedding::embed_text;
use memra_core::retrieval::scoring::cosine_similarity;
use memra_core::storage::cold_storage::ColdStorageWriter;
use memra_core::storage::db::DbPool;
use memra_core::storage::writer::vector_to_blob;
use rusqlite::{Connection, params};
use serde_json::{Map, Value};

const ITERATIONS: usize = 200;
const BASE_CONTENT: &str = "write null fuzz anchor content for runtime smoke";

fn temp_db_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("ma-write-null-fuzz-{label}-{nanos}.sqlite3"))
}

fn create_full_schema(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE notes (
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
        CREATE VIRTUAL TABLE notes_fts USING fts5(note_id UNINDEXED, content);
        CREATE TABLE note_events (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            note_id         TEXT NOT NULL,
            event_type      TEXT NOT NULL,
            related_note_id TEXT,
            payload_json    TEXT,
            created_at      TEXT
        );
        CREATE TABLE note_relations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            from_note_id TEXT NOT NULL,
            to_note_id TEXT NOT NULL,
            relation_type TEXT NOT NULL,
            strength REAL,
            created_at TEXT
        );",
    )
    .expect("schema creation failed");
}

fn clear_tables(pool: &DbPool) {
    pool.with_conn(|conn| {
        conn.execute_batch(
            "DELETE FROM note_relations;
             DELETE FROM note_events;
             DELETE FROM notes_fts;
             DELETE FROM notes;",
        )
        .expect("clear tables");
    });
}

fn unit(vec: &[f32]) -> Vec<f32> {
    let norm = vec
        .iter()
        .map(|v| (*v as f64) * (*v as f64))
        .sum::<f64>()
        .sqrt() as f32;
    assert!(norm > 0.0, "embedding norm must be > 0");
    vec.iter().map(|v| *v / norm).collect()
}

fn orthogonal_unit(vec: &[f32]) -> Vec<f32> {
    assert!(vec.len() >= 2, "need at least 2 dims");
    for i in 0..(vec.len() - 1) {
        let a = vec[i];
        let b = vec[i + 1];
        if a.abs() > 1e-6 || b.abs() > 1e-6 {
            let mut out = vec![0.0f32; vec.len()];
            out[i] = -b;
            out[i + 1] = a;
            return unit(&out);
        }
    }

    let mut out = vec![0.0f32; vec.len()];
    out[0] = 1.0;
    out[1] = -1.0;
    unit(&out)
}

fn similarity_probe(base: &[f32], target_cosine: f32) -> Vec<f32> {
    let base_unit = unit(base);
    let ortho = orthogonal_unit(&base_unit);
    let ortho_weight = (1.0f32 - target_cosine * target_cosine).sqrt();

    base_unit
        .iter()
        .zip(ortho.iter())
        .map(|(u, p)| target_cosine * *u + ortho_weight * *p)
        .collect()
}

fn insert_legacy_candidate(pool: &DbPool, note_id: &str, iteration: usize, similarity_vec: &[f32]) {
    let use_blob = iteration % 2 == 0;
    let category = if iteration % 3 == 0 {
        Some("bug")
    } else {
        None
    };
    let confidence = if iteration % 5 == 0 {
        None
    } else {
        Some(0.81f64)
    };
    let created_at = if iteration % 7 == 0 {
        None
    } else {
        Some(format!("2026-04-21T10:00:{:02}.000Z", iteration % 60))
    };
    let updated_at = if iteration % 11 == 0 {
        None
    } else {
        Some(format!("2026-04-21T10:01:{:02}.000Z", iteration % 60))
    };
    let topic_key = if iteration % 13 == 0 {
        None
    } else {
        Some(format!("verified_fact:legacy-null-{iteration}"))
    };
    let room = if iteration % 17 == 0 {
        None
    } else {
        Some(format!("lane-{}", iteration % 5))
    };
    let vector_blob = if use_blob {
        Some(vector_to_blob(similarity_vec))
    } else {
        None
    };
    let vector_json = if use_blob {
        None
    } else {
        Some(serde_json::to_string(similarity_vec).expect("serialize vector"))
    };

    pool.with_conn(|conn| {
        conn.execute(
            "INSERT INTO notes (
                id, content, layer, category, is_active, confidence, project_id,
                created_at, updated_at, metadata_json, vector_json, vector_blob,
                evolution_state, topic_key, is_head, room, source, created_by
             ) VALUES (
                ?1, ?2, 'verified_fact', ?3, 1, ?4, 'test-project',
                ?5, ?6, NULL, ?7, ?8,
                'active', ?9, 1, ?10, NULL, NULL
             )",
            params![
                note_id,
                format!("legacy null metadata candidate {iteration}"),
                category,
                confidence,
                created_at,
                updated_at,
                vector_json,
                vector_blob,
                topic_key,
                room,
            ],
        )
        .expect("insert legacy candidate");
        conn.execute(
            "INSERT INTO notes_fts (note_id, content) VALUES (?1, ?2)",
            params![
                note_id,
                format!("legacy null metadata candidate {iteration}")
            ],
        )
        .expect("insert legacy fts row");
    });
}

fn make_params(iteration: usize) -> AddMemoryParams {
    let mut extra_metadata = Map::new();
    if iteration % 3 == 0 {
        extra_metadata.insert("probe".to_string(), Value::from(iteration as i64));
    }

    AddMemoryParams {
        content: BASE_CONTENT.to_string(),
        layer: Some("verified_fact".to_string()),
        category: if iteration % 2 == 0 {
            Some("bug".to_string())
        } else {
            None
        },
        confidence: if iteration % 5 == 0 { None } else { Some(0.92) },
        agent: if iteration % 7 == 0 {
            Some("codex".to_string())
        } else {
            None
        },
        room: if iteration % 11 == 0 {
            None
        } else {
            Some(format!("room-{}", iteration % 4))
        },
        role: if iteration % 13 == 0 {
            None
        } else {
            Some("assistant".to_string())
        },
        difficulty: if iteration % 17 == 0 { None } else { Some(2) },
        time_cost_hint: if iteration % 19 == 0 {
            None
        } else {
            Some("5min".to_string())
        },
        related_ids: if iteration % 23 == 0 {
            None
        } else {
            Some(vec![format!("rel-{iteration}")])
        },
        session_id: if iteration % 29 == 0 {
            None
        } else {
            Some(format!("sess-{iteration}"))
        },
        source: Some("user".to_string()),
        created_by: if iteration % 37 == 0 {
            None
        } else {
            Some("user".to_string())
        },
        extra_metadata: if extra_metadata.is_empty() {
            None
        } else {
            Some(extra_metadata)
        },
        ..Default::default()
    }
}

fn cleanup_db(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = fs::remove_file(path.with_extension("sqlite3-shm"));
}

#[test]
fn write_path_handles_nullable_fields_across_200_runtime_iterations() {
    let db_path = temp_db_path("runtime");
    {
        let conn = Connection::open(&db_path).expect("create temp db");
        create_full_schema(&conn);
    }

    let write_pool = DbPool::open(&db_path).expect("open write pool");
    let control_pool = DbPool::open(&db_path).expect("open control pool");
    let cold = ColdStorageWriter::disabled();
    let orchestrator = WriteOrchestrator::new(write_pool, cold, "test-project".to_string());

    let query_vec = embed_text(BASE_CONTENT).expect("base content should embed");

    for iteration in 0..ITERATIONS {
        clear_tables(&control_pool);

        let target_cosine = 0.72f32 + ((iteration % 15) as f32 * 0.01);
        let similarity_vec = similarity_probe(&query_vec, target_cosine);
        let measured = cosine_similarity(&query_vec, &similarity_vec);
        assert!(
            (0.70..0.88).contains(&measured),
            "iteration {iteration}: measured similarity must stay in auto-supersede window, got {measured:.4}"
        );

        let legacy_id = format!("legacy-null-{iteration:03}");
        insert_legacy_candidate(&control_pool, &legacy_id, iteration, &similarity_vec);

        let params = make_params(iteration);
        let saved_id = match orchestrator.add_memory(&params) {
            AddMemoryResult::Saved {
                id,
                superseded_ids,
                warnings,
                ..
            } => {
                assert!(
                    superseded_ids
                        .iter()
                        .any(|candidate| candidate == &legacy_id),
                    "iteration {iteration}: expected legacy row to be auto-superseded, got {superseded_ids:?}"
                );
                assert!(
                    warnings.is_empty(),
                    "iteration {iteration}: runtime smoke should not degrade into warning-only path: {warnings:?}"
                );
                id
            }
            AddMemoryResult::Duplicate {
                existing_id,
                similarity,
            } => {
                panic!(
                    "iteration {iteration}: unexpected duplicate existing_id={existing_id} similarity={similarity:.4}"
                );
            }
            AddMemoryResult::PolicySkipped { reason, .. } => {
                panic!("iteration {iteration}: unexpected policy skip: {reason}");
            }
            AddMemoryResult::Error(err) => {
                panic!("iteration {iteration}: add_memory failed: {err}");
            }
        };

        let outcome_reason = if iteration % 2 == 0 {
            Some("runtime smoke")
        } else {
            None
        };
        let updated = orchestrator
            .report_outcome(&saved_id, "confirmed", outcome_reason)
            .expect("report_outcome should succeed");
        assert!(
            updated,
            "iteration {iteration}: report_outcome must update saved row"
        );

        control_pool.with_conn(|conn| {
            let (legacy_state, legacy_active, legacy_meta): (String, i64, Option<String>) = conn
                .query_row(
                    "SELECT evolution_state, is_active, metadata_json
                     FROM notes
                     WHERE id = ?1",
                    [&legacy_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("legacy row should exist");

            assert_eq!(legacy_state, "superseded");
            assert_eq!(legacy_active, 0);

            let legacy_meta: Value = serde_json::from_str(
                legacy_meta
                    .as_deref()
                    .expect("legacy metadata_json should be created after supersede"),
            )
            .expect("legacy metadata_json must be valid JSON");
            assert_eq!(legacy_meta["superseded_by"], Value::from(saved_id.clone()));
            assert_eq!(legacy_meta["is_outdated"], Value::Bool(true));

            let forget_cascade_relations: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM note_relations
                     WHERE from_note_id = ?1
                       AND to_note_id = ?2
                       AND relation_type = 'forget_cascade'",
                    params![saved_id, legacy_id],
                    |row| row.get(0),
                )
                .expect("count note_relations");
            assert_eq!(forget_cascade_relations, 1);

            let saved_meta: Option<String> = conn
                .query_row(
                    "SELECT metadata_json FROM notes WHERE id = ?1",
                    [&saved_id],
                    |row| row.get(0),
                )
                .expect("saved row should exist");
            let saved_meta: Value = serde_json::from_str(
                saved_meta
                    .as_deref()
                    .expect("saved metadata_json should exist after outcome update"),
            )
            .expect("saved metadata_json must be valid JSON");
            assert_eq!(
                saved_meta.get("success_count").and_then(Value::as_i64),
                Some(1),
                "iteration {iteration}: confirmed outcome should write success_count"
            );
            assert_eq!(
                saved_meta.get("failure_count").and_then(Value::as_i64),
                Some(0),
                "iteration {iteration}: confirmed outcome should write failure_count=0"
            );
            // Issue #114: Rust must NOT stamp `supersedes` array on the new
            // note's metadata. Python (source of truth) does not emit this key
            // — the supersede link lives on the OLD row's `superseded_by` +
            // SQL evolution_state flip.
            assert!(
                saved_meta.get("supersedes").is_none(),
                "iteration {iteration}: issue #114 — new note metadata must not carry `supersedes` (Python parity), got {:?}",
                saved_meta.get("supersedes")
            );
        });
    }

    cleanup_db(&db_path);
}
