use std::collections::HashMap;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use tracing::{info, warn};

use super::types::{ChannelAdapter, ChannelKind, ChannelStatus};

pub struct ChannelRouter {
    adapters: Mutex<HashMap<ChannelKind, Box<dyn ChannelAdapter>>>,
}

impl ChannelRouter {
    pub fn new() -> Self {
        Self {
            adapters: Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&self, adapter: Box<dyn ChannelAdapter>) {
        let kind = adapter.kind();
        info!(target: "nine_snake.channel", kind = %kind.as_str(), "registered channel adapter");
        self.adapters.lock().insert(kind, adapter);
    }

    pub fn unregister(&self, kind: &ChannelKind) {
        self.adapters.lock().remove(kind);
    }

    pub async fn start_all(&self) -> Result<()> {
        let mut adapters = self.adapters.lock();
        for (kind, adapter) in adapters.iter_mut() {
            if let Err(e) = adapter.start().await {
                warn!(target: "nine_snake.channel", kind = %kind.as_str(), error = %e, "failed to start adapter");
            } else {
                info!(target: "nine_snake.channel", kind = %kind.as_str(), "adapter started");
            }
        }
        Ok(())
    }

    pub async fn stop_all(&self) -> Result<()> {
        let mut adapters = self.adapters.lock();
        for (kind, adapter) in adapters.iter_mut() {
            if let Err(e) = adapter.stop().await {
                warn!(target: "nine_snake.channel", kind = %kind.as_str(), error = %e, "failed to stop adapter");
            }
        }
        Ok(())
    }

    pub async fn send(
        &self,
        kind: &ChannelKind,
        message: &str,
        reply_to: Option<&str>,
    ) -> Result<()> {
        let adapters = self.adapters.lock();
        let adapter = adapters
            .get(kind)
            .ok_or_else(|| anyhow!("no adapter registered for {:?}", kind))?;
        adapter.send(message, reply_to).await
    }

    pub fn status(&self, kind: &ChannelKind) -> ChannelStatus {
        let adapters = self.adapters.lock();
        adapters
            .get(kind)
            .map(|a| a.status())
            .unwrap_or(ChannelStatus::Offline)
    }

    pub fn list_channels(&self) -> Vec<(ChannelKind, ChannelStatus)> {
        let adapters = self.adapters.lock();
        adapters
            .iter()
            .map(|(k, a)| (k.clone(), a.status()))
            .collect()
    }
}

impl Default for ChannelRouter {
    fn default() -> Self {
        Self::new()
    }
}

pub struct WebChatAdapter {
    status: ChannelStatus,
}

impl Default for WebChatAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl WebChatAdapter {
    pub fn new() -> Self {
        Self {
            status: ChannelStatus::Offline,
        }
    }
}

#[async_trait::async_trait]
impl ChannelAdapter for WebChatAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::WebChat
    }

    async fn start(&mut self) -> Result<()> {
        self.status = ChannelStatus::Online;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.status = ChannelStatus::Offline;
        Ok(())
    }

    async fn send(&self, _message: &str, _reply_to: Option<&str>) -> Result<()> {
        Ok(())
    }

    fn status(&self) -> ChannelStatus {
        self.status.clone()
    }
}

pub struct TelegramAdapter {
    status: ChannelStatus,
    bot_token: String,
}

impl TelegramAdapter {
    pub fn new(bot_token: &str) -> Self {
        Self {
            status: ChannelStatus::Offline,
            bot_token: bot_token.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl ChannelAdapter for TelegramAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Telegram
    }

    async fn start(&mut self) -> Result<()> {
        if self.bot_token.is_empty() {
            anyhow::bail!("Telegram bot token is required");
        }
        self.status = ChannelStatus::Online;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.status = ChannelStatus::Offline;
        Ok(())
    }

    async fn send(&self, message: &str, reply_to: Option<&str>) -> Result<()> {
        let _ = (message, reply_to);
        Ok(())
    }

    fn status(&self) -> ChannelStatus {
        self.status.clone()
    }
}

pub struct DiscordAdapter {
    status: ChannelStatus,
    webhook_url: String,
}

impl DiscordAdapter {
    pub fn new(webhook_url: &str) -> Self {
        Self {
            status: ChannelStatus::Offline,
            webhook_url: webhook_url.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl ChannelAdapter for DiscordAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Discord
    }

    async fn start(&mut self) -> Result<()> {
        if self.webhook_url.is_empty() {
            anyhow::bail!("Discord webhook URL is required");
        }
        self.status = ChannelStatus::Online;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.status = ChannelStatus::Offline;
        Ok(())
    }

    async fn send(&self, message: &str, _reply_to: Option<&str>) -> Result<()> {
        let _ = message;
        Ok(())
    }

    fn status(&self) -> ChannelStatus {
        self.status.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_starts_empty() {
        let router = ChannelRouter::new();
        assert!(router.list_channels().is_empty());
    }

    #[test]
    fn register_and_list() {
        let router = ChannelRouter::new();
        router.register(Box::new(WebChatAdapter::new()));
        let channels = router.list_channels();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].0, ChannelKind::WebChat);
    }

    #[test]
    fn status_offline_for_unregistered() {
        let router = ChannelRouter::new();
        assert_eq!(
            router.status(&ChannelKind::Telegram),
            ChannelStatus::Offline
        );
    }
}
