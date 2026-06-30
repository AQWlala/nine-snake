//! Chat commands — `chat` and `chat_stream`.

use serde::{Deserialize, Serialize};
use tauri::State;
use tracing::{info, instrument, warn};

use crate::api::server::{ChatRequestDto, NineSnakeService, StoreMemoryRequest};
use crate::commands::error::CommandError;
use crate::llm::ChatMessage;
use crate::memory::types::{MemoryLayer, MemoryType, SourceKind};
use crate::AppState;

/// Tauri command: send a chat message, return the assistant reply, and
/// persist both sides to memory (L1).
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "chat"))]
pub async fn chat(
    state: State<'_, AppState>,
    request: ChatRequestDto,
) -> Result<ChatResponseDto, CommandError> {
    // v1.1: Prompt injection scan before processing.
    let scan = crate::security::injection_guard::full_injection_scan(&request.user_message);
    if let Some(severity) = scan.max_severity {
        if severity >= crate::security::injection_guard::InjectionSeverity::Critical {
            tracing::warn!(
                target: "nine_snake.cmd",
                hits = scan.injection_hits.len(),
                leaks = scan.credential_leaks.len(),
                "blocked critical injection / credential leak in chat"
            );
            return Err(CommandError::validation("chat").with_details(
                "输入包含潜在的安全风险（注入攻击或凭证泄露），已被拦截".to_string(),
            ));
        }
        if !scan.safe {
            tracing::warn!(
                target: "nine_snake.cmd",
                severity = %severity,
                "non-critical injection warning in chat"
            );
        }
    }

    let resp = state
        .chat(request.clone())
        .await
        .map_err(|e| CommandError::llm("chat", &e))?;
    crate::metrics::global().record_chat();
    info!(target: "nine_snake.cmd", model = %resp.model, "chat ok");

    let state_for_memory = state.inner().clone();
    let user_msg = request.user_message.clone();
    let asst_msg = resp.message.content.clone();
    tokio::spawn(async move {
        if let Err(e) = absorb_chat_turn(&state_for_memory, &user_msg, &asst_msg).await {
            warn!(target: "nine_snake.cmd", error = ?e, "failed to absorb chat turn into memory");
        }
    });

    Ok(ChatResponseDto {
        model: resp.model,
        content: resp.message.content,
        role: resp.message.role,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponseDto {
    pub model: String,
    pub role: String,
    pub content: String,
}

/// Tauri command: streaming chat (collects all tokens).
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "chat_stream"))]
pub async fn chat_stream(
    state: State<'_, AppState>,
    request: ChatRequestDto,
) -> Result<Vec<crate::llm::StreamToken>, CommandError> {
    let scan = crate::security::injection_guard::full_injection_scan(&request.user_message);
    if let Some(severity) = scan.max_severity {
        if severity >= crate::security::injection_guard::InjectionSeverity::Critical {
            return Err(CommandError::validation("chat_stream").with_details(
                "输入包含潜在的安全风险（注入攻击或凭证泄露），已被拦截".to_string(),
            ));
        }
    }

    let mut msgs: Vec<ChatMessage> = Vec::new();
    if let Some(sys) = request.system.as_deref() {
        msgs.push(ChatMessage::system(sys));
    }
    msgs.push(ChatMessage::user(request.user_message));

    let stream = state.llm.chat_stream(msgs);
    use futures::StreamExt;
    let tokens: Vec<crate::llm::StreamToken> =
        stream.filter_map(|r| async move { r.ok() }).collect().await;
    Ok(tokens)
}

/// Persist a chat turn (user prompt + assistant reply) as a pair of
/// L1 Episodic memories. Best-effort; errors are surfaced to the
/// caller so the spawn-and-forget site can log them.
async fn absorb_chat_turn(state: &AppState, user_msg: &str, asst_msg: &str) -> anyhow::Result<()> {
    if !user_msg.trim().is_empty() {
        let req = StoreMemoryRequest {
            content: user_msg.to_string(),
            memory_type: MemoryType::Episodic,
            layer: MemoryLayer::L1,
            source: SourceKind::UserInput,
            metadata: Some(serde_json::json!({ "channel": "chat.user" })),
        };
        state.memory_store(req).await?;
    }
    if !asst_msg.trim().is_empty() {
        let req = StoreMemoryRequest {
            content: asst_msg.to_string(),
            memory_type: MemoryType::Episodic,
            layer: MemoryLayer::L1,
            source: SourceKind::AgentOutput,
            metadata: Some(serde_json::json!({ "channel": "chat.assistant" })),
        };
        state.memory_store(req).await?;
    }
    Ok(())
}
