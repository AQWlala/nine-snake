//! Memory commands — store, search, read, update, delete, stats.

use serde::{Deserialize, Serialize};
use tauri::State;
use tracing::{instrument, warn};

use crate::api::server::{
    NineSnakeService, SearchMemoryHit, SearchMemoryRequest, StoreMemoryRequest, StoreMemoryResponse,
};
use crate::commands::error::CommandError;
use crate::memory::types::{Memory, MemoryLayer, SourceKind};
use crate::AppState;

/// Tauri command: store a memory.
///
/// L7 (Singularity) guard: only `SourceKind::System` may write to L7.
/// Front-end requests with `layer: L7` from non-System sources are
/// silently demoted to L6 (Values) to prevent memory poisoning.
#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "memory_store"))]
pub async fn memory_store(
    state: State<'_, AppState>,
    mut request: StoreMemoryRequest,
) -> Result<StoreMemoryResponse, CommandError> {
    if request.layer == MemoryLayer::L7 && request.source != SourceKind::System {
        warn!(
            target: "nine_snake.cmd",
            source = ?request.source,
            "non-System source attempted L7 write; demoting to L6"
        );
        request.layer = MemoryLayer::L6;
    }
    let resp = state
        .memory_store(request)
        .await
        .map_err(|e| CommandError::memory("memory_store", &e))?;
    crate::metrics::global().record_store();
    Ok(resp)
}

/// Tauri command: vector search over the memory store.
#[tauri::command]
#[instrument(skip(state, request), fields(otel.kind = "memory_search"))]
pub async fn memory_search(
    state: State<'_, AppState>,
    request: SearchMemoryRequest,
) -> Result<Vec<SearchMemoryHit>, CommandError> {
    let resp = state
        .memory_search(request)
        .await
        .map_err(|e| CommandError::lance("memory_search", &e))?;
    crate::metrics::global().record_search();
    Ok(resp)
}

/// Tauri command: fetch a memory by id.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "memory_get"))]
pub async fn memory_get(
    state: State<'_, AppState>,
    id: String,
) -> Result<Option<Memory>, CommandError> {
    let sqlite = state.sqlite.clone();
    tokio::task::spawn(async move { sqlite.get(&id).await })
        .await
        .map_err(|e| CommandError::internal("memory_get", &anyhow::anyhow!("{e}")))?
        .map_err(|e| CommandError::db("memory_get", &e))
}

/// Tauri command: list the N most recent memories.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "memory_list_recent"))]
pub async fn memory_list_recent(
    state: State<'_, AppState>,
    limit: usize,
) -> Result<Vec<Memory>, CommandError> {
    let sqlite = state.sqlite.clone();
    tokio::task::spawn(async move { sqlite.list_recent(limit.max(1)).await })
        .await
        .map_err(|e| CommandError::internal("memory_list_recent", &anyhow::anyhow!("{e}")))?
        .map_err(|e| CommandError::db("memory_list_recent", &e))
}

/// Tauri command: update a memory's `importance` (clamped to `[0, 1]`).
///
/// L7 guard: memories on L7 cannot have their importance lowered
/// below 0.9 — this prevents accidental demotion of core-value
/// memories that should only be removed via explicit delete.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "memory_update_importance"))]
pub async fn memory_update_importance(
    state: State<'_, AppState>,
    id: String,
    importance: f32,
) -> Result<Memory, CommandError> {
    let sqlite = state.sqlite.clone();
    let sqlite_for_check = sqlite.clone();
    let id_clone = id.clone();
    let mem = tokio::task::spawn(async move { sqlite_for_check.get(&id_clone).await })
        .await
        .map_err(|e| CommandError::internal("memory_update_importance", &anyhow::anyhow!("{e}")))?
        .map_err(|e| CommandError::db("memory_update_importance", &e))?;
    let clamped = importance.clamp(0.0, 1.0);
    let final_importance = if let Some(m) = &mem {
        if m.layer == MemoryLayer::L7 && clamped < 0.9 {
            warn!(
                target: "nine_snake.cmd",
                id = %id,
                requested = clamped,
                "L7 memory importance cannot be lowered below 0.9; clamping"
            );
            0.9
        } else {
            clamped
        }
    } else {
        clamped
    };
    tokio::task::spawn(async move { sqlite.update_importance(&id, final_importance).await })
        .await
        .map_err(|e| CommandError::internal("memory_update_importance", &anyhow::anyhow!("{e}")))?
        .map_err(|e| CommandError::db("memory_update_importance", &e))
}

/// Tauri command: hard-delete a memory.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "memory_delete"))]
pub async fn memory_delete(state: State<'_, AppState>, id: String) -> Result<bool, CommandError> {
    let sqlite = state.sqlite.clone();
    let id_for_thread = id.clone();
    let res = tokio::task::spawn(async move { sqlite.delete(&id_for_thread).await })
        .await
        .map_err(|e| CommandError::internal("memory_delete", &anyhow::anyhow!("{e}")))?;
    match res {
        Ok(deleted) => {
            if deleted {
                if let Err(e) = state.lance.delete(&id).await {
                    warn!(target: "nine_snake.cmd", error = ?e, "lance delete failed");
                }
            }
            Ok(deleted)
        }
        Err(e) => Err(CommandError::db("memory_delete", &e)),
    }
}

/// Tauri command: batch-fetch memories by id (preserves the
/// caller's order).
#[tauri::command]
#[instrument(skip(state, ids), fields(otel.kind = "memory_get_many"))]
pub async fn memory_get_many(
    state: State<'_, AppState>,
    ids: Vec<String>,
) -> Result<Vec<Memory>, CommandError> {
    let sqlite = state.sqlite.clone();
    tokio::task::spawn(async move { sqlite.get_many(&ids).await })
        .await
        .map_err(|e| CommandError::internal("memory_get_many", &anyhow::anyhow!("{e}")))?
        .map_err(|e| CommandError::db("memory_get_many", &e))
}

/// Snapshot of layer distribution for the stats RPC.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryStats {
    pub total: u64,
    pub by_layer: std::collections::HashMap<MemoryLayer, u64>,
}

/// Tauri command: per-layer memory counts.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "memory_stats"))]
pub async fn memory_stats(state: State<'_, AppState>) -> Result<MemoryStats, CommandError> {
    let sqlite = state.sqlite.clone();
    let rows = tokio::task::spawn(async move { sqlite.counts_per_layer().await })
        .await
        .map_err(|e| CommandError::internal("memory_stats", &anyhow::anyhow!("{e}")))?
        .map_err(|e| CommandError::db("memory_stats", &e))?;
    let total = rows.values().sum();
    Ok(MemoryStats {
        total,
        by_layer: rows,
    })
}
