//! Tauri command handlers — the entry points invoked from the
//! front-end. Each command is a thin shim that translates a JSON DTO
//! into a call on the shared [`AppState`].
//!
//! ## v0.1 / v0.2
//! * Blocking SQLite I/O is funnelled through
//!   [`tokio::task::spawn_blocking`] so the Tauri runtime is never
//!   starved.
//! * Errors that escape the layer are mapped to a stable
//!   [`CommandError`] envelope (error code + safe message). The full
//!   chain is logged via `tracing::error!` for debugging.
//! * `chat` writes both the user prompt and the assistant reply to
//!   memory (L1 by default) so the sponge can absorb them.
//!
//! ## v0.3
//! * Skill CRUD commands (`skill_create`, `skill_use`, `skill_rate`,
//!   `skill_list`, `skill_search`).
//! * Memory read-by-id, update-importance, delete, get-many, stats.
//! * Swarm read commands (list-agents, get-agent).
//! * LLM `chat` and `embed` commands (previously only `complete`
//!   existed).
//! * Reflection read-by-id command.
//!
//! ## v1.0.2
//! * Commands split into logical submodules for maintainability.
//!   All public items are re-exported so `generate_handler!` paths
//!   (`commands::chat`, `commands::memory_store`, etc.) continue to
//!   resolve.

pub mod error;

// Submodules — each groups related commands and DTOs.
pub mod chat;
pub mod core;
pub mod editor;
pub mod llm;
pub mod memory;
pub mod os;
pub mod reflect;
pub mod skill;
pub mod swarm;
pub mod sync;
pub mod work;
pub mod writing;
// v1.2: channel commands — feature-gated behind `channels`.
#[cfg(feature = "channels")]
pub mod channel;
pub mod device;
pub mod export;
pub mod identity;
pub mod security;
// v1.3: WebChat share — feature-gated behind `channels`.
pub mod acl;
pub mod tool;
#[cfg(feature = "channels")]
pub mod webchat;

// Re-export the API DTOs so other modules (gRPC, tests) can reach them
// through the `commands` namespace without depending on the internal
// `api::server` module path.
pub mod api {
    pub use crate::api::server::{
        ChatRequestDto, NineSnakeService, SearchMemoryHit, SearchMemoryRequest, StoreMemoryRequest,
        StoreMemoryResponse,
    };
    pub use crate::skills::types::{
        CreateSkillRequest as CreateSkillDto, ListSkillsRequest as ListSkillsDto,
        RateSkillRequest as RateSkillDto, Skill as SkillDto, SkillResult as SkillResultDto,
        SkillSearchRequest as SkillSearchDto, UseSkillRequest as UseSkillDto,
    };
}

// Re-export all public items from submodules so that
// `commands::chat`, `commands::memory_store`, etc. still resolve
// for `generate_handler!` in `lib.rs`.
pub use acl::*;
#[cfg(feature = "channels")]
pub use channel::*;
pub use chat::*;
pub use core::*;
pub use device::*;
pub use editor::*;
pub use export::*;
pub use identity::*;
pub use llm::*;
pub use memory::*;
pub use os::*;
pub use reflect::*;
pub use security::*;
pub use skill::*;
pub use swarm::*;
pub use sync::*;
pub use tool::*;
#[cfg(feature = "channels")]
pub use webchat::*;
pub use work::*;
pub use writing::*;

pub use error::{CommandError, ErrorCode};

// ---------------------------------------------------------------------------
// Implementation of the service trait on `AppState`.
// ---------------------------------------------------------------------------

use crate::api::server::{
    ChatRequestDto, NineSnakeService, SearchMemoryHit, SearchMemoryRequest, StoreMemoryRequest,
    StoreMemoryResponse,
};
use crate::llm::ChatMessage;
use crate::memory::sponge::SpongeResult;
use crate::memory::types::Memory;
use crate::swarm::{OrchestrationReport, SwarmTask};
use crate::AppState;

#[async_trait::async_trait]
impl NineSnakeService for AppState {
    async fn chat(&self, req: ChatRequestDto) -> anyhow::Result<crate::llm::ChatResponse> {
        let mut msgs: Vec<ChatMessage> = Vec::new();
        if let Some(sys) = req.system.as_deref() {
            msgs.push(ChatMessage::system(sys));
        }
        msgs.push(ChatMessage::user(req.user_message));
        let resp = self.llm.chat(msgs).await?;
        Ok(resp)
    }

    async fn memory_store(&self, req: StoreMemoryRequest) -> anyhow::Result<StoreMemoryResponse> {
        let mut mem = Memory::new(req.memory_type, req.layer, req.content, req.source);
        if let Some(meta) = req.metadata {
            mem.metadata = meta;
        }
        match self.sponge.absorb(mem).await? {
            SpongeResult::Inserted { id } => Ok(StoreMemoryResponse {
                id,
                merged: false,
                similarity: None,
            }),
            SpongeResult::Merged { id, similarity } => Ok(StoreMemoryResponse {
                id,
                merged: true,
                similarity: Some(similarity),
            }),
            SpongeResult::Duplicate { id } => Ok(StoreMemoryResponse {
                id,
                merged: true,
                similarity: Some(1.0),
            }),
        }
    }

    async fn memory_search(
        &self,
        req: SearchMemoryRequest,
    ) -> anyhow::Result<Vec<SearchMemoryHit>> {
        let k = req.k.max(1);
        let query_emb = self.embedder.embed(&req.query).await?;
        let hits = self.lance.search(&query_emb, k).await?;
        if hits.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = hits.iter().map(|(id, _)| id.clone()).collect();
        let memories = self
            .sqlite
            .get_many(&ids)
            .await
            .map_err(|e| anyhow::anyhow!("get_many error: {e}"))?;

        let score_by_id: std::collections::HashMap<&str, f32> =
            hits.iter().map(|(id, s)| (id.as_str(), *s)).collect();
        let mut ordered: Vec<(Memory, f32)> = memories
            .into_iter()
            .filter_map(|m| score_by_id.get(m.id.as_str()).map(|s| (m, *s)))
            .collect();
        ordered.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let out = ordered
            .into_iter()
            .filter_map(|(m, s)| {
                if let Some(layer) = req.layer {
                    if m.layer != layer {
                        return None;
                    }
                }
                Some(SearchMemoryHit {
                    memory: m,
                    score: s,
                })
            })
            .collect();
        Ok(out)
    }

    async fn swarm_execute(&self, task: SwarmTask) -> anyhow::Result<OrchestrationReport> {
        self.swarm.execute(task).await
    }

    async fn llm_complete(&self, prompt: String) -> anyhow::Result<String> {
        self.llm.generate(&prompt).await
    }
}

// ---------------------------------------------------------------------------
// MCP commands (feature-gated) — kept in mod.rs because they are
// few and feature-gated; splitting them into a separate file would
// require the same `#[cfg(feature = "mcp")]` on the `pub mod mcp;`
// declaration, which is supported but adds complexity for only 4
// commands.
// ---------------------------------------------------------------------------

#[cfg(feature = "mcp")]
use crate::AppState;
#[cfg(feature = "mcp")]
use tauri::State;
#[cfg(feature = "mcp")]
use tracing::instrument;

#[cfg(feature = "mcp")]
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "mcp_list_servers"))]
pub async fn mcp_list_servers(state: State<'_, AppState>) -> Result<Vec<String>, CommandError> {
    Ok(state.mcp_manager.list_servers())
}

#[cfg(feature = "mcp")]
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "mcp_add_server"))]
pub async fn mcp_add_server(
    state: State<'_, AppState>,
    config: crate::mcp::config::McpServerConfig,
) -> Result<bool, CommandError> {
    state.mcp_manager.add_server(config);
    Ok(true)
}

#[cfg(feature = "mcp")]
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "mcp_remove_server"))]
pub async fn mcp_remove_server(
    state: State<'_, AppState>,
    name: String,
) -> Result<bool, CommandError> {
    state.mcp_manager.remove_server(&name);
    Ok(true)
}

#[cfg(feature = "mcp")]
#[tauri::command]
#[instrument(skip(state), fields(otel.kind = "mcp_list_tools"))]
pub async fn mcp_list_tools(
    state: State<'_, AppState>,
) -> Result<Vec<crate::mcp::client::McpTool>, CommandError> {
    Ok(state.mcp_manager.list_all_tools().await)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_error_validation_includes_code_and_safe_message() {
        let e = CommandError::validation("empty user_message");
        assert_eq!(e.code, ErrorCode::Validation);
        assert!(e.message.contains("empty user_message"));
        assert!(format!("{e}").contains("validation"));
    }

    #[test]
    fn command_error_internal_hides_internal_details() {
        let internal = anyhow::anyhow!("DB at /home/alice/.nine_snake/secret.db blew up");
        let e = CommandError::internal("memory_store", &internal);
        assert!(!e.message.contains("/home/alice"));
        assert!(!e.message.contains("secret"));
        assert_eq!(e.code, ErrorCode::Internal);
    }

    #[test]
    fn command_error_from_anyhow_is_internal() {
        let e: CommandError = anyhow::anyhow!("boom").into();
        assert_eq!(e.code, ErrorCode::Internal);
    }

    #[test]
    fn command_error_not_found() {
        let e = CommandError::not_found("memory");
        assert_eq!(e.code, ErrorCode::NotFound);
        assert!(e.message.contains("memory"));
    }

    #[test]
    fn command_error_memory_uses_memory_code() {
        let e = CommandError::memory("sponge", &anyhow::anyhow!("x"));
        assert_eq!(e.code, ErrorCode::Memory);
    }

    #[test]
    fn command_error_llm_uses_llm_code() {
        let e = CommandError::llm("chat", &anyhow::anyhow!("x"));
        assert_eq!(e.code, ErrorCode::Llm);
    }

    #[test]
    fn command_error_lance_uses_lance_code() {
        let e = CommandError::lance("search", &anyhow::anyhow!("x"));
        assert_eq!(e.code, ErrorCode::Lance);
    }

    #[test]
    fn command_error_db_uses_db_code() {
        let e = CommandError::db("open", &anyhow::anyhow!("x"));
        assert_eq!(e.code, ErrorCode::Db);
    }

    #[test]
    fn command_error_swarm_uses_swarm_code() {
        let e = CommandError::swarm("orchestrate", &anyhow::anyhow!("x"));
        assert_eq!(e.code, ErrorCode::Swarm);
    }

    #[test]
    fn swarm_agent_kind_parses_known_values() {
        use crate::swarm::agents::AgentKind;
        assert_eq!("coder".parse::<AgentKind>().unwrap(), AgentKind::Coder);
        assert_eq!("writer".parse::<AgentKind>().unwrap(), AgentKind::Writer);
        assert_eq!(
            "reviewer".parse::<AgentKind>().unwrap(),
            AgentKind::Reviewer
        );
        assert!("unknown".parse::<AgentKind>().is_err());
    }
}
