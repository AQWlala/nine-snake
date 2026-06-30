//! WebChat share link commands.

use tauri::State;
use tracing::instrument;

use crate::commands::error::CommandError;
use crate::AppState;

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "share_chat"))]
pub async fn share_chat(state: State<'_, AppState>) -> Result<String, CommandError> {
    Ok(state.webchat_service.create_session())
}
