use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{NaiveDate, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::Value;

const AUDIT_FILE_NAME: &str = "audit.jsonl";
const AUDIT_RETENTION_DAYS: i64 = 90;

#[derive(Debug, Clone)]
pub struct AuditLogger {
    dir: PathBuf,
}

impl AuditLogger {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn default_dir() -> PathBuf {
        home_dir().join(".memra").join("audit")
    }

    pub fn current_path(&self) -> PathBuf {
        self.dir.join(AUDIT_FILE_NAME)
    }

    pub fn append(&self, event: AuditEvent) -> Result<(), AuditError> {
        fs::create_dir_all(&self.dir).map_err(|source| AuditError::Io {
            path: self.dir.clone(),
            source,
        })?;
        let path = self.current_path();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|source| AuditError::Io {
                path: path.clone(),
                source,
            })?;
        let line = serde_json::to_string(&event).map_err(AuditError::Serialize)?;
        file.write_all(line.as_bytes())
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|source| AuditError::Io { path, source })
    }

    pub fn rotate_current(&self, date: &str) -> Result<PathBuf, AuditError> {
        fs::create_dir_all(&self.dir).map_err(|source| AuditError::Io {
            path: self.dir.clone(),
            source,
        })?;
        let current = self.current_path();
        let rotated = self.dir.join(format!("audit-{date}.jsonl"));
        if !current.exists() {
            return Ok(rotated);
        }
        fs::rename(&current, &rotated).map_err(|source| AuditError::Io {
            path: current,
            source,
        })?;
        Ok(rotated)
    }

    pub fn sweep_old_rotated_logs(&self, today: &str) -> Result<usize, AuditError> {
        let today = parse_date(today)?;
        let mut removed = 0;
        if !self.dir.exists() {
            return Ok(0);
        }
        for entry in fs::read_dir(&self.dir).map_err(|source| AuditError::Io {
            path: self.dir.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| AuditError::Io {
                path: self.dir.clone(),
                source,
            })?;
            let path = entry.path();
            let Some(date) = rotated_log_date(&path) else {
                continue;
            };
            if today.signed_duration_since(date).num_days() > AUDIT_RETENTION_DAYS {
                fs::remove_file(&path).map_err(|source| AuditError::Io {
                    path: path.clone(),
                    source,
                })?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

impl Default for AuditLogger {
    fn default() -> Self {
        Self::new(Self::default_dir())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditEvent {
    pub ts: String,
    pub event_type: String,
    pub actor_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub project_id: Option<String>,
    pub req_id: Option<String>,
    pub src_ip: Option<String>,
    pub result: String,
    pub fields: Value,
}

impl AuditEvent {
    pub fn new(event_type: impl Into<String>, result: impl Into<String>) -> Self {
        Self {
            ts: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            event_type: event_type.into(),
            actor_id: None,
            session_id: None,
            project_id: std::env::var("MCP_MEMORY_PROJECT_ID").ok(),
            req_id: None,
            src_ip: Some("stdio".to_string()),
            result: result.into(),
            fields: Value::Object(serde_json::Map::new()),
        }
    }

    /// Set the `actor_id` from an authenticated actor.
    pub fn with_actor(mut self, actor_id: impl Into<String>) -> Self {
        self.actor_id = Some(actor_id.into());
        self
    }

    /// Set the `session_id` from a resolved session identifier.
    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        if let Value::Object(fields) = &mut self.fields {
            fields.insert(key.into(), value.into());
        }
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("audit serialization error: {0}")]
    Serialize(serde_json::Error),
    #[error("invalid audit date: {0}")]
    InvalidDate(String),
}

fn parse_date(date: &str) -> Result<NaiveDate, AuditError> {
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_err(|_| AuditError::InvalidDate(date.to_string()))
}

fn rotated_log_date(path: &Path) -> Option<NaiveDate> {
    let file_name = path.file_name()?.to_str()?;
    let date = file_name.strip_prefix("audit-")?.strip_suffix(".jsonl")?;
    NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::audit::{AuditEvent, AuditLogger};

    fn temp_dir(name: &str) -> Result<PathBuf, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ma-audit-test-{now}-{name}"));
        fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        Ok(dir)
    }

    #[test]
    fn append_writes_jsonl_event() -> Result<(), String> {
        let logger = AuditLogger::new(temp_dir("append")?);
        logger
            .append(AuditEvent::new("add_rule", "accepted"))
            .map_err(|error| error.to_string())?;

        let content =
            fs::read_to_string(logger.current_path()).map_err(|error| error.to_string())?;
        assert!(content.contains(r#""event_type":"add_rule""#));
        assert!(content.contains(r#""result":"accepted""#));
        assert_eq!(content.lines().count(), 1);
        Ok(())
    }

    #[test]
    fn rotate_current_moves_audit_file_to_dated_name() -> Result<(), String> {
        let logger = AuditLogger::new(temp_dir("rotate")?);
        logger
            .append(AuditEvent::new("approve_change", "approved"))
            .map_err(|error| error.to_string())?;

        let rotated = logger
            .rotate_current("2026-04-13")
            .map_err(|error| error.to_string())?;

        assert_eq!(
            rotated.file_name().and_then(|name| name.to_str()),
            Some("audit-2026-04-13.jsonl")
        );
        assert!(rotated.exists());
        assert!(!logger.current_path().exists());
        Ok(())
    }

    #[test]
    fn sweep_old_rotated_logs_removes_files_older_than_retention() -> Result<(), String> {
        let dir = temp_dir("sweep")?;
        fs::write(dir.join("audit-2026-01-01.jsonl"), "{}\n").map_err(|error| error.to_string())?;
        fs::write(dir.join("audit-2026-04-01.jsonl"), "{}\n").map_err(|error| error.to_string())?;
        let logger = AuditLogger::new(dir.clone());

        let removed = logger
            .sweep_old_rotated_logs("2026-04-13")
            .map_err(|error| error.to_string())?;

        assert_eq!(removed, 1);
        assert!(!dir.join("audit-2026-01-01.jsonl").exists());
        assert!(dir.join("audit-2026-04-01.jsonl").exists());
        Ok(())
    }

    #[test]
    fn audit_event_with_session_id_serializes_field() {
        let event = AuditEvent::new("add_rule", "saved").with_session_id("http:abc");
        let json = serde_json::to_string(&event).expect("serialize failed");
        assert!(
            json.contains(r#""session_id":"http:abc""#),
            "expected session_id field in JSON, got: {json}"
        );
    }

    #[test]
    fn audit_event_without_session_id_omits_field() {
        let event = AuditEvent::new("add_rule", "saved");
        let json = serde_json::to_string(&event).expect("serialize failed");
        assert!(
            !json.contains("session_id"),
            "expected session_id to be absent when None (skip_serializing_if), got: {json}"
        );
    }

    #[test]
    fn audit_event_session_id_combines_with_actor() {
        let event = AuditEvent::new("add_rule", "saved")
            .with_actor("actor-42")
            .with_session_id("http:session-xyz");
        let json = serde_json::to_string(&event).expect("serialize failed");
        assert!(
            json.contains(r#""actor_id":"actor-42""#),
            "expected actor_id in JSON, got: {json}"
        );
        assert!(
            json.contains(r#""session_id":"http:session-xyz""#),
            "expected session_id in JSON, got: {json}"
        );
    }
}
