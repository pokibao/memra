//! Candidate retrieval: FTS5 (BM25) + vector (brute-force cosine).
//!
//! Ported from `backend/services/recall.py`.

use std::collections::HashSet;

use rusqlite::{Connection, params_from_iter};
use tracing::warn;

use crate::storage::db::{Candidate, NoteRow};

// Characters with special meaning in FTS5 query syntax.
// Must be stripped to prevent sqlite3 OperationalError on MATCH.
fn is_fts5_special(c: char) -> bool {
    matches!(
        c,
        '"' | '\''
            | '('
            | ')'
            | '*'
            | '+'
            | '-'
            | ':'
            | '^'
            | '{'
            | '}'
            | '['
            | ']'
            | '~'
            | '?'
            | ','
            | ';'
            | '!'
            | '\\'
            | '/'
            | '@'
            | '#'
            | '$'
            | '%'
            | '&'
            | '='
            | '<'
            | '>'
            | '|'
            | '.'
    )
}

/// Remove FTS5 special/punctuation characters from text.
///
/// Preserves alphanumeric, CJK, underscores, and spaces.
pub fn sanitize_fts_query(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let cleaned: String = text
        .chars()
        .map(|c| if is_fts5_special(c) { ' ' } else { c })
        .collect();
    // Collapse multiple spaces
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_cjk_char(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
}

fn has_cjk(text: &str) -> bool {
    text.chars().any(is_cjk_char)
}

/// CJK stopwords (high-frequency, low-information single characters).
static CJK_STOPWORDS: &[char] = &[
    '的', '了', '是', '在', '我', '有', '和', '就', '不', '人', '都', '一', '个', '上', '也', '很',
    '到', '说', '要', '去', '你', '会', '着', '看', '好', '这',
];
static EXACT_PREFILTER_STOPWORDS: &[&str] = &["alice"];

fn is_cjk_stopword(c: char) -> bool {
    CJK_STOPWORDS.contains(&c)
}

/// Convert query string to FTS5 MATCH expression.
///
/// Mixed CJK/English support:
/// - Pure English: sanitize, pass through
/// - CJK/mixed: unicode61 tokenizer stores entire CJK runs as single tokens,
///   so we use prefix-matching (千牛*) for sub-sequence matching.
pub fn build_fts_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }

    let query = sanitize_fts_query(query);
    if query.trim().is_empty() {
        return String::new();
    }

    if !has_cjk(&query) {
        return query;
    }
    if query.split_whitespace().count() >= 2 {
        return build_exact_substring_fts_prefilter(&query);
    }

    let mut terms: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut add_term = |t: String| {
        if !t.is_empty() && seen.insert(t.clone()) {
            terms.push(t);
        }
    };

    // Split into CJK segments and non-CJK segments
    let mut segments: Vec<(bool, String)> = Vec::new();
    let mut current = String::new();
    let mut current_is_cjk = false;

    for c in query.chars() {
        let c_is_cjk = is_cjk_char(c);
        if current.is_empty() {
            current_is_cjk = c_is_cjk;
            current.push(c);
        } else if c_is_cjk == current_is_cjk {
            current.push(c);
        } else {
            segments.push((current_is_cjk, std::mem::take(&mut current)));
            current_is_cjk = c_is_cjk;
            current.push(c);
        }
    }
    if !current.is_empty() {
        segments.push((current_is_cjk, current));
    }

    for (is_cjk_seg, seg) in &segments {
        let seg = seg.trim();
        if seg.is_empty() {
            continue;
        }
        if *is_cjk_seg {
            let cjk_chars: Vec<char> = seg
                .chars()
                .filter(|c| is_cjk_char(*c) && !is_cjk_stopword(*c))
                .collect();
            if cjk_chars.is_empty() {
                continue;
            }

            // Full segment prefix (most specific)
            let full: String = cjk_chars.iter().collect();
            add_term(format!("{full}*"));

            // 2-char window prefixes (recall fallback)
            for window in cjk_chars.windows(2) {
                let bigram: String = window.iter().collect();
                add_term(format!("{bigram}*"));
            }

            // Single char for very short queries
            if cjk_chars.len() == 1 {
                add_term(format!("{}*", cjk_chars[0]));
            }
        } else {
            // Non-CJK: split into tokens, quote each
            for token in seg.split_whitespace() {
                if !token.is_empty() {
                    add_term(format!("\"{token}\""));
                }
            }
        }
    }

    if terms.is_empty() {
        return query;
    }
    terms.join(" OR ")
}

/// Build project filter SQL clause and params.
///
/// Returns (where_clause, params_vec).
pub fn build_project_filter(
    project_id: Option<&str>,
    cross_project: bool,
) -> (String, Vec<String>) {
    if cross_project {
        return ("1=1".to_string(), vec![]);
    }
    match project_id {
        Some(pid) => ("n.project_id = ?".to_string(), vec![pid.to_string()]),
        None => ("1=1".to_string(), vec![]),
    }
}

/// Build temporal SQL filter parts.
///
/// Returns (where_parts, params).
pub fn build_temporal_sql(
    start_time: Option<&str>,
    end_time: Option<&str>,
    include_expired: bool,
) -> (Vec<String>, Vec<String>) {
    let mut parts = Vec::new();
    let mut params = Vec::new();

    if let Some(st) = start_time {
        parts.push("COALESCE(n.updated_at, n.created_at) >= ?".to_string());
        params.push(st.to_string());
    }
    if let Some(et) = end_time {
        parts.push("COALESCE(n.updated_at, n.created_at) <= ?".to_string());
        params.push(et.to_string());
    }
    if !include_expired {
        // No explicit expiry filter needed for basic temporal
    }

    (parts, params)
}

/// Shared recall filters for FTS, vector, and CJK fallback candidate queries.
#[derive(Debug, Clone, Copy)]
pub struct CandidateFilters<'a> {
    pub layer: Option<&'a str>,
    pub category: Option<&'a str>,
    pub only_active: bool,
    pub agent_id: Option<&'a str>,
    pub project_id: Option<&'a str>,
    pub cross_project: bool,
    pub start_time: Option<&'a str>,
    pub end_time: Option<&'a str>,
    pub include_expired: bool,
    pub room: Option<&'a str>,
    pub include_constitution: bool,
}

impl<'a> CandidateFilters<'a> {
    pub fn with_layer(self, layer: Option<&'a str>) -> Self {
        Self { layer, ..self }
    }
}

/// FTS5 candidate retrieval with BM25 scoring.
///
/// Returns candidates sorted by BM25 score (ascending = best first for FTS5).
/// Falls back to LIKE search if FTS5 MATCH fails.
pub fn fts_candidates(
    conn: &Connection,
    fts_query: &str,
    original_query: &str,
    filters: CandidateFilters<'_>,
    candidate_limit: usize,
) -> Vec<Candidate> {
    if fts_query.is_empty() || fts_query.trim().is_empty() {
        return vec![];
    }

    let mut where_parts: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if filters.only_active {
        where_parts.push("n.is_active = 1".to_string());
    }

    if let Some(l) = filters.layer {
        where_parts.push("n.layer = ?".to_string());
        params.push(Box::new(l.to_string()));
        if let Some(aid) = filters.agent_id {
            if l == "event_log" {
                where_parts.push("n.agent_id = ?".to_string());
                params.push(Box::new(aid.to_string()));
            }
        }
    } else if let Some(aid) = filters.agent_id {
        where_parts.push("(n.layer != 'event_log' OR n.agent_id = ?)".to_string());
        params.push(Box::new(aid.to_string()));
    }
    if !filters.include_constitution && filters.layer != Some("identity_schema") {
        where_parts.push("n.layer != 'identity_schema'".to_string());
    }

    if let Some(cat) = filters.category {
        where_parts.push("n.category = ?".to_string());
        params.push(Box::new(cat.to_string()));
    }

    if let Some(r) = filters.room {
        where_parts.push("(n.room = ? OR n.room LIKE ? ESCAPE '\\')".to_string());
        params.push(Box::new(r.to_string()));
        params.push(Box::new(format!(
            "{}/%",
            r.replace('%', "\\%").replace('_', "\\_")
        )));
    }

    // Project filter
    let (proj_clause, proj_params) =
        build_project_filter(filters.project_id, filters.cross_project);
    where_parts.push(proj_clause);
    for p in proj_params {
        params.push(Box::new(p));
    }

    // Temporal filter
    let (t_parts, t_params) = build_temporal_sql(
        filters.start_time,
        filters.end_time,
        filters.include_expired,
    );
    for tp in t_parts {
        where_parts.push(tp);
    }
    for p in t_params {
        params.push(Box::new(p));
    }

    let where_sql = if where_parts.is_empty() {
        "1=1".to_string()
    } else {
        where_parts.join(" AND ")
    };

    // Try FTS5 MATCH first
    let fts_sql = format!(
        "SELECT n.*, bm25(notes_fts) AS bm25_score
         FROM notes_fts
         JOIN notes n ON n.id = notes_fts.note_id
         WHERE notes_fts MATCH ?
           AND {where_sql}
         ORDER BY bm25_score ASC
         LIMIT ?"
    );

    // Build param refs for rusqlite
    let mut all_params: Vec<&dyn rusqlite::types::ToSql> = Vec::new();
    let fts_query_owned = fts_query.to_string();
    all_params.push(&fts_query_owned);
    for p in &params {
        all_params.push(p.as_ref());
    }
    let limit_val = candidate_limit as i64;
    all_params.push(&limit_val);

    match conn.prepare(&fts_sql) {
        Ok(mut stmt) => match stmt.query_map(params_from_iter(all_params.iter()), |row| {
            let note = NoteRow::from_row(row)?;
            let bm25: f64 = row.get("bm25_score")?;
            Ok(Candidate {
                note,
                bm25_score: Some(bm25),
            })
        }) {
            Ok(rows) => rows
                .filter_map(|r| match r {
                    Ok(c) => Some(c),
                    Err(e) => {
                        warn!("Skipping FTS row: {e}");
                        None
                    }
                })
                .collect(),
            Err(e) => {
                warn!("FTS query failed, falling back to LIKE: {e}");
                like_fallback(conn, original_query, &where_sql, &params, candidate_limit)
            }
        },
        Err(e) => {
            warn!("FTS prepare failed, falling back to LIKE: {e}");
            like_fallback(conn, original_query, &where_sql, &params, candidate_limit)
        }
    }
}

/// Exact substring candidate retrieval for long self-recall style queries.
///
/// FTS5/BM25 is a good broad recall surface, but long CJK/mixed queries can
/// explode into many OR terms and crowd out the row that literally contains
/// the query prefix. Replay/self-recall checks use `content[..N]` as the query;
/// that path must at least seed exact substring matches into the candidate
/// pool before scoring decides final order.
pub fn exact_substring_candidates(
    conn: &Connection,
    query: &str,
    filters: CandidateFilters<'_>,
    exclude_ids: &HashSet<String>,
    limit: usize,
) -> Vec<Candidate> {
    let query = query.trim();
    if !should_seed_exact_substring(query) || limit == 0 {
        return vec![];
    }

    let mut where_parts: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let fts_prefilter = query.chars().count() < 16;
    let fts_query = fts_prefilter
        .then(|| build_exact_substring_fts_prefilter(query))
        .filter(|value| !value.trim().is_empty());
    let from_sql = if fts_query.is_some() {
        "notes_fts JOIN notes n ON n.id = notes_fts.note_id"
    } else {
        "notes n"
    };

    let using_fts_prefilter = fts_query.is_some();
    if let Some(fts_query) = fts_query {
        where_parts.push("notes_fts MATCH ?".to_string());
        params.push(Box::new(fts_query));
    }

    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    if !using_fts_prefilter {
        where_parts.push("n.content LIKE ? ESCAPE '\\'".to_string());
        params.push(Box::new(format!("%{escaped}%")));
    }

    if filters.only_active {
        where_parts.push("n.is_active = 1".to_string());
    }
    if let Some(l) = filters.layer {
        where_parts.push("n.layer = ?".to_string());
        params.push(Box::new(l.to_string()));
        if let Some(aid) = filters.agent_id {
            if l == "event_log" {
                where_parts.push("n.agent_id = ?".to_string());
                params.push(Box::new(aid.to_string()));
            }
        }
    } else if let Some(aid) = filters.agent_id {
        where_parts.push("(n.layer != 'event_log' OR n.agent_id = ?)".to_string());
        params.push(Box::new(aid.to_string()));
    }
    if !filters.include_constitution && filters.layer != Some("identity_schema") {
        where_parts.push("n.layer != 'identity_schema'".to_string());
    }
    if let Some(cat) = filters.category {
        where_parts.push("n.category = ?".to_string());
        params.push(Box::new(cat.to_string()));
    }
    if let Some(r) = filters.room {
        where_parts.push("(n.room = ? OR n.room LIKE ? ESCAPE '\\')".to_string());
        params.push(Box::new(r.to_string()));
        params.push(Box::new(format!(
            "{}/%",
            r.replace('%', "\\%").replace('_', "\\_")
        )));
    }

    let (proj_clause, proj_params) =
        build_project_filter(filters.project_id, filters.cross_project);
    where_parts.push(proj_clause);
    for p in proj_params {
        params.push(Box::new(p));
    }

    let (t_parts, t_params) = build_temporal_sql(
        filters.start_time,
        filters.end_time,
        filters.include_expired,
    );
    for tp in t_parts {
        where_parts.push(tp);
    }
    for p in t_params {
        params.push(Box::new(p));
    }

    let where_sql = where_parts.join(" AND ");
    let sql = format!(
        "SELECT n.*, NULL AS bm25_score
         FROM {from_sql}
         WHERE {where_sql}
         ORDER BY
           CASE WHEN n.content = ? THEN 0 ELSE 1 END,
           CASE WHEN n.content LIKE '[FACT]%' THEN 0 ELSE 1 END,
           CASE WHEN n.layer = 'verified_fact' THEN 0 ELSE 1 END,
           ABS(length(n.content) - length(?)) ASC,
           COALESCE(n.updated_at, n.created_at) DESC,
           n.id ASC
         LIMIT ?"
    );
    params.push(Box::new(query.to_string()));
    params.push(Box::new(query.to_string()));
    params.push(Box::new(limit as i64));

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    match conn.prepare(&sql) {
        Ok(mut stmt) => stmt
            .query_map(params_from_iter(param_refs.iter()), |row| {
                let note = NoteRow::from_row(row)?;
                Ok(Candidate {
                    note,
                    bm25_score: None,
                })
            })
            .map(|rows| {
                rows.filter_map(|r| match r {
                    Ok(c) if !exclude_ids.contains(&c.note.id) => Some(c),
                    Ok(_) => None,
                    Err(e) => {
                        warn!("Skipping exact substring row: {e}");
                        None
                    }
                })
                .collect()
            })
            .unwrap_or_default(),
        Err(e) => {
            warn!("exact substring query failed: {e}");
            vec![]
        }
    }
}

fn should_seed_exact_substring(query: &str) -> bool {
    let query = query.trim();
    if query.chars().count() >= 16 {
        return true;
    }

    let meaningful_terms = sanitize_fts_query(query)
        .split_whitespace()
        .filter(|term| {
            term.chars()
                .any(|ch| ch.is_ascii_alphanumeric() || is_cjk_char(ch))
        })
        .count();
    meaningful_terms >= 2
}

fn build_exact_substring_fts_prefilter(query: &str) -> String {
    let query = sanitize_fts_query(query);
    let mut terms = Vec::new();
    for token in query.split_whitespace() {
        if token.chars().any(is_cjk_char) {
            let cjk_term = token
                .chars()
                .filter(|ch| is_cjk_char(*ch) && !is_cjk_stopword(*ch))
                .collect::<String>();
            if !cjk_term.is_empty() {
                terms.push(format!("{cjk_term}*"));
            }
        } else {
            terms.push(format!("\"{token}\""));
        }
    }
    if terms.len() > 1 {
        let filtered = terms
            .iter()
            .filter(|term| {
                let normalized = term.trim_matches('"').to_lowercase();
                !EXACT_PREFILTER_STOPWORDS.contains(&normalized.as_str())
            })
            .cloned()
            .collect::<Vec<_>>();
        if !filtered.is_empty() {
            terms = filtered;
        }
    }
    terms.join(" AND ")
}

fn like_fallback(
    conn: &Connection,
    query: &str,
    where_sql: &str,
    params: &[Box<dyn rusqlite::types::ToSql>],
    limit: usize,
) -> Vec<Candidate> {
    let like_sql = format!(
        "SELECT n.*, NULL AS bm25_score
         FROM notes n
         WHERE n.content LIKE ? ESCAPE '\\'
           AND {where_sql}
         ORDER BY COALESCE(n.updated_at, n.created_at) DESC, n.id ASC
         LIMIT ?"
    );

    // Escape LIKE wildcards in query
    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let like_pattern = format!("%{escaped}%");
    let mut all_params: Vec<&dyn rusqlite::types::ToSql> = Vec::new();
    all_params.push(&like_pattern);
    for p in params {
        all_params.push(p.as_ref());
    }
    let limit_val = limit as i64;
    all_params.push(&limit_val);

    match conn.prepare(&like_sql) {
        Ok(mut stmt) => stmt
            .query_map(params_from_iter(all_params.iter()), |row| {
                let note = NoteRow::from_row(row)?;
                Ok(Candidate {
                    note,
                    bm25_score: None,
                })
            })
            .map(|rows| {
                rows.filter_map(|r| match r {
                    Ok(c) => Some(c),
                    Err(e) => {
                        warn!("Skipping LIKE row: {e}");
                        None
                    }
                })
                .collect()
            })
            .unwrap_or_default(),
        Err(e) => {
            warn!("LIKE fallback also failed: {e}");
            vec![]
        }
    }
}

/// Vector candidate retrieval: brute-force cosine similarity.
///
/// For <5000 records, brute-force scan is fast enough (<50ms).
/// Only retrieves candidates not already found by FTS (exclude_ids dedup).
pub fn vector_candidates(
    conn: &Connection,
    query_vector: &[f32],
    filters: CandidateFilters<'_>,
    candidate_limit: usize,
    exclude_ids: &HashSet<String>,
) -> Vec<Candidate> {
    let mut _discard_counter = 0usize;
    let mut _discard_seen: HashSet<String> = HashSet::new();
    vector_candidates_with_stats(
        conn,
        query_vector,
        filters,
        candidate_limit,
        exclude_ids,
        &mut _discard_seen,
        &mut _discard_counter,
    )
}

/// Same as [`vector_candidates`] but reports the number of legacy-dim rows
/// that had to be skipped (e.g. 384-dim rows when the query vector is
/// 1024-dim bge-m3). REL-02: surface this count in MCP response so users can
/// see "N notes skipped due to embedding dim mismatch" instead of silent
/// recall loss.
pub fn vector_candidates_with_stats(
    conn: &Connection,
    query_vector: &[f32],
    filters: CandidateFilters<'_>,
    candidate_limit: usize,
    exclude_ids: &HashSet<String>,
    dim_mismatch_seen: &mut HashSet<String>,
    dim_mismatch_skipped: &mut usize,
) -> Vec<Candidate> {
    let mut where_parts: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    where_parts.push("(n.vector_blob IS NOT NULL OR n.vector_json IS NOT NULL)".to_string());

    if filters.only_active {
        where_parts.push("n.is_active = 1".to_string());
    }

    if let Some(l) = filters.layer {
        where_parts.push("n.layer = ?".to_string());
        params.push(Box::new(l.to_string()));
        if let Some(aid) = filters.agent_id {
            if l == "event_log" {
                where_parts.push("n.agent_id = ?".to_string());
                params.push(Box::new(aid.to_string()));
            }
        }
    } else if let Some(aid) = filters.agent_id {
        where_parts.push("(n.layer != 'event_log' OR n.agent_id = ?)".to_string());
        params.push(Box::new(aid.to_string()));
    }
    if !filters.include_constitution && filters.layer != Some("identity_schema") {
        where_parts.push("n.layer != 'identity_schema'".to_string());
    }

    if let Some(cat) = filters.category {
        where_parts.push("n.category = ?".to_string());
        params.push(Box::new(cat.to_string()));
    }

    if let Some(r) = filters.room {
        where_parts.push("(n.room = ? OR n.room LIKE ? ESCAPE '\\')".to_string());
        params.push(Box::new(r.to_string()));
        params.push(Box::new(format!(
            "{}/%",
            r.replace('%', "\\%").replace('_', "\\_")
        )));
    }

    let (proj_clause, proj_params) =
        build_project_filter(filters.project_id, filters.cross_project);
    where_parts.push(proj_clause);
    for p in proj_params {
        params.push(Box::new(p));
    }

    let (t_parts, t_params) = build_temporal_sql(
        filters.start_time,
        filters.end_time,
        filters.include_expired,
    );
    for tp in t_parts {
        where_parts.push(tp);
    }
    for p in t_params {
        params.push(Box::new(p));
    }

    let where_sql = where_parts.join(" AND ");
    let sql = format!("SELECT n.* FROM notes n WHERE {where_sql}");

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            warn!("Vector candidates query failed: {e}");
            return vec![];
        }
    };

    let mut scored: Vec<(Candidate, f64)> = Vec::new();

    let rows = match stmt.query_map(params_from_iter(param_refs.iter()), |row| {
        NoteRow::from_row(row)
    }) {
        Ok(r) => r,
        Err(e) => {
            warn!("Vector candidates iteration failed: {e}");
            return vec![];
        }
    };

    for row_result in rows {
        let note = match row_result {
            Ok(n) => n,
            Err(e) => {
                warn!("Skipping vector candidate row: {e}");
                continue;
            }
        };

        if exclude_ids.contains(&note.id) {
            continue;
        }

        // Extract stored vector (prefer blob, fallback to JSON)
        let Some(stored_vec) = extract_vector(&note) else {
            continue;
        };

        // Dim-skip: cosine silently returns 0.0 on mismatched dims, which would
        // wrongly drop legacy 384-dim rows below the 0.3 threshold without signal.
        // Skip before cosine so the silent-zero path can't degrade recall.
        //
        // Dedup across calls: search.rs invokes this function multiple times
        // on the same DB (layer-scoped + global fallback). Increment the
        // counter only when we haven't seen this row before, so the final
        // diagnostic reports unique affected notes rather than rejection
        // events. (Codex P2 on PR #194.)
        if stored_vec.len() != query_vector.len() {
            if dim_mismatch_seen.insert(note.id.clone()) {
                *dim_mismatch_skipped += 1;
            }
            continue;
        }

        let sim = crate::retrieval::scoring::cosine_similarity(query_vector, &stored_vec);
        if sim > 0.3 {
            scored.push((
                Candidate {
                    note,
                    bm25_score: None,
                },
                sim,
            ));
        }
    }

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .into_iter()
        .take(candidate_limit)
        .map(|(c, _)| c)
        .collect()
}

/// Extract vector from a NoteRow as f32, preferring blob over JSON.
pub fn extract_vector(note: &NoteRow) -> Option<Vec<f32>> {
    // Try vector_blob first (binary f32 LE array)
    if let Some(ref blob) = note.vector_blob {
        if blob.len() >= 4 {
            let floats: Vec<f32> = blob
                .chunks_exact(4)
                .filter_map(|chunk| {
                    let arr: [u8; 4] = chunk.try_into().ok()?;
                    Some(f32::from_le_bytes(arr))
                })
                .collect();
            if !floats.is_empty() {
                return Some(floats);
            }
        }
    }

    // Fallback to vector_json
    if let Some(ref json_str) = note.vector_json {
        if let Ok(vec) = serde_json::from_str::<Vec<f32>>(json_str) {
            if !vec.is_empty() {
                return Some(vec);
            }
        }
        // Try as f64 array, convert to f32
        if let Ok(vec) = serde_json::from_str::<Vec<f64>>(json_str) {
            if !vec.is_empty() {
                return Some(vec.into_iter().map(|f| f as f32).collect());
            }
        }
    }

    None
}

/// CJK bigram LIKE fallback for when FTS5 misses CJK content.
///
/// Extracts 2-char CJK bigrams from query and uses LIKE %bigram% matching.
pub fn cjk_bigram_candidates(
    conn: &Connection,
    query: &str,
    filters: CandidateFilters<'_>,
    exclude_ids: &HashSet<String>,
) -> Vec<Candidate> {
    let cjk_chars: Vec<char> = query.chars().filter(|c| is_cjk_char(*c)).collect();
    if cjk_chars.len() < 2 {
        return vec![];
    }

    // Build bigrams (limit to first and last if > 2)
    let mut bigrams: Vec<String> = Vec::new();
    for window in cjk_chars.windows(2) {
        bigrams.push(window.iter().collect());
    }
    if bigrams.len() > 2 {
        let first = bigrams[0].clone();
        let last = bigrams[bigrams.len() - 1].clone();
        bigrams = vec![first, last];
    }

    let mut where_parts: Vec<String> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    // LIKE conditions for bigrams
    for bg in &bigrams {
        where_parts.push("n.content LIKE ?".to_string());
        params.push(Box::new(format!("%{bg}%")));
    }

    if filters.only_active {
        where_parts.push("n.is_active = 1".to_string());
    }
    if let Some(l) = filters.layer {
        where_parts.push("n.layer = ?".to_string());
        params.push(Box::new(l.to_string()));
        if let Some(aid) = filters.agent_id {
            if l == "event_log" {
                where_parts.push("n.agent_id = ?".to_string());
                params.push(Box::new(aid.to_string()));
            }
        }
    }
    if !filters.include_constitution && filters.layer != Some("identity_schema") {
        where_parts.push("n.layer != 'identity_schema'".to_string());
    }
    if let Some(cat) = filters.category {
        where_parts.push("n.category = ?".to_string());
        params.push(Box::new(cat.to_string()));
    }
    if let Some(r) = filters.room {
        where_parts.push("(n.room = ? OR n.room LIKE ? ESCAPE '\\')".to_string());
        params.push(Box::new(r.to_string()));
        params.push(Box::new(format!(
            "{}/%",
            r.replace('%', "\\%").replace('_', "\\_")
        )));
    }

    let (proj_clause, proj_params) =
        build_project_filter(filters.project_id, filters.cross_project);
    where_parts.push(proj_clause);
    for p in proj_params {
        params.push(Box::new(p));
    }

    let where_sql = where_parts.join(" AND ");
    let sql = format!("SELECT n.* FROM notes n WHERE {where_sql} LIMIT 10");

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            warn!("CJK bigram query prepare failed: {e}");
            return vec![];
        }
    };

    stmt.query_map(params_from_iter(param_refs.iter()), |row| {
        NoteRow::from_row(row)
    })
    .map(|rows| {
        rows.filter_map(|r| match r {
            Ok(note) => Some(note),
            Err(e) => {
                warn!("Skipping CJK bigram row: {e}");
                None
            }
        })
        .filter(|note| !exclude_ids.contains(&note.id))
        .map(|note| Candidate {
            note,
            bm25_score: None,
        })
        .collect()
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_special_chars() {
        assert_eq!(sanitize_fts_query("hello.world"), "hello world");
        assert_eq!(sanitize_fts_query("foo(bar)"), "foo bar");
        assert_eq!(sanitize_fts_query("a+b-c*d"), "a b c d");
    }

    #[test]
    fn sanitize_preserves_cjk_and_underscore() {
        assert_eq!(sanitize_fts_query("recall_count"), "recall_count");
        assert_eq!(sanitize_fts_query("千牛"), "千牛");
    }

    #[test]
    fn build_fts_pure_english() {
        let result = build_fts_query("hello world");
        assert_eq!(result, "hello world");
    }

    #[test]
    fn build_fts_cjk_prefix() {
        let result = build_fts_query("千牛淘宝");
        // Should have full prefix + bigrams
        assert!(result.contains("千牛淘宝*"));
        assert!(result.contains("千牛*"));
        assert!(result.contains("淘宝*"));
        assert!(result.contains(" OR "));
    }

    #[test]
    fn build_fts_mixed_cjk_english() {
        let result = build_fts_query("Laplace平滑");
        assert!(result.contains("\"Laplace\""));
        assert!(result.contains("平滑*"));
    }

    #[test]
    fn build_fts_spaced_mixed_query_uses_and() {
        assert_eq!(build_fts_query("alice 北极星"), "北极星*");
        assert_eq!(build_fts_query("DemoERP ERP"), "DemoERP ERP");
    }

    #[test]
    fn build_fts_empty() {
        assert_eq!(build_fts_query(""), "");
    }

    #[test]
    fn exact_substring_seed_allows_short_multi_term_queries() {
        assert!(should_seed_exact_substring("memory anchor"));
        assert!(should_seed_exact_substring("AKM keychain"));
        assert!(should_seed_exact_substring("AcmeOps ERP"));
        assert!(should_seed_exact_substring("R1 soak defer"));
        assert!(!should_seed_exact_substring("anchor"));
        assert!(!should_seed_exact_substring("千牛"));
    }

    #[test]
    fn exact_substring_prefilter_uses_and_terms() {
        assert_eq!(
            build_exact_substring_fts_prefilter("alice 北极星"),
            "北极星*"
        );
        assert_eq!(
            build_exact_substring_fts_prefilter("DemoERP ERP"),
            "\"DemoERP\" AND \"ERP\""
        );
    }

    /// REL-02: `vector_candidates_with_stats` must increment the dim-skip
    /// counter once per row whose stored vector has a different length than
    /// the query vector. Without this counter the MCP layer cannot surface
    /// "N notes silently skipped" to the user.
    #[test]
    fn vector_candidates_with_stats_counts_dim_mismatches() {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "CREATE TABLE notes (
                id               TEXT PRIMARY KEY,
                content          TEXT NOT NULL,
                layer            TEXT,
                category         TEXT,
                is_active        INTEGER NOT NULL DEFAULT 1,
                confidence       REAL,
                agent_id         TEXT,
                project_id       TEXT,
                created_at       TEXT,
                updated_at       TEXT,
                valid_at         REAL,
                metadata_json    TEXT,
                vector_json      TEXT,
                vector_blob      BLOB,
                evolution_state  TEXT DEFAULT 'active',
                topic_key        TEXT,
                is_head          INTEGER DEFAULT 1,
                review_after     TEXT,
                room             TEXT,
                agent            TEXT,
                difficulty       INTEGER,
                time_cost_hint   TEXT,
                related_ids_json TEXT,
                role             TEXT,
                session_id       TEXT,
                source           TEXT,
                created_by       TEXT,
                version          INTEGER DEFAULT 1,
                root_id          TEXT,
                cold_storage_ref TEXT,
                superseded_by    TEXT,
                recall_count     INTEGER DEFAULT 0,
                event_when       TEXT,
                event_when_ts    REAL
            );",
        )
        .expect("create schema");

        // Insert 2 legacy 384-dim rows (1536 bytes) and 1 current 1024-dim row.
        let legacy_blob = vec![0u8; 1536]; // 384 × 4
        let current_blob = vec![0u8; 4096]; // 1024 × 4
        for (id, blob) in [
            ("legacy-1", &legacy_blob),
            ("legacy-2", &legacy_blob),
            ("current", &current_blob),
        ] {
            conn.execute(
                "INSERT INTO notes (id, content, layer, is_active, project_id, vector_blob) \
                 VALUES (?, ?, 'verified_fact', 1, 'p', ?)",
                rusqlite::params![id, format!("content {id}"), blob],
            )
            .expect("insert row");
        }

        // Query with a 1024-dim vector (mock bge-m3 output). All zeros so
        // cosine with current row rounds to 0 and may not pass the 0.3
        // threshold — we only care about the counter here, not the return set.
        let query_vector = vec![0.0f32; 1024];
        let filters = CandidateFilters {
            layer: None,
            category: None,
            only_active: true,
            agent_id: None,
            project_id: Some("p"),
            cross_project: false,
            start_time: None,
            end_time: None,
            include_expired: false,
            room: None,
            include_constitution: false,
        };
        let exclude: HashSet<String> = HashSet::new();
        let mut skip_counter = 0usize;
        let mut dim_mismatch_seen: HashSet<String> = HashSet::new();

        let _candidates = vector_candidates_with_stats(
            &conn,
            &query_vector,
            filters,
            10,
            &exclude,
            &mut dim_mismatch_seen,
            &mut skip_counter,
        );

        assert_eq!(
            skip_counter, 2,
            "expected both legacy 384-dim rows to be counted as dim-mismatch skips"
        );
    }

    /// Regression: Codex P2 on PR #194 — `dim_mismatch_skipped` must count
    /// UNIQUE rows, not rejection events. The real search pipeline invokes
    /// `vector_candidates_with_stats` multiple times on the same DB (layer-
    /// scoped L0/L2/L3 + global fallback). The same legacy-dim row is visited
    /// by each pass. Without dedup, a DB with N legacy rows and K passes
    /// reports N*K skips and users see inflated "X notes skipped" banners.
    #[test]
    fn vector_candidates_with_stats_dedups_dim_mismatches_across_calls() {
        use rusqlite::Connection;

        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            "CREATE TABLE notes (
                id               TEXT PRIMARY KEY,
                content          TEXT NOT NULL,
                layer            TEXT,
                category         TEXT,
                is_active        INTEGER NOT NULL DEFAULT 1,
                confidence       REAL,
                agent_id         TEXT,
                project_id       TEXT,
                created_at       TEXT,
                updated_at       TEXT,
                valid_at         REAL,
                metadata_json    TEXT,
                vector_json      TEXT,
                vector_blob      BLOB,
                evolution_state  TEXT DEFAULT 'active',
                topic_key        TEXT,
                is_head          INTEGER DEFAULT 1,
                review_after     TEXT,
                room             TEXT,
                agent            TEXT,
                difficulty       INTEGER,
                time_cost_hint   TEXT,
                related_ids_json TEXT,
                role             TEXT,
                session_id       TEXT,
                source           TEXT,
                created_by       TEXT,
                version          INTEGER DEFAULT 1,
                root_id          TEXT,
                cold_storage_ref TEXT,
                superseded_by    TEXT,
                recall_count     INTEGER DEFAULT 0,
                event_when       TEXT,
                event_when_ts    REAL
            );",
        )
        .expect("create schema");

        let legacy_blob = vec![0u8; 1536]; // 384 × 4
        for id in ["legacy-1", "legacy-2"] {
            conn.execute(
                "INSERT INTO notes (id, content, layer, is_active, project_id, vector_blob) \
                 VALUES (?, ?, 'verified_fact', 1, 'p', ?)",
                rusqlite::params![id, format!("content {id}"), &legacy_blob],
            )
            .expect("insert row");
        }

        let query_vector = vec![0.0f32; 1024];
        let filters = CandidateFilters {
            layer: None,
            category: None,
            only_active: true,
            agent_id: None,
            project_id: Some("p"),
            cross_project: false,
            start_time: None,
            end_time: None,
            include_expired: false,
            room: None,
            include_constitution: false,
        };
        let exclude: HashSet<String> = HashSet::new();
        let mut skip_counter = 0usize;
        let mut dim_mismatch_seen: HashSet<String> = HashSet::new();

        // Simulate search.rs calling vector_candidates_with_stats twice on
        // the same DB (e.g. layer-scoped pass + global fallback pass).
        let _call1 = vector_candidates_with_stats(
            &conn,
            &query_vector,
            filters,
            10,
            &exclude,
            &mut dim_mismatch_seen,
            &mut skip_counter,
        );
        let _call2 = vector_candidates_with_stats(
            &conn,
            &query_vector,
            filters,
            10,
            &exclude,
            &mut dim_mismatch_seen,
            &mut skip_counter,
        );

        assert_eq!(
            skip_counter, 2,
            "expected 2 unique dim-mismatch rows, got {skip_counter} — \
             counter is counting rejection events instead of unique notes"
        );
    }

    #[test]
    fn extract_vector_from_blob() {
        let floats: Vec<f32> = vec![1.0, 2.0, 3.0];
        let blob: Vec<u8> = floats.iter().flat_map(|f| f.to_le_bytes()).collect();
        let note = NoteRow {
            id: "test".to_string(),
            content: "test".to_string(),
            layer: "verified_fact".to_string(),
            category: None,
            vector_json: None,
            vector_blob: Some(blob),
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
            project_id: None,
            confidence: None,
            recall_count: 0,
        };
        let vec = extract_vector(&note).unwrap();
        assert_eq!(vec.len(), 3);
        assert!((vec[0] - 1.0_f32).abs() < 1e-6);
        assert!((vec[1] - 2.0_f32).abs() < 1e-6);
        assert!((vec[2] - 3.0_f32).abs() < 1e-6);
    }
}
