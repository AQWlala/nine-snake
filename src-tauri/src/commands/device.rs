//! Device management commands.

use tauri::State;
use tracing::instrument;

use crate::commands::error::CommandError;
use crate::AppState;

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "list_devices"))]
pub async fn list_devices(
    state: State<'_, AppState>,
) -> Result<Vec<crate::sync::device_manager::DeviceInfo>, CommandError> {
    let dm = state.device_manager.clone();
    tokio::task::spawn_blocking(move || {
        dm.lock()
            .list_devices()
            .map_err(|e| CommandError::internal("list_devices", &e))
    })
    .await
    .map_err(|e| CommandError::internal("list_devices", &anyhow::anyhow!("{e}")))?
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "revoke_device"))]
pub async fn revoke_device(
    state: State<'_, AppState>,
    device_id: String,
) -> Result<bool, CommandError> {
    let dm = state.device_manager.clone();
    tokio::task::spawn_blocking(move || {
        let result = dm.lock().revoke_device(&device_id);
        Ok(result.success)
    })
    .await
    .map_err(|e| CommandError::internal("revoke_device", &anyhow::anyhow!("{e}")))?
}
