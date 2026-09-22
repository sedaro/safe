use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod http_client;

use http_client::Endpoint;

/// Provider-neutral request issued by an LLM caller.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub prompt: String,
    pub response_schema: Value,
    pub model: String,
    pub temperature: f64,
    pub max_output_tokens: u32,
    pub timeout: Duration,
}

/// Normalized completion result. Callers decide whether a truncated response is acceptable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    pub finish_reason: CompletionFinishReason,
}

/// Provider-neutral native tool-call request.
#[derive(Debug, Clone)]
pub struct ToolChatRequest {
    pub model: String,
    pub messages: Vec<ToolChatMessage>,
    /// OpenAI function-tool definitions. Ollama accepts the same shape.
    pub tools: Vec<Value>,
    pub temperature: f64,
    pub max_output_tokens: u32,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolChatMessage {
    pub role: String,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolChatCompletion {
    pub message: ToolChatMessage,
    pub finish_reason: CompletionFinishReason,
    /// Provider-specific attempt summary intended for opt-in caller diagnostics.
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionFinishReason {
    Complete,
    Length,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError {
    Configuration(String),
    Transport(String),
    Http { status: u16, body: String },
    Response(String),
    Timeout,
}

impl Display for AdapterError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Configuration(message) => write!(f, "adapter configuration error: {message}"),
            Self::Transport(message) => write!(f, "adapter transport error: {message}"),
            Self::Http { status, body } => {
                write!(f, "adapter returned HTTP status {status}: {body}")
            }
            Self::Response(message) => write!(f, "adapter response error: {message}"),
            Self::Timeout => write!(f, "adapter request timed out"),
        }
    }
}

impl std::error::Error for AdapterError {}

#[async_trait]
pub trait LlmAdapter: Send + Sync {
    fn kind(&self) -> &'static str;

    async fn complete(&self, request: CompletionRequest) -> Result<Completion, AdapterError>;

    async fn tool_chat(
        &self,
        _request: ToolChatRequest,
    ) -> Result<ToolChatCompletion, AdapterError> {
        Err(AdapterError::Configuration(format!(
            "{} adapter does not support native tool calls",
            self.kind()
        )))
    }
}

pub trait LlmAdapterFactory: Send + Sync {
    fn kind(&self) -> &'static str;

    fn build(&self, config: &Value) -> Result<Box<dyn LlmAdapter>, AdapterError>;
}

/// The selected adapter and its provider-owned configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterSelection {
    pub kind: String,
    pub config: Value,
}

/// Compile-time adapter registry. Custom adapters are linked into the binary and registered here.
#[derive(Default)]
pub struct AdapterRegistry {
    factories: HashMap<String, Box<dyn LlmAdapterFactory>>,
}

impl AdapterRegistry {
    pub fn with_builtin_adapters() -> Self {
        let mut registry = Self::default();
        registry
            .register(Box::new(OllamaAdapterFactory))
            .expect("built-in Ollama adapter kind must be unique");
        registry
            .register(Box::new(OpenAiCompatibleAdapterFactory))
            .expect("built-in OpenAI-compatible adapter kind must be unique");
        registry
    }

    pub fn register(&mut self, factory: Box<dyn LlmAdapterFactory>) -> Result<(), AdapterError> {
        let kind = factory.kind();
        if kind.trim().is_empty() {
            return Err(AdapterError::Configuration(
                "adapter kind must not be empty".to_string(),
            ));
        }
        if self.factories.contains_key(kind) {
            return Err(AdapterError::Configuration(format!(
                "adapter kind '{kind}' is already registered"
            )));
        }
        self.factories.insert(kind.to_string(), factory);
        Ok(())
    }

    pub fn build(&self, selection: &AdapterSelection) -> Result<Box<dyn LlmAdapter>, AdapterError> {
        let factory = self.factories.get(&selection.kind).ok_or_else(|| {
            let available = self
                .factories
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>();
            AdapterError::Configuration(format!(
                "unknown adapter kind '{}'; registered adapters: {}",
                selection.kind,
                available.join(", ")
            ))
        })?;
        factory.build(&selection.config)
    }
}

pub struct OllamaAdapterFactory;

impl LlmAdapterFactory for OllamaAdapterFactory {
    fn kind(&self) -> &'static str {
        "ollama"
    }

    fn build(&self, config: &Value) -> Result<Box<dyn LlmAdapter>, AdapterError> {
        let config: OllamaAdapterConfig =
            serde_json::from_value(config.clone()).map_err(|error| {
                AdapterError::Configuration(format!("invalid ollama adapter config: {error}"))
            })?;
        Ok(Box::new(OllamaAdapter {
            endpoint: parse_endpoint(&config.endpoint, "ollama")?,
        }))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OllamaAdapterConfig {
    endpoint: String,
}

struct OllamaAdapter {
    endpoint: Endpoint,
}

#[async_trait]
impl LlmAdapter for OllamaAdapter {
    fn kind(&self) -> &'static str {
        "ollama"
    }

    async fn complete(&self, request: CompletionRequest) -> Result<Completion, AdapterError> {
        let body = json!({
            "model": request.model,
            "prompt": request.prompt,
            "stream": false,
            "format": request.response_schema,
            "options": {
                "temperature": request.temperature,
                "num_predict": request.max_output_tokens,
            },
        });
        let body_text = post_json(&self.endpoint, body, request.timeout).await?;
        let response: OllamaResponse = serde_json::from_str(&body_text).map_err(|error| {
            AdapterError::Response(format!("invalid Ollama JSON payload: {error}"))
        })?;
        let text = response.response.trim();
        if text.is_empty() {
            return Err(AdapterError::Response(
                "Ollama response was empty".to_string(),
            ));
        }

        Ok(Completion {
            text: text.to_string(),
            finish_reason: match response.done_reason.as_deref() {
                Some("length") => CompletionFinishReason::Length,
                Some(_) => CompletionFinishReason::Other,
                None if response.done => CompletionFinishReason::Complete,
                None => CompletionFinishReason::Other,
            },
        })
    }

    async fn tool_chat(
        &self,
        request: ToolChatRequest,
    ) -> Result<ToolChatCompletion, AdapterError> {
        let mut endpoint = self.endpoint.clone();
        let path = endpoint.path().strip_suffix("/generate").ok_or_else(|| {
            AdapterError::Configuration(
                "ollama tool calls require an endpoint ending in /generate".to_string(),
            )
        })?;
        endpoint.set_path(&format!("{path}/chat"));
        let body = json!({
            "model": request.model,
            "messages": request.messages.iter().map(ollama_message).collect::<Vec<_>>(),
            "tools": request.tools,
            "stream": false,
            "options": {"temperature": request.temperature, "num_predict": request.max_output_tokens},
        });
        let body_text = post_json(&endpoint, body, request.timeout).await?;
        let response: OllamaChatResponse = serde_json::from_str(&body_text).map_err(|error| {
            AdapterError::Response(format!("invalid Ollama chat JSON payload: {error}"))
        })?;
        Ok(ToolChatCompletion {
            message: ToolChatMessage {
                role: response.message.role,
                content: response.message.content,
                tool_calls: response
                    .message
                    .tool_calls
                    .into_iter()
                    .map(|call| ToolCall {
                        name: call.function.name,
                        arguments: call.function.arguments,
                    })
                    .collect(),
            },
            finish_reason: ollama_finish_reason(response.done, response.done_reason.as_deref()),
            diagnostic: None,
        })
    }
}

#[derive(Debug, Deserialize)]
struct OllamaResponse {
    response: String,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: OllamaChatMessage,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct OllamaChatMessage {
    role: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<OllamaToolCall>,
}

#[derive(Serialize, Deserialize)]
struct OllamaToolCall {
    function: OllamaToolFunction,
}

#[derive(Serialize, Deserialize)]
struct OllamaToolFunction {
    name: String,
    arguments: Value,
}

fn ollama_message(message: &ToolChatMessage) -> OllamaChatMessage {
    OllamaChatMessage {
        role: message.role.clone(),
        content: message.content.clone(),
        tool_calls: message
            .tool_calls
            .iter()
            .map(|call| OllamaToolCall {
                function: OllamaToolFunction {
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                },
            })
            .collect(),
    }
}

fn ollama_finish_reason(done: bool, done_reason: Option<&str>) -> CompletionFinishReason {
    match done_reason {
        Some("length") => CompletionFinishReason::Length,
        Some(_) => CompletionFinishReason::Other,
        None if done => CompletionFinishReason::Complete,
        None => CompletionFinishReason::Other,
    }
}

pub struct OpenAiCompatibleAdapterFactory;

impl LlmAdapterFactory for OpenAiCompatibleAdapterFactory {
    fn kind(&self) -> &'static str {
        "openai_compatible"
    }

    fn build(&self, config: &Value) -> Result<Box<dyn LlmAdapter>, AdapterError> {
        let config: OpenAiCompatibleAdapterConfig = serde_json::from_value(config.clone())
            .map_err(|error| {
                AdapterError::Configuration(format!(
                    "invalid openai_compatible adapter config: {error}"
                ))
            })?;
        let api_key = config
            .api_key_env
            .map(|name| {
                if name.trim().is_empty() {
                    return Err(AdapterError::Configuration(
                        "openai_compatible api_key_env must not be empty".to_string(),
                    ));
                }
                std::env::var(&name).map_err(|_| {
                    AdapterError::Configuration(format!(
                        "openai_compatible API key environment variable '{name}' is not set"
                    ))
                })
            })
            .transpose()?;
        if api_key.as_ref().is_some_and(|key| key.trim().is_empty()) {
            return Err(AdapterError::Configuration(
                "openai_compatible API key environment variable must not be empty".to_string(),
            ));
        }

        Ok(Box::new(OpenAiCompatibleAdapter {
            endpoint: parse_endpoint(&config.endpoint, "openai_compatible")?,
            api_key,
        }))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenAiCompatibleAdapterConfig {
    endpoint: String,
    #[serde(default)]
    api_key_env: Option<String>,
}

struct OpenAiCompatibleAdapter {
    endpoint: Endpoint,
    api_key: Option<String>,
}

#[async_trait]
impl LlmAdapter for OpenAiCompatibleAdapter {
    fn kind(&self) -> &'static str {
        "openai_compatible"
    }

    async fn complete(&self, request: CompletionRequest) -> Result<Completion, AdapterError> {
        let body = json!({
            "model": request.model,
            "messages": [{"role": "user", "content": request.prompt}],
            "temperature": request.temperature,
            "max_tokens": request.max_output_tokens,
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "anomaly_recovery_decision",
                    "strict": true,
                    "schema": request.response_schema,
                },
            },
        });
        let body_text = post_json_with_auth(
            &self.endpoint,
            body.clone(),
            request.timeout,
            self.api_key.as_deref(),
        )
        .await?;
        let response: OpenAiCompatibleResponse =
            serde_json::from_str(&body_text).map_err(|error| {
                AdapterError::Response(format!(
                    "invalid OpenAI-compatible JSON payload: {error}; body={}",
                    clip_chars(&body_text, 600)
                ))
            })?;
        let choice = response.choices.into_iter().next().ok_or_else(|| {
            AdapterError::Response(
                "OpenAI-compatible response did not include a choice".to_string(),
            )
        })?;
        let message = choice_message(&choice)?;
        let text = message.content.unwrap_or_default().trim().to_string();
        if text.is_empty() {
            return Err(AdapterError::Response(
                "OpenAI-compatible response content was empty".to_string(),
            ));
        }

        Ok(Completion {
            text,
            finish_reason: match choice.finish_reason.as_deref() {
                Some("stop") | Some("end_turn") => CompletionFinishReason::Complete,
                Some("length") | Some("max_tokens") => CompletionFinishReason::Length,
                _ => CompletionFinishReason::Other,
            },
        })
    }

    async fn tool_chat(
        &self,
        request: ToolChatRequest,
    ) -> Result<ToolChatCompletion, AdapterError> {
        let body = json!({
            "model": request.model,
            "messages": request.messages.iter().map(openai_message).collect::<Vec<_>>(),
            "tools": request.tools,
            "tool_choice": "required",
            // Current OpenAI reasoning models require native tools to run
            // without reasoning in the Chat Completions API.
            "reasoning_effort": "none",
            "temperature": request.temperature,
            "max_completion_tokens": request.max_output_tokens,
        });
        let body_text = post_json_with_auth(
            &self.endpoint,
            body.clone(),
            request.timeout,
            self.api_key.as_deref(),
        )
        .await?;
        let first = parse_tool_chat_response(&body_text)?;
        if first.finish_reason == CompletionFinishReason::Length {
            // Some OpenAI-compatible local servers ignore max_completion_tokens
            // and only honor the legacy max_tokens field.
            let mut legacy_body = body;
            if let Some(object) = legacy_body.as_object_mut() {
                object.remove("max_completion_tokens");
                object.insert("max_tokens".to_string(), json!(request.max_output_tokens));
            }
            if let Ok(legacy_text) = post_json_with_auth(
                &self.endpoint,
                legacy_body,
                request.timeout,
                self.api_key.as_deref(),
            )
            .await
                && let Ok(legacy_result) = parse_tool_chat_response(&legacy_text)
            {
                let diagnostic = openai_tool_attempt_diagnostic(
                    &first,
                    "max_completion_tokens",
                    &body_text,
                    &legacy_result,
                    "max_tokens",
                    &legacy_text,
                );
                if legacy_result.finish_reason != CompletionFinishReason::Length
                    || !legacy_result.message.tool_calls.is_empty()
                {
                    return Ok(ToolChatCompletion {
                        diagnostic: Some(diagnostic),
                        ..legacy_result
                    });
                }
                return Ok(ToolChatCompletion {
                    diagnostic: Some(diagnostic),
                    ..first
                });
            }
        }
        Ok(first)
    }
}

fn parse_tool_chat_response(body_text: &str) -> Result<ToolChatCompletion, AdapterError> {
    let response: OpenAiCompatibleResponse = serde_json::from_str(body_text).map_err(|error| {
        AdapterError::Response(format!(
            "invalid OpenAI-compatible JSON payload: {error}; body={}",
            clip_chars(body_text, 600)
        ))
    })?;
    if response.object.as_deref() == Some("text_completion") {
        return Err(AdapterError::Response(
            "OpenAI-compatible tool calls require a Chat Completions endpoint, but the server returned object=text_completion; configure the adapter endpoint for /v1/chat/completions rather than /v1/completions".to_string(),
        ));
    }
    let choice = response.choices.into_iter().next().ok_or_else(|| {
        AdapterError::Response("OpenAI-compatible response did not include a choice".to_string())
    })?;
    let finish_reason = openai_finish_reason(choice.finish_reason.as_deref());
    let Some(message) = choice.message.or(choice.delta) else {
        if finish_reason != CompletionFinishReason::Length {
            return Err(AdapterError::Response(
                "OpenAI-compatible choice contained neither message nor delta".to_string(),
            ));
        }
        return Ok(ToolChatCompletion {
            message: ToolChatMessage {
                role: "assistant".to_string(),
                content: String::new(),
                tool_calls: Vec::new(),
            },
            finish_reason,
            diagnostic: None,
        });
    };
    let tool_calls = message
        .tool_calls
        .into_iter()
        .map(parse_openai_tool_call)
        .collect::<Result<Vec<_>, _>>()?;
    let content = message.content.unwrap_or_default();
    // A complete native tool-call envelope is executable after host validation,
    // even if a local server spends its final token on the envelope terminator.
    let finish_reason = if finish_reason == CompletionFinishReason::Length
        && content.trim().is_empty()
        && !tool_calls.is_empty()
    {
        CompletionFinishReason::Complete
    } else {
        finish_reason
    };
    Ok(ToolChatCompletion {
        message: ToolChatMessage {
            role: message.role.unwrap_or_else(|| "assistant".to_string()),
            content,
            tool_calls,
        },
        finish_reason,
        diagnostic: None,
    })
}

fn openai_tool_attempt_diagnostic(
    first: &ToolChatCompletion,
    first_token_parameter: &str,
    first_body: &str,
    second: &ToolChatCompletion,
    second_token_parameter: &str,
    second_body: &str,
) -> String {
    format!(
        "{first_token_parameter}:finish_reason={:?},tool_calls={},assistant_content={:?},response_body={:?}; {second_token_parameter}:finish_reason={:?},tool_calls={},assistant_content={:?},response_body={:?}",
        first.finish_reason,
        first.message.tool_calls.len(),
        clip_chars(&first.message.content, 240),
        clip_chars(first_body, 600),
        second.finish_reason,
        second.message.tool_calls.len(),
        clip_chars(&second.message.content, 240),
        clip_chars(second_body, 600),
    )
}

#[derive(Debug, Deserialize, Serialize)]
struct OpenAiCompatibleResponse {
    #[serde(default)]
    object: Option<String>,
    choices: Vec<OpenAiCompatibleChoice>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OpenAiCompatibleChoice {
    #[serde(default)]
    message: Option<OpenAiCompatibleMessage>,
    #[serde(default)]
    delta: Option<OpenAiCompatibleMessage>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct OpenAiCompatibleMessage {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAiCompatibleToolCall>,
}

fn choice_message(
    choice: &OpenAiCompatibleChoice,
) -> Result<OpenAiCompatibleMessage, AdapterError> {
    choice
        .message
        .clone()
        .or_else(|| choice.delta.clone())
        .ok_or_else(|| {
            AdapterError::Response(format!(
                "OpenAI-compatible choice contained neither message nor delta: {}",
                serde_json::to_string(choice).unwrap_or_else(|_| "<unserializable choice>".into())
            ))
        })
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct OpenAiCompatibleToolCall {
    #[serde(rename = "type")]
    kind: String,
    function: OpenAiCompatibleToolFunction,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct OpenAiCompatibleToolFunction {
    name: String,
    arguments: String,
}

fn openai_message(message: &ToolChatMessage) -> Value {
    let mut value = json!({"role": message.role, "content": message.content});
    if !message.tool_calls.is_empty() {
        value["tool_calls"] = Value::Array(message.tool_calls.iter().map(|call| json!({
            "type": "function", "function": {"name": call.name, "arguments": call.arguments.to_string()}
        })).collect());
    }
    value
}

fn parse_openai_tool_call(call: OpenAiCompatibleToolCall) -> Result<ToolCall, AdapterError> {
    if call.kind != "function" {
        return Err(AdapterError::Response(
            "OpenAI-compatible tool call was not a function".to_string(),
        ));
    }
    let arguments = serde_json::from_str(&call.function.arguments).map_err(|error| {
        AdapterError::Response(format!(
            "OpenAI-compatible tool-call arguments were not JSON: {error}"
        ))
    })?;
    Ok(ToolCall {
        name: call.function.name,
        arguments,
    })
}

fn openai_finish_reason(reason: Option<&str>) -> CompletionFinishReason {
    match reason {
        Some("stop") | Some("end_turn") | Some("tool_calls") => CompletionFinishReason::Complete,
        Some("length") | Some("max_tokens") => CompletionFinishReason::Length,
        _ => CompletionFinishReason::Other,
    }
}

fn parse_endpoint(endpoint: &str, adapter: &str) -> Result<Endpoint, AdapterError> {
    Endpoint::parse(endpoint, adapter)
}

async fn post_json(
    endpoint: &Endpoint,
    body: Value,
    request_timeout: Duration,
) -> Result<String, AdapterError> {
    post_json_with_auth(endpoint, body, request_timeout, None).await
}

async fn post_json_with_auth(
    endpoint: &Endpoint,
    body: Value,
    request_timeout: Duration,
    api_key: Option<&str>,
) -> Result<String, AdapterError> {
    http_client::post_json(endpoint, &body.to_string(), request_timeout, api_key).await
}

fn clip_chars(input: &str, max_chars: usize) -> String {
    input.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn completion_request() -> CompletionRequest {
        CompletionRequest {
            prompt: "select exactly one action".to_string(),
            response_schema: json!({"type": "object", "required": ["action_id"]}),
            model: "test-model".to_string(),
            temperature: 0.0,
            max_output_tokens: 64,
            timeout: Duration::from_secs(1),
        }
    }

    fn tool_chat_request() -> ToolChatRequest {
        ToolChatRequest {
            model: "test-model".to_string(),
            messages: vec![ToolChatMessage {
                role: "user".to_string(),
                content: "select exactly one action".to_string(),
                tool_calls: Vec::new(),
            }],
            tools: vec![
                json!({"type":"function","function":{"name":"select_recovery_action","parameters":{"type":"object"}}}),
            ],
            temperature: 0.0,
            max_output_tokens: 64,
            timeout: Duration::from_secs(1),
        }
    }

    async fn mock_json_server(response_body: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let endpoint = format!(
            "http://{}/api/generate",
            listener
                .local_addr()
                .expect("listener should have an address")
        );
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("test client should connect");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            let expected_len = loop {
                let read = stream
                    .read(&mut buffer)
                    .await
                    .expect("test request should be readable");
                assert_ne!(read, 0, "test client closed before a complete request");
                request.extend_from_slice(&buffer[..read]);
                let Some(headers_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = std::str::from_utf8(&request[..headers_end])
                    .expect("test request headers should be UTF-8");
                let content_length = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .or_else(|| {
                        headers
                            .lines()
                            .find_map(|line| line.strip_prefix("Content-Length: "))
                    })
                    .expect("test request should include content length")
                    .parse::<usize>()
                    .expect("content length should be numeric");
                let total_len = headers_end + 4 + content_length;
                if request.len() >= total_len {
                    break total_len;
                }
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("test response should be writable");
            String::from_utf8(request[..expected_len].to_vec())
                .expect("test request should be UTF-8")
        });
        (endpoint, task)
    }

    #[test]
    fn registry_builds_builtin_ollama_adapter() {
        let registry = AdapterRegistry::with_builtin_adapters();
        let adapter = registry
            .build(&AdapterSelection {
                kind: "ollama".to_string(),
                config: json!({"endpoint": "http://127.0.0.1:11434/api/generate"}),
            })
            .expect("Ollama adapter should build");
        assert_eq!(adapter.kind(), "ollama");
    }

    #[test]
    fn registry_rejects_unknown_adapter() {
        let registry = AdapterRegistry::with_builtin_adapters();
        let error = match registry.build(&AdapterSelection {
            kind: "unknown".to_string(),
            config: json!({}),
        }) {
            Err(error) => error,
            Ok(_) => panic!("unknown adapter should fail"),
        };
        assert!(error.to_string().contains("unknown adapter kind 'unknown'"));
    }

    #[test]
    fn ollama_configuration_rejects_unknown_fields() {
        let registry = AdapterRegistry::with_builtin_adapters();
        let error = match registry.build(&AdapterSelection {
            kind: "ollama".to_string(),
            config: json!({
                "endpoint": "http://127.0.0.1:11434/api/generate",
                "extra": true,
            }),
        }) {
            Err(error) => error,
            Ok(_) => panic!("unknown provider field should fail"),
        };
        assert!(error.to_string().contains("unknown field"));
    }

    #[tokio::test]
    async fn ollama_adapter_translates_a_constrained_completion() {
        let response = json!({
            "response": "{\"action_id\":\"point_nadir\"}",
            "done": true,
            "done_reason": "length"
        })
        .to_string();
        let (endpoint, server) = mock_json_server(response).await;
        let registry = AdapterRegistry::with_builtin_adapters();
        let adapter = registry
            .build(&AdapterSelection {
                kind: "ollama".to_string(),
                config: json!({"endpoint": endpoint}),
            })
            .expect("Ollama adapter should build");

        let completion = adapter
            .complete(completion_request())
            .await
            .expect("Ollama completion should parse");
        let request = server.await.expect("test server should finish");
        assert_eq!(completion.text, "{\"action_id\":\"point_nadir\"}");
        assert_eq!(completion.finish_reason, CompletionFinishReason::Length);
        assert!(request.contains("\"format\""));
        assert!(request.contains("\"num_predict\":64"));
    }

    #[tokio::test]
    async fn openai_compatible_adapter_translates_a_constrained_completion() {
        let response = json!({
            "choices": [{
                "message": {"content": "{\"action_id\":\"point_nadir\"}"},
                "finish_reason": "stop"
            }]
        })
        .to_string();
        let (endpoint, server) = mock_json_server(response).await;
        let registry = AdapterRegistry::with_builtin_adapters();
        let adapter = registry
            .build(&AdapterSelection {
                kind: "openai_compatible".to_string(),
                config: json!({"endpoint": endpoint}),
            })
            .expect("OpenAI-compatible adapter should build");

        let completion = adapter
            .complete(completion_request())
            .await
            .expect("OpenAI-compatible completion should parse");
        let request = server.await.expect("test server should finish");
        assert_eq!(completion.text, "{\"action_id\":\"point_nadir\"}");
        assert_eq!(completion.finish_reason, CompletionFinishReason::Complete);
        assert!(request.contains("\"response_format\""));
        assert!(request.contains("\"max_tokens\":64"));
    }

    #[tokio::test]
    async fn openai_compatible_adapter_normalizes_native_tool_calls() {
        let response = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "type": "function",
                        "function": {
                            "name": "select_recovery_action",
                            "arguments": "{\"anomaly_id\":\"thermal\",\"action_id\":\"point_nadir\",\"reason\":\"approved\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }).to_string();
        let (endpoint, server) = mock_json_server(response).await;
        let adapter = AdapterRegistry::with_builtin_adapters()
            .build(&AdapterSelection {
                kind: "openai_compatible".to_string(),
                config: json!({"endpoint": endpoint}),
            })
            .expect("OpenAI-compatible adapter should build");

        let completion = adapter
            .tool_chat(tool_chat_request())
            .await
            .expect("tool call should parse");
        let request = server.await.expect("test server should finish");
        assert_eq!(completion.finish_reason, CompletionFinishReason::Complete);
        assert_eq!(completion.message.tool_calls.len(), 1);
        assert_eq!(
            completion.message.tool_calls[0].name,
            "select_recovery_action"
        );
        assert_eq!(
            completion.message.tool_calls[0].arguments["action_id"],
            "point_nadir"
        );
        assert!(request.contains("\"tools\""));
        assert!(request.contains("\"tool_choice\":\"required\""));
        assert!(request.contains("\"reasoning_effort\":\"none\""));
        assert!(request.contains("\"max_completion_tokens\":64"));
    }

    #[tokio::test]
    async fn openai_compatible_adapter_reports_choice_without_message() {
        let response = json!({
            "choices": [{"finish_reason": "stop"}]
        })
        .to_string();
        let (endpoint, server) = mock_json_server(response).await;
        let adapter = AdapterRegistry::with_builtin_adapters()
            .build(&AdapterSelection {
                kind: "openai_compatible".to_string(),
                config: json!({"endpoint": endpoint}),
            })
            .expect("OpenAI-compatible adapter should build");

        let error = adapter
            .tool_chat(tool_chat_request())
            .await
            .expect_err("missing choice message must fail closed");
        assert!(error.to_string().contains("neither message nor delta"));
        let _ = server.await.expect("test server should finish");
    }

    #[tokio::test]
    async fn openai_compatible_adapter_rejects_text_completion_for_tool_calls() {
        let response = json!({
            "object": "text_completion",
            "choices": [{
                "text": "unrelated continuation",
                "finish_reason": "length"
            }]
        })
        .to_string();
        let (endpoint, server) = mock_json_server(response).await;
        let adapter = AdapterRegistry::with_builtin_adapters()
            .build(&AdapterSelection {
                kind: "openai_compatible".to_string(),
                config: json!({"endpoint": endpoint}),
            })
            .expect("OpenAI-compatible adapter should build");

        let error = adapter
            .tool_chat(tool_chat_request())
            .await
            .expect_err("text completions cannot carry native tool calls");

        assert!(error.to_string().contains("/v1/chat/completions"));
        let _ = server.await.expect("test server should finish");
    }

    #[tokio::test]
    async fn openai_compatible_adapter_preserves_length_without_message() {
        let response = json!({
            "choices": [{"message": null, "finish_reason": "length"}]
        })
        .to_string();
        let (endpoint, server) = mock_json_server(response).await;
        let adapter = AdapterRegistry::with_builtin_adapters()
            .build(&AdapterSelection {
                kind: "openai_compatible".to_string(),
                config: json!({"endpoint": endpoint}),
            })
            .expect("OpenAI-compatible adapter should build");

        let completion = adapter
            .tool_chat(tool_chat_request())
            .await
            .expect("length response should remain classified");
        assert_eq!(completion.finish_reason, CompletionFinishReason::Length);
        assert!(completion.message.tool_calls.is_empty());
        let _ = server.await.expect("test server should finish");
    }

    #[tokio::test]
    async fn openai_compatible_adapter_accepts_complete_tool_call_at_length() {
        let response = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "type": "function",
                        "function": {
                            "name": "get_latest_telemetry",
                            "arguments": "{}"
                        }
                    }]
                },
                "finish_reason": "length"
            }]
        })
        .to_string();
        let (endpoint, server) = mock_json_server(response).await;
        let adapter = AdapterRegistry::with_builtin_adapters()
            .build(&AdapterSelection {
                kind: "openai_compatible".to_string(),
                config: json!({"endpoint": endpoint}),
            })
            .expect("OpenAI-compatible adapter should build");

        let completion = adapter
            .tool_chat(tool_chat_request())
            .await
            .expect("complete native tool call should remain usable");
        assert_eq!(completion.finish_reason, CompletionFinishReason::Complete);
        assert_eq!(completion.message.tool_calls.len(), 1);
        assert_eq!(
            completion.message.tool_calls[0].name,
            "get_latest_telemetry"
        );
        let _ = server.await.expect("test server should finish");
    }

    #[test]
    fn openai_tool_attempt_diagnostic_identifies_both_token_parameters() {
        let truncated = ToolChatCompletion {
            message: ToolChatMessage {
                role: "assistant".to_string(),
                content: "<tool_call>".to_string(),
                tool_calls: Vec::new(),
            },
            finish_reason: CompletionFinishReason::Length,
            diagnostic: None,
        };

        let diagnostic = openai_tool_attempt_diagnostic(
            &truncated,
            "max_completion_tokens",
            "{\"choices\":[{\"finish_reason\":\"length\"}]}",
            &truncated,
            "max_tokens",
            "{\"choices\":[{\"finish_reason\":\"length\"}]}",
        );

        assert!(diagnostic.contains("max_completion_tokens:finish_reason=Length,tool_calls=0"));
        assert!(diagnostic.contains("max_tokens:finish_reason=Length,tool_calls=0"));
        assert!(diagnostic.contains("assistant_content=\"<tool_call>\""));
        assert!(diagnostic.contains("response_body=\"{\\\"choices\\\""));
    }

    #[tokio::test]
    async fn ollama_adapter_uses_native_chat_for_tool_calls() {
        let response = json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": "select_recovery_action",
                        "arguments": {"anomaly_id":"thermal","action_id":"point_nadir","reason":"approved"}
                    }
                }]
            },
            "done": true,
            "done_reason": "stop"
        })
        .to_string();
        let (endpoint, server) = mock_json_server(response).await;
        let adapter = AdapterRegistry::with_builtin_adapters()
            .build(&AdapterSelection {
                kind: "ollama".to_string(),
                config: json!({"endpoint": endpoint}),
            })
            .expect("Ollama adapter should build");

        let completion = adapter
            .tool_chat(tool_chat_request())
            .await
            .expect("tool call should parse");
        let request = server.await.expect("test server should finish");
        assert_eq!(completion.message.tool_calls.len(), 1);
        assert_eq!(
            completion.message.tool_calls[0].name,
            "select_recovery_action"
        );
        assert!(request.starts_with("POST /api/chat HTTP/1.1"));
        assert!(request.contains("\"num_predict\":64"));
    }
}
