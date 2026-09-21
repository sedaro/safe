use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::timeout;

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
}

pub trait LlmAdapterFactory: Send + Sync {
    fn kind(&self) -> &'static str;

    fn build(&self, config: &Value) -> Result<Box<dyn LlmAdapter>, AdapterError>;
}

/// The selected adapter and its provider-owned configuration.
#[derive(Debug, Clone, Deserialize)]
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
            client: Client::new(),
        }))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OllamaAdapterConfig {
    endpoint: String,
}

struct OllamaAdapter {
    endpoint: Url,
    client: Client,
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
        let body_text =
            post_json(&self.client, self.endpoint.clone(), body, request.timeout).await?;
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
}

#[derive(Debug, Deserialize)]
struct OllamaResponse {
    response: String,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    done_reason: Option<String>,
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
            client: Client::new(),
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
    endpoint: Url,
    api_key: Option<String>,
    client: Client,
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
            &self.client,
            self.endpoint.clone(),
            body,
            request.timeout,
            self.api_key.as_deref(),
        )
        .await?;
        let response: OpenAiCompatibleResponse =
            serde_json::from_str(&body_text).map_err(|error| {
                AdapterError::Response(format!("invalid OpenAI-compatible JSON payload: {error}"))
            })?;
        let choice = response.choices.into_iter().next().ok_or_else(|| {
            AdapterError::Response(
                "OpenAI-compatible response did not include a choice".to_string(),
            )
        })?;
        let text = choice
            .message
            .content
            .unwrap_or_default()
            .trim()
            .to_string();
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
}

#[derive(Debug, Deserialize)]
struct OpenAiCompatibleResponse {
    choices: Vec<OpenAiCompatibleChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiCompatibleChoice {
    message: OpenAiCompatibleMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiCompatibleMessage {
    #[serde(default)]
    content: Option<String>,
}

fn parse_endpoint(endpoint: &str, adapter: &str) -> Result<Url, AdapterError> {
    let url = Url::parse(endpoint).map_err(|error| {
        AdapterError::Configuration(format!(
            "{adapter} endpoint must be an absolute URL: {error}"
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(AdapterError::Configuration(format!(
            "{adapter} endpoint must be an absolute HTTP(S) URL"
        )));
    }
    Ok(url)
}

async fn post_json(
    client: &Client,
    endpoint: Url,
    body: Value,
    request_timeout: Duration,
) -> Result<String, AdapterError> {
    post_json_with_auth(client, endpoint, body, request_timeout, None).await
}

async fn post_json_with_auth(
    client: &Client,
    endpoint: Url,
    body: Value,
    request_timeout: Duration,
    api_key: Option<&str>,
) -> Result<String, AdapterError> {
    let mut request = client
        .post(endpoint)
        .header(CONTENT_TYPE, "application/json")
        .json(&body);
    if let Some(api_key) = api_key {
        request = request.header(AUTHORIZATION, format!("Bearer {api_key}"));
    }
    let response = timeout(request_timeout, request.send())
        .await
        .map_err(|_| AdapterError::Timeout)?
        .map_err(|error| AdapterError::Transport(error.to_string()))?;
    let status = response.status();
    let body_text = response.text().await.map_err(|error| {
        AdapterError::Transport(format!("failed reading response body: {error}"))
    })?;
    if !status.is_success() {
        return Err(AdapterError::Http {
            status: status.as_u16(),
            body: clip_chars(&body_text, 400),
        });
    }
    Ok(body_text)
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

    async fn mock_json_server(response_body: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test listener should bind");
        let endpoint = format!(
            "http://{}/completion",
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
}
