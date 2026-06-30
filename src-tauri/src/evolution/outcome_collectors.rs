//! Outcome collectors — thin wrappers that the *existing* engine code
//! (skills/engine.rs, swarm/orchestrator.rs, commands/mod.rs) uses
//! to report back what happened.  The collectors always work even
//! when the evolution feature is disabled — they emit a row in
//! either case and let the `SkillAutoEvolver` decide whether to act.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::outcome::{fresh_outcome_id, Outcome, OutcomeLedger, OutcomeSource, OutcomeStatus};
use crate::evolution::evolution_enabled;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCollectedArgs {
    pub skill_id: String,
    pub status: OutcomeStatus,
    pub confidence: f32,
    pub error: String,
    pub duration_ms: u32,
}

pub fn collect_skill(ledger: &Arc<dyn OutcomeLedger>, args: SkillCollectedArgs) -> Result<()> {
    ledger.record(&Outcome {
        id: fresh_outcome_id(),
        source_id: args.skill_id,
        source: OutcomeSource::Skill,
        status: args.status,
        confidence: args.confidence.clamp(0.0, 1.0),
        error: args.error,
        duration_ms: args.duration_ms,
        created_at: chrono::Utc::now().timestamp(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmCollectedArgs {
    pub task_id: String,
    pub agent_kinds: Vec<String>,
    pub status: OutcomeStatus,
    pub confidence: f32,
    pub error: String,
    pub duration_ms: u32,
}

pub fn collect_swarm(ledger: &Arc<dyn OutcomeLedger>, args: SwarmCollectedArgs) -> Result<()> {
    // One outcome per agent in the pipeline.
    for kind in &args.agent_kinds {
        ledger.record(&Outcome {
            id: fresh_outcome_id(),
            source_id: format!("{}::{}", args.task_id, kind),
            source: OutcomeSource::Swarm,
            status: args.status,
            confidence: args.confidence.clamp(0.0, 1.0),
            error: args.error.clone(),
            duration_ms: args.duration_ms,
            created_at: chrono::Utc::now().timestamp(),
        })?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCollectedArgs {
    pub conversation_id: String,
    pub status: OutcomeStatus,
    pub confidence: f32,
    pub error: String,
    pub duration_ms: u32,
}

pub fn collect_chat(ledger: &Arc<dyn OutcomeLedger>, args: ChatCollectedArgs) -> Result<()> {
    ledger.record(&Outcome {
        id: fresh_outcome_id(),
        source_id: args.conversation_id,
        source: OutcomeSource::Chat,
        status: args.status,
        confidence: args.confidence.clamp(0.0, 1.0),
        error: args.error,
        duration_ms: args.duration_ms,
        created_at: chrono::Utc::now().timestamp(),
    })
}

/// Returns true iff the evolution master switch is on.  Public so the
/// patch-sites (skills/engine, swarm/orchestrator) can short-circuit
/// to avoid hot-path overhead when the feature is disabled.
pub fn evolution_hot_path_active() -> bool {
    evolution_enabled()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evolution::outcome::InMemoryOutcomeLedger;

    #[test]
    fn skill_collector_clamps_confidence() {
        let ledger: Arc<dyn OutcomeLedger> = Arc::new(InMemoryOutcomeLedger::default());
        collect_skill(
            &ledger,
            SkillCollectedArgs {
                skill_id: "k".into(),
                status: OutcomeStatus::Success,
                confidence: 2.0, // out of range
                error: "".into(),
                duration_ms: 1,
            },
        )
        .unwrap();
        let rows = ledger.recent(10).unwrap();
        assert_eq!(rows[0].confidence, 1.0);
    }

    #[test]
    fn swarm_collector_emits_per_agent() {
        let ledger: Arc<dyn OutcomeLedger> = Arc::new(InMemoryOutcomeLedger::default());
        collect_swarm(
            &ledger,
            SwarmCollectedArgs {
                task_id: "t1".into(),
                agent_kinds: vec!["coder".into(), "writer".into()],
                status: OutcomeStatus::Success,
                confidence: 0.8,
                error: "".into(),
                duration_ms: 10,
            },
        )
        .unwrap();
        let rows = ledger.recent(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.source_id.ends_with("::coder")));
        assert!(rows.iter().any(|r| r.source_id.ends_with("::writer")));
    }

    #[test]
    fn chat_collector_records() {
        let ledger: Arc<dyn OutcomeLedger> = Arc::new(InMemoryOutcomeLedger::default());
        collect_chat(
            &ledger,
            ChatCollectedArgs {
                conversation_id: "c1".into(),
                status: OutcomeStatus::Timeout,
                confidence: 0.0,
                error: "8s".into(),
                duration_ms: 8_000,
            },
        )
        .unwrap();
        let rows = ledger.recent(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, OutcomeStatus::Timeout);
    }
}
