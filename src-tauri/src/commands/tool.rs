//! Tool registry commands — list, invoke.

use serde::{Deserialize, Serialize};
use tauri::State;
use tracing::instrument;

use crate::commands::error::CommandError;
use crate::tools::{ToolInput, ToolOutput};
use crate::AppState;

/// Tool descriptor for the front-end.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

/// Tauri command: list all registered tools.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "tool_list"))]
pub async fn tool_list(state: State<'_, AppState>) -> Result<Vec<ToolDescriptor>, CommandError> {
    let tools = state.tool_registry.list_all();
    Ok(tools
        .into_iter()
        .map(|(name, description, schema)| ToolDescriptor {
            name,
            description,
            schema,
        })
        .collect())
}

/// Tauri command: invoke a registered tool by name with arguments.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "tool_invoke"))]
pub async fn tool_invoke(
    state: State<'_, AppState>,
    tool_name: String,
    arguments: serde_json::Value,
) -> Result<ToolOutput, CommandError> {
    let input = ToolInput {
        tool_name,
        arguments,
    };
    state
        .tool_registry
        .invoke(input)
        .map_err(|e| CommandError::internal("tool_invoke", &e))
}
