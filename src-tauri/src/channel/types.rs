//! Channel data types — v1.2 → v2.0

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Supported delivery channels (mirrors JiuwenSwarm's channel set).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Web,
    Feishu,
    Telegram,
    Wechat,
    Dingtalk,
    Wecom,
    Desktop,
    Discord,
}

impl Channel {
    pub fn as_str(&self) -> &str {
        match self {
            Channel::Web => "web",
            Channel::Feishu => "feishu",
            Channel::Telegram => "telegram",
            Channel::Wechat => "wechat",
            Channel::Dingtalk => "dingtalk",
            Channel::Wecom => "wecom",
            Channel::Desktop => "desktop",
            Channel::Discord => "discord",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    JiuwenSwarm,
    Telegram,
    Discord,
    WebChat,
}

impl ChannelKind {
    pub fn as_str(&self) -> &str {
        match self {
            ChannelKind::JiuwenSwarm => "jiuwenswarm",
            ChannelKind::Telegram => "telegram",
            ChannelKind::Discord => "discord",
            ChannelKind::WebChat => "webchat",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub channel_type: ChannelKind,
    pub enabled: bool,
    pub token_key_id: Option<String>,
    pub poll_interval_secs: u64,
    pub max_retries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChannelStatus {
    Online,
    Offline,
    RateLimited,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMessageV2 {
    pub channel: ChannelKind,
    pub sender_id: String,
    pub content: String,
    pub timestamp: i64,
    pub reply_to: Option<String>,
}

#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    fn kind(&self) -> ChannelKind;
    async fn start(&mut self) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
    async fn send(&self, message: &str, reply_to: Option<&str>) -> Result<()>;
    fn status(&self) -> ChannelStatus;
}

/// An incoming message from any channel, routed through JiuwenSwarm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMessage {
    /// JiuwenSwarm session identifier.
    pub session_id: String,
    /// Originating channel.
    pub channel: Channel,
    /// Sender identity on that channel (e.g. WeChat openid).
    pub sender: String,
    /// Message body text.
    pub body: String,
    /// Optional conversation/group identifier.
    pub conversation_id: Option<String>,
    /// Unix millis timestamp.
    pub timestamp_ms: i64,
}

/// An outgoing message destined for a specific channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSendRequest {
    /// Target session (must match a known JiuwenSwarm session).
    pub session_id: String,
    /// Target channel.
    pub channel: Channel,
    /// Response body text.
    pub body: String,
    /// Optional conversation/group identifier.
    pub conversation_id: Option<String>,
}

/// Status of the message bridge connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeStatus {
    /// Whether the bridge is configured and connected.
    pub connected: bool,
    /// Configured JiuwenSwarm endpoint URL.
    pub endpoint_url: Option<String>,
    /// Number of messages received in this session.
    pub messages_received: u64,
    /// Number of messages sent in this session.
    pub messages_sent: u64,
    /// Last error message (if any).
    pub last_error: Option<String>,
}
