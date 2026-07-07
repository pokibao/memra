//! Chat export normalization for cold-path ingestion.
//!
//! This module ports the deprecated Python `backend.services.convo_normalizer`
//! behavior into the Rust core so R2 can remove Python-only ingestion paths.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exchange {
    pub user_text: String,
    pub assistant_text: String,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationFormat {
    ClaudeJsonl,
    ChatGptJson,
    JsonUnknown,
    PlainText,
    Unknown,
}

impl fmt::Display for ConversationFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ConversationFormat::ClaudeJsonl => "claude_jsonl",
            ConversationFormat::ChatGptJson => "chatgpt_json",
            ConversationFormat::JsonUnknown => "json_unknown",
            ConversationFormat::PlainText => "plain_text",
            ConversationFormat::Unknown => "unknown",
        })
    }
}

#[derive(Debug, Error)]
pub enum ConvoNormalizeError {
    #[error("conversation file I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("conversation JSON parse failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported conversation format: {format} (file: {path})")]
    UnsupportedFormat {
        format: ConversationFormat,
        path: PathBuf,
    },
}

pub fn detect_format(path: &Path) -> io::Result<ConversationFormat> {
    fs::metadata(path)?;

    let suffix = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase);

    match suffix.as_deref() {
        Some("jsonl") => Ok(ConversationFormat::ClaudeJsonl),
        Some("json") => {
            let text = read_prefix_lossy(path, 500)?;
            if text.contains("\"mapping\"") || text.contains("\"conversation_id\"") {
                Ok(ConversationFormat::ChatGptJson)
            } else {
                Ok(ConversationFormat::JsonUnknown)
            }
        }
        Some("txt") | Some("md") | Some("") | None => Ok(ConversationFormat::PlainText),
        _ => Ok(ConversationFormat::Unknown),
    }
}

pub fn normalize(path: &Path) -> Result<Vec<Exchange>, ConvoNormalizeError> {
    match detect_format(path)? {
        ConversationFormat::ClaudeJsonl => normalize_claude_jsonl(path),
        ConversationFormat::ChatGptJson => normalize_chatgpt_json(path),
        ConversationFormat::PlainText => normalize_plain_text(path),
        format => Err(ConvoNormalizeError::UnsupportedFormat {
            format,
            path: path.to_path_buf(),
        }),
    }
}

pub fn normalize_claude_jsonl(path: &Path) -> Result<Vec<Exchange>, ConvoNormalizeError> {
    let text = read_lossy(path)?;
    let mut exchanges = Vec::new();
    let mut current_user: Option<String> = None;
    let mut current_ts: Option<String> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        let role = nonempty_str_field(&entry, "role")
            .or_else(|| nonempty_str_field(&entry, "type"))
            .unwrap_or_default();
        let content = claude_entry_content(&entry).trim().to_string();
        if content.is_empty() {
            continue;
        }

        let ts = entry
            .get("timestamp")
            .or_else(|| entry.get("ts"))
            .and_then(json_value_to_timestamp);

        match role.as_str() {
            "user" | "human" => {
                if let Some(user_text) = current_user.replace(content) {
                    exchanges.push(Exchange {
                        user_text,
                        assistant_text: String::new(),
                        timestamp: current_ts.take(),
                    });
                }
                current_ts = ts;
            }
            "assistant" | "ai" | "model" => {
                if let Some(user_text) = current_user.take() {
                    exchanges.push(Exchange {
                        user_text,
                        assistant_text: content,
                        timestamp: current_ts.take(),
                    });
                }
            }
            _ => {}
        }
    }

    if let Some(user_text) = current_user {
        exchanges.push(Exchange {
            user_text,
            assistant_text: String::new(),
            timestamp: current_ts,
        });
    }

    Ok(exchanges)
}

pub fn normalize_chatgpt_json(path: &Path) -> Result<Vec<Exchange>, ConvoNormalizeError> {
    let text = read_lossy(path)?;
    let data = serde_json::from_str::<Value>(&text)?;
    let mut exchanges = Vec::new();

    match &data {
        Value::Array(conversations) => {
            for conversation in conversations {
                append_chatgpt_conversation(conversation, &mut exchanges);
            }
        }
        conversation => append_chatgpt_conversation(conversation, &mut exchanges),
    }

    Ok(exchanges)
}

pub fn normalize_plain_text(path: &Path) -> Result<Vec<Exchange>, ConvoNormalizeError> {
    let text = read_lossy(path)?;
    let mut exchanges = Vec::new();
    let mut current_user: Option<String> = None;
    let mut assistant_lines: Vec<String> = Vec::new();

    for line in text.lines() {
        if let Some(user_line) = line.strip_prefix("> ") {
            if let Some(user_text) = current_user.take() {
                exchanges.push(Exchange {
                    user_text,
                    assistant_text: assistant_lines.join("\n").trim().to_string(),
                    timestamp: None,
                });
                assistant_lines.clear();
            }
            current_user = Some(user_line.trim().to_string());
        } else if current_user.is_some() {
            assistant_lines.push(line.to_string());
        }
    }

    if let Some(user_text) = current_user {
        exchanges.push(Exchange {
            user_text,
            assistant_text: assistant_lines.join("\n").trim().to_string(),
            timestamp: None,
        });
    }

    Ok(exchanges)
}

fn append_chatgpt_conversation(conversation: &Value, exchanges: &mut Vec<Exchange>) {
    let Some(mapping) = conversation.get("mapping").and_then(Value::as_object) else {
        return;
    };
    if mapping.is_empty() {
        return;
    }

    let mut current_node_id = conversation
        .get("current_node")
        .and_then(Value::as_str)
        .filter(|node_id| mapping.contains_key(*node_id))
        .map(ToOwned::to_owned);

    if current_node_id.is_none() {
        current_node_id = latest_leaf_node(mapping);
    }

    let Some(current_node_id) = current_node_id else {
        return;
    };

    let chain_ids = active_chain_ids(mapping, current_node_id);
    let messages = chatgpt_messages(mapping, &chain_ids);

    let mut current_user: Option<String> = None;
    let mut current_ts: Option<String> = None;

    for message in messages {
        match message.role.as_str() {
            "user" => {
                if let Some(user_text) = current_user.replace(message.text) {
                    exchanges.push(Exchange {
                        user_text,
                        assistant_text: String::new(),
                        timestamp: current_ts.take(),
                    });
                }
                current_ts = message.timestamp;
            }
            "assistant" => {
                if let Some(user_text) = current_user.take() {
                    exchanges.push(Exchange {
                        user_text,
                        assistant_text: message.text,
                        timestamp: current_ts.take(),
                    });
                }
            }
            _ => {}
        }
    }

    if let Some(user_text) = current_user {
        exchanges.push(Exchange {
            user_text,
            assistant_text: String::new(),
            timestamp: current_ts,
        });
    }
}

#[derive(Debug)]
struct ChatMessage {
    role: String,
    text: String,
    timestamp: Option<String>,
}

fn latest_leaf_node(mapping: &Map<String, Value>) -> Option<String> {
    let parents: HashSet<&str> = mapping
        .values()
        .filter_map(|node| node.get("parent").and_then(Value::as_str))
        .collect();

    mapping
        .keys()
        .filter(|node_id| !parents.contains(node_id.as_str()))
        .max_by(|left, right| {
            leaf_timestamp(mapping, left)
                .partial_cmp(&leaf_timestamp(mapping, right))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned()
}

fn leaf_timestamp(mapping: &Map<String, Value>, node_id: &str) -> f64 {
    mapping
        .get(node_id)
        .and_then(|node| node.get("message"))
        .and_then(|message| message.get("create_time"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

fn active_chain_ids(mapping: &Map<String, Value>, current_node_id: String) -> Vec<String> {
    let mut chain_ids = Vec::new();
    let mut visited = HashSet::new();
    let mut node_id = Some(current_node_id);

    while let Some(id) = node_id {
        if !mapping.contains_key(&id) || !visited.insert(id.clone()) {
            break;
        }

        chain_ids.push(id.clone());
        node_id = mapping
            .get(&id)
            .and_then(|node| node.get("parent"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }

    chain_ids.reverse();
    chain_ids
}

fn chatgpt_messages(mapping: &Map<String, Value>, chain_ids: &[String]) -> Vec<ChatMessage> {
    let mut messages = Vec::new();

    for node_id in chain_ids {
        let Some(message) = mapping.get(node_id).and_then(|node| node.get("message")) else {
            continue;
        };
        let Some(parts) = message
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        else {
            continue;
        };

        let role = message
            .get("author")
            .and_then(|author| author.get("role"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        if role != "user" && role != "assistant" {
            continue;
        }

        let text = parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();

        if text.is_empty() {
            continue;
        }

        messages.push(ChatMessage {
            role: role.to_string(),
            text,
            timestamp: message.get("create_time").and_then(json_value_to_timestamp),
        });
    }

    messages
}

fn claude_entry_content(entry: &Value) -> String {
    match entry.get("content") {
        Some(Value::String(content)) => content.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| match block {
                Value::Object(object) => {
                    let block_type = object
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    if block_type == "tool_result" || block_type == "tool_use" {
                        None
                    } else {
                        object
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                            .map(ToOwned::to_owned)
                    }
                }
                Value::String(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => entry
            .get("message")
            .map(json_value_to_plain_text)
            .unwrap_or_default(),
    }
}

fn nonempty_str_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

fn json_value_to_timestamp(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

fn json_value_to_plain_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn read_lossy(path: &Path) -> io::Result<String> {
    fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn read_prefix_lossy(path: &Path, byte_limit: u64) -> io::Result<String> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(byte_limit).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_TEST_FILE_ID: AtomicU64 = AtomicU64::new(0);

    struct TestFile {
        path: PathBuf,
    }

    impl TestFile {
        fn new(name: &str, contents: impl AsRef<[u8]>) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time before epoch")
                .as_nanos();
            let sequence = NEXT_TEST_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ma-convo-normalizer-{}-{}-{}-{}",
                std::process::id(),
                unique,
                sequence,
                name
            ));
            fs::write(&path, contents).expect("write temp conversation export");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    #[test]
    fn detect_format_matches_python_contract() {
        let jsonl = TestFile::new("session.jsonl", r#"{"role":"user","content":"hi"}"#);
        assert_eq!(
            detect_format(jsonl.path()).unwrap(),
            ConversationFormat::ClaudeJsonl
        );

        let chatgpt = TestFile::new(
            "convos.json",
            r#"{"mapping": {}, "conversation_id": "abc"}"#,
        );
        assert_eq!(
            detect_format(chatgpt.path()).unwrap(),
            ConversationFormat::ChatGptJson
        );

        let json_unknown = TestFile::new("other.json", r#"{"items":[]}"#);
        assert_eq!(
            detect_format(json_unknown.path()).unwrap(),
            ConversationFormat::JsonUnknown
        );

        let plain = TestFile::new("chat.txt", "> hello\nworld");
        assert_eq!(
            detect_format(plain.path()).unwrap(),
            ConversationFormat::PlainText
        );

        assert!(detect_format(&std::env::temp_dir().join("missing-ma-convo.txt")).is_err());
    }

    #[test]
    fn claude_jsonl_pairs_basic_and_multiple_exchanges() {
        let file = TestFile::new(
            "session.jsonl",
            [
                json!({"role": "user", "content": "Q1"}).to_string(),
                json!({"role": "assistant", "content": "A1"}).to_string(),
                json!({"role": "user", "content": "Q2"}).to_string(),
                json!({"role": "assistant", "content": "A2"}).to_string(),
            ]
            .join("\n"),
        );

        let exchanges = normalize_claude_jsonl(file.path()).unwrap();
        assert_eq!(exchanges.len(), 2);
        assert_eq!(exchanges[0].user_text, "Q1");
        assert_eq!(exchanges[0].assistant_text, "A1");
        assert_eq!(exchanges[1].user_text, "Q2");
        assert_eq!(exchanges[1].assistant_text, "A2");
    }

    #[test]
    fn claude_jsonl_handles_blocks_tools_invalid_lines_and_trailing_user() {
        let file = TestFile::new(
            "session.jsonl",
            [
                "not json".to_string(),
                json!({
                    "role": "user",
                    "content": [
                        {"type": "tool_use", "text": "skip me"},
                        {"type": "text", "text": "hello"},
                        "from string block"
                    ],
                    "timestamp": "2026-05-16T00:00:00Z"
                })
                .to_string(),
                json!({
                    "role": "assistant",
                    "content": [
                        {"type": "tool_result", "text": "skip tool result"},
                        {"type": "text", "text": "world"}
                    ]
                })
                .to_string(),
                json!({"type": "human", "message": "unanswered", "ts": 5}).to_string(),
            ]
            .join("\n"),
        );

        let exchanges = normalize_claude_jsonl(file.path()).unwrap();
        assert_eq!(exchanges.len(), 2);
        assert_eq!(exchanges[0].user_text, "hello\nfrom string block");
        assert_eq!(exchanges[0].assistant_text, "world");
        assert_eq!(
            exchanges[0].timestamp.as_deref(),
            Some("2026-05-16T00:00:00Z")
        );
        assert_eq!(exchanges[1].user_text, "unanswered");
        assert_eq!(exchanges[1].assistant_text, "");
        assert_eq!(exchanges[1].timestamp.as_deref(), Some("5"));
    }

    #[test]
    fn plain_text_pairs_user_markers_with_multiline_assistant() {
        let file = TestFile::new(
            "chat.txt",
            "> What is this?\nIt is a test.\nWith multiple lines.\n> Q2\nA2",
        );

        let exchanges = normalize_plain_text(file.path()).unwrap();
        assert_eq!(exchanges.len(), 2);
        assert_eq!(exchanges[0].user_text, "What is this?");
        assert_eq!(
            exchanges[0].assistant_text,
            "It is a test.\nWith multiple lines."
        );
        assert_eq!(exchanges[1].user_text, "Q2");
        assert_eq!(exchanges[1].assistant_text, "A2");
    }

    #[test]
    fn chatgpt_uses_current_node_branch() {
        let file = TestFile::new(
            "convos.json",
            json!([{
                "conversation_id": "abc",
                "current_node": "2",
                "mapping": {
                    "root": {"parent": null, "message": null},
                    "1": {"parent": "root", "message": {"author": {"role": "user"}, "content": {"parts": ["Hello"]}, "create_time": 1.0}},
                    "2": {"parent": "1", "message": {"author": {"role": "assistant"}, "content": {"parts": ["Hi there"]}, "create_time": 2.0}}
                }
            }])
            .to_string(),
        );

        let exchanges = normalize_chatgpt_json(file.path()).unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].user_text, "Hello");
        assert_eq!(exchanges[0].assistant_text, "Hi there");
        assert_eq!(exchanges[0].timestamp.as_deref(), Some("1.0"));
    }

    #[test]
    fn chatgpt_fallback_picks_latest_leaf_regeneration() {
        let file = TestFile::new(
            "convos.json",
            json!([{
                "conversation_id": "abc",
                "mapping": {
                    "root": {"parent": null, "message": null},
                    "u1": {"parent": "root", "message": {"author": {"role": "user"}, "content": {"parts": ["Question"]}, "create_time": 1.0}},
                    "a1_old": {"parent": "u1", "message": {"author": {"role": "assistant"}, "content": {"parts": ["Old answer"]}, "create_time": 2.0}},
                    "a1_new": {"parent": "u1", "message": {"author": {"role": "assistant"}, "content": {"parts": ["New answer"]}, "create_time": 3.0}}
                }
            }])
            .to_string(),
        );

        let exchanges = normalize_chatgpt_json(file.path()).unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].assistant_text, "New answer");
    }

    #[test]
    fn chatgpt_fallback_preserves_trailing_user_turn() {
        let file = TestFile::new(
            "convos.json",
            json!([{
                "conversation_id": "abc",
                "mapping": {
                    "root": {"parent": null, "message": null},
                    "u1": {"parent": "root", "message": {"author": {"role": "user"}, "content": {"parts": ["First"]}, "create_time": 1.0}},
                    "a1": {"parent": "u1", "message": {"author": {"role": "assistant"}, "content": {"parts": ["Reply"]}, "create_time": 2.0}},
                    "u2": {"parent": "a1", "message": {"author": {"role": "user"}, "content": {"parts": ["Follow-up"]}, "create_time": 3.0}}
                }
            }])
            .to_string(),
        );

        let exchanges = normalize_chatgpt_json(file.path()).unwrap();
        assert_eq!(exchanges.len(), 2);
        assert_eq!(exchanges[0].user_text, "First");
        assert_eq!(exchanges[0].assistant_text, "Reply");
        assert_eq!(exchanges[1].user_text, "Follow-up");
        assert_eq!(exchanges[1].assistant_text, "");
    }

    #[test]
    fn chatgpt_fallback_mixed_role_leaves_picks_latest() {
        let file = TestFile::new(
            "convos.json",
            json!([{
                "conversation_id": "mixed",
                "mapping": {
                    "root": {"parent": null, "message": null},
                    "u1": {"parent": "root", "message": {"author": {"role": "user"}, "content": {"parts": ["Start"]}, "create_time": 1.0}},
                    "a1": {"parent": "u1", "message": {"author": {"role": "assistant"}, "content": {"parts": ["First reply"]}, "create_time": 2.0}},
                    "u2": {"parent": "a1", "message": {"author": {"role": "user"}, "content": {"parts": ["Continue"]}, "create_time": 3.0}},
                    "a2": {"parent": "u2", "message": {"author": {"role": "assistant"}, "content": {"parts": ["Old branch reply"]}, "create_time": 4.0}},
                    "u3": {"parent": "a1", "message": {"author": {"role": "user"}, "content": {"parts": ["New direction"]}, "create_time": 5.0}}
                }
            }])
            .to_string(),
        );

        let exchanges = normalize_chatgpt_json(file.path()).unwrap();
        assert!(
            exchanges
                .iter()
                .any(|exchange| exchange.user_text == "New direction")
        );
    }

    #[test]
    fn normalize_dispatch_rejects_unknown_formats() {
        let unknown = TestFile::new("archive.bin", "nope");
        assert!(matches!(
            normalize(unknown.path()),
            Err(ConvoNormalizeError::UnsupportedFormat {
                format: ConversationFormat::Unknown,
                ..
            })
        ));
    }
}
