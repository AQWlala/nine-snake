use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::types::{ChannelAdapter, ChannelKind, ChannelStatus};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiscordWebhookPayload {
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
}

pub struct DiscordBotAdapter {
    webhook_url: String,
    client: Client,
    status: Arc<Mutex<ChannelStatus>>,
}

impl DiscordBotAdapter {
    pub fn new(webhook_url: &str) -> Self {
        Self {
            webhook_url: webhook_url.to_string(),
            client: Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest Client::build is infallible"),
            status: Arc::new(Mutex::new(ChannelStatus::Offline)),
        }
    }

    pub async fn send_webhook(&self, content: &str, username: Option<&str>) -> Result<()> {
        let payload = DiscordWebhookPayload {
            content: content.to_string(),
            username: username.map(|s| s.to_string()),
        };
        let resp = self
            .client
            .post(&self.webhook_url)
            .json(&payload)
            .send()
            .await?;
        if resp.status().as_u16() == 429 {
            *self.status.lock() = ChannelStatus::RateLimited;
            warn!(target: "nine_snake.discord", "rate limited by Discord API");
        } else if !resp.status().is_success() {
            *self.status.lock() = ChannelStatus::Failed;
            anyhow::bail!("Discord webhook failed: {}", resp.status());
        }
        Ok(())
    }
}

#[async_trait]
impl ChannelAdapter for DiscordBotAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Discord
    }

    async fn start(&mut self) -> Result<()> {
        let resp = self.client.get(&self.webhook_url).send().await;
        match resp {
            Ok(r) if r.status().is_success() => {
                *self.status.lock() = ChannelStatus::Online;
                info!(target: "nine_snake.discord", "Discord webhook adapter started");
                Ok(())
            }
            Ok(r) => {
                *self.status.lock() = ChannelStatus::Failed;
                anyhow::bail!("Discord webhook validation failed: {}", r.status())
            }
            Err(e) => {
                *self.status.lock() = ChannelStatus::Failed;
                anyhow::bail!("Discord webhook connection failed: {e}")
            }
        }
    }

    async fn stop(&mut self) -> Result<()> {
        *self.status.lock() = ChannelStatus::Offline;
        Ok(())
    }

    async fn send(&self, message: &str, _reply_to: Option<&str>) -> Result<()> {
        self.send_webhook(message, None).await
    }

    fn status(&self) -> ChannelStatus {
        self.status.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_kind_is_discord() {
        let adapter = DiscordBotAdapter::new("https://discord.com/api/webhooks/test");
        assert_eq!(adapter.kind(), ChannelKind::Discord);
    }
}
