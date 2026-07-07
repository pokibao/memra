//! Search engine: orchestrates FTS5 + vector + scoring + postprocessing.
//!
//! Ported from `backend/services/search.py` SearchService.search().

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use rusqlite::Connection;
use tracing::warn;

use crate::personal::{ProjectEntityFilter, allows_strengthening, project_entity_filters};
use crate::retrieval::canonical_reader::{
    CanonicalRead, CanonicalReadSource, CanonicalSeed, hydrate_head_by_canonical_or_topic,
    hydrate_heads_by_canonical_or_topic,
};
use crate::retrieval::postprocess::{
    add_temperature_labels, dedupe_results_by_topic_key, enforce_type_diversity,
    ensure_default_channel,
};
use crate::retrieval::recall::{
    CandidateFilters, build_fts_query, cjk_bigram_candidates, exact_substring_candidates,
    extract_vector, fts_candidates, vector_candidates_with_stats,
};
use crate::retrieval::scoring::{
    CURRENT_STATE_MARKERS_EN, CURRENT_STATE_MARKERS_ZH, HISTORY_MARKERS_EN, HISTORY_MARKERS_ZH,
    ScoreInput, ScoringMetadata, ScoringPayload, score_result,
};
use crate::storage::db::{
    Candidate, DbPool, NoteRow, SearchDiagnostics, SearchParams, SearchResult,
};

const ACTIVATION_RELATION_TYPES: &[&str] = &["supports", "refines", "overlaps", "co_activated"];
const DREAMING_EVIDENCE_RELATION_TYPE: &str = "dreaming_evidence";
const ACTIVATION_STRENGTH_THRESHOLD: f64 = 0.4;
const ACTIVATION_WEIGHT: f64 = 0.3;
const ACTIVATION_MAX_HOPS: usize = 3;

/// Non-head evolution states: rows where `mark_superseded` has written
/// `is_head = 0` (writer.rs mark_superseded reason-to-state mapping).
/// Keep this aligned with `backend/core/write_orchestrator.py` reason
/// mapping + `memory_lifecycle.CONTRADICTED_EVOLUTION_STATE` when adding
/// new non-head states (e.g. `deprecated`).
pub(crate) fn evolution_state_is_head(state: Option<&str>) -> bool {
    !matches!(state, Some("superseded") | Some("contradicted"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScoreNormalizationMode {
    None,
    Z,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RoomEntry {
    pub room: Option<String>,
    pub count: i64,
    pub last_used_at: Option<String>,
}

/// Safe UTF-8 truncation.
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

fn normalize_exact_match_text(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_literal_self_recall_match(query: &str, content: &str) -> bool {
    if query.trim().chars().count() < 24 {
        return false;
    }
    let query = normalize_exact_match_text(query);
    let content = normalize_exact_match_text(content);
    !query.is_empty() && content.contains(&query)
}

fn should_try_lexical_first(query: &str) -> bool {
    normalize_exact_match_text(query).split_whitespace().count() >= 2
}

fn lexical_candidates_are_sufficient(query: &str, candidates: &[Candidate], limit: usize) -> bool {
    if candidates.is_empty() {
        return false;
    }
    candidates.len() >= limit
        || candidates
            .iter()
            .any(|candidate| literal_terms_match(query, &candidate.note.content))
}

fn literal_terms_match(query: &str, content: &str) -> bool {
    let query = normalize_exact_match_text(query);
    if query.is_empty() {
        return false;
    }
    let content = normalize_exact_match_text(content);
    if content.contains(&query) {
        return true;
    }

    let terms = query.split_whitespace().collect::<Vec<_>>();
    terms.len() >= 2 && terms.iter().all(|term| content.contains(term))
}

fn text_contains_any_alias(text: &str, aliases: &[&str]) -> bool {
    let normalized = normalize_exact_match_text(text);
    aliases.iter().any(|alias| normalized.contains(alias))
}

fn project_entity_filter_for_query(query: &str) -> Option<ProjectEntityFilter> {
    project_entity_filters()
        .iter()
        .copied()
        .find(|filter| text_contains_any_alias(query, filter.aliases))
}

fn entity_residual_terms(query: &str, filter: ProjectEntityFilter) -> Vec<String> {
    let mut residual = normalize_exact_match_text(query);
    let mut aliases = filter
        .aliases
        .iter()
        .map(|alias| normalize_exact_match_text(alias))
        .collect::<Vec<_>>();
    aliases.sort_by_key(|alias| std::cmp::Reverse(alias.len()));
    for alias in aliases {
        residual = residual.replace(&alias, " ");
    }

    residual
        .split_whitespace()
        .filter(|term| {
            term.chars().count() > 1
                && !matches!(
                    *term,
                    "erp" | "project" | "app" | "system" | "项目" | "系统" | "平台"
                )
        })
        .map(str::to_string)
        .collect()
}

fn note_matches_project_entity_filter(note: &NoteRow, filter: ProjectEntityFilter) -> bool {
    if text_contains_any_alias(&note.content, filter.aliases) {
        return true;
    }
    note.room.as_deref().is_some_and(|room| {
        filter
            .rooms
            .iter()
            .any(|expected| normalize_exact_match_text(room) == *expected)
            || text_contains_any_alias(room, filter.aliases)
    })
}

fn apply_project_entity_strong_filter(query: &str, candidates: Vec<Candidate>) -> Vec<Candidate> {
    let Some(filter) = project_entity_filter_for_query(query) else {
        return candidates;
    };
    if !candidates
        .iter()
        .any(|candidate| note_matches_project_entity_filter(&candidate.note, filter))
    {
        return candidates;
    }
    candidates
        .into_iter()
        .filter(|candidate| note_matches_project_entity_filter(&candidate.note, filter))
        .collect()
}

fn escape_like_pattern(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn search_result_from_note(note: NoteRow, score: f64, channel: &str) -> SearchResult {
    let metadata = note
        .metadata_json
        .as_deref()
        .and_then(|value| serde_json::from_str(value).ok());
    SearchResult {
        id: note.id,
        content: note.content,
        layer: note.layer,
        category: note.category,
        score,
        created_at: note.created_at,
        topic_key: note.topic_key,
        root_id: note.root_id,
        version: note.version,
        channel: Some(channel.to_string()),
        temperature: None,
        is_head: note.is_head && evolution_state_is_head(note.evolution_state.as_deref()),
        metadata,
        recall_count: note.recall_count,
        last_recalled_at: None,
    }
}

fn entity_namespace_prepass_results(
    conn: &Connection,
    params: &SearchParams,
    query: &str,
    limit: usize,
) -> Vec<SearchResult> {
    let Some(filter) = project_entity_filter_for_query(query) else {
        return Vec::new();
    };
    if limit == 0 {
        return Vec::new();
    }

    let mut entity_parts = Vec::new();
    let mut sql_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    for alias in filter.aliases {
        entity_parts.push("LOWER(n.content) LIKE ? ESCAPE '\\'".to_string());
        sql_params.push(Box::new(format!(
            "%{}%",
            escape_like_pattern(&alias.to_ascii_lowercase())
        )));
    }
    for room in filter.rooms {
        entity_parts.push("LOWER(COALESCE(n.room, '')) = ?".to_string());
        sql_params.push(Box::new(room.to_ascii_lowercase()));
    }
    let mut where_parts = vec![format!("({})", entity_parts.join(" OR "))];
    for term in entity_residual_terms(query, filter) {
        let pattern = format!("%{}%", escape_like_pattern(&term));
        where_parts.push(
            "(LOWER(n.content) LIKE ? ESCAPE '\\' OR LOWER(COALESCE(n.room, '')) LIKE ? ESCAPE '\\')"
                .to_string(),
        );
        sql_params.push(Box::new(pattern.clone()));
        sql_params.push(Box::new(pattern));
    }

    if params.only_active {
        where_parts.push("n.is_active = 1".to_string());
    }
    if !params.include_constitution {
        where_parts.push("n.layer != 'identity_schema'".to_string());
    }
    if !params.cross_project {
        if let Some(project_id) = params.project_id.as_deref() {
            where_parts.push("n.project_id = ?".to_string());
            sql_params.push(Box::new(project_id.to_string()));
        }
    }

    let sql = format!(
        "SELECT n.*
         FROM notes n
         WHERE {}
         ORDER BY
           CASE WHEN n.layer = 'verified_fact' THEN 0 ELSE 1 END,
           CASE WHEN n.room IS NOT NULL THEN 0 ELSE 1 END,
           LENGTH(n.content) ASC,
           COALESCE(n.updated_at, n.created_at) DESC,
           n.id ASC
         LIMIT ?",
        where_parts.join(" AND ")
    );
    sql_params.push(Box::new(limit as i64));
    let param_refs = sql_params
        .iter()
        .map(|param| param.as_ref())
        .collect::<Vec<_>>();

    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(error) => {
            warn!("entity namespace prepass prepare failed: {error}");
            return Vec::new();
        }
    };
    let rows = match stmt.query_map(rusqlite::params_from_iter(param_refs.iter()), |row| {
        NoteRow::from_row(row)
    }) {
        Ok(rows) => rows,
        Err(error) => {
            warn!("entity namespace prepass query failed: {error}");
            return Vec::new();
        }
    };

    rows.filter_map(|row| match row {
        Ok(note) => Some(search_result_from_note(note, 0.9997, "entity")),
        Err(error) => {
            warn!("Skipping entity namespace prepass row: {error}");
            None
        }
    })
    .filter(|result| params.min_score <= 0.0 || result.score >= params.min_score)
    .collect()
}

fn direct_fact_prefix_results(
    conn: &Connection,
    params: &SearchParams,
    query: &str,
    limit: usize,
) -> Vec<SearchResult> {
    let query = query.trim();
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }

    let escaped = escape_like_pattern(query);
    let pattern = format!("[FACT] {escaped}%");
    let mut where_parts = vec!["n.content LIKE ? ESCAPE '\\'".to_string()];
    let mut sql_params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(pattern)];

    if params.only_active {
        where_parts.push("n.is_active = 1".to_string());
    }
    if !params.include_constitution {
        where_parts.push("n.layer != 'identity_schema'".to_string());
    }
    if !params.cross_project {
        if let Some(project_id) = params.project_id.as_deref() {
            where_parts.push("n.project_id = ?".to_string());
            sql_params.push(Box::new(project_id.to_string()));
        }
    }

    let sql = format!(
        "SELECT n.*
         FROM notes n
         WHERE {}
         ORDER BY
           CASE WHEN n.layer = 'verified_fact' THEN 0 ELSE 1 END,
           LENGTH(n.content) ASC,
           COALESCE(n.updated_at, n.created_at) DESC,
           n.id ASC
         LIMIT ?",
        where_parts.join(" AND ")
    );
    sql_params.push(Box::new(limit as i64));
    let param_refs = sql_params
        .iter()
        .map(|param| param.as_ref())
        .collect::<Vec<_>>();

    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(error) => {
            warn!("direct fact prefix prepare failed: {error}");
            return Vec::new();
        }
    };
    let rows = match stmt.query_map(rusqlite::params_from_iter(param_refs.iter()), |row| {
        NoteRow::from_row(row)
    }) {
        Ok(rows) => rows,
        Err(error) => {
            warn!("direct fact prefix query failed: {error}");
            return Vec::new();
        }
    };

    rows.filter_map(|row| match row {
        Ok(note) => Some(search_result_from_note(note, 0.9996, "lexical")),
        Err(error) => {
            warn!("Skipping direct fact prefix row: {error}");
            None
        }
    })
    .filter(|result| params.min_score <= 0.0 || result.score >= params.min_score)
    .collect()
}

/// RRF constants (matching Python).
const RRF_K: usize = 60;
const RRF_BLEND_WEIGHT: f64 = 0.05;
const LEXICAL_HARD_MATCH_BONUS: f64 = 0.10;
const MISSING_KEYWORD_PENALTY: f64 = 0.05;
const Z_SCORE_UNIT_SCALE: f64 = 0.15;

/// Primary layers for layer-aware recall.
const PRIMARY_LAYERS: &[&str] = &[
    "identity_schema",
    "procedure_schema",
    "verified_fact",
    "event_log",
];

fn primary_layers(include_constitution: bool) -> impl Iterator<Item = &'static str> {
    PRIMARY_LAYERS
        .iter()
        .copied()
        .filter(move |layer| include_constitution || *layer != "identity_schema")
}

fn score_normalization_mode_from_env() -> ScoreNormalizationMode {
    match std::env::var("MA_SCORE_NORMALIZE") {
        Ok(value) if value.eq_ignore_ascii_case("z") => ScoreNormalizationMode::Z,
        Ok(value) if value.eq_ignore_ascii_case("none") || value.trim().is_empty() => {
            ScoreNormalizationMode::None
        }
        Ok(value) => {
            warn!("unsupported MA_SCORE_NORMALIZE value '{value}', using none");
            ScoreNormalizationMode::None
        }
        Err(_) => ScoreNormalizationMode::None,
    }
}

fn normalize_result_scores(results: &mut [SearchResult], mode: ScoreNormalizationMode) {
    if mode == ScoreNormalizationMode::None || results.len() < 2 {
        return;
    }

    let mean = results.iter().map(|result| result.score).sum::<f64>() / results.len() as f64;
    let variance = results
        .iter()
        .map(|result| {
            let delta = result.score - mean;
            delta * delta
        })
        .sum::<f64>()
        / results.len() as f64;
    let std_dev = variance.sqrt();
    if std_dev <= f64::EPSILON {
        return;
    }

    for result in results {
        let z_score = (result.score - mean) / std_dev;
        result.score = (0.5 + z_score * Z_SCORE_UNIT_SCALE).clamp(0.0, 1.0);
    }
}

fn apply_lexical_hard_match_bonus(query: &str, content: &str, score: f64) -> f64 {
    if literal_terms_match(query, content) {
        (score + LEXICAL_HARD_MATCH_BONUS).min(1.0)
    } else {
        score
    }
}

fn apply_missing_keyword_penalty(query: &str, content: &str, score: f64) -> f64 {
    let normalized_query = normalize_exact_match_text(query);
    let terms = normalized_query
        .split_whitespace()
        .filter(|term| term.chars().count() > 1)
        .collect::<Vec<_>>();
    if terms.len() < 2 {
        return score;
    }
    let normalized_content = normalize_exact_match_text(content);
    if terms.iter().all(|term| normalized_content.contains(term)) {
        score
    } else {
        (score - MISSING_KEYWORD_PENALTY).max(0.0)
    }
}

/// The search engine: holds a DB pool and provides the search pipeline.
pub struct SearchEngine {
    db: DbPool,
}

impl SearchEngine {
    /// Borrow the underlying pool. Marked `#[doc(hidden)]` because production
    /// code should go through the engine's public methods; integration tests
    /// use this to seed fixtures into an in-memory DB without round-tripping
    /// through the full write pipeline.
    #[doc(hidden)]
    pub fn pool(&self) -> &DbPool {
        &self.db
    }

    pub fn new(db: DbPool) -> Self {
        Self { db }
    }

    /// Main search pipeline.
    ///
    /// Orchestrates: query preprocessing → candidate retrieval → scoring →
    /// postprocessing → result assembly.
    pub fn search(&self, params: &SearchParams) -> Vec<SearchResult> {
        self.search_with_diagnostics(params).0
    }

    /// Same as [`Self::search`] but also returns a [`SearchDiagnostics`] so
    /// MCP callers can surface observability fields (e.g. dim-mismatch skip
    /// counts from the legacy 384-dim / bge-m3 1024-dim transition) to the
    /// user. REL-02.
    pub fn search_with_diagnostics(
        &self,
        params: &SearchParams,
    ) -> (Vec<SearchResult>, SearchDiagnostics) {
        self.db.with_conn(|conn| {
            let mut diagnostics = SearchDiagnostics::default();
            let results = self.search_inner(conn, params, &mut diagnostics);
            (results, diagnostics)
        })
    }

    pub fn list_rooms(&self, project_id: Option<&str>, include_null: bool) -> Vec<RoomEntry> {
        self.db.with_conn(|conn| {
            let mut where_parts = vec!["is_active = 1".to_string()];
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            if let Some(pid) = project_id {
                where_parts.push("project_id = ?".to_string());
                params.push(Box::new(pid.to_string()));
            }
            if !include_null {
                where_parts.push("room IS NOT NULL".to_string());
            }

            let where_sql = where_parts.join(" AND ");
            let sql = format!(
                "SELECT room, COUNT(*) AS count, MAX(COALESCE(updated_at, created_at)) AS last_used_at
                 FROM notes
                 WHERE {where_sql}
                 GROUP BY room
                 ORDER BY count DESC, COALESCE(room, '') ASC"
            );
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();

            let mut stmt = match conn.prepare(&sql) {
                Ok(stmt) => stmt,
                Err(error) => {
                    warn!("list_rooms query prepare failed: {error}");
                    return vec![];
                }
            };
            stmt.query_map(rusqlite::params_from_iter(param_refs.iter()), |row| {
                Ok(RoomEntry {
                    room: row.get(0)?,
                    count: row.get(1)?,
                    last_used_at: row.get(2)?,
                })
            })
            .map(|rows| rows.filter_map(Result::ok).collect())
            .unwrap_or_default()
        })
    }

    fn search_inner(
        &self,
        conn: &Connection,
        params: &SearchParams,
        diagnostics: &mut SearchDiagnostics,
    ) -> Vec<SearchResult> {
        let query = params.query.trim();
        let effective_mode = params.search_mode.as_deref().unwrap_or("auto");
        let limit = if params.limit == 0 { 5 } else { params.limit };

        // Strip temporal markers from recall query (keep original for scoring)
        let recall_query = strip_temporal_markers(query);
        let recall_query = if recall_query.is_empty() {
            query
        } else {
            &recall_query
        };

        let candidate_limit = (limit * 8).max(50);
        let mut query_vector: Option<Vec<f32>> = None;

        if effective_mode == "auto"
            && params.layer.is_none()
            && params.category.is_none()
            && should_try_lexical_first(query)
        {
            let direct_results = direct_fact_prefix_results(conn, params, query, limit);
            if !direct_results.is_empty() {
                return direct_results;
            }
        }
        if effective_mode == "auto" && params.layer.is_none() && params.category.is_none() {
            let entity_results = entity_namespace_prepass_results(conn, params, query, limit);
            if !entity_results.is_empty() {
                return entity_results;
            }
        }

        // Collect candidates
        let candidates = if !query.is_empty() {
            if effective_mode == "auto" && should_try_lexical_first(recall_query) {
                let lexical_candidates = self.collect_candidates(
                    conn,
                    query,
                    recall_query,
                    None,
                    params,
                    candidate_limit,
                    limit,
                    diagnostics,
                );
                if lexical_candidates_are_sufficient(recall_query, &lexical_candidates, limit) {
                    lexical_candidates
                } else {
                    query_vector = crate::embedding::embed_text(recall_query);
                    self.collect_candidates(
                        conn,
                        query,
                        recall_query,
                        query_vector.as_deref(),
                        params,
                        candidate_limit,
                        limit,
                        diagnostics,
                    )
                }
            } else {
                if effective_mode != "lexical" {
                    query_vector = crate::embedding::embed_text(recall_query);
                }
                self.collect_candidates(
                    conn,
                    query,
                    recall_query,
                    query_vector.as_deref(),
                    params,
                    candidate_limit,
                    limit,
                    diagnostics,
                )
            }
        } else {
            // No query: fetch recent rows
            self.fetch_recent_rows(conn, params, candidate_limit)
        };
        let candidates = apply_project_entity_strong_filter(query, candidates);

        if candidates.is_empty() {
            return vec![];
        }

        // RRF fusion (opt-in via search_mode="rrf")
        let rrf_scores = if effective_mode == "rrf" {
            rrf_fusion(&candidates, query_vector.as_deref())
        } else {
            std::collections::HashMap::new()
        };

        // Score each candidate
        let mut results: Vec<SearchResult> = Vec::new();
        let mut seen_ids: HashSet<String> = HashSet::new();
        let recall_timestamps =
            latest_recall_timestamps(conn, &candidates).unwrap_or_else(|error| {
                warn!("latest_recall_timestamps failed: {error}");
                HashMap::new()
            });

        let boost_cats: Option<HashSet<String>> = params
            .boost_categories
            .as_ref()
            .map(|v| v.iter().cloned().collect());

        for cand in &candidates {
            if seen_ids.contains(&cand.note.id) {
                continue;
            }

            // Build ScoringPayload from NoteRow
            let payload = ScoringPayload {
                content: cand.note.content.clone(),
                layer: cand.note.layer.clone(),
                confidence: cand.note.confidence,
                created_at: cand.note.created_at.clone(),
                evolution_state: cand.note.evolution_state.clone(),
                category: cand.note.category.clone(),
                source: None,
            };

            // Parse ScoringMetadata from metadata_json
            let scoring_meta: ScoringMetadata = cand
                .note
                .metadata_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_default();

            // Extract stored vector as f32 for scoring
            let stored_vec_f32: Option<Vec<f32>> = extract_vector(&cand.note);
            let last_recalled_at = recall_timestamps
                .get(&cand.note.id)
                .map(std::string::String::as_str);

            let mut score = score_result(&ScoreInput {
                query,
                query_vector: query_vector.as_deref(),
                payload: &payload,
                stored_vector: stored_vec_f32.as_deref(),
                bm25_score: cand.bm25_score,
                metadata: &scoring_meta,
                boost_categories: boost_cats.as_ref(),
                recall_count: cand.note.recall_count,
                last_recalled_at,
            });

            // Multiplicative RRF boost
            if let Some(&rrf_score) = rrf_scores.get(&cand.note.id) {
                score *= 1.0 + RRF_BLEND_WEIGHT * rrf_score;
            }

            // Parse metadata
            let metadata: Option<serde_json::Value> = cand
                .note
                .metadata_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());

            results.push(SearchResult {
                id: cand.note.id.clone(),
                content: cand.note.content.clone(),
                layer: cand.note.layer.clone(),
                category: cand.note.category.clone(),
                score,
                created_at: cand.note.created_at.clone(),
                topic_key: cand.note.topic_key.clone(),
                root_id: cand.note.root_id.clone(),
                version: cand.note.version,
                channel: is_literal_self_recall_match(query, &cand.note.content)
                    .then(|| "exact".to_string()),
                temperature: None,
                is_head: cand.note.is_head
                    && evolution_state_is_head(cand.note.evolution_state.as_deref()),
                metadata,
                recall_count: cand.note.recall_count,
                last_recalled_at: last_recalled_at.map(str::to_string),
            });
            seen_ids.insert(cand.note.id.clone());
        }

        follow_results_to_canonical_heads(conn, &mut results);

        // Sort by score descending
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        dedupe_results_by_id(&mut results);
        normalize_result_scores(&mut results, score_normalization_mode_from_env());
        for result in &mut results {
            result.score = apply_missing_keyword_penalty(query, &result.content, result.score);
            result.score = apply_lexical_hard_match_bonus(query, &result.content, result.score);
        }
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Post-process
        let seen_topics = dedupe_results_by_topic_key(&mut results);
        results.truncate(limit);
        enforce_type_diversity(&mut results, 0.6);
        results = self.spread_activation(conn, params, results, limit, &seen_topics, diagnostics);

        let default_channel = if effective_mode == "lexical" || query_vector.is_none() {
            "lexical"
        } else {
            "semantic"
        };
        ensure_default_channel(&mut results, default_channel);
        add_temperature_labels(&mut results);

        // Apply min_score filter
        if params.min_score > 0.0 {
            results.retain(|r| r.score >= params.min_score);
        }

        results
    }

    fn spread_activation(
        &self,
        conn: &Connection,
        params: &SearchParams,
        mut results: Vec<SearchResult>,
        limit: usize,
        seen_topics: &HashSet<String>,
        diagnostics: &mut SearchDiagnostics,
    ) -> Vec<SearchResult> {
        if results.is_empty() || results.len() >= limit || limit == 0 {
            return results;
        }
        let started_at = Instant::now();

        let has_note_relations = table_exists(conn, "note_relations");
        let has_dream_candidates = table_exists(conn, "dream_candidates");
        if !has_note_relations && !has_dream_candidates {
            diagnostics.activation_latency_ms = started_at.elapsed().as_millis();
            return results;
        }

        let direct_ids: HashSet<String> = results.iter().map(|r| r.id.clone()).collect();
        let mut best_scores: HashMap<String, f64> =
            results.iter().map(|r| (r.id.clone(), r.score)).collect();
        let mut seen_topics = seen_topics.clone();
        let mut activated: HashMap<String, SearchResult> = HashMap::new();
        let mut frontier: Vec<ActivationSeed> = results
            .iter()
            .map(|result| ActivationSeed {
                id: result.id.clone(),
                root_id: result.id.clone(),
                score: result.score,
                path: vec![result.id.clone()],
            })
            .collect();

        for depth in 1..=ACTIVATION_MAX_HOPS {
            if frontier.is_empty() {
                break;
            }
            let mut next_frontier = Vec::new();
            for source in &frontier {
                if !activation_source_is_trusted(conn, &source.id) {
                    continue;
                }
                let mut neighbors = Vec::new();
                if has_note_relations {
                    neighbors.extend(activation_neighbors_for_source(conn, &source.id));
                }
                if has_dream_candidates {
                    neighbors.extend(dreaming_activation_neighbors_for_source(
                        conn,
                        &source.id,
                        params.project_id.as_deref(),
                        params.cross_project,
                    ));
                }
                let hydrated_neighbors = hydrate_activation_neighbors(conn, &neighbors);
                for neighbor in neighbors {
                    let original_note = neighbor.note;
                    let original_id = original_note.id.clone();
                    if direct_ids.contains(&original_id)
                        || source.path.iter().any(|id| id == &original_id)
                    {
                        continue;
                    }

                    let read = hydrated_neighbors.get(&original_id);
                    let read_source = read
                        .map(|read| read.source)
                        .unwrap_or(CanonicalReadSource::Miss);
                    let note = read
                        .and_then(|read| read.note.clone())
                        .unwrap_or(original_note);

                    if direct_ids.contains(&note.id)
                        || source.path.iter().any(|id| id == &note.id)
                        || !activation_note_passes_filters(conn, &note, params)
                    {
                        continue;
                    }
                    if let Some(topic_key) = note.topic_key.as_ref() {
                        if seen_topics.contains(topic_key) {
                            continue;
                        }
                    }

                    let connected_score = source.score * neighbor.strength * ACTIVATION_WEIGHT;
                    if connected_score <= 0.0 {
                        continue;
                    }
                    if best_scores
                        .get(&note.id)
                        .is_some_and(|existing| connected_score <= *existing)
                    {
                        continue;
                    }

                    let mut activation_path = source.path.clone();
                    activation_path.push(note.id.clone());

                    let should_replace = activated
                        .get(&note.id)
                        .map(|existing| connected_score > existing.score)
                        .unwrap_or(true);
                    if !should_replace {
                        continue;
                    }

                    let note_id = note.id.clone();
                    let mut metadata = note
                        .metadata_json
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                        .filter(|v| v.is_object())
                        .unwrap_or_else(|| serde_json::json!({}));
                    if let Some(obj) = metadata.as_object_mut() {
                        obj.insert("is_association".to_string(), serde_json::json!(true));
                        obj.insert(
                            "associated_from".to_string(),
                            serde_json::json!(neighbor.source_id),
                        );
                        obj.insert(
                            "association_root".to_string(),
                            serde_json::json!(source.root_id),
                        );
                        obj.insert("activation_depth".to_string(), serde_json::json!(depth));
                        obj.insert(
                            "activation_path".to_string(),
                            serde_json::json!(activation_path),
                        );
                        obj.insert(
                            "relation_type".to_string(),
                            serde_json::json!(neighbor.relation_type),
                        );
                        obj.insert(
                            "relation_strength".to_string(),
                            serde_json::json!(neighbor.strength),
                        );
                        obj.insert(
                            "canonical_read_source".to_string(),
                            serde_json::json!(read_source.as_str()),
                        );
                        if original_id != note_id {
                            obj.insert("evolved_from".to_string(), serde_json::json!(original_id));
                        }
                        if let Some(candidate_id) = neighbor.dream_candidate_id.as_deref() {
                            obj.insert(
                                "dream_candidate_id".to_string(),
                                serde_json::json!(candidate_id),
                            );
                        }
                    }

                    activated.insert(
                        note_id.clone(),
                        SearchResult {
                            id: note.id,
                            content: note.content,
                            layer: note.layer,
                            category: note.category,
                            score: connected_score,
                            created_at: note.created_at,
                            topic_key: note.topic_key,
                            root_id: note.root_id,
                            version: note.version,
                            channel: Some("association".to_string()),
                            temperature: None,
                            is_head: note.is_head
                                && evolution_state_is_head(note.evolution_state.as_deref()),
                            metadata: Some(metadata),
                            recall_count: note.recall_count,
                            last_recalled_at: None,
                        },
                    );
                    best_scores.insert(note_id.clone(), connected_score);
                    next_frontier.push(ActivationSeed {
                        id: note_id,
                        root_id: source.root_id.clone(),
                        score: connected_score,
                        path: activation_path,
                    });
                }
            }
            frontier = next_frontier;
        }

        if activated.is_empty() {
            diagnostics.activation_latency_ms = started_at.elapsed().as_millis();
            return results;
        }

        let remaining = limit.saturating_sub(results.len());
        let mut activated: Vec<SearchResult> = activated.into_values().collect();
        activated.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        for result in activated.into_iter().take(remaining) {
            if let Some(trace) = activation_trace_from_metadata(result.metadata.as_ref()) {
                diagnostics.activation_result_count += 1;
                diagnostics.activation_max_depth =
                    diagnostics.activation_max_depth.max(trace.depth);
                *diagnostics
                    .activation_relation_type_counts
                    .entry(trace.relation_type)
                    .or_insert(0) += 1;
                if trace.dream_candidate_id.is_some() {
                    diagnostics.activation_dream_evidence_count += 1;
                }
            }
            if let Some(topic_key) = result.topic_key.as_ref() {
                seen_topics.insert(topic_key.clone());
            }
            results.push(result);
        }
        diagnostics.activation_latency_ms = started_at.elapsed().as_millis();

        results
    }

    /// Collect candidates from FTS5 + vector + CJK bigram channels.
    #[allow(clippy::too_many_arguments)]
    fn collect_candidates(
        &self,
        conn: &Connection,
        exact_query: &str,
        recall_query: &str,
        query_vector: Option<&[f32]>,
        params: &SearchParams,
        candidate_limit: usize,
        limit: usize,
        diagnostics: &mut SearchDiagnostics,
    ) -> Vec<Candidate> {
        let fts_text = build_fts_query(recall_query);
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut seen_ids: HashSet<String> = HashSet::new();
        // Shared across layer-scoped + global vector passes so a legacy-dim
        // row is only counted once per search (not once per pass). See
        // recall.rs::vector_candidates_with_stats dedup comment.
        let mut dim_mismatch_seen: HashSet<String> = HashSet::new();

        let layer_str = params.layer.as_deref();
        let effective_project = if params.cross_project {
            None
        } else {
            params.project_id.as_deref()
        };
        let base_filters = CandidateFilters {
            layer: layer_str,
            category: params.category.as_deref(),
            only_active: params.only_active,
            agent_id: params.agent_id.as_deref(),
            project_id: effective_project,
            cross_project: params.cross_project,
            start_time: params.start_time.as_deref(),
            end_time: params.end_time.as_deref(),
            include_expired: params.include_expired,
            room: params.room.as_deref(),
            include_constitution: params.include_constitution,
        };

        if layer_str.is_none() {
            let exact_cands =
                exact_substring_candidates(conn, exact_query, base_filters, &seen_ids, 12);
            for c in exact_cands {
                if seen_ids.insert(c.note.id.clone()) {
                    candidates.push(c);
                }
            }

            // Layer-aware recall: fetch top candidates per layer first
            for layer_name in primary_layers(params.include_constitution) {
                let layer_cands = fts_candidates(
                    conn,
                    &fts_text,
                    recall_query,
                    base_filters.with_layer(Some(layer_name)),
                    10,
                );
                for c in layer_cands {
                    if seen_ids.insert(c.note.id.clone()) {
                        candidates.push(c);
                    }
                }
            }

            // Global FTS fallback
            let global_cands = fts_candidates(
                conn,
                &fts_text,
                recall_query,
                base_filters.with_layer(None),
                candidate_limit,
            );
            for c in global_cands {
                if seen_ids.insert(c.note.id.clone()) {
                    candidates.push(c);
                }
            }

            // Layer-aware vector recall
            if let Some(qv) = query_vector {
                for layer_name in primary_layers(params.include_constitution) {
                    let layer_vec = vector_candidates_with_stats(
                        conn,
                        qv,
                        base_filters.with_layer(Some(layer_name)),
                        8,
                        &seen_ids,
                        &mut dim_mismatch_seen,
                        &mut diagnostics.dim_mismatch_skipped,
                    );
                    for c in layer_vec {
                        if seen_ids.insert(c.note.id.clone()) {
                            candidates.push(c);
                        }
                    }
                }

                // Global vector fallback
                let global_vec = vector_candidates_with_stats(
                    conn,
                    qv,
                    base_filters.with_layer(None),
                    (limit * 4).max(20),
                    &seen_ids,
                    &mut dim_mismatch_seen,
                    &mut diagnostics.dim_mismatch_skipped,
                );
                for c in global_vec {
                    if seen_ids.insert(c.note.id.clone()) {
                        candidates.push(c);
                    }
                }
            }
        } else {
            let exact_cands =
                exact_substring_candidates(conn, exact_query, base_filters, &seen_ids, 12);
            for c in exact_cands {
                if seen_ids.insert(c.note.id.clone()) {
                    candidates.push(c);
                }
            }

            // Single-layer search
            let fts_cands =
                fts_candidates(conn, &fts_text, recall_query, base_filters, candidate_limit);
            for c in fts_cands {
                if seen_ids.insert(c.note.id.clone()) {
                    candidates.push(c);
                }
            }

            if let Some(qv) = query_vector {
                let vec_cands = vector_candidates_with_stats(
                    conn,
                    qv,
                    base_filters,
                    (limit * 4).max(20),
                    &seen_ids,
                    &mut dim_mismatch_seen,
                    &mut diagnostics.dim_mismatch_skipped,
                );
                candidates.extend(vec_cands);
            }
        }

        // CJK bigram LIKE fallback
        let cjk_chars: Vec<char> = recall_query
            .chars()
            .filter(|c| ('\u{4e00}'..='\u{9fff}').contains(c))
            .collect();
        if cjk_chars.len() >= 2 && candidates.is_empty() {
            let bigram_cands = cjk_bigram_candidates(conn, recall_query, base_filters, &seen_ids);
            for c in bigram_cands {
                if seen_ids.insert(c.note.id.clone()) {
                    candidates.push(c);
                }
            }
        }

        // If still empty, fetch recent rows as fallback
        if candidates.is_empty() {
            return self.fetch_recent_rows(conn, params, candidate_limit);
        }

        candidates
    }

    /// Fetch recent rows when no query or no candidates found.
    fn fetch_recent_rows(
        &self,
        conn: &Connection,
        params: &SearchParams,
        limit: usize,
    ) -> Vec<Candidate> {
        let mut where_parts: Vec<String> = Vec::new();
        let mut sql_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if params.only_active {
            where_parts.push("n.is_active = 1".to_string());
        }
        if let Some(ref l) = params.layer {
            where_parts.push("n.layer = ?".to_string());
            sql_params.push(Box::new(l.clone()));
        }
        if let Some(ref cat) = params.category {
            where_parts.push("n.category = ?".to_string());
            sql_params.push(Box::new(cat.clone()));
        }
        if !params.cross_project {
            if let Some(ref pid) = params.project_id {
                where_parts.push("n.project_id = ?".to_string());
                sql_params.push(Box::new(pid.clone()));
            }
        }
        // Only exclude identity rows when the caller did not explicitly ask for them.
        // Without this guard, `layer=identity_schema` + `include_constitution=false` (the
        // post-PR-#48 default) collapses to a self-contradictory WHERE clause and returns 0 rows.
        let explicit_identity = params.layer.as_deref() == Some("identity_schema");
        if !params.include_constitution && !explicit_identity {
            where_parts.push("n.layer != 'identity_schema'".to_string());
        }

        let where_sql = if where_parts.is_empty() {
            "1=1".to_string()
        } else {
            where_parts.join(" AND ")
        };

        let sql = format!(
            "SELECT n.* FROM notes n WHERE {where_sql}
             ORDER BY COALESCE(n.updated_at, n.created_at) DESC
             LIMIT ?"
        );
        sql_params.push(Box::new(limit as i64));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            sql_params.iter().map(|p| p.as_ref()).collect();

        match conn.prepare(&sql) {
            Ok(mut stmt) => stmt
                .query_map(rusqlite::params_from_iter(param_refs.iter()), |row| {
                    crate::storage::db::NoteRow::from_row(row)
                })
                .map(|rows| {
                    rows.filter_map(|r| match r {
                        Ok(note) => Some(Candidate {
                            note,
                            bm25_score: None,
                        }),
                        Err(e) => {
                            warn!("Skipping recent row: {e}");
                            None
                        }
                    })
                    .collect()
                })
                .unwrap_or_default(),
            Err(e) => {
                warn!("fetch_recent_rows failed: {e}");
                vec![]
            }
        }
    }

    /// Search for checkpoints by query and/or status.
    pub fn search_checkpoints(
        &self,
        query: Option<&str>,
        task_id: Option<&str>,
        task_status: Option<&str>,
        limit: usize,
    ) -> Vec<SearchResult> {
        self.search_checkpoints_for_project(query, task_id, task_status, None, limit)
    }

    pub fn search_checkpoints_for_project(
        &self,
        query: Option<&str>,
        task_id: Option<&str>,
        task_status: Option<&str>,
        project_id: Option<&str>,
        limit: usize,
    ) -> Vec<SearchResult> {
        self.db.with_conn(|conn| {
            self.search_checkpoints_inner(conn, query, task_id, task_status, project_id, limit)
        })
    }

    fn search_checkpoints_inner(
        &self,
        conn: &Connection,
        query: Option<&str>,
        task_id: Option<&str>,
        task_status: Option<&str>,
        project_id: Option<&str>,
        limit: usize,
    ) -> Vec<SearchResult> {
        let mut where_parts: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        // Checkpoints are event_log records with record_type=checkpoint in metadata
        where_parts.push("n.layer = 'event_log'".to_string());
        where_parts.push("n.is_active = 1".to_string());
        where_parts.push("json_valid(n.metadata_json)".to_string());
        where_parts
            .push("json_extract(n.metadata_json, '$.record_type') = 'checkpoint'".to_string());
        if let Some(pid) = project_id {
            where_parts.push("n.project_id = ?".to_string());
            params.push(Box::new(pid.to_string()));
        }

        // Status filter
        let effective_status = task_status.unwrap_or("active");
        match effective_status {
            "active" => {
                where_parts.push(
                    "(json_extract(n.metadata_json, '$.task_status') IN ('blocked', 'in_progress'))"
                        .to_string(),
                );
            }
            "blocked" | "in_progress" | "completed" | "abandoned" => {
                where_parts.push("json_extract(n.metadata_json, '$.task_status') = ?".to_string());
                params.push(Box::new(effective_status.to_string()));
            }
            _ => {}
        }

        // Task ID exact match
        if let Some(tid) = task_id {
            where_parts.push("json_extract(n.metadata_json, '$.task_id') = ?".to_string());
            params.push(Box::new(tid.to_string()));
        }

        let where_sql = where_parts.join(" AND ");

        // If query provided, use FTS or LIKE
        let sql = if let Some(q) = query {
            if !q.trim().is_empty() {
                // Use content LIKE for checkpoint search (simpler than FTS)
                let like_clause = format!("{where_sql} AND n.content LIKE ? ESCAPE '\\'");
                let escaped = q
                    .trim()
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_");
                params.push(Box::new(format!("%{escaped}%")));
                format!(
                    "SELECT n.* FROM notes n WHERE {like_clause}
                     ORDER BY COALESCE(n.updated_at, n.created_at) DESC LIMIT ?"
                )
            } else {
                format!(
                    "SELECT n.* FROM notes n WHERE {where_sql}
                     ORDER BY COALESCE(n.updated_at, n.created_at) DESC LIMIT ?"
                )
            }
        } else {
            format!(
                "SELECT n.* FROM notes n WHERE {where_sql}
                 ORDER BY COALESCE(n.updated_at, n.created_at) DESC LIMIT ?"
            )
        };
        params.push(Box::new(limit as i64));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();

        match conn.prepare(&sql) {
            Ok(mut stmt) => stmt
                .query_map(rusqlite::params_from_iter(param_refs.iter()), |row| {
                    crate::storage::db::NoteRow::from_row(row)
                })
                .map(|rows| {
                    rows.filter_map(|r| match r {
                        Ok(note) => Some(note),
                        Err(e) => {
                            warn!("Skipping checkpoint row: {e}");
                            None
                        }
                    })
                    .map(|note| {
                        let metadata: Option<serde_json::Value> = note
                            .metadata_json
                            .as_deref()
                            .and_then(|s| serde_json::from_str(s).ok());
                        SearchResult {
                            id: note.id.clone(),
                            content: note.content.clone(),
                            layer: note.layer.clone(),
                            category: note.category.clone(),
                            score: 1.0, // Checkpoints aren't scored
                            created_at: note.created_at.clone(),
                            topic_key: note.topic_key.clone(),
                            root_id: note.root_id.clone(),
                            version: note.version,
                            channel: Some("checkpoint".to_string()),
                            temperature: None,
                            is_head: true,
                            metadata,
                            recall_count: 0,
                            last_recalled_at: None,
                        }
                    })
                    .collect()
                })
                .unwrap_or_default(),
            Err(e) => {
                warn!("search_checkpoints failed: {e}");
                vec![]
            }
        }
    }

    /// Get context snapshot (wake mode).
    ///
    /// Returns active checkpoints + recent memory count.
    pub fn get_context_wake(&self) -> serde_json::Value {
        self.get_context_wake_for_project(None)
    }

    pub fn get_context_wake_for_project(&self, project_id: Option<&str>) -> serde_json::Value {
        self.db.with_conn(|conn| {
            let checkpoints =
                self.search_checkpoints_inner(conn, None, None, Some("active"), project_id, 10);
            // 2026-05-04 fix (wake_search_drift_red): recent_memories used to be
            // `count_recent_memories(...)` returning a bare integer. The AI
            // opening a session would see "recent_memories: 15" — the 15 rows
            // themselves were completely invisible, so anything the user wrote in
            // the last hour stayed hidden behind the active-checkpoint dump.
            // Now the field carries the actual previews (title + layer +
            // created_at) and active_workstream surfaces the top task_ids by
            // recent write density (Python build2-Q3 parity).
            let recent_memories = self.list_recent_memories(conn, project_id, 10);
            let active_workstream = self.list_active_workstreams(conn, project_id, 7, 5);
            let recently_promoted_from_dreaming =
                self.list_recently_promoted_from_dreaming(conn, project_id, 7, 5);
            let since_iso = (chrono::Utc::now() - chrono::Duration::minutes(5))
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string();
            let active_sessions = self.list_active_sessions(conn, &since_iso, project_id);

            serde_json::json!({
                "mode": "wake",
                "active_checkpoints": checkpoints.len(),
                "checkpoints": checkpoints.iter().map(|c| {
                    let meta = c.metadata.as_ref();
                    serde_json::json!({
                        "task_id": meta.and_then(|m| m.get("task_id")).and_then(|v| v.as_str()),
                        "task_status": meta.and_then(|m| m.get("task_status")).and_then(|v| v.as_str()),
                        "summary": safe_truncate(&c.content, 200),
                        "next_step": meta.and_then(|m| m.get("next_step")).and_then(|v| v.as_str()),
                        "blocker": meta.and_then(|m| m.get("blocker")).and_then(|v| v.as_str()),
                    })
                }).collect::<Vec<_>>(),
                "recent_memories": recent_memories,
                "active_workstream": active_workstream,
                "recently_promoted_from_dreaming": recently_promoted_from_dreaming,
                "active_sessions": active_sessions,
            })
        })
    }

    /// Get a Rust-owned full context snapshot.
    ///
    /// This intentionally starts with the stable, database-backed core of the
    /// legacy Python full snapshot: identity/fact entities, recent events,
    /// active checkpoints/task states, recent memories, review/failed-memory
    /// signals, procedures, active sessions, and workstreams. Python-only
    /// satellites such as doc-refresh and daemon state remain empty sections
    /// until their R2/R3 Rust owners exist, but full mode no longer falls back
    /// to an unsupported stub.
    pub fn get_context_full(&self) -> serde_json::Value {
        self.get_context_full_for_project(None)
    }

    pub fn get_context_full_for_project(&self, project_id: Option<&str>) -> serde_json::Value {
        self.db.with_conn(|conn| {
            let now = chrono::Utc::now();
            let generated_at = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
            let expires_at = (now + chrono::Duration::minutes(30))
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string();
            let checkpoints =
                self.search_checkpoints_inner(conn, None, None, Some("active"), project_id, 5);
            let active_checkpoints = checkpoints
                .iter()
                .map(checkpoint_snapshot_json)
                .collect::<Vec<_>>();
            let active_task_states = checkpoints
                .iter()
                .map(checkpoint_task_state_json)
                .collect::<Vec<_>>();
            let key_entities = self.list_context_notes(
                conn,
                project_id,
                &["identity_schema", "verified_fact"],
                12,
                true,
            );
            let recent_events =
                self.list_context_notes(conn, project_id, &["event_log"], 12, false);
            let active_procedures =
                self.list_context_notes(conn, project_id, &["procedure_schema"], 5, true);
            let recent_memories = self.list_recent_memories(conn, project_id, 15);
            let needs_review = self.list_needs_review(conn, project_id, &generated_at, 5);
            let failed_memories = self.list_failed_memories(conn, project_id, 15);
            let active_workstream = self.list_active_workstreams(conn, project_id, 7, 5);
            let recently_promoted_from_dreaming =
                self.list_recently_promoted_from_dreaming(conn, project_id, 7, 5);
            let since_iso = (now - chrono::Duration::minutes(5))
                .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                .to_string();
            let active_sessions = self.list_active_sessions(conn, &since_iso, project_id);

            let visible_task_count = active_task_states.len();
            let mut summary_parts = Vec::new();
            if let Some(first) = active_task_states.first() {
                if let Some(task_id) = first.get("task_id").and_then(|v| v.as_str()) {
                    let status = first
                        .get("task_status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("in_progress");
                    summary_parts.push(format!("任务「{task_id}」({status})"));
                }
                if let Some(next_step) = first.get("next_step").and_then(|v| v.as_str()) {
                    if !next_step.is_empty() {
                        summary_parts.push(format!("下一步: {}", safe_truncate(next_step, 160)));
                    }
                }
            }
            if !failed_memories.is_empty() || !needs_review.is_empty() {
                summary_parts.push(format!(
                    "记忆质量预警: 失败/过时×{} | 待复核×{}",
                    failed_memories.len(),
                    needs_review.len()
                ));
            }
            if !recently_promoted_from_dreaming.is_empty() {
                summary_parts.push(format!(
                    "dream反馈待回收×{}",
                    recently_promoted_from_dreaming.len()
                ));
            }
            summary_parts.push(format!(
                "{}实体/{}事件/{}任务",
                key_entities.len(),
                recent_events.len(),
                visible_task_count
            ));
            let summary = summary_parts.join(" | ");

            serde_json::json!({
                "project_id": project_id.unwrap_or("memra"),
                "generated_at": generated_at,
                "expires_at": expires_at,
                "mode": "full",
                "cache_hit": false,
                "flush_error_warning": "",
                "stats": {
                    "key_entities": key_entities.len(),
                    "recent_events": recent_events.len(),
                    "active_tasks": 0,
                    "active_procedures": active_procedures.len(),
                    "active_task_states": active_task_states.len(),
                    "active_checkpoints": active_checkpoints.len(),
                    "recent_memories": recent_memories.len(),
                    "failed_memories": failed_memories.len(),
                    "experience_candidates": 0,
                    "experience_space": 0,
                    "expert_reviews": 0,
                    "recommended_actions": 0,
                    "living_doc_candidates": 0,
                    "always_on_daemon": 0,
                    "startup_manifest": 0,
                    "dream_feedback_due": recently_promoted_from_dreaming.len(),
                    "session_memory_summary": 0,
                },
                "key_entities": key_entities,
                "recent_events": recent_events,
                "active_tasks": [],
                "active_procedures": active_procedures,
                "active_task_states": active_task_states,
                "active_checkpoints": active_checkpoints,
                "recent_memories": recent_memories,
                "needs_review": needs_review,
                "failed_memories": failed_memories.clone(),
                "experience_space": {
                    "summary": "",
                    "stable_procedures": [],
                    "candidate_experiences": [],
                    "review_queue": [],
                    "degraded_memories": failed_memories,
                },
                "expert_reviews": [],
                "recommended_actions": [],
                "living_doc_candidates": [],
                "always_on_daemon_summary": "",
                "startup_manifest": {},
                "dream_feedback_due": recently_promoted_from_dreaming.clone(),
                "doc_refresh_report": {},
                "doc_refresh_patch": {},
                "doc_refresh_plan": {},
                "doc_refresh_execution": {},
                "doc_refresh_apply_ready": {},
                "doc_refresh_diff_preview": {},
                "doc_refresh_verification": {},
                "session_memory_summary": "",
                "l1_active_workstream": active_workstream,
                "l2_recent_broadcasts": [],
                "active_sessions": active_sessions,
                "summary": summary,
                "usage_protocol": "MA 2026 使用协议: get_context() 建立初始上下文 | search_manifest(query) 搜索北极星/长期目标 | search_rules(query) 语义搜索(全层) | add_rule(content, room?) 写入记忆 | save_checkpoint(task_id, summary) 存任务断点 | report_outcome(memory_id, outcome) 反馈 | list_rooms() 查看可用记忆宫殿"
            })
        })
    }

    fn list_context_notes(
        &self,
        conn: &Connection,
        project_id: Option<&str>,
        layers: &[&str],
        limit: usize,
        include_checkpoints: bool,
    ) -> Vec<serde_json::Value> {
        if layers.is_empty() || limit == 0 {
            return Vec::new();
        }
        let recall_expr = if column_exists(conn, "notes", "recall_count") {
            "COALESCE(recall_count, 0)"
        } else {
            "0"
        };
        let layer_placeholders = std::iter::repeat_n("?", layers.len())
            .collect::<Vec<_>>()
            .join(", ");
        let mut where_parts = vec![
            active_note_where_clause(conn).to_string(),
            format!("layer IN ({layer_placeholders})"),
        ];
        if project_id.is_some() {
            where_parts.push("project_id = ?".to_string());
        }
        let sql = format!(
            "SELECT id, content, layer, category, confidence, created_at, metadata_json, {recall_expr} AS recall_count
             FROM notes
             WHERE {}
             ORDER BY
               CASE layer WHEN 'identity_schema' THEN 0 WHEN 'verified_fact' THEN 1 WHEN 'event_log' THEN 2 ELSE 3 END,
               recall_count DESC,
               confidence DESC,
               created_at DESC
             LIMIT ?",
            where_parts.join(" AND ")
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = layers
            .iter()
            .map(|layer| Box::new((*layer).to_string()) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        if let Some(pid) = project_id {
            params.push(Box::new(pid.to_string()));
        }
        params.push(Box::new((limit * 3) as i64));
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();

        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(error) => {
                warn!("list_context_notes prepare failed: {error}");
                return Vec::new();
            }
        };
        stmt.query_map(rusqlite::params_from_iter(param_refs.iter()), |row| {
            let id: String = row.get(0)?;
            let content: String = row.get(1)?;
            let layer: String = row.get(2)?;
            let category: Option<String> = row.get(3)?;
            let confidence: Option<f64> = row.get(4)?;
            let created_at: Option<String> = row.get(5)?;
            let metadata_json: Option<String> = row.get(6)?;
            let recall_count: i64 = row.get(7)?;
            let source = if layer == "identity_schema" {
                "constitution"
            } else {
                "memory"
            };
            let metadata = metadata_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
            let is_checkpoint = metadata
                .as_ref()
                .and_then(|m| m.get("record_type"))
                .and_then(|v| v.as_str())
                == Some("checkpoint");
            Ok((
                serde_json::json!({
                    "id": id,
                    "content": safe_truncate(&content, 240),
                    "preview": safe_truncate(&content, 160),
                    "layer": layer,
                    "category": category,
                    "confidence": confidence,
                    "created_at": created_at,
                    "recall_count": recall_count,
                    "source": source,
                    "metadata": metadata,
                    "is_checkpoint": is_checkpoint,
                }),
                metadata,
            ))
        })
        .map(|rows| {
            rows.filter_map(Result::ok)
                .filter(|(value, metadata)| {
                    if !include_checkpoints && value["is_checkpoint"].as_bool().unwrap_or(false) {
                        return false;
                    }
                    !metadata_has_failure_flag(metadata.as_ref())
                })
                .map(|(mut value, _)| {
                    if let Some(obj) = value.as_object_mut() {
                        obj.remove("is_checkpoint");
                    }
                    value
                })
                .take(limit)
                .collect()
        })
        .unwrap_or_default()
    }

    fn list_needs_review(
        &self,
        conn: &Connection,
        project_id: Option<&str>,
        now_iso: &str,
        limit: usize,
    ) -> Vec<serde_json::Value> {
        if limit == 0 {
            return Vec::new();
        }
        let active_clause = active_note_where_clause(conn);
        let mut sql = format!(
            "SELECT id, content, layer, category, review_after, created_at
             FROM notes
             WHERE {active_clause}
               AND review_after IS NOT NULL
               AND review_after < ?"
        );
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(now_iso.to_string())];
        if let Some(pid) = project_id {
            sql.push_str(" AND project_id = ?");
            params.push(Box::new(pid.to_string()));
        }
        sql.push_str(" ORDER BY review_after ASC LIMIT ?");
        params.push(Box::new(limit as i64));
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(rusqlite::params_from_iter(param_refs.iter()), |row| {
            let id: String = row.get(0)?;
            let content: String = row.get(1)?;
            let layer: String = row.get(2)?;
            let category: Option<String> = row.get(3)?;
            let review_after: Option<String> = row.get(4)?;
            let created_at: Option<String> = row.get(5)?;
            Ok(serde_json::json!({
                "id": id,
                "content": safe_truncate(&content, 200),
                "layer": layer,
                "category": category,
                "review_after": review_after,
                "created_at": created_at,
            }))
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    fn list_failed_memories(
        &self,
        conn: &Connection,
        project_id: Option<&str>,
        limit: usize,
    ) -> Vec<serde_json::Value> {
        if limit == 0 {
            return Vec::new();
        }
        let mut sql = "SELECT id, content, layer, category, metadata_json, created_at
                       FROM notes
                       WHERE is_active = 1
                         AND metadata_json IS NOT NULL
                         AND json_valid(metadata_json)"
            .to_string();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(pid) = project_id {
            sql.push_str(" AND project_id = ?");
            params.push(Box::new(pid.to_string()));
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?");
        params.push(Box::new((limit * 3) as i64));
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        stmt.query_map(rusqlite::params_from_iter(param_refs.iter()), |row| {
            let id: String = row.get(0)?;
            let content: String = row.get(1)?;
            let layer: String = row.get(2)?;
            let category: Option<String> = row.get(3)?;
            let metadata_json: Option<String> = row.get(4)?;
            let created_at: Option<String> = row.get(5)?;
            let metadata = metadata_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());
            Ok((
                serde_json::json!({
                    "id": id,
                    "content": safe_truncate(&content, 200),
                    "layer": layer,
                    "category": category,
                    "created_at": created_at,
                    "is_outdated": metadata_bool(metadata.as_ref(), "is_outdated"),
                    "is_corrected": metadata_bool(metadata.as_ref(), "is_corrected"),
                    "failure_count": metadata_i64(metadata.as_ref(), "failure_count"),
                }),
                metadata,
            ))
        })
        .map(|rows| {
            rows.filter_map(Result::ok)
                .filter(|(_, metadata)| metadata_has_failure_flag(metadata.as_ref()))
                .map(|(value, _)| value)
                .take(limit)
                .collect()
        })
        .unwrap_or_default()
    }

    /// Return the most-recent memory previews, newest first, as a list of
    /// JSON objects ready for embedding in the wake snapshot.
    ///
    /// Each entry carries enough to anchor the AI's "what just happened"
    /// reasoning without dragging full content into the snapshot:
    ///   { id, layer, category, created_at, preview, task_id?, status? }
    ///
    /// Excludes active checkpoint rows so wake doesn't double-count what
    /// already appears under `checkpoints`. Completed checkpoints are kept:
    /// they represent freshly-finished work and otherwise disappear from wake.
    fn list_recent_memories(
        &self,
        conn: &Connection,
        project_id: Option<&str>,
        limit: usize,
    ) -> Vec<serde_json::Value> {
        let mapper = |row: &rusqlite::Row<'_>| -> rusqlite::Result<serde_json::Value> {
            let id: String = row.get(0)?;
            let layer: Option<String> = row.get(1)?;
            let category: Option<String> = row.get(2)?;
            let created_at: Option<String> = row.get(3)?;
            let content: String = row.get(4)?;
            let metadata_json: Option<String> = row.get(5)?;
            let (is_checkpoint, task_id, task_status) = match metadata_json
                .as_deref()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            {
                Some(meta) => {
                    // Checkpoints are persisted as event_log rows whose
                    // metadata_json carries record_type="checkpoint" — the
                    // same discriminator used by search_checkpoints_inner.
                    // (Earlier `metadata.checkpoint == bool` was a guess that
                    // never matched real rows, so checkpoints leaked into
                    // recent_memories.)
                    let cp = meta
                        .get("record_type")
                        .and_then(|v| v.as_str())
                        .map(|s| s == "checkpoint")
                        .unwrap_or(false);
                    let tid = meta
                        .get("task_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let ts = meta
                        .get("task_status")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    (cp, tid, ts)
                }
                None => (false, None, None),
            };
            Ok(serde_json::json!({
                "id": id,
                "layer": layer,
                "category": category,
                "created_at": created_at,
                "preview": safe_truncate(&content, 160),
                "task_id": task_id,
                "task_status": task_status,
                "is_checkpoint": is_checkpoint,
            }))
        };
        let rows: Vec<serde_json::Value> = match project_id {
            Some(pid) => {
                let active_clause = active_note_where_clause(conn);
                let sql = format!(
                    "SELECT id, layer, category, created_at, content, metadata_json
                     FROM notes
                     WHERE {active_clause} AND project_id = ?1
                     ORDER BY created_at DESC
                     LIMIT ?2"
                );
                let mut stmt = match conn.prepare(&sql) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map(rusqlite::params![pid, (limit * 3) as i64], mapper)
                    .map(|r| r.filter_map(|x| x.ok()).collect())
                    .unwrap_or_default()
            }
            None => {
                let active_clause = active_note_where_clause(conn);
                let sql = format!(
                    "SELECT id, layer, category, created_at, content, metadata_json
                     FROM notes
                     WHERE {active_clause}
                     ORDER BY created_at DESC
                     LIMIT ?1"
                );
                let mut stmt = match conn.prepare(&sql) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map([(limit * 3) as i64], mapper)
                    .map(|r| r.filter_map(|x| x.ok()).collect())
                    .unwrap_or_default()
            }
        };
        // Active checkpoints show up under `checkpoints`; completed
        // checkpoints do not, so keep them visible as fresh work.
        rows.into_iter()
            .filter(|v| {
                !v["is_checkpoint"].as_bool().unwrap_or(false)
                    || v["task_status"].as_str() == Some("completed")
            })
            .map(|mut v| {
                if let Some(note_id) = v["id"].as_str() {
                    let (relation_count, relation_types) = relation_summary(conn, note_id);
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert(
                            "relation_count".to_string(),
                            serde_json::json!(relation_count),
                        );
                        obj.insert(
                            "relation_types".to_string(),
                            serde_json::json!(relation_types),
                        );
                    }
                }
                v
            })
            .take(limit)
            .collect()
    }

    /// Return dream candidates promoted recently.
    ///
    /// This mirrors Python `_safe_list_recently_promoted_from_dreaming` enough
    /// for Rust wake to expose the consolidation loop instead of hiding it
    /// behind background tables. Missing legacy tables return an empty list.
    fn list_recently_promoted_from_dreaming(
        &self,
        conn: &Connection,
        project_id: Option<&str>,
        lookback_days: i64,
        limit: usize,
    ) -> Vec<serde_json::Value> {
        if lookback_days <= 0 || limit == 0 || !table_exists(conn, "dream_candidates") {
            return Vec::new();
        }

        let cutoff_seconds = lookback_days * 86400;
        let mapper = |row: &rusqlite::Row<'_>| -> rusqlite::Result<serde_json::Value> {
            let id: String = row.get(0)?;
            let summary: String = row.get(1)?;
            let confidence: f64 = row.get(2)?;
            let promoted_to: Option<String> = row.get(3)?;
            let promoted_at: Option<String> = row.get(4)?;
            let frequency: Option<i64> = row.get(5)?;
            Ok(serde_json::json!({
                "id": id,
                "summary": safe_truncate(&summary, 150),
                "confidence": (confidence * 100.0).round() / 100.0,
                "promoted_to": promoted_to,
                "promoted_at": promoted_at,
                "frequency": frequency.unwrap_or(1),
                "feedback_needed": true,
            }))
        };

        let base_sql = "
            SELECT id, summary, confidence, promoted_to, promoted_at, frequency
            FROM dream_candidates
            WHERE verdict = 'promoted'
              AND promoted_at IS NOT NULL
              AND CAST(strftime('%s', 'now') AS INTEGER)
                  - CAST(strftime('%s', substr(promoted_at, 1, 19)) AS INTEGER)
                  <= ?
              {PROJECT}
            ORDER BY CAST(strftime('%s', substr(promoted_at, 1, 19)) AS INTEGER) DESC
            LIMIT ?";

        match project_id {
            Some(pid) => {
                let sql = base_sql.replace("{PROJECT}", "AND project_id = ?");
                let mut stmt = match conn.prepare(&sql) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map(rusqlite::params![cutoff_seconds, pid, limit as i64], mapper)
                    .map(|r| r.filter_map(|x| x.ok()).collect())
                    .unwrap_or_default()
            }
            None => {
                let sql = base_sql.replace("{PROJECT}", "");
                let mut stmt = match conn.prepare(&sql) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map(rusqlite::params![cutoff_seconds, limit as i64], mapper)
                    .map(|r| r.filter_map(|x| x.ok()).collect())
                    .unwrap_or_default()
            }
        }
    }

    /// Return the top active workstreams (by event/note write count) within
    /// the last `since_days` window, scoped to `project_id` when supplied.
    ///
    /// Mirrors Python `_list_active_workstreams` (build2 Q3) so the AI gets a
    /// passive-recall signal of "what task_ids alice is actively touching"
    /// at session start, independent of the cosine-based search engine.
    ///
    /// Aggregation key is `metadata_json.task_id`, falling back to
    /// `metadata_json.branch`. Untagged rows are intentionally skipped — they
    /// would otherwise collapse into one giant bucket and dominate top-K.
    fn list_active_workstreams(
        &self,
        conn: &Connection,
        project_id: Option<&str>,
        since_days: i64,
        limit: usize,
    ) -> Vec<serde_json::Value> {
        if since_days <= 0 || limit == 0 {
            return Vec::new();
        }
        let cutoff_seconds = since_days * 86400;
        let mapper = |row: &rusqlite::Row<'_>| -> rusqlite::Result<serde_json::Value> {
            let key: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            let last_at: Option<String> = row.get(2)?;
            let preview: Option<String> = row.get(3)?;
            Ok(serde_json::json!({
                "key": key,
                "count": count,
                "last_at": last_at,
                "preview": preview,
            }))
        };
        // Placeholder convention: anonymous `?` only.
        //
        // The first draft of this function used `?1` for cutoff_seconds and
        // bare `?` everywhere else. Per SQLite's parameter rules the `?N`
        // form claims index N explicitly, while bare `?` keeps numbering
        // from "one larger than the largest index used so far" — which means
        // a bare `?` appearing *before* `?1` lands on index 1, then `?1`
        // re-uses that same index, and the bare `?`s after `?1` skip ahead
        // to index 2+. Mixed numbering plus rusqlite's positional binding
        // therefore left the project_id slot bound to cutoff_seconds, the
        // prepared statement returned no rows, and `unwrap_or_default()`
        // hid the failure as an empty wake.active_workstream. Codex review
        // (2026-05-04 P1-1) caught this in production; the test
        // wake_search_drift_red::red_codex_p1_active_workstream_returns_data_when_project_scoped
        // pins the regression. Use only `?` here so source-order positional
        // binding is the single source of truth.
        let base_sql = "
            SELECT
                grouped.key,
                grouped.cnt,
                (
                    SELECT n_at.created_at
                    FROM notes AS n_at
                    WHERE COALESCE(
                              json_extract(n_at.metadata_json, '$.task_id'),
                              json_extract(n_at.metadata_json, '$.branch')
                          ) = grouped.key
                      AND n_at.is_active = 1
                      {AT_PROJECT}
                    ORDER BY CAST(strftime('%s', substr(n_at.created_at, 1, 19)) AS INTEGER) DESC
                    LIMIT 1
                ) AS last_at,
                (
                    SELECT SUBSTR(n2.content, 1, 80)
                    FROM notes AS n2
                    WHERE COALESCE(
                              json_extract(n2.metadata_json, '$.task_id'),
                              json_extract(n2.metadata_json, '$.branch')
                          ) = grouped.key
                      AND n2.is_active = 1
                      {N2_PROJECT}
                    ORDER BY CAST(strftime('%s', substr(n2.created_at, 1, 19)) AS INTEGER) DESC
                    LIMIT 1
                ) AS preview
            FROM (
                SELECT
                    COALESCE(
                        json_extract(metadata_json, '$.task_id'),
                        json_extract(metadata_json, '$.branch')
                    ) AS key,
                    COUNT(*) AS cnt
                FROM notes
                WHERE is_active = 1
                  AND CAST(strftime('%s', 'now') AS INTEGER)
                      - CAST(strftime('%s', substr(created_at, 1, 19)) AS INTEGER)
                      <= ?
                  {INNER_PROJECT}
                GROUP BY key
                HAVING key IS NOT NULL
            ) AS grouped
            ORDER BY grouped.cnt DESC
            LIMIT ?";
        match project_id {
            Some(pid) => {
                let sql = base_sql
                    .replace("{AT_PROJECT}", "AND n_at.project_id = ?")
                    .replace("{N2_PROJECT}", "AND n2.project_id = ?")
                    .replace("{INNER_PROJECT}", "AND project_id = ?");
                let mut stmt = match conn.prepare(&sql) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map(
                    rusqlite::params![pid, pid, cutoff_seconds, pid, limit as i64],
                    mapper,
                )
                .map(|r| r.filter_map(|x| x.ok()).collect())
                .unwrap_or_default()
            }
            None => {
                let sql = base_sql
                    .replace("{AT_PROJECT}", "")
                    .replace("{N2_PROJECT}", "")
                    .replace("{INNER_PROJECT}", "");
                let mut stmt = match conn.prepare(&sql) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map(rusqlite::params![cutoff_seconds, limit as i64], mapper)
                    .map(|r| r.filter_map(|x| x.ok()).collect())
                    .unwrap_or_default()
            }
        }
    }

    /// List sessions whose `last_seen` is within the last 5 minutes.
    ///
    /// Returns at most 50 rows ordered by `last_seen DESC`.  If the `sessions`
    /// table doesn't exist (legacy DB without PR1 migration) returns an empty
    /// `Vec` rather than propagating an error.
    ///
    /// PR #232 Codex P1 fix: scopes to `project_id` when supplied so
    /// multi-project DBs don't leak session ids/labels across project
    /// boundaries via `get_context.active_sessions`. Sessions written before
    /// the migration (NULL `project_id`) are excluded from project-scoped
    /// queries — they remain visible only via the cross-project path
    /// (`project_id == None`), which is intentional: the column is opt-in
    /// for legacy data.
    fn list_active_sessions(
        &self,
        conn: &Connection,
        since_iso: &str,
        project_id: Option<&str>,
    ) -> Vec<serde_json::Value> {
        if !table_exists(conn, "sessions") {
            return Vec::new();
        }
        let mapper = |row: &rusqlite::Row<'_>| -> rusqlite::Result<serde_json::Value> {
            let session_id: String = row.get(0)?;
            let agent_label: Option<String> = row.get(1)?;
            let first_seen: String = row.get(2)?;
            let last_seen: String = row.get(3)?;
            Ok(serde_json::json!({
                "session_id": session_id,
                "agent_label": agent_label,
                "first_seen": first_seen,
                "last_seen": last_seen,
            }))
        };
        match project_id {
            Some(pid) => {
                let sql = "SELECT session_id, agent_label, first_seen, last_seen
                           FROM sessions
                           WHERE last_seen >= ?1 AND project_id = ?2
                           ORDER BY last_seen DESC
                           LIMIT 50";
                let mut stmt = match conn.prepare(sql) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map(rusqlite::params![since_iso, pid], mapper)
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
            }
            None => {
                let sql = "SELECT session_id, agent_label, first_seen, last_seen
                           FROM sessions
                           WHERE last_seen >= ?1
                           ORDER BY last_seen DESC
                           LIMIT 50";
                let mut stmt = match conn.prepare(sql) {
                    Ok(s) => s,
                    Err(_) => return Vec::new(),
                };
                stmt.query_map([since_iso], mapper)
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
            }
        }
    }
}

fn latest_recall_timestamps(
    conn: &Connection,
    candidates: &[Candidate],
) -> Result<HashMap<String, String>, String> {
    let mut ids: Vec<&str> = candidates
        .iter()
        .map(|candidate| candidate.note.id.as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() || !table_exists(conn, "recall_log") {
        return Ok(HashMap::new());
    }

    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(", ");
    // ORDER BY id DESC, NOT julianday(recalled_at) DESC.
    //
    // recall_log.id is AUTOINCREMENT — strictly monotonic with insertion
    // order — so "highest id per note" IS the most recent recall event
    // by definition. Sorting by julianday() of the timestamp text instead
    // (the previous design) had two failure modes Codex adversarial review
    // surfaced across 4 iterations on this PR:
    //
    // (a) `julianday('not-a-date')` returns NULL, and SQLite's default DESC
    //     ordering puts NULL after valid rows, so a malformed *newer* row
    //     was visited *after* an older valid sibling. The loop accepted
    //     the older valid row, then silently skipped the malformed one
    //     because the note was already in `out` — leaking stale
    //     `last_recalled_at` into recall scoring.
    //
    // (b) Chasing every SQLite timestamp grammar variant (`Z`, `+00:00`,
    //     ` +00:00`, subsecond fractions, etc.) is a losing game. Insertion
    //     order is the authoritative recency signal anyway; the timestamp
    //     text is decorative as far as ranking goes.
    //
    // With id DESC, the row our loop sees first IS the most recent event.
    // If that row's timestamp text fails parse, the note quarantines
    // immediately — no chance for an older sibling to backfill.
    let sql = format!(
        "SELECT id, note_id, recalled_at
         FROM recall_log
         WHERE note_id IN ({placeholders})
         ORDER BY note_id ASC, id DESC"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|error| format!("latest_recall_timestamps prepare failed: {error}"))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(ids), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| format!("latest_recall_timestamps query failed: {error}"))?;

    // Two-bucket policy:
    //   `out`     — note_ids whose newest-by-julianday row parsed cleanly
    //   `tainted` — note_ids whose newest row failed parse; we MUST NOT fall
    //               through to an older sibling, because returning a stale
    //               `last_recalled_at` would silently corrupt downstream
    //               recency scoring (Codex adversarial review on a759fc9
    //               found this regression in the previous fail-open design).
    //
    // Cross-batch failure is still graceful (one tainted note does not wipe
    // hydration for siblings), but PER NOTE we quarantine on first bad row
    // instead of pretending an older parseable row is "the latest".
    let mut out = HashMap::new();
    let mut tainted: HashSet<String> = HashSet::new();
    for row in rows {
        match row {
            Ok((id, note_id, recalled_at)) => {
                if out.contains_key(&note_id) || tainted.contains(&note_id) {
                    // Already resolved (good) or already quarantined (bad);
                    // older siblings are irrelevant either way.
                    continue;
                }
                let (_parsed, display) = match parse_recall_timestamp(&recalled_at) {
                    Ok(pair) => pair,
                    Err(error) => {
                        warn!(
                            "latest_recall_timestamps note tainted: invalid recall_log.recalled_at for id {id}, note_id {note_id}: {recalled_at} ({error})"
                        );
                        tainted.insert(note_id);
                        continue;
                    }
                };
                out.insert(note_id, display);
            }
            Err(error) => warn!("latest_recall_timestamps row skipped: {error}"),
        }
    }
    Ok(out)
}

/// Parse a `recall_log.recalled_at` text into a UTC `DateTime`.
///
/// Three shapes are accepted, in priority order:
///
/// 1. RFC3339 with offset, e.g. `2026-04-14T17:40:08.123456+00:00` or
///    `...Z`. This is what Python `datetime.now(timezone.utc).isoformat()`
///    and Rust `chrono::Utc::now().to_rfc3339()` produce when the writer
///    passes `recalled_at` explicitly.
/// 2. SQLite `datetime('now', 'subsec')` and any explicit `YYYY-MM-DD
///    HH:MM:SS.fff` writer (assumed UTC). The ORDER BY uses
///    `julianday(recalled_at)`, which accepts subsecond precision, so a
///    row that survives ordering must also survive parse — otherwise it
///    wipes the whole hydration map for the candidate batch.
/// 3. SQLite `datetime('now')` default format, `YYYY-MM-DD HH:MM:SS`,
///    assumed UTC. This is how `backend/services/storage_recall.py`
///    inserts today — its `INSERT` omits `recalled_at`, so rows receive
///    the schema DEFAULT from `storage_schema.py::recall_log`. All
///    existing production rows are in this shape, so failing them would
///    regress recall-based scoring on every search.
///
/// Parse a recall timestamp and return `(instant, display)`.
///
/// `display` is what gets forwarded to callers as `last_recalled_at`. For
/// RFC3339 input it is the verbatim raw string, preserving Python parity —
/// `backend/services/storage_recall.py::get_recall_stats` returns the stored
/// text as-is via `MAX(recalled_at)`, so round-tripping through
/// `to_rfc3339()` would silently drift `...Z` → `...+00:00` and truncate
/// microsecond precision (`.900000` → `.900`). For SQLite-default input
/// (`YYYY-MM-DD HH:MM:SS[.fff]`) we must canonicalize to RFC3339 because
/// downstream `postprocess::add_temperature_labels` calls
/// `DateTime::parse_from_rfc3339`, which would otherwise reject the row.
fn parse_recall_timestamp(raw: &str) -> Result<(DateTime<Utc>, String), String> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Ok((dt.with_timezone(&Utc), raw.to_string()));
    }
    // `%.f` accepts an optional `.fffffffff` fraction (zero-or-more digits),
    // so this single format covers both `2026-04-14 17:40:08` and
    // `2026-04-14 17:40:08.123` without needing a second parse attempt.
    if let Ok(naive) = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S%.f") {
        return match Utc.from_local_datetime(&naive) {
            chrono::LocalResult::Single(dt) => Ok((dt, dt.to_rfc3339())),
            _ => Err("ambiguous or invalid SQLite datetime('now') value".to_string()),
        };
    }
    Err("not RFC3339 and not SQLite datetime('now') format".to_string())
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .unwrap_or(false)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(_) => return false,
    };
    stmt.query_map([], |row| row.get::<_, String>(1))
        .map(|rows| {
            rows.filter_map(Result::ok)
                .any(|name| name.eq_ignore_ascii_case(column))
        })
        .unwrap_or(false)
}

fn active_note_where_clause(conn: &Connection) -> &'static str {
    if column_exists(conn, "notes", "evolution_state") {
        "is_active = 1 AND COALESCE(evolution_state, 'active') = 'active'"
    } else {
        "is_active = 1"
    }
}

fn metadata_bool(metadata: Option<&serde_json::Value>, key: &str) -> bool {
    metadata
        .and_then(|m| m.get(key))
        .and_then(|v| {
            v.as_bool().or_else(|| {
                v.as_i64()
                    .map(|n| n != 0)
                    .or_else(|| v.as_str().map(|s| matches!(s, "1" | "true" | "True")))
            })
        })
        .unwrap_or(false)
}

fn metadata_i64(metadata: Option<&serde_json::Value>, key: &str) -> i64 {
    metadata
        .and_then(|m| m.get(key))
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_u64().and_then(|n| i64::try_from(n).ok()))
                .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
        })
        .unwrap_or(0)
}

fn metadata_has_failure_flag(metadata: Option<&serde_json::Value>) -> bool {
    metadata_bool(metadata, "is_outdated")
        || metadata_bool(metadata, "is_corrected")
        || metadata_i64(metadata, "failure_count") > 0
}

fn checkpoint_snapshot_json(checkpoint: &SearchResult) -> serde_json::Value {
    let meta = checkpoint.metadata.as_ref();
    serde_json::json!({
        "id": checkpoint.id,
        "task_id": meta.and_then(|m| m.get("task_id")).and_then(|v| v.as_str()),
        "task_status": meta.and_then(|m| m.get("task_status")).and_then(|v| v.as_str()).unwrap_or("in_progress"),
        "summary": safe_truncate(&checkpoint.content, 240),
        "blocker": meta.and_then(|m| m.get("blocker")).cloned().unwrap_or(serde_json::Value::Null),
        "next_step": meta.and_then(|m| m.get("next_step")).cloned().unwrap_or(serde_json::Value::Null),
        "task_context": meta.and_then(|m| m.get("task_context")).cloned().unwrap_or(serde_json::Value::Null),
        "task_specification": meta.and_then(|m| m.get("task_specification")).cloned().unwrap_or(serde_json::Value::Null),
        "files_and_functions": meta.and_then(|m| m.get("files_and_functions")).cloned().unwrap_or_else(|| serde_json::json!([])),
        "workflow": meta.and_then(|m| m.get("workflow")).cloned().unwrap_or_else(|| serde_json::json!([])),
        "errors_and_corrections": meta.and_then(|m| m.get("errors_and_corrections")).cloned().unwrap_or_else(|| serde_json::json!([])),
        "decisions": meta.and_then(|m| m.get("decisions")).cloned().unwrap_or_else(|| serde_json::json!([])),
        "living_docs": meta.and_then(|m| m.get("living_docs")).cloned().unwrap_or_else(|| serde_json::json!([])),
        "created_at": checkpoint.created_at,
        "stale": false,
    })
}

fn checkpoint_task_state_json(checkpoint: &SearchResult) -> serde_json::Value {
    let meta = checkpoint.metadata.as_ref();
    serde_json::json!({
        "task_id": meta.and_then(|m| m.get("task_id")).and_then(|v| v.as_str()),
        "task_status": meta.and_then(|m| m.get("task_status")).and_then(|v| v.as_str()).unwrap_or("in_progress"),
        "summary": safe_truncate(&checkpoint.content, 240),
        "blocker": meta.and_then(|m| m.get("blocker")).cloned().unwrap_or(serde_json::Value::Null),
        "next_step": meta.and_then(|m| m.get("next_step")).cloned().unwrap_or(serde_json::Value::Null),
        "task_context": meta.and_then(|m| m.get("task_context")).cloned().unwrap_or(serde_json::Value::Null),
        "task_specification": meta.and_then(|m| m.get("task_specification")).cloned().unwrap_or(serde_json::Value::Null),
        "files_and_functions": meta.and_then(|m| m.get("files_and_functions")).cloned().unwrap_or_else(|| serde_json::json!([])),
        "workflow": meta.and_then(|m| m.get("workflow")).cloned().unwrap_or_else(|| serde_json::json!([])),
        "errors_and_corrections": meta.and_then(|m| m.get("errors_and_corrections")).cloned().unwrap_or_else(|| serde_json::json!([])),
        "decisions": meta.and_then(|m| m.get("decisions")).cloned().unwrap_or_else(|| serde_json::json!([])),
        "living_docs": meta.and_then(|m| m.get("living_docs")).cloned().unwrap_or_else(|| serde_json::json!([])),
        "created_at": checkpoint.created_at,
        "stale": false,
    })
}

fn follow_results_to_canonical_heads(conn: &Connection, results: &mut [SearchResult]) {
    for result in results {
        let original_id = result.id.clone();
        let read = match hydrate_head_by_canonical_or_topic(
            conn,
            Some(&original_id),
            result.root_id.as_deref(),
            result.topic_key.as_deref(),
        ) {
            Ok(read) => read,
            Err(error) => {
                warn!("canonical head hydration failed for {original_id}: {error}");
                continue;
            }
        };
        let Some(head) = read.note else {
            continue;
        };
        if head.id == original_id {
            continue;
        }
        apply_canonical_head(result, head, &original_id, read.source);
    }
}

fn apply_canonical_head(
    result: &mut SearchResult,
    head: NoteRow,
    evolved_from: &str,
    source: CanonicalReadSource,
) {
    let previous_metadata = result.metadata.take();
    let head_metadata = head
        .metadata_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());

    result.id = head.id;
    result.content = head.content;
    result.layer = head.layer;
    result.category = head.category;
    result.created_at = head.created_at;
    result.topic_key = head.topic_key;
    result.root_id = head.root_id;
    result.version = head.version;
    result.is_head = head.is_head && evolution_state_is_head(head.evolution_state.as_deref());
    result.recall_count = head.recall_count;
    result.metadata = Some(merge_canonical_metadata(
        head_metadata,
        previous_metadata,
        evolved_from,
        source,
    ));
}

fn merge_canonical_metadata(
    head_metadata: Option<serde_json::Value>,
    previous_metadata: Option<serde_json::Value>,
    evolved_from: &str,
    source: CanonicalReadSource,
) -> serde_json::Value {
    let mut merged = head_metadata
        .filter(|value| value.is_object())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(previous) = previous_metadata.and_then(|value| value.as_object().cloned()) {
        for (key, value) in previous {
            merged.entry(key).or_insert(value);
        }
    }
    merged.insert("evolved_from".to_string(), serde_json::json!(evolved_from));
    merged.insert(
        "canonical_read_source".to_string(),
        serde_json::json!(source.as_str()),
    );
    serde_json::Value::Object(merged)
}

fn dedupe_results_by_id(results: &mut Vec<SearchResult>) {
    let mut seen = HashSet::new();
    results.retain(|result| seen.insert(result.id.clone()));
}

fn relation_summary(conn: &Connection, note_id: &str) -> (i64, Vec<String>) {
    if !table_exists(conn, "note_relations") {
        return (0, Vec::new());
    }

    let count = conn
        .query_row(
            "SELECT COUNT(*)
             FROM note_relations
             WHERE from_note_id = ?1 OR to_note_id = ?1",
            [note_id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0);

    let mut stmt = match conn.prepare(
        "SELECT DISTINCT relation_type
         FROM note_relations
         WHERE from_note_id = ?1 OR to_note_id = ?1
         ORDER BY relation_type ASC
         LIMIT 8",
    ) {
        Ok(s) => s,
        Err(_) => return (count, Vec::new()),
    };
    let types = stmt
        .query_map([note_id], |row| row.get::<_, String>(0))
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    (count, types)
}

#[derive(Debug)]
struct ActivationSeed {
    id: String,
    root_id: String,
    score: f64,
    path: Vec<String>,
}

#[derive(Debug)]
struct ActivationNeighbor {
    source_id: String,
    relation_type: String,
    strength: f64,
    dream_candidate_id: Option<String>,
    note: NoteRow,
}

#[derive(Debug)]
struct ReturnedActivationTrace {
    relation_type: String,
    depth: usize,
    dream_candidate_id: Option<String>,
}

fn activation_trace_from_metadata(
    metadata: Option<&serde_json::Value>,
) -> Option<ReturnedActivationTrace> {
    let meta = metadata?;
    if meta
        .get("is_association")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return None;
    }
    let relation_type = meta
        .get("relation_type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("association")
        .to_string();
    let depth = meta
        .get("activation_depth")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    let dream_candidate_id = meta
        .get("dream_candidate_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    Some(ReturnedActivationTrace {
        relation_type,
        depth,
        dream_candidate_id,
    })
}

fn hydrate_activation_neighbors(
    conn: &Connection,
    neighbors: &[ActivationNeighbor],
) -> HashMap<String, CanonicalRead> {
    let seeds = neighbors
        .iter()
        .map(|neighbor| CanonicalSeed {
            seed_id: neighbor.note.id.clone(),
            note_id: Some(neighbor.note.id.clone()),
            root_id: neighbor.note.root_id.clone(),
            topic_key: neighbor.note.topic_key.clone(),
        })
        .collect::<Vec<_>>();
    match hydrate_heads_by_canonical_or_topic(conn, &seeds) {
        Ok(reads) => reads,
        Err(error) => {
            warn!("spread_activation canonical hydration failed: {error}");
            HashMap::new()
        }
    }
}

fn activation_neighbors_for_source(conn: &Connection, source_id: &str) -> Vec<ActivationNeighbor> {
    let relation_placeholders = ACTIVATION_RELATION_TYPES
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT
             r.from_note_id AS activation_source_id,
             r.relation_type AS activation_relation_type,
             r.strength AS activation_strength,
             n.*
         FROM note_relations r
         JOIN notes n ON n.id = r.to_note_id
         WHERE r.from_note_id = ?
           AND r.relation_type IN ({relation_placeholders})
           AND r.strength > ?
        UNION ALL
        SELECT
             r.to_note_id AS activation_source_id,
             r.relation_type AS activation_relation_type,
             r.strength AS activation_strength,
             n.*
         FROM note_relations r
         JOIN notes n ON n.id = r.from_note_id
         WHERE r.to_note_id = ?
           AND r.relation_type IN ({relation_placeholders})
           AND r.strength > ?"
    );

    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    params.push(Box::new(source_id.to_string()));
    for relation_type in ACTIVATION_RELATION_TYPES {
        params.push(Box::new((*relation_type).to_string()));
    }
    params.push(Box::new(ACTIVATION_STRENGTH_THRESHOLD));
    params.push(Box::new(source_id.to_string()));
    for relation_type in ACTIVATION_RELATION_TYPES {
        params.push(Box::new((*relation_type).to_string()));
    }
    params.push(Box::new(ACTIVATION_STRENGTH_THRESHOLD));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(error) => {
            warn!("spread_activation prepare failed: {error}");
            return Vec::new();
        }
    };

    stmt.query_map(rusqlite::params_from_iter(param_refs.iter()), |row| {
        Ok(ActivationNeighbor {
            source_id: row.get("activation_source_id")?,
            relation_type: row.get("activation_relation_type")?,
            strength: row.get("activation_strength")?,
            dream_candidate_id: None,
            note: NoteRow::from_row(row)?,
        })
    })
    .map(|rows| {
        rows.filter_map(|row| match row {
            Ok(neighbor) => Some(neighbor),
            Err(error) => {
                warn!("Skipping activation neighbor: {error}");
                None
            }
        })
        .collect()
    })
    .unwrap_or_default()
}

fn dreaming_activation_neighbors_for_source(
    conn: &Connection,
    source_id: &str,
    project_id: Option<&str>,
    cross_project: bool,
) -> Vec<ActivationNeighbor> {
    let mut sql = String::from(
        "SELECT id, project_id, confidence, evidence_ids, promoted_to
         FROM dream_candidates
         WHERE verdict = 'promoted'
           AND promoted_to IS NOT NULL
           AND evidence_ids IS NOT NULL
           AND (promoted_to = ?1 OR evidence_ids LIKE ?2)",
    );
    let source_like = format!("%{source_id}%");
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
        vec![Box::new(source_id.to_string()), Box::new(source_like)];
    if !cross_project {
        if let Some(project_id) = project_id {
            sql.push_str(" AND project_id = ?3");
            params.push(Box::new(project_id.to_string()));
        }
    }
    sql.push_str(" ORDER BY confidence DESC, promoted_at DESC, created_at DESC LIMIT 50");
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(error) => {
            warn!("dreaming activation prepare failed: {error}");
            return Vec::new();
        }
    };

    let rows = match stmt.query_map(rusqlite::params_from_iter(param_refs.iter()), |row| {
        Ok((
            row.get::<_, String>("id")?,
            row.get::<_, f64>("confidence").unwrap_or(0.0),
            row.get::<_, Option<String>>("evidence_ids")
                .unwrap_or_default(),
            row.get::<_, Option<String>>("promoted_to")
                .unwrap_or_default(),
        ))
    }) {
        Ok(rows) => rows,
        Err(error) => {
            warn!("dreaming activation query failed: {error}");
            return Vec::new();
        }
    };

    let mut neighbors = Vec::new();
    for row in rows {
        let (candidate_id, confidence, evidence_ids_json, promoted_to) = match row {
            Ok(row) => row,
            Err(error) => {
                warn!("Skipping dreaming activation row: {error}");
                continue;
            }
        };
        let strength = confidence.clamp(0.0, 1.0);
        if strength <= ACTIVATION_STRENGTH_THRESHOLD {
            continue;
        }
        let Some(promoted_to) = promoted_to.filter(|id| !id.trim().is_empty()) else {
            continue;
        };
        let evidence_ids: Vec<String> = evidence_ids_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect();
        if evidence_ids.is_empty() {
            continue;
        }

        let mut target_ids = Vec::new();
        if promoted_to == source_id {
            target_ids.extend(
                evidence_ids
                    .iter()
                    .filter(|id| id.as_str() != source_id)
                    .cloned(),
            );
        }
        if evidence_ids.iter().any(|id| id == source_id) && promoted_to != source_id {
            target_ids.push(promoted_to);
        }

        for target_id in target_ids {
            if let Some(note) = note_by_id(conn, &target_id) {
                neighbors.push(ActivationNeighbor {
                    source_id: source_id.to_string(),
                    relation_type: DREAMING_EVIDENCE_RELATION_TYPE.to_string(),
                    strength,
                    dream_candidate_id: Some(candidate_id.clone()),
                    note,
                });
            }
        }
    }

    neighbors
}

fn note_by_id(conn: &Connection, note_id: &str) -> Option<NoteRow> {
    let mut stmt = match conn.prepare("SELECT n.* FROM notes n WHERE n.id = ?1 LIMIT 1") {
        Ok(stmt) => stmt,
        Err(error) => {
            warn!("activation note lookup prepare failed: {error}");
            return None;
        }
    };
    match stmt.query_row([note_id], NoteRow::from_row) {
        Ok(note) => Some(note),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => {
            warn!("activation note lookup failed for {note_id}: {error}");
            None
        }
    }
}

fn activation_note_passes_filters(
    conn: &Connection,
    note: &NoteRow,
    params: &SearchParams,
) -> bool {
    if !activation_source_is_trusted(conn, &note.id) {
        return false;
    }
    if params.only_active && !note.is_active {
        return false;
    }
    if !note.is_head {
        return false;
    }
    if !evolution_state_is_head(note.evolution_state.as_deref()) {
        return false;
    }
    if let Some(layer) = params.layer.as_deref() {
        if note.layer != layer {
            return false;
        }
    }
    if let Some(category) = params.category.as_deref() {
        if note.category.as_deref() != Some(category) {
            return false;
        }
    }
    if let Some(room) = params.room.as_deref() {
        if note.room.as_deref() != Some(room) {
            return false;
        }
    }
    if !params.cross_project {
        if let Some(project_id) = params.project_id.as_deref() {
            if note.project_id.as_deref() != Some(project_id) {
                return false;
            }
        }
    }
    if !params.include_constitution
        && params.layer.as_deref() != Some("identity_schema")
        && note.layer == "identity_schema"
    {
        return false;
    }

    true
}

fn activation_source_is_trusted(conn: &Connection, note_id: &str) -> bool {
    let source_expr = if column_exists(conn, "notes", "source") {
        "source"
    } else {
        "NULL"
    };
    let confirmed_expr = if column_exists(conn, "notes", "confirmed_at") {
        "confirmed_at"
    } else {
        "NULL"
    };
    let sql = format!("SELECT {source_expr}, {confirmed_expr} FROM notes WHERE id = ?1 LIMIT 1");
    conn.query_row(&sql, [note_id], |row| {
        let source: Option<String> = row.get(0)?;
        let confirmed_at: Option<i64> = row.get(1)?;
        Ok(allows_strengthening(
            source.as_deref(),
            confirmed_at.is_some(),
            true,
        ))
    })
    .unwrap_or(false)
}

/// Strip temporal markers from query to improve recall.
fn strip_temporal_markers(query: &str) -> String {
    let mut result = query.to_string();

    // Collect all markers, sorted by length (longest first)
    let mut markers: Vec<&str> = Vec::new();
    markers.extend_from_slice(HISTORY_MARKERS_ZH);
    markers.extend_from_slice(CURRENT_STATE_MARKERS_ZH);
    markers.extend_from_slice(HISTORY_MARKERS_EN);
    markers.extend_from_slice(CURRENT_STATE_MARKERS_EN);
    markers.sort_by_key(|b| std::cmp::Reverse(b.len()));

    for marker in markers {
        result = result.replace(marker, " ");
    }

    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Reciprocal Rank Fusion over BM25 and vector channels.
///
/// Only boosts candidates that appear in BOTH channels (dual-hit).
fn rrf_fusion(
    candidates: &[Candidate],
    _query_vector: Option<&[f32]>,
) -> std::collections::HashMap<String, f64> {
    use std::collections::HashMap;

    if candidates.is_empty() {
        return HashMap::new();
    }

    // Channel 1: BM25 ranked list
    let mut bm25_entries: Vec<(String, f64)> = Vec::new();
    for c in candidates {
        if let Some(bm25) = c.bm25_score {
            bm25_entries.push((c.note.id.clone(), (-bm25).max(0.0)));
        }
    }

    if bm25_entries.is_empty() {
        return HashMap::new();
    }

    bm25_entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut rrf_scores: HashMap<String, f64> = HashMap::new();
    for (rank, (cid, _)) in bm25_entries.iter().enumerate() {
        *rrf_scores.entry(cid.clone()).or_default() += 1.0 / (RRF_K + rank) as f64;
    }

    // Channel 2: Vector similarity ranked list
    let mut vec_ids: HashSet<String> = HashSet::new();
    if let Some(qv) = _query_vector {
        let mut vec_entries: Vec<(String, f64)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        for c in candidates {
            if !seen.insert(c.note.id.clone()) {
                continue;
            }
            if let Some(stored) = extract_vector(&c.note) {
                if stored.len() != qv.len() {
                    continue;
                }
                let sim = crate::retrieval::scoring::cosine_similarity(qv, &stored);
                if sim > 0.0 {
                    vec_entries.push((c.note.id.clone(), sim));
                }
            }
        }

        vec_entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (rank, (cid, _)) in vec_entries.iter().enumerate() {
            *rrf_scores.entry(cid.clone()).or_default() += 1.0 / (RRF_K + rank) as f64;
            vec_ids.insert(cid.clone());
        }
    }

    // Only keep dual-channel hits
    let bm25_ids: HashSet<String> = bm25_entries.into_iter().map(|(id, _)| id).collect();
    let dual_hits: HashSet<String> = bm25_ids.intersection(&vec_ids).cloned().collect();

    if dual_hits.is_empty() {
        return HashMap::new();
    }

    let mut dual_scores: HashMap<String, f64> = rrf_scores
        .into_iter()
        .filter(|(id, _)| dual_hits.contains(id))
        .collect();

    // Normalize to [0, 1]
    if let Some(max_score) = dual_scores.values().copied().reduce(f64::max) {
        if max_score > 0.0 {
            for score in dual_scores.values_mut() {
                *score /= max_score;
            }
        }
    }

    dual_scores
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::NoteRow;

    fn candidate(id: &str) -> Candidate {
        Candidate {
            note: NoteRow {
                id: id.to_string(),
                content: "fixture".to_string(),
                layer: "verified_fact".to_string(),
                category: None,
                vector_json: None,
                vector_blob: None,
                metadata_json: None,
                created_at: None,
                updated_at: None,
                is_active: true,
                agent_id: None,
                evolution_state: None,
                is_head: true,
                topic_key: None,
                root_id: None,
                version: Some(1),
                review_after: None,
                superseded_by: None,
                room: None,
                project_id: Some("alpha".to_string()),
                confidence: Some(0.9),
                recall_count: 0,
            },
            bm25_score: None,
        }
    }

    fn candidate_with_content(id: &str, content: &str, room: Option<&str>) -> Candidate {
        let mut candidate = candidate(id);
        candidate.note.content = content.to_string();
        candidate.note.room = room.map(str::to_string);
        candidate
    }

    fn result_with_score(id: &str, score: f64) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            content: "fixture".to_string(),
            layer: "verified_fact".to_string(),
            category: None,
            score,
            created_at: None,
            topic_key: None,
            root_id: None,
            version: Some(1),
            channel: None,
            temperature: None,
            is_head: true,
            metadata: None,
            recall_count: 0,
            last_recalled_at: None,
        }
    }

    #[test]
    fn project_entity_strong_filter_keeps_matching_candidates_when_available() {
        let candidates = vec![
            candidate_with_content("ming-shan", "AcmeOps ERP workflow editor", None),
            candidate_with_content("demoerp", "DemoERP ERP dealer finance context", None),
            candidate_with_content("generic", "ERP generic operational note", None),
        ];

        let filtered = apply_project_entity_strong_filter("DemoERP ERP", candidates);
        let ids = filtered
            .iter()
            .map(|candidate| candidate.note.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["ming-shan", "demoerp", "generic"]);
    }

    #[test]
    fn project_entity_strong_filter_falls_back_when_no_match_exists() {
        let candidates = vec![
            candidate_with_content("ming-shan", "AcmeOps ERP workflow editor", None),
            candidate_with_content("generic", "ERP generic operational note", None),
        ];

        let filtered = apply_project_entity_strong_filter("DemoERP ERP", candidates);
        let ids = filtered
            .iter()
            .map(|candidate| candidate.note.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["ming-shan", "generic"]);
    }

    #[test]
    fn project_entity_strong_filter_accepts_room_matches() {
        let candidates = vec![
            candidate_with_content("generic", "ERP operational note", None),
            candidate_with_content("room-match", "dealer workflow", Some("demoerp-erp")),
        ];

        let filtered = apply_project_entity_strong_filter("DemoERP ERP", candidates);
        let ids = filtered
            .iter()
            .map(|candidate| candidate.note.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec!["generic", "room-match"]);
    }

    #[test]
    fn entity_namespace_prepass_returns_matching_fact_for_single_token_entity() {
        let engine = make_engine();
        insert_search_note(
            &engine,
            SearchNoteFixture {
                id: "acmeops",
                content: "[FACT] AcmeOps ERP workflow editor context.",
                root_id: None,
                topic_key: None,
                is_head: 1,
                evolution_state: "active",
                created_at: "2026-05-17T00:00:00Z",
                metadata: None,
                is_active: 1,
            },
        );
        insert_search_note(
            &engine,
            SearchNoteFixture {
                id: "demoerp",
                content: "[FACT] DemoERP ERP dealer finance context.",
                root_id: None,
                topic_key: None,
                is_head: 1,
                evolution_state: "active",
                created_at: "2026-05-17T00:00:01Z",
                metadata: None,
                is_active: 1,
            },
        );

        let results = engine.search(&SearchParams {
            query: "DemoERP".to_string(),
            limit: 5,
            only_active: true,
            project_id: Some("alpha".to_string()),
            min_score: 0.2,
            ..Default::default()
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "demoerp");
        assert_eq!(results[0].channel.as_deref(), Some("semantic"));
    }

    #[test]
    fn entity_namespace_prepass_respects_non_entity_query_terms() {
        let engine = make_engine();
        insert_search_note(
            &engine,
            SearchNoteFixture {
                id: "demoerp-generic",
                content: "[FACT] DemoERP ERP dealer finance context.",
                root_id: None,
                topic_key: None,
                is_head: 1,
                evolution_state: "active",
                created_at: "2026-05-17T00:00:00Z",
                metadata: None,
                is_active: 1,
            },
        );
        insert_search_note(
            &engine,
            SearchNoteFixture {
                id: "demoerp-login",
                content: "[FACT] DemoERP ERP login troubleshooting keeps credential stability.",
                root_id: None,
                topic_key: None,
                is_head: 1,
                evolution_state: "active",
                created_at: "2026-05-17T00:00:01Z",
                metadata: None,
                is_active: 1,
            },
        );

        let results = engine.search(&SearchParams {
            query: "DemoERP login".to_string(),
            limit: 5,
            only_active: true,
            project_id: Some("alpha".to_string()),
            min_score: 0.2,
            ..Default::default()
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "demoerp-login");
        assert_eq!(results[0].channel.as_deref(), Some("lexical"));
    }

    #[test]
    fn z_score_normalization_spreads_clustered_scores() {
        let mut results = vec![
            result_with_score("a", 0.90),
            result_with_score("b", 0.91),
            result_with_score("c", 0.92),
        ];

        normalize_result_scores(&mut results, ScoreNormalizationMode::Z);

        assert!(
            results[0].score < results[1].score && results[1].score < results[2].score,
            "normalization must preserve score ordering: {results:?}"
        );
        assert!(
            results[2].score - results[0].score > 0.30,
            "clustered scores should become visibly separated: {results:?}"
        );
    }

    #[test]
    fn z_score_normalization_leaves_flat_scores_unchanged() {
        let mut results = vec![result_with_score("a", 0.90), result_with_score("b", 0.90)];

        normalize_result_scores(&mut results, ScoreNormalizationMode::Z);

        assert_eq!(results[0].score, 0.90);
        assert_eq!(results[1].score, 0.90);
    }

    #[test]
    fn lexical_hard_match_bonus_rewards_literal_multi_term_hits() {
        let boosted = apply_lexical_hard_match_bonus(
            "DemoERP ERP",
            "[FACT] DemoERP ERP dealer finance context",
            0.52,
        );
        let unchanged =
            apply_lexical_hard_match_bonus("DemoERP ERP", "[FACT] AcmeOps ERP workflow context", 0.52);

        assert!((boosted - 0.62).abs() < f64::EPSILON);
        assert_eq!(unchanged, 0.52);
    }

    #[test]
    fn missing_keyword_penalty_demotes_partial_multi_term_hits() {
        let penalized =
            apply_missing_keyword_penalty("DemoERP ERP", "[FACT] AcmeOps ERP workflow context", 0.52);
        let unchanged = apply_missing_keyword_penalty(
            "DemoERP ERP",
            "[FACT] DemoERP ERP dealer finance context",
            0.52,
        );

        assert!((penalized - 0.47).abs() < f64::EPSILON);
        assert_eq!(unchanged, 0.52);
    }

    fn recall_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory recall DB");
        conn.execute_batch(
            "CREATE TABLE recall_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                note_id TEXT NOT NULL,
                recalled_at TEXT NOT NULL,
                session_id TEXT,
                query_text TEXT
            );",
        )
        .expect("recall_log schema");
        conn
    }

    #[test]
    fn latest_recall_timestamps_preserves_subsecond_ordering() {
        let conn = recall_conn();
        conn.execute(
            "INSERT INTO recall_log (note_id, recalled_at) VALUES
             ('note-1', '2026-04-10T00:00:00.100000Z'),
             ('note-1', '2026-04-10T00:00:00.900000+00:00')",
            [],
        )
        .expect("insert recalls");

        let map = latest_recall_timestamps(&conn, &[candidate("note-1")])
            .expect("valid recall timestamps");
        // RFC3339 input is forwarded verbatim to preserve Python parity —
        // `get_recall_stats` returns stored text as-is via `MAX(recalled_at)`.
        assert_eq!(
            map.get("note-1").map(String::as_str),
            Some("2026-04-10T00:00:00.900000+00:00")
        );
    }

    #[test]
    fn latest_recall_timestamps_skips_malformed_row_without_wiping_batch() {
        // T10/T11 established fail-loud for malformed timestamps, but that
        // policy fits cold reporting paths (dream_stats), not the search hot
        // path. Here, one weird recall_at would otherwise wipe recall
        // hydration for every note in the batch — far worse than just
        // dropping the offending row. Verify the new policy: bad row is
        // logged + skipped, good rows still hydrate.
        let conn = recall_conn();
        conn.execute(
            "INSERT INTO recall_log (id, note_id, recalled_at) VALUES
             (42, 'note-bad', 'not-a-date'),
             (43, 'note-good', '2026-04-14T17:40:08Z')",
            [],
        )
        .expect("insert mixed good/bad recalls");

        let map = latest_recall_timestamps(&conn, &[candidate("note-bad"), candidate("note-good")])
            .expect("malformed row must not propagate as Err");

        assert!(
            !map.contains_key("note-bad"),
            "malformed row must be skipped, got: {:?}",
            map.get("note-bad")
        );
        assert_eq!(
            map.get("note-good").map(String::as_str),
            Some("2026-04-14T17:40:08Z"),
            "good row must still hydrate even when sibling row is malformed (verbatim `Z`)"
        );
    }

    #[test]
    fn latest_recall_timestamps_quarantines_note_when_newest_row_fails_parse() {
        // Codex adversarial review on a759fc9 found this: with the previous
        // fail-open + or_insert design, a note whose newest row was unparseable
        // (e.g. SQLite-weird offset format) would silently fall through to
        // its older parseable sibling. Downstream recall scoring would then
        // see a STALE `last_recalled_at` while the function still claimed
        // success — a real ranking bug, not graceful degradation.
        //
        // New contract: once any note's newest-by-julianday row fails parse,
        // that note is quarantined; older siblings MUST NOT backfill it.
        let conn = recall_conn();
        conn.execute(
            "INSERT INTO recall_log (note_id, recalled_at) VALUES
             ('note-tainted', '2026-04-14T10:00:00Z'),
             ('note-tainted', '2026-04-14 17:40:08.123 +00:00'),
             ('note-clean',   '2026-04-14T12:00:00Z')",
            [],
        )
        .expect("insert tainted + clean recalls");

        let map =
            latest_recall_timestamps(&conn, &[candidate("note-tainted"), candidate("note-clean")])
                .expect("Result must propagate Ok even with tainted note");

        assert!(
            !map.contains_key("note-tainted"),
            "tainted note must NOT fall through to older parseable sibling, got: {:?}",
            map.get("note-tainted")
        );
        assert_eq!(
            map.get("note-clean").map(String::as_str),
            Some("2026-04-14T12:00:00Z"),
            "unrelated clean note must still hydrate (verbatim `Z`)"
        );
    }

    #[test]
    fn latest_recall_timestamps_quarantines_when_newest_by_id_is_unparseable_by_sqlite() {
        // Codex adversarial review on dce9426 found this: julianday('not-a-date')
        // returns NULL, and SQL DESC sorted NULL after valid rows. The previous
        // fix relied on julianday() ordering, so a *newer* malformed row got
        // visited AFTER an older valid sibling, the loop inserted the older
        // row into `out`, and the bad newer row was silently skipped via
        // contains_key — leaking stale `last_recalled_at` to scoring.
        //
        // The current fix changes ORDER BY to `id DESC` (insertion order is
        // monotonic and never NULL), so the latest event is ALWAYS visited
        // first regardless of timestamp text validity. This test exercises
        // exactly the Codex scenario: older valid (id=99) + newer malformed
        // (id=100). The note must quarantine, NOT silently fall back.
        let conn = recall_conn();
        conn.execute(
            "INSERT INTO recall_log (id, note_id, recalled_at) VALUES
             (99, 'note-stale-leak', '2026-04-14T10:00:00Z'),
             (100, 'note-stale-leak', 'not-a-date')",
            [],
        )
        .expect("insert older-valid + newer-malformed pair");

        let map = latest_recall_timestamps(&conn, &[candidate("note-stale-leak")])
            .expect("Result must be Ok even when newest row is malformed");

        assert!(
            !map.contains_key("note-stale-leak"),
            "newer malformed row must quarantine the note even though an older parseable sibling exists; got stale fallback: {:?}",
            map.get("note-stale-leak")
        );
    }

    #[test]
    fn latest_recall_timestamps_skips_sqlite_weird_offset_without_wiping_batch() {
        // Codex's second adversarial pass on this PR found SQLite accepts
        // `YYYY-MM-DD HH:MM:SS.fff +00:00` (space before offset) — a shape
        // chrono RFC3339 rejects and our naive parser also rejects.
        // julianday() still ranks it, so it can be selected as newest. Under
        // the old fail-loud policy that would wipe the whole batch. Verify
        // the per-row skip policy degrades gracefully instead.
        let conn = recall_conn();
        conn.execute(
            "INSERT INTO recall_log (note_id, recalled_at) VALUES
             ('note-sibling', '2026-04-14T15:00:00Z'),
             ('note-weird',   '2026-04-14 17:40:08.123 +00:00')",
            [],
        )
        .expect("insert SQLite weird offset alongside normal row");

        let map =
            latest_recall_timestamps(&conn, &[candidate("note-weird"), candidate("note-sibling")])
                .expect("weird format must not propagate as Err");

        assert_eq!(
            map.get("note-sibling").map(String::as_str),
            Some("2026-04-14T15:00:00Z"),
            "sibling row must hydrate even when SQLite-weird row is skipped (verbatim `Z`)"
        );
        // note-weird may or may not appear depending on parser future expansion;
        // the hot-path contract is just "do not wipe the batch".
    }

    #[test]
    fn latest_recall_timestamps_accepts_sqlite_default_format() {
        // `backend/services/storage_recall.py` inserts into `recall_log` WITHOUT
        // supplying `recalled_at`, so the column receives the SQLite DEFAULT
        // `datetime('now')`, which produces `YYYY-MM-DD HH:MM:SS` — *not* RFC3339.
        // Every existing production row in user's DB is in this shape. If the
        // Rust hydrator rejected it, every search would silently lose recall
        // scoring and `last_recalled_at` would always come back null.
        let conn = recall_conn();
        conn.execute(
            "INSERT INTO recall_log (note_id, recalled_at) VALUES
             ('note-1', '2026-04-14 17:39:02'),
             ('note-1', '2026-04-14 17:40:08')",
            [],
        )
        .expect("insert SQLite-default recalls");

        let map = latest_recall_timestamps(&conn, &[candidate("note-1")])
            .expect("SQLite datetime('now') format must parse");
        // Stored as SQLite default (`YYYY-MM-DD HH:MM:SS`); emitted as
        // canonical RFC3339 so postprocess::add_temperature_labels doesn't
        // reject it and mislabel every SQLite-default row as cold.
        assert_eq!(
            map.get("note-1").map(String::as_str),
            Some("2026-04-14T17:40:08+00:00"),
            "newest SQLite-default row must win, normalized to RFC3339"
        );
    }

    #[test]
    fn latest_recall_timestamps_accepts_sqlite_subsecond_default() {
        // Codex adversarial review on 6b1a90d caught this: ORDER BY uses
        // `julianday(recalled_at)`, which happily accepts
        // `YYYY-MM-DD HH:MM:SS.fff`. If parse_recall_timestamp rejects that
        // shape, a row written by `datetime('now', 'subsec')` (or any explicit
        // sub-second SQLite-style writer) survives the SQL ORDER BY, gets
        // selected as the newest, then triggers Err — wiping the whole
        // candidate-batch hydration map and reproducing the original P1.
        let conn = recall_conn();
        conn.execute(
            "INSERT INTO recall_log (note_id, recalled_at) VALUES
             ('note-1', '2026-04-14 17:40:08'),
             ('note-1', '2026-04-14 17:40:08.123')",
            [],
        )
        .expect("insert SQLite-default subsecond recall");

        let map = latest_recall_timestamps(&conn, &[candidate("note-1")])
            .expect("SQLite subsecond format must parse");
        assert_eq!(
            map.get("note-1").map(String::as_str),
            Some("2026-04-14T17:40:08.123+00:00"),
            "subsecond winner must hydrate (canonical RFC3339), not be silently rejected"
        );
    }

    #[test]
    fn latest_recall_timestamps_mix_sqlite_default_and_rfc3339() {
        // Exercise the mixed-format case that will exist during any migration
        // window: historical rows use SQLite default, new rows emit RFC3339.
        // julianday() normalizes both, so ordering must still pick the newest
        // regardless of which writer produced it.
        let conn = recall_conn();
        conn.execute(
            "INSERT INTO recall_log (note_id, recalled_at) VALUES
             ('note-1', '2026-04-14 17:00:00'),
             ('note-1', '2026-04-14T18:00:00Z')",
            [],
        )
        .expect("insert mixed-format recalls");

        let map = latest_recall_timestamps(&conn, &[candidate("note-1")])
            .expect("mixed SQLite-default + RFC3339 must parse");
        assert_eq!(
            map.get("note-1").map(String::as_str),
            Some("2026-04-14T18:00:00Z"),
            "newer RFC3339 row wins AND its original suffix (`Z`) is preserved verbatim for Python parity"
        );
    }

    #[test]
    fn latest_recall_timestamps_preserves_rfc3339_verbatim_for_python_parity() {
        // Codex review on 756533d flagged: `MAX(recalled_at)` in
        // `backend/services/storage_recall.py::get_recall_stats` returns the
        // stored text verbatim. If the Rust hydrator re-formats an
        // already-valid RFC3339 string via `to_rfc3339()`, `...Z` becomes
        // `...+00:00` and microsecond precision `.900000` collapses to
        // `.900`. That is a response-surface parity regression for any
        // caller comparing or snapshotting raw MCP payloads.
        let conn = recall_conn();
        conn.execute(
            "INSERT INTO recall_log (note_id, recalled_at) VALUES
             ('note-zulu', '2026-04-14T17:40:08.123456Z'),
             ('note-offset', '2026-04-14T17:40:08.900000+00:00')",
            [],
        )
        .expect("insert verbatim RFC3339 recalls");

        let map =
            latest_recall_timestamps(&conn, &[candidate("note-zulu"), candidate("note-offset")])
                .expect("RFC3339 verbatim must parse");

        assert_eq!(
            map.get("note-zulu").map(String::as_str),
            Some("2026-04-14T17:40:08.123456Z"),
            "`Z` suffix must not be rewritten to `+00:00`"
        );
        assert_eq!(
            map.get("note-offset").map(String::as_str),
            Some("2026-04-14T17:40:08.900000+00:00"),
            "microsecond precision (`.900000`) must not collapse to `.900`"
        );
    }

    #[test]
    fn evolution_state_is_head_enumerates_all_known_states() {
        // Python parity + Codex P2 on PR #181:
        // `mark_superseded` writes `is_head = 0` for both `superseded`
        // and `contradicted` rows. Search result derivation must stay
        // aligned so those rows never surface with is_head=true.
        // NULL / active / any unknown state defaults to head.
        assert!(
            evolution_state_is_head(None),
            "NULL evolution_state falls back to head (new writes)"
        );
        assert!(
            evolution_state_is_head(Some("active")),
            "active rows are always head"
        );
        assert!(
            !evolution_state_is_head(Some("superseded")),
            "superseded rows must never be head"
        );
        assert!(
            !evolution_state_is_head(Some("contradicted")),
            "contradicted rows must never be head (regression guard)"
        );
    }

    #[test]
    fn lexical_first_detects_literal_multi_term_queries() {
        assert!(should_try_lexical_first("mission control"));
        assert!(literal_terms_match(
            "mission control",
            "[FACT] Mission Control rollout gates use getDb().transaction"
        ));
        assert!(literal_terms_match(
            "DemoERP ERP",
            "[FACT] DemoERP ERP 大客列表/门店概念群聊拍板隐藏"
        ));
        assert!(!should_try_lexical_first("anchor"));
        assert!(!literal_terms_match("mission control", "unrelated rollout"));
    }

    // -----------------------------------------------------------------------
    // active_sessions tests (TODO-IMPL-01 PR3/3 = B)
    // -----------------------------------------------------------------------

    /// Create an in-memory `SearchEngine` with the full schema (notes + sessions).
    fn make_engine() -> SearchEngine {
        let pool =
            crate::storage::db::DbPool::open_in_memory().expect("open_in_memory should succeed");
        SearchEngine::new(pool)
    }

    /// Insert a session row directly into the pool's connection.
    fn insert_session(
        engine: &SearchEngine,
        session_id: &str,
        agent_label: Option<&str>,
        first_seen: &str,
        last_seen: &str,
    ) {
        insert_session_with_project(engine, session_id, agent_label, None, first_seen, last_seen);
    }

    /// Insert a session row with explicit `project_id` for isolation tests.
    fn insert_session_with_project(
        engine: &SearchEngine,
        session_id: &str,
        agent_label: Option<&str>,
        project_id: Option<&str>,
        first_seen: &str,
        last_seen: &str,
    ) {
        engine.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO sessions (session_id, agent_label, project_id, first_seen, last_seen)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![session_id, agent_label, project_id, first_seen, last_seen],
            )
            .expect("insert_session fixture");
        });
    }

    struct ContextNoteFixture<'a> {
        id: &'a str,
        layer: &'a str,
        content: &'a str,
        project_id: &'a str,
        created_at: &'a str,
        metadata: Option<serde_json::Value>,
        review_after: Option<&'a str>,
    }

    fn insert_context_note(engine: &SearchEngine, note: ContextNoteFixture<'_>) {
        engine.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO notes
                 (id, content, layer, category, is_active, confidence, project_id, created_at, updated_at, metadata_json, review_after)
                 VALUES (?1, ?2, ?3, 'workflow', 1, 0.92, ?4, ?5, ?5, ?6, ?7)",
                rusqlite::params![
                    note.id,
                    note.content,
                    note.layer,
                    note.project_id,
                    note.created_at,
                    note.metadata.map(|value| value.to_string()),
                    note.review_after,
                ],
            )
            .expect("insert_context_note fixture");
        });
    }

    struct SearchNoteFixture<'a> {
        id: &'a str,
        content: &'a str,
        root_id: Option<&'a str>,
        topic_key: Option<&'a str>,
        is_head: i64,
        evolution_state: &'a str,
        created_at: &'a str,
        metadata: Option<serde_json::Value>,
        is_active: i64,
    }

    fn insert_search_note(engine: &SearchEngine, note: SearchNoteFixture<'_>) {
        engine.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO notes
                 (id, content, layer, category, is_active, confidence, project_id, created_at, updated_at, metadata_json, root_id, topic_key, is_head, evolution_state, source)
                 VALUES (?1, ?2, 'verified_fact', 'workflow', ?3, 0.95, 'alpha', ?4, ?4, ?5, ?6, ?7, ?8, ?9, 'user')",
                rusqlite::params![
                    note.id,
                    note.content,
                    note.is_active,
                    note.created_at,
                    note.metadata.map(|value| value.to_string()),
                    note.root_id,
                    note.topic_key,
                    note.is_head,
                    note.evolution_state,
                ],
            )
            .expect("insert_search_note fixture");
            conn.execute(
                "INSERT INTO notes_fts (note_id, content) VALUES (?1, ?2)",
                rusqlite::params![note.id, note.content],
            )
            .expect("insert_search_note fts");
        });
    }

    fn insert_note_relation(engine: &SearchEngine, from_note_id: &str, to_note_id: &str) {
        engine.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO note_relations
                 (from_note_id, to_note_id, relation_type, strength)
                 VALUES (?1, ?2, 'supports', 0.9)",
                rusqlite::params![from_note_id, to_note_id],
            )
            .expect("insert_note_relation fixture");
        });
    }

    #[test]
    fn direct_fact_prefix_lookup_returns_exact_fact() {
        let engine = make_engine();
        insert_search_note(
            &engine,
            SearchNoteFixture {
                id: "north-star",
                content: "[FACT] alice 北极星: Memra should remember context.",
                root_id: None,
                topic_key: None,
                is_head: 1,
                evolution_state: "active",
                created_at: "2026-05-17T00:00:00Z",
                metadata: None,
                is_active: 1,
            },
        );

        let results = engine.search(&SearchParams {
            query: "alice 北极星".to_string(),
            limit: 5,
            only_active: true,
            project_id: Some("alpha".to_string()),
            min_score: 0.2,
            ..Default::default()
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "north-star");
        assert_eq!(results[0].channel.as_deref(), Some("lexical"));
        assert!(results[0].score > 0.99);
    }

    #[test]
    fn search_follows_canonical_root_when_query_hits_tail() {
        let engine = make_engine();
        insert_search_note(
            &engine,
            SearchNoteFixture {
                id: "tail-v1",
                content: "缓存策略采用 LRU 淘汰",
                root_id: Some("root-cache"),
                topic_key: Some("cache-policy-v1"),
                is_head: 0,
                evolution_state: "active",
                created_at: "2026-04-20T00:00:01Z",
                metadata: Some(serde_json::json!({"superseded_by": "head-v2"})),
                is_active: 1,
            },
        );
        insert_search_note(
            &engine,
            SearchNoteFixture {
                id: "head-v2",
                content: "缓存策略改为 LFU 淘汰",
                root_id: Some("root-cache"),
                topic_key: Some("cache-policy-v2"),
                is_head: 1,
                evolution_state: "active",
                created_at: "2026-04-20T00:00:02Z",
                metadata: None,
                is_active: 1,
            },
        );

        let results = engine.search(&SearchParams {
            query: "LRU".to_string(),
            limit: 5,
            only_active: true,
            project_id: Some("alpha".to_string()),
            search_mode: Some("lexical".to_string()),
            min_score: 0.0,
            ..Default::default()
        });

        assert!(!results.is_empty());
        assert_eq!(results[0].id, "head-v2");
        assert_eq!(
            results[0]
                .metadata
                .as_ref()
                .and_then(|meta| meta.get("evolved_from")),
            Some(&serde_json::json!("tail-v1"))
        );
        assert_eq!(
            results[0]
                .metadata
                .as_ref()
                .and_then(|meta| meta.get("canonical_read_source")),
            Some(&serde_json::json!("canonical"))
        );
    }

    #[test]
    fn spread_activation_hydrates_canonical_head_for_stale_neighbor() {
        let engine = make_engine();
        insert_search_note(
            &engine,
            SearchNoteFixture {
                id: "hub",
                content: "Hub note for parallel activation",
                root_id: Some("hub-root"),
                topic_key: Some("hub-topic"),
                is_head: 1,
                evolution_state: "active",
                created_at: "2026-04-20T00:00:00Z",
                metadata: None,
                is_active: 1,
            },
        );
        insert_search_note(
            &engine,
            SearchNoteFixture {
                id: "stale-neighbor",
                content: "stale related note that should not surface",
                root_id: Some("neighbor-root"),
                topic_key: Some("neighbor-topic-v1"),
                is_head: 0,
                evolution_state: "superseded",
                created_at: "2026-04-20T00:00:01Z",
                metadata: None,
                is_active: 0,
            },
        );
        insert_search_note(
            &engine,
            SearchNoteFixture {
                id: "head-neighbor",
                content: "fresh related note only reachable through canonical hydration",
                root_id: Some("neighbor-root"),
                topic_key: Some("neighbor-topic-v2"),
                is_head: 1,
                evolution_state: "active",
                created_at: "2026-04-20T00:00:02Z",
                metadata: None,
                is_active: 1,
            },
        );
        insert_note_relation(&engine, "hub", "stale-neighbor");

        let (results, diagnostics) = engine.search_with_diagnostics(&SearchParams {
            query: "Hub note".to_string(),
            limit: 10,
            only_active: true,
            project_id: Some("alpha".to_string()),
            search_mode: Some("lexical".to_string()),
            min_score: 0.0,
            ..Default::default()
        });

        assert!(results.iter().any(|result| result.id == "head-neighbor"));
        assert!(!results.iter().any(|result| result.id == "stale-neighbor"));
        let association = results
            .iter()
            .find(|result| result.id == "head-neighbor")
            .expect("hydrated association");
        assert_eq!(association.channel.as_deref(), Some("association"));
        assert!(association.is_head);
        assert_eq!(
            association
                .metadata
                .as_ref()
                .and_then(|meta| meta.get("canonical_read_source")),
            Some(&serde_json::json!("canonical"))
        );
        assert_eq!(
            association
                .metadata
                .as_ref()
                .and_then(|meta| meta.get("evolved_from")),
            Some(&serde_json::json!("stale-neighbor"))
        );
        assert!(diagnostics.activation_result_count >= 1);
    }

    #[test]
    fn full_context_surfaces_core_sections_and_project_scope() {
        let engine = make_engine();
        let now = chrono::Utc::now();
        let ts = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let review_due = (now - chrono::Duration::days(1))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();

        insert_context_note(
            &engine,
            ContextNoteFixture {
                id: "ident-1",
                layer: "identity_schema",
                content: "alice prefers heavy validation on lab-m1",
                project_id: "alpha",
                created_at: &ts,
                metadata: None,
                review_after: None,
            },
        );
        insert_context_note(
            &engine,
            ContextNoteFixture {
                id: "fact-1",
                layer: "verified_fact",
                content: "context full snapshot parity is active",
                project_id: "alpha",
                created_at: &ts,
                metadata: None,
                review_after: None,
            },
        );
        insert_context_note(
            &engine,
            ContextNoteFixture {
                id: "event-1",
                layer: "event_log",
                content: "R2 context full implementation started",
                project_id: "alpha",
                created_at: &ts,
                metadata: None,
                review_after: None,
            },
        );
        insert_context_note(
            &engine,
            ContextNoteFixture {
                id: "proc-1",
                layer: "procedure_schema",
                content: "R2 validation SOP",
                project_id: "alpha",
                created_at: &ts,
                metadata: Some(serde_json::json!({"steps": ["light checks", "lab-m1 CI"]})),
                review_after: None,
            },
        );
        insert_context_note(
            &engine,
            ContextNoteFixture {
                id: "cp-1",
                layer: "event_log",
                content: "[断点] context-full: implement Rust full snapshot",
                project_id: "alpha",
                created_at: &ts,
                metadata: Some(serde_json::json!({
                    "record_type": "checkpoint",
                    "task_id": "context-full",
                    "task_status": "in_progress",
                    "next_step": "run lab-m1 validation",
                    "living_docs": ["ACTIVE.md"]
                })),
                review_after: None,
            },
        );
        insert_context_note(
            &engine,
            ContextNoteFixture {
                id: "review-1",
                layer: "verified_fact",
                content: "memory that needs review",
                project_id: "alpha",
                created_at: &ts,
                metadata: None,
                review_after: Some(&review_due),
            },
        );
        insert_context_note(
            &engine,
            ContextNoteFixture {
                id: "failed-1",
                layer: "verified_fact",
                content: "outdated memory should surface in failed memories",
                project_id: "alpha",
                created_at: &ts,
                metadata: Some(serde_json::json!({"is_outdated": true, "failure_count": 2})),
                review_after: None,
            },
        );
        insert_context_note(
            &engine,
            ContextNoteFixture {
                id: "other-project",
                layer: "verified_fact",
                content: "beta should not leak",
                project_id: "beta",
                created_at: &ts,
                metadata: None,
                review_after: None,
            },
        );
        insert_context_note(
            &engine,
            ContextNoteFixture {
                id: "superseded-alpha",
                layer: "verified_fact",
                content: "superseded alpha should not leak",
                project_id: "alpha",
                created_at: &ts,
                metadata: None,
                review_after: None,
            },
        );
        engine.db.with_conn(|conn| {
            conn.execute(
                "UPDATE notes SET evolution_state = 'superseded' WHERE id = 'superseded-alpha'",
                [],
            )
            .expect("mark superseded fixture");
        });

        let snap = engine.get_context_full_for_project(Some("alpha"));

        assert_eq!(snap["mode"], serde_json::json!("full"));
        assert_eq!(snap["project_id"], serde_json::json!("alpha"));
        assert_eq!(snap["stats"]["active_checkpoints"], serde_json::json!(1));
        assert_eq!(
            snap["active_task_states"][0]["task_id"],
            serde_json::json!("context-full")
        );
        assert!(snap["summary"].as_str().unwrap().contains("context-full"));
        assert!(
            snap["key_entities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["content"].as_str().unwrap().contains("lab-m1"))
        );
        assert!(
            snap["recent_events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["content"]
                    .as_str()
                    .unwrap()
                    .contains("implementation started"))
        );
        assert_eq!(snap["active_procedures"].as_array().unwrap().len(), 1);
        assert_eq!(snap["needs_review"].as_array().unwrap().len(), 1);
        assert_eq!(snap["failed_memories"].as_array().unwrap().len(), 1);
        assert!(
            !serde_json::to_string(&snap)
                .unwrap()
                .contains("beta should not leak")
        );
        assert!(
            !serde_json::to_string(&snap)
                .unwrap()
                .contains("superseded alpha should not leak")
        );
    }

    #[test]
    fn active_sessions_returns_recent_only() {
        // Insert 3 sessions: now, now-3min, now-10min.
        // Only the first 2 should appear in get_context_wake (5-min window).
        let engine = make_engine();
        let now = chrono::Utc::now();
        let ts_now = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let ts_3min = (now - chrono::Duration::minutes(3))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        let ts_10min = (now - chrono::Duration::minutes(10))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();

        insert_session(&engine, "sess-now", None, &ts_now, &ts_now);
        insert_session(&engine, "sess-3min", None, &ts_3min, &ts_3min);
        insert_session(&engine, "sess-10min", None, &ts_10min, &ts_10min);

        let snap = engine.get_context_wake();
        let sessions = snap["active_sessions"]
            .as_array()
            .expect("active_sessions must be array");
        assert_eq!(
            sessions.len(),
            2,
            "only now and 3-min-ago sessions should be active"
        );

        let ids: Vec<&str> = sessions
            .iter()
            .filter_map(|s| s["session_id"].as_str())
            .collect();
        assert!(ids.contains(&"sess-now"), "sess-now must be present");
        assert!(ids.contains(&"sess-3min"), "sess-3min must be present");
        assert!(
            !ids.contains(&"sess-10min"),
            "sess-10min must NOT be present"
        );
    }

    #[test]
    fn active_sessions_orders_by_last_seen_desc() {
        // Insert 3 sessions in non-chronological order; verify DESC ordering.
        let engine = make_engine();
        let now = chrono::Utc::now();
        let ts_a = (now - chrono::Duration::minutes(4))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        let ts_b = (now - chrono::Duration::minutes(1))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        let ts_c = (now - chrono::Duration::minutes(2))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();

        // Insert in non-sorted order: a(oldest active), c, b(newest active)
        insert_session(&engine, "sess-a", None, &ts_a, &ts_a);
        insert_session(&engine, "sess-c", None, &ts_c, &ts_c);
        insert_session(&engine, "sess-b", None, &ts_b, &ts_b);

        let snap = engine.get_context_wake();
        let sessions = snap["active_sessions"]
            .as_array()
            .expect("active_sessions must be array");
        assert_eq!(sessions.len(), 3, "all 3 sessions are within 5 min");

        // Verify descending last_seen: b > c > a
        let ids: Vec<&str> = sessions
            .iter()
            .filter_map(|s| s["session_id"].as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["sess-b", "sess-c", "sess-a"],
            "must be DESC by last_seen"
        );
    }

    #[test]
    fn active_sessions_handles_missing_table() {
        // Open a DbPool whose connection has NO sessions table.
        // get_context_wake must return an empty active_sessions array (no panic/error).
        let pool = {
            // Use rusqlite Connection directly; wrap in DbPool via open_in_memory
            // then drop the sessions table so we simulate a legacy DB.
            let p = crate::storage::db::DbPool::open_in_memory().expect("in-memory pool");
            p.with_conn(|conn| {
                conn.execute_batch("DROP TABLE IF EXISTS sessions;")
                    .expect("drop sessions for legacy-DB test");
            });
            p
        };
        let engine = SearchEngine::new(pool);
        let snap = engine.get_context_wake();
        let sessions = snap["active_sessions"]
            .as_array()
            .expect("active_sessions must be present as empty array");
        assert!(
            sessions.is_empty(),
            "missing sessions table must yield empty list"
        );
    }

    #[test]
    fn active_sessions_includes_agent_label() {
        // A session written with agent_label="claude-code" must surface that label.
        let engine = make_engine();
        let now = chrono::Utc::now();
        let ts = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        insert_session(&engine, "sess-labeled", Some("claude-code"), &ts, &ts);

        let snap = engine.get_context_wake();
        let sessions = snap["active_sessions"]
            .as_array()
            .expect("active_sessions must be array");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session_id"].as_str(), Some("sess-labeled"),);
        assert_eq!(
            sessions[0]["agent_label"].as_str(),
            Some("claude-code"),
            "agent_label must be surfaced in active_sessions"
        );
    }

    #[test]
    fn active_sessions_scoped_to_project_id() {
        // PR #232 Codex P1 regression: when get_context_wake_for_project is
        // called with a project_id, active_sessions must include ONLY the
        // sessions whose project_id matches. Cross-project leak = isolation
        // breach.
        let engine = make_engine();
        let now = chrono::Utc::now();
        let ts = now.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();

        insert_session_with_project(
            &engine,
            "sess-proj-a",
            Some("claude-code"),
            Some("project-a"),
            &ts,
            &ts,
        );
        insert_session_with_project(
            &engine,
            "sess-proj-b",
            Some("codex"),
            Some("project-b"),
            &ts,
            &ts,
        );
        // Legacy session with NULL project_id (pre-migration data).
        insert_session_with_project(&engine, "sess-legacy", None, None, &ts, &ts);

        // Project-scoped query: only project-a session visible.
        let snap_a = engine.get_context_wake_for_project(Some("project-a"));
        let a_ids: Vec<&str> = snap_a["active_sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["session_id"].as_str())
            .collect();
        assert_eq!(
            a_ids,
            vec!["sess-proj-a"],
            "project-a query must include ONLY sess-proj-a"
        );

        let snap_b = engine.get_context_wake_for_project(Some("project-b"));
        let b_ids: Vec<&str> = snap_b["active_sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["session_id"].as_str())
            .collect();
        assert_eq!(
            b_ids,
            vec!["sess-proj-b"],
            "project-b query must include ONLY sess-proj-b"
        );

        // Cross-project (None) path still returns all 3 (used by stdio /
        // legacy callers with no project context).
        let snap_all = engine.get_context_wake_for_project(None);
        let all_ids: Vec<&str> = snap_all["active_sessions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["session_id"].as_str())
            .collect();
        assert_eq!(
            all_ids.len(),
            3,
            "None project_id must include all sessions for backwards compat"
        );
    }
}
