use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub const DEFAULT_WRITE_QUEUE_LIMIT: usize = 64;
pub const DEFAULT_WRITE_RATE_LIMIT_PER_MINUTE: u32 = 600;
pub const WRITE_RETRY_AFTER_SECONDS: u64 = 2;

const WRITE_TOOL_NAMES: &[&str] = &[
    "add_rule",
    "propose_change",
    "approve_change",
    "save_checkpoint",
    "report_outcome",
];

#[derive(Debug, Clone)]
pub struct WriteAdmission {
    semaphore: Arc<Semaphore>,
    rate_limit: Arc<Mutex<RateLimitState>>,
    writes_per_minute: u32,
}

impl WriteAdmission {
    pub fn new(limit: usize) -> Self {
        Self::with_limits(limit, DEFAULT_WRITE_RATE_LIMIT_PER_MINUTE)
    }

    pub fn with_limits(queue_limit: usize, writes_per_minute: u32) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(queue_limit)),
            rate_limit: Arc::new(Mutex::new(RateLimitState::new())),
            writes_per_minute,
        }
    }

    pub fn try_enter(&self) -> Result<WritePermit, WriteAdmissionError> {
        self.check_rate_limit()?;
        self.semaphore
            .clone()
            .try_acquire_owned()
            .map(|permit| WritePermit { _permit: permit })
            .map_err(|_| WriteAdmissionError::QueueFull)
    }

    fn check_rate_limit(&self) -> Result<(), WriteAdmissionError> {
        let mut state = self
            .rate_limit
            .lock()
            .map_err(|_| WriteAdmissionError::QueueFull)?;
        state.reset_if_expired();
        if state.used >= self.writes_per_minute {
            return Err(WriteAdmissionError::RateLimited {
                retry_after: state.retry_after_seconds(),
            });
        }
        state.used += 1;
        Ok(())
    }
}

impl Default for WriteAdmission {
    fn default() -> Self {
        Self::new(DEFAULT_WRITE_QUEUE_LIMIT)
    }
}

#[derive(Debug)]
pub struct WritePermit {
    _permit: OwnedSemaphorePermit,
}

#[derive(Debug, Clone, Copy, thiserror::Error, PartialEq, Eq)]
pub enum WriteAdmissionError {
    #[error("WriteQueueFull")]
    QueueFull,
    #[error("WriteRateLimited")]
    RateLimited { retry_after: u64 },
}

#[derive(Debug)]
struct RateLimitState {
    window_started_at: Instant,
    used: u32,
}

impl RateLimitState {
    fn new() -> Self {
        Self {
            window_started_at: Instant::now(),
            used: 0,
        }
    }

    fn reset_if_expired(&mut self) {
        if self.window_started_at.elapsed() >= Duration::from_secs(60) {
            self.window_started_at = Instant::now();
            self.used = 0;
        }
    }

    fn retry_after_seconds(&self) -> u64 {
        60_u64.saturating_sub(self.window_started_at.elapsed().as_secs())
    }
}

pub fn write_admission_error_json(error: WriteAdmissionError) -> String {
    match error {
        WriteAdmissionError::QueueFull => format!(
            r#"{{"error":"WriteQueueFull","retry_after_seconds":{WRITE_RETRY_AFTER_SECONDS}}}"#
        ),
        WriteAdmissionError::RateLimited { retry_after } => {
            format!(r#"{{"error":"WriteRateLimited","retry_after_seconds":{retry_after}}}"#)
        }
    }
}

pub fn is_write_tool_call_body(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return false;
    };

    match value {
        Value::Array(items) => items.iter().any(is_write_tool_call_value),
        other => is_write_tool_call_value(&other),
    }
}

fn is_write_tool_call_value(value: &Value) -> bool {
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        return false;
    };
    if method != "tools/call" {
        return false;
    }

    value
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str)
        .is_some_and(|name| WRITE_TOOL_NAMES.contains(&name))
}

#[cfg(test)]
mod tests {
    use crate::transport::admission::{
        DEFAULT_WRITE_RATE_LIMIT_PER_MINUTE, WRITE_RETRY_AFTER_SECONDS, WriteAdmission,
        WriteAdmissionError, is_write_tool_call_body,
    };

    #[test]
    fn write_admission_rejects_when_queue_is_full() {
        let admission = WriteAdmission::with_limits(1, DEFAULT_WRITE_RATE_LIMIT_PER_MINUTE);
        let first = admission.try_enter();
        assert!(first.is_ok());

        let second = admission.try_enter();
        assert!(matches!(second, Err(WriteAdmissionError::QueueFull)));

        drop(first);
        let third = admission.try_enter();
        assert!(third.is_ok());
    }

    #[test]
    fn write_admission_enforces_per_minute_limit() {
        let admission = WriteAdmission::with_limits(2, 1);
        let first = admission.try_enter();
        assert!(first.is_ok());

        let second = admission.try_enter();
        assert!(matches!(
            second,
            Err(WriteAdmissionError::RateLimited { retry_after }) if retry_after <= 60
        ));
    }

    #[test]
    fn write_classifier_detects_write_tool_calls() {
        let body = br#"{
            "jsonrpc":"2.0",
            "id":1,
            "method":"tools/call",
            "params":{"name":"add_rule","arguments":{"content":"hello"}}
        }"#;

        assert!(is_write_tool_call_body(body));
    }

    #[test]
    fn write_classifier_ignores_read_tool_calls_and_invalid_json() {
        let read_body = br#"{
            "jsonrpc":"2.0",
            "id":1,
            "method":"tools/call",
            "params":{"name":"search_rules","arguments":{"query":"hello"}}
        }"#;

        assert!(!is_write_tool_call_body(read_body));
        assert!(!is_write_tool_call_body(b"not json"));
    }

    #[test]
    fn write_retry_after_is_two_seconds() {
        assert_eq!(WRITE_RETRY_AFTER_SECONDS, 2);
    }
}
