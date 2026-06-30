use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::warn;

const BUS_CAPACITY: usize = 256;
const BROADCAST_CAPACITY: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMessage {
    pub from: String,
    pub to: Option<String>,
    pub content: String,
    pub timestamp: i64,
    pub msg_type: BusMessageType,
    pub correlation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BusMessageType {
    Request,
    Response,
    Notification,
    Capability,
}

type PendingReply = tokio::sync::Mutex<HashMap<String, oneshot::Sender<BusMessage>>>;

pub struct AgentBus {
    mailboxes: Arc<tokio::sync::Mutex<HashMap<String, mpsc::Sender<BusMessage>>>>,
    broadcast_tx: broadcast::Sender<BusMessage>,
    pending_replies: Arc<PendingReply>,
}

impl AgentBus {
    pub fn new() -> Self {
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            mailboxes: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            broadcast_tx,
            pending_replies: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    pub async fn register(&self, agent_id: &str) -> mpsc::Receiver<BusMessage> {
        let (tx, rx) = mpsc::channel(BUS_CAPACITY);
        self.mailboxes.lock().await.insert(agent_id.to_string(), tx);
        rx
    }

    pub async fn unregister(&self, agent_id: &str) {
        self.mailboxes.lock().await.remove(agent_id);
    }

    pub async fn send(&self, message: BusMessage) -> Result<()> {
        let target = message
            .to
            .as_deref()
            .ok_or_else(|| anyhow!("message has no target"))?;
        let mailboxes = self.mailboxes.lock().await;
        let sender = mailboxes
            .get(target)
            .ok_or_else(|| anyhow!("agent '{target}' not found or not registered"))?;
        sender
            .send(message)
            .await
            .map_err(|e| anyhow!("send failed: {e}"))
    }

    /// Send a request to a specific agent and wait for a response.
    /// This enables P2P request-response communication between agents.
    pub async fn request(
        &self,
        from: &str,
        to: &str,
        content: String,
        timeout: std::time::Duration,
    ) -> Result<BusMessage> {
        let correlation_id = uuid::Uuid::new_v4().to_string();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.pending_replies
            .lock()
            .await
            .insert(correlation_id.clone(), reply_tx);

        let msg = BusMessage {
            from: from.to_string(),
            to: Some(to.to_string()),
            content,
            timestamp: chrono::Utc::now().timestamp_millis(),
            msg_type: BusMessageType::Request,
            correlation_id: Some(correlation_id.clone()),
        };
        self.send(msg).await?;

        let result = tokio::time::timeout(timeout, reply_rx).await;
        self.pending_replies.lock().await.remove(&correlation_id);
        match result {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(anyhow!("reply channel closed for request to '{to}'")),
            Err(_) => Err(anyhow!("request to '{to}' timed out after {:?}", timeout)),
        }
    }

    /// Reply to a request using the correlation_id from the original message.
    pub async fn reply(&self, original: &BusMessage, content: String) -> Result<()> {
        let correlation_id = original
            .correlation_id
            .as_deref()
            .ok_or_else(|| anyhow!("cannot reply to a message without correlation_id"))?;

        let mut pending = self.pending_replies.lock().await;
        if let Some(reply_tx) = pending.remove(correlation_id) {
            let response = BusMessage {
                from: original.to.clone().unwrap_or_default(),
                to: Some(original.from.clone()),
                content,
                timestamp: chrono::Utc::now().timestamp_millis(),
                msg_type: BusMessageType::Response,
                correlation_id: Some(correlation_id.to_string()),
            };
            let _ = reply_tx.send(response);
            Ok(())
        } else {
            Err(anyhow!(
                "no pending reply for correlation_id '{correlation_id}'"
            ))
        }
    }

    pub fn broadcast(&self, message: BusMessage) {
        if self.broadcast_tx.send(message).is_err() {
            warn!(target: "nine_snake.bus", "no active broadcast receivers");
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BusMessage> {
        self.broadcast_tx.subscribe()
    }
}

impl Default for AgentBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_and_send() {
        let bus = AgentBus::new();
        let mut rx = bus.register("agent-1").await;
        let msg = BusMessage {
            from: "agent-2".to_string(),
            to: Some("agent-1".to_string()),
            content: "hello".to_string(),
            timestamp: 0,
            msg_type: BusMessageType::Request,
            correlation_id: None,
        };
        bus.send(msg).await.unwrap();
        let received = rx.recv().await.unwrap();
        assert_eq!(received.content, "hello");
    }

    #[tokio::test]
    async fn send_to_unknown_fails() {
        let bus = AgentBus::new();
        let msg = BusMessage {
            from: "agent-1".to_string(),
            to: Some("unknown".to_string()),
            content: "hello".to_string(),
            timestamp: 0,
            msg_type: BusMessageType::Request,
            correlation_id: None,
        };
        assert!(bus.send(msg).await.is_err());
    }

    #[tokio::test]
    async fn broadcast_works() {
        let bus = AgentBus::new();
        let mut sub1 = bus.subscribe();
        let mut sub2 = bus.subscribe();
        let msg = BusMessage {
            from: "agent-1".to_string(),
            to: None,
            content: "broadcast!".to_string(),
            timestamp: 0,
            msg_type: BusMessageType::Notification,
            correlation_id: None,
        };
        bus.broadcast(msg);
        assert_eq!(sub1.recv().await.unwrap().content, "broadcast!");
        assert_eq!(sub2.recv().await.unwrap().content, "broadcast!");
    }

    #[tokio::test]
    async fn request_reply_p2p() {
        let bus = AgentBus::new();
        let mut rx = bus.register("responder").await;

        let bus_clone = Arc::new(bus);
        let bus_for_task = bus_clone.clone();
        let handle = tokio::spawn(async move {
            let msg = rx.recv().await.unwrap();
            assert_eq!(msg.msg_type, BusMessageType::Request);
            bus_for_task.reply(&msg, "pong".to_string()).await.unwrap();
        });

        let response = bus_clone
            .request(
                "caller",
                "responder",
                "ping".to_string(),
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(response.content, "pong");
        handle.await.unwrap();
    }
}
