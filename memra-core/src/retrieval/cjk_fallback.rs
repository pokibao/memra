//! CJK bigram LIKE fallback query builder.
//!
//! Ported from `backend/services/search.py:474-529`.
//! When a query has ≥2 CJK characters and FTS5 returned too few hits,
//! this builds a SQL LIKE fallback using first + last bigram.
//!
//! This is NOT a custom FTS5 tokenizer — it's a query-time Python-side
//! LIKE fallback. See DESIGN_DECISIONS.md D1.1.

/// Extract CJK characters from a query string.
fn extract_cjk_chars(query: &str) -> Vec<char> {
    query
        .chars()
        .filter(|&c| ('\u{4e00}'..='\u{9fff}').contains(&c))
        .collect()
}

/// Build CJK bigram LIKE conditions for SQL WHERE clause.
///
/// Returns a list of (sql_fragment, param) pairs. Each fragment is
/// `"n.content LIKE ?"` and the param is `"%XX%"` where XX is a CJK bigram.
///
/// If there are more than 2 bigrams, only the first and last are used
/// to keep the query fast.
pub fn build_cjk_like_conditions(query: &str) -> Vec<(String, String)> {
    let cjk_chars = extract_cjk_chars(query);
    if cjk_chars.len() < 2 {
        return Vec::new();
    }

    let mut bigrams: Vec<String> = Vec::new();
    for i in 0..cjk_chars.len() - 1 {
        let mut bg = String::with_capacity(6);
        bg.push(cjk_chars[i]);
        bg.push(cjk_chars[i + 1]);
        bigrams.push(bg);
    }

    // Keep only first + last if more than 2
    if bigrams.len() > 2 {
        let first = bigrams[0].clone();
        let last = bigrams[bigrams.len() - 1].clone();
        bigrams = vec![first, last];
    }

    bigrams
        .into_iter()
        .map(|bg| {
            let sql = r"n.content LIKE ? ESCAPE '\'".to_string();
            // Escape any literal % or _ in the bigram (CJK chars won't have these,
            // but defensive coding per ADR D1.1 audit lesson)
            let escaped = bg.replace('%', "\\%").replace('_', "\\_");
            let param = format!("%{escaped}%");
            (sql, param)
        })
        .collect()
}

/// Build a complete SQL WHERE clause combining CJK LIKE conditions with
/// optional filters (layer, category, active, room, project).
///
/// Returns (where_clause, params) ready for `SELECT n.* FROM notes n WHERE {where_clause} LIMIT 10`.
pub fn build_cjk_fallback_query(
    query: &str,
    only_active: bool,
    layer: Option<&str>,
    category: Option<&str>,
    room: Option<&str>,
    project_clause: &str,
    project_params: &[String],
) -> Option<(String, Vec<String>)> {
    let like_conditions = build_cjk_like_conditions(query);
    if like_conditions.is_empty() {
        return None;
    }

    let mut where_parts: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();

    for (sql, param) in like_conditions {
        where_parts.push(sql);
        params.push(param);
    }

    if only_active {
        where_parts.push("n.is_active = 1".to_string());
    }
    if let Some(l) = layer {
        where_parts.push("n.layer = ?".to_string());
        params.push(l.to_string());
    }
    if let Some(c) = category {
        where_parts.push("n.category = ?".to_string());
        params.push(c.to_string());
    }
    if let Some(r) = room {
        // Room prefix filter (ADR-2)
        where_parts.push(r"(n.room = ? OR n.room LIKE ? ESCAPE '\')".to_string());
        params.push(r.to_string());
        let escaped = r.replace('%', "\\%").replace('_', "\\_");
        params.push(format!("{escaped}/%"));
    }

    where_parts.push(project_clause.to_string());
    params.extend(project_params.iter().cloned());

    let where_clause = where_parts.join(" AND ");
    Some((where_clause, params))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_cjk_returns_empty() {
        assert!(build_cjk_like_conditions("hello world").is_empty());
    }

    #[test]
    fn single_cjk_char_returns_empty() {
        assert!(build_cjk_like_conditions("中").is_empty());
    }

    #[test]
    fn two_cjk_chars_returns_one_bigram() {
        let conds = build_cjk_like_conditions("中文");
        assert_eq!(conds.len(), 1);
        assert_eq!(conds[0].0, r"n.content LIKE ? ESCAPE '\'");
        assert_eq!(conds[0].1, "%中文%");
    }

    #[test]
    fn many_cjk_uses_first_and_last_bigram() {
        let conds = build_cjk_like_conditions("记忆锚点搜索");
        assert_eq!(conds.len(), 2);
        assert_eq!(conds[0].1, "%记忆%");
        assert_eq!(conds[1].1, "%搜索%");
    }

    #[test]
    fn mixed_query_extracts_cjk_only() {
        let conds = build_cjk_like_conditions("memory 记忆 anchor 锚点");
        assert_eq!(conds.len(), 2);
        assert_eq!(conds[0].1, "%记忆%");
        assert_eq!(conds[1].1, "%锚点%");
    }
}
