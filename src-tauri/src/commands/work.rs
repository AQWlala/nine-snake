//! Work-mode commands — tasks, timers, meeting summary.

use serde::{Deserialize, Serialize};
use tauri::State;
use tracing::instrument;

use crate::commands::error::CommandError;
use crate::work::{self as work_ops, TaskStatus, WorkTask};
use crate::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTaskRequest {
    pub title: String,
    pub description: String,
    pub priority: Option<i32>,
    pub due_at: Option<i64>,
}

#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "work_create_task"))]
pub async fn work_create_task(
    state: State<'_, AppState>,
    request: CreateTaskRequest,
) -> Result<WorkTask, CommandError> {
    let engine = state.work.clone();
    tokio::task::spawn_blocking(move || {
        engine
            .create_task(
                request.title,
                request.description,
                request.priority,
                request.due_at,
            )
            .map_err(|e| CommandError::validation("work_create_task").with_details(e.to_string()))
    })
    .await
    .map_err(|e| CommandError::internal("work_create_task", &anyhow::anyhow!("{e}")))?
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "work_get_task"))]
pub async fn work_get_task(
    state: State<'_, AppState>,
    id: String,
) -> Result<Option<WorkTask>, CommandError> {
    let engine = state.work.clone();
    tokio::task::spawn_blocking(move || {
        engine
            .get_task(&id)
            .map_err(|e| CommandError::internal("work_get_task", &e))
    })
    .await
    .map_err(|e| CommandError::internal("work_get_task", &anyhow::anyhow!("{e}")))?
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "work_list_tasks"))]
pub async fn work_list_tasks(
    state: State<'_, AppState>,
    status: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<WorkTask>, CommandError> {
    let engine = state.work.clone();
    let parsed = status
        .map(|s| TaskStatus::from_str(&s))
        .transpose()
        .map_err(|e| CommandError::validation("work_list_tasks").with_details(e.to_string()))?;
    tokio::task::spawn_blocking(move || {
        engine
            .list_tasks(parsed, limit)
            .map_err(|e| CommandError::internal("work_list_tasks", &e))
    })
    .await
    .map_err(|e| CommandError::internal("work_list_tasks", &anyhow::anyhow!("{e}")))?
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "work_set_status"))]
pub async fn work_set_status(
    state: State<'_, AppState>,
    id: String,
    status: String,
) -> Result<WorkTask, CommandError> {
    let parsed = TaskStatus::from_str(&status)
        .map_err(|e| CommandError::validation("work_set_status").with_details(e.to_string()))?;
    let engine = state.work.clone();
    tokio::task::spawn_blocking(move || {
        engine
            .set_status(&id, parsed)
            .map_err(|e| CommandError::internal("work_set_status", &e))
    })
    .await
    .map_err(|e| CommandError::internal("work_set_status", &anyhow::anyhow!("{e}")))?
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateTaskRequest {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub priority: Option<i32>,
    /// `null` clears the due date, `Some(v)` sets it, `None` leaves it.
    pub due_at: Option<Option<i64>>,
}

#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "work_update_task"))]
pub async fn work_update_task(
    state: State<'_, AppState>,
    request: UpdateTaskRequest,
) -> Result<WorkTask, CommandError> {
    let engine = state.work.clone();
    tokio::task::spawn_blocking(move || {
        engine
            .update_task(
                &request.id,
                request.title,
                request.description,
                request.priority,
                request.due_at,
            )
            .map_err(|e| CommandError::internal("work_update_task", &e))
    })
    .await
    .map_err(|e| CommandError::internal("work_update_task", &anyhow::anyhow!("{e}")))?
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "work_delete_task"))]
pub async fn work_delete_task(
    state: State<'_, AppState>,
    id: String,
) -> Result<bool, CommandError> {
    let engine = state.work.clone();
    tokio::task::spawn_blocking(move || {
        engine
            .delete_task(&id)
            .map_err(|e| CommandError::internal("work_delete_task", &e))
    })
    .await
    .map_err(|e| CommandError::internal("work_delete_task", &anyhow::anyhow!("{e}")))?
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriorityRequest {
    pub title: String,
    pub due_at: Option<i64>,
}

#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "work_recommend_priority"))]
pub async fn work_recommend_priority(
    state: State<'_, AppState>,
    request: PriorityRequest,
) -> Result<i32, CommandError> {
    let _ = state;
    Ok(work_ops::recommend_priority(&request.title, request.due_at))
}

#[tauri::command]
#[instrument(skip(state, transcript), fields(otel.kind = "work_summarise_meeting"))]
pub async fn work_summarise_meeting(
    state: State<'_, AppState>,
    transcript: String,
) -> Result<work_ops::MeetingMinutes, CommandError> {
    let _ = state;
    Ok(work_ops::summarise_meeting(&transcript))
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "work_start_timer"))]
pub async fn work_start_timer(
    state: State<'_, AppState>,
    id: String,
) -> Result<WorkTask, CommandError> {
    let engine = state.work.clone();
    tokio::task::spawn_blocking(move || {
        engine
            .start_timer(&id)
            .map_err(|e| CommandError::internal("work_start_timer", &e))
    })
    .await
    .map_err(|e| CommandError::internal("work_start_timer", &anyhow::anyhow!("{e}")))?
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "work_stop_timer"))]
pub async fn work_stop_timer(state: State<'_, AppState>) -> Result<Option<WorkTask>, CommandError> {
    let engine = state.work.clone();
    tokio::task::spawn_blocking(move || {
        engine
            .stop_timer()
            .map_err(|e| CommandError::internal("work_stop_timer", &e))
    })
    .await
    .map_err(|e| CommandError::internal("work_stop_timer", &anyhow::anyhow!("{e}")))?
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "work_add_time"))]
pub async fn work_add_time(
    state: State<'_, AppState>,
    id: String,
    elapsed_ms: i64,
) -> Result<WorkTask, CommandError> {
    let engine = state.work.clone();
    tokio::task::spawn_blocking(move || {
        engine
            .add_time(&id, elapsed_ms)
            .map_err(|e| CommandError::internal("work_add_time", &e))
    })
    .await
    .map_err(|e| CommandError::internal("work_add_time", &anyhow::anyhow!("{e}")))?
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "work_active_timer"))]
pub async fn work_active_timer(state: State<'_, AppState>) -> Result<Option<String>, CommandError> {
    Ok(state.work.active_timer())
}
