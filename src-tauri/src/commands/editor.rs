//! Editor and git commands.

use tauri::State;
use tracing::instrument;

use crate::commands::error::CommandError;
use crate::editor::{self as editor_ops};
use crate::AppState;

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "editor_workspace_root"))]
pub async fn editor_workspace_root(state: State<'_, AppState>) -> Result<String, CommandError> {
    Ok(state.editor.workspace_root().to_string_lossy().into_owned())
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "editor_read"))]
pub async fn editor_read(
    state: State<'_, AppState>,
    path: String,
) -> Result<editor_ops::FileContent, CommandError> {
    state
        .editor
        .read_file(&path)
        .map_err(|e| CommandError::validation("editor_read").with_details(e.to_string()))
}

#[tauri::command]
#[instrument(skip(state, content), fields(otel.kind = "editor_write"))]
pub async fn editor_write(
    state: State<'_, AppState>,
    path: String,
    content: String,
) -> Result<editor_ops::FileContent, CommandError> {
    state
        .editor
        .write_file(&path, &content)
        .map_err(|e| CommandError::validation("editor_write").with_details(e.to_string()))
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "editor_list"))]
pub async fn editor_list(
    state: State<'_, AppState>,
    max_depth: Option<usize>,
) -> Result<Vec<editor_ops::FileEntry>, CommandError> {
    state
        .editor
        .list_tree(max_depth)
        .map_err(|e| CommandError::internal("editor_list", &e))
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "git_status"))]
pub async fn git_status(state: State<'_, AppState>) -> Result<editor_ops::GitStatus, CommandError> {
    editor_ops::git_status(state.editor.workspace_root())
        .map_err(|e| CommandError::internal("git_status", &e))
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "git_log"))]
pub async fn git_log(
    state: State<'_, AppState>,
    limit: Option<usize>,
) -> Result<Vec<editor_ops::GitLogEntry>, CommandError> {
    editor_ops::git_log(state.editor.workspace_root(), limit.unwrap_or(20))
        .map_err(|e| CommandError::internal("git_log", &e))
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "git_diff"))]
pub async fn git_diff(
    state: State<'_, AppState>,
    path: Option<String>,
) -> Result<editor_ops::GitDiff, CommandError> {
    let p = path.unwrap_or_default();
    editor_ops::git_diff(state.editor.workspace_root(), &p)
        .map_err(|e| CommandError::internal("git_diff", &e))
}

#[tauri::command]
#[instrument(skip(state, message), fields(otel.kind = "git_commit"))]
pub async fn git_commit(
    state: State<'_, AppState>,
    message: String,
) -> Result<String, CommandError> {
    editor_ops::git_commit(state.editor.workspace_root(), &message)
        .map_err(|e| CommandError::validation("git_commit").with_details(e.to_string()))
}
