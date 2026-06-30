//! Message bridge core — v1.2
//!
//! Communicates with JiuwenSwarm's agent delivery fabric for multi-channel
//! messaging.  When `NINE_SNAKE_BRIDGE_URL` is set, the bridge acts as a
//! JiuwenSwarm agent: it receives user messages routed from any channel
//! (WeChat / Feishu / Telegram / Web) and can push responses back.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use super::types::{BridgeStatus, Channel, ChannelMessage, ChannelSendRequest};

/// Default interval (seconds) between polls for new messages.
#[allow(dead_code)]
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;

/// JiuwenSwarm agent-compatible message envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JiuwenSwarmMessage {
    source: String,
    channel: String,
    content: String,
    session_id: String,
    sender: Option<String>,
    conversation_id: Option<String>,
    timestamp_ms: i64,
}

/// JiuwenSwarm agent turn response.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JiuwenSwarmResponse {
    session_id: String,
    channel: String,
    body: String,
    conversation_id: Option<String>,
}

/// JiuwenSwarm ping response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
struct JiuwenSwarmPingResponse {
    status: String,
    agent_id: Option<String>,
}

// ---------------------------------------------------------------------------
// MessageBridge
// ---------------------------------------------------------------------------

pub struct MessageBridge {
    /// JiuwenSwarm endpoint URL (e.g. `http://127.0.0.1:8080/agent/`).
    endpoint: String,
    /// Reusable HTTP client.
    client: Client,
    /// Incoming message buffer (handled by the caller).
    #[allow(dead_code)]
    inbox: Arc<Mutex<Vec<ChannelMessage>>>,
    /// Messages received counter.
    received: AtomicU64,
    /// Messages sent counter.
    sent: AtomicU64,
    /// Last error message.
    last_error: Arc<Mutex<Option<String>>>,
}

impl MessageBridge {
    /// Creates a new bridge connected to the given JiuwenSwarm endpoint.
    ///
    /// Returns `None` when the endpoint is empty (bridge disabled).
    pub fn new(endpoint_url: &str) -> Option<Self> {
        if endpoint_url.is_empty() {
            info!(target: "nine_snake.channel", "bridge disabled (no endpoint configured)");
            return None;
        }

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest Client::build is infallible");

        let bridge = Self {
            endpoint: endpoint_url.trim_end_matches('/').to_string(),
            client,
            inbox: Arc::new(Mutex::new(Vec::new())),
            received: AtomicU64::new(0),
            sent: AtomicU64::new(0),
            last_error: Arc::new(Mutex::new(None)),
        };

        info!(
            target: "nine_snake.channel",
            endpoint = %endpoint_url,
            "message bridge initialised"
        );

        Some(bridge)
    }

    /// Pings the JiuwenSwarm endpoint to verify connectivity.
    pub async fn ping(&self) -> bool {
        let url = format!("{}/ping", self.endpoint);
        match self.client.get(&url).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    info!(target: "nine_snake.channel", "bridge ping succeeded");
                    true
                } else {
                    warn!(target: "nine_snake.channel", status = %resp.status(), "bridge ping failed");
                    false
                }
            }
            Err(e) => {
                let msg = format!("ping error: {e}");
                self.set_error(&msg);
                false
            }
        }
    }

    /// Polls JiuwenSwarm for new messages addressed to this agent.
    ///
    /// Returns immediately; the caller should invoke this periodically
    /// (e.g. from a Tokio interval or on user demand).
    pub async fn poll(&self) -> Vec<ChannelMessage> {
        let url = format!("{}/messages", self.endpoint);
        match self.client.get(&url).send().await {
            Ok(resp) => match resp.json::<Vec<JiuwenSwarmMessage>>().await {
                Ok(msgs) => {
                    let count = msgs.len();
                    let converted: Vec<ChannelMessage> = msgs
                        .into_iter()
                        .map(|m| ChannelMessage {
                            session_id: m.session_id,
                            channel: parse_channel(&m.channel),
                            sender: m.sender.unwrap_or_default(),
                            body: m.content,
                            conversation_id: m.conversation_id,
                            timestamp_ms: m.timestamp_ms,
                        })
                        .collect();
                    self.received.fetch_add(count as u64, Ordering::Relaxed);
                    if count > 0 {
                        info!(target: "nine_snake.channel", count, "received messages");
                    }
                    converted
                }
                Err(e) => {
                    let msg = format!("failed to parse messages: {e}");
                    self.set_error(&msg);
                    Vec::new()
                }
            },
            Err(e) => {
                let msg = format!("poll error: {e}");
                self.set_error(&msg);
                Vec::new()
            }
        }
    }

    /// Sends a message back through JiuwenSwarm to the target channel.
    ///
    /// JiuwenSwarm handles the actual channel delivery (WeChat / Feishu /
    /// Telegram / Web / etc.).
    pub async fn send(&self, req: &ChannelSendRequest) -> Result<(), String> {
        let url = format!("{}/respond", self.endpoint);

        let payload = JiuwenSwarmResponse {
            session_id: req.session_id.clone(),
            channel: req.channel.as_str().to_string(),
            body: req.body.clone(),
            conversation_id: req.conversation_id.clone(),
        };

        match self.client.post(&url).json(&payload).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    self.sent.fetch_add(1, Ordering::Relaxed);
                    info!(
                        target: "nine_snake.channel",
                        channel = %req.channel.as_str(),
                        session = %req.session_id,
                        "message sent"
                    );
                    Ok(())
                } else {
                    let msg = format!("send failed: HTTP {}", resp.status());
                    self.set_error(&msg);
                    Err(msg)
                }
            }
            Err(e) => {
                let msg = format!("send error: {e}");
                self.set_error(&msg);
                Err(msg)
            }
        }
    }

    // ------------------------------------------------------------------
    // Status query
    // ------------------------------------------------------------------

    /// Returns a snapshot of the bridge status.
    pub fn status(&self) -> BridgeStatus {
        BridgeStatus {
            connected: true,
            endpoint_url: Some(self.endpoint.clone()),
            messages_received: self.received.load(Ordering::Relaxed),
            messages_sent: self.sent.load(Ordering::Relaxed),
            last_error: self.last_error.lock().clone(),
        }
    }

    /// Clears the last error.
    pub fn clear_error(&self) {
        *self.last_error.lock() = None;
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    fn set_error(&self, msg: &str) {
        error!(target: "nine_snake.channel", "{}", msg);
        *self.last_error.lock() = Some(msg.to_string());
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_channel(raw: &str) -> Channel {
    match raw.to_lowercase().as_str() {
        "web" => Channel::Web,
        "feishu" => Channel::Feishu,
        "telegram" => Channel::Telegram,
        "wechat" => Channel::Wechat,
        "dingtalk" => Channel::Dingtalk,
        "wecom" => Channel::Wecom,
        "desktop" => Channel::Desktop,
        other => {
            warn!(target: "nine_snake.channel", channel = other, "unknown channel, falling back to web");
            Channel::Web
        }
    }
}

impl Default for BridgeStatus {
    fn default() -> Self {
        Self {
            connected: false,
            endpoint_url: None,
            messages_received: 0,
            messages_sent: 0,
            last_error: None,
        }
    }
}
