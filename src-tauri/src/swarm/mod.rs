//! `nine_snake::swarm` — multi-agent orchestration.
//!
//! The swarm subsystem coordinates a small team of specialised agents
//! that collaborate on every non-trivial task. The key invariants are:
//!
//! * Every task dispatches **at least two agents** (so the "砍一个，长
//!   两个" principle is upheld even when one of them is the user's
//!   own dialogue).
//! * All agents read from a shared [`context::TeamContext`] so the
//!   output of one agent can condition the next.
//! * [`orchestrator::SwarmOrchestrator`] owns the dispatch logic and
//!   the retry / fallback policy.

pub mod agents;
pub mod bus;
pub mod composer;
pub mod context;
pub mod negotiator;
pub mod orchestrator;

pub use agents::{build_agent_pool, Agent, AgentKind, AgentOutput, GenericAgent};
#[allow(deprecated)]
pub use agents::{canonical_team, CoderAgent, ReviewerAgent, WriterAgent};
pub use bus::{AgentBus, BusMessage, BusMessageType};
pub use composer::{SkillComposer, SkillContext, SkillMatch};
pub use context::{ContextEntry, TeamContext};
pub use negotiator::{NegotiationMethod, NegotiationResult, Negotiator};
pub use orchestrator::{AgentDescriptor, OrchestrationReport, SwarmOrchestrator, SwarmTask};
