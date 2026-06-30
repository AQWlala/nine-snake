//! Security commands — injection scan, sandbox config.

use tracing::instrument;

use crate::commands::error::CommandError;

/// Full injection scan of arbitrary input.
#[tauri::command]
#[instrument(fields(otel.kind = "injection_scan"))]
pub async fn injection_scan(
    input: String,
) -> Result<crate::security::InjectionScanResult, CommandError> {
    Ok(crate::security::full_injection_scan(&input))
}

/// Retrieve sandbox configuration for a skill.
#[tauri::command]
#[instrument(fields(otel.kind = "sandbox_config"))]
pub async fn sandbox_config(
    skill_id: String,
) -> Result<crate::skills::sandbox::SandboxConfig, CommandError> {
    let mut config = crate::skills::sandbox::SandboxConfig::default();
    config.capabilities = crate::skills::sandbox::CapabilitySet::llm_only();
    Ok(config)
}
