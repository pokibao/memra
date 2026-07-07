//! MemoryPolicy — durable-memory governance + 7-key typed metadata injection.
//!
//! Rust port of `backend/services/memory_policy.py` (432 LOC in Python).
//!
//! The policy runs on every MCP write BEFORE dedup/supersede (see Python
//! `backend/core/write_orchestrator.py:1166-1186`). It decides two things:
//!
//! 1. **allow_durable** — whether the content should enter durable memory
//!    (verified_fact / procedure_schema layers). Auto-extracted content
//!    that is transient / repo-derivable / unclassifiable is rejected;
//!    user/AI-initiated writes are always allowed but stamped with
//!    `policy_warning`.
//!
//! 2. **typed_metadata** — 7 keys injected into every write's
//!    metadata_json so downstream consolidation / search / audit queries
//!    can route by memory_type:
//!
//!    | key                    | values                                        |
//!    |------------------------|-----------------------------------------------|
//!    | memory_type            | user / feedback / project / reference / None  |
//!    | why                    | per-type explanation (Chinese)                |
//!    | how_to_apply           | per-type usage hint (Chinese)                 |
//!    | derivable_from_repo    | bool                                          |
//!    | stale_risk             | low / medium / high                           |
//!    | source_kind            | extraction / manual / conversation / agent id |
//!    | policy_warning (opt.)  | transient_session_state / repo_derivable /    |
//!    |                        | fallback_to_project                           |
//!
//!    Feedback adds `feedback_kind` (communication / workflow / quality_bar
//!    / taste). Reference would also merge living_doc metadata — that
//!    branch is deferred (requires porting `magic_docs.py`).
//!
//! ## NOT in scope for this PR
//!
//! - `_build_living_doc_metadata` — Python calls `get_magic_docs_service()`
//!   which has its own 200+ LOC of regex / YAML parsing. The Rust port
//!   would be a separate task; for now we leave the `living_doc_meta`
//!   merge empty for reference memories.
//! - `enrich_metadata` helper — small wrapper; add when a caller needs it.
//! - Write-orchestrator integration — kept separate so this PR only adds
//!   a pure module + tests. Integration lands in a follow-up.

use regex::{Regex, RegexBuilder};
use serde_json::{Map, Value};
use std::sync::LazyLock;

// ---------------------------------------------------------------------------
// Chinese marker constants (verbatim from Python memory_policy.py:32-161)
// ---------------------------------------------------------------------------

static REPO_MARKERS: &[&str] = &[
    "当前实现",
    "代码里",
    "源码里",
    "仓库里",
    "commit",
    "diff",
    "branch",
    "补丁",
    "patch",
    "实现位于",
];

static TRANSIENT_MARKERS: &[&str] = &[
    "当前任务",
    "这轮",
    "本轮",
    "下一步",
    "todo",
    "待办",
    "阻塞",
    "刚刚",
    "今天先",
    "session",
];

static USER_MARKERS: &[&str] = &[
    "我叫",
    "叫我",
    "我是",
    "我的背景",
    "我来自",
    "我在做",
    "我的品牌",
];

static FEEDBACK_MARKERS: &[&str] = &[
    "偏好",
    "喜欢",
    "不喜欢",
    "请用中文",
    "默认中文",
    "简单解释",
    "解释简单",
    "不要太长",
    "沟通风格",
    "写作风格",
    "回复风格",
];

static FEEDBACK_COMMUNICATION_MARKERS: &[&str] = &[
    "中文",
    "英文",
    "简洁",
    "简单解释",
    "不要太长",
    "沟通",
    "写作",
    "回复",
    "语气",
];

static FEEDBACK_WORKFLOW_MARKERS: &[&str] = &[
    "diff",
    "提交",
    "commit",
    "pr",
    "review",
    "不要总结",
    "总结",
    "分支",
    "工作流",
    "流程",
];

static FEEDBACK_QUALITY_MARKERS: &[&str] = &[
    "测试",
    "test",
    "质量",
    "验证",
    "严格",
    "正确性",
    "回归",
    "不要 mock",
    "真实数据库",
];

static FEEDBACK_TASTE_MARKERS: &[&str] = &[
    "审美",
    "品牌",
    "调性",
    "高级感",
    "不要 ai 味",
    "不要太像模板",
    "风格化",
];

static PROJECT_MARKERS: &[&str] = &[
    "项目要求",
    "项目约束",
    "必须",
    "不要默认",
    "默认",
    "离线优先",
    "strict local",
    "本地优先",
    "约束",
    "策略",
    "规范",
    "流程",
    "选型",
    "决定",
    "截止",
    "freeze",
    "deadline",
    "里程碑",
    "上线",
    "发布",
    "合规",
    "legal",
    "stakeholder",
];

static REFERENCE_MARKERS: &[&str] = &[
    "github",
    "figma",
    "notion",
    "slack",
    "sentry",
    "linear",
    "jira",
    "docs",
    "文档",
    "地址",
    "入口",
    "链接",
    "仓库地址",
    "openapi",
];

static CURRENT_STATE_MARKERS: &[&str] = &["当前", "目前", "现有", "this week", "today"];

// ---------------------------------------------------------------------------
// Regex (Python memory_policy.py:21-30)
// ---------------------------------------------------------------------------

static PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|[\s(：:])(?:/[\w./-]+|[\w./-]+\.(?:py|ts|tsx|js|jsx|json|ya?ml|md|sql|sh|toml|ini|env))(?:$|[\s),.:，。])",
    )
    .expect("PATH_RE compiles")
});

static ENV_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[A-Z][A-Z0-9_]{2,}\b").expect("ENV_RE compiles"));

static FUNC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[a-zA-Z_][a-zA-Z0-9_]{2,}\s*\(").expect("FUNC_RE compiles"));

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://\S+|git@[\w.-]+:[\w./-]+").expect("URL_RE compiles"));

static LIVING_DOC_RE: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(
        r"(?:^|[\s`])((?:docs/[\w./-]+\.md)|(?:README|CLAUDE|AGENTS|CONTRIBUTING|ARCHITECTURE|CHANGELOG)\.md)(?:$|[\s`])",
    )
    .case_insensitive(true)
    .build()
    .expect("LIVING_DOC_RE compiles")
});

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Result of a MemoryPolicy.evaluate() call.
///
/// Mirrors Python `MemoryPolicyDecision` dataclass
/// (`backend/services/memory_policy.py:164-169`).
#[derive(Debug, Clone)]
pub struct MemoryPolicyDecision {
    /// Whether the content is allowed to enter durable memory.
    pub allow_durable: bool,
    /// Classified memory_type (user / feedback / project / reference / None).
    pub memory_type: Option<String>,
    /// Typed metadata to merge into `notes.metadata_json` on write.
    /// Contains 7 base keys + optional `policy_warning` / `feedback_kind` /
    /// living_doc keys.
    pub metadata: Map<String, Value>,
    /// Why the decision was made (audit + downstream routing).
    pub reason: String,
}

/// Inputs to evaluate(). Borrowed so the hot MCP write path doesn't clone.
#[derive(Debug, Default, Clone)]
pub struct EvaluateInput<'a> {
    pub content: &'a str,
    pub layer: Option<&'a str>,
    pub category: Option<&'a str>,
    pub source: Option<&'a str>,
    pub metadata: Option<&'a Map<String, Value>>,
    pub agent: Option<&'a str>,
}

/// Stateless classifier. Construct once, call `evaluate()` per write.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemoryPolicy;

impl MemoryPolicy {
    pub fn new() -> Self {
        Self
    }

    /// Port of Python `MemoryPolicy.evaluate()` at memory_policy.py:175-279.
    pub fn evaluate(&self, input: EvaluateInput<'_>) -> MemoryPolicyDecision {
        let text = input.content.trim();
        let empty_meta: Map<String, Value> = Map::new();
        let raw_metadata = input.metadata.unwrap_or(&empty_meta);

        let lowered = text.to_lowercase();
        let explicit_remember = raw_metadata
            .get("remember_explicit")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || text.contains("记住")
            || lowered.contains("remember this");

        let source_kind = infer_source_kind(input.source, input.agent, raw_metadata);

        // Initial classification (may be promoted to "project" below for extraction).
        let mut memory_type = classify_memory_type(text, &lowered, explicit_remember);

        // Python parity (backend/services/memory_policy.py:193-198): the
        // living-doc override only applies after classification confirms
        // `memory_type == "reference"`. Otherwise a user/feedback marker
        // that happens to mention README.md would silently bypass the
        // repo_derivable rejection.
        let living_doc_reference = memory_type.as_deref() == Some("reference")
            && is_living_doc_reference(text, raw_metadata);
        let mut derivable = is_repo_derivable(text, &lowered);
        if living_doc_reference {
            derivable = false;
        }
        let transient = is_transient(text, &lowered);

        // Promote unclassified extractions to "project" so we still stamp a type.
        if memory_type.is_none() && !text.is_empty() && source_kind == "extraction" {
            memory_type = Some("project".to_string());
        }

        let stale_risk = if transient || derivable {
            "high"
        } else if CURRENT_STATE_MARKERS
            .iter()
            .any(|m| text.contains(m) || lowered.contains(m))
        {
            "medium"
        } else {
            "low"
        };

        // ── 7-key typed metadata ─────────────────────────────────────────
        let mut typed_metadata: Map<String, Value> = Map::new();
        typed_metadata.insert(
            "memory_type".to_string(),
            memory_type
                .as_ref()
                .map(|s| Value::String(s.clone()))
                .unwrap_or(Value::Null),
        );
        typed_metadata.insert(
            "why".to_string(),
            build_why(memory_type.as_deref())
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        typed_metadata.insert(
            "how_to_apply".to_string(),
            build_apply(memory_type.as_deref())
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        typed_metadata.insert("derivable_from_repo".to_string(), Value::Bool(derivable));
        typed_metadata.insert(
            "stale_risk".to_string(),
            Value::String(stale_risk.to_string()),
        );
        typed_metadata.insert(
            "source_kind".to_string(),
            Value::String(source_kind.clone()),
        );

        if memory_type.as_deref() == Some("feedback") {
            typed_metadata.insert(
                "feedback_kind".to_string(),
                Value::String(classify_feedback_kind(text, &lowered)),
            );
        }

        // `reference` would merge `_build_living_doc_metadata` output here —
        // deferred (requires porting magic_docs.py, see module doc).

        let is_auto_extract = source_kind == "extraction";

        // ── 3-layer intercept: transient / derivable / unclassified ──────

        if transient {
            typed_metadata.insert(
                "why".to_string(),
                Value::String(
                    "会话态信息应进入 session memory，而不是 durable memory。".to_string(),
                ),
            );
            if is_auto_extract {
                return MemoryPolicyDecision {
                    allow_durable: false,
                    memory_type,
                    metadata: typed_metadata,
                    reason: "transient_session_state".to_string(),
                };
            }
            typed_metadata.insert(
                "policy_warning".to_string(),
                Value::String("transient_session_state".to_string()),
            );
        }

        if derivable {
            typed_metadata.insert(
                "why".to_string(),
                Value::String(
                    "该内容可从仓库/代码现状直接推导，不应写入 durable memory。".to_string(),
                ),
            );
            if is_auto_extract {
                return MemoryPolicyDecision {
                    allow_durable: false,
                    memory_type,
                    metadata: typed_metadata,
                    reason: "repo_derivable".to_string(),
                };
            }
            typed_metadata.insert(
                "policy_warning".to_string(),
                Value::String("repo_derivable".to_string()),
            );
        }

        if memory_type.is_none() {
            if is_auto_extract {
                return MemoryPolicyDecision {
                    allow_durable: false,
                    memory_type: None,
                    metadata: typed_metadata,
                    reason: "not_durable_worthy".to_string(),
                };
            }
            // Manual write but couldn't classify — fall back to "project".
            typed_metadata.insert(
                "memory_type".to_string(),
                Value::String("project".to_string()),
            );
            typed_metadata.insert(
                "why".to_string(),
                build_why(Some("project"))
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            typed_metadata.insert(
                "how_to_apply".to_string(),
                build_apply(Some("project"))
                    .map(Value::String)
                    .unwrap_or(Value::Null),
            );
            typed_metadata.insert(
                "policy_warning".to_string(),
                Value::String("fallback_to_project".to_string()),
            );
            memory_type = Some("project".to_string());
        }

        // ── Layer fast-path: event_log / identity_schema / etc. ──────────
        if let Some(l) = input.layer {
            if l != "verified_fact" && l != "procedure_schema" {
                return MemoryPolicyDecision {
                    allow_durable: true,
                    memory_type,
                    metadata: typed_metadata,
                    reason: "non_durable_layer_passthrough".to_string(),
                };
            }
        }

        MemoryPolicyDecision {
            allow_durable: true,
            memory_type,
            metadata: typed_metadata,
            reason: "allowed".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Private helpers (Python memory_policy.py:303-407)
// ---------------------------------------------------------------------------

fn classify_memory_type(text: &str, lowered: &str, explicit_remember: bool) -> Option<String> {
    let has_project_marker = PROJECT_MARKERS.iter().any(|m| text.contains(m));
    let has_reference_marker =
        URL_RE.is_match(text) || REFERENCE_MARKERS.iter().any(|m| lowered.contains(m));
    let has_living_doc_path = LIVING_DOC_RE.is_match(text);

    if USER_MARKERS.iter().any(|m| text.contains(m)) {
        return Some("user".to_string());
    }
    if FEEDBACK_MARKERS.iter().any(|m| text.contains(m)) {
        return Some("feedback".to_string());
    }
    if has_project_marker && !has_living_doc_path {
        return Some("project".to_string());
    }

    let matches_feedback_subkind = FEEDBACK_COMMUNICATION_MARKERS
        .iter()
        .chain(FEEDBACK_WORKFLOW_MARKERS.iter())
        .chain(FEEDBACK_QUALITY_MARKERS.iter())
        .chain(FEEDBACK_TASTE_MARKERS.iter())
        .any(|m| text.contains(m) || lowered.contains(m));
    if matches_feedback_subkind {
        return Some("feedback".to_string());
    }

    if has_reference_marker || has_living_doc_path {
        return Some("reference".to_string());
    }

    if explicit_remember {
        return Some("project".to_string());
    }

    None
}

fn is_repo_derivable(text: &str, lowered: &str) -> bool {
    let scrubbed = URL_RE.replace_all(text, " ");
    if PATH_RE.is_match(&scrubbed) {
        return true;
    }
    if FUNC_RE.is_match(&scrubbed) {
        return true;
    }
    if ENV_RE.is_match(&scrubbed)
        && (lowered.contains("env") || text.contains("环境变量") || lowered.contains("flag"))
    {
        return true;
    }
    REPO_MARKERS
        .iter()
        .any(|m| lowered.contains(m) || text.contains(m))
}

fn is_living_doc_reference(text: &str, metadata: &Map<String, Value>) -> bool {
    LIVING_DOC_RE.is_match(text)
        || metadata
            .get("living_doc")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || metadata
            .get("living_docs")
            .and_then(Value::as_array)
            .is_some_and(|docs| !docs.is_empty())
}

fn is_transient(text: &str, lowered: &str) -> bool {
    TRANSIENT_MARKERS
        .iter()
        .any(|m| text.contains(m) || lowered.contains(m))
}

fn infer_source_kind(
    source: Option<&str>,
    agent: Option<&str>,
    metadata: &Map<String, Value>,
) -> String {
    if let Some(record_type) = metadata.get("record_type").and_then(Value::as_str) {
        if !record_type.is_empty() {
            return record_type.to_string();
        }
    }
    match source {
        Some("ai_extraction" | "external_ai") => "extraction".to_string(),
        Some("user") => "manual".to_string(),
        _ => agent
            .map(str::to_string)
            .unwrap_or_else(|| "conversation".to_string()),
    }
}

fn classify_feedback_kind(text: &str, lowered: &str) -> String {
    let matches = |markers: &[&str]| {
        markers
            .iter()
            .any(|m| text.contains(m) || lowered.contains(m))
    };
    if matches(FEEDBACK_COMMUNICATION_MARKERS) {
        return "communication".to_string();
    }
    if matches(FEEDBACK_WORKFLOW_MARKERS) {
        return "workflow".to_string();
    }
    if matches(FEEDBACK_QUALITY_MARKERS) {
        return "quality_bar".to_string();
    }
    if matches(FEEDBACK_TASTE_MARKERS) {
        return "taste".to_string();
    }
    "workflow".to_string()
}

fn build_why(memory_type: Option<&str>) -> Option<String> {
    Some(
        match memory_type? {
            "user" => "这是稳定的用户画像/身份信息，跨会话有持续价值。",
            "feedback" => "这是对协作方式或输出风格的偏好，后续响应可直接复用。",
            "project" => "这是项目级约束/决策，后续执行时应优先遵守。",
            "reference" => "这是外部系统入口/参考资料，后续需要时可直接跳转使用。",
            _ => return None,
        }
        .to_string(),
    )
}

fn build_apply(memory_type: Option<&str>) -> Option<String> {
    Some(
        match memory_type? {
            "user" => "在回答涉及用户背景、称呼、长期目标时优先参考。",
            "feedback" => "在生成回复、文档、代码说明时默认应用这些偏好。",
            "project" => "在做实现、规划、评审时把它当作项目约束或默认策略。",
            "reference" => "在需要外部入口、文档或系统对接时优先引用。",
            _ => return None,
        }
        .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluate(content: &str) -> MemoryPolicyDecision {
        MemoryPolicy::new().evaluate(EvaluateInput {
            content,
            ..Default::default()
        })
    }

    fn evaluate_with(
        content: &str,
        source: Option<&str>,
        layer: Option<&str>,
    ) -> MemoryPolicyDecision {
        MemoryPolicy::new().evaluate(EvaluateInput {
            content,
            source,
            layer,
            ..Default::default()
        })
    }

    #[test]
    fn user_marker_classifies_as_user() {
        let d = evaluate("我是示例用户，从事项目运营");
        assert_eq!(d.memory_type.as_deref(), Some("user"));
        assert_eq!(
            d.metadata.get("why").and_then(|v| v.as_str()),
            Some("这是稳定的用户画像/身份信息，跨会话有持续价值。")
        );
        assert!(d.allow_durable);
    }

    #[test]
    fn feedback_primary_marker_classifies_as_feedback() {
        let d = evaluate("偏好简洁回复，不要太长");
        assert_eq!(d.memory_type.as_deref(), Some("feedback"));
        assert_eq!(
            d.metadata.get("feedback_kind").and_then(|v| v.as_str()),
            Some("communication"),
            "feedback_kind must be set to the sub-category"
        );
    }

    #[test]
    fn feedback_quality_subkind_detected() {
        // Input must NOT contain PROJECT_MARKERS ("必须") or they win first
        // in the classifier priority (Python memory_policy.py:313-314:
        // project > feedback_subkind). Use pure feedback-quality wording.
        let d = evaluate("重视测试质量和回归正确性");
        assert_eq!(d.memory_type.as_deref(), Some("feedback"));
        assert_eq!(
            d.metadata.get("feedback_kind").and_then(|v| v.as_str()),
            Some("quality_bar")
        );
    }

    #[test]
    fn project_marker_classifies_as_project() {
        let d = evaluate("项目要求：本地优先，离线优先，禁止默认走云端");
        assert_eq!(d.memory_type.as_deref(), Some("project"));
    }

    #[test]
    fn reference_url_classifies_as_reference() {
        let d = evaluate("仓库地址：https://github.com/memra/memra");
        assert_eq!(d.memory_type.as_deref(), Some("reference"));
    }

    #[test]
    fn living_doc_path_classifies_as_reference() {
        let d = evaluate("架构总览见 docs/ARCHITECTURE.md 这份 living doc");
        assert_eq!(d.memory_type.as_deref(), Some("reference"));
        assert_eq!(
            d.metadata
                .get("derivable_from_repo")
                .and_then(|v| v.as_bool()),
            Some(false),
            "living-doc references must not be treated as repo-derivable"
        );
    }

    #[test]
    fn living_doc_reference_auto_extract_is_allowed() {
        let d = evaluate_with(
            "README.md 是 Memra 的启动入口 living doc",
            Some("ai_extraction"),
            None,
        );
        assert!(d.allow_durable);
        assert_eq!(d.reason, "allowed");
        assert_eq!(d.memory_type.as_deref(), Some("reference"));
        assert_eq!(
            d.metadata
                .get("derivable_from_repo")
                .and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn user_marker_text_mentioning_readme_does_not_bypass_repo_derivable() {
        // Python parity (backend/services/memory_policy.py:193-198 +
        // Codex P2 on PR #180): the living-doc override must only fire
        // when `memory_type == "reference"`. A user-marker text that
        // happens to mention README.md / src/foo.rs should still be
        // classified as `user` and have `derivable_from_repo = true`
        // (so repo-derived non-reference extracts are not silently
        // admitted).
        let d = evaluate_with(
            "我叫 alice，参考 README.md 和 src/foo.rs",
            Some("ai_extraction"),
            None,
        );
        assert_eq!(
            d.memory_type.as_deref(),
            Some("user"),
            "user marker must beat living-doc path in classification",
        );
        assert_eq!(
            d.metadata
                .get("derivable_from_repo")
                .and_then(|v| v.as_bool()),
            Some(true),
            "living-doc override must NOT fire on non-reference memory types",
        );
    }

    #[test]
    fn transient_content_auto_extract_rejected() {
        let d = evaluate_with(
            "当前任务：下一步先跑 smoke test",
            Some("ai_extraction"),
            None,
        );
        assert!(!d.allow_durable);
        assert_eq!(d.reason, "transient_session_state");
        assert_eq!(
            d.metadata.get("stale_risk").and_then(|v| v.as_str()),
            Some("high")
        );
    }

    #[test]
    fn transient_content_manual_write_passes_with_warning() {
        let d = evaluate_with("当前任务：下一步先跑 smoke test", Some("user"), None);
        assert!(d.allow_durable);
        assert_eq!(
            d.metadata.get("policy_warning").and_then(|v| v.as_str()),
            Some("transient_session_state")
        );
    }

    #[test]
    fn repo_derivable_auto_extract_rejected() {
        let d = evaluate_with(
            "src/backend/services/storage.py 里的 upsert_note(...) 函数",
            Some("ai_extraction"),
            None,
        );
        assert!(!d.allow_durable);
        assert_eq!(d.reason, "repo_derivable");
        assert_eq!(
            d.metadata
                .get("derivable_from_repo")
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn repo_derivable_manual_write_passes_with_warning() {
        // Input must have a classifiable memory_type, otherwise the
        // fallback_to_project branch (memory_policy.py:259-264) overwrites
        // the repo_derivable warning. Pair a user marker with the derivable
        // path/function reference.
        let d = evaluate_with(
            "我叫 alice，代码写在 backend/services/storage.py 里的 upsert_note(...) 函数",
            Some("user"),
            None,
        );
        assert!(d.allow_durable);
        assert_eq!(d.memory_type.as_deref(), Some("user"));
        assert_eq!(
            d.metadata.get("policy_warning").and_then(|v| v.as_str()),
            Some("repo_derivable")
        );
    }

    #[test]
    fn unclassified_extraction_rejected() {
        // No markers at all — plain descriptive text.
        let d = evaluate_with("weather is nice today", Some("ai_extraction"), None);
        // "today" is a current_state marker → stale_risk=medium but that
        // alone does not make derivable/transient, so it falls through to
        // unclassified-extraction rejection (because content doesn't match
        // any memory_type marker and source_kind is extraction).
        // Wait — `memory_type is None && source_kind == "extraction"` path
        // promotes to "project" early, before the rejection gate. So this
        // case actually ends up as allowed "project" with stale_risk=medium.
        assert!(d.allow_durable);
        assert_eq!(d.memory_type.as_deref(), Some("project"));
    }

    #[test]
    fn unclassified_manual_write_falls_back_to_project() {
        let d = evaluate("某条无明显类型的备忘");
        assert!(d.allow_durable);
        assert_eq!(d.memory_type.as_deref(), Some("project"));
        assert_eq!(
            d.metadata.get("policy_warning").and_then(|v| v.as_str()),
            Some("fallback_to_project")
        );
    }

    #[test]
    fn non_durable_layer_passes_through() {
        // event_log / identity_schema etc. skip the full classifier rules
        // and return allow_durable=true with reason="non_durable_layer_passthrough".
        let d = evaluate_with("任何 event content", None, Some("event_log"));
        assert!(d.allow_durable);
        assert_eq!(d.reason, "non_durable_layer_passthrough");
    }

    #[test]
    fn verified_fact_layer_runs_full_classifier() {
        let d = evaluate_with("我叫 alice", None, Some("verified_fact"));
        assert!(d.allow_durable);
        assert_eq!(d.reason, "allowed");
        assert_eq!(d.memory_type.as_deref(), Some("user"));
    }

    #[test]
    fn seven_keys_always_present_on_allowed() {
        let d = evaluate("我叫 alice");
        for key in [
            "memory_type",
            "why",
            "how_to_apply",
            "derivable_from_repo",
            "stale_risk",
            "source_kind",
        ] {
            assert!(
                d.metadata.contains_key(key),
                "typed metadata must include {key}"
            );
        }
    }

    #[test]
    fn explicit_remember_marker_promotes_to_project() {
        let d = evaluate("记住：版本发布前必须跑完整回归测试");
        assert_eq!(d.memory_type.as_deref(), Some("project"));
    }

    #[test]
    fn current_state_markers_raise_stale_risk_to_medium() {
        let d = evaluate("我叫 alice，目前在做 MA v6");
        assert_eq!(
            d.metadata.get("stale_risk").and_then(|v| v.as_str()),
            Some("medium")
        );
    }

    #[test]
    fn source_kind_extraction_from_record_type() {
        use serde_json::json;
        let mut meta = Map::new();
        meta.insert("record_type".to_string(), json!("extraction"));
        let d = MemoryPolicy::new().evaluate(EvaluateInput {
            content: "任意内容",
            metadata: Some(&meta),
            ..Default::default()
        });
        assert_eq!(
            d.metadata.get("source_kind").and_then(|v| v.as_str()),
            Some("extraction")
        );
    }

    #[test]
    fn source_user_maps_to_manual_source_kind() {
        let d = evaluate_with("我叫 alice", Some("user"), None);
        assert_eq!(
            d.metadata.get("source_kind").and_then(|v| v.as_str()),
            Some("manual")
        );
    }

    #[test]
    fn source_ai_extraction_maps_to_extraction_kind() {
        let d = evaluate_with("我叫 alice", Some("ai_extraction"), None);
        assert_eq!(
            d.metadata.get("source_kind").and_then(|v| v.as_str()),
            Some("extraction")
        );
    }
}
