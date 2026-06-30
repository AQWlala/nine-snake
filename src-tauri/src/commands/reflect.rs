//! Reflect commands — trigger, list, get.

use tauri::State;
use tracing::instrument;

use crate::commands::error::CommandError;
use crate::memory::reflect::Reflection;
use crate::AppState;

/// v0.2: Tauri command — trigger a single reflection pass manually.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "reflect_now"))]
pub async fn reflect_now(state: State<'_, AppState>) -> Result<Vec<Reflection>, CommandError> {
    let engine = state.reflection.clone();
    engine
        .reflect_now()
        .await
        .map_err(|e| CommandError::memory("reflect_now", &e))
}

/// v0.2: Tauri command — list the most recent reflections.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "list_reflections"))]
pub async fn list_reflections(
    state: State<'_, AppState>,
    limit: Option<usize>,
) -> Result<Vec<Reflection>, CommandError> {
    let engine = state.reflection.clone();
    let lim = limit.unwrap_or(20);
    tokio::task::spawn_blocking(move || engine.list_recent(lim))
        .await
        .map_err(|e| CommandError::internal("list_reflections", &anyhow::anyhow!("{e}")))?
        .map_err(|e| CommandError::memory("list_reflections", &e))
}

/// v0.3: fetch a reflection by id.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "get_reflection"))]
pub async fn get_reflection(
    state: State<'_, AppState>,
    id: String,
) -> Result<Option<Reflection>, CommandError> {
    let engine = state.reflection.clone();
    tokio::task::spawn_blocking(move || engine.get(&id))
        .await
        .map_err(|e| CommandError::internal("get_reflection", &anyhow::anyhow!("{e}")))?
        .map_err(|e| CommandError::memory("get_reflection", &e))
}
