use std::env;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::header::{
    AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmProviderKind {
    OpenAi,
    DeepSeek,
    Anthropic,
    Gemini,
}

impl LlmProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::DeepSeek => "deepseek",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    pub model: String,
    pub prompt: String,
    pub system_prompt: Option<String>,
    pub max_tokens: u32,
    pub temperature: f32,
}

impl LlmRequest {
    pub fn new(model: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            prompt: prompt.into(),
            system_prompt: None,
            max_tokens: 500,
            temperature: 0.3,
        }
    }

    pub fn system_prompt(mut self, value: impl Into<String>) -> Self {
        self.system_prompt = Some(value.into());
        self
    }

    pub fn max_tokens(mut self, value: u32) -> Self {
        self.max_tokens = value;
        self
    }

    pub fn temperature(mut self, value: f32) -> Self {
        self.temperature = value;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LlmUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmResponse {
    pub provider: String,
    pub model: String,
    pub content: String,
    pub usage: LlmUsage,
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("LLM request failed: {0}")]
    Request(String),
    #[error("LLM secret resolution failed: {0}")]
    Secret(String),
    #[error("LLM request timed out: {0}")]
    Timeout(String),
    #[error("LLM HTTP status {status}: {body}")]
    HttpStatus {
        status: u16,
        body: String,
        retryable: bool,
    },
    #[error("LLM rate limited: {body}")]
    RateLimited {
        retry_after: Option<String>,
        body: String,
    },
    #[error("LLM response was malformed: {0}")]
    Malformed(String),
    #[error("LLM stream interrupted: {0}")]
    StreamInterrupted(String),
}

impl LlmError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout(_) | Self::RateLimited { .. } | Self::StreamInterrupted(_) => true,
            Self::HttpStatus { retryable, .. } => *retryable,
            Self::Request(_) | Self::Secret(_) | Self::Malformed(_) => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmKeyResolver {
    env_var: Option<String>,
    akm_key_name: String,
    akm_bin: PathBuf,
    timeout: Duration,
}

impl LlmKeyResolver {
    pub fn new(akm_key_name: impl Into<String>) -> Self {
        let akm_key_name = akm_key_name.into();
        Self {
            env_var: Some(akm_key_name.clone()),
            akm_key_name,
            akm_bin: default_akm_bin(),
            timeout: Duration::from_secs(10),
        }
    }

    pub fn with_env_var(mut self, env_var: impl Into<String>) -> Self {
        self.env_var = Some(env_var.into());
        self
    }

    pub fn without_env_var(mut self) -> Self {
        self.env_var = None;
        self
    }

    pub fn with_akm_bin(mut self, akm_bin: impl Into<PathBuf>) -> Self {
        self.akm_bin = akm_bin.into();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn akm_key_name(&self) -> &str {
        &self.akm_key_name
    }

    pub fn akm_bin(&self) -> &Path {
        &self.akm_bin
    }

    pub fn resolve(&self) -> Result<Option<String>, LlmError> {
        if let Some(env_var) = self.env_var.as_deref()
            && !env_var.trim().is_empty()
            && let Ok(value) = env::var(env_var)
            && let Some(key) = trimmed_secret(value)
        {
            return Ok(Some(key));
        }
        self.resolve_from_akm()
    }

    fn resolve_from_akm(&self) -> Result<Option<String>, LlmError> {
        let mut command = Command::new(&self.akm_bin);
        command.arg("get").arg(&self.akm_key_name).arg("-y");
        let output = match output_with_timeout(command, self.timeout) {
            Ok(output) => output,
            Err(LlmError::Request(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        if !output.status.success() {
            return Ok(None);
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(trimmed_secret(stdout))
    }
}

fn default_akm_bin() -> PathBuf {
    if let Ok(value) = env::var("MA_AKM_BIN")
        && !value.trim().is_empty()
    {
        return PathBuf::from(value);
    }
    if let Some(home) = env::var_os("HOME") {
        let fallback = PathBuf::from(home).join("go/bin/akm");
        if fallback.is_file() {
            return fallback;
        }
    }
    PathBuf::from("akm")
}

fn trimmed_secret(value: impl AsRef<str>) -> Option<String> {
    let value = value.as_ref().trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn output_with_timeout(mut command: Command, timeout: Duration) -> Result<Output, LlmError> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| LlmError::Request(error.to_string()))?;
    let started = Instant::now();
    loop {
        match child
            .try_wait()
            .map_err(|error| LlmError::Secret(error.to_string()))?
        {
            Some(status) => {
                let mut stdout = Vec::new();
                if let Some(mut pipe) = child.stdout.take() {
                    pipe.read_to_end(&mut stdout)
                        .map_err(|error| LlmError::Secret(error.to_string()))?;
                }
                let mut stderr = Vec::new();
                if let Some(mut pipe) = child.stderr.take() {
                    pipe.read_to_end(&mut stderr)
                        .map_err(|error| LlmError::Secret(error.to_string()))?;
                }
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(LlmError::Timeout(format!(
                    "akm get {} exceeded {:?}",
                    self_redacted(&command),
                    timeout
                )));
            }
            None => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn self_redacted(command: &Command) -> String {
    command
        .get_args()
        .nth(1)
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<key>".to_string())
}

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleClient {
    provider: LlmProviderKind,
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl OpenAiCompatibleClient {
    pub fn new(
        provider: LlmProviderKind,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Result<Self, LlmError> {
        Self::with_timeout(provider, base_url, api_key, Duration::from_secs(60))
    }

    pub fn with_timeout(
        provider: LlmProviderKind,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| LlmError::Request(error.to_string()))?;
        Ok(Self {
            provider,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            http,
        })
    }

    pub async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let body = self.request_body(request, false);
        let response = self
            .http
            .post(self.chat_completions_url())
            .headers(self.auth_headers()?)
            .json(&body)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let response = error_for_status(response).await?;
        let payload: OpenAiChatResponse = response
            .json()
            .await
            .map_err(|error| LlmError::Malformed(error.to_string()))?;
        payload.into_llm_response(self.provider, &request.model)
    }

    pub async fn complete_streaming(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let body = self.request_body(request, true);
        let response = self
            .http
            .post(self.chat_completions_url())
            .headers(self.auth_headers()?)
            .json(&body)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let response = error_for_status(response).await?;

        let mut buffer = String::new();
        let mut output = String::new();
        let mut saw_done = false;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_reqwest_error)?;
            let text = std::str::from_utf8(&chunk)
                .map_err(|error| LlmError::Malformed(error.to_string()))?;
            buffer.push_str(text);
            while let Some((frame, rest)) = split_sse_frame(&buffer) {
                buffer = rest;
                if apply_sse_frame(&frame, &mut output)? {
                    saw_done = true;
                }
            }
        }

        if !buffer.trim().is_empty() && apply_sse_frame(&buffer, &mut output)? {
            saw_done = true;
        }

        if !saw_done {
            return Err(LlmError::StreamInterrupted(
                "stream ended before [DONE]".to_string(),
            ));
        }

        Ok(LlmResponse {
            provider: self.provider.as_str().to_string(),
            model: request.model.clone(),
            content: output,
            usage: LlmUsage::default(),
        })
    }

    fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn auth_headers(&self) -> Result<HeaderMap, LlmError> {
        let mut headers = HeaderMap::new();
        let auth = HeaderValue::from_str(&format!("Bearer {}", self.api_key))
            .map_err(|error| LlmError::Request(error.to_string()))?;
        headers.insert(AUTHORIZATION, auth);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(headers)
    }

    fn request_body(&self, request: &LlmRequest, stream: bool) -> serde_json::Value {
        let mut messages = Vec::new();
        if let Some(system) = request.system_prompt.as_deref() {
            messages.push(json!({"role": "system", "content": system}));
        }
        messages.push(json!({"role": "user", "content": request.prompt}));
        json!({
            "model": request.model,
            "messages": messages,
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
            "stream": stream,
        })
    }
}

#[derive(Debug, Clone)]
pub struct AnthropicClient {
    base_url: String,
    api_key: String,
    version: String,
    http: reqwest::Client,
}

impl AnthropicClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self, LlmError> {
        Self::with_timeout(base_url, api_key, Duration::from_secs(60))
    }

    pub fn with_timeout(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, LlmError> {
        Self::with_version_and_timeout(base_url, api_key, "2023-06-01", timeout)
    }

    pub fn with_version_and_timeout(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        version: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| LlmError::Request(error.to_string()))?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            version: version.into(),
            http,
        })
    }

    pub async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let response = self
            .http
            .post(self.messages_url())
            .headers(self.headers()?)
            .json(&self.request_body(request))
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let response = error_for_status(response).await?;
        let payload: AnthropicMessageResponse = response
            .json()
            .await
            .map_err(|error| LlmError::Malformed(error.to_string()))?;
        payload.into_llm_response(&request.model)
    }

    fn messages_url(&self) -> String {
        format!("{}/v1/messages", self.base_url)
    }

    fn headers(&self) -> Result<HeaderMap, LlmError> {
        let mut headers = HeaderMap::new();
        let api_key = HeaderValue::from_str(&self.api_key)
            .map_err(|error| LlmError::Request(error.to_string()))?;
        let version = HeaderValue::from_str(&self.version)
            .map_err(|error| LlmError::Request(error.to_string()))?;
        headers.insert(HeaderName::from_static("x-api-key"), api_key);
        headers.insert(HeaderName::from_static("anthropic-version"), version);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(headers)
    }

    fn request_body(&self, request: &LlmRequest) -> serde_json::Value {
        let mut body = json!({
            "model": request.model,
            "messages": [{"role": "user", "content": request.prompt}],
            "max_tokens": request.max_tokens,
            "temperature": request.temperature,
        });
        if let Some(system) = request.system_prompt.as_deref() {
            body["system"] = json!(system);
        }
        body
    }
}

#[derive(Debug, Clone)]
pub struct GeminiClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl GeminiClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self, LlmError> {
        Self::with_timeout(base_url, api_key, Duration::from_secs(60))
    }

    pub fn with_timeout(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, LlmError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| LlmError::Request(error.to_string()))?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            http,
        })
    }

    pub async fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let response = self
            .http
            .post(self.generate_content_url(&request.model))
            .headers(self.headers()?)
            .json(&self.request_body(request))
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let response = error_for_status(response).await?;
        let payload: GeminiGenerateContentResponse = response
            .json()
            .await
            .map_err(|error| LlmError::Malformed(error.to_string()))?;
        payload.into_llm_response(&request.model)
    }

    fn generate_content_url(&self, model: &str) -> String {
        format!("{}/v1beta/models/{model}:generateContent", self.base_url)
    }

    fn headers(&self) -> Result<HeaderMap, LlmError> {
        let mut headers = HeaderMap::new();
        let api_key = HeaderValue::from_str(&self.api_key)
            .map_err(|error| LlmError::Request(error.to_string()))?;
        headers.insert(HeaderName::from_static("x-goog-api-key"), api_key);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(headers)
    }

    fn request_body(&self, request: &LlmRequest) -> serde_json::Value {
        let mut body = json!({
            "contents": [{
                "parts": [{"text": request.prompt}]
            }],
            "generationConfig": {
                "maxOutputTokens": request.max_tokens,
                "temperature": request.temperature,
            }
        });
        if let Some(system) = request.system_prompt.as_deref() {
            body["systemInstruction"] = json!({
                "parts": [{"text": system}]
            });
        }
        body
    }
}

async fn error_for_status(response: reqwest::Response) -> Result<reqwest::Response, LlmError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response.text().await.unwrap_or_default();
    if status.as_u16() == 429 {
        return Err(LlmError::RateLimited { retry_after, body });
    }
    Err(LlmError::HttpStatus {
        status: status.as_u16(),
        body,
        retryable: status.is_server_error(),
    })
}

fn map_reqwest_error(error: reqwest::Error) -> LlmError {
    if error.is_timeout() {
        LlmError::Timeout(error.to_string())
    } else {
        LlmError::Request(error.to_string())
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiChatResponse {
    model: Option<String>,
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
}

impl OpenAiChatResponse {
    fn into_llm_response(
        self,
        provider: LlmProviderKind,
        request_model: &str,
    ) -> Result<LlmResponse, LlmError> {
        let content = self
            .choices
            .first()
            .and_then(|choice| choice.message.as_ref())
            .and_then(|message| message.content.as_deref())
            .unwrap_or("")
            .to_string();
        if content.is_empty() {
            return Err(LlmError::Malformed(
                "missing choices[0].message.content".to_string(),
            ));
        }
        let usage = self.usage.unwrap_or_default();
        Ok(LlmResponse {
            provider: provider.as_str().to_string(),
            model: self.model.unwrap_or_else(|| request_model.to_string()),
            content,
            usage: LlmUsage {
                input_tokens: usage.prompt_tokens.unwrap_or(0),
                output_tokens: usage.completion_tokens.unwrap_or(0),
            },
        })
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: Option<OpenAiMessage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiMessage {
    content: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiUsage {
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct AnthropicMessageResponse {
    model: Option<String>,
    content: Vec<AnthropicContentBlock>,
    usage: Option<AnthropicUsage>,
}

impl AnthropicMessageResponse {
    fn into_llm_response(self, request_model: &str) -> Result<LlmResponse, LlmError> {
        let content = self
            .content
            .into_iter()
            .filter_map(|block| match block {
                AnthropicContentBlock::Text { text, .. } => Some(text),
                AnthropicContentBlock::Other => None,
            })
            .collect::<String>();
        if content.is_empty() {
            return Err(LlmError::Malformed(
                "missing content[] text block".to_string(),
            ));
        }
        let usage = self.usage.unwrap_or_default();
        Ok(LlmResponse {
            provider: LlmProviderKind::Anthropic.as_str().to_string(),
            model: self.model.unwrap_or_else(|| request_model.to_string()),
            content,
            usage: LlmUsage {
                input_tokens: usage.input_tokens.unwrap_or(0),
                output_tokens: usage.output_tokens.unwrap_or(0),
            },
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
struct AnthropicUsage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct GeminiGenerateContentResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageMetadata>,
    #[serde(rename = "modelVersion")]
    model_version: Option<String>,
}

impl GeminiGenerateContentResponse {
    fn into_llm_response(self, request_model: &str) -> Result<LlmResponse, LlmError> {
        let content = self
            .candidates
            .first()
            .and_then(|candidate| candidate.content.as_ref())
            .map(|content| {
                content
                    .parts
                    .iter()
                    .filter_map(|part| part.text.as_deref())
                    .collect::<String>()
            })
            .unwrap_or_default();
        if content.is_empty() {
            return Err(LlmError::Malformed(
                "missing candidates[0].content.parts[].text".to_string(),
            ));
        }
        let usage = self.usage_metadata.unwrap_or_default();
        Ok(LlmResponse {
            provider: LlmProviderKind::Gemini.as_str().to_string(),
            model: self
                .model_version
                .unwrap_or_else(|| request_model.to_string()),
            content,
            usage: LlmUsage {
                input_tokens: usage.prompt_token_count.unwrap_or(0),
                output_tokens: usage.candidates_token_count.unwrap_or(0),
            },
        })
    }
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiContent>,
}

#[derive(Debug, Deserialize)]
struct GeminiContent {
    #[serde(default)]
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Deserialize)]
struct GeminiPart {
    text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GeminiUsageMetadata {
    #[serde(rename = "promptTokenCount")]
    prompt_token_count: Option<u32>,
    #[serde(rename = "candidatesTokenCount")]
    candidates_token_count: Option<u32>,
}

fn split_sse_frame(buffer: &str) -> Option<(String, String)> {
    buffer
        .find("\n\n")
        .map(|idx| (buffer[..idx].to_string(), buffer[idx + 2..].to_string()))
}

fn apply_sse_frame(frame: &str, output: &mut String) -> Result<bool, LlmError> {
    let mut saw_done = false;
    for line in frame.lines().map(str::trim) {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data == "[DONE]" {
            saw_done = true;
            continue;
        }
        let event: OpenAiStreamEvent =
            serde_json::from_str(data).map_err(|error| LlmError::Malformed(error.to_string()))?;
        if let Some(delta) = event
            .choices
            .first()
            .and_then(|choice| choice.delta.as_ref())
            .and_then(|delta| delta.content.as_deref())
        {
            output.push_str(delta);
        }
    }
    Ok(saw_done)
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamEvent {
    choices: Vec<OpenAiStreamChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamChoice {
    delta: Option<OpenAiStreamDelta>,
}

#[derive(Debug, Deserialize)]
struct OpenAiStreamDelta {
    content: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn request() -> LlmRequest {
        LlmRequest::new("test-model", "hello")
            .system_prompt("system")
            .max_tokens(42)
            .temperature(0.1)
    }

    async fn client(server: &MockServer) -> OpenAiCompatibleClient {
        OpenAiCompatibleClient::with_timeout(
            LlmProviderKind::DeepSeek,
            server.uri(),
            "test-key",
            Duration::from_secs(1),
        )
        .unwrap()
    }

    async fn anthropic_client(server: &MockServer) -> AnthropicClient {
        AnthropicClient::with_timeout(server.uri(), "anthropic-key", Duration::from_secs(1))
            .unwrap()
    }

    async fn gemini_client(server: &MockServer) -> GeminiClient {
        GeminiClient::with_timeout(server.uri(), "gemini-key", Duration::from_secs(1)).unwrap()
    }

    fn unique_env_var(name: &str) -> String {
        format!("MA_TEST_{name}_{}", std::process::id())
    }

    fn temp_test_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("ma-llm-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_executable_script(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn fake_akm_timeout() -> Duration {
        Duration::from_secs(5)
    }

    #[test]
    fn key_resolver_prefers_env_var_over_missing_akm() {
        let env_var = unique_env_var("LLM_KEY");
        unsafe {
            env::set_var(&env_var, " env-secret \n");
        }
        let resolver = LlmKeyResolver::new("DEEPSEEK_API_KEY")
            .with_env_var(&env_var)
            .with_akm_bin("/missing/akm");

        let resolved = resolver.resolve().unwrap();

        assert_eq!(resolved.as_deref(), Some("env-secret"));
        unsafe {
            env::remove_var(env_var);
        }
    }

    #[cfg(unix)]
    #[test]
    fn key_resolver_falls_back_to_akm_get_y() {
        let dir = temp_test_dir("akm-get");
        let akm = dir.join("akm");
        let args_path = dir.join("args.txt");
        write_executable_script(
            &akm,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf 'akm-secret\\n'\n",
                args_path.display()
            ),
        );
        let env_var = unique_env_var("AKM_FALLBACK");
        unsafe {
            env::remove_var(&env_var);
        }
        let resolver = LlmKeyResolver::new("DEEPSEEK_API_KEY")
            .with_env_var(&env_var)
            .with_akm_bin(&akm)
            .with_timeout(fake_akm_timeout());

        let resolved = resolver.resolve().unwrap();

        assert_eq!(resolved.as_deref(), Some("akm-secret"));
        assert_eq!(
            fs::read_to_string(args_path).unwrap(),
            "get\nDEEPSEEK_API_KEY\n-y\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn key_resolver_returns_none_when_akm_fails() {
        let dir = temp_test_dir("akm-fail");
        let akm = dir.join("akm");
        write_executable_script(&akm, "#!/bin/sh\nprintf 'nope' >&2\nexit 42\n");
        let resolver = LlmKeyResolver::new("DEEPSEEK_API_KEY")
            .without_env_var()
            .with_akm_bin(&akm)
            .with_timeout(fake_akm_timeout());

        let resolved = resolver.resolve().unwrap();

        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn openai_compatible_complete_handles_200_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .and(body_string_contains("\"model\":\"test-model\""))
            .and(body_string_contains("\"role\":\"system\""))
            .and(body_string_contains("\"role\":\"user\""))
            .and(body_string_contains("\"stream\":false"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "test-model",
                "choices": [{"message": {"content": "world"}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 4}
            })))
            .mount(&server)
            .await;

        let response = client(&server).await.complete(&request()).await.unwrap();

        assert_eq!(response.provider, "deepseek");
        assert_eq!(response.model, "test-model");
        assert_eq!(response.content, "world");
        assert_eq!(response.usage.input_tokens, 3);
        assert_eq!(response.usage.output_tokens, 4);
    }

    #[tokio::test]
    async fn openai_compatible_complete_classifies_4xx_as_non_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .complete(&request())
            .await
            .expect_err("401 should fail");

        match error {
            LlmError::HttpStatus {
                status,
                body,
                retryable,
            } => {
                assert_eq!(status, 401);
                assert_eq!(body, "bad key");
                assert!(!retryable);
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn openai_compatible_complete_classifies_5xx_as_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("try later"))
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .complete(&request())
            .await
            .expect_err("503 should fail");

        assert!(error.is_retryable());
        match error {
            LlmError::HttpStatus { status, body, .. } => {
                assert_eq!(status, 503);
                assert_eq!(body, "try later");
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn openai_compatible_complete_classifies_429_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "2")
                    .set_body_string("slow down"),
            )
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .complete(&request())
            .await
            .expect_err("429 should fail");

        assert!(error.is_retryable());
        match error {
            LlmError::RateLimited { retry_after, body } => {
                assert_eq!(retry_after.as_deref(), Some("2"));
                assert_eq!(body, "slow down");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn openai_compatible_streaming_collects_sse_until_done() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\n\
                         data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n\
                         data: [DONE]\n\n",
                    ),
            )
            .mount(&server)
            .await;

        let response = client(&server)
            .await
            .complete_streaming(&request())
            .await
            .unwrap();

        assert_eq!(response.content, "hello");
        assert_eq!(response.usage, LlmUsage::default());
    }

    #[tokio::test]
    async fn openai_compatible_streaming_interrupt_is_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
                    ),
            )
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .complete_streaming(&request())
            .await
            .expect_err("missing [DONE] should fail");

        assert!(matches!(error, LlmError::StreamInterrupted(_)));
        assert!(error.is_retryable());
    }

    #[tokio::test]
    async fn openai_compatible_timeout_is_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(200))
                    .set_body_json(json!({
                        "choices": [{"message": {"content": "late"}}]
                    })),
            )
            .mount(&server)
            .await;
        let client = OpenAiCompatibleClient::with_timeout(
            LlmProviderKind::OpenAi,
            server.uri(),
            "test-key",
            Duration::from_millis(25),
        )
        .unwrap();

        let error = client
            .complete(&request())
            .await
            .expect_err("delayed response should time out");

        assert!(matches!(error, LlmError::Timeout(_)));
        assert!(error.is_retryable());
    }

    #[tokio::test]
    async fn anthropic_complete_posts_messages_headers_and_parses_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "anthropic-key"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(body_string_contains("\"model\":\"test-model\""))
            .and(body_string_contains("\"system\":\"system\""))
            .and(body_string_contains("\"max_tokens\":42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model": "claude-test",
                "content": [{"type": "text", "text": "anthropic world"}],
                "usage": {"input_tokens": 11, "output_tokens": 12}
            })))
            .mount(&server)
            .await;

        let response = anthropic_client(&server)
            .await
            .complete(&request())
            .await
            .unwrap();

        assert_eq!(response.provider, "anthropic");
        assert_eq!(response.model, "claude-test");
        assert_eq!(response.content, "anthropic world");
        assert_eq!(response.usage.input_tokens, 11);
        assert_eq!(response.usage.output_tokens, 12);
    }

    #[tokio::test]
    async fn anthropic_complete_classifies_429_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "5")
                    .set_body_string("rate limit"),
            )
            .mount(&server)
            .await;

        let error = anthropic_client(&server)
            .await
            .complete(&request())
            .await
            .expect_err("429 should fail");

        assert!(error.is_retryable());
        match error {
            LlmError::RateLimited { retry_after, body } => {
                assert_eq!(retry_after.as_deref(), Some("5"));
                assert_eq!(body, "rate limit");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn anthropic_complete_rejects_missing_text_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "content": [{"type": "tool_use", "id": "toolu_1"}],
                "usage": {"input_tokens": 1, "output_tokens": 0}
            })))
            .mount(&server)
            .await;

        let error = anthropic_client(&server)
            .await
            .complete(&request())
            .await
            .expect_err("missing text should fail");

        assert!(matches!(error, LlmError::Malformed(_)));
        assert!(!error.is_retryable());
    }

    #[tokio::test]
    async fn gemini_complete_posts_generate_content_headers_and_parses_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/test-model:generateContent"))
            .and(header("x-goog-api-key", "gemini-key"))
            .and(body_string_contains("\"text\":\"hello\""))
            .and(body_string_contains("\"systemInstruction\""))
            .and(body_string_contains("\"maxOutputTokens\":42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{
                    "content": {
                        "parts": [
                            {"text": "gemini "},
                            {"text": "world"}
                        ]
                    }
                }],
                "usageMetadata": {
                    "promptTokenCount": 13,
                    "candidatesTokenCount": 14
                },
                "modelVersion": "gemini-test"
            })))
            .mount(&server)
            .await;

        let response = gemini_client(&server)
            .await
            .complete(&request())
            .await
            .unwrap();

        assert_eq!(response.provider, "gemini");
        assert_eq!(response.model, "gemini-test");
        assert_eq!(response.content, "gemini world");
        assert_eq!(response.usage.input_tokens, 13);
        assert_eq!(response.usage.output_tokens, 14);
    }

    #[tokio::test]
    async fn gemini_complete_classifies_5xx_as_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/test-model:generateContent"))
            .respond_with(ResponseTemplate::new(502).set_body_string("upstream"))
            .mount(&server)
            .await;

        let error = gemini_client(&server)
            .await
            .complete(&request())
            .await
            .expect_err("502 should fail");

        assert!(error.is_retryable());
        match error {
            LlmError::HttpStatus { status, body, .. } => {
                assert_eq!(status, 502);
                assert_eq!(body, "upstream");
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn gemini_complete_rejects_missing_candidate_text() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/test-model:generateContent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "candidates": [{"content": {"parts": [{"inlineData": {}}]}}],
                "usageMetadata": {"promptTokenCount": 1}
            })))
            .mount(&server)
            .await;

        let error = gemini_client(&server)
            .await
            .complete(&request())
            .await
            .expect_err("missing text should fail");

        assert!(matches!(error, LlmError::Malformed(_)));
        assert!(!error.is_retryable());
    }
}
