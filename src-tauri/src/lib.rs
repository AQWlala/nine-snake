//! `nine_snake_lib` — the library crate backing the `nine-snake` binary.
//!
//! The crate is organised as a small collection of mostly independent
//! subsystems that communicate through a shared [`AppState`] living inside
//! the Tauri managed-state container:
//!
//! * [`memory`]   — the 5-layer v7.0 memory system (L0-L5)
//! * [`llm`]      — model gateway (Ollama + optional remote fallback)
//! * [`swarm`]    — multi-agent orchestration
//! * [`api`]      — internal Rust-side service trait surface
//! * [`commands`] — the Tauri command handlers exposed to the front-end
//! * [`metrics`]  — process-wide atomic counters
//! * [`grpc`]     — v0.3: optional gRPC server (22 RPCs, tonic 0.12)
//! * [`skills`]   — v0.3: skill CRUD + execution engine
//!
//! The public surface intentionally re-exports a few well-known types so
//! downstream crates (and the binary) don't have to memorise module paths.

// Clippy: allow style lints that are noisy across this codebase but do
// not indicate correctness issues.  Individual modules may still fix
// them opportunistically.
#![allow(
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::derivable_impls,
    clippy::should_implement_trait,
    clippy::manual_strip,
    clippy::len_without_is_empty,
    clippy::unnecessary_sort_by,
    clippy::doc_lazy_continuation,
    clippy::doc_overindented_list_items,
    clippy::needless_borrow,
    clippy::manual_clamp,
    clippy::empty_line_after_doc_comments,
    clippy::await_holding_lock,
    clippy::field_reassign_with_default,
    clippy::new_without_default
)]

pub mod api;
#[cfg(feature = "channels")]
pub mod channel;
pub mod commands;
pub mod editor;
pub mod error_ui;
pub mod llm;
pub mod memory;
pub mod metrics;
pub mod os;
pub mod perf;
// v0.5: OS keychain + sensitive-data redaction.
pub mod identity;
pub mod security;
pub mod skills;
pub mod swarm;
pub mod sync;
// v1.3: MCP protocol client (feature-gated).
#[cfg(feature = "mcp")]
pub mod mcp;
pub mod work;
pub mod writing;
// v1.1 P0-2: Tool abstraction layer.
pub mod tools;

// v1.3: closed-loop self-evolution (task outcomes + skill archive +
// prompt mutator).  Off by default; enable with
// `--features self-evolution`.
#[cfg(feature = "self-evolution")]
pub mod evolution;

#[cfg(feature = "grpc")]
pub mod grpc;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use parking_lot::Mutex;
use tauri::Manager;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, EnvFilter};

#[cfg(feature = "channels")]
use crate::channel::bridge::MessageBridge;
#[cfg(feature = "channels")]
use crate::channel::webchat::WebChatService;
use crate::editor::EditorState;
use crate::llm::gateway::LlmGateway;
use crate::llm::ollama::OllamaClient;
use crate::memory::blackhole::BlackholeEngine;
use crate::memory::embedder::Embedder;
use crate::memory::lance_store::LanceStore;
use crate::memory::reflect::{ReflectConfig, ReflectionEngine};
use crate::memory::sponge::SpongeEngine;
use crate::memory::sqlite_store::SqliteStore;
use crate::os::ClipboardService;
use crate::os::ShellExecutor;
use crate::perf::StartupTimer;
use crate::skills::audit::SkillAuditLogger;
use crate::skills::engine::SkillEngine;
use crate::skills::extractor::SkillExtractor;
use crate::skills::importer::SkillImporter;
use crate::skills::store::SkillStore;
use crate::swarm::composer::SkillComposer;
use crate::swarm::orchestrator::SwarmOrchestrator;
use crate::sync::device_manager::DeviceManager;
use crate::sync::LocalTransport;
use crate::tools::{shell_tool::ShellTool, ToolRegistry};
use crate::work::WorkEngine;
use crate::writing::WritingEngine;

/// Configuration sourced from environment variables (with sensible defaults).
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Path to the SQLite database file.
    pub db_path: String,
    /// Path to the LanceDB vector store directory.
    pub lance_path: String,
    /// Base URL of the Ollama HTTP server.
    pub ollama_url: String,
    /// Default chat model name served by Ollama.
    pub chat_model: String,
    /// Default embedding model name served by Ollama.
    pub embed_model: String,
    /// Optional remote fallback URL (e.g. OpenAI-compatible endpoint).
    pub remote_fallback_url: Option<String>,
    /// Number of days of inactivity before the black-hole engine may compress.
    pub blackhole_threshold_days: u32,
    /// Embedding vector dimensionality.
    pub embedding_dim: usize,
    /// v0.2: background reflection worker period in seconds. `0`
    /// disables the worker.
    pub reflect_interval_secs: u64,
    /// v0.2: reflection window size in days.
    pub reflect_window_days: i64,
    /// v0.2: minimum source-memory importance for reflection.
    pub reflect_min_importance: f32,
    /// v0.3: enable the in-process gRPC server. Default `true` (set
    /// `NINE_SNAKE_GRPC=0` to disable). The gRPC port is configured
    /// via `grpc_bind_addr`.
    pub grpc_enabled: bool,
    /// v0.3: bind address for the gRPC server. Default
    /// `127.0.0.1:50051`.
    pub grpc_bind_addr: String,
    /// v0.5: workspace root for the editor.  All file operations
    /// are sandboxed to this directory.
    pub editor_workspace: String,
    /// v0.5: directory used by the local sync transport.  Defaults
    /// to `<config_dir>/sync_inbox`.
    pub sync_inbox: String,
}

impl AppConfig {
    /// Loads configuration from environment variables, falling back to
    /// defaults appropriate for a first-run local development setup.
    pub fn from_env() -> Self {
        Self {
            db_path: std::env::var("NINE_SNAKE_DB").unwrap_or_else(|_| "nine_snake.db".to_string()),
            lance_path: std::env::var("NINE_SNAKE_LANCE")
                .unwrap_or_else(|_| "nine_snake_lance".to_string()),
            ollama_url: std::env::var("OLLAMA_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string()),
            chat_model: std::env::var("NINE_SNAKE_CHAT_MODEL")
                .unwrap_or_else(|_| "qwen2.5:3b".to_string()),
            embed_model: std::env::var("NINE_SNAKE_EMBED_MODEL")
                .unwrap_or_else(|_| "BAAI/bge-small-zh-v1.5".to_string()),
            remote_fallback_url: std::env::var("NINE_SNAKE_REMOTE_URL").ok(),
            blackhole_threshold_days: std::env::var("NINE_SNAKE_BH_DAYS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
            embedding_dim: std::env::var("NINE_SNAKE_EMBED_DIM")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(512),
            reflect_interval_secs: std::env::var("NINE_SNAKE_REFLECT_INTERVAL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(crate::memory::reflect::DEFAULT_REFLECT_INTERVAL_SECS),
            reflect_window_days: std::env::var("NINE_SNAKE_REFLECT_WINDOW_DAYS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(crate::memory::reflect::DEFAULT_REFLECT_WINDOW_DAYS),
            reflect_min_importance: std::env::var("NINE_SNAKE_REFLECT_MIN_IMPORTANCE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(crate::memory::reflect::DEFAULT_REFLECT_MIN_IMPORTANCE),
            grpc_enabled: std::env::var("NINE_SNAKE_GRPC")
                .ok()
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
            grpc_bind_addr: std::env::var("NINE_SNAKE_GRPC_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:50051".to_string()),
            editor_workspace: std::env::var("NINE_SNAKE_WORKSPACE")
                .unwrap_or_else(|_| ".".to_string()),
            sync_inbox: std::env::var("NINE_SNAKE_SYNC_INBOX")
                .unwrap_or_else(|_| "sync_inbox".to_string()),
        }
    }
}

/// The single managed-state struct shared across Tauri commands.
///
/// Cloning the handle is cheap: every field is an `Arc` or itself clonable.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub sqlite: Arc<SqliteStore>,
    pub lance: Arc<LanceStore>,
    pub embedder: Arc<Embedder>,
    pub llm: Arc<LlmGateway>,
    pub sponge: Arc<SpongeEngine>,
    pub blackhole: Arc<BlackholeEngine>,
    pub swarm: Arc<SwarmOrchestrator>,
    /// v0.2: L5 reflection engine. Always present (even if LLM is
    /// unavailable, in which case the engine falls back to template
    /// synthesis).
    pub reflection: Arc<ReflectionEngine>,
    /// v0.3: skill CRUD + execution engine.
    pub skills: Arc<SkillEngine>,
    /// v1.2: skill closed-loop learning 鈥?auto-extracts reusable skills from swarm tasks.
    pub skill_extractor: Arc<SkillExtractor>,
    /// v1.2: skill auto-composer for orchestration upgrade.
    pub skill_composer: Arc<SkillComposer>,
    /// v1.3 P2-7: skill marketplace — search, install, update, publish.
    pub marketplace: Arc<skills::SkillMarketplace>,
    /// v1.3: skill audit logger.
    pub skill_audit_logger: Arc<SkillAuditLogger>,
    #[cfg(feature = "channels")]
    /// v1.2: multi-channel message bridge (JiWenSwarm delivery fabric).
    pub message_bridge: Option<Arc<MessageBridge>>,
    /// v0.3: handle to the reflection background worker, so the
    /// `AppState::shutdown` call can `await` the join (rather than
    /// dropping a `JoinHandle` into a static `OnceCell` as v0.2 did).
    pub reflect_worker: Arc<Mutex<Option<JoinHandle<()>>>>,
    /// v0.3: handle to the in-process gRPC server task.
    #[cfg(feature = "grpc")]
    pub grpc_server: Arc<Mutex<Option<grpc::GrpcHandle>>>,
    /// v0.5: writing engine (long-form documents + template library).
    pub writing: Arc<WritingEngine>,
    /// v0.5: work engine (kanban + time tracking).
    pub work: Arc<WorkEngine>,
    /// v0.5: editor state (file ops, watcher, git).
    pub editor: EditorState,
    /// v0.5: clipboard service.
    pub clipboard: ClipboardService,
    /// v0.5: shell executor with whitelist.
    pub shell: Arc<ShellExecutor>,
    /// v0.5: local sync transport.
    pub sync_transport: Arc<LocalTransport>,
    /// v1.0: startup profiler (milestones + final report).
    pub startup_timer: StartupTimer,
    /// v1.1 P0-2: tool registry with registered tools (shell, etc.).
    pub tool_registry: Arc<ToolRegistry>,
    #[cfg(feature = "channels")]
    /// v1.3: WebChat share link service.
    pub webchat_service: WebChatService,
    /// v1.3: device manager for sync pairing.
    pub device_manager: Arc<parking_lot::Mutex<DeviceManager>>,
    /// v1.3: MCP manager (feature-gated).
    #[cfg(feature = "mcp")]
    pub mcp_manager: Arc<crate::mcp::client::McpManager>,
}

impl AppState {
    /// Bootstraps a fully-wired [`AppState`] from the given config.
    ///
    /// On failure all already-initialised subsystems are dropped; the
    /// returned `anyhow::Error` carries the full context chain.
    pub async fn bootstrap(config: AppConfig) -> anyhow::Result<Self> {
        info!(target: "nine_snake", "bootstrapping app state");
        let startup = StartupTimer::start();
        startup.mark("bootstrap.start");

        // Phase 1: storage (SQLite + migrations + LanceDB)
        let (sqlite, lance) = Self::bootstrap_storage(&config, &startup).await?;

        // Phase 2: AI core (embedder, LLM, sponge, blackhole)
        let (embedder, llm, sponge, blackhole) =
            Self::bootstrap_ai_core(&config, &sqlite, &lance, &startup).await?;

        // Phase 3: swarm + reflection
        let (swarm, reflection) = Self::bootstrap_swarm_and_reflection(
            &config, &sqlite, &lance, &embedder, &llm, &sponge,
        );

        // Phase 4: skills ecosystem
        let (skills, skill_extractor, skill_composer, marketplace, skill_audit_logger) =
            Self::bootstrap_skills(&config, &sqlite, &llm);
        swarm.set_composer(skill_composer.clone());

        // Phase 5: workspace tooling + final assembly
        #[cfg(feature = "channels")]
        let message_bridge = Self::bootstrap_message_bridge();
        let writing = Arc::new(WritingEngine::new(sqlite.clone(), Some(sponge.clone())));
        let work = Arc::new(WorkEngine::new(sqlite.clone()));
        let editor = Self::bootstrap_editor(&config);
        startup.mark("bootstrap.editor");
        let clipboard = Self::bootstrap_clipboard();
        let shell = Arc::new(ShellExecutor::new());
        let tool_registry = Arc::new(ToolRegistry::new());
        tool_registry.register(Arc::new(ShellTool::new((*shell).clone())));
        let sync_transport = Self::bootstrap_sync(&config);
        startup.mark("bootstrap.end");

        let device_manager = Arc::new(parking_lot::Mutex::new(DeviceManager::new(
            sqlite.raw_connection(),
        )));

        Ok(Self {
            config: Arc::new(config),
            sqlite,
            lance,
            embedder,
            llm,
            sponge,
            blackhole,
            swarm,
            reflection,
            skills,
            writing,
            work,
            editor,
            clipboard,
            shell,
            sync_transport,
            reflect_worker: Arc::new(Mutex::new(None)),
            #[cfg(feature = "grpc")]
            grpc_server: Arc::new(Mutex::new(None)),
            startup_timer: startup,
            skill_extractor,
            skill_composer,
            marketplace,
            skill_audit_logger,
            #[cfg(feature = "channels")]
            message_bridge,
            tool_registry,
            #[cfg(feature = "channels")]
            webchat_service: WebChatService::new(),
            device_manager,
            #[cfg(feature = "mcp")]
            mcp_manager: Arc::new(crate::mcp::client::McpManager::new()),
        })
    }

    // -- bootstrap phase helpers --

    async fn bootstrap_storage(
        config: &AppConfig,
        startup: &StartupTimer,
    ) -> anyhow::Result<(Arc<SqliteStore>, Arc<LanceStore>)> {
        let db_path = config.db_path.clone();
        let sqlite = tokio::task::spawn_blocking(move || SqliteStore::open(&db_path))
            .await
            .context("spawn_blocking for sqlite open failed")?
            .context("opening sqlite store")?;
        let sqlite = Arc::new(sqlite);
        startup.mark("bootstrap.sqlite");

        let s = sqlite.clone();
        let mdir = crate::memory::migration::bundled_migrations_dir().to_path_buf();
        let applied = tokio::task::spawn_blocking(move || {
            let conn = s.raw_connection();
            let g = conn.lock();
            crate::memory::migration::run_migrations(&g, &mdir)
        })
        .await
        .context("spawn_blocking for migrations failed")?
        .context("running migrations")?;
        if !applied.is_empty() {
            info!(target: "nine_snake", count = applied.len(),
                last = applied.last().map(|m| m.version).unwrap_or(0),
                "applied migrations");
        }
        startup.mark("bootstrap.migrations");

        let lance = Arc::new(
            LanceStore::open(&config.lance_path, config.embedding_dim)
                .await
                .context("opening lance store")?,
        );
        startup.mark("bootstrap.lance");
        Ok((sqlite, lance))
    }

    async fn bootstrap_ai_core(
        config: &AppConfig,
        sqlite: &Arc<SqliteStore>,
        lance: &Arc<LanceStore>,
        startup: &StartupTimer,
    ) -> anyhow::Result<(
        Arc<Embedder>,
        Arc<LlmGateway>,
        Arc<SpongeEngine>,
        Arc<BlackholeEngine>,
    )> {
        let embedder = Arc::new(Embedder::new(
            OllamaClient::new(config.ollama_url.clone()),
            config.embed_model.clone(),
            config.embedding_dim,
        ));
        let ollama = Arc::new(OllamaClient::new(config.ollama_url.clone()));
        let ak = std::env::var("NINE_SNAKE_ANTHROPIC_KEY").ok();
        let am = std::env::var("NINE_SNAKE_ANTHROPIC_MODEL").ok();
        let llm = Arc::new(LlmGateway::new(
            ollama,
            config.chat_model.clone(),
            config.remote_fallback_url.clone(),
            ak,
            am,
        ));
        startup.mark("bootstrap.llm");
        let sponge = Arc::new(SpongeEngine::new(
            sqlite.clone(),
            lance.clone(),
            embedder.clone(),
        ));
        let blackhole = Arc::new(BlackholeEngine::new(
            sqlite.clone(),
            lance.clone(),
            config.blackhole_threshold_days,
        ));
        Ok((embedder, llm, sponge, blackhole))
    }

    fn bootstrap_swarm_and_reflection(
        config: &AppConfig,
        sqlite: &Arc<SqliteStore>,
        lance: &Arc<LanceStore>,
        embedder: &Arc<Embedder>,
        llm: &Arc<LlmGateway>,
        sponge: &Arc<SpongeEngine>,
    ) -> (Arc<SwarmOrchestrator>, Arc<ReflectionEngine>) {
        let swarm = Arc::new(SwarmOrchestrator::new(
            llm.clone(),
            sponge.clone(),
            lance.clone(),
            embedder.clone(),
            sqlite.clone(),
        ));
        let cfg = ReflectConfig {
            window_days: config.reflect_window_days,
            min_importance: config.reflect_min_importance,
            worker_interval_secs: config.reflect_interval_secs,
            ..ReflectConfig::default()
        };
        let reflection = Arc::new(ReflectionEngine::new(
            sqlite.clone(),
            Some(llm.clone()),
            cfg,
        ));
        (swarm, reflection)
    }

    fn bootstrap_skills(
        config: &AppConfig,
        sqlite: &Arc<SqliteStore>,
        llm: &Arc<LlmGateway>,
    ) -> (
        Arc<SkillEngine>,
        Arc<SkillExtractor>,
        Arc<SkillComposer>,
        Arc<skills::SkillMarketplace>,
        Arc<SkillAuditLogger>,
    ) {
        let ss = Arc::new(
            SkillStore::new(sqlite.as_ref().clone()).expect("SkillStore::new must succeed"),
        );
        let audit = Arc::new(SkillAuditLogger::new(sqlite.raw_connection()));
        let skills =
            Arc::new(SkillEngine::from_store((*ss).clone(), llm.clone()).with_audit(audit.clone()));
        let adir = config
            .db_path
            .rsplit_once(std::path::MAIN_SEPARATOR)
            .map(|(d, _)| d)
            .unwrap_or(".")
            .to_string()
            + "/skills_archive";
        let extr = Arc::new(SkillExtractor::new(llm.clone(), ss.clone(), adir));
        let comp = Arc::new(SkillComposer::new(ss.clone(), Some(llm.clone())));
        let imp = Arc::new(SkillImporter::new((*ss).clone()));
        let mp = Arc::new(skills::SkillMarketplace::new(ss, imp));
        let _ = mp.refresh();
        // v2.0: seed built-in demo skills on first run (idempotent).
        crate::skills::seed_demo_skills(&skills).unwrap_or_else(|e| {
            tracing::warn!(target: "nine_snake", error = ?e, "failed to seed demo skills");
            Vec::new()
        });
        (skills, extr, comp, mp, audit)
    }

    #[cfg(feature = "channels")]
    fn bootstrap_message_bridge() -> Option<Arc<MessageBridge>> {
        let url = std::env::var("NINE_SNAKE_BRIDGE_URL").unwrap_or_default();
        let b = MessageBridge::new(&url).map(Arc::new);
        if b.is_some() {
            info!(target: "nine_snake", bridge_url = %url, "message bridge initialised");
        }
        b
    }

    fn bootstrap_editor(config: &AppConfig) -> EditorState {
        EditorState::new(&config.editor_workspace).unwrap_or_else(|e| {
            tracing::warn!(target: "nine_snake", error = ?e,
                workspace = %config.editor_workspace,
                "editor workspace unavailable; falling back to current dir");
            EditorState::new(".").expect("current dir is always a directory")
        })
    }

    fn bootstrap_clipboard() -> ClipboardService {
        ClipboardService::new().unwrap_or_else(|e| {
            tracing::warn!(target: "nine_snake", error = ?e,
                "clipboard unavailable; using noop fallback");
            ClipboardService::noop()
        })
    }

    fn bootstrap_sync(config: &AppConfig) -> Arc<LocalTransport> {
        Arc::new(LocalTransport::new(&config.sync_inbox).unwrap_or_else(|e| {
            tracing::warn!(target: "nine_snake", error = ?e,
                inbox = %config.sync_inbox,
                "sync inbox unavailable; using temp dir");
            let tmp = std::env::temp_dir().join("nine-snake-sync-inbox");
            LocalTransport::new(&tmp).expect("temp dir always works")
        }))
    }
    /// Wakes the background reflection worker, signals the gRPC
    /// server to stop, and awaits both joins with a brief grace
    /// period. Idempotent and safe to call from Tauri shutdown.
    pub async fn shutdown(&self) {
        let notify = self.reflection.shutdown_handle();
        notify.notify_waiters();

        // Take the worker handle out of the mutex and await it.
        let worker = { self.reflect_worker.lock().take() };
        if let Some(h) = worker {
            // Give the worker up to 250 ms to exit cleanly. We use a
            // timeout so a misbehaving worker can't deadlock the
            // shutdown path.
            match tokio::time::timeout(Duration::from_millis(250), h).await {
                Ok(_) => info!(target: "nine_snake", "reflection worker stopped"),
                Err(_) => warn!(target: "nine_snake", "reflection worker did not stop in time"),
            }
        }

        // Tear down the gRPC server (if any) gracefully.
        #[cfg(feature = "grpc")]
        {
            let grpc = { self.grpc_server.lock().take() };
            if let Some(h) = grpc {
                h.shutdown().await;
            }
        }
    }
}

/// Installs the `tracing` subscriber. Safe to call multiple times.
///
/// v0.2: writes structured JSON to stdout when the
/// `NINE_SNAKE_LOG_FORMAT=json` environment variable is set; the
/// default remains human-readable pretty output.
///
/// v1.0: when `NINE_SNAKE_LOG_DIR` is set we also write a
/// daily-rotated JSON log file via `tracing_appender`.  This is
/// what the user-facing "Open logs folder" menu points at.
pub fn init_tracing() {
    static INIT: once_cell::sync::OnceCell<()> = once_cell::sync::OnceCell::new();
    INIT.get_or_init(|| {
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info,nine_snake=debug"));
        let use_json = std::env::var("NINE_SNAKE_LOG_FORMAT")
            .map(|v| v.eq_ignore_ascii_case("json"))
            .unwrap_or(false);
        if let Ok(dir) = std::env::var("NINE_SNAKE_LOG_DIR") {
            // v1.0: also append to a daily-rotated file.  We use
            // a non-blocking guard so a slow disk cannot stall the
            // Tauri command loop.
            let appender = tracing_appender::rolling::daily(&dir, "nine-snake.log");
            let (nb, _guard) = tracing_appender::non_blocking(appender);
            let _ = Box::leak(Box::new(_guard));
            if use_json {
                let _ = fmt()
                    .with_env_filter(filter.clone())
                    .json()
                    .with_writer(nb)
                    .try_init();
            } else {
                let _ = fmt()
                    .with_env_filter(filter.clone())
                    .with_writer(nb)
                    .try_init();
            }
        } else if use_json {
            let _ = fmt().with_env_filter(filter).json().try_init();
        } else {
            let _ = fmt().with_env_filter(filter).try_init();
        }
    });
}

/// Tauri application entry — builds the runtime, wires commands, runs the app.
pub fn run() {
    init_tracing();
    info!(target: "nine_snake", version = env!("CARGO_PKG_VERSION"), "starting nine-snake");

    let config = AppConfig::from_env();
    // v1.0.1 fix: `?expr` is the `Debug`-format shorthand that
    // `tracing` recognises only when the *argument* is a literal
    // expression.  `?config.db_path` is parsed as a single
    // identifier that starts with `?`, which the macro cannot
    // disambiguate.  Use the explicit form `db_path = ?db_path`.
    info!(target: "nine_snake", db_path = ?config.db_path, "loaded configuration");

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_dialog::init())
        // v0.5: OS integration plugins.
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        // v0.5: autostart is opt-in at runtime; we only initialise
        // the plugin here so the user can toggle it from settings.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![]),
        ))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(move |app| {
            let handle = app.handle().clone();
            // Bootstrap state asynchronously so we don't block Tauri's main thread.
            //
            // v0.3 fix: when bootstrap fails, surface a user-facing
            // dialog and exit the application. The v0.2 behaviour of
            // "warn and continue" left the user with an uninitialised
            // app and no clue what to do.
            tauri::async_runtime::spawn(async move {
                match AppState::bootstrap(config.clone()).await {
                    Ok(state) => {
                        // Start the reflection background worker; the
                        // JoinHandle is parked on the AppState so
                        // shutdown can await it (see
                        // `AppState::shutdown`).
                        if let Some(h) = state.reflection.clone().spawn_worker() {
                            *state.reflect_worker.lock() = Some(h);
                            info!(target: "nine_snake", "reflection worker started");
                        }

                        // v0.3: optionally start the in-process gRPC
                        // server.
                        #[cfg(feature = "grpc")]
                        if state.config.grpc_enabled {
                            match grpc::start_server(
                                state.config.grpc_bind_addr.clone(),
                                state.clone(),
                            )
                            .await
                            {
                                Ok(handle) => {
                                    info!(
                                        target: "nine_snake",
                                        addr = %state.config.grpc_bind_addr,
                                        "gRPC server started"
                                    );
                                    *state.grpc_server.lock() = Some(handle);
                                }
                                Err(e) => {
                                    error!(
                                        target: "nine_snake",
                                        error = ?e,
                                        "gRPC server failed to start; continuing without it"
                                    );
                                }
                            }
                        } else {
                            info!(target: "nine_snake", "gRPC server disabled by config");
                        }

                        // v1.3: MCP — connect all configured servers and
                        // register their tools into the ToolRegistry.
                        #[cfg(feature = "mcp")]
                        {
                            state.mcp_manager.connect_all().await;
                            let mcp_tools = state.mcp_manager.list_all_tools().await;
                            if !mcp_tools.is_empty() {
                                info!(target: "nine_snake", count = mcp_tools.len(), "MCP tools discovered");
                            }
                        }

                        handle.manage(state);
                        info!(target: "nine_snake", "app state ready");
                    }
                    Err(e) => {
                        error!(target: "nine_snake", error = ?e, "failed to bootstrap app state");
                        // v0.3: tell the user, then exit. The dialog
                        // plugin is registered above so the call
                        // resolves. The process exit ensures the user
                        // doesn't end up with a half-broken UI.
                        use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
                        let _ = handle
                            .dialog()
                            .message(format!("九头蛇启动失败：{e:#}\n\n将退出应用。"))
                            .title("Nine-snake bootstrap error")
                            .kind(MessageDialogKind::Error)
                            .blocking_show();
                        std::process::exit(1);
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::health,
            commands::health_full,
            // v1.0.1 revert: P0#13 commands-by-topic split is

            // path.  `chat` is the only name that historically
            // collided with a `chat` module elsewhere (e.g. the
            // gRPC-generated client) — it now resolves to the
            commands::chat,
            commands::memory_store,
            commands::memory_search,
            commands::memory_get,
            commands::memory_list_recent,
            commands::memory_update_importance,
            commands::memory_delete,
            commands::memory_get_many,
            commands::memory_stats,
            commands::swarm_execute,
            commands::swarm_list_agents,
            commands::swarm_get_agent,
            commands::llm_complete,
            commands::llm_chat,
            commands::llm_embed,
            commands::reflect_now,
            commands::list_reflections,
            commands::get_reflection,
            commands::metrics,
            commands::migration_status,
            // v1.0: perf + settings commands.
            commands::startup_report,
            commands::perf_sample,
            commands::load_app_settings,
            commands::save_app_settings,
            // v0.3: Skill CRUD.
            commands::skill_create,
            commands::skill_use,
            commands::skill_rate,
            commands::skill_list,
            commands::skill_search,
            commands::skill_import,
            // v0.5: writing.
            commands::writing_list_templates,
            commands::writing_get_template,
            commands::writing_create_document,
            commands::writing_update_document,
            commands::writing_get_document,
            commands::writing_list_documents,
            commands::writing_delete_document,
            commands::writing_export,
            // v0.5: work.
            commands::work_create_task,
            commands::work_get_task,
            commands::work_list_tasks,
            commands::work_set_status,
            commands::work_update_task,
            commands::work_delete_task,
            commands::work_recommend_priority,
            commands::work_summarise_meeting,
            commands::work_start_timer,
            commands::work_stop_timer,
            commands::work_add_time,
            commands::work_active_timer,
            // v0.5: editor.
            commands::editor_read,
            commands::editor_write,
            commands::editor_list,
            commands::editor_workspace_root,
            commands::git_status,
            commands::git_log,
            commands::git_diff,
            commands::git_commit,
            // v0.5: OS.
            commands::os_clipboard_read,
            commands::os_clipboard_write,
            commands::os_shell_exec,
            commands::os_notify,
            // v0.5: sync.
            commands::sync_encrypt,
            commands::sync_decrypt,
            commands::sync_send,
            commands::sync_recv,
            commands::sync_ack,
            commands::sync_make_identity,
            // v1.3: DID identity.
            commands::generate_did,
            commands::resolve_did,
            // v1.3: skill audit.
            commands::skill_audit_list,
            commands::skill_audit_list_for_skill,
            // v1.3: chat_stream.
            commands::chat_stream,
            // v1.3: data export/import.
            commands::export_memories,
            commands::import_memories,
            // v1.3: device management.
            commands::list_devices,
            commands::revoke_device,
            // v1.3: WebChat share — feature-gated behind `channels`.
            #[cfg(feature = "channels")]
            commands::share_chat,
            // v1.3: ACL.
            commands::acl_set,
            commands::acl_list,
            commands::acl_remove,
            // v1.0.1 P0#12: API key (OS keychain).
            commands::set_api_key,
            commands::get_api_key,
            commands::delete_api_key,
            // v1.2: channel (message bridge) — feature-gated.
            #[cfg(feature = "channels")]
            commands::channel_status,
            #[cfg(feature = "channels")]
            commands::channel_send,
            #[cfg(feature = "channels")]
            commands::channel_poll,
            #[cfg(feature = "channels")]
            commands::channel_ping,
            // v1.1 P1-4: security scan.
            commands::injection_scan,
            commands::sandbox_config,
            // v1.1 P0-2: tool registry.
            commands::tool_list,
            commands::tool_invoke,
            // v1.3 P2-7: skill marketplace.
            commands::marketplace_search,
            commands::marketplace_quick_search,
            commands::marketplace_install,
            commands::marketplace_check_updates,
            commands::marketplace_refresh,
            commands::marketplace_stats,
            commands::marketplace_tags,
            commands::marketplace_generate_manifest,
            // v1.3: MCP (feature-gated).
            #[cfg(feature = "mcp")]
            commands::mcp_list_servers,
            #[cfg(feature = "mcp")]
            commands::mcp_add_server,
            #[cfg(feature = "mcp")]
            commands::mcp_remove_server,
            #[cfg(feature = "mcp")]
            commands::mcp_list_tools,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Convenience wrapper for non-Tauri contexts (tests, CLI) that want the
/// same [`AppState`] wiring without spawning a window.
pub async fn build_state_for_tests(config: AppConfig) -> anyhow::Result<AppState> {
    AppState::bootstrap(config).await
}

/// Re-exported for convenience.
pub use memory::reflect::Reflection;
pub use memory::types::{Memory, MemoryLayer, MemoryType, MultiGranularity};
pub use memory::MigrationStatus;
pub use metrics::MetricsSnapshot;
