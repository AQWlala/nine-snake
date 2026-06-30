//! Channel commands — message bridge status, send, poll, ping.

use tauri::State;
use tracing::instrument;

use crate::commands::error::CommandError;
use crate::AppState;

/// v1.2: Get current status of the message bridge.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "channel_status"))]
pub async fn channel_status(state: State<'_, AppState>) -> Result<serde_json::Value, CommandError> {
    match &state.message_bridge {
        Some(bridge) => Ok(serde_json::json!({
            "connected": bridge.status().connected,
            "endpoint_url": bridge.status().endpoint_url,
            "messages_received": bridge.status().messages_received,
            "messages_sent": bridge.status().messages_sent,
        })),
        None => Ok(serde_json::json!({"connected": false, "channels": []})),
    }
}

/// v1.2: Send a message through the message bridge.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "channel_send"))]
pub async fn channel_send(
    state: State<'_, AppState>,
    target: String,
    text: String,
) -> Result<bool, CommandError> {
    match &state.message_bridge {
        Some(bridge) => {
            let req = crate::channel::types::ChannelSendRequest {
                session_id: target.clone(),
                channel: crate::channel::types::Channel::Web,
                body: text,
                conversation_id: None,
            };
            bridge
                .send(&req)
                .await
                .map(|_| true)
                .map_err(|e| CommandError::internal("channel_send", &anyhow::anyhow!("{e}")))
        }
        None => Err(CommandError::internal(
            "channel_send",
            &anyhow::anyhow!("message bridge not configured"),
        )),
    }
}

/// v1.2: Poll the message bridge for new messages.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "channel_poll"))]
pub async fn channel_poll(
    state: State<'_, AppState>,
) -> Result<Vec<serde_json::Value>, CommandError> {
    match &state.message_bridge {
        Some(bridge) => Ok(bridge
            .poll()
            .await
            .into_iter()
            .map(|m| serde_json::to_value(&m).unwrap_or_default())
            .collect()),
        None => Ok(Vec::new()),
    }
}

/// v1.2: Ping the message bridge.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "channel_ping"))]
pub async fn channel_ping(state: State<'_, AppState>) -> Result<bool, CommandError> {
    match &state.message_bridge {
        Some(bridge) => Ok(bridge.ping().await),
        None => Ok(false),
    }
}
