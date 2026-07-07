//! Narrow detector for AI-origin commercial-number claims.
//!
//! Mirrors `backend/core/commercial_number_detector.py` while keeping the
//! expensive neighbor lookup lazy: source / amount / entity prechecks must pass
//! before callers touch hebbian neighbors.

use std::sync::OnceLock;

use regex::Regex;

use crate::personal::{commercial_entities, is_ai_provisional_source};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectorResult {
    pub flagged: bool,
    pub reason: String,
}

pub fn detect_with_neighbor_sources<F>(
    content: &str,
    trigger: Option<&str>,
    source: &str,
    neighbor_sources: F,
) -> DetectorResult
where
    F: FnOnce() -> Vec<Option<String>>,
{
    if !is_ai_provisional_source(source) {
        return result(false, "source_not_ai_commercial");
    }

    let text = [content, trigger.unwrap_or("")]
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !has_commercial_amount(&text) {
        return result(false, "no_commercial_amount");
    }

    let Some(entity) = matched_entity(&text) else {
        return result(false, "no_commercial_entity");
    };

    if neighbor_sources()
        .into_iter()
        .flatten()
        .any(|source| source.trim() == "user")
    {
        return result(false, "user_anchor_neighbor_present");
    }

    result(
        true,
        format!("ai_commercial_number_without_user_anchor: entity={entity}"),
    )
}

pub fn has_commercial_amount(text: &str) -> bool {
    if text.trim().is_empty() {
        return false;
    }
    let non_technical_text = technical_number_re().replace_all(text, " ");
    currency_amount_re().is_match(&non_technical_text)
        || chinese_amount_re().is_match(&non_technical_text)
}

fn matched_entity(text: &str) -> Option<&'static str> {
    let lowered = text.to_lowercase();
    commercial_entities()
        .iter()
        .copied()
        .find(|entity| lowered.contains(&entity.to_lowercase()))
}

fn currency_amount_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:[¥$€￥]\s*\d+(?:\.\d+)?\s*[KkMm]?)").unwrap())
}

fn chinese_amount_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:\d+(?:\.\d+)?\s*[万亿]|[万亿]\s*\d+(?:\.\d+)?)").unwrap())
}

fn technical_number_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\d+\s*(?:port|端口|mb|gb|tb|ms|s|px|%|min_score|threshold)\b").unwrap()
    })
}

fn result(flagged: bool, reason: impl Into<String>) -> DetectorResult {
    DetectorResult {
        flagged,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::detect_with_neighbor_sources;

    #[test]
    fn public_build_does_not_flag_without_commercial_entity_list() {
        let result = detect_with_neighbor_sources("MarketLab ¥600K Riley续签", None, "ai", Vec::new);

        assert!(!result.flagged);
        assert_eq!(result.reason, "no_commercial_entity");
    }

    #[test]
    fn public_build_does_not_flag_vendor_amount_without_commercial_entity_list() {
        let result = detect_with_neighbor_sources(
            "对标GenericBI升级方案 $50K",
            None,
            "ai_extraction",
            Vec::new,
        );

        assert!(!result.flagged);
        assert_eq!(result.reason, "no_commercial_entity");
    }

    #[test]
    fn does_not_flag_user_source_commercial_number() {
        let result =
            detect_with_neighbor_sources("MarketLab现价¥480K合同已签", None, "user", Vec::new);

        assert!(!result.flagged);
        assert_eq!(result.reason, "source_not_ai_commercial");
    }

    #[test]
    fn does_not_flag_technical_numbers_only() {
        let result = detect_with_neighbor_sources("服务器8080端口内存512MB", None, "ai", Vec::new);

        assert!(!result.flagged);
        assert_eq!(result.reason, "no_commercial_amount");
    }

    #[test]
    fn does_not_check_neighbors_without_public_commercial_entities() {
        let calls = Cell::new(0);
        let result = detect_with_neighbor_sources("订单金额¥12K Riley", None, "ai", || {
            calls.set(calls.get() + 1);
            vec![Some("user".to_string())]
        });

        assert!(!result.flagged);
        assert_eq!(result.reason, "no_commercial_entity");
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn public_build_does_not_flag_chinese_unit_without_commercial_entity_list() {
        let result = detect_with_neighbor_sources(
            "自动摘要提到续费",
            Some("GridFin 300万 方案"),
            "external_ai",
            Vec::new,
        );

        assert!(!result.flagged);
        assert_eq!(result.reason, "no_commercial_entity");
    }

    #[test]
    fn neighbor_provider_stays_lazy_until_all_prechecks_pass() {
        for (content, source, reason) in [
            ("¥600K Riley续签", "user", "source_not_ai_commercial"),
            ("服务器8080端口内存512MB", "ai", "no_commercial_amount"),
            ("某产品定价¥299", "ai", "no_commercial_entity"),
        ] {
            let calls = Cell::new(0);
            let result = detect_with_neighbor_sources(content, None, source, || {
                calls.set(calls.get() + 1);
                Vec::new()
            });
            assert!(!result.flagged);
            assert_eq!(result.reason, reason);
            assert_eq!(calls.get(), 0, "neighbor lookup ran for {reason}");
        }

        let calls = Cell::new(0);
        let result = detect_with_neighbor_sources("MarketLab ¥600K Riley续签", None, "ai", || {
            calls.set(calls.get() + 1);
            Vec::new()
        });
        assert!(!result.flagged);
        assert_eq!(result.reason, "no_commercial_entity");
        assert_eq!(calls.get(), 0);
    }
}
