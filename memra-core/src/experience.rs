//! Experience substrate read/write surface for the Rust MCP backend.
//!
//! This is intentionally DB-backed and deterministic.  The Python backend still
//! owns the richer distillation/LLM evolution logic, but Rust can now expose the
//! same MCP tools without returning silent no-ops.

use std::fs;
use std::path::PathBuf;

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::retrieval::canonical_reader::{CanonicalReadSource, hydrate_head_by_canonical_or_topic};
use crate::storage::db::DbPool;

pub struct ExperienceStore {
    db: DbPool,
    project_id: String,
}

impl ExperienceStore {
    pub fn with_db(db: DbPool, project_id: impl Into<String>) -> Self {
        let store = Self {
            db,
            project_id: project_id.into(),
        };
        let _ = store.init_tables();
        store
    }

    pub fn init_tables(&self) -> rusqlite::Result<()> {
        self.db.with_conn(init_experience_tables)
    }

    pub fn search_experiences(
        &self,
        query: &str,
        limit: usize,
        statuses: Option<&[String]>,
        kind: Option<&str>,
    ) -> Value {
        let limit = limit.clamp(1, 20);
        let rows = self.db.with_conn(|conn| {
            list_experience_records(conn, &self.project_id, statuses, kind, limit * 6)
        });
        let query_lower = query.trim().to_lowercase();
        let tokens: Vec<&str> = query_lower.split_whitespace().collect();
        let mut scored = Vec::new();
        for row in rows {
            let haystack = format!(
                "{} {} {} {}",
                row.get("title").and_then(Value::as_str).unwrap_or(""),
                row.get("summary").and_then(Value::as_str).unwrap_or(""),
                row.get("topic_key").and_then(Value::as_str).unwrap_or(""),
                row.get("kind").and_then(Value::as_str).unwrap_or("")
            )
            .to_lowercase();
            let mut score = status_weight(row.get("status").and_then(Value::as_str).unwrap_or(""));
            if !query_lower.is_empty() {
                if haystack.contains(&query_lower) {
                    score += 0.7;
                }
                if !tokens.is_empty() {
                    let hits = tokens
                        .iter()
                        .filter(|token| haystack.contains(**token))
                        .count();
                    score += ((hits as f64 / tokens.len() as f64) * 0.5).min(0.5);
                }
            }
            scored.push((score, row));
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let results: Vec<Value> = scored
            .into_iter()
            .take(limit)
            .map(|(score, mut row)| {
                row["search_score"] = json!((score * 1000.0).round() / 1000.0);
                row
            })
            .collect();

        json!({
            "query": query,
            "results": results,
            "count": results.len(),
            "rust_notes": "DB-backed deterministic experience search; Python still owns substrate refresh/distillation.",
        })
    }

    pub fn review_experience(
        &self,
        experience_id: Option<&str>,
        topic: Option<&str>,
        force: bool,
    ) -> Value {
        let Some(experience) = self.resolve_experience(experience_id, topic) else {
            return json!({
                "error": "experience_not_found",
                "message": "No matching experience found for experience_id/topic",
            });
        };
        let id = experience["id"].as_str().unwrap_or_default().to_string();
        let reviews = self.db.with_conn(|conn| {
            let existing = list_reviews(conn, &id);
            if force || existing.is_empty() {
                let generated = deterministic_reviews(&experience);
                for review in &generated {
                    let _ = upsert_review(conn, &id, review);
                }
                generated
            } else {
                existing
            }
        });
        let readiness = readiness_summary(&experience, &reviews);
        json!({
            "experience": experience,
            "reviews": reviews,
            "readiness": readiness,
            "rust_notes": "Deterministic Rust review; Python retains richer expert review heuristics.",
        })
    }

    pub fn get_surfaces(
        &self,
        target: &str,
        limit: usize,
        topic: Option<&str>,
        auto_topic: bool,
    ) -> Value {
        let limit = limit.clamp(1, 20);
        let target = if target.trim().is_empty() {
            "all"
        } else {
            target
        };
        if target != "all" && !matches!(target, "hermes" | "harness" | "autoresearch" | "cold_eye")
        {
            return json!({"error": "unknown surface target", "target": target});
        }
        let stable_status = vec!["stable".to_string()];
        let review_status = vec!["review_required".to_string(), "candidate".to_string()];
        let degraded_status = vec!["degraded".to_string()];
        let stable = self.db.with_conn(|conn| {
            list_experience_records(
                conn,
                &self.project_id,
                Some(&stable_status),
                None,
                limit * 4,
            )
        });
        let review = self.db.with_conn(|conn| {
            list_experience_records(
                conn,
                &self.project_id,
                Some(&review_status),
                None,
                limit * 4,
            )
        });
        let degraded = self.db.with_conn(|conn| {
            list_experience_records(
                conn,
                &self.project_id,
                Some(&degraded_status),
                None,
                limit * 4,
            )
        });
        let routing = json!({
            "topic": topic,
            "auto_topic": auto_topic,
            "source": "rust-experience-store",
        });
        let mut surfaces = serde_json::Map::new();
        if target == "all" || target == "hermes" {
            surfaces.insert(
                "hermes".to_string(),
                hermes_surface(&stable, &review, limit, &routing),
            );
        }
        if target == "all" || target == "harness" {
            surfaces.insert(
                "harness".to_string(),
                harness_surface(&stable, &review, &degraded, limit, &routing),
            );
        }
        if target == "all" || target == "autoresearch" {
            surfaces.insert(
                "autoresearch".to_string(),
                autoresearch_surface(&stable, &degraded, limit, &routing),
            );
        }
        if target == "all" || target == "cold_eye" {
            surfaces.insert(
                "cold_eye".to_string(),
                cold_eye_surface(&stable, &review, &degraded, limit, &routing),
            );
        }
        if target == "all" {
            json!({
                "target": "all",
                "summary": "Rust experience surfaces generated from experience_records",
                "routing": routing,
                "surfaces": surfaces,
            })
        } else {
            let surface = surfaces.remove(target).unwrap_or_else(|| json!({}));
            json!({
                "target": target,
                "summary": surface.get("summary").and_then(Value::as_str).unwrap_or(""),
                "routing": routing,
                "surface": surface,
            })
        }
    }

    pub fn build_artifacts(
        &self,
        target: &str,
        limit: usize,
        topic: Option<&str>,
        auto_topic: bool,
        save: bool,
    ) -> Value {
        let batch_id = format!("abatch_{}", Uuid::new_v4().simple());
        let now = Utc::now().to_rfc3339();
        let surfaces = self.get_surfaces(target, limit, topic, auto_topic);
        let targets: Vec<&str> = if target == "all" {
            vec!["hermes", "harness"]
        } else {
            vec![target]
        };
        let mut payload = serde_json::Map::new();
        payload.insert("artifact_batch_id".to_string(), json!(batch_id));
        payload.insert("generated_at".to_string(), json!(now));
        payload.insert(
            "routing".to_string(),
            surfaces
                .get("routing")
                .cloned()
                .unwrap_or_else(|| json!({})),
        );
        for artifact_target in targets {
            let Some((key, markdown)) = artifact_markdown(artifact_target, &surfaces) else {
                continue;
            };
            let artifact_run_id = format!("arun_{}", Uuid::new_v4().simple());
            let feedback_token = format!("aftk_{}", Uuid::new_v4().simple());
            let artifact_path = if save {
                write_artifact_file(&self.project_id, artifact_target, topic, &markdown).ok()
            } else {
                None
            };
            let routing = surfaces
                .get("routing")
                .cloned()
                .unwrap_or_else(|| json!({}));
            self.db.with_conn(|conn| {
                let _ = insert_artifact_run(
                    conn,
                    &ArtifactRunInsert {
                        id: &artifact_run_id,
                        batch_id: payload
                            .get("artifact_batch_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        project_id: &self.project_id,
                        target: artifact_target,
                        feedback_token: &feedback_token,
                        topic,
                        routing: &routing,
                        artifact_path: artifact_path.as_deref(),
                        summary: &markdown,
                    },
                );
            });
            payload.insert(
                key,
                json!({
                    "artifact_run_id": artifact_run_id,
                    "feedback_token": feedback_token,
                    "artifact_target": artifact_target,
                    "artifact_path": artifact_path,
                    "markdown": markdown,
                    "feedback_contract": {
                        "feedback_token": feedback_token,
                        "artifact_target": artifact_target,
                        "helpful_feedback": {"tool": "report_artifact_feedback", "verdict": "helpful"},
                        "miss_feedback": {"tool": "report_artifact_feedback", "verdict": "miss"},
                    }
                }),
            );
        }
        Value::Object(payload)
    }

    pub fn refresh_substrate(&self, limit: usize) -> Value {
        let requested_limit = limit.clamp(1, 100);
        match self.db.with_conn(|conn| {
            init_experience_tables(conn)?;
            let activation_gate =
                phase2_activation_gate(conn, &self.project_id, requested_limit)?;
            let refresh_limit = activation_gate
                .get("activation_limit")
                .and_then(Value::as_u64)
                .unwrap_or(requested_limit as u64) as usize;
            let seeded = seed_procedure_experiences(conn, &self.project_id, 80)?;
            let review_required =
                materialize_review_required_experiences(conn, &self.project_id, refresh_limit)?;
            let degraded = materialize_degraded_experiences(conn, &self.project_id, refresh_limit)?;
            let candidates = review_required + degraded;
            let reviewed =
                review_candidate_experiences(conn, &self.project_id, refresh_limit.max(6))?;
            let lifecycle_updates = refresh_experience_statuses(conn, &self.project_id, 200)?;
            Ok::<Value, rusqlite::Error>(json!({
                "seeded": seeded,
                "candidates": candidates,
                "reviewed": reviewed,
                "lifecycle_updates": lifecycle_updates,
                "action_updates": 0,
                "activation_gate": activation_gate,
                "rust_notes": "Deterministic Rust substrate refresh seeds procedure_schema records, review-required notes, degraded notes, and deterministic reviews.",
            }))
        }) {
            Ok(value) => value,
            Err(error) => json!({
                "error": "refresh_substrate_failed",
                "message": error.to_string(),
            }),
        }
    }

    pub fn get_feedback_due(&self, limit: usize, refresh: bool) -> Value {
        let bounded_limit = limit.clamp(1, 20);
        let refresh_result = if refresh {
            Some(self.refresh_substrate(bounded_limit.max(6)))
        } else {
            None
        };
        match self.db.with_conn(|conn| {
            init_experience_tables(conn)?;
            let activation_gate = refresh_result
                .as_ref()
                .and_then(|value| value.get("activation_gate"))
                .filter(|value| value.is_object())
                .cloned()
                .map(Ok)
                .unwrap_or_else(|| {
                    phase2_activation_gate(conn, &self.project_id, bounded_limit.max(6))
                })?;
            let review_statuses = vec!["review_required".to_string()];
            let review_queue = list_experience_records(
                conn,
                &self.project_id,
                Some(&review_statuses),
                None,
                bounded_limit,
            );
            let review_total =
                count_experience_records(conn, &self.project_id, Some(&review_statuses), None)?;
            let recommended_actions =
                list_pending_recommended_actions(conn, &self.project_id, bounded_limit)?;
            let recommended_total = count_pending_recommended_actions(conn, &self.project_id)?;
            let dream_due = list_dream_feedback_due(conn, &self.project_id, bounded_limit)?;
            let dream_total = count_dream_feedback_due(conn, &self.project_id)?;
            let feedback_deficit = activation_gate
                .get("feedback_deficit")
                .and_then(Value::as_i64)
                .unwrap_or(0);

            Ok::<Value, rusqlite::Error>(json!({
                "project_id": self.project_id,
                "activation_gate": activation_gate,
                "counts": {
                    "feedback_deficit": feedback_deficit,
                    "review_required": review_total,
                    "recommended_actions": recommended_total,
                    "dream_feedback_due": dream_total,
                },
                "returned_counts": {
                    "review_required": review_queue.len(),
                    "recommended_actions": recommended_actions.len(),
                    "dream_feedback_due": dream_due.len(),
                },
                "review_required": review_queue
                    .iter()
                    .map(feedback_due_review_item)
                    .collect::<Vec<_>>(),
                "recommended_actions": recommended_actions
                    .iter()
                    .map(|action| feedback_due_action_item(conn, &self.project_id, action))
                    .collect::<Vec<_>>(),
                "dream_feedback_due": dream_due,
            }))
        }) {
            Ok(value) => value,
            Err(error) => json!({
                "error": "feedback_due_failed",
                "message": error.to_string(),
                "project_id": self.project_id,
            }),
        }
    }

    pub fn record_topic_feedback(
        &self,
        topic: &str,
        verdict: &str,
        artifact_target: Option<&str>,
        source: &str,
        reason: Option<&str>,
        context: Option<Value>,
    ) -> Value {
        if topic.trim().is_empty() {
            return json!({"error": "topic_required"});
        }
        let topic_norm = normalize_topic(topic);
        let terms: Vec<String> = topic_norm.split_whitespace().map(str::to_string).collect();
        let context = context.unwrap_or_else(|| json!({}));
        let result = self.db.with_conn(|conn| {
            conn.execute(
                // datetime() OK here: write-side created/updated stamps only;
                // this statement does not compare or order timestamp text.
                "INSERT INTO experience_topic_feedback
                 (project_id, topic, topic_norm, verdict, artifact_target, source, reason,
                  topic_terms_json, context_json, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'), datetime('now'))",
                params![
                    self.project_id,
                    topic,
                    topic_norm,
                    verdict,
                    artifact_target,
                    source,
                    reason,
                    serde_json::to_string(&terms).unwrap_or_else(|_| "[]".to_string()),
                    serde_json::to_string(&context).unwrap_or_else(|_| "{}".to_string()),
                ],
            )
        });
        match result {
            Ok(_) => json!({
                "project_id": self.project_id,
                "topic": topic,
                "topic_norm": topic_norm,
                "verdict": verdict,
                "artifact_target": artifact_target,
                "source": source,
                "reason": reason,
            }),
            Err(error) => json!({"error": "topic_feedback_failed", "message": error.to_string()}),
        }
    }

    pub fn report_artifact_feedback(
        &self,
        feedback_token: Option<&str>,
        artifact_run_id: Option<&str>,
        verdict: &str,
        reason: Option<&str>,
        source: &str,
    ) -> Value {
        let run = self
            .db
            .with_conn(|conn| get_artifact_run(conn, feedback_token, artifact_run_id));
        let Some(run) = run else {
            return json!({"error": "artifact_run_not_found"});
        };
        let Some(topic) = run
            .get("topic")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            return json!({"error": "artifact_run_missing_topic"});
        };
        let normalized_verdict = match verdict {
            "helpful" | "artifact_helpful" => "artifact_helpful",
            "miss" | "artifact_miss" => "artifact_miss",
            other => return json!({"error": "unknown_artifact_feedback_verdict", "verdict": other}),
        };
        let topic_feedback = self.record_topic_feedback(
            topic,
            normalized_verdict,
            run.get("artifact_target").and_then(Value::as_str),
            source,
            reason,
            Some(json!({
                "artifact_run_id": run["artifact_run_id"],
                "feedback_token": run["feedback_token"],
                "artifact_path": run.get("artifact_path"),
                "routing": run.get("routing"),
            })),
        );
        json!({
            "artifact_run_id": run["artifact_run_id"],
            "feedback_token": run["feedback_token"],
            "artifact_target": run.get("artifact_target"),
            "topic": topic,
            "verdict": normalized_verdict,
            "topic_feedback": topic_feedback,
        })
    }

    fn resolve_experience(
        &self,
        experience_id: Option<&str>,
        topic: Option<&str>,
    ) -> Option<Value> {
        if let Some(id) = experience_id {
            return self
                .db
                .with_conn(|conn| get_experience(conn, &self.project_id, id));
        }
        let topic = topic?;
        self.search_experiences(topic, 1, None, None)
            .get("results")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .cloned()
    }
}

fn init_experience_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "-- datetime() OK in this schema block: DEFAULT values only generate
        -- current write timestamps; later ordering over these fields uses
        -- stored text but does not select same-second winners as correctness
        -- gates.
        CREATE TABLE IF NOT EXISTS experience_records (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            summary TEXT NOT NULL,
            kind TEXT NOT NULL,
            status TEXT NOT NULL,
            origin TEXT,
            project_id TEXT,
            source_note_id TEXT,
            topic_key TEXT,
            confidence REAL,
            evidence_note_ids_json TEXT,
            metadata_json TEXT,
            superseded_by TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_experience_status ON experience_records(status, updated_at DESC);
        CREATE INDEX IF NOT EXISTS idx_experience_kind ON experience_records(kind, updated_at DESC);
        CREATE INDEX IF NOT EXISTS idx_experience_project ON experience_records(project_id, updated_at DESC);
        CREATE INDEX IF NOT EXISTS idx_experience_source_note ON experience_records(source_note_id);
        CREATE INDEX IF NOT EXISTS idx_experience_topic_key ON experience_records(topic_key);
        CREATE TABLE IF NOT EXISTS experience_reviews (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            experience_id TEXT NOT NULL,
            lens TEXT NOT NULL,
            verdict TEXT NOT NULL,
            confidence REAL NOT NULL DEFAULT 0.5,
            why TEXT,
            risk TEXT,
            recommended_action TEXT,
            evidence_refs_json TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(experience_id, lens)
        );
        CREATE INDEX IF NOT EXISTS idx_experience_reviews_exp ON experience_reviews(experience_id, updated_at DESC);
        CREATE INDEX IF NOT EXISTS idx_experience_reviews_lens ON experience_reviews(lens, updated_at DESC);
        CREATE TABLE IF NOT EXISTS experience_actions (
            id TEXT PRIMARY KEY,
            experience_id TEXT NOT NULL,
            action_type TEXT NOT NULL,
            target_system TEXT,
            status TEXT NOT NULL DEFAULT 'recommended',
            summary TEXT NOT NULL,
            payload_json TEXT,
            outcome_note_id TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_experience_actions_exp ON experience_actions(experience_id, updated_at DESC);
        CREATE INDEX IF NOT EXISTS idx_experience_actions_status ON experience_actions(status, updated_at DESC);
        CREATE TABLE IF NOT EXISTS experience_topic_feedback (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id TEXT NOT NULL,
            topic TEXT NOT NULL,
            topic_norm TEXT NOT NULL,
            verdict TEXT NOT NULL,
            artifact_target TEXT,
            source TEXT NOT NULL DEFAULT 'user',
            reason TEXT,
            topic_terms_json TEXT,
            context_json TEXT,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_topic_feedback_project_topic
            ON experience_topic_feedback(project_id, topic_norm, updated_at DESC);
        CREATE INDEX IF NOT EXISTS idx_topic_feedback_verdict
            ON experience_topic_feedback(verdict, updated_at DESC);
        CREATE TABLE IF NOT EXISTS experience_artifact_runs (
            id TEXT PRIMARY KEY,
            batch_id TEXT,
            project_id TEXT NOT NULL,
            artifact_target TEXT NOT NULL,
            feedback_token TEXT NOT NULL UNIQUE,
            topic TEXT,
            routing_json TEXT,
            artifact_path TEXT,
            summary TEXT,
            source TEXT NOT NULL DEFAULT 'artifact_build',
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        );
        CREATE INDEX IF NOT EXISTS idx_artifact_runs_project
            ON experience_artifact_runs(project_id, updated_at DESC);
        CREATE INDEX IF NOT EXISTS idx_artifact_runs_batch
            ON experience_artifact_runs(batch_id, updated_at DESC);
        CREATE INDEX IF NOT EXISTS idx_artifact_runs_token
            ON experience_artifact_runs(feedback_token);",
    )
}

fn phase2_activation_gate(
    conn: &Connection,
    project_id: &str,
    requested_limit: usize,
) -> rusqlite::Result<Value> {
    let cutoff = (Utc::now() - chrono::Duration::days(30)).to_rfc3339();
    let recent_feedback_count: i64 = conn.query_row(
        "SELECT COUNT(*)
         FROM experience_topic_feedback
         WHERE project_id = ?1
           AND updated_at >= ?2",
        params![project_id, cutoff],
        |row| row.get(0),
    )?;
    let target_feedback_count = 50_i64;
    let deficit = (target_feedback_count - recent_feedback_count).max(0);
    let mut activation_limit = requested_limit.max(8);
    if deficit > 0 {
        activation_limit = 24.min(activation_limit.max(deficit as usize));
    }
    Ok(json!({
        "window_days": 30,
        "target_feedback_count": target_feedback_count,
        "recent_feedback_count": recent_feedback_count,
        "feedback_deficit": deficit,
        "under_target": deficit > 0,
        "requested_limit": requested_limit,
        "activation_limit": activation_limit,
    }))
}

fn seed_procedure_experiences(
    conn: &Connection,
    project_id: &str,
    limit: usize,
) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, content, confidence, topic_key, metadata_json, created_at, updated_at
         FROM notes
         WHERE layer = 'procedure_schema'
           AND is_active = 1
         ORDER BY updated_at DESC, created_at DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |row| {
        Ok((
            row.get::<_, String>("id")?,
            row.get::<_, String>("content")?,
            row.get::<_, Option<f64>>("confidence")?.unwrap_or(0.85),
            row.get::<_, Option<String>>("topic_key")?,
            row.get::<_, Option<String>>("metadata_json")?,
            row.get::<_, Option<String>>("created_at")?,
            row.get::<_, Option<String>>("updated_at")?,
        ))
    })?;
    let mut written = 0usize;
    for row in rows {
        let (note_id, content, confidence, topic_key, metadata_json, created_at, updated_at) = row?;
        let metadata = parse_json_object(metadata_json.as_deref());
        let status = if metadata
            .get("is_outdated")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || metadata
                .get("is_corrected")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            "degraded"
        } else {
            "stable"
        };
        let steps_count = metadata
            .get("steps")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let activation_cues = metadata
            .get("activation_cues")
            .cloned()
            .unwrap_or_else(|| json!([]));
        upsert_experience_record(
            conn,
            &ExperienceRecordInsert {
                id: stable_experience_id("procedure", &note_id),
                title: compact_text(&content, 48),
                summary: content,
                kind: "procedure".to_string(),
                status: status.to_string(),
                origin: "procedure_schema_backfill".to_string(),
                project_id: project_id.to_string(),
                source_note_id: Some(note_id.clone()),
                topic_key,
                confidence,
                evidence_note_ids: json!([note_id]),
                metadata: json!({
                    "source_layer": "procedure_schema",
                    "steps_count": steps_count,
                    "activation_cues": activation_cues,
                    "record_type": metadata.get("record_type").cloned().unwrap_or(Value::Null),
                    "review_after": metadata.get("review_after").cloned().unwrap_or(Value::Null),
                    "procedure_status": metadata.get("procedure_status").cloned().unwrap_or_else(|| json!("active")),
                    "source_metadata": metadata,
                }),
                superseded_by: None,
                created_at: created_at.unwrap_or_else(|| Utc::now().to_rfc3339()),
                updated_at: updated_at.unwrap_or_else(|| Utc::now().to_rfc3339()),
            },
        )?;
        written += 1;
    }
    Ok(written)
}

fn materialize_review_required_experiences(
    conn: &Connection,
    project_id: &str,
    limit: usize,
) -> rusqlite::Result<usize> {
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT id, content, layer, category, confidence, review_after, created_at
         FROM notes
         WHERE is_active = 1
           AND COALESCE(evolution_state, 'active') = 'active'
           AND review_after IS NOT NULL
           AND review_after <= ?1
         ORDER BY review_after ASC, created_at DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![now, limit as i64], |row| {
        Ok((
            row.get::<_, String>("id")?,
            row.get::<_, String>("content")?,
            row.get::<_, Option<String>>("layer")?,
            row.get::<_, Option<String>>("category")?,
            row.get::<_, Option<f64>>("confidence")?.unwrap_or(0.62),
            row.get::<_, Option<String>>("review_after")?,
            row.get::<_, Option<String>>("created_at")?,
        ))
    })?;
    let mut written = 0usize;
    for row in rows {
        let (note_id, content, layer, category, confidence, review_after, created_at) = row?;
        upsert_experience_record(
            conn,
            &ExperienceRecordInsert {
                id: stable_experience_id("review", &note_id),
                title: format!("待复核: {}", compact_text(&content, 56)),
                summary: content,
                kind: infer_experience_kind(layer.as_deref(), category.as_deref(), false),
                status: "review_required".to_string(),
                origin: "note_review_queue".to_string(),
                project_id: project_id.to_string(),
                source_note_id: Some(note_id.clone()),
                topic_key: None,
                confidence,
                evidence_note_ids: json!([note_id]),
                metadata: json!({
                    "review_after": review_after,
                    "source_layer": layer,
                    "source_category": category,
                }),
                superseded_by: None,
                created_at: created_at.unwrap_or_else(|| Utc::now().to_rfc3339()),
                updated_at: Utc::now().to_rfc3339(),
            },
        )?;
        written += 1;
    }
    Ok(written)
}

fn materialize_degraded_experiences(
    conn: &Connection,
    project_id: &str,
    limit: usize,
) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, content, layer, category, metadata_json, created_at
         FROM notes
         WHERE is_active = 1
           AND COALESCE(evolution_state, 'active') = 'active'
           AND (
             json_extract(COALESCE(metadata_json, '{}'), '$.is_outdated') = 1
             OR json_extract(COALESCE(metadata_json, '{}'), '$.is_corrected') = 1
             OR COALESCE(json_extract(COALESCE(metadata_json, '{}'), '$.failure_count'), 0) > 0
           )
         ORDER BY updated_at DESC, created_at DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |row| {
        Ok((
            row.get::<_, String>("id")?,
            row.get::<_, String>("content")?,
            row.get::<_, Option<String>>("layer")?,
            row.get::<_, Option<String>>("category")?,
            row.get::<_, Option<String>>("metadata_json")?,
            row.get::<_, Option<String>>("created_at")?,
        ))
    })?;
    let mut written = 0usize;
    for row in rows {
        let (note_id, content, layer, category, metadata_json, created_at) = row?;
        let metadata = parse_json_object(metadata_json.as_deref());
        let failure_count = metadata
            .get("failure_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        upsert_experience_record(
            conn,
            &ExperienceRecordInsert {
                id: stable_experience_id("degraded", &note_id),
                title: format!("降级经验: {}", compact_text(&content, 56)),
                summary: content,
                kind: infer_experience_kind(layer.as_deref(), category.as_deref(), true),
                status: "degraded".to_string(),
                origin: "outcome_feedback".to_string(),
                project_id: project_id.to_string(),
                source_note_id: Some(note_id.clone()),
                topic_key: None,
                confidence: 0.38,
                evidence_note_ids: json!([note_id]),
                metadata: json!({
                    "failure_count": failure_count,
                    "is_outdated": metadata.get("is_outdated").cloned().unwrap_or(Value::Bool(false)),
                    "is_corrected": metadata.get("is_corrected").cloned().unwrap_or(Value::Bool(false)),
                    "source_layer": layer,
                    "source_category": category,
                }),
                superseded_by: None,
                created_at: created_at.unwrap_or_else(|| Utc::now().to_rfc3339()),
                updated_at: Utc::now().to_rfc3339(),
            },
        )?;
        written += 1;
    }
    Ok(written)
}

fn review_candidate_experiences(
    conn: &Connection,
    project_id: &str,
    limit: usize,
) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id
         FROM experience_records
         WHERE project_id = ?1
           AND status = 'candidate'
         ORDER BY updated_at DESC, created_at DESC
         LIMIT ?2",
    )?;
    let ids = stmt
        .query_map(params![project_id, limit as i64], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut reviewed = 0usize;
    for id in ids {
        if !list_reviews(conn, &id).is_empty() {
            continue;
        }
        let Some(experience) = get_experience(conn, project_id, &id) else {
            continue;
        };
        for review in deterministic_reviews(&experience) {
            upsert_review(conn, &id, &review)?;
        }
        reviewed += 1;
    }
    Ok(reviewed)
}

fn refresh_experience_statuses(
    conn: &Connection,
    project_id: &str,
    limit: usize,
) -> rusqlite::Result<usize> {
    let statuses = vec!["candidate".to_string(), "review_required".to_string()];
    let records = list_experience_records(conn, project_id, Some(&statuses), None, limit);
    let mut updated = 0usize;
    for record in records {
        let reviews = record
            .get("reviews")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let readiness = readiness_summary(&record, &reviews);
        if readiness
            .get("promotion_gate_passed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let Some(id) = record.get("id").and_then(Value::as_str) else {
                continue;
            };
            updated += conn.execute(
                "UPDATE experience_records
                 SET status = 'stable', updated_at = datetime('now')
                 WHERE id = ?1 AND project_id = ?2 AND status != 'stable'",
                params![id, project_id],
            )?;
        }
    }
    Ok(updated)
}

struct ExperienceRecordInsert {
    id: String,
    title: String,
    summary: String,
    kind: String,
    status: String,
    origin: String,
    project_id: String,
    source_note_id: Option<String>,
    topic_key: Option<String>,
    confidence: f64,
    evidence_note_ids: Value,
    metadata: Value,
    superseded_by: Option<String>,
    created_at: String,
    updated_at: String,
}

fn upsert_experience_record(
    conn: &Connection,
    record: &ExperienceRecordInsert,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO experience_records
         (id, title, summary, kind, status, origin, project_id, source_note_id,
          topic_key, confidence, evidence_note_ids_json, metadata_json,
          superseded_by, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(id) DO UPDATE SET
            title = excluded.title,
            summary = excluded.summary,
            kind = excluded.kind,
            status = excluded.status,
            origin = excluded.origin,
            project_id = excluded.project_id,
            source_note_id = excluded.source_note_id,
            topic_key = excluded.topic_key,
            confidence = excluded.confidence,
            evidence_note_ids_json = excluded.evidence_note_ids_json,
            metadata_json = excluded.metadata_json,
            superseded_by = excluded.superseded_by,
            updated_at = excluded.updated_at",
        params![
            record.id,
            record.title,
            record.summary,
            record.kind,
            record.status,
            record.origin,
            record.project_id,
            record.source_note_id,
            record.topic_key,
            record.confidence,
            record.evidence_note_ids.to_string(),
            record.metadata.to_string(),
            record.superseded_by,
            record.created_at,
            record.updated_at,
        ],
    )?;
    Ok(())
}

fn list_experience_records(
    conn: &Connection,
    project_id: &str,
    statuses: Option<&[String]>,
    kind: Option<&str>,
    limit: usize,
) -> Vec<Value> {
    let mut where_parts = vec!["project_id = ?".to_string()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(project_id.to_string())];
    if let Some(statuses) = statuses.filter(|s| !s.is_empty()) {
        where_parts.push(format!(
            "status IN ({})",
            vec!["?"; statuses.len()].join(",")
        ));
        for status in statuses {
            params.push(Box::new(status.to_string()));
        }
    }
    if let Some(kind) = kind.filter(|k| !k.is_empty()) {
        where_parts.push("kind = ?".to_string());
        params.push(Box::new(kind.to_string()));
    }
    let sql = format!(
        "SELECT * FROM experience_records WHERE {}
         ORDER BY updated_at DESC, created_at DESC LIMIT ?",
        where_parts.join(" AND ")
    );
    params.push(Box::new(limit.max(1) as i64));
    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(_) => return vec![],
    };
    stmt.query_map(rusqlite::params_from_iter(refs.iter()), |row| {
        hydrate_record(conn, row)
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

fn count_experience_records(
    conn: &Connection,
    project_id: &str,
    statuses: Option<&[String]>,
    kind: Option<&str>,
) -> rusqlite::Result<i64> {
    let mut where_parts = vec!["project_id = ?".to_string()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(project_id.to_string())];
    if let Some(statuses) = statuses.filter(|s| !s.is_empty()) {
        where_parts.push(format!(
            "status IN ({})",
            vec!["?"; statuses.len()].join(",")
        ));
        for status in statuses {
            params.push(Box::new(status.to_string()));
        }
    }
    if let Some(kind) = kind.filter(|k| !k.is_empty()) {
        where_parts.push("kind = ?".to_string());
        params.push(Box::new(kind.to_string()));
    }
    let sql = format!(
        "SELECT COUNT(*) FROM experience_records WHERE {}",
        where_parts.join(" AND ")
    );
    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    conn.query_row(&sql, rusqlite::params_from_iter(refs.iter()), |row| {
        row.get(0)
    })
}

fn get_experience(conn: &Connection, project_id: &str, id: &str) -> Option<Value> {
    conn.query_row(
        "SELECT * FROM experience_records WHERE id = ?1 AND project_id = ?2",
        params![id, project_id],
        |row| hydrate_record(conn, row),
    )
    .optional()
    .ok()
    .flatten()
}

fn hydrate_record(conn: &Connection, row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let id: String = row.get("id")?;
    let metadata_json: Option<String> = row.get("metadata_json")?;
    let evidence_json: Option<String> = row.get("evidence_note_ids_json")?;
    let metadata = parse_json_object(metadata_json.as_deref());
    let evidence_note_ids = parse_json_array(evidence_json.as_deref());
    let reviews = list_reviews(conn, &id);
    let actions = list_actions(conn, &id);
    let kind: String = row.get("kind")?;
    let status: String = row.get("status")?;
    let summary: String = row.get("summary")?;
    let confidence: Option<f64> = row.get("confidence")?;
    let created_at: String = row.get("created_at")?;
    let updated_at: String = row.get("updated_at")?;
    let source_note_id: Option<String> = row.get("source_note_id")?;
    let evidence_refs = evidence_refs(&metadata, source_note_id.as_deref());
    let stored_topic_key: Option<String> = row.get("topic_key")?;
    let (topic_key, canonical_read_source) = canonical_topic_key_for_record(
        conn,
        source_note_id.as_deref(),
        stored_topic_key.as_deref(),
        &metadata,
    );
    let readiness = readiness_summary(
        &json!({
            "status": status,
            "confidence": confidence.unwrap_or(0.0),
            "evidence_note_ids": evidence_note_ids,
            "evidence_refs": evidence_refs,
        }),
        &reviews,
    );

    Ok(json!({
        "id": id,
        "title": row.get::<_, String>("title")?,
        "summary": summary,
        "content": summary,
        "kind": kind,
        "kind_label": kind_label(&kind),
        "status": status,
        "origin": row.get::<_, Option<String>>("origin")?.unwrap_or_default(),
        "source_note_id": source_note_id,
        "topic_key": topic_key,
        "stored_topic_key": stored_topic_key,
        "canonical_read_source": canonical_read_source.as_str(),
        "confidence": confidence.unwrap_or(0.0),
        "evidence_note_ids": evidence_note_ids,
        "evidence_refs": evidence_refs,
        "metadata": metadata,
        "reviews": reviews,
        "actions": actions,
        "updated_at": updated_at,
        "created_at": created_at,
        "readiness": readiness,
        "promotion_gate_passed": readiness.get("promotion_gate_passed").and_then(Value::as_bool).unwrap_or(false),
        "promotion_blockers": readiness.get("promotion_blockers").cloned().unwrap_or_else(|| json!([])),
    }))
}

fn canonical_topic_key_for_record(
    conn: &Connection,
    source_note_id: Option<&str>,
    stored_topic_key: Option<&str>,
    metadata: &Value,
) -> (Option<String>, CanonicalReadSource) {
    let root_id = metadata
        .get("root_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match hydrate_head_by_canonical_or_topic(conn, source_note_id, root_id, stored_topic_key) {
        Ok(read) => {
            let topic_key = read
                .note
                .and_then(|note| note.topic_key)
                .or_else(|| stored_topic_key.map(str::to_string));
            (topic_key, read.source)
        }
        Err(_) => (
            stored_topic_key.map(str::to_string),
            CanonicalReadSource::Miss,
        ),
    }
}

fn list_reviews(conn: &Connection, experience_id: &str) -> Vec<Value> {
    let mut stmt = match conn.prepare(
        "SELECT experience_id, lens, verdict, confidence, why, risk, recommended_action,
                evidence_refs_json, updated_at
         FROM experience_reviews
         WHERE experience_id = ?
         ORDER BY updated_at DESC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return vec![],
    };
    stmt.query_map([experience_id], |row| {
        Ok(json!({
            "experience_id": row.get::<_, String>(0)?,
            "lens": row.get::<_, String>(1)?,
            "verdict": row.get::<_, String>(2)?,
            "confidence": row.get::<_, f64>(3)?,
            "why": row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            "risk": row.get::<_, Option<String>>(5)?.unwrap_or_default(),
            "recommended_action": row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            "evidence_refs": parse_json_array(row.get::<_, Option<String>>(7)?.as_deref()),
            "updated_at": row.get::<_, Option<String>>(8)?,
        }))
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

fn list_actions(conn: &Connection, experience_id: &str) -> Vec<Value> {
    let mut stmt = match conn.prepare(
        "SELECT id, action_type, target_system, status, summary, payload_json,
                outcome_note_id, updated_at
         FROM experience_actions
         WHERE experience_id = ?
         ORDER BY updated_at DESC",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return vec![],
    };
    stmt.query_map([experience_id], |row| {
        Ok(json!({
            "id": row.get::<_, String>(0)?,
            "action_type": row.get::<_, String>(1)?,
            "target_system": row.get::<_, Option<String>>(2)?,
            "status": row.get::<_, String>(3)?,
            "summary": row.get::<_, String>(4)?,
            "payload": parse_json_object(row.get::<_, Option<String>>(5)?.as_deref()),
            "outcome_note_id": row.get::<_, Option<String>>(6)?,
            "updated_at": row.get::<_, Option<String>>(7)?,
        }))
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

fn list_pending_recommended_actions(
    conn: &Connection,
    project_id: &str,
    limit: usize,
) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.experience_id, a.action_type, a.target_system, a.status,
                a.summary, a.payload_json, a.outcome_note_id, a.updated_at,
                e.title, e.kind, e.status AS experience_status
         FROM experience_actions a
         JOIN experience_records e ON e.id = a.experience_id
         WHERE e.project_id = ?1
           AND a.status IN ('recommended', 'rework_required')
           AND a.outcome_note_id IS NULL
         ORDER BY a.updated_at DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![project_id, limit.max(1) as i64], |row| {
        Ok(json!({
            "id": row.get::<_, String>(0)?,
            "experience_id": row.get::<_, String>(1)?,
            "action_type": row.get::<_, String>(2)?,
            "target_system": row.get::<_, Option<String>>(3)?,
            "status": row.get::<_, String>(4)?,
            "summary": row.get::<_, String>(5)?,
            "payload": parse_json_object(row.get::<_, Option<String>>(6)?.as_deref()),
            "outcome_note_id": row.get::<_, Option<String>>(7)?,
            "updated_at": row.get::<_, Option<String>>(8)?,
            "title": row.get::<_, String>(9)?,
            "kind": row.get::<_, String>(10)?,
            "experience_status": row.get::<_, String>(11)?,
        }))
    })?;
    rows.collect()
}

fn count_pending_recommended_actions(conn: &Connection, project_id: &str) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*)
         FROM experience_actions a
         JOIN experience_records e ON e.id = a.experience_id
         WHERE e.project_id = ?1
           AND a.status IN ('recommended', 'rework_required')
           AND a.outcome_note_id IS NULL",
        [project_id],
        |row| row.get(0),
    )
}

fn feedback_due_review_item(item: &Value) -> Value {
    let reviews = item
        .get("reviews")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let blockers = item
        .get("promotion_blockers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let last_reviewed_at = reviews
        .iter()
        .filter_map(|review| review.get("updated_at").and_then(Value::as_str))
        .max()
        .map(str::to_string);
    let review_outcome = if reviews.is_empty() {
        "needs_review"
    } else if !blockers.is_empty() {
        "blocked_recently"
    } else {
        "reviewed"
    };
    let expert_deliberation = item
        .get("metadata")
        .and_then(|metadata| metadata.get("expert_deliberation"))
        .and_then(Value::as_object);
    let expert_summary = expert_deliberation
        .and_then(|value| value.get("summary"))
        .and_then(Value::as_str);
    let next_actions = expert_deliberation
        .and_then(|value| value.get("next_actions"))
        .and_then(Value::as_array)
        .map(|values| values.iter().take(4).cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    let consensus_state = item
        .get("metadata")
        .and_then(|metadata| metadata.get("consensus_state"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            if item
                .get("promotion_gate_passed")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                Some("reviewed".to_string())
            } else {
                Some("needs_evidence".to_string())
            }
        });
    let experience_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
    json!({
        "experience_id": experience_id,
        "title": item.get("title").cloned().unwrap_or(Value::Null),
        "summary": item.get("summary").cloned().unwrap_or(Value::Null),
        "kind": item.get("kind").cloned().unwrap_or(Value::Null),
        "status": item.get("status").cloned().unwrap_or(Value::Null),
        "updated_at": item.get("updated_at").cloned().unwrap_or(Value::Null),
        "review_count": reviews.len(),
        "last_reviewed_at": last_reviewed_at,
        "review_outcome": review_outcome,
        "consensus_state": consensus_state,
        "promotion_blockers": blockers,
        "promotion_gate_passed": item.get("promotion_gate_passed").and_then(Value::as_bool).unwrap_or(false),
        "expert_summary": expert_summary,
        "next_actions": next_actions,
        "suggested_tool": format!("review_experience(experience_id=\"{experience_id}\", force=true)"),
    })
}

fn feedback_due_action_item(conn: &Connection, project_id: &str, action: &Value) -> Value {
    let action_id = action.get("id").and_then(Value::as_str).unwrap_or_default();
    let experience_id = action
        .get("experience_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let memory_id = feedback_note_id_for_action(conn, project_id, experience_id);
    json!({
        "action_id": action_id,
        "experience_id": experience_id,
        "memory_id": memory_id,
        "title": action.get("title").cloned().unwrap_or(Value::Null),
        "status": action.get("status").cloned().unwrap_or(Value::Null),
        "target_system": action.get("target_system").cloned().unwrap_or(Value::Null),
        "summary": action.get("summary").cloned().unwrap_or(Value::Null),
        "updated_at": action.get("updated_at").cloned().unwrap_or(Value::Null),
        "suggested_tool": format!(
            "report_outcome(memory_id=\"{memory_id}\", outcome=\"confirmed|corrected|outdated\", experience_id=\"{experience_id}\", action_id=\"{action_id}\")"
        ),
    })
}

fn feedback_note_id_for_action(conn: &Connection, project_id: &str, experience_id: &str) -> String {
    let Some(experience) = get_experience(conn, project_id, experience_id) else {
        return String::new();
    };
    if let Some(source_note_id) = experience
        .get("source_note_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        return source_note_id.to_string();
    }
    experience
        .get("evidence_note_ids")
        .and_then(Value::as_array)
        .and_then(|ids| ids.first())
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn list_dream_feedback_due(
    conn: &Connection,
    project_id: &str,
    limit: usize,
) -> rusqlite::Result<Vec<Value>> {
    if !table_exists(conn, "dream_candidates")? || !table_exists(conn, "notes")? {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT dc.id, dc.summary, dc.promoted_to, dc.promoted_at, n.metadata_json
         FROM dream_candidates dc
         LEFT JOIN notes n ON n.id = dc.promoted_to
         WHERE dc.project_id = ?1
           AND dc.verdict = 'promoted'
           AND dc.promoted_to IS NOT NULL
           AND dc.promoted_at IS NOT NULL
         ORDER BY dc.promoted_at DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(
        params![project_id, (limit.max(1) * 5).max(25) as i64],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                row.get::<_, Option<String>>(4)?,
            ))
        },
    )?;

    let mut due = Vec::new();
    for row in rows {
        let (candidate_id, summary, memory_id, promoted_at, metadata_json) = row?;
        let status = dream_feedback_status(metadata_json.as_deref());
        if status != "unreviewed" {
            continue;
        }
        due.push(json!({
            "candidate_id": candidate_id,
            "memory_id": memory_id,
            "summary": compact_text(&summary, 160),
            "promoted_at": promoted_at,
            "feedback_status": status,
            "suggested_tool": dream_feedback_instruction(&memory_id),
        }));
        if due.len() >= limit {
            break;
        }
    }
    Ok(due)
}

fn count_dream_feedback_due(conn: &Connection, project_id: &str) -> rusqlite::Result<i64> {
    if !table_exists(conn, "dream_candidates")? || !table_exists(conn, "notes")? {
        return Ok(0);
    }
    let mut stmt = conn.prepare(
        "SELECT n.metadata_json
         FROM dream_candidates dc
         LEFT JOIN notes n ON n.id = dc.promoted_to
         WHERE dc.project_id = ?1
           AND dc.verdict = 'promoted'
           AND dc.promoted_to IS NOT NULL
           AND dc.promoted_at IS NOT NULL",
    )?;
    let rows = stmt.query_map([project_id], |row| row.get::<_, Option<String>>(0))?;
    let mut count = 0;
    for row in rows {
        if dream_feedback_status(row?.as_deref()) == "unreviewed" {
            count += 1;
        }
    }
    Ok(count)
}

fn dream_feedback_status(raw_metadata: Option<&str>) -> &'static str {
    let metadata = parse_json_object(raw_metadata);
    if metadata
        .get("is_outdated")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return "outdated";
    }
    let failure_count = metadata
        .get("failure_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if metadata
        .get("is_corrected")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || failure_count > 0
    {
        return "corrected";
    }
    if metadata
        .get("success_count")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        > 0
    {
        return "confirmed";
    }
    "unreviewed"
}

fn dream_feedback_instruction(memory_id: &str) -> String {
    format!("report_outcome(memory_id=\"{memory_id}\", outcome=\"confirmed|corrected|outdated\")")
}

fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = ?1
         )",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|value| value != 0)
}

fn upsert_review(conn: &Connection, experience_id: &str, review: &Value) -> rusqlite::Result<()> {
    conn.execute(
        // datetime() OK here: write-side review timestamps only; this
        // statement does not compare or order timestamp text.
        "INSERT INTO experience_reviews
         (experience_id, lens, verdict, confidence, why, risk, recommended_action,
          evidence_refs_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'), datetime('now'))
         ON CONFLICT(experience_id, lens) DO UPDATE SET
            verdict = excluded.verdict,
            confidence = excluded.confidence,
            why = excluded.why,
            risk = excluded.risk,
            recommended_action = excluded.recommended_action,
            evidence_refs_json = excluded.evidence_refs_json,
            updated_at = datetime('now')",
        params![
            experience_id,
            review["lens"].as_str().unwrap_or("hermes"),
            review["verdict"].as_str().unwrap_or("monitor"),
            review["confidence"].as_f64().unwrap_or(0.65),
            review["why"].as_str(),
            review["risk"].as_str(),
            review["recommended_action"].as_str(),
            serde_json::to_string(review["evidence_refs"].as_array().unwrap_or(&Vec::new()))
                .unwrap_or_else(|_| "[]".to_string()),
        ],
    )?;
    Ok(())
}

fn deterministic_reviews(experience: &Value) -> Vec<Value> {
    let status = experience["status"].as_str().unwrap_or("candidate");
    let verdict = if status == "stable" {
        "adopt"
    } else {
        "monitor"
    };
    let confidence = if status == "stable" { 0.82 } else { 0.66 };
    let summary = experience["summary"].as_str().unwrap_or("");
    [
        ("hermes", "Reusable agent procedure surface"),
        ("harness", "QA contract should verify concrete behavior"),
        (
            "research",
            "Evidence should remain traceable to source notes",
        ),
        ("cold_eye", "Watch for overgeneralized lessons"),
    ]
    .into_iter()
    .map(|(lens, action)| {
        json!({
            "experience_id": experience["id"],
            "lens": lens,
            "verdict": verdict,
            "confidence": confidence,
            "why": format!("Rust deterministic review for: {summary}"),
            "risk": if status == "stable" { "low" } else { "needs more evidence" },
            "recommended_action": action,
            "evidence_refs": experience["evidence_refs"].as_array().cloned().unwrap_or_default(),
        })
    })
    .collect()
}

fn readiness_summary(experience: &Value, reviews: &[Value]) -> Value {
    let confidence = experience["confidence"].as_f64().unwrap_or(0.0);
    let evidence_count = experience["evidence_note_ids"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0)
        + experience["evidence_refs"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0);
    let review_count = reviews.len();
    let mut blockers = Vec::new();
    if evidence_count < 2 {
        blockers.push(json!("证据不足"));
    }
    if review_count < 2 {
        blockers.push(json!("评审不足"));
    }
    if confidence < 0.75 {
        blockers.push(json!("置信度不足"));
    }
    json!({
        "quality_score": confidence,
        "evidence_count": evidence_count,
        "review_count": review_count,
        "promotion_gate_passed": blockers.is_empty(),
        "promotion_blockers": blockers,
        "summary": format!("quality={confidence:.2} / reviews={review_count} / evidence={evidence_count}"),
    })
}

fn hermes_surface(stable: &[Value], review: &[Value], limit: usize, routing: &Value) -> Value {
    json!({
        "summary": format!("{} stable procedures / {} candidates", stable.len().min(limit), review.len().min(limit)),
        "stable_playbooks": surface_items(stable, limit, "procedure"),
        "procedure_candidates": surface_items(review, limit, "candidate"),
        "routing": routing,
    })
}

fn harness_surface(
    stable: &[Value],
    review: &[Value],
    degraded: &[Value],
    limit: usize,
    routing: &Value,
) -> Value {
    json!({
        "summary": format!("{} contracts / {} review candidates / {} degraded", stable.len().min(limit), review.len().min(limit), degraded.len().min(limit)),
        "qa_contracts": surface_items(stable, limit, "contract"),
        "review_queue": surface_items(review, limit, "review"),
        "known_risks": surface_items(degraded, limit, "risk"),
        "routing": routing,
    })
}

fn autoresearch_surface(
    stable: &[Value],
    degraded: &[Value],
    limit: usize,
    routing: &Value,
) -> Value {
    json!({
        "summary": format!("{} validated patterns / {} known failures", stable.len().min(limit), degraded.len().min(limit)),
        "validated_patterns": surface_items(stable, limit, "pattern"),
        "known_failures": surface_items(degraded, limit, "failure"),
        "guardrails": surface_items(stable, limit, "guardrail"),
        "routing": routing,
    })
}

fn cold_eye_surface(
    stable: &[Value],
    review: &[Value],
    degraded: &[Value],
    limit: usize,
    routing: &Value,
) -> Value {
    let mut items = Vec::new();
    items.extend_from_slice(degraded);
    items.extend_from_slice(review);
    items.extend_from_slice(stable);
    json!({
        "summary": format!("{} challenge prompts", items.len().min(limit)),
        "anti_patterns": surface_items(degraded, limit, "anti_pattern"),
        "challenge_prompts": surface_items(&items, limit, "challenge"),
        "routing": routing,
    })
}

fn surface_items(items: &[Value], limit: usize, item_type: &str) -> Vec<Value> {
    items
        .iter()
        .take(limit)
        .map(|item| {
            json!({
                "experience_id": item["id"],
                "type": item_type,
                "title": item["title"],
                "summary": item["summary"],
                "kind": item["kind"],
                "status": item["status"],
                "topic_key": item["topic_key"],
                "stored_topic_key": item["stored_topic_key"],
                "canonical_read_source": item["canonical_read_source"],
                "confidence": item["confidence"],
                "readiness": item["readiness"],
                "actions": item["actions"],
            })
        })
        .collect()
}

fn artifact_markdown(target: &str, surfaces: &Value) -> Option<(String, String)> {
    let key = match target {
        "hermes" => "hermes_skill_bundle",
        "harness" => "harness_qa_contract",
        "autoresearch" => "autoresearch_overlay",
        "cold_eye" => "cold_eye_brief",
        _ => return None,
    };
    let surface = if surfaces["target"] == "all" {
        surfaces["surfaces"].get(target)?
    } else {
        surfaces.get("surface")?
    };
    let markdown = format!(
        "# {target} experience artifact\n\nSummary: {}\n\n```json\n{}\n```\n",
        surface.get("summary").and_then(Value::as_str).unwrap_or(""),
        serde_json::to_string_pretty(surface).unwrap_or_else(|_| "{}".to_string())
    );
    Some((key.to_string(), markdown))
}

fn write_artifact_file(
    project_id: &str,
    target: &str,
    topic: Option<&str>,
    markdown: &str,
) -> std::io::Result<String> {
    let slug = topic.map(slugify).unwrap_or_else(|| "current".to_string());
    let dir = home_dir()
        .join(".memra")
        .join("projects")
        .join(project_id)
        .join("experience-artifacts")
        .join(target);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{slug}.md"));
    fs::write(&path, markdown)?;
    Ok(path.display().to_string())
}

struct ArtifactRunInsert<'a> {
    id: &'a str,
    batch_id: &'a str,
    project_id: &'a str,
    target: &'a str,
    feedback_token: &'a str,
    topic: Option<&'a str>,
    routing: &'a Value,
    artifact_path: Option<&'a str>,
    summary: &'a str,
}

fn insert_artifact_run(conn: &Connection, run: &ArtifactRunInsert<'_>) -> rusqlite::Result<()> {
    conn.execute(
        // datetime() OK here: write-side artifact-run timestamps only; this
        // statement does not compare or order timestamp text.
        "INSERT INTO experience_artifact_runs
         (id, batch_id, project_id, artifact_target, feedback_token, topic,
          routing_json, artifact_path, summary, source, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'rust_artifact_build',
                 datetime('now'), datetime('now'))",
        params![
            run.id,
            run.batch_id,
            run.project_id,
            run.target,
            run.feedback_token,
            run.topic,
            serde_json::to_string(run.routing).unwrap_or_else(|_| "{}".to_string()),
            run.artifact_path,
            run.summary,
        ],
    )?;
    Ok(())
}

fn get_artifact_run(
    conn: &Connection,
    feedback_token: Option<&str>,
    artifact_run_id: Option<&str>,
) -> Option<Value> {
    let (sql, value) = if let Some(token) = feedback_token.filter(|s| !s.is_empty()) {
        (
            "SELECT * FROM experience_artifact_runs WHERE feedback_token = ?1",
            token,
        )
    } else if let Some(id) = artifact_run_id.filter(|s| !s.is_empty()) {
        ("SELECT * FROM experience_artifact_runs WHERE id = ?1", id)
    } else {
        return None;
    };
    conn.query_row(sql, [value], |row| {
        let routing_json: Option<String> = row.get("routing_json")?;
        Ok(json!({
            "artifact_run_id": row.get::<_, String>("id")?,
            "batch_id": row.get::<_, Option<String>>("batch_id")?,
            "project_id": row.get::<_, String>("project_id")?,
            "artifact_target": row.get::<_, String>("artifact_target")?,
            "feedback_token": row.get::<_, String>("feedback_token")?,
            "topic": row.get::<_, Option<String>>("topic")?,
            "routing": parse_json_object(routing_json.as_deref()),
            "artifact_path": row.get::<_, Option<String>>("artifact_path")?,
            "summary": row.get::<_, Option<String>>("summary")?,
        }))
    })
    .optional()
    .ok()
    .flatten()
}

fn status_weight(status: &str) -> f64 {
    match status {
        "stable" => 0.35,
        "review_required" => 0.2,
        "candidate" => 0.1,
        "degraded" => -0.1,
        _ => 0.0,
    }
}

fn stable_experience_id(prefix: &str, raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches('_');
    if trimmed.is_empty() {
        format!("exp_{prefix}")
    } else {
        format!("exp_{prefix}_{trimmed}")
    }
}

fn compact_text(text: &str, max_chars: usize) -> String {
    let cleaned = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = cleaned.chars().count();
    if count <= max_chars {
        return cleaned;
    }
    cleaned
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>()
        + "..."
}

fn infer_experience_kind(layer: Option<&str>, category: Option<&str>, degraded: bool) -> String {
    if degraded {
        return "anti_pattern".to_string();
    }
    match (layer.unwrap_or_default(), category.unwrap_or_default()) {
        ("procedure_schema", _) | (_, "routine") => "procedure",
        (_, "decision") => "decision",
        (_, "bug") | (_, "lesson") => "anti_pattern",
        (_, "config") | (_, "architecture") => "contract",
        _ => "heuristic",
    }
    .to_string()
}

fn kind_label(kind: &str) -> &str {
    match kind {
        "procedure" => "流程经验",
        "heuristic" => "启发式",
        "anti_pattern" => "反模式",
        "contract" => "契约",
        "decision" => "决策",
        _ => kind,
    }
}

fn parse_json_object(raw: Option<&str>) -> Value {
    raw.and_then(|text| serde_json::from_str::<Value>(text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn parse_json_array(raw: Option<&str>) -> Value {
    raw.and_then(|text| serde_json::from_str::<Value>(text).ok())
        .filter(Value::is_array)
        .unwrap_or_else(|| json!([]))
}

fn evidence_refs(metadata: &Value, source_note_id: Option<&str>) -> Value {
    let mut refs: Vec<Value> = metadata
        .get("evidence_refs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(id) = source_note_id {
        refs.insert(0, json!(format!("note:{id}")));
    }
    Value::Array(refs)
}

fn normalize_topic(topic: &str) -> String {
    topic
        .trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn slugify(raw: &str) -> String {
    let slug: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = slug.trim_matches('-').replace("--", "-");
    if cleaned.is_empty() {
        "topic".to_string()
    } else {
        cleaned
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_substrate_seeds_and_reviews_experiences() -> rusqlite::Result<()> {
        let pool = DbPool::open_in_memory()?;
        pool.with_conn(|conn| {
            conn.execute(
                "INSERT INTO notes
                 (id, content, layer, category, project_id, is_active, confidence,
                  metadata_json, review_after, created_at, updated_at)
                 VALUES
                 ('proc-1', 'Use Rust refresh substrate for deterministic experience records.', 'procedure_schema', 'routine', 'alpha', 1, 0.91,
                  '{\"steps\":[\"one\",\"two\",\"three\"],\"activation_cues\":[\"refresh\"]}', NULL, '2026-05-01T00:00:00Z', '2026-05-01T00:00:00Z'),
                 ('review-1', 'This memory needs explicit review before becoming stable.', 'verified_fact', 'decision', 'alpha', 1, 0.72,
                  '{}', '2026-05-01T00:00:00Z', '2026-05-01T00:00:00Z', '2026-05-01T00:00:00Z'),
                 ('failed-1', 'This old memory failed in production and should be treated as degraded.', 'verified_fact', 'bug', 'alpha', 1, 0.55,
                  '{\"failure_count\":2}', NULL, '2026-05-01T00:00:00Z', '2026-05-01T00:00:00Z')",
                [],
            )?;
            Ok::<(), rusqlite::Error>(())
        })?;
        let store = ExperienceStore::with_db(pool, "alpha");

        let report = store.refresh_substrate(12);
        assert_eq!(report["seeded"], json!(1));
        assert_eq!(report["candidates"], json!(2));
        assert_eq!(report["activation_gate"]["activation_limit"], json!(24));

        let stable = store.search_experiences(
            "Rust refresh substrate",
            5,
            Some(&["stable".to_string()]),
            None,
        );
        assert_eq!(stable["count"], json!(1));
        assert_eq!(
            stable["results"][0]["origin"],
            json!("procedure_schema_backfill")
        );

        let degraded = store.search_experiences(
            "production failed",
            5,
            Some(&["degraded".to_string()]),
            None,
        );
        assert_eq!(degraded["count"], json!(1));
        assert_eq!(degraded["results"][0]["kind"], json!("anti_pattern"));
        Ok(())
    }

    #[test]
    fn experience_records_surface_canonical_topic_from_source_note() -> rusqlite::Result<()> {
        let store = ExperienceStore::with_db(DbPool::open_in_memory()?, "alpha");
        store.db.with_conn(|conn| {
            conn.execute_batch(
                "INSERT INTO notes
                 (id, content, layer, category, project_id, is_active, is_head,
                  evolution_state, root_id, topic_key, created_at, updated_at)
                 VALUES
                 ('old-note', 'old gatekeeper workflow', 'procedure_schema', 'routine',
                  'alpha', 0, 0, 'superseded', 'topic-root', 'old-topic',
                  '2026-05-01T00:00:00Z', '2026-05-01T00:00:00Z'),
                 ('head-note', 'new gatekeeper workflow', 'procedure_schema', 'routine',
                  'alpha', 1, 1, 'active', 'topic-root', 'gatekeeper',
                  '2026-05-01T00:00:01Z', '2026-05-01T00:00:01Z')",
            )?;
            conn.execute(
                "INSERT INTO experience_records
                 (id, title, summary, kind, status, origin, project_id, source_note_id,
                  topic_key, confidence, evidence_note_ids_json, metadata_json,
                  superseded_by, created_at, updated_at)
                 VALUES
                 ('exp-old', 'Legacy signing flow', 'Use the old signing flow',
                  'procedure', 'stable', 'manual', 'alpha', 'old-note',
                  'old-topic', 0.91, '[\"old-note\"]', '{}', NULL,
                  '2026-05-01T00:00:02Z', '2026-05-01T00:00:02Z')",
                [],
            )?;
            Ok::<(), rusqlite::Error>(())
        })?;

        let search = store.search_experiences("gatekeeper", 5, None, None);
        assert_eq!(search["count"], json!(1));
        assert_eq!(search["results"][0]["id"], json!("exp-old"));
        assert_eq!(search["results"][0]["topic_key"], json!("gatekeeper"));
        assert_eq!(search["results"][0]["stored_topic_key"], json!("old-topic"));
        assert_eq!(
            search["results"][0]["canonical_read_source"],
            json!("canonical")
        );

        let surfaces = store.get_surfaces("hermes", 5, None, false);
        let playbook = &surfaces["surface"]["stable_playbooks"][0];
        assert_eq!(playbook["experience_id"], json!("exp-old"));
        assert_eq!(playbook["topic_key"], json!("gatekeeper"));
        assert_eq!(playbook["stored_topic_key"], json!("old-topic"));
        assert_eq!(playbook["canonical_read_source"], json!("canonical"));
        Ok(())
    }
}
