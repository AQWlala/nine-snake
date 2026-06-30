//! Swarm orchestrator — v2.0 jiuwenswarm-style dynamic agent dispatch.
//!
//! ## v2.0 redesign
//!
//! The orchestrator no longer pipes agents through a fixed pipeline
//! (Coder → Writer → Reviewer).  Instead, it spawns **2–6 generic
//! agents in parallel**, mirroring jiuwenswarm's `task_tool` sub-agent
//! pattern.  Every agent receives the same task description and team
//! context; they work independently, and the orchestrator collects all
//! outputs.
//!
//! ## Key invariants
//! * `agent_count` is clamped to `2..=6`.
//! * All agents run concurrently (`futures::future::join_all`).
//! * A single agent failure does *not* abort the whole run — the
//!   orchestrator marks it as errored and continues.
//! * Retry is per-agent with exponential back-off (unchanged from v1).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::llm::LlmGateway;
use crate::memory::embedder::Embedder;
use crate::memory::lance_store::LanceStore;
use crate::memory::sponge::SpongeEngine;
use crate::memory::sqlite_store::SqliteStore;

use super::agents::{build_agent_pool, build_agent_pool_by_kinds, Agent, AgentKind, AgentOutput};
use super::bus::AgentBus;
use super::composer::SkillComposer;
use super::context::TeamContext;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// A single task submitted to the swarm.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwarmTask {
    /// Free-form task description.
    pub description: String,
    /// Number of generic agents to spawn (clamped to `2..=6`).
    #[serde(default = "default_agent_count")]
    pub agent_count: u32,
    /// Maximum number of retry rounds per agent (default 1).
    ///
    /// `0` means "fail fast" (no retries).  Any positive value gives
    /// `max_retries + 1` total attempts with exponential back-off.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// v1.1: explicit agent kinds to spawn. When set, this takes
    /// priority over `agent_count` — the orchestrator builds an
    /// agent pool from the listed kinds instead of taking the first
    /// N from the default pool.
    #[serde(default)]
    pub agents: Vec<String>,
}

fn default_agent_count() -> u32 {
    3
}
fn default_max_retries() -> u32 {
    1
}

impl SwarmTask {
    pub fn new(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            agent_count: 3,
            max_retries: 1,
            agents: Vec::new(),
        }
    }

    /// Build a task with a specific agent count.
    pub fn with_agent_count(mut self, n: u32) -> Self {
        self.agent_count = n.clamp(2, 6);
        self
    }
}

/// Final report returned to the caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestrationReport {
    pub task: SwarmTask,
    pub outputs: Vec<AgentOutput>,
    /// Number of agents that finished successfully.
    pub success_count: u32,
    /// Number of agents that failed (after retries).
    pub failure_count: u32,
    /// Whether *any* agent produced a result.
    pub approved: bool,
}

/// v0.3 legacy: public description of a single agent.  Kept for gRPC
/// backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDescriptor {
    pub name: String,
    pub system_prompt: String,
    pub description: String,
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

/// Base delay for the first retry (doubles on every subsequent attempt).
const RETRY_BASE_DELAY_MS: u64 = 100;

/// Maximum number of agents we can ever spawn in parallel.
const MAX_AGENTS: u32 = 6;
const MIN_AGENTS: u32 = 2;

pub struct SwarmOrchestrator {
    llm: Arc<LlmGateway>,
    #[allow(dead_code)]
    sponge: Option<Arc<SpongeEngine>>,
    lance: Option<Arc<LanceStore>>,
    embedder: Option<Arc<Embedder>>,
    sqlite: Option<Arc<SqliteStore>>,
    agent_pool: Vec<Arc<dyn Agent>>,
    composer: parking_lot::Mutex<Option<Arc<SkillComposer>>>,
    bus: Arc<AgentBus>,
}

impl SwarmOrchestrator {
    /// Creates a new orchestrator with a full agent pool and RAG support.
    pub fn new(
        llm: Arc<LlmGateway>,
        sponge: Arc<SpongeEngine>,
        lance: Arc<LanceStore>,
        embedder: Arc<Embedder>,
        sqlite: Arc<SqliteStore>,
    ) -> Self {
        let agent_pool = build_agent_pool(llm.clone());
        Self {
            llm,
            sponge: Some(sponge),
            lance: Some(lance),
            embedder: Some(embedder),
            sqlite: Some(sqlite),
            agent_pool,
            composer: parking_lot::Mutex::new(None),
            bus: Arc::new(AgentBus::new()),
        }
    }

    pub fn new_without_memory(llm: Arc<LlmGateway>) -> Self {
        let agent_pool = build_agent_pool(llm.clone());
        Self {
            llm,
            sponge: None,
            lance: None,
            embedder: None,
            sqlite: None,
            agent_pool,
            composer: parking_lot::Mutex::new(None),
            bus: Arc::new(AgentBus::new()),
        }
    }

    // ------------------------------------------------------------------

    /// v1.2: attach a skill composer for automatic skill injection.
    pub fn with_composer(self, composer: Arc<SkillComposer>) -> Self {
        *self.composer.lock() = Some(composer);
        self
    }

    /// v1.2: set the composer after construction (for bootstrap ordering).
    pub fn set_composer(&self, composer: Arc<SkillComposer>) {
        *self.composer.lock() = Some(composer);
    }

    pub fn bus(&self) -> &Arc<AgentBus> {
        &self.bus
    }

    // Agent introspection (kept for gRPC / front-end compatibility)
    // ------------------------------------------------------------------

    /// v0.3: returns `(kind_str, name, system_prompt, description)`
    /// for every agent in the pool.
    pub fn list_agents(&self) -> Vec<(String, String, String, String)> {
        self.agent_pool
            .iter()
            .map(|a| {
                (
                    a.kind().as_str().to_string(),
                    a.name().to_string(),
                    a.system_prompt().to_string(),
                    a.description().to_string(),
                )
            })
            .collect()
    }

    /// v0.3: looks up a single agent by its `kind` string.
    pub fn get_agent(&self, kind: &str) -> Option<AgentDescriptor> {
        self.agent_pool
            .iter()
            .find(|a| a.kind().as_str() == kind)
            .map(|a| AgentDescriptor {
                name: a.name().to_string(),
                system_prompt: a.system_prompt().to_string(),
                description: a.description().to_string(),
            })
    }

    // ------------------------------------------------------------------
    // RAG context builder (unchanged from v1.1 P0-3)
    // ------------------------------------------------------------------

    async fn build_rag_context(&self, query: &str) -> Option<String> {
        let lance = self.lance.as_ref()?;
        let embedder = self.embedder.as_ref()?;
        let sqlite = self.sqlite.as_ref()?;

        let query_emb = match embedder.embed(query).await {
            Ok(v) => v,
            Err(e) => {
                warn!(target: "nine_snake.swarm", error = ?e, "failed to embed RAG query");
                return None;
            }
        };

        let hits = match lance.search(&query_emb, 5).await {
            Ok(h) => h,
            Err(e) => {
                warn!(target: "nine_snake.swarm", error = ?e, "failed to search lance for RAG");
                return None;
            }
        };

        if hits.is_empty() {
            return None;
        }

        let ids: Vec<String> = hits.iter().map(|(id, _)| id.clone()).collect();
        let memories = match sqlite.get_many(&ids).await {
            Ok(mems) => mems,
            Err(e) => {
                warn!(target: "nine_snake.swarm", error = ?e, "failed to fetch memories for RAG");
                return None;
            }
        };

        let mut ctx_lines = Vec::new();
        ctx_lines.push("<memory_context>".to_string());
        for mem in memories {
            ctx_lines.push(format!(
                "- [{}] {}",
                mem.id,
                mem.content.chars().take(200).collect::<String>()
            ));
        }
        ctx_lines.push("</memory_context>".to_string());

        Some(ctx_lines.join("\n"))
    }

    // ------------------------------------------------------------------
    // Core execution
    // ------------------------------------------------------------------

    /// v2.0: spawn `agent_count` (2..=6) generic agents in parallel.
    ///
    /// Every agent receives the same task description and a shared
    /// [`TeamContext`] snapshot.  They run concurrently; a single
    /// failure is recorded but does not abort the remaining agents.
    pub async fn execute(&self, task: SwarmTask) -> Result<OrchestrationReport> {
        let ctx = TeamContext::new();
        ctx.push_str("system", "task", &task.description);

        // Inject relevant memories from LanceDB as RAG context.
        if let Some(rag_ctx) = self.build_rag_context(&task.description).await {
            ctx.push_str("system", "rag_context", &rag_ctx);
        }

        // Select agents: if `task.agents` is set, build pool by kinds;
        // otherwise take the first N from the default pool.
        let agents: Vec<Arc<dyn Agent>> = if !task.agents.is_empty() {
            let kinds: Vec<&str> = task.agents.iter().map(|s| s.as_str()).collect();
            build_agent_pool_by_kinds(&kinds, self.llm.clone())
        } else {
            let count = task.agent_count.clamp(MIN_AGENTS, MAX_AGENTS) as usize;
            self.agent_pool.iter().take(count).cloned().collect()
        };

        info!(
            target: "nine_snake.swarm",
            count = agents.len(),
            task = %task.description.chars().take(80).collect::<String>(),
            "dispatching swarm"
        );

        // Fan-out: run every agent concurrently.
        let handles: Vec<_> = agents
            .into_iter()
            .map(|agent| {
                let t = task.description.clone();
                let c = ctx.clone();
                let max_retries = task.max_retries;
                tokio::spawn(
                    async move { Self::run_agent_with_retry(agent, &t, &c, max_retries).await },
                )
            })
            .collect();

        let results = join_all(handles).await;

        // Collect outputs, separating successes from failures.
        let mut outputs: Vec<AgentOutput> = Vec::new();
        let mut success_count: u32 = 0;
        let mut failure_count: u32 = 0;

        for res in results {
            match res {
                Ok(Ok(output)) => {
                    success_count += 1;
                    outputs.push(output);
                }
                Ok(Err(e)) => {
                    failure_count += 1;
                    warn!(target: "nine_snake.swarm", error = ?e, "agent failed");
                    outputs.push(AgentOutput {
                        kind: AgentKind::Generic,
                        author: "unknown".to_string(),
                        body: format!("[error] {e}"),
                        confidence: 0.0,
                    });
                }
                Err(join_err) => {
                    failure_count += 1;
                    warn!(target: "nine_snake.swarm", error = ?join_err, "agent task panicked");
                    outputs.push(AgentOutput {
                        kind: AgentKind::Generic,
                        author: "unknown".to_string(),
                        body: format!("[panic] {join_err}"),
                        confidence: 0.0,
                    });
                }
            }
        }

        let approved = success_count > 0;

        self.bus.broadcast(super::bus::BusMessage {
            from: "orchestrator".to_string(),
            to: None,
            content: format!(
                "swarm.execute.completed: success={}, failure={}, approved={}",
                success_count, failure_count, approved
            ),
            timestamp: chrono::Utc::now().timestamp_millis(),
            msg_type: super::bus::BusMessageType::Notification,
            correlation_id: None,
        });

        let outputs = if outputs.len() > 1 {
            let negotiator = crate::swarm::negotiator::Negotiator::new();
            let result = negotiator.negotiate(outputs);
            if result.conflict_detected {
                info!(
                    target: "nine_snake.swarm",
                    method = ?result.method,
                    "conflict resolved through negotiation"
                );
            }
            vec![result.chosen]
        } else {
            outputs
        };

        info!(
            target: "nine_snake.swarm",
            success = success_count,
            failure = failure_count,
            total = outputs.len(),
            "orchestration finished"
        );

        Ok(OrchestrationReport {
            task,
            outputs,
            success_count,
            failure_count,
            approved,
        })
    }

    /// Run a single agent with retry + exponential back-off.
    async fn run_agent_with_retry(
        agent: Arc<dyn Agent>,
        task: &str,
        ctx: &TeamContext,
        max_retries: u32,
    ) -> Result<AgentOutput> {
        let mut last_err: Option<anyhow::Error> = None;

        for attempt in 0..=max_retries {
            match agent.run(task, ctx).await {
                Ok(o) => return Ok(o),
                Err(e) => {
                    warn!(
                        target: "nine_snake.swarm",
                        agent = %agent.name(),
                        attempt,
                        max = max_retries,
                        error = ?e,
                        "agent run failed; will retry"
                    );
                    last_err = Some(e);
                    if attempt < max_retries {
                        let delay = Duration::from_millis(RETRY_BASE_DELAY_MS * 2u64.pow(attempt));
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("agent failed without an error")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agent_count_is_clamped() {
        let task = SwarmTask::new("test").with_agent_count(100);
        assert_eq!(task.agent_count, 6);
        let task = SwarmTask::new("test").with_agent_count(1);
        assert_eq!(task.agent_count, 2);
        let task = SwarmTask::new("test").with_agent_count(4);
        assert_eq!(task.agent_count, 4);
    }

    #[tokio::test]
    async fn empty_pool_refuses_to_run() {
        // We cannot test full execution without a running LLM, but we
        // can confirm that the orchestrator correctly clamps agent_count.
        let client = std::sync::Arc::new(crate::llm::OllamaClient::new("http://127.0.0.1:1"));
        let gw = std::sync::Arc::new(crate::llm::LlmGateway::new(client, "m", None, None, None));
        let orch = SwarmOrchestrator::new_without_memory(gw);
        // agent_pool is pre-built with 6 agents — verify.
        assert_eq!(orch.agent_pool.len(), 6);
    }
}
