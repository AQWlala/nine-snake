//! Thin async HTTP wrapper around the local Ollama server.
//!
//! This module intentionally exposes only the three endpoints we use
//! (chat, generate, embeddings) so the rest of the code base does not
//! have to know about Ollama's request/response shapes.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Role of a chat participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One chat message in an Ollama chat request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

/// `/api/chat` request body.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatMessage],
    pub stream: bool,
}

/// `/api/chat` response body (non-streaming).
#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    pub model: String,
    pub message: ChatMessage,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub total_duration: Option<u64>,
    #[serde(default)]
    pub eval_count: Option<u64>,
}

/// `/api/generate` request body.
#[derive(Debug, Clone, Serialize)]
pub struct GenerateRequest<'a> {
    pub model: &'a str,
    pub prompt: &'a str,
    pub stream: bool,
}

/// `/api/generate` response body (non-streaming).
#[derive(Debug, Clone, Deserialize)]
pub struct GenerateResponse {
    pub model: String,
    pub response: String,
    #[serde(default)]
    pub done: bool,
}

/// A long-lived HTTP client + base URL pair.
#[derive(Clone)]
pub struct OllamaClient {
    base_url: String,
    http: Client,
}

impl OllamaClient {
    /// Creates a new client targeting `base_url` (e.g.
    /// `http://127.0.0.1:11434`).
    pub fn new(base_url: impl Into<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .expect("reqwest client should build");
        Self {
            base_url: base_url.into(),
            http,
        }
    }

    /// Returns the configured base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns a reference to the underlying HTTP client. Useful for
    /// callers (e.g. the embedder) that need a `reqwest::Client`.
    pub fn http(&self) -> &Client {
        &self.http
    }

    /// Issues a non-streaming chat completion.
    pub async fn chat(
        &self,
        model: &str,
        messages: &[ChatMessage],
    ) -> anyhow::Result<ChatResponse> {
        let url = format!("{}/api/chat", self.base_url);
        let req = ChatRequest {
            model,
            messages,
            stream: false,
        };
        let resp: ChatResponse = self
            .http
            .post(&url)
            .json(&req)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }

    /// Issues a non-streaming generation.
    pub async fn generate(&self, model: &str, prompt: &str) -> anyhow::Result<GenerateResponse> {
        let url = format!("{}/api/generate", self.base_url);
        let req = GenerateRequest {
            model,
            prompt,
            stream: false,
        };
        let resp: GenerateResponse = self
            .http
            .post(&url)
            .json(&req)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp)
    }

    /// Lightweight health check that pings `/api/tags` (lists installed
    /// models). Returns `true` if the server responds with 2xx.
    pub async fn ping(&self) -> bool {
        let url = format!("{}/api/tags", self.base_url);
        match self.http.get(&url).send().await {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_constructors_set_role_correctly() {
        assert_eq!(ChatMessage::system("x").role, "system");
        assert_eq!(ChatMessage::user("x").role, "user");
        assert_eq!(ChatMessage::assistant("x").role, "assistant");
    }

    #[test]
    fn role_as_str() {
        assert_eq!(Role::System.as_str(), "system");
        assert_eq!(Role::User.as_str(), "user");
        assert_eq!(Role::Assistant.as_str(), "assistant");
    }
}
