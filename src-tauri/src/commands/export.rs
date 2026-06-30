//! Data export/import commands.

use tauri::State;
use tracing::instrument;

use crate::commands::error::CommandError;
use crate::AppState;

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "export_memories"))]
pub async fn export_memories(
    state: State<'_, AppState>,
    format: String,
    path: String,
) -> Result<crate::memory::export::ExportManifest, CommandError> {
    let exporter = crate::memory::export::DataExporter::new((*state.sqlite).clone());
    let p = std::path::PathBuf::from(&path);
    match format.as_str() {
        "jsonld" | "json-ld" => exporter
            .export_jsonld(&p)
            .await
            .map_err(|e| CommandError::internal("export_memories", &e)),
        _ => Err(CommandError::validation("export_memories")
            .with_details(format!("unsupported format: {format}"))),
    }
}

#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "import_memories"))]
pub async fn import_memories(
    state: State<'_, AppState>,
    path: String,
) -> Result<crate::memory::export::ImportResult, CommandError> {
    let exporter = crate::memory::export::DataExporter::new((*state.sqlite).clone());
    let p = std::path::PathBuf::from(&path);
    exporter
        .import_jsonld(&p)
        .await
        .map_err(|e| CommandError::internal("import_memories", &e))
}
