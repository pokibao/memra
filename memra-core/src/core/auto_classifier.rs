//! Rule-first automatic memory classifier.
//!
//! Rust port of the archived v6 classifier. The old fallback LLM path was
//! already a no-op, so this module intentionally stays deterministic.

const DEFAULT_CATEGORY: &str = "event";
const DEFAULT_LAYER: &str = "verified_fact";
const VALID_LAYERS: &[&str] = &["verified_fact", "event_log"];

const CATEGORY_KEYWORDS: &[(&str, &[&str])] = &[
    (
        "person",
        &[
            "人",
            "人物",
            "联系人",
            "客户",
            "用户",
            "同事",
            "person",
            "contact",
            "user",
            "colleague",
            "team",
        ],
    ),
    (
        "place",
        &[
            "地点",
            "地址",
            "位置",
            "会议室",
            "办公室",
            "place",
            "location",
            "office",
            "address",
            "room",
        ],
    ),
    (
        "event",
        &[
            "会议",
            "活动",
            "发生",
            "完成",
            "提交",
            "发布",
            "上线",
            "更新",
            "修复",
            "event",
            "meeting",
            "completed",
            "shipped",
            "deployed",
            "released",
            "fixed",
            "merged",
        ],
    ),
    (
        "item",
        &[
            "物品", "工具", "设备", "文件", "文档", "item", "tool", "document", "config", "file",
            "artifact",
        ],
    ),
    (
        "routine",
        &[
            "每天", "每周", "习惯", "流程", "步骤", "日常", "routine", "daily", "weekly",
            "workflow", "process", "habit",
        ],
    ),
    (
        "sku",
        &[
            "款式",
            "SKU",
            "产品",
            "商品",
            "货号",
            "面料",
            "尺码",
            "颜色",
            "价格",
            "库存",
            "product",
            "inventory",
            "stock",
            "price",
        ],
    ),
    (
        "customer",
        &[
            "客户",
            "顾客",
            "买家",
            "订单",
            "复购",
            "私域",
            "customer",
            "buyer",
            "order",
            "subscriber",
        ],
    ),
    (
        "trend",
        &[
            "趋势", "流行", "热门", "爆款", "风格", "潮流", "trend", "fashion", "popular", "viral",
            "trending",
        ],
    ),
    (
        "campaign",
        &[
            "活动",
            "推广",
            "营销",
            "促销",
            "折扣",
            "campaign",
            "promotion",
            "marketing",
            "discount",
            "launch",
        ],
    ),
    (
        "decision",
        &[
            "decided",
            "chosen",
            "went with",
            "picked",
            "selected",
            "agreed",
            "switched to",
            "committed to",
            "approved",
            "rejected",
            "vetoed",
            "决定",
            "选定",
            "采用",
            "放弃",
            "否决",
            "确认",
            "拍板",
            "定了",
            "choose",
            "opt for",
            "settled on",
            "final call",
            "go with",
            "trade-off",
            "trade off",
            "instead of",
            "rather than",
        ],
    ),
    (
        "bug",
        &[
            "broke",
            "broken",
            "failed",
            "crash",
            "error",
            "bug",
            "regression",
            "doesn't work",
            "not working",
            "exception",
            "timeout",
            "hang",
            "报错",
            "崩溃",
            "失败",
            "挂了",
            "卡住",
            "出错",
            "炸了",
            "fix",
            "hotfix",
            "patch",
            "workaround",
            "rollback",
            "segfault",
            "OOM",
            "memory leak",
            "deadlock",
            "race condition",
        ],
    ),
    (
        "preference",
        &[
            "prefer",
            "like",
            "always use",
            "never use",
            "favorite",
            "hate",
            "avoid",
            "dislike",
            "better than",
            "worse than",
            "喜欢",
            "讨厌",
            "偏好",
            "习惯用",
            "不要用",
            "受不了",
            "default to",
            "go-to",
            "style preference",
            "convention",
        ],
    ),
    (
        "architecture",
        &[
            "architecture",
            "design",
            "pattern",
            "schema",
            "migration",
            "refactor",
            "modular",
            "coupling",
            "interface",
            "contract",
            "架构",
            "设计",
            "重构",
            "模式",
            "模块",
            "接口",
            "契约",
            "system design",
            "data model",
            "API design",
            "trade-off",
        ],
    ),
    (
        "research",
        &[
            "found",
            "discovered",
            "learned",
            "researched",
            "investigated",
            "benchmarked",
            "compared",
            "evaluated",
            "analyzed",
            "tested",
            "发现",
            "调研",
            "对比",
            "评估",
            "分析",
            "验证",
            "测试",
            "turns out",
            "TIL",
            "insight",
            "observation",
            "conclusion",
        ],
    ),
];

const SEMANTIC_TYPE_PATTERNS: &[(&str, &[&str])] = &[
    (
        "decision",
        &[
            "decided",
            "chosen",
            "went with",
            "selected",
            "approved",
            "rejected",
            "vetoed",
            "agreed on",
            "committed to",
            "settled on",
            "final call",
            "go with",
            "trade-off",
            "决定",
            "选定",
            "确认",
            "拍板",
            "放弃",
            "否决",
            "instead of",
            "rather than",
            "over",
            "architecture decision",
            "ADR",
            "design decision",
        ],
    ),
    (
        "milestone",
        &[
            "shipped",
            "launched",
            "completed",
            "deployed",
            "released",
            "merged",
            "passed",
            "achieved",
            "reached",
            "finished",
            "上线",
            "发布",
            "完成",
            "通过",
            "达成",
            "交付",
            "v1",
            "v2",
            "version",
            "sprint",
            "MVP",
            "went live",
            "in production",
            "GA",
            "first user",
        ],
    ),
    (
        "problem",
        &[
            "broke",
            "broken",
            "failed",
            "crash",
            "error",
            "bug",
            "regression",
            "doesn't work",
            "not working",
            "exception",
            "timeout",
            "hang",
            "stuck",
            "blocked",
            "issue",
            "报错",
            "崩溃",
            "失败",
            "挂了",
            "卡住",
            "出错",
            "performance degradation",
            "flaky",
            "intermittent",
            "can't figure out",
            "struggling with",
        ],
    ),
    (
        "preference",
        &[
            "prefer",
            "like",
            "always use",
            "never use",
            "favorite",
            "hate",
            "avoid",
            "dislike",
            "better than",
            "worse than",
            "喜欢",
            "讨厌",
            "偏好",
            "习惯用",
            "不要用",
            "default to",
            "go-to",
            "my style",
            "convention",
            "don't like",
            "annoy",
            "wish",
            "should be",
        ],
    ),
    (
        "emotional",
        &[
            "frustrated",
            "excited",
            "worried",
            "anxious",
            "happy",
            "relieved",
            "confused",
            "overwhelmed",
            "proud",
            "disappointed",
            "angry",
            "scared",
            "hopeful",
            "grateful",
            "burned out",
            "沮丧",
            "兴奋",
            "担心",
            "焦虑",
            "开心",
            "失望",
            "崩溃",
            "无语",
            "绝望",
            "感动",
            "骄傲",
            "stressed",
            "tired",
            "energized",
            "motivated",
            "stuck",
        ],
    ),
];

const LAYER_KEYWORDS: &[(&str, &[&str])] = &[
    (
        "event_log",
        &[
            "今天",
            "昨天",
            "刚刚",
            "上午",
            "下午",
            "晚上",
            "本周",
            "发生",
            "完成",
            "提交",
            "修复",
            "today",
            "yesterday",
            "recently",
            "just now",
            "this week",
            "happened",
            "submitted",
            "fixed",
            "merged",
        ],
    ),
    (
        "verified_fact",
        &[
            "一直",
            "通常",
            "默认",
            "固定",
            "地址",
            "电话",
            "账号",
            "规则",
            "原则",
            "定义",
            "always",
            "usually",
            "default",
            "rule",
            "principle",
            "standard",
            "convention",
            "permanent",
        ],
    ),
];

#[derive(Debug, Clone, PartialEq)]
pub struct AutoClassificationResult {
    pub category: String,
    pub layer: String,
    pub confidence: f64,
    pub method: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct AutoClassifier {
    enable_llm_fallback: bool,
    rule_confidence_threshold: f64,
}

impl Default for AutoClassifier {
    fn default() -> Self {
        Self::new(true, 0.6)
    }
}

impl AutoClassifier {
    pub fn new(enable_llm_fallback: bool, rule_confidence_threshold: f64) -> Self {
        Self {
            enable_llm_fallback,
            rule_confidence_threshold: rule_confidence_threshold.clamp(0.0, 1.0),
        }
    }

    pub fn classify(&self, content: &str, layer_hint: Option<&str>) -> AutoClassificationResult {
        let rule_result = self.classify_with_rules(content, layer_hint);
        if !self.enable_llm_fallback || rule_result.confidence >= self.rule_confidence_threshold {
            return rule_result;
        }

        // Python v5.1 slim removed the LLM classifier and falls through to the
        // rule result. Preserve that behavior exactly.
        rule_result
    }

    pub fn detect_semantic_types(&self, content: &str) -> Vec<String> {
        let text = content.trim().to_lowercase();
        if text.is_empty() {
            return Vec::new();
        }

        let mut matched = SEMANTIC_TYPE_PATTERNS
            .iter()
            .filter_map(|(semantic_type, keywords)| {
                let hits = keywords
                    .iter()
                    .filter(|keyword| text.contains(&keyword.to_lowercase()))
                    .count();
                (hits >= 2).then(|| ((*semantic_type).to_string(), hits))
            })
            .collect::<Vec<_>>();

        let has_problem = matched.iter().any(|(name, _)| name == "problem");
        let has_milestone = matched.iter().any(|(name, _)| name == "milestone");
        if has_problem && has_milestone {
            matched.retain(|(name, _)| name != "problem");
        }

        matched.sort_by_key(|(_, hits)| std::cmp::Reverse(*hits));
        matched.into_iter().map(|(name, _)| name).collect()
    }

    fn classify_with_rules(
        &self,
        content: &str,
        layer_hint: Option<&str>,
    ) -> AutoClassificationResult {
        let text = content.trim().to_lowercase();
        if text.is_empty() {
            return AutoClassificationResult {
                category: DEFAULT_CATEGORY.to_string(),
                layer: layer_hint.unwrap_or(DEFAULT_LAYER).to_string(),
                confidence: 0.0,
                method: "default".to_string(),
                reason: "empty content".to_string(),
            };
        }

        let (category, category_hits) = match_keywords(&text, CATEGORY_KEYWORDS);
        let (layer, layer_hits) = match_keywords(&text, LAYER_KEYWORDS);

        let category_value = category.unwrap_or(DEFAULT_CATEGORY);
        let mut layer_value = layer
            .or_else(|| layer_hint.filter(|hint| VALID_LAYERS.contains(hint)))
            .unwrap_or(DEFAULT_LAYER);
        if !VALID_LAYERS.contains(&layer_value) {
            layer_value = DEFAULT_LAYER;
        }

        let confidence = confidence_from_hits(category_hits.max(layer_hits));
        let reason = if category_hits > 0 || layer_hits > 0 {
            format!("rule hits category={category_hits} layer={layer_hits}")
        } else {
            "rule default".to_string()
        };

        AutoClassificationResult {
            category: category_value.to_string(),
            layer: layer_value.to_string(),
            confidence,
            method: "rule".to_string(),
            reason,
        }
    }
}

fn match_keywords<'a>(
    text: &str,
    keyword_map: &'a [(&'a str, &[&str])],
) -> (Option<&'a str>, usize) {
    let mut best_key = None;
    let mut best_hits = 0;
    for (key, keywords) in keyword_map {
        let hits = keywords
            .iter()
            .filter(|keyword| text.contains(&keyword.to_lowercase()))
            .count();
        if hits > best_hits {
            best_key = Some(*key);
            best_hits = hits;
        }
    }
    (best_key, best_hits)
}

fn confidence_from_hits(hits: usize) -> f64 {
    match hits {
        0 => 0.35,
        1 => 0.5,
        2 => 0.6,
        _ => 0.75,
    }
}

#[cfg(test)]
mod tests {
    use super::AutoClassifier;

    fn classifier() -> AutoClassifier {
        AutoClassifier::new(false, 0.6)
    }

    #[test]
    fn rule_classification_prefers_event_for_time_based_content() {
        let result =
            AutoClassifier::new(false, 0.8).classify("今天下午在会议室开会，完成发布评审。", None);

        assert_eq!(result.category, "event");
        assert_eq!(result.layer, "event_log");
        assert_eq!(result.method, "rule");
        assert!(result.confidence > 0.0);
    }

    #[test]
    fn enhanced_categories_match_python_examples() {
        let classifier = classifier();
        assert_eq!(
            classifier
                .classify("We decided to go with option A, committed to this approach, trade-off accepted", None)
                .category,
            "decision"
        );
        assert_eq!(
            classifier
                .classify(
                    "The system crashed with a segfault, error in regression, fix needed",
                    None
                )
                .category,
            "bug"
        );
        assert_eq!(
            classifier
                .classify(
                    "Found and discovered that the benchmarked approach performs better after evaluation",
                    None,
                )
                .category,
            "research"
        );
    }

    #[test]
    fn semantic_type_examples_match_python_tests() {
        let classifier = classifier();
        let examples = [
            (
                "We decided to go with SQLite instead of PostgreSQL. Selected after trade-off analysis.",
                "decision",
            ),
            (
                "v2.0 shipped and deployed to production. First user onboarded. MVP completed.",
                "milestone",
            ),
            (
                "The search service crashed with a timeout error. Bug in regression test.",
                "problem",
            ),
            (
                "I prefer using ruff over pylint. Always use FastEmbed, never use OpenAI embeddings.",
                "preference",
            ),
            (
                "Feeling frustrated and overwhelmed with the deadline. Burned out from debugging.",
                "emotional",
            ),
            (
                "最终决定采用 MiniMax 替代 Gemini，拍板用 SQLite。",
                "decision",
            ),
            ("系统崩溃了，一直报错，卡住无法继续。", "problem"),
        ];
        for (content, expected) in examples {
            assert!(
                classifier
                    .detect_semantic_types(content)
                    .contains(&expected.to_string()),
                "{expected} missing for {content}"
            );
        }
    }

    #[test]
    fn semantic_type_mixed_and_disambiguation_match_python_tests() {
        let classifier = classifier();
        let mixed = classifier.detect_semantic_types(
            "Decided to switch to SQLite, selected after trade-off. Shipped the migration and deployed to production.",
        );
        assert!(mixed.contains(&"decision".to_string()));
        assert!(mixed.contains(&"milestone".to_string()));

        let resolved = classifier.detect_semantic_types(
            "The crash bug that broke production was fixed, shipped the hotfix, and deployed to production successfully.",
        );
        assert!(resolved.contains(&"milestone".to_string()));
        assert!(!resolved.contains(&"problem".to_string()));
    }

    #[test]
    fn semantic_type_empty_plain_and_single_hit_match_python_tests() {
        let classifier = classifier();
        assert!(classifier.detect_semantic_types("").is_empty());
        assert!(
            classifier
                .detect_semantic_types("This is a plain note about something.")
                .is_empty()
        );
        assert!(
            !classifier
                .detect_semantic_types("The word decided appears once here.")
                .contains(&"decision".to_string())
        );
    }

    #[test]
    fn empty_content_uses_python_defaults() {
        let result = classifier().classify("", Some("event_log"));
        assert_eq!(result.category, "event");
        assert_eq!(result.layer, "event_log");
        assert_eq!(result.confidence, 0.0);
        assert_eq!(result.method, "default");
        assert_eq!(result.reason, "empty content");
    }
}
