//! Three-axis hybrid scoring: relevance + freshness + trust.
//!
//! Ported from `backend/services/scoring.py` (Sprint 11 overhaul).
//! Parity gate: all 20 boundary samples must match Python within |Δ| < 1e-6.
//! See DESIGN_DECISIONS.md D1.1.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const OUTDATED_PENALTY_DEFAULT: f64 = 0.10;
const OUTDATED_PENALTY_HISTORY: f64 = 0.85;

// Evolution state score factors (from memory_lifecycle.py)
const FACTOR_ACTIVE: f64 = 1.0;
const FACTOR_SUPERSEDED: f64 = 0.05;
const FACTOR_DEPRECATED: f64 = 0.02;
const FACTOR_CONTRADICTED: f64 = 0.03;
const FACTOR_LEGACY_SUPERSEDED: f64 = 0.10;
const FACTOR_CORRECTED: f64 = 0.15;

// Layer-specific decay parameters: (half_life_days, floor)
fn layer_decay(layer: &str) -> (f64, f64) {
    match layer {
        "event_log" => (30.0, 0.50),
        "verified_fact" => (90.0, 0.65),
        "procedure_schema" => (180.0, 0.75),
        "identity_schema" => (365.0, 0.90),
        _ => (90.0, 0.65),
    }
}

// ---------------------------------------------------------------------------
// Marker sets (temporal intent detection)
// ---------------------------------------------------------------------------

// CURRENT_STATE_MARKERS_ZH cover three concentric circles of vocabulary that
// all signal "user wants the latest, not the historical" — bigger than just
// `当前 / 现状` because alice's actual phrasing in chat traffic spans much
// further than the literal status words.
//
//   1. Original literals (现在/当前/目前/最新/现状/状态/...).
//   2. Python build2 sprint-12 evaluation/meta-evaluation vocabulary:
//      项目{状态,进度,最近,方向} / {进度,如何,怎么样,近期,本周,这周} /
//      {差距,对标,愿景,北极星,方向,评估,盘点,回顾}.
//      Rust drifted from Python after build2-ma-search-recall-fix-q1q3-v2
//      (commits c224aab/0d4393e/0627b25 on 2026-04-28); this is the parity
//      catch-up.
//   3. alice spoken-style triggers observed in 4-13 / 4-20 / 5-03 / 5-04
//      sessions: 回忆/回一下/上下文/今天/昨晚/这两天/刚做完/什么情况/在执行.
//      Without these, `回一下上下文` / `Hermes 在执行` fall through to
//      generic 90/2/8 weights and 25-day-old memories beat today's work —
//      which is the reverse-north-star bug encoded in the
//      `wake_search_drift_red` integration tests.
pub static CURRENT_STATE_MARKERS_ZH: &[&str] = &[
    // (1) Original literals
    "现在",
    "当前",
    "目前",
    "最新",
    "现状",
    "状态",
    "还在用吗",
    "还在吗",
    "仍在",
    "还用吗",
    "还活着吗",
    "是否还在",
    "是否还用",
    "最近状态",
    // (2) Python build2 evaluation/meta-evaluation vocabulary
    "项目状态",
    "项目进度",
    "项目最近",
    "项目方向",
    "进度",
    "如何",
    "怎么样",
    "近期",
    "本周",
    "这周",
    "差距",
    "对标",
    "愿景",
    "北极星",
    "方向",
    "评估",
    "盘点",
    "回顾",
    // (3) alice spoken-style triggers (5-04 drift discovery)
    "回忆",
    "回一下",
    "上下文",
    "今天",
    "昨晚",
    "这两天",
    "刚做完",
    "什么情况",
    "在执行",
];
pub static CURRENT_STATE_MARKERS_EN: &[&str] = &[
    "current",
    "currently",
    "latest",
    "latest status",
    "current status",
    "still using",
    "still in use",
    "still active",
    "active now",
    "status now",
    // Python build2 substring-safe phrase markers
    "how is",
    "how's",
    "evaluate",
];
pub static HISTORY_MARKERS_ZH: &[&str] = &[
    "上次",
    "之前",
    "以前",
    "最近",
    "昨天",
    "前天",
    "刚才",
    "当初",
    "那时",
    "那天",
    "那年",
    "那个时候",
    "最初",
    "开始时",
    "后来",
    "第一次",
    "最后一次",
    "上一次",
    "什么时候",
    "多久",
    "多少天",
    "先",
    "后",
    "之后",
    "以后",
    "从那以后",
    "在此之前",
    "在那之后",
    "为什么后来",
    "时间线",
];
pub static HISTORY_MARKERS_EN: &[&str] = &[
    "previous",
    "last time",
    "recently",
    "before",
    "earlier",
    "when did",
    "how long",
    "how many days",
    "how many weeks",
    "how many months",
    "first time",
    "last time",
    "at the time",
    "back then",
    "used to",
    "after",
    "since",
    "until",
    "once",
    "ago",
    "timeline",
    "order",
    "which came first",
    "which happened first",
    "before or after",
    "what date",
    "what time",
    "what month",
    "what year",
];
static REMOTE_HISTORY_MARKERS_ZH: &[&str] =
    &["当初", "以前", "最初", "早先", "过去", "那年", "那时"];
static REMOTE_HISTORY_MARKERS_EN: &[&str] =
    &["back then", "originally", "formerly", "used to", "initial"];

static CJK_STOPWORDS: &[char] = &[
    '的', '了', '是', '在', '和', '与', '也', '都', '就', '把', '被', '给', '让', '从', '个', '什',
    '么', '这', '那', '它', '有', '没', '着', '过', '之', '于', '以', '及', '吧', '呢', '吗', '而',
    '或', '且', '则', '为', '所', '由',
];

static TYPED_FACT_CATEGORIES: &[&str] = &["assistant_response", "preference"];

static VAGUE_KEYWORDS: &[&str] = &["工具", "方案", "项目", "记忆", "记录", "讨论", "流程"];

static PERSON_MARKERS: &[&str] = &["alice", "bob", "mary", "ceo", "demo portrait"];

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Input payload fields needed by scoring (mirrors Python `payload` dict).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScoringPayload {
    pub content: String,
    pub layer: String,
    pub confidence: Option<f64>,
    pub created_at: Option<String>,
    pub evolution_state: Option<String>,
    pub category: Option<String>,
    pub source: Option<String>,
}

/// Metadata fields used by scoring (from `metadata_json`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScoringMetadata {
    pub failure_count: Option<i64>,
    pub superseded_by: Option<String>,
    pub is_outdated: Option<bool>,
    pub is_corrected: Option<bool>,
    pub search_aliases: Option<serde_json::Value>,
    pub aliases: Option<serde_json::Value>,
    pub task_slug: Option<serde_json::Value>,
    pub source_tasks: Option<serde_json::Value>,
    pub source_task: Option<serde_json::Value>,
}

/// All inputs to the scoring function.
#[derive(Debug)]
pub struct ScoreInput<'a> {
    pub query: &'a str,
    pub query_vector: Option<&'a [f32]>,
    pub payload: &'a ScoringPayload,
    pub stored_vector: Option<&'a [f32]>,
    pub bm25_score: Option<f64>,
    pub metadata: &'a ScoringMetadata,
    pub boost_categories: Option<&'a HashSet<String>>,
    pub recall_count: i64,
    pub last_recalled_at: Option<&'a str>,
}

/// Debug output with all factor values.
#[derive(Debug, Serialize)]
pub struct ScoreDebug {
    pub score: f64,
    pub relevance: f64,
    pub vector_component: f64,
    pub bm25_component: f64,
    pub token_score: f64,
    pub exact_match: f64,
    pub freshness: f64,
    pub time_decay_raw: f64,
    pub activation_bonus: f64,
    pub has_temporal: bool,
    pub trust: f64,
    pub confidence_factor: f64,
    pub failure_factor: f64,
    pub w_rel: f64,
    pub w_fresh: f64,
    pub w_trust: f64,
    pub evolution_state_factor: f64,
    pub outdated_factor: f64,
    pub sku_factor: f64,
    pub brand_authority_factor: f64,
    pub semantic_type_factor: f64,
    pub primary_task_match: bool,
    pub secondary_task_match: bool,
    pub canonical_task_rank_guard: f64,
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn normalize_text(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut result = String::with_capacity(lower.len());
    let mut prev_space = true;
    for ch in lower.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                result.push(' ');
                prev_space = true;
            }
        } else {
            result.push(ch);
            prev_space = false;
        }
    }
    if result.ends_with(' ') {
        result.pop();
    }
    result
}

fn is_cjk_char(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
}

fn is_cjk_text(s: &str) -> bool {
    s.chars().any(is_cjk_char)
}

fn is_cjk_stopword(c: char) -> bool {
    CJK_STOPWORDS.contains(&c)
}

fn has_current_state_intent(query: &str) -> bool {
    let q = query.to_lowercase();
    CURRENT_STATE_MARKERS_ZH.iter().any(|m| q.contains(m))
        || CURRENT_STATE_MARKERS_EN.iter().any(|m| q.contains(m))
}

fn has_history_intent(query: &str) -> bool {
    if has_current_state_intent(query) {
        return false;
    }
    let q = query.to_lowercase();
    HISTORY_MARKERS_ZH.iter().any(|m| q.contains(m))
        || HISTORY_MARKERS_EN.iter().any(|m| q.contains(m))
}

fn has_remote_history_intent(query: &str) -> bool {
    let q = query.to_lowercase();
    REMOTE_HISTORY_MARKERS_ZH.iter().any(|m| q.contains(m))
        || REMOTE_HISTORY_MARKERS_EN.iter().any(|m| q.contains(m))
}

pub fn has_temporal_markers(query: &str) -> bool {
    has_current_state_intent(query) || has_history_intent(query)
}

/// Token score: (ratio, hits, total, position_bonus).
fn token_score(query: &str, content: &str) -> (f64, usize, usize, f64) {
    let q = normalize_text(query);
    let c = normalize_text(content);
    if q.is_empty() || c.is_empty() {
        return (0.0, 0, 0, 0.0);
    }

    let mut tokens: Vec<String> = Vec::new();
    let whitespace_tokens: Vec<&str> = q.split_whitespace().collect();

    if whitespace_tokens.len() <= 1 && is_cjk_text(&q) {
        // CJK bigram tokenization
        for seg in split_cjk_segments(&q) {
            if is_cjk_text(&seg) {
                let chars: Vec<char> = seg
                    .chars()
                    .filter(|&c| is_cjk_char(c) && !is_cjk_stopword(c))
                    .collect();
                if chars.len() >= 2 {
                    for i in 0..chars.len() - 1 {
                        let mut bigram = String::with_capacity(6);
                        bigram.push(chars[i]);
                        bigram.push(chars[i + 1]);
                        tokens.push(bigram);
                    }
                } else {
                    for ch in chars {
                        tokens.push(ch.to_string());
                    }
                }
            } else {
                for word in seg.split_whitespace() {
                    if !word.is_empty() {
                        tokens.push(word.to_string());
                    }
                }
            }
        }
    } else {
        tokens = whitespace_tokens
            .iter()
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .collect();
    }

    if tokens.is_empty() {
        return (0.0, 0, 0, 0.0);
    }

    let hits = tokens
        .iter()
        .filter(|t| !t.is_empty() && c.contains(t.as_str()))
        .count();

    // Position bonus: tokens in first 120 chars
    let first_120: String = c.chars().take(120).collect();
    let pos_hits = tokens
        .iter()
        .filter(|t| !t.is_empty() && first_120.contains(t.as_str()))
        .count();
    let pos_bonus = 0.06 * pos_hits as f64 / tokens.len() as f64;

    (
        hits as f64 / tokens.len().max(1) as f64,
        hits,
        tokens.len(),
        pos_bonus,
    )
}

/// Split text into alternating CJK and non-CJK segments.
fn split_cjk_segments(s: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut in_cjk = false;
    let mut first = true;

    for ch in s.chars() {
        let ch_is_cjk = is_cjk_char(ch);
        if first {
            in_cjk = ch_is_cjk;
            first = false;
        } else if ch_is_cjk != in_cjk {
            if !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
            in_cjk = ch_is_cjk;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

fn compute_time_decay(created_at: Option<&str>, half_life_days: f64) -> f64 {
    let Some(created_str) = created_at else {
        return 1.0;
    };

    let dt = match parse_datetime(created_str) {
        Some(d) => d,
        None => return 1.0,
    };

    let age_secs = (Utc::now() - dt).num_seconds().max(0) as f64;
    let age_days = age_secs / 86400.0;
    (-age_days * f64::ln(2.0) / half_life_days).exp()
}

fn parse_datetime(s: &str) -> Option<DateTime<Utc>> {
    // Try ISO 8601 with timezone
    if let Ok(dt) = DateTime::parse_from_rfc3339(&s.replace(' ', "T").replace("Z", "+00:00")) {
        return Some(dt.with_timezone(&Utc));
    }
    // Try without timezone (assume UTC)
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(naive.and_utc());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(naive.and_utc());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(naive.and_utc());
    }
    None
}

fn compute_activation(recall_count: i64, last_recalled_at: Option<&str>) -> f64 {
    if recall_count <= 0 {
        return 0.0;
    }
    let base = (0.05 * ((recall_count + 1) as f64).ln()).min(0.15);
    if let Some(last) = last_recalled_at {
        let recency = compute_time_decay(Some(last), 14.0);
        base * (0.5 + 0.5 * recency)
    } else {
        base
    }
}

pub fn cosine_similarity(v1: &[f32], v2: &[f32]) -> f64 {
    if v1.is_empty() || v2.is_empty() || v1.len() != v2.len() {
        return 0.0;
    }
    let mut dot: f64 = 0.0;
    let mut norm1: f64 = 0.0;
    let mut norm2: f64 = 0.0;
    for (a, b) in v1.iter().zip(v2.iter()) {
        let af = *a as f64;
        let bf = *b as f64;
        dot += af * bf;
        norm1 += af * af;
        norm2 += bf * bf;
    }
    if norm1 <= 0.0 || norm2 <= 0.0 {
        return 0.0;
    }
    (dot / (norm1.sqrt() * norm2.sqrt())).clamp(0.0, 1.0)
}

fn normalize_evolution_state(state: Option<&str>) -> &'static str {
    match state {
        Some("active") => "active",
        Some("superseded") => "superseded",
        Some("deprecated") => "deprecated",
        Some("contradicted") => "contradicted",
        _ => "active",
    }
}

fn evolution_state_factor(state: Option<&str>, metadata: &ScoringMetadata) -> (String, f64) {
    let norm = normalize_evolution_state(state);
    if norm != "active" {
        let factor = match norm {
            "superseded" => FACTOR_SUPERSEDED,
            "deprecated" => FACTOR_DEPRECATED,
            "contradicted" => FACTOR_CONTRADICTED,
            _ => FACTOR_ACTIVE,
        };
        return (norm.to_string(), factor);
    }
    if metadata.superseded_by.is_some() {
        return ("superseded".to_string(), FACTOR_LEGACY_SUPERSEDED);
    }
    ("active".to_string(), 1.0)
}

fn staleness_factor(metadata: &ScoringMetadata, has_history: bool) -> (f64, bool) {
    if metadata.is_outdated == Some(true) {
        let penalty = if has_history {
            OUTDATED_PENALTY_HISTORY
        } else {
            OUTDATED_PENALTY_DEFAULT
        };
        return (penalty, false);
    }
    if metadata.is_corrected == Some(true) {
        return (FACTOR_CORRECTED, true);
    }
    (1.0, false)
}

/// Check if a string looks like a task slug (e.g. "build2-ma-rrf-fusion").
fn is_task_slug_like(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Must be lowercase alphanumeric with at least 2 separators
    let parts: Vec<&str> = s.split(['-', '_']).collect();
    if parts.len() < 3 {
        return false;
    }
    parts.iter().all(|p| {
        !p.is_empty()
            && p.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    })
}

fn extract_query_task_slugs(query: &str) -> Vec<String> {
    let normalized = normalize_text(query);
    // Find task-slug-like patterns
    let mut slugs = Vec::new();
    for word in normalized.split_whitespace() {
        if is_task_slug_like(word) {
            slugs.push(word.to_string());
        }
    }
    // Also check for slug-like patterns with hyphens embedded in the text
    for cap in regex_task_slugs(&normalized) {
        if !slugs.contains(&cap) {
            slugs.push(cap);
        }
    }
    slugs
}

fn regex_task_slugs(s: &str) -> Vec<String> {
    let mut results = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Find start of potential slug
        if bytes[i].is_ascii_lowercase() || bytes[i].is_ascii_digit() {
            let start = i;
            let mut sep_count = 0;
            while i < bytes.len()
                && (bytes[i].is_ascii_lowercase()
                    || bytes[i].is_ascii_digit()
                    || bytes[i] == b'-'
                    || bytes[i] == b'_')
            {
                if bytes[i] == b'-' || bytes[i] == b'_' {
                    sep_count += 1;
                }
                i += 1;
            }
            if sep_count >= 2 {
                let candidate = &s[start..i];
                if is_task_slug_like(candidate) {
                    results.push(candidate.to_string());
                }
            }
        } else {
            i += 1;
        }
    }
    results
}

/// Extract task aliases from metadata JSON values.
fn extract_aliases_from_value(value: &serde_json::Value) -> Vec<String> {
    let mut aliases = Vec::new();
    match value {
        serde_json::Value::String(s) => {
            let norm = normalize_text(s);
            if !norm.is_empty() {
                aliases.push(norm.clone());
                for part in norm.split([',', ';', '\n']) {
                    let p = part.trim().to_string();
                    if !p.is_empty() && p != norm {
                        aliases.push(p);
                    }
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                aliases.extend(extract_aliases_from_value(item));
            }
        }
        _ => {}
    }
    aliases
}

fn extract_inline_task_slugs(content: &str, field: &str) -> Vec<String> {
    let pattern = match field {
        "task_slug" => "task_slug",
        "source_tasks" => "source_task",
        _ => return Vec::new(),
    };
    let mut results = Vec::new();
    for line in content.lines() {
        let lower = line.to_lowercase();
        if let Some(pos) = lower.find(pattern) {
            if let Some(colon) = line[pos..].find(':') {
                let val = line[pos + colon + 1..].trim();
                let end = val.find(['|', '\n']).unwrap_or(val.len());
                let slug_text = normalize_text(&val[..end]);
                if is_task_slug_like(&slug_text) {
                    results.push(slug_text);
                }
            }
        }
    }
    results
}

// Regex-like code identifier detection
fn find_code_identifiers(query: &str) -> Vec<String> {
    let normalized = normalize_text(query);
    let mut idents = Vec::new();
    // Simple scan for patterns:
    // snake_case, ALLCAPS, alphanum_mix, dotted
    for word in normalized.split(|c: char| c.is_whitespace() || c == ',' || c == '|') {
        let w = word.trim();
        if w.is_empty() {
            continue;
        }
        // snake_case: contains underscore with alphanumeric
        if w.contains('_')
            && w.len() >= 3
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            idents.push(w.to_string());
            continue;
        }
        // ALLCAPS: 2+ uppercase letters
        if w.len() >= 2
            && w.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-')
            && w.chars().any(|c| c.is_ascii_uppercase())
        {
            idents.push(w.to_string());
            continue;
        }
        // Dotted: foo.bar
        if w.contains('.')
            && w.len() >= 3
            && w.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        {
            idents.push(w.to_string());
            continue;
        }
        // Alphanum mix: contains digit mixed with letters (e.g. BM25, v4)
        if w.chars().any(|c| c.is_ascii_digit())
            && w.chars().any(|c| c.is_ascii_alphabetic())
            && w.len() >= 2
        {
            idents.push(w.to_string());
        }
    }
    idents
}

// Known tools list (subset — full list is in scoring.py)
static KNOWN_TOOLS: &[&str] = &[
    "memory anchor",
    "comfyui",
    "photoshop",
    "得物",
    "小红书",
    "instagram",
    "shopify",
    "tiktok",
    "demo-brand",
    "kimi",
    "gemma",
    "claude",
    "openai",
    "chatgpt",
    "midjourney",
    "sqlite",
    "qdrant",
    "postgresql",
    "redis",
    "docker",
    "git",
    "github",
    "vscode",
    "cursor",
    "windsurf",
    "workbench",
    "wal",
    "fts",
    "fts5",
    "bm25",
    "vercel",
    "cloudflare",
    "pages",
    "ollama",
    "fastapi",
    "express",
    "react",
    "next.js",
    "tailwind",
    "seedance",
    "seedream",
    "flux",
    "schnell",
    "wechatpadpro",
    "wireguard",
    "tailscale",
    "paperclip",
    "mission control",
    "playwright",
    "browser-use",
    "rsync",
    "pm2",
    "akm",
    "lemon squeezy",
    "mcp",
    "fastembed",
];

static TECHNICAL_TERMS: &[&str] = &[
    "bug",
    "error",
    "crash",
    "failure",
    "exception",
    "upsert",
    "checkpoint",
    "sqlite",
    "fts5",
    "token",
    "vector",
    "embedding",
    "llm",
    "synapse",
    "consolidation",
    "dedup",
    "pr",
    "repo",
    "git",
    "cli",
    "api",
    "json",
    "jsonl",
    "config",
    "yaml",
    "env",
    "proxy",
    "port",
    "host",
    "server",
    "client",
    "request",
    "response",
    "mcp",
    "fastembed",
    "nomic",
    "embed",
    "wal",
    "wal-mode",
    "deploy",
    "deployment",
    "部署",
    "上线",
    "freshness",
    "decay",
    "时间衰减",
    "衰减",
    "recall",
    "mrr",
    "ndcg",
    "benchmark",
    "environment",
    "variable",
    "环境变量",
    "认证",
    "auth",
    "tokenization",
    "indexing",
    "pipeline",
    "schema",
    "metadata",
    "payload",
    "id",
    "uuid",
    "async",
    "await",
    "sync",
    "concurrent",
    "thread",
    "process",
    "memory",
    "storage",
    "database",
    "query",
    "search",
    "retrieval",
    "scoring",
    "ranking",
    "mrr@5",
    "ndcg@5",
    "top-k",
    "latency",
    "performance",
    "optimization",
    "根因",
    "原因",
    "失效",
    "故障",
    "报错",
    "问题",
    "root-cause",
    "reason",
    "fault",
    "architecture",
    "design",
    "pattern",
    "singleton",
    "isolation",
    "conftest",
    "fixture",
    "workflow",
    "pipeline",
    "adapter",
    "middleware",
    "webhook",
    "intent",
    "routing",
    "logic",
    "implementation",
];

// ---------------------------------------------------------------------------
// Main scoring function
// ---------------------------------------------------------------------------

/// Score a single search result. Returns final score in [0, 1].
pub fn score_result(input: &ScoreInput<'_>) -> f64 {
    score_result_inner(input).0
}

/// Score with full debug output.
pub fn score_result_debug(input: &ScoreInput<'_>) -> ScoreDebug {
    let (score, debug) = score_result_inner(input);
    debug.unwrap_or(ScoreDebug {
        score,
        relevance: 0.0,
        vector_component: 0.0,
        bm25_component: 0.0,
        token_score: 0.0,
        exact_match: 0.0,
        freshness: 0.0,
        time_decay_raw: 0.0,
        activation_bonus: 0.0,
        has_temporal: false,
        trust: 0.0,
        confidence_factor: 0.0,
        failure_factor: 0.0,
        w_rel: 0.0,
        w_fresh: 0.0,
        w_trust: 0.0,
        evolution_state_factor: 0.0,
        outdated_factor: 0.0,
        sku_factor: 0.0,
        brand_authority_factor: 0.0,
        semantic_type_factor: 0.0,
        primary_task_match: false,
        secondary_task_match: false,
        canonical_task_rank_guard: 1.0,
    })
}

fn score_result_inner(input: &ScoreInput<'_>) -> (f64, Option<ScoreDebug>) {
    let query = input.query;
    if query.is_empty() {
        return (1.0, None);
    }

    let content = &input.payload.content;
    let normalized_query = normalize_text(query);
    let normalized_content = normalize_text(content);

    // Exact match
    let exact = if !normalized_query.is_empty() && normalized_content.contains(&normalized_query) {
        1.0
    } else {
        0.0
    };

    let (token, token_hits, token_total, token_pos_bonus) = token_score(query, content);

    // BM25 component
    let has_bm25 = input.bm25_score.is_some();
    let bm25_component = if let Some(bm25) = input.bm25_score {
        let raw = (-bm25).max(0.0);
        raw / (1.0 + raw)
    } else {
        0.0
    };

    // Vector component
    let vector_component = match (input.query_vector, input.stored_vector) {
        (Some(qv), Some(sv)) => cosine_similarity(qv, sv),
        _ => 0.0,
    };

    // Task slug matching
    let query_task_slugs = extract_query_task_slugs(query);
    let mut primary_aliases: Vec<String> = Vec::new();
    let mut secondary_aliases: Vec<String> = Vec::new();

    // From metadata
    for val in [
        &input.metadata.search_aliases,
        &input.metadata.aliases,
        &input.metadata.task_slug,
    ]
    .into_iter()
    .flatten()
    {
        primary_aliases.extend(extract_aliases_from_value(val));
    }
    for val in [&input.metadata.source_tasks, &input.metadata.source_task]
        .into_iter()
        .flatten()
    {
        secondary_aliases.extend(extract_aliases_from_value(val));
    }
    // From inline content
    primary_aliases.extend(extract_inline_task_slugs(content, "task_slug"));
    secondary_aliases.extend(extract_inline_task_slugs(content, "source_tasks"));

    primary_aliases.retain(|a| is_task_slug_like(a));
    secondary_aliases.retain(|a| is_task_slug_like(a));

    let primary_task_match = query_task_slugs.iter().any(|s| primary_aliases.contains(s));
    let secondary_task_match = query_task_slugs
        .iter()
        .any(|s| secondary_aliases.contains(s));

    // Query characteristics
    let cjk_count = normalized_query.chars().filter(|&c| is_cjk_char(c)).count();
    let is_short_cjk = (2..=4).contains(&cjk_count) && token_total <= 5;
    let is_very_short = normalized_query.len() < 4;
    let is_cjk_only = cjk_count > 0 && !normalized_query.chars().any(|c| c.is_ascii_alphabetic());

    let en_tokens_q: Vec<&str> = normalized_query
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty() && t.chars().any(|c| c.is_ascii_alphabetic()))
        .collect();
    let is_long_en = en_tokens_q.len() > 3;

    let code_idents = find_code_identifiers(&normalized_query);
    let has_tech_terms =
        TECHNICAL_TERMS.iter().any(|t| normalized_query.contains(t)) || !code_idents.is_empty();

    // Relevance score mixing
    let has_qv = input.query_vector.is_some();
    let mut score = if has_qv {
        if has_bm25 {
            if is_very_short {
                if has_tech_terms {
                    0.40 * vector_component + 0.40 * bm25_component + 0.20 * token
                } else {
                    0.60 * vector_component + 0.20 * bm25_component + 0.20 * token
                }
            } else if is_long_en {
                if has_tech_terms {
                    0.45 * vector_component + 0.35 * bm25_component + 0.20 * token
                } else {
                    0.60 * vector_component + 0.20 * bm25_component + 0.20 * token
                }
            } else if is_short_cjk {
                if has_tech_terms {
                    0.40 * vector_component + 0.30 * bm25_component + 0.30 * token
                } else if is_cjk_only {
                    0.45 * vector_component + 0.25 * bm25_component + 0.30 * token
                } else {
                    0.50 * vector_component + 0.20 * bm25_component + 0.30 * token
                }
            } else if is_cjk_only {
                0.35 * vector_component + 0.35 * bm25_component + 0.30 * token
            } else {
                0.45 * vector_component + 0.35 * bm25_component + 0.20 * token
            }
        } else if is_very_short {
            0.65 * vector_component + 0.35 * token
        } else if is_cjk_only {
            0.40 * vector_component + 0.60 * token
        } else {
            0.55 * vector_component + 0.45 * token
        }
    } else if is_very_short {
        0.55 * bm25_component + 0.45 * token
    } else if is_cjk_only {
        0.50 * bm25_component + 0.50 * token
    } else {
        0.55 * bm25_component + 0.45 * token
    };

    // Exact match floor
    if exact > 0.0 {
        let exact_floor = if normalized_query.len() > 12 {
            0.96
        } else {
            0.93
        };
        score = score.max(exact_floor);
    } else if token_hits >= 2 && token >= 0.50 {
        let cjk_chars: Vec<char> = normalized_query
            .chars()
            .filter(|&c| is_cjk_char(c))
            .collect();
        let bigram_ok = if cjk_chars.len() >= 2 {
            (0..cjk_chars.len() - 1).any(|i| {
                let bg: String = [cjk_chars[i], cjk_chars[i + 1]].iter().collect();
                normalized_content.contains(&bg)
            })
        } else {
            true
        };
        if bigram_ok {
            let partial_floor = 0.75 + 0.08 * token + 0.08 * vector_component;
            score = score.max(partial_floor);
            score += 0.03;
        }
    }

    // Tool/entity/tech/person boosts
    let mut tool_boost = 0.0_f64;
    for tool in KNOWN_TOOLS {
        if normalized_query.contains(tool) && normalized_content.contains(tool) {
            tool_boost = tool_boost.max(0.10);
        }
    }
    for term in TECHNICAL_TERMS {
        if normalized_query.contains(term) && normalized_content.contains(term) {
            tool_boost = tool_boost.max(0.06);
        }
    }
    for person in PERSON_MARKERS {
        if normalized_query.contains(person) && normalized_content.contains(person) {
            tool_boost = tool_boost.max(0.08);
        }
    }

    let mut vague_boost = 0.0;
    let layer_name = input.payload.layer.as_str();
    if VAGUE_KEYWORDS
        .iter()
        .any(|vk| normalized_query.contains(vk) && normalized_content.contains(vk))
        && matches!(
            layer_name,
            "verified_fact" | "procedure_schema" | "identity_schema"
        )
    {
        vague_boost = 0.04;
    }

    // Acronym boost
    let acronyms: Vec<&str> = query
        .split_whitespace()
        .filter(|w| w.len() >= 2 && w.chars().all(|c| c.is_ascii_uppercase()))
        .collect();
    if !acronyms.is_empty() {
        let acr_hits = acronyms.iter().filter(|a| content.contains(**a)).count();
        if acr_hits > 0 {
            tool_boost = tool_boost.max(0.06 * acr_hits as f64 / acronyms.len() as f64);
        }
    }

    score += tool_boost;
    score += vague_boost;
    score += 1.2 * token_pos_bonus;

    // Code identifier boost
    if !code_idents.is_empty() {
        let ident_hits = code_idents
            .iter()
            .filter(|id| normalized_content.contains(id.as_str()))
            .count();
        if ident_hits > 0 {
            score = score.max(0.94 + 0.04 * ident_hits as f64 / code_idents.len() as f64);
        }
    }

    // Task match boost
    if primary_task_match {
        let task_record_floor = if content.starts_with("[BUILD-SUCCESS]") {
            0.99
        } else {
            0.975
        };
        score = score.max(task_record_floor);
        score += 0.02;
    } else if secondary_task_match {
        score += 0.01;
    }

    // CJK bigram coherence boost
    let cjk_q: Vec<char> = normalized_query
        .chars()
        .filter(|&c| is_cjk_char(c))
        .collect();
    if cjk_q.len() >= 2 {
        let bgs: Vec<String> = (0..cjk_q.len() - 1)
            .map(|i| format!("{}{}", cjk_q[i], cjk_q[i + 1]))
            .collect();
        let bg_hits = bgs
            .iter()
            .filter(|bg| normalized_content.contains(bg.as_str()))
            .count();
        let bg_ratio = bg_hits as f64 / bgs.len() as f64;
        if bg_ratio >= 0.5 {
            score += 0.06 * bg_ratio;
        } else {
            score += 0.04 * bg_ratio;
        }
    }

    // Mixed CJK+EN coherence boost
    if !en_tokens_q.is_empty() && cjk_count >= 2 {
        let en_match = en_tokens_q
            .iter()
            .any(|t| t.len() >= 2 && normalized_content.contains(&t.to_lowercase()));
        let cjk_match = {
            let chars: Vec<char> = normalized_query.chars().collect();
            (0..chars.len().saturating_sub(1)).any(|i| {
                is_cjk_char(chars[i]) && is_cjk_char(chars[i + 1]) && {
                    let bg: String = [chars[i], chars[i + 1]].iter().collect();
                    normalized_content.contains(&bg)
                }
            })
        };
        if en_match && cjk_match {
            score += 0.03;
        }
    }

    // === Three-Axis Scoring ===
    let relevance = score.clamp(0.0, 1.0);

    let (half_life, mut floor) = layer_decay(layer_name);
    let has_temporal = has_temporal_markers(query);
    let has_history = has_history_intent(query);
    let has_remote_history = has_remote_history_intent(query);

    if has_temporal {
        floor *= 0.4;
    }

    let time_decay_raw = compute_time_decay(input.payload.created_at.as_deref(), half_life);
    let activation_bonus = compute_activation(input.recall_count, input.last_recalled_at);
    let freshness = (floor + (1.0 - floor) * time_decay_raw + activation_bonus).min(1.0);

    let confidence_factor = match input.payload.confidence {
        Some(c) if c > 0.0 => 0.85 + 0.15 * c.min(1.0),
        _ => 1.0,
    };

    let failure_count = input.metadata.failure_count.unwrap_or(0);
    let failure_factor = if failure_count >= 1 {
        (1.0 - failure_count as f64 * 0.15).max(0.3)
    } else {
        1.0
    };

    let trust = confidence_factor * failure_factor;

    // Three-axis weights:
    //
    //   - remote-history: keep relevance dominant. "当初/以前" queries are
    //     literally asking for the original record, freshness is harmful.
    //   - temporal (current-state or near-history): freshness ~ relevance.
    //     alice asked about "今天 / 最近 / 项目状态" — recent wins ties.
    //   - generic: rebalanced 2026-05-04. Was (0.90, 0.02, 0.08), but then
    //     marker drift caused recurring "AI uses stale memory" complaints —
    //     enriching the marker dictionary plays whack-a-mole forever, so
    //     the generic path also gets meaningful freshness now. Relevance
    //     still dominates so cosine-strong matches don't lose to fluff,
    //     but a 25-day gap will no longer be invisible at the score level.
    //     See memra-core/tests/wake_search_drift_red.rs::red_bug1 for the
    //     regression encoding the lower bound on freshness weight.
    let (w_rel, w_fresh, w_trust) = if has_remote_history {
        (0.90, 0.02, 0.08)
    } else if has_temporal {
        (0.45, 0.40, 0.15)
    } else {
        (0.60, 0.30, 0.10)
    };

    let mut score = w_rel * relevance + w_fresh * freshness + w_trust * trust;
    let literal_self_recall = exact > 0.0 && normalized_query.chars().count() >= 24;

    // Evolution state demotion is applied below, AFTER every boost and
    // floor guard runs. The task_slug rank guard can otherwise `score.max(…)`
    // a superseded row back up to ~0.9995 and defeat the 0.05 demotion.
    let (_, evo_factor) =
        evolution_state_factor(input.payload.evolution_state.as_deref(), input.metadata);

    // Staleness/correction demotion
    let (outdated_factor_val, _is_corrected) = staleness_factor(input.metadata, has_history);

    // SKU demotion
    let category = input.payload.category.as_deref().unwrap_or("");
    let is_sku = category == "sku" || category == "product_lines" || content.starts_with("[SKU]");
    let sku_factor_val = if is_sku { 0.30 } else { 1.0 };
    if is_sku {
        score *= sku_factor_val;
    }

    // Brand authority demotion
    let source = input.payload.source.as_deref().unwrap_or("");
    let brand_authority_factor_val =
        if source == "BRAND_AUTHORITY" || content.starts_with("[BRAND_AUTHORITY]") {
            0.60
        } else {
            1.0
        };
    if brand_authority_factor_val != 1.0 {
        score *= brand_authority_factor_val;
    }

    // Intent routing: typed fact boost/penalty
    let mut semantic_type_factor_val = 1.0;
    if let Some(boost_cats) = input.boost_categories {
        if boost_cats.contains(category) {
            semantic_type_factor_val = 1.20;
            score *= semantic_type_factor_val;
        } else if TYPED_FACT_CATEGORIES.contains(&category) {
            semantic_type_factor_val = 0.70;
            score *= semantic_type_factor_val;
        }
    } else if TYPED_FACT_CATEGORIES.contains(&category) {
        semantic_type_factor_val = 0.85;
        score *= semantic_type_factor_val;
    }

    // Task slug rank guard
    let mut canonical_task_rank_guard = 1.0;
    if primary_task_match {
        canonical_task_rank_guard = 0.9995;
        score = score.max(canonical_task_rank_guard);
    } else if secondary_task_match {
        canonical_task_rank_guard = 0.9895;
        score = score.min(canonical_task_rank_guard);
    }

    // Literal self-recall guard: when a long query is an exact substring of a
    // note, it is usually a replay/checkpoint smoke asking "can Rust find this
    // memory by its own words?". The relevance sub-score already has an exact
    // floor above, but freshness and typed-fact routing can still let newer
    // distractors outrank the literal source row. Apply a final exact floor
    // after routing demotions, but before lifecycle/staleness demotions.
    if literal_self_recall {
        score = score.max(0.985);
    }
    if exact > 0.0 && layer_name == "verified_fact" && content.starts_with("[FACT]") {
        score = score.max(0.9996);
    }

    score = score.clamp(0.0, 1.0);

    // History queries prefer episodic evidence
    if has_history && layer_name == "event_log" {
        score = (score * 1.12).min(1.0);
    }

    if outdated_factor_val != 1.0 {
        score *= outdated_factor_val;
    }

    // Final evolution-state demotion. Applied last so no subsequent floor
    // or boost can lift a superseded/deprecated row back into range.
    if evo_factor != 1.0 {
        score *= evo_factor;
    }
    score = score.clamp(0.0, 1.0);

    let debug = ScoreDebug {
        score,
        relevance,
        vector_component,
        bm25_component,
        token_score: token,
        exact_match: exact,
        freshness,
        time_decay_raw,
        activation_bonus,
        has_temporal,
        trust,
        confidence_factor,
        failure_factor,
        w_rel,
        w_fresh,
        w_trust,
        evolution_state_factor: evo_factor,
        outdated_factor: outdated_factor_val,
        sku_factor: sku_factor_val,
        brand_authority_factor: brand_authority_factor_val,
        semantic_type_factor: semantic_type_factor_val,
        primary_task_match,
        secondary_task_match,
        canonical_task_rank_guard,
    };

    (score, Some(debug))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn default_metadata() -> ScoringMetadata {
        ScoringMetadata::default()
    }

    fn make_payload(content: &str, layer: &str) -> ScoringPayload {
        ScoringPayload {
            content: content.to_string(),
            layer: layer.to_string(),
            confidence: Some(0.9),
            created_at: Some("2026-04-01T00:00:00+00:00".to_string()),
            ..Default::default()
        }
    }

    fn make_input<'a>(
        query: &'a str,
        payload: &'a ScoringPayload,
        metadata: &'a ScoringMetadata,
    ) -> ScoreInput<'a> {
        ScoreInput {
            query,
            query_vector: None,
            payload,
            stored_vector: None,
            bm25_score: None,
            metadata,
            boost_categories: None,
            recall_count: 0,
            last_recalled_at: None,
        }
    }

    #[test]
    fn empty_query_returns_one() {
        let payload = make_payload("some content", "verified_fact");
        let meta = default_metadata();
        let input = make_input("", &payload, &meta);
        let s = score_result(&input);
        assert!((s - 1.0).abs() < 1e-9);
    }

    #[test]
    fn exact_match_floor() {
        let payload = make_payload("fix the authentication bug in login", "verified_fact");
        let meta = default_metadata();
        let input = make_input("fix the authentication bug in login", &payload, &meta);
        let debug = score_result_debug(&input);
        assert!(
            debug.score >= 0.90,
            "exact match score should be high: {}",
            debug.score
        );
        assert!(debug.exact_match > 0.0);
    }

    #[test]
    fn exact_fact_rank_guard_beats_near_tie_distractors() {
        let payload = make_payload(
            "[FACT] SQLite FTS5: Memra uses SQLite FTS5 for lexical retrieval.",
            "verified_fact",
        );
        let meta = default_metadata();
        let input = make_input("SQLite FTS5", &payload, &meta);
        let debug = score_result_debug(&input);
        assert!(
            debug.score >= 0.9996,
            "exact [FACT] score should survive near-tie reranking: {}",
            debug.score
        );
    }

    #[test]
    fn superseded_gets_demoted() {
        let payload = ScoringPayload {
            content: "old memory about auth".to_string(),
            layer: "verified_fact".to_string(),
            confidence: Some(0.9),
            evolution_state: Some("superseded".to_string()),
            created_at: Some("2026-04-01T00:00:00+00:00".to_string()),
            ..Default::default()
        };
        let meta = default_metadata();
        let input = make_input("auth", &payload, &meta);
        let debug = score_result_debug(&input);
        assert!(
            debug.evolution_state_factor < 0.1,
            "superseded should be heavily demoted"
        );
    }

    #[test]
    fn superseded_final_score_is_at_most_five_percent_of_active_equivalent() {
        // Regression guard for the demotion-order bug: the task_slug rank
        // guard could previously `score.max(0.9995)` a superseded row back
        // up to near-1.0, defeating the 0.05 demotion. Evolution demotion
        // must now run AFTER the rank guard so superseded stays demoted.
        let active_payload = ScoringPayload {
            content: "gate-f-auto-supersede canonical memory".to_string(),
            layer: "verified_fact".to_string(),
            confidence: Some(0.9),
            evolution_state: Some("active".to_string()),
            created_at: Some("2026-04-01T00:00:00+00:00".to_string()),
            ..Default::default()
        };
        let superseded_payload = ScoringPayload {
            evolution_state: Some("superseded".to_string()),
            ..active_payload.clone()
        };
        let meta = ScoringMetadata {
            task_slug: Some(serde_json::json!("gate-f-auto-supersede")),
            ..Default::default()
        };

        let active = score_result(&make_input("gate-f-auto-supersede", &active_payload, &meta));
        let superseded = score_result(&make_input(
            "gate-f-auto-supersede",
            &superseded_payload,
            &meta,
        ));

        assert!(
            superseded <= active * FACTOR_SUPERSEDED + 1e-9,
            "superseded score should be <= 5% of active equivalent: active={active}, superseded={superseded}"
        );
    }

    #[test]
    fn failure_count_penalty() {
        let payload = make_payload("unreliable memory", "verified_fact");
        let meta = ScoringMetadata {
            failure_count: Some(3),
            ..Default::default()
        };
        let input = make_input("unreliable", &payload, &meta);
        let debug = score_result_debug(&input);
        assert!(
            debug.failure_factor < 0.60,
            "3 failures should penalize trust: {}",
            debug.failure_factor
        );
    }

    #[test]
    fn cjk_bigram_tokenization() {
        let payload = make_payload("记忆锚点搜索系统升级", "verified_fact");
        let meta = default_metadata();
        let input = make_input("记忆搜索", &payload, &meta);
        let s = score_result(&input);
        assert!(s > 0.0, "CJK query should produce non-zero score");
    }

    #[test]
    fn temporal_markers_detected() {
        assert!(has_temporal_markers("最近有什么变化"));
        assert!(has_temporal_markers("what happened last time"));
        assert!(!has_temporal_markers("how does scoring work"));
    }

    #[test]
    fn normalize_text_works() {
        assert_eq!(normalize_text("  Hello   World  "), "hello world");
        assert_eq!(normalize_text("CJK 中文 Test"), "cjk 中文 test");
    }

    #[test]
    fn cosine_identical_vectors() {
        let v = vec![1.0_f32, 2.0, 3.0];
        let sim = cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn task_slug_detection() {
        assert!(is_task_slug_like("build2-ma-rrf-fusion"));
        assert!(is_task_slug_like("build2-v6-rust-rewrite"));
        assert!(!is_task_slug_like("hello"));
        assert!(!is_task_slug_like("a-b"));
    }
}
