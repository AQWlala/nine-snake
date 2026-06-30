//! OS commands — clipboard, shell, notify.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::State;
use tracing::instrument;

use crate::commands::error::CommandError;
use crate::os::{self, Notification, NotificationLevel};
use crate::AppState;

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "os_clipboard_read"))]
pub async fn os_clipboard_read(state: State<'_, AppState>) -> Result<String, CommandError> {
    state
        .clipboard
        .read_text()
        .map_err(|e| CommandError::internal("os_clipboard_read", &e))
}

#[tauri::command]
#[instrument(skip(state, text), fields(otel.kind = "os_clipboard_write"))]
pub async fn os_clipboard_write(
    state: State<'_, AppState>,
    text: String,
) -> Result<(), CommandError> {
    state
        .clipboard
        .write_text(&text)
        .map_err(|e| CommandError::internal("os_clipboard_write", &e))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellExecRequest {
    /// Either a parsed argv array or a single string to be split
    /// via `shell-words`.  Callers SHOULD prefer the array form.
    pub argv: Option<Vec<String>>,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "os_shell_exec"))]
pub async fn os_shell_exec(
    state: State<'_, AppState>,
    request: ShellExecRequest,
) -> Result<os::ShellOutput, CommandError> {
    let argv: Vec<String> = if let Some(arr) = request.argv {
        arr
    } else if let Some(cmd) = request.command {
        os::parse_argv(&cmd)
            .map_err(|e| CommandError::validation("os_shell_exec").with_details(e.to_string()))?
    } else {
        return Err(CommandError::validation("os_shell_exec")
            .with_details("argv or command is required".to_string()));
    };
    let cwd: Option<PathBuf> = request.cwd.map(PathBuf::from);
    let shell = state.shell.clone();
    let timeout = request.timeout_ms.map(std::time::Duration::from_millis);
    // v1.0.1 P0#3: `ShellExecutor::exec` is now `async` so the
    // timeout branch can `start_kill()` the child.  No more
    // `spawn_blocking`.
    let exec = if let Some(t) = timeout {
        (*shell).clone().with_timeout(t)
    } else {
        (*shell).clone()
    };
    exec.exec(argv, cwd.as_deref())
        .await
        .map_err(|e| CommandError::validation("os_shell_exec").with_details(e.to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyRequest {
    pub title: String,
    pub body: String,
    pub level: Option<String>,
}

#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "os_notify"))]
pub async fn os_notify(
    state: State<'_, AppState>,
    request: NotifyRequest,
) -> Result<(), CommandError> {
    let _ = state;
    let level = match request.level.as_deref() {
        Some("success") => NotificationLevel::Success,
        Some("warning") => NotificationLevel::Warning,
        Some("error") => NotificationLevel::Error,
        _ => NotificationLevel::Info,
    };
    let n = Notification {
        title: request.title,
        body: request.body,
        level,
    };
    os::send_notification(&n).map_err(|e| CommandError::internal("os_notify", &e))
}
