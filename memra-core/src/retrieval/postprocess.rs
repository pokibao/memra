//! Search result post-processing: dedup, type diversity, temperature labels.
//!
//! Ported from `backend/services/search_postprocessor.py`.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::storage::db::SearchResult;

/// Deduplicate results by topic_key, keeping the newest version.
///
/// Returns (deduped_results, seen_topic_keys).
pub fn dedupe_results_by_topic_key(results: &mut Vec<SearchResult>) -> HashSet<String> {
    let mut topic_best: HashMap<String, usize> = HashMap::new();
    let mut no_topic_indices: Vec<usize> = Vec::new();

    for (i, result) in results.iter().enumerate() {
        match &result.topic_key {
            Some(tk) if !tk.is_empty() => {
                if let Some(&existing_idx) = topic_best.get(tk) {
                    let new_exact = result.channel.as_deref() == Some("exact");
                    let old_exact = results[existing_idx].channel.as_deref() == Some("exact");
                    let should_replace = if new_exact != old_exact {
                        new_exact
                    } else {
                        let new_ts = result.created_at.as_deref().unwrap_or("");
                        let old_ts = results[existing_idx].created_at.as_deref().unwrap_or("");
                        new_ts > old_ts
                    };
                    if should_replace {
                        topic_best.insert(tk.clone(), i);
                    }
                } else {
                    topic_best.insert(tk.clone(), i);
                }
            }
            _ => no_topic_indices.push(i),
        }
    }

    let seen_topics: HashSet<String> = topic_best.keys().cloned().collect();
    let keep_indices: HashSet<usize> = topic_best
        .values()
        .copied()
        .chain(no_topic_indices)
        .collect();

    let mut i = 0;
    results.retain(|_| {
        let keep = keep_indices.contains(&i);
        i += 1;
        keep
    });

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    seen_topics
}

/// Ensure no single category dominates search results.
///
/// Removes lowest-scored results from any category exceeding max_ratio.
pub fn enforce_type_diversity(results: &mut Vec<SearchResult>, max_ratio: f64) {
    if results.len() <= 2 || max_ratio >= 1.0 {
        return;
    }

    // Group by category (fallback to layer)
    let mut category_items: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, r) in results.iter().enumerate() {
        let cat = r.category.clone().unwrap_or_else(|| r.layer.clone());
        category_items.entry(cat).or_default().push(i);
    }

    // Only enforce when multiple categories
    if category_items.len() <= 1 {
        return;
    }

    let total = results.len();
    let max_per_category = (total as f64 * max_ratio).floor().max(2.0) as usize;

    let mut excluded: HashSet<usize> = HashSet::new();
    for items in category_items.values() {
        if items.len() > max_per_category {
            // Sort by score descending, drop the rest
            let mut sorted_items: Vec<usize> = items.clone();
            sorted_items.sort_by(|&a, &b| {
                results[b]
                    .score
                    .partial_cmp(&results[a].score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for &idx in &sorted_items[max_per_category..] {
                excluded.insert(idx);
            }
        }
    }

    if excluded.is_empty() {
        return;
    }

    let mut i = 0;
    results.retain(|_| {
        let keep = !excluded.contains(&i);
        i += 1;
        keep
    });
}

/// Add temperature labels (hot/warm/cold) based on recall activity.
///
/// Temperature is purely informational — does NOT change scoring.
pub fn add_temperature_labels(results: &mut [SearchResult]) {
    let now = Utc::now();

    for result in results.iter_mut() {
        let recall_count = result.recall_count;
        let days_since = result
            .last_recalled_at
            .as_deref()
            .and_then(|s| {
                let cleaned = s.replace("Z", "+00:00");
                DateTime::parse_from_rfc3339(&cleaned)
                    .ok()
                    .map(|dt| (now - dt.with_timezone(&Utc)).num_days())
            })
            .unwrap_or(999);

        result.temperature = Some(if recall_count >= 3 && days_since <= 30 {
            "hot".to_string()
        } else if recall_count >= 1 && days_since <= 180 {
            "warm".to_string()
        } else {
            "cold".to_string()
        });
    }
}

/// Ensure all results have a default channel.
pub fn ensure_default_channel(results: &mut [SearchResult], default: &str) {
    for r in results.iter_mut() {
        if r.channel.is_none() {
            r.channel = Some(default.to_string());
        }
    }
}

/// Merge temporal (recent) results with primary results.
pub fn merge_temporal_results(
    primary: &[SearchResult],
    recent: &[SearchResult],
    mode: &str,
    limit: usize,
) -> Vec<SearchResult> {
    let ordered: Vec<&SearchResult> = if mode == "recent" {
        recent.iter().chain(primary.iter()).collect()
    } else {
        primary.iter().chain(recent.iter()).collect()
    };

    let mut merged: Vec<SearchResult> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for r in ordered {
        if seen.contains(&r.id) {
            continue;
        }
        seen.insert(r.id.clone());
        merged.push(r.clone());
        if merged.len() >= limit {
            break;
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(
        id: &str,
        score: f64,
        category: Option<&str>,
        topic_key: Option<&str>,
    ) -> SearchResult {
        SearchResult {
            id: id.to_string(),
            content: format!("content_{id}"),
            layer: "verified_fact".to_string(),
            category: category.map(|s| s.to_string()),
            score,
            created_at: Some("2026-04-13T00:00:00Z".to_string()),
            topic_key: topic_key.map(|s| s.to_string()),
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
    fn dedup_keeps_newest_per_topic() {
        let mut results = vec![
            {
                let mut r = make_result("a", 0.9, None, Some("topic1"));
                r.created_at = Some("2026-04-12T00:00:00Z".to_string());
                r
            },
            {
                let mut r = make_result("b", 0.8, None, Some("topic1"));
                r.created_at = Some("2026-04-13T00:00:00Z".to_string());
                r
            },
            make_result("c", 0.7, None, None),
        ];

        let topics = dedupe_results_by_topic_key(&mut results);
        assert!(topics.contains("topic1"));
        assert_eq!(results.len(), 2);
        // "b" should win (newer)
        assert!(results.iter().any(|r| r.id == "b"));
        assert!(results.iter().any(|r| r.id == "c"));
    }

    #[test]
    fn dedup_keeps_literal_exact_match_over_newer_topic_sibling() {
        let mut results = vec![
            {
                let mut r = make_result("exact-old", 0.985, None, Some("topic1"));
                r.created_at = Some("2026-04-12T00:00:00Z".to_string());
                r.channel = Some("exact".to_string());
                r
            },
            {
                let mut r = make_result("newer-sibling", 0.999, None, Some("topic1"));
                r.created_at = Some("2026-04-13T00:00:00Z".to_string());
                r
            },
        ];

        dedupe_results_by_topic_key(&mut results);

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "exact-old");
    }

    #[test]
    fn type_diversity_caps_dominant_category() {
        let mut results = vec![
            make_result("a1", 0.9, Some("bug"), None),
            make_result("a2", 0.8, Some("bug"), None),
            make_result("a3", 0.7, Some("bug"), None),
            make_result("a4", 0.6, Some("bug"), None),
            make_result("b1", 0.5, Some("feature"), None),
        ];

        enforce_type_diversity(&mut results, 0.6);
        let bug_count = results
            .iter()
            .filter(|r| r.category.as_deref() == Some("bug"))
            .count();
        assert!(bug_count <= 3); // 60% of 5 = 3
    }

    #[test]
    fn temperature_labels() {
        let mut results = vec![
            {
                let mut r = make_result("hot", 0.9, None, None);
                r.recall_count = 5;
                r.last_recalled_at = Some(Utc::now().to_rfc3339());
                r
            },
            {
                let mut r = make_result("warm", 0.8, None, None);
                r.recall_count = 1;
                r.last_recalled_at = Some(Utc::now().to_rfc3339());
                r
            },
            make_result("cold", 0.7, None, None),
        ];

        add_temperature_labels(&mut results);
        assert_eq!(results[0].temperature.as_deref(), Some("hot"));
        assert_eq!(results[1].temperature.as_deref(), Some("warm"));
        assert_eq!(results[2].temperature.as_deref(), Some("cold"));
    }
}
