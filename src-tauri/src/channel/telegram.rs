use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::types::{ChannelAdapter, ChannelKind, ChannelStatus};

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramMessage {
    update_id: i64,
    message: Option<TelegramMessageInner>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramMessageInner {
    message_id: i64,
    chat: TelegramChat,
    text: Option<String>,
    from: Option<TelegramUser>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramChat {
    id: i64,
    #[serde(rename = "type")]
    chat_type: String,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TelegramUser {
    id: i64,
    first_name: String,
    username: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SendMessageRequest {
    chat_id: i64,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to_message_id: Option<i64>,
}

pub struct TelegramBotAdapter {
    bot_token: String,
    client: Client,
    status: Arc<Mutex<ChannelStatus>>,
    last_update_id: Arc<Mutex<i64>>,
}

impl TelegramBotAdapter {
    pub fn new(bot_token: &str) -> Self {
        Self {
            bot_token: bot_token.to_string(),
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest Client::build is infallible"),
            status: Arc::new(Mutex::new(ChannelStatus::Offline)),
            last_update_id: Arc::new(Mutex::new(0)),
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.bot_token, method)
    }

    pub async fn poll_updates(&self) -> Result<Vec<(i64, String, String)>> {
        let offset = *self.last_update_id.lock() + 1;
        let url = self.api_url("getUpdates");
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "offset": offset,
                "timeout": 10,
                "limit": 100,
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            *self.status.lock() = ChannelStatus::Failed;
            anyhow::bail!("Telegram getUpdates HTTP error: {}", resp.status());
        }

        let body: serde_json::Value = resp.json().await?;
        let updates = body
            .get("result")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();

        let mut messages = Vec::new();
        for update in updates {
            if let Some(update_id) = update.get("update_id").and_then(|v| v.as_i64()) {
                *self.last_update_id.lock() = update_id;
            }
            if let Some(msg) = update.get("message") {
                let chat_id = msg
                    .get("chat")
                    .and_then(|c| c.get("id"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let text = msg
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !text.is_empty() && chat_id != 0 {
                    messages.push((chat_id, format!("tg_{}", chat_id), text));
                }
            }
        }
        Ok(messages)
    }

    pub async fn send_message(
        &self,
        chat_id: i64,
        text: &str,
        reply_to: Option<i64>,
    ) -> Result<()> {
        let url = self.api_url("sendMessage");
        let mut req = SendMessageRequest {
            chat_id,
            text: text.to_string(),
            reply_to_message_id: None,
        };
        if let Some(r) = reply_to {
            req.reply_to_message_id = Some(r);
        }
        let resp = self.client.post(&url).json(&req).send().await?;
        if resp.status().as_u16() == 429 {
            *self.status.lock() = ChannelStatus::RateLimited;
            warn!(target: "nine_snake.telegram", "rate limited by Telegram API");
        } else if !resp.status().is_success() {
            *self.status.lock() = ChannelStatus::Failed;
            anyhow::bail!("Telegram sendMessage failed: {}", resp.status());
        }
        Ok(())
    }
}

#[async_trait]
impl ChannelAdapter for TelegramBotAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Telegram
    }

    async fn start(&mut self) -> Result<()> {
        let url = self.api_url("getMe");
        let resp = self.client.get(&url).send().await?;
        if resp.status().is_success() {
            *self.status.lock() = ChannelStatus::Online;
            info!(target: "nine_snake.telegram", "Telegram bot started");
            Ok(())
        } else {
            *self.status.lock() = ChannelStatus::Failed;
            anyhow::bail!("Telegram getMe failed: {}", resp.status())
        }
    }

    async fn stop(&mut self) -> Result<()> {
        *self.status.lock() = ChannelStatus::Offline;
        Ok(())
    }

    async fn send(&self, message: &str, reply_to: Option<&str>) -> Result<()> {
        let chat_id: i64 = message.parse().unwrap_or(0);
        let reply_id = reply_to.and_then(|r| r.parse::<i64>().ok());
        self.send_message(chat_id, message, reply_id).await
    }

    fn status(&self) -> ChannelStatus {
        self.status.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_kind_is_telegram() {
        let adapter = TelegramBotAdapter::new("test-token");
        assert_eq!(adapter.kind(), ChannelKind::Telegram);
    }
}
