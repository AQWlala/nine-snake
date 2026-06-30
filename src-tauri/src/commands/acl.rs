//! ACL commands — set, list, remove.

use tauri::State;
use tracing::instrument;

use crate::commands::error::CommandError;
use crate::AppState;

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "acl_set"))]
pub async fn acl_set(
    state: State<'_, AppState>,
    principal: String,
    resource: String,
    permission: String,
    effect: String,
) -> Result<bool, CommandError> {
    let sqlite = state.sqlite.clone();
    tokio::task::spawn_blocking(move || {
        let id = uuid::Uuid::new_v4().to_string();
        sqlite
            .insert_acl(&id, &principal, &resource, &permission, &effect)
            .map(|_| true)
            .map_err(|e| CommandError::db("acl_set", &e))
    })
    .await
    .map_err(|e| CommandError::internal("acl_set", &anyhow::anyhow!("{e}")))?
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "acl_list"))]
pub async fn acl_list(
    state: State<'_, AppState>,
) -> Result<Vec<(String, String, String, String, String)>, CommandError> {
    let sqlite = state.sqlite.clone();
    tokio::task::spawn_blocking(move || {
        sqlite
            .list_acl()
            .map_err(|e| CommandError::db("acl_list", &e))
    })
    .await
    .map_err(|e| CommandError::internal("acl_list", &anyhow::anyhow!("{e}")))?
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "acl_remove"))]
pub async fn acl_remove(state: State<'_, AppState>, id: String) -> Result<bool, CommandError> {
    let sqlite = state.sqlite.clone();
    tokio::task::spawn_blocking(move || {
        sqlite
            .remove_acl(&id)
            .map(|_| true)
            .map_err(|e| CommandError::db("acl_remove", &e))
    })
    .await
    .map_err(|e| CommandError::internal("acl_remove", &anyhow::anyhow!("{e}")))?
}
