//! LLM commands — complete, chat, embed.

use serde::{Deserialize, Serialize};
use tauri::State;
use tracing::instrument;

use crate::api::server::NineSnakeService;
use crate::commands::error::CommandError;
use crate::llm::ChatMessage;
use crate::AppState;

/// Tauri command: raw LLM completion.
#[tauri::command]
#[instrument(skip(state, prompt), fields(otel.kind = "llm_complete"))]
pub async fn llm_complete(
    state: State<'_, AppState>,
    prompt: String,
    model: Option<String>,
) -> Result<String, CommandError> {
    let _ = model; // currently unused; reserved for v0.5 routing
    state
        .llm_complete(prompt)
        .await
        .map_err(|e| CommandError::llm("llm_complete", &e))
}

/// v0.3: multi-message LLM chat.
#[tauri::command]
#[instrument(skip(state, messages), fields(otel.kind = "llm_chat"))]
pub async fn llm_chat(
    state: State<'_, AppState>,
    messages: Vec<(String, String)>,
    model: Option<String>,
) -> Result<LlmChatDto, CommandError> {
    let model_ref = model.as_deref().unwrap_or("");
    let msgs: Vec<ChatMessage> = messages
        .into_iter()
        .map(|(role, content)| ChatMessage { role, content })
        .collect();
    let resp = if model_ref.is_empty() {
        state.llm.chat(msgs).await
    } else {
        state.llm.chat_with_model(model_ref, msgs).await
    }
    .map_err(|e| CommandError::llm("llm_chat", &e))?;
    Ok(LlmChatDto {
        role: resp.message.role,
        content: resp.message.content,
        model: resp.model,
        eval_count: resp.eval_count.unwrap_or(0) as i64,
        total_duration_ns: resp.total_duration.unwrap_or(0) as i64,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmChatDto {
    pub role: String,
    pub content: String,
    pub model: String,
    pub eval_count: i64,
    pub total_duration_ns: i64,
}

/// v0.3: embed a single text.
#[tauri::command]
#[instrument(skip(state, text), fields(otel.kind = "llm_embed"))]
pub async fn llm_embed(state: State<'_, AppState>, text: String) -> Result<Vec<f32>, CommandError> {
    state
        .embedder
        .embed(&text)
        .await
        .map_err(|e| CommandError::llm("llm_embed", &e))
}
