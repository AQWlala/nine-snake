//! Core commands — bootstrap, health, metrics, migration, startup, perf, settings, API keys.

use serde::{Deserialize, Serialize};
use tauri::State;
use tracing::instrument;

use crate::commands::error::CommandError;
use crate::memory::MigrationStatus;
use crate::metrics::MetricsSnapshot;
use crate::AppState;

/// v1.0: front-end handshake.  The store calls this on mount to
/// confirm the Tauri runtime is responsive and to record a
/// `bootstrap.start` milestone.  No-op on the back-end side; the
/// actual work is the `AppState::bootstrap` call inside
/// `lib::run`.
#[tauri::command]
#[instrument(fields(otel.kind = "bootstrap"))]
pub async fn bootstrap() -> Result<(), CommandError> {
    Ok(())
}

/// v1.0: reports the running version.  The front-end shows it in
/// the sidebar footer.
#[tauri::command]
pub async fn health() -> Result<HealthDto, CommandError> {
    Ok(HealthDto {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        ollama: "unknown".to_string(),
    })
}

#[tauri::command]
pub async fn health_full(state: State<'_, AppState>) -> Result<HealthDto, CommandError> {
    let ollama_status = {
        let client = state.llm.ollama_client();
        match tokio::time::timeout(std::time::Duration::from_secs(2), client.ping()).await {
            Ok(true) => "ok".to_string(),
            Ok(false) => "down".to_string(),
            Err(_) => "timeout".to_string(),
        }
    };
    Ok(HealthDto {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        ollama: ollama_status,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthDto {
    pub status: String,
    pub version: String,
    pub ollama: String,
}

/// v1.0: returns the cold-start report.  Cheap; just a clone of
/// the in-memory `BTreeMap`.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "startup_report"))]
pub async fn startup_report(
    state: State<'_, AppState>,
) -> Result<crate::perf::StartupReport, CommandError> {
    Ok(state.startup_timer.report())
}

/// v1.0: live process sample.  Returns an empty struct when the
/// `perf-telemetry` feature is off; the front-end handles the
/// "no data" case in `StatusBar`.
#[tauri::command]
#[instrument(fields(otel.kind = "perf_sample"))]
pub async fn perf_sample() -> Result<crate::perf::monitor::PerfSample, CommandError> {
    Ok(crate::perf::monitor::PerfSample::empty())
}

/// v0.2: Tauri command — snapshot the process-wide metrics.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "metrics"))]
pub async fn metrics(state: State<'_, AppState>) -> Result<MetricsSnapshot, CommandError> {
    let _ = state;
    Ok(crate::metrics::global().snapshot())
}

/// v0.2: Tauri command — read the current migration status.
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "migration_status"))]
pub async fn migration_status(state: State<'_, AppState>) -> Result<MigrationStatus, CommandError> {
    let sqlite = state.sqlite.clone();
    let dir = crate::memory::migration::bundled_migrations_dir().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let conn = sqlite.raw_connection();
        let conn = conn.lock();
        crate::memory::migration::migration_status(&conn, &dir)
    })
    .await
    .map_err(|e| CommandError::internal("migration_status", &anyhow::anyhow!("{e}")))?
    .map_err(|e| CommandError::db("migration_status", &e))
}

// ---------------------------------------------------------------------------
// v1.0: persisted app settings (front-end mirror of the user's
// preferences that need to live on disk rather than localStorage).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AppSettingsDto {
    /// Last-selected mode (writing/work/code).
    pub last_mode: Option<String>,
    /// Last-selected view (chat/swarm/memory/code/skills).
    pub last_view: Option<String>,
    /// Ollama URL.
    pub ollama_url: Option<String>,
    /// Default chat model.
    pub chat_model: Option<String>,
    /// Editor workspace root (relative to the project root or
    /// absolute).
    pub workspace: Option<String>,
    /// UI locale: "zh-CN" or "en-US".
    pub locale: Option<String>,
    /// UI theme: "dark" | "light" | "system".
    pub theme: Option<String>,
    /// Accent color (CSS hex).
    pub accent: Option<String>,
    /// Font size in px.
    pub font_size: Option<u32>,
    /// Auto-save interval in seconds.
    pub autosave_sec: Option<u32>,
    /// Custom shell whitelist additions.
    pub extra_shell_bins: Option<Vec<String>>,
    /// Onboarding completed.
    pub onboarding_done: Option<bool>,
}

fn settings_path() -> std::path::PathBuf {
    let base = std::env::var("NINE_SNAKE_DATA_DIR").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(base).join("settings.json")
}

fn read_settings() -> AppSettingsDto {
    let p = settings_path();
    match std::fs::read(&p) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => AppSettingsDto::default(),
    }
}

fn write_settings(s: &AppSettingsDto) -> anyhow::Result<()> {
    let p = settings_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let bytes = serde_json::to_vec_pretty(s)?;
    std::fs::write(&p, bytes)?;
    Ok(())
}

#[tauri::command]
#[instrument(fields(otel.kind = "load_app_settings"))]
pub async fn load_app_settings() -> Result<AppSettingsDto, CommandError> {
    Ok(read_settings())
}

#[tauri::command]
#[instrument(skip(state, settings), fields(otel.kind = "save_app_settings"))]
pub async fn save_app_settings(
    state: State<'_, AppState>,
    settings: AppSettingsDto,
) -> Result<(), CommandError> {
    // v1.0 audit: shell whitelist additions are validated against
    // the in-memory executor.  We currently do not mutate the
    // live whitelist (the executor is `Arc<ShellExecutor>` and
    // `allow` consumes `self`); the user is shown a warning when
    // they add a bin that isn't in the default whitelist.  v1.1
    // will switch to a `parking_lot::RwLock<Vec<String>>` so this
    // can be a hot update.
    if let Some(ref extras) = settings.extra_shell_bins {
        for b in extras {
            if !state.shell.is_allowed(b) {
                tracing::warn!(
                    target: "nine_snake.cmd",
                    bin = %b,
                    "user requested shell bin not in default whitelist; v1.0 cannot hot-add (see docs)"
                );
            }
        }
    }
    write_settings(&settings).map_err(|e| CommandError::internal("save_app_settings", &e))
}

// ---------------------------------------------------------------------------
// v1.0.1 P0#12: API key storage backed by the OS keychain.
// ---------------------------------------------------------------------------

/// Tauri command: write the API key into the OS keychain.
/// Returns `Ok(())` — the key is never echoed back.
#[tauri::command]
#[instrument(fields(otel.kind = "set_api_key"))]
pub async fn set_api_key(value: String) -> Result<(), CommandError> {
    if value.trim().is_empty() {
        crate::security::keychain::delete(crate::security::keychain::KEY_API_KEY)
            .map_err(|e| CommandError::internal("set_api_key", &e))?;
        return Ok(());
    }
    crate::security::keychain::set(crate::security::keychain::KEY_API_KEY, &value)
        .map_err(|e| CommandError::internal("set_api_key", &e))
}

/// Masked API key returned to the front-end — never the full secret.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaskedApiKey {
    pub masked: String,
    pub length: usize,
    pub prefix: String,
}

/// Tauri command: read the API key from the OS keychain.
/// Returns a masked version (e.g. `sk-****678`) — the full key
/// is never sent across the IPC boundary.
#[tauri::command]
#[instrument(fields(otel.kind = "get_api_key"))]
pub async fn get_api_key() -> Result<Option<MaskedApiKey>, CommandError> {
    let raw = crate::security::keychain::get(crate::security::keychain::KEY_API_KEY)
        .map_err(|e| CommandError::internal("get_api_key", &e))?;
    Ok(raw.map(|key| {
        let len = key.len();
        let prefix_len = key.len().min(3);
        let suffix_len = key.len().min(3);
        let prefix = key[..prefix_len].to_string();
        let suffix = if len > 6 {
            &key[len - suffix_len..]
        } else {
            ""
        };
        let masked = if len > 6 {
            format!("{}****{}", &key[..prefix_len], suffix)
        } else if len > 0 {
            format!("{}****", &key[..prefix_len])
        } else {
            String::new()
        };
        MaskedApiKey {
            masked,
            length: len,
            prefix,
        }
    }))
}

/// Tauri command: delete the API key from the OS keychain.
/// Idempotent — deleting a missing entry is a successful no-op.
#[tauri::command]
#[instrument(fields(otel.kind = "delete_api_key"))]
pub async fn delete_api_key() -> Result<(), CommandError> {
    crate::security::keychain::delete(crate::security::keychain::KEY_API_KEY)
        .map_err(|e| CommandError::internal("delete_api_key", &e))
}
