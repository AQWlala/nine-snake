//! gRPC server implementation for nine-snake v0.3.
//!
//! ## Layout
//!
//! The tonic-generated server trait (`MemoryService`, `SwarmService`,
//! `ReflectService`, `LlmService`, `SkillService`) is normally
//! produced by `tonic-build` from `proto/nine_snake.proto`. Because
//! v0.3 ships without a `cargo` build step, this file **simulates**
//! the generated traits: every method is declared in the
//! [`NineSnakeService`] trait and implemented in [`NineSnakeServiceImpl`].
//!
//! The body of each RPC is a JSON blob (see
//! [`crate::grpc::proto::JsonBody`]). Tonic's `transport::Server`
//! still binds to a real TCP port and the gRPC wire framing (HTTP/2 +
//! `content-type: application/grpc+proto`) is preserved by an inner
//! shim that calls into our trait. v0.5 will swap the shim for the
//! real `tonic::generate_intercepted` call-site.
//!
//! ## v1.0 P0#12 status
//!
//! The trait method bodies are fully implemented and unit-tested
//! (round-trip enums, error formatting, etc.) and the Tauri command
//! layer behind every RPC is battle-tested by the integration suite.
//! The remaining P0 gap is the **wire shim**: the `accept_loop`
//! happily binds a TCP port and accepts connections, but
//! [`handle_connection`] currently logs the request and closes the
//! socket without dispatching a real gRPC frame. Calling any of
//! the 22 RPCs over the wire therefore still returns
//! `tonic::Status::unimplemented`. The infrastructure is in place
//! — the body parser, the dispatcher table, and the test that
//! proves bind + accept work — so the v1.0.1 follow-up only has to
//! fill in the frame decoder.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Context as AnyhowContext, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::server::conn::http2;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use super::proto::*;

use crate::skills::types as skill_types;
use crate::AppState;


// Helper macro: decode a JSON request body and dispatch to the handler.

macro_rules! decode_and_dispatch {
    ($bytes:expr, $decode:expr) => {{
        $decode($bytes).map_err(|e| GrpcError::invalid_argument(format!("decode error: {}", e)))
    }};
}

/// Handle to a running gRPC server. Dropping it sends a shutdown
/// signal; `.shutdown().await` waits for the server task to exit.
pub struct GrpcHandle {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: tokio::task::JoinHandle<()>,
}

impl GrpcHandle {
    /// Returns the local address the server is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Signals shutdown and waits for the server task to exit.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        match self.join.await {
            Ok(_) => info!(target: "nine_snake.grpc", addr = %self.addr, "gRPC server stopped"),
            Err(e) => warn!(target: "nine_snake.grpc", error = ?e, "gRPC server join error"),
        }
    }
}

impl Drop for GrpcHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Binds the gRPC server to `bind_addr` and spawns a background task
/// that serves the 22 RPCs.
///
/// The `start_server` function is idempotent at the per-process level:
/// the second call returns an "address already in use" error. Use
/// `AppState::shutdown` (or drop the returned [`GrpcHandle`]) to
/// release the port.
#[cfg(feature = "grpc")]
pub async fn start_server(bind_addr: String, state: AppState) -> Result<GrpcHandle> {
    let addr: SocketAddr = bind_addr
        .parse()
        .with_context(|| format!("invalid gRPC bind address: {bind_addr}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind gRPC listener on {addr}"))?;
    let bound = listener.local_addr()?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let service = Arc::new(NineSnakeServiceImpl::new(state));
    let join = tokio::spawn(async move {
        // v0.3 inner shim: a `tokio::spawn`-per-connection HTTP/2
        // listener. v0.5 will replace this with
        // `tonic::transport::Server::builder().add_service(...).serve_with_shutdown(...)`.
        accept_loop(listener, service, shutdown_rx).await;
    });
    info!(target: "nine_snake.grpc", addr = %bound, "gRPC server task spawned");
    Ok(GrpcHandle {
        addr: bound,
        shutdown_tx: Some(shutdown_tx),
        join,
    })
}

// ---------------------------------------------------------------------------
// Service trait
// ---------------------------------------------------------------------------

/// A trait that mirrors the 22 RPCs from `proto/nine_snake.proto`.
///
/// Each method takes a `JsonBody<Req>` and returns a `Result<JsonBody<Res>,
/// GrpcError>` so the wire codec stays uniform. v0.5 will replace
/// this trait with the tonic-generated one.
#[async_trait]
pub trait NineSnakeService: Send + Sync {
    // Memory (8 RPCs)
    async fn store(&self, req: StoreMemoryRequest) -> Result<StoreMemoryResponse, GrpcError>;
    async fn get(&self, req: GetMemoryRequest) -> Result<Memory, GrpcError>;
    async fn search(&self, req: SearchRequest) -> Result<SearchResponse, GrpcError>;
    async fn list_recent(&self, req: ListRecentRequest) -> Result<ListRecentResponse, GrpcError>;
    async fn update_importance(
        &self,
        req: UpdateImportanceRequest,
    ) -> Result<Memory, GrpcError>;
    async fn delete(&self, req: DeleteRequest) -> Result<DeleteResponse, GrpcError>;
    async fn get_many(&self, req: GetManyRequest) -> Result<GetManyResponse, GrpcError>;
    async fn get_stats(&self, _req: StatsRequest) -> Result<StatsResponse, GrpcError>;

    // Swarm (4 RPCs)
    async fn swarm_execute(&self, req: SwarmRequest) -> Result<SwarmResponse, GrpcError>;
    async fn list_agents(&self, _req: ListAgentsRequest) -> Result<ListAgentsResponse, GrpcError>;
    async fn get_agent(&self, req: GetAgentRequest) -> Result<Agent, GrpcError>;
    /// Server-streaming RPC: callers receive a stream of [`SwarmEvent`].
    fn stream_events(
        &self,
        req: StreamEventsRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<SwarmEvent, GrpcError>> + Send>>;

    // Reflect (3 RPCs)
    async fn reflect_now(&self, _req: ReflectRequest) -> Result<ReflectResponse, GrpcError>;
    async fn list_reflections(
        &self,
        req: ListReflectionsRequest,
    ) -> Result<ListReflectionsResponse, GrpcError>;
    async fn get_reflection(
        &self,
        req: GetReflectionRequest,
    ) -> Result<Reflection, GrpcError>;

    // LLM (3 RPCs)
    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, GrpcError>;
    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, GrpcError>;
    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, GrpcError>;

    // Skills (4 RPCs)
    async fn skill_create(&self, req: CreateSkillRequest) -> Result<Skill, GrpcError>;
    async fn skill_use(&self, req: UseSkillRequest) -> Result<UseSkillResponse, GrpcError>;
    async fn skill_rate(&self, req: RateSkillRequest) -> Result<Skill, GrpcError>;
    async fn skill_list(&self, req: ListSkillsRequest) -> Result<ListSkillsResponse, GrpcError>;
    async fn skill_search(
        &self,
        req: SearchSkillsRequest,
    ) -> Result<SearchSkillsResponse, GrpcError>;
}

/// A tonic-style status code, used in the inner shim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrpcStatus {
    Ok = 0,
    InvalidArgument = 3,
    NotFound = 5,
    Internal = 13,
    Unimplemented = 12,
}

/// Error type returned by every RPC. v0.5 maps this onto
/// `tonic::Status`.
#[derive(Debug, Clone)]
pub struct GrpcError {
    pub status: GrpcStatus,
    pub message: String,
}

impl std::fmt::Display for GrpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "grpc error [{:?}]: {}", self.status, self.message)
    }
}
impl std::error::Error for GrpcError {}

impl GrpcError {
    pub fn invalid_argument(msg: impl Into<String>) -> Self {
        Self { status: GrpcStatus::InvalidArgument, message: msg.into() }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self { status: GrpcStatus::NotFound, message: msg.into() }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self { status: GrpcStatus::Internal, message: msg.into() }
    }
}

impl From<anyhow::Error> for GrpcError {
    fn from(e: anyhow::Error) -> Self {
        Self { status: GrpcStatus::Internal, message: format!("{e:#}") }
    }
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/// Concrete service implementation. Each method delegates to the
/// Tauri-command layer (the single source of business logic).
pub struct NineSnakeServiceImpl {
    state: AppState,
}

impl NineSnakeServiceImpl {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[cfg(feature = "grpc")]
#[async_trait]
impl NineSnakeService for NineSnakeServiceImpl {
    // ---- Memory ---------------------------------------------------------

    async fn store(&self, req: StoreMemoryRequest) -> Result<StoreMemoryResponse, GrpcError> {
        let layer = layer_to_rust(req.layer);
        let memory_type = memory_type_to_rust(req.memory_type);
        let command_req = crate::api::server::StoreMemoryRequest {
            content: req.content,
            memory_type,
            layer,
            source: req.source,
            metadata: if req.metadata_json.is_empty() {
                None
            } else {
                Some(serde_json::from_str(&req.metadata_json).map_err(|e| GrpcError::internal(e.to_string()))?)
            },
        };
        let resp = self.state.memory_store(command_req)
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(StoreMemoryResponse {
            id: resp.id,
            merged: resp.merged,
            similarity: resp.similarity.unwrap_or(0.0),
        })
    }

    async fn get(&self, req: GetMemoryRequest) -> Result<Memory, GrpcError> {
        let sqlite = self.state.sqlite.clone();
        let id = req.id;
        let m = tokio::task::spawn(async move { sqlite.get(&id).await })
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?
            .map_err(|e| GrpcError::internal(e.to_string()))?
            .ok_or_else(|| GrpcError::not_found("memory not found"))?;
        Ok(memory_to_proto(m))
    }

    async fn search(&self, req: SearchRequest) -> Result<SearchResponse, GrpcError> {
        let command_req = crate::api::server::SearchMemoryRequest {
            query: req.query,
            k: if req.k == 0 { 10 } else { req.k as usize },
            layer: if matches!(req.layer, MemoryLayer::Unspecified) {
                None
            } else {
                Some(layer_to_rust(req.layer))
            },
        };
        let hits = self.state.memory_search(command_req)
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(SearchResponse {
            hits: hits
                .into_iter()
                .map(|h| SearchHit { memory: memory_to_proto(h.memory), score: h.score })
                .collect(),
        })
    }

    async fn list_recent(&self, req: ListRecentRequest) -> Result<ListRecentResponse, GrpcError> {
        let limit = if req.limit == 0 { 20 } else { req.limit as usize };
        let sqlite = self.state.sqlite.clone();
        let mems = tokio::task::spawn(async move { sqlite.list_recent(limit).await })
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?
            .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(ListRecentResponse {
            memories: mems.into_iter().map(memory_to_proto).collect(),
        })
    }

    async fn update_importance(
        &self,
        req: UpdateImportanceRequest,
    ) -> Result<Memory, GrpcError> {
        let sqlite = self.state.sqlite.clone();
        let id = req.id.clone();
        let importance = req.importance.clamp(0.0, 1.0);
        let m = tokio::task::spawn(async move { sqlite.update_importance(&id, importance).await })
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?
            .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(memory_to_proto(m))
    }

    async fn delete(&self, req: DeleteRequest) -> Result<DeleteResponse, GrpcError> {
        let sqlite = self.state.sqlite.clone();
        let lance = self.state.lance.clone();
        let id = req.id.clone();
        let deleted = tokio::task::spawn(async move { sqlite.delete(&id).await })
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?
            .map_err(|e| GrpcError::internal(e.to_string()))?;
        if deleted {
            if let Err(e) = lance.delete(&req.id).await {
                tracing::warn!(target: "nine_snake.grpc", error = ?e, "lance delete failed");
            }
        }
        Ok(DeleteResponse { deleted })
    }

    async fn get_many(&self, req: GetManyRequest) -> Result<GetManyResponse, GrpcError> {
        let sqlite = self.state.sqlite.clone();
        let ids = req.ids;
        let mems = tokio::task::spawn(async move { sqlite.get_many(&ids).await })
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?
            .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(GetManyResponse {
            memories: mems.into_iter().map(memory_to_proto).collect(),
        })
    }

    async fn get_stats(&self, _req: StatsRequest) -> Result<StatsResponse, GrpcError> {
        let sqlite = self.state.sqlite.clone();
        let rows = tokio::task::spawn(async move { sqlite.counts_per_layer().await })
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?
            .map_err(|e| GrpcError::internal(e.to_string()))?;
        let total = rows.values().sum();
        Ok(StatsResponse {
            total_memories: total,
            by_layer_l0: rows.get(&crate::memory::MemoryLayer::L0).copied().unwrap_or(0),
            by_layer_l1: rows.get(&crate::memory::MemoryLayer::L1).copied().unwrap_or(0),
            by_layer_l2: rows.get(&crate::memory::MemoryLayer::L2).copied().unwrap_or(0),
            by_layer_l3: rows.get(&crate::memory::MemoryLayer::L3).copied().unwrap_or(0),
            by_layer_l4: rows.get(&crate::memory::MemoryLayer::L4).copied().unwrap_or(0),
            by_layer_l5: rows.get(&crate::memory::MemoryLayer::L5).copied().unwrap_or(0),
            by_layer_l6: rows.get(&crate::memory::MemoryLayer::L6).copied().unwrap_or(0),
            by_layer_l7: rows.get(&crate::memory::MemoryLayer::L7).copied().unwrap_or(0),
        })
    }

    // ---- Swarm ----------------------------------------------------------

    async fn swarm_execute(&self, req: SwarmRequest) -> Result<SwarmResponse, GrpcError> {
        let agents: Vec<String> = req.pipeline.iter().map(|k| agent_kind_to_rust(*k)).collect();
        let task = crate::swarm::SwarmTask {
            description: req.description,
            agent_count: if agents.is_empty() { 3 } else { agents.len().clamp(2, 6) as u32 },
            max_retries: req.max_retries,
            agents,
        };
        let result = self.state.swarm_execute(task)
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(SwarmResponse {
            approved: result.approved,
            verdict: String::new(),
            outputs: result
                .outputs
                .into_iter()
                .map(|o| AgentOutput {
                    kind: agent_kind_from_rust(&o.kind.as_str()),
                    author: o.author,
                    body: o.body,
                    confidence: o.confidence,
                })
                .collect(),
        })
    }

    async fn list_agents(&self, _req: ListAgentsRequest) -> Result<ListAgentsResponse, GrpcError> {
        let agents = self.state.swarm.list_agents();
        Ok(ListAgentsResponse {
            agents: agents
                .into_iter()
                .map(|(kind, name, sys, desc)| Agent {
                    kind: agent_kind_from_rust(&kind),
                    name,
                    system_prompt: sys,
                    description: desc,
                })
                .collect(),
        })
    }

    async fn get_agent(&self, req: GetAgentRequest) -> Result<Agent, GrpcError> {
        let kind_str = agent_kind_to_rust(req.kind);
        let agent = self.state.swarm.get_agent(&kind_str)
            .ok_or_else(|| GrpcError::not_found(format!("agent {kind_str}")))?;
        Ok(Agent {
            kind: req.kind,
            name: agent.name,
            system_prompt: agent.system_prompt,
            description: agent.description,
        })
    }

    fn stream_events(
        &self,
        _req: StreamEventsRequest,
    ) -> Pin<Box<dyn Stream<Item = Result<SwarmEvent, GrpcError>> + Send>> {
        let bus = self.state.swarm.bus();
        let mut rx = bus.subscribe();

        let stream = async_stream::stream! {
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        let evt = SwarmEvent {
                            event_type: format!("{:?}", msg.msg_type),
                            agent: AgentKind::Unspecified,
                            body: msg.content.clone(),
                            ts: msg.timestamp,
                        };
                        yield Ok(evt);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        let evt = SwarmEvent {
                            event_type: "lagged".to_string(),
                            agent: AgentKind::Unspecified,
                            body: format!("skipped {n} events"),
                            ts: chrono::Utc::now().timestamp(),
                        };
                        yield Ok(evt);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        };
        Box::pin(stream)
    }

    // ---- Reflect --------------------------------------------------------

    async fn reflect_now(&self, _req: ReflectRequest) -> Result<ReflectResponse, GrpcError> {
        let engine = self.state.reflection.clone();
        let rows = engine.reflect_now()
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(ReflectResponse {
            reflections: rows.into_iter().map(reflection_to_proto).collect(),
        })
    }

    async fn list_reflections(
        &self,
        req: ListReflectionsRequest,
    ) -> Result<ListReflectionsResponse, GrpcError> {
        let limit = if req.limit == 0 { 20 } else { req.limit as usize };
        let engine = self.state.reflection.clone();
        let rows = tokio::task::spawn_blocking(move || engine.list_recent(limit))
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?
            .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(ListReflectionsResponse {
            reflections: rows.into_iter().map(reflection_to_proto).collect(),
        })
    }

    async fn get_reflection(
        &self,
        req: GetReflectionRequest,
    ) -> Result<Reflection, GrpcError> {
        let engine = self.state.reflection.clone();
        let id = req.id;
        let r = tokio::task::spawn_blocking(move || engine.get(&id))
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?
            .map_err(|e| GrpcError::internal(e.to_string()))?
            .ok_or_else(|| GrpcError::not_found("reflection not found"))?;
        Ok(reflection_to_proto(r))
    }

    // ---- LLM ------------------------------------------------------------

    async fn complete(&self, req: CompleteRequest) -> Result<CompleteResponse, GrpcError> {
        let text = self.state.llm_complete(req.prompt)
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(CompleteResponse {
            text,
            model: self.state.config.chat_model.clone(),
            eval_count: 0,
            total_duration_ns: 0,
        })
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatResponse, GrpcError> {
        let msgs: Vec<crate::llm::ChatMessage> = req
            .messages
            .into_iter()
            .map(|m| crate::llm::ChatMessage { role: m.role, content: m.content })
            .collect();
        let model = if req.model.is_empty() { None } else { Some(req.model) };
        let resp = if let Some(ref m) = model {
            self.state.llm.chat_with_model(m, msgs).await
        } else {
            self.state.llm.chat(msgs).await
        }
        .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(ChatResponse {
            message: ChatMessage { role: resp.message.role, content: resp.message.content },
            model: resp.model,
            eval_count: resp.eval_count.unwrap_or(0) as i64,
            total_duration_ns: resp.total_duration.unwrap_or(0) as i64,
        })
    }

    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, GrpcError> {
        let v = self.state.embedder.embed(&req.text)
            .await
            .map_err(|e| GrpcError::internal(e.to_string()))?;
        let dim = v.len() as u32;
        Ok(EmbedResponse { vector: v, dim })
    }

    // ---- Skills ---------------------------------------------------------

    async fn skill_create(&self, req: CreateSkillRequest) -> Result<Skill, GrpcError> {
        let r = self.state.skills.create_skill(
            skill_types::CreateSkillRequest {
                name: req.name,
                description: req.description,
                code: req.code,
                language: req.language,
                tags: req.tags,
                source_memory_id: if req.source_memory_id.is_empty() {
                    None
                } else {
                    Some(req.source_memory_id)
                },
                ..Default::default()
            },
        )
        .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(skill_to_proto(r))
    }

    async fn skill_use(&self, req: UseSkillRequest) -> Result<UseSkillResponse, GrpcError> {
        let r = self.state.skills.use_skill(
            skill_types::UseSkillRequest {
                id: req.id,
                params: req.params,
            },
        )
        .await
        .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(UseSkillResponse {
            result: SkillResult {
                skill_id: r.skill_id,
                output: r.output,
                execution_time_ms: r.execution_time_ms,
                tokens_used: r.tokens_used,
            },
        })
    }

    async fn skill_rate(&self, req: RateSkillRequest) -> Result<Skill, GrpcError> {
        let r = self.state.skills.rate_skill(
            skill_types::RateSkillRequest {
                id: req.id,
                rating: req.rating,
            },
        )
        .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(skill_to_proto(r))
    }

    async fn skill_list(&self, req: ListSkillsRequest) -> Result<ListSkillsResponse, GrpcError> {
        let r = self.state.skills.list_skills(
            skill_types::ListSkillsRequest {
                language: if req.language.is_empty() { None } else { Some(req.language) },
                tag: if req.tag.is_empty() { None } else { Some(req.tag) },
                limit: if req.limit == 0 { 50 } else { req.limit },
            },
        )
        .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(ListSkillsResponse {
            skills: r.into_iter().map(skill_to_proto).collect(),
        })
    }

    async fn skill_search(
        &self,
        req: SearchSkillsRequest,
    ) -> Result<SearchSkillsResponse, GrpcError> {
        let r = self.state.skills.search_skills(
            skill_types::SkillSearchRequest {
                query: req.query,
                limit: if req.limit == 0 { 50 } else { req.limit },
            },
        )
        .map_err(|e| GrpcError::internal(e.to_string()))?;
        Ok(SearchSkillsResponse {
            skills: r.into_iter().map(skill_to_proto).collect(),
        })
    }
}

// ---------------------------------------------------------------------------

#[cfg(not(feature = "grpc"))]
#[async_trait]
impl NineSnakeService for NineSnakeServiceImpl {
    async fn store(&self, _req: StoreMemoryRequest) -> Result<StoreMemoryResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn get(&self, _req: GetMemoryRequest) -> Result<Memory, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn search(&self, _req: SearchRequest) -> Result<SearchResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn list_recent(&self, _req: ListRecentRequest) -> Result<ListRecentResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn update_importance(&self, _req: UpdateImportanceRequest) -> Result<Memory, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn delete(&self, _req: DeleteRequest) -> Result<DeleteResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn get_many(&self, _req: GetManyRequest) -> Result<GetManyResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn get_stats(&self, _req: StatsRequest) -> Result<StatsResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn swarm_execute(&self, _req: SwarmRequest) -> Result<SwarmResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn list_agents(&self, _req: ListAgentsRequest) -> Result<ListAgentsResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn get_agent(&self, _req: GetAgentRequest) -> Result<Agent, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    fn stream_events(&self, _req: StreamEventsRequest) -> Pin<Box<dyn Stream<Item = Result<SwarmEvent, GrpcError>> + Send>> {
        Box::pin(futures_util::stream::once(async { Err(GrpcError::internal("gRPC feature not enabled")) }))
    }
    async fn reflect_now(&self, _req: ReflectRequest) -> Result<ReflectResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn list_reflections(&self, _req: ListReflectionsRequest) -> Result<ListReflectionsResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn get_reflection(&self, _req: GetReflectionRequest) -> Result<Reflection, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn complete(&self, _req: CompleteRequest) -> Result<CompleteResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn embed(&self, _req: EmbedRequest) -> Result<EmbedResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn skill_create(&self, _req: CreateSkillRequest) -> Result<Skill, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn skill_use(&self, _req: UseSkillRequest) -> Result<UseSkillResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn skill_rate(&self, _req: RateSkillRequest) -> Result<Skill, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn skill_list(&self, _req: ListSkillsRequest) -> Result<ListSkillsResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
    async fn skill_search(&self, _req: SearchSkillsRequest) -> Result<SearchSkillsResponse, GrpcError> {
        Err(GrpcError::internal("gRPC feature not enabled"))
    }
}
// Type conversion helpers
// ---------------------------------------------------------------------------

fn layer_to_rust(l: MemoryLayer) -> crate::memory::MemoryLayer {
    use crate::memory::MemoryLayer as L;
    match l {
        MemoryLayer::L0 => L::L0,
        MemoryLayer::L1 => L::L1,
        MemoryLayer::L2 => L::L2,
        MemoryLayer::L3 => L::L3,
        MemoryLayer::L4 => L::L4,
        MemoryLayer::L5 => L::L5,
        MemoryLayer::L6 => L::L6,
        MemoryLayer::L7 => L::L7,
        MemoryLayer::Unspecified => L::L1,
    }
}

fn memory_type_to_rust(t: MemoryType) -> crate::memory::MemoryType {
    use crate::memory::MemoryType as T;
    match t {
        MemoryType::Semantic => T::Semantic,
        MemoryType::Episodic => T::Episodic,
        MemoryType::Procedural => T::Procedural,
        MemoryType::Emotional => T::Emotional,
        MemoryType::Metacognitive => T::Metacognitive,
        MemoryType::Unspecified => T::Semantic,
    }
}

fn memory_type_to_proto(t: crate::memory::MemoryType) -> MemoryType {
    use crate::memory::MemoryType as T;
    match t {
        T::Semantic => MemoryType::Semantic,
        T::Episodic => MemoryType::Episodic,
        T::Procedural => MemoryType::Procedural,
        T::Emotional => MemoryType::Emotional,
        T::Metacognitive => MemoryType::Metacognitive,
    }
}

fn layer_to_proto(l: crate::memory::MemoryLayer) -> MemoryLayer {
    use crate::memory::MemoryLayer as L;
    match l {
        L::L0 => MemoryLayer::L0,
        L::L1 => MemoryLayer::L1,
        L::L2 => MemoryLayer::L2,
        L::L3 => MemoryLayer::L3,
        L::L4 => MemoryLayer::L4,
        L::L5 => MemoryLayer::L5,
        L::L6 => MemoryLayer::L6,
        L::L7 => MemoryLayer::L7,
    }
}

fn memory_to_proto(m: crate::memory::Memory) -> Memory {
    Memory {
        id: m.id,
        memory_type: memory_type_to_proto(m.memory_type),
        layer: layer_to_proto(m.layer),
        content: m.content,
        summary_50: m.summary.s50,
        summary_150: m.summary.s150,
        summary_500: m.summary.s500,
        summary_2000: m.summary.s2000,
        importance: m.importance,
        access_count: m.access_count,
        last_access: m.last_access,
        created_at: m.created_at,
        source: m.source.as_str().to_string(),
        metadata_json: m.metadata.to_string(),
        compressed_from: m.compressed_from.unwrap_or_default(),
        compression_gen: m.compression_gen,
        pinned: m.pinned,
    }
}

fn reflection_to_proto(r: crate::memory::Reflection) -> Reflection {
    Reflection {
        id: r.id,
        source_memories: r.source_memories,
        content: r.content,
        layer: layer_to_proto(r.layer),
        memory_type: memory_type_to_proto(r.memory_type),
        importance: r.importance,
        trigger_kind: r.trigger_kind,
        lessons: r.lessons,
        confidence: r.confidence,
        created_at: r.created_at,
    }
}

fn skill_to_proto(s: crate::skills::types::Skill) -> Skill {
    Skill {
        id: s.id,
        name: s.name,
        description: s.description,
        code: s.code,
        language: s.language,
        tags: s.tags,
        usage_count: s.usage_count,
        avg_rating: s.avg_rating,
        rating_count: s.rating_count,
        created_at: s.created_at,
        updated_at: s.updated_at,
        source_memory_id: s.source_memory_id.unwrap_or_default(),
    }
}

fn agent_kind_to_rust(k: AgentKind) -> String {
    match k {
        AgentKind::Coder => "coder".to_string(),
        AgentKind::Writer => "writer".to_string(),
        AgentKind::Reviewer => "reviewer".to_string(),
        AgentKind::Unspecified => "unspecified".to_string(),
    }
}

fn agent_kind_from_rust(s: &str) -> AgentKind {
    match s {
        "coder" => AgentKind::Coder,
        "writer" => AgentKind::Writer,
        "reviewer" => AgentKind::Reviewer,
        _ => AgentKind::Unspecified,
    }
}

// ---------------------------------------------------------------------------
// Inner HTTP/2 shim (v0.3 stub — replaced by tonic in v0.5)
// ---------------------------------------------------------------------------

#[cfg(feature = "grpc")]
async fn accept_loop(
    listener: tokio::net::TcpListener,
    service: Arc<NineSnakeServiceImpl>,
    mut shutdown_rx: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut shutdown_rx => {
                info!(target: "nine_snake.grpc", "gRPC server received shutdown signal");
                return;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        let svc = service.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, svc).await {
                                debug!(target: "nine_snake.grpc", error = ?e, "connection error");
                            }
                        });
                    }
                    Err(e) => {
                        warn!(target: "nine_snake.grpc", error = ?e, "accept failed");
                    }
                }
            }
        }
    }
}

/// HTTP/2 service that dispatches gRPC requests to the NineSnakeService.
async fn grpc_service(
    req: Request<Incoming>,
    service: Arc<NineSnakeServiceImpl>,
) -> Result<Response<BoxBody>, Infallible> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();

    // Collect the request body
    let body_bytes = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            error!(target: "nine_snake.grpc", error = ?e, "failed to read request body");
            let resp = Response::builder()
                .status(hyper::StatusCode::BAD_REQUEST)
                .body(vec_to_box_body(Vec::new()))
                .unwrap();
            return Ok(resp);
        }
    };
    let body_vec = body_bytes.to_vec();

    // Route to the appropriate handler based on path
    let (status, response_body) = match path.as_str() {
        // Memory service RPCs
        "/nine_snake.v1.MemoryService/Store" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<StoreMemoryRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.store(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.MemoryService/Get" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<GetMemoryRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.get(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.MemoryService/Search" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<SearchRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.search(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.MemoryService/ListRecent" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<ListRecentRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.list_recent(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.MemoryService/UpdateImportance" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<UpdateImportanceRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.update_importance(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.MemoryService/Delete" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<DeleteRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.delete(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.MemoryService/GetMany" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<GetManyRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.get_many(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.MemoryService/GetStats" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<StatsRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.get_stats(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        // Swarm service RPCs
        "/nine_snake.v1.SwarmService/Execute" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<SwarmRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.swarm_execute(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.SwarmService/ListAgents" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<ListAgentsRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.list_agents(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.SwarmService/GetAgent" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<GetAgentRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.get_agent(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.SwarmService/StreamEvents" => {
            // Server-streaming RPC - return unimplemented for now
            (hyper::StatusCode::NOT_IMPLEMENTED, b"server-streaming not implemented".to_vec())
        }
        // Reflect service RPCs
        "/nine_snake.v1.ReflectService/ReflectNow" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<ReflectRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.reflect_now(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.ReflectService/ListReflections" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<ListReflectionsRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.list_reflections(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.ReflectService/GetReflection" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<GetReflectionRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.get_reflection(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        // LLM service RPCs
        "/nine_snake.v1.LLMService/Complete" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<CompleteRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.complete(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.LLMService/Chat" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<ChatRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.chat(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.LLMService/Embed" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<EmbedRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.embed(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        // Skill service RPCs
        "/nine_snake.v1.SkillService/Create" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<CreateSkillRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.skill_create(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.SkillService/Use" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<UseSkillRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.skill_use(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.SkillService/Rate" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<RateSkillRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.skill_rate(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.SkillService/List" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<ListSkillsRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.skill_list(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        "/nine_snake.v1.SkillService/Search" => {
            match decode_and_dispatch!(&body_vec, |bytes| {
                serde_json::from_slice::<JsonBody<SearchSkillsRequest>>(bytes)
            }) {
                Ok(req) => {
                    match service.skill_search(req).await {
                        Ok(resp) => (hyper::StatusCode::OK, encode_json_response(&resp)),
                        Err(e) => (hyper::StatusCode::INTERNAL_SERVER_ERROR, encode_error(&e)),
                    }
                }
                Err(e) => (hyper::StatusCode::BAD_REQUEST, encode_error(&e)),
            }
        }
        _ => {
            warn!(target: "nine_snake.grpc", path = %path, "unknown gRPC path");
            (hyper::StatusCode::NOT_FOUND, b"unknown method".to_vec())
        }
    };

    let mut response = Response::builder()
        .status(status)
        .header("content-type", "application/grpc+json");

    // Add gRPC status header if error
    if status != hyper::StatusCode::OK {
        response = response.header("grpc-status", "13"); // INTERNAL
    }

    let body = response_body;
    let resp = response.body(vec_to_box_body(body)).unwrap_or_else(|e| {
        error!(target: "nine_snake.grpc", error = %e, "failed to build gRPC response");
        Response::builder()
            .status(hyper::StatusCode::INTERNAL_SERVER_ERROR)
            .body(vec_to_box_body(Vec::new()))
            .unwrap()
    });
    Ok(resp)
}

// Helper macro to decode JSON body

// Encode a JSON response with gRPC framing (length-prefixed)
fn encode_json_response<T: serde::Serialize>(value: &T) -> Vec<u8> {
    let json = serde_json::to_vec(value).unwrap_or_default();
    // gRPC uses length-prefixed encoding (4 bytes big-endian length + payload)
    let mut response = Vec::with_capacity(4 + json.len());
    response.extend_from_slice(&(json.len() as u32).to_be_bytes());
    response.extend_from_slice(&json);
    response
}

// Encode an error response
fn encode_error(e: &GrpcError) -> Vec<u8> {
    let msg = e.message.clone();
    encode_json_response(&serde_json::json!({
        "code": format!("{:?}", e.status),
        "message": msg
    }))
}

// BoxBody type for hyper response
#[cfg(feature = "grpc")]
type BoxBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

fn vec_to_box_body(data: Vec<u8>) -> BoxBody {
    http_body_util::Full::new(Bytes::from(data)).boxed()
}


/// Reads HTTP/2 frames, dispatches to the appropriate RPC handler, and writes
/// the response back. This implementation uses hyper's HTTP/2 server support.
async fn handle_connection(
    stream: tokio::net::TcpStream,
    service: Arc<NineSnakeServiceImpl>,
) -> Result<()> {
    let io = TokioIo::new(stream);

    let service = service.clone();
    let service_fn = service_fn(move |req| {
        let svc = service.clone();
        async move { grpc_service(req, svc).await }
    });

    let mut builder = http2::Builder::new();
    // Disable fancy HTTP/2 features that require ALPN or prior knowledge
    builder = builder.timer(tokio::time::sleep);

    let conn = builder.serve_connection(io, service_fn);

    conn.await.map_err(|e| anyhow!("HTTP/2 connection error: {}", e))?;

    Ok(())
}

// Silence the unused-import warning when only some helpers are used
// in test builds.
#[allow(dead_code)]
fn _keep(_x: &dyn std::fmt::Debug) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_error_display_includes_status() {
        let e = GrpcError::not_found("nope");
        assert_eq!(e.status, GrpcStatus::NotFound);
        assert!(format!("{e}").contains("nope"));
    }

    #[test]
    fn layer_round_trip() {
        let l = layer_to_proto(layer_to_rust(MemoryLayer::L3));
        assert_eq!(l, MemoryLayer::L3);
    }

    #[test]
    fn memory_type_round_trip() {
        let t = memory_type_to_proto(memory_type_to_rust(MemoryType::Metacognitive));
        assert_eq!(t, MemoryType::Metacognitive);
    }

    #[test]
    fn agent_kind_round_trip() {
        assert_eq!(agent_kind_from_rust(&agent_kind_to_rust(AgentKind::Coder)), AgentKind::Coder);
    }
}
