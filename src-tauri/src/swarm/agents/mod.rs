//! Concrete agent implementations.
//!
//! Every agent implements the [`Agent`] trait and reads from / writes
//! to a shared [`TeamContext`]. The trait is intentionally small so
//! new agents can be added without touching the orchestrator.
//!
//! ## v2.0 — jiuwenswarm-style dynamic agents
//!
//! Starting with v2.0, the swarm no longer uses hard-coded role agents
//! (Coder / Writer / Reviewer).  Instead, every task spawns 2–6
//! [`GenericAgent`] instances that work independently on the same task
//! description — mirroring jiuwenswarm's `task_tool` sub-agent pattern.
//! The old role agents are kept as deprecated modules for backward
//! compatibility with existing gRPC consumers.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::llm::LlmGateway;
use crate::memory::sponge::SpongeEngine;

use super::bus::BusMessage;
use super::context::TeamContext;

mod coder;
mod generic_agent;
mod planner;
mod researcher;
mod reviewer;
mod writer;

pub use coder::CoderAgent;
pub use generic_agent::GenericAgent;
pub use planner::PlannerAgent;
pub use researcher::ResearcherAgent;
pub use reviewer::ReviewerAgent;
pub use writer::WriterAgent;

/// Identifies a concrete agent implementation.
///
/// v2.0: `Generic` is the default for all new swarms.  `Coder`,
/// `Writer`, `Reviewer`, `Researcher`, and `Planner` are
/// **deprecated** and kept only for backward-compatible gRPC queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    /// v2.0: general-purpose task-driven agent (jiuwenswarm pattern).
    Generic,
    /// Deprecated — use Generic instead.
    Coder,
    /// Deprecated — use Generic instead.
    Writer,
    /// Deprecated — use Generic instead.
    Reviewer,
    /// Deprecated — use Generic instead. Researcher role from white paper.
    Researcher,
    /// Deprecated — use Generic instead. Planner role from white paper.
    Planner,
}

impl AgentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentKind::Generic => "generic",
            AgentKind::Coder => "coder",
            AgentKind::Writer => "writer",
            AgentKind::Reviewer => "reviewer",
            AgentKind::Researcher => "researcher",
            AgentKind::Planner => "planner",
        }
    }
}

impl std::str::FromStr for AgentKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "generic" => Ok(AgentKind::Generic),
            "coder" => Ok(AgentKind::Coder),
            "writer" => Ok(AgentKind::Writer),
            "reviewer" => Ok(AgentKind::Reviewer),
            "researcher" => Ok(AgentKind::Researcher),
            "planner" => Ok(AgentKind::Planner),
            other => Err(format!("unknown agent kind: {other}")),
        }
    }
}

/// Output produced by an agent run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOutput {
    pub kind: AgentKind,
    pub author: String,
    pub body: String,
    pub confidence: f32,
}

impl AgentOutput {
    pub fn new(kind: AgentKind, author: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            kind,
            author: author.into(),
            body: body.into(),
            confidence: 0.8,
        }
    }

    pub fn with_confidence(mut self, c: f32) -> Self {
        self.confidence = c.clamp(0.0, 1.0);
        self
    }
}

/// The shared contract for every agent.
#[async_trait]
pub trait Agent: Send + Sync {
    fn kind(&self) -> AgentKind;
    fn name(&self) -> &str;
    fn system_prompt(&self) -> &str;
    fn description(&self) -> &str {
        ""
    }
    async fn run(&self, task: &str, ctx: &TeamContext) -> Result<AgentOutput>;
    fn set_mailbox(&mut self, _rx: tokio::sync::mpsc::Receiver<BusMessage>) {}
}

/// v2.0: builds a pool of up to 6 generic agents.
///
/// The pool is lazy — agents are created on first access and cached
/// for the lifetime of the orchestrator.  Each `execute` call uses
/// `agent_count` (2..=6) of them so we never pay for more LLM calls
/// than the user explicitly requests.
pub fn build_agent_pool(llm: Arc<LlmGateway>) -> Vec<Arc<dyn Agent>> {
    (1..=6)
        .map(|i| Arc::new(GenericAgent::new(llm.clone(), i)) as Arc<dyn Agent>)
        .collect()
}

/// v1.1: builds an agent pool from a list of kind strings.
///
/// Each kind maps to a specific agent type. Unknown kinds fall back
/// to `GenericAgent`. The result is clamped to `2..=6` agents.
pub fn build_agent_pool_by_kinds(kinds: &[&str], llm: Arc<LlmGateway>) -> Vec<Arc<dyn Agent>> {
    let clamped = kinds.len().clamp(2, 6);
    kinds
        .iter()
        .take(clamped)
        .enumerate()
        .map(|(i, kind)| {
            let agent: Arc<dyn Agent> = match *kind {
                "coder" => Arc::new(CoderAgent::new(llm.clone())) as Arc<dyn Agent>,
                "writer" => Arc::new(WriterAgent::new(llm.clone(), None)) as Arc<dyn Agent>,
                "reviewer" => Arc::new(ReviewerAgent::new(llm.clone())) as Arc<dyn Agent>,
                "planner" => Arc::new(PlannerAgent::new(llm.clone())) as Arc<dyn Agent>,
                "researcher" => Arc::new(ResearcherAgent::new(llm.clone())) as Arc<dyn Agent>,
                _ => Arc::new(GenericAgent::new(llm.clone(), (i + 1) as u32)) as Arc<dyn Agent>,
            };
            agent
        })
        .collect()
}

/// Deprecated — kept for gRPC backward compatibility.
/// New code should use [`build_agent_pool`].
#[deprecated(since = "2.0.0", note = "use `build_agent_pool` instead")]
pub fn canonical_team(
    llm: Arc<LlmGateway>,
    sponge: Option<Arc<SpongeEngine>>,
) -> Vec<Arc<dyn Agent>> {
    let _ = sponge;
    vec![
        Arc::new(CoderAgent::new(llm.clone())),
        Arc::new(ResearcherAgent::new(llm.clone())),
        Arc::new(WriterAgent::new(llm.clone(), None)),
        Arc::new(ReviewerAgent::new(llm.clone())),
        Arc::new(PlannerAgent::new(llm)),
    ]
}

const DEFAULT_MAX_AGENTS: usize = 20;
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 300;

struct PooledAgent {
    agent: Arc<dyn Agent>,
    last_used: std::time::Instant,
    in_use: bool,
}

pub struct DynamicAgentPool {
    llm: Arc<LlmGateway>,
    agents: Vec<PooledAgent>,
    max_agents: usize,
    idle_timeout: std::time::Duration,
    next_id: u32,
}

impl DynamicAgentPool {
    pub fn new(llm: Arc<LlmGateway>) -> Self {
        Self {
            llm,
            agents: Vec::new(),
            max_agents: DEFAULT_MAX_AGENTS,
            idle_timeout: std::time::Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
            next_id: 1,
        }
    }

    pub fn with_max_agents(mut self, max: usize) -> Self {
        self.max_agents = max.max(1);
        self
    }

    pub fn with_idle_timeout(mut self, secs: u64) -> Self {
        self.idle_timeout = std::time::Duration::from_secs(secs);
        self
    }

    pub fn acquire(&mut self, kind: AgentKind) -> Option<Arc<dyn Agent>> {
        if let Some(pooled) = self
            .agents
            .iter_mut()
            .find(|a| !a.in_use && a.agent.kind() == kind)
        {
            pooled.in_use = true;
            pooled.last_used = std::time::Instant::now();
            return Some(pooled.agent.clone());
        }

        if self.agents.len() >= self.max_agents {
            if let Some(pooled) = self.agents.iter_mut().find(|a| !a.in_use) {
                pooled.in_use = true;
                pooled.last_used = std::time::Instant::now();
                return Some(pooled.agent.clone());
            }
            return None;
        }

        let id = self.next_id;
        self.next_id += 1;
        let agent: Arc<dyn Agent> = match kind {
            AgentKind::Generic => Arc::new(GenericAgent::new(self.llm.clone(), id)),
            AgentKind::Coder => Arc::new(CoderAgent::new(self.llm.clone())),
            AgentKind::Writer => Arc::new(WriterAgent::new(self.llm.clone(), None)),
            AgentKind::Reviewer => Arc::new(ReviewerAgent::new(self.llm.clone())),
            AgentKind::Researcher => Arc::new(ResearcherAgent::new(self.llm.clone())),
            AgentKind::Planner => Arc::new(PlannerAgent::new(self.llm.clone())),
        };

        self.agents.push(PooledAgent {
            agent: agent.clone(),
            last_used: std::time::Instant::now(),
            in_use: true,
        });

        Some(agent)
    }

    pub fn release(&mut self, agent_name: &str) {
        if let Some(pooled) = self
            .agents
            .iter_mut()
            .find(|a| a.agent.name() == agent_name)
        {
            pooled.in_use = false;
            pooled.last_used = std::time::Instant::now();
        }
    }

    pub fn cleanup_idle(&mut self) -> usize {
        let before = self.agents.len();
        self.agents
            .retain(|a| a.in_use || a.last_used.elapsed() < self.idle_timeout);
        before - self.agents.len()
    }

    pub fn active_count(&self) -> usize {
        self.agents.iter().filter(|a| a.in_use).count()
    }

    pub fn total_count(&self) -> usize {
        self.agents.len()
    }
}
