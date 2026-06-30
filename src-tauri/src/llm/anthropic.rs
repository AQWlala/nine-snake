//! v1.1 P0-1: Anthropic Claude HTTP client.
//!
//! Supports `claude-3-5-haiku`、`claude-3-5-sonnet`、`claude-3-opus`
//! 等模型。使用 Anthropic Messages API (`POST /v1/messages`)，
//! 并通过 `x-api-key` header 进行认证。

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Anthropic message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}

/// A single message in an Anthropic conversation.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

/// Response from the Claude API.
#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(default)]
    pub text: Option<String>,
}

/// Request payload for the Anthropic Messages API.
#[derive(Debug, Serialize)]
struct Request {
    model: String,
    messages: Vec<RequestMessage>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
}

/// A message inside an Anthropic API request.
#[derive(Debug, Serialize)]
struct RequestMessage {
    role: String,
    content: String,
}

/// Anthropic Claude HTTP client.
#[derive(Clone)]
pub struct AnthropicClient {
    base_url: String,
    api_key: String,
    pub model: String,
    http: Client,
}

impl AnthropicClient {
    /// Creates a new client.
    ///
    /// # Arguments
    /// * `api_key` — Anthropic API key (`sk-ant-...`).
    /// * `model` — Model name, e.g. `claude-3-5-haiku-20241022`.
    /// * `base_url` — Override for proxy/self-hosted deployments.
    pub fn new(api_key: String, model: String, base_url: Option<String>) -> Self {
        let base_url = base_url.unwrap_or_else(|| "https://api.anthropic.com".to_string());
        Self {
            base_url,
            api_key,
            model,
            http: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("reqwest client should build"),
        }
    }

    /// Sends a chat request and returns the assistant's text response.
    pub async fn chat(&self, messages: &[Message]) -> Result<String> {
        let payload = self.build_request(messages);
        let url = format!("{}/v1/messages", self.base_url);
        let ssrf_guard = crate::security::SsrfGuard::new();
        ssrf_guard
            .validate_url(&url)
            .map_err(|e| anyhow::anyhow!("SSRF validation failed: {e}"))?;

        debug!(target: "nine_snake.llm", model = %self.model, "calling Anthropic API");

        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("Anthropic HTTP request to {url} failed"))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            warn!(
                target: "nine_snake.llm",
                status = %status,
                body = %body,
                "Anthropic API error"
            );
            return Err(anyhow!("Anthropic API error {status}: {body}"));
        }

        let parsed: Response = resp
            .json()
            .await
            .with_context(|| "failed to parse Anthropic response")?;

        let text = parsed
            .content
            .iter()
            .filter(|b| b.block_type == "text")
            .filter_map(|b| b.text.clone())
            .collect::<Vec<_>>()
            .join("\n\n");

        if text.is_empty() {
            return Err(anyhow!("Anthropic returned no text content"));
        }

        debug!(target: "nine_snake.llm", chars = text.len(), "Anthropic response received");
        Ok(text)
    }

    fn build_request(&self, messages: &[Message]) -> Request {
        let mut system: Option<String> = None;
        let msgs: Vec<RequestMessage> = messages
            .iter()
            .filter_map(|m| {
                let role_str = match m.role {
                    Role::System => {
                        // Anthropic uses a dedicated system field.
                        system = Some(m.content.clone());
                        return None;
                    }
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                Some(RequestMessage {
                    role: role_str.to_string(),
                    content: m.content.clone(),
                })
            })
            .collect();

        Request {
            model: self.model.clone(),
            messages: msgs,
            max_tokens: 4096,
            system,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_message_mapping() {
        let client = AnthropicClient::new(
            "sk-ant-test".to_string(),
            "claude-3-5-haiku-20241022".to_string(),
            None,
        );

        let messages = vec![
            Message {
                role: Role::System,
                content: "You are a helpful assistant.".to_string(),
            },
            Message {
                role: Role::User,
                content: "Hello, Claude.".to_string(),
            },
        ];

        let req = client.build_request(&messages);
        assert_eq!(req.model, "claude-3-5-haiku-20241022");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        assert_eq!(req.system, Some("You are a helpful assistant.".to_string()));
    }
}
