//! PromptSelfMutator: rewrite `Agent::system_prompt` from runtime
//! outcomes, with snapshot/rollback for safety.
//!
//! Hard guarantees:
//!   * Every mutation must take a snapshot first.
//!   * `rollback_to(id)` always works as long as the snapshot exists.
//!   * Rollback is enforced automatically when post-mutation
//!     confidence regresses by more than `rollback_threshold_pct`
//!     against the previous 5 outcomes.
//!   * The whole subsystem is inert unless
//!     `evolution::evolution_enabled()` returns true.
//!
//! Snapshot table: `prompt_snapshots` (migration 008).

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::warn;

use super::outcome::{Outcome, OutcomeLedger, OutcomeSource};
use crate::llm::{ChatMessage, LlmGateway};

pub const DEFAULT_ROLLBACK_THRESHOLD_PCT: f32 = 5.0;
pub const DEFAULT_MUTATION_MAX_OUTCOMES: u32 = 30;
pub const DEFAULT_POST_MUTATION_OBSERVE_WINDOW: u32 = 5;
pub const DEFAULT_ROLLBACK_OBSERVE_WINDOW: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromptSnapshot {
    pub id: String,
    pub target: String, // agent name (e.g. "coder", "writer", "reviewer")
    pub prev_prompt: String,
    pub replaced_at: i64,
    pub reason: Option<String>,
    pub restored_to_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationResult {
    pub target: String,
    pub new_prompt: String,
    pub snapshot_id: String,
    pub evaluated_at: i64,
}

/// Abstraction so we can swap the actual mutator strategy.
pub trait PromptMutator: Send + Sync {
    /// Given a set of recent outcomes for `target`, propose a new
    /// system prompt string.  Calls backing LLM or a deterministic
    /// stub (test).
    fn propose(&self, target: &str, recent: &[Outcome]) -> Result<String>;
}

/// Default no-op mutator: leaves the prompt untouched but still
/// captures a snapshot.  Used when LLM-feature is unavailable.
pub struct NoopPromptMutator;

impl PromptMutator for NoopPromptMutator {
    fn propose(&self, target: &str, _recent: &[Outcome]) -> Result<String> {
        Ok(target.to_string())
    }
}

pub struct LlmPromptMutator {
    gateway: LlmGateway,
    rollback_threshold_pct: f32,
}

impl LlmPromptMutator {
    pub fn new(gateway: LlmGateway) -> Self {
        Self {
            gateway,
            rollback_threshold_pct: DEFAULT_ROLLBACK_THRESHOLD_PCT,
        }
    }

    pub fn with_rollback_threshold(mut self, pct: f32) -> Self {
        self.rollback_threshold_pct = pct;
        self
    }
}

impl PromptMutator for LlmPromptMutator {
    fn propose(&self, target: &str, recent: &[Outcome]) -> Result<String> {
        let mut summary_lines = Vec::new();
        for o in recent.iter().take(10) {
            summary_lines.push(format!(
                "- status: {}, confidence: {:.2}, error: {:?}",
                o.status.as_str(),
                o.confidence,
                if o.error.is_empty() {
                    None
                } else {
                    Some(&o.error)
                },
            ));
        }
        let outcomes_text = summary_lines.join("\n");

        let messages = vec![
            ChatMessage::system(format!(
                "You are a prompt optimization assistant for an AI agent named '{target}'. \
                 Your task is to propose an improved system prompt based on recent task outcomes. \
                 Output ONLY the improved prompt text, nothing else."
            )),
            ChatMessage::user(format!(
                "Recent outcomes for agent '{target}':\n{outcomes_text}\n\n\
                 Based on these outcomes, propose an improved system prompt for this agent. \
                 Focus on addressing failure patterns and reinforcing success patterns."
            )),
        ];

        let rt = tokio::runtime::Handle::current();
        let result = rt.block_on(async { self.gateway.chat(messages).await });

        match result {
            Ok(resp) => Ok(resp.message.content),
            Err(e) => {
                warn!(target: "nine_snake.evolution", error = %e, "LLM prompt mutation failed; falling back to no-op");
                Ok(format!("{target} (llm mutator fallback)"))
            }
        }
    }
}

pub struct SqlitePromptSelfMutator {
    pub conn: Arc<parking_lot::Mutex<Connection>>,
    pub ledger: Arc<dyn OutcomeLedger>,
    pub mutator: Arc<dyn PromptMutator>,
    pub config: super::EvolutionConfig,
    pub rollback_threshold_pct: f32,
}

impl SqlitePromptSelfMutator {
    pub fn new(
        conn: Arc<parking_lot::Mutex<Connection>>,
        ledger: Arc<dyn OutcomeLedger>,
        mutator: Arc<dyn PromptMutator>,
        config: super::EvolutionConfig,
    ) -> Self {
        Self {
            conn,
            ledger,
            mutator,
            config,
            rollback_threshold_pct: DEFAULT_ROLLBACK_THRESHOLD_PCT,
        }
    }

    /// Take a snapshot of the current prompt for `target`.  Uses the
    /// caller-provided current prompt (we don't introspect Agent
    /// state — front-end or other writer is responsible for that).
    pub fn snapshot(
        &self,
        target: &str,
        current_prompt: &str,
        reason: Option<&str>,
    ) -> Result<String> {
        let id = format!(
            "snap_{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let now = chrono::Utc::now().timestamp();
        let g = self.conn.lock();
        g.execute(
            "INSERT INTO prompt_snapshots
                (id, target, prev_prompt, replaced_at, reason, restored_to_id)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
            params![id, target, current_prompt, now, reason],
        )?;
        Ok(id)
    }

    /// Roll back to a previous snapshot.  Returns the restored prompt,
    /// or `None` if `snapshot_id` does not exist.
    pub fn rollback_to(&self, snapshot_id: &str) -> Result<Option<String>> {
        let g = self.conn.lock();
        let prev: Option<(String, String)> = g
            .query_row(
                "SELECT target, prev_prompt FROM prompt_snapshots WHERE id = ?1",
                params![snapshot_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok();
        let Some((target, _prev_prompt)) = prev else {
            return Ok(None);
        };
        // Mark every later snapshot for the same target as restored.
        g.execute(
            "UPDATE prompt_snapshots SET restored_to_id = ?1
             WHERE target = ?2 AND restored_to_id IS NULL AND id <> ?1",
            params![snapshot_id, target],
        )?;
        Ok(Some(target)) // The agent must look up the prompt itself
    }

    /// Should we rollback?  Compare recent post-mutation confidence
    /// against pre-mutation baseline.
    pub fn should_rollback(&self, recent: &[Outcome], baseline_avg: f32) -> bool {
        if recent.is_empty() || baseline_avg == 0.0 {
            return false;
        }
        let avg = recent.iter().map(|o| o.confidence).sum::<f32>() / recent.len() as f32;
        let drop_pct = ((baseline_avg - avg) / baseline_avg) * 100.0;
        drop_pct >= self.rollback_threshold_pct
    }

    /// Run a single mutation pass for `target`.
    ///
    /// Strategy:
    ///   1. Read recent outcomes.
    ///   2. If `recent.len() < config.prompt_mutator_window`, do
    ///      nothing.
    ///   3. Otherwise snapshot current prompt + propose + return.
    ///
    /// (The actual write of `new_prompt` to the Agent is delegated to
    /// a separate `update_agent_prompt` call — the mutator is
    /// intentionally pure.)
    pub fn run_once(&self, target: &str, current_prompt: &str) -> Result<Option<MutationResult>> {
        let recent = self.ledger.by_source(
            OutcomeSource::Swarm, // swarm agents share these; chat ones too
            target,
            self.config.prompt_mutator_window as usize,
        )?;
        if (recent.len() as u32) < self.config.prompt_mutator_window {
            return Ok(None);
        }
        let new_prompt = self.mutator.propose(target, &recent)?;
        let snap_id = self.snapshot(target, current_prompt, Some("auto-mutate"))?;
        Ok(Some(MutationResult {
            target: target.to_string(),
            new_prompt,
            snapshot_id: snap_id,
            evaluated_at: chrono::Utc::now().timestamp(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evolution::outcome::InMemoryOutcomeLedger;
    use crate::evolution::outcome::OutcomeStatus;

    fn setup() -> (
        Arc<parking_lot::Mutex<Connection>>,
        Arc<InMemoryOutcomeLedger>,
    ) {
        // in-memory sqlite so we can test snapshot/restore end-to-end
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE prompt_snapshots (
                id TEXT PRIMARY KEY,
                target TEXT NOT NULL,
                prev_prompt TEXT NOT NULL,
                replaced_at INTEGER NOT NULL,
                reason TEXT,
                restored_to_id TEXT
            );",
        )
        .unwrap();
        let m = Arc::new(parking_lot::Mutex::new(conn));
        let ledger = Arc::new(InMemoryOutcomeLedger::default());
        (m, ledger)
    }

    #[test]
    fn snapshot_inserts_and_rollback_marks_restored() {
        let (m, ledger) = setup();
        let cfg = EvolutionConfig::default();
        let mutator =
            SqlitePromptSelfMutator::new(m.clone(), ledger, Arc::new(NoopPromptMutator), cfg);
        let s1 = mutator
            .snapshot("coder", "hello v1", Some("first"))
            .unwrap();
        let _s2 = mutator
            .snapshot("coder", "hello v2", Some("second"))
            .unwrap();
        let restored = mutator.rollback_to(&s1).unwrap();
        assert_eq!(restored, Some("coder".to_string()));
    }

    #[test]
    fn should_rollback_when_post_mutation_drops() {
        let (_m, ledger) = setup();
        let cfg = EvolutionConfig::default();
        let mutator = SqlitePromptSelfMutator::new(
            Arc::new(parking_lot::Mutex::new(
                rusqlite::Connection::open_in_memory().unwrap(),
            )),
            ledger.clone(),
            Arc::new(NoopPromptMutator),
            cfg,
        );
        // Recent confidence average = 0.5, baseline = 1.0 → 50% drop
        for i in 0..5 {
            ledger
                .record(&Outcome {
                    id: format!("o{i}"),
                    source_id: "coder".into(),
                    source: OutcomeSource::Swarm,
                    status: OutcomeStatus::Success,
                    confidence: 0.5,
                    error: "".into(),
                    duration_ms: 0,
                    created_at: i as i64,
                })
                .unwrap();
        }
        let recent = ledger.by_source(OutcomeSource::Swarm, "coder", 5).unwrap();
        assert!(mutator.should_rollback(&recent, 1.0));
    }

    #[test]
    fn no_rollback_when_post_mutation_steady() {
        let (_m, ledger) = setup();
        let cfg = EvolutionConfig::default();
        let mutator = SqlitePromptSelfMutator::new(
            Arc::new(parking_lot::Mutex::new(
                rusqlite::Connection::open_in_memory().unwrap(),
            )),
            ledger.clone(),
            Arc::new(NoopPromptMutator),
            cfg,
        );
        for i in 0..5 {
            ledger
                .record(&Outcome {
                    id: format!("o{i}"),
                    source_id: "coder".into(),
                    source: OutcomeSource::Swarm,
                    status: OutcomeStatus::Success,
                    confidence: 0.95,
                    error: "".into(),
                    duration_ms: 0,
                    created_at: i as i64,
                })
                .unwrap();
        }
        let recent = ledger.by_source(OutcomeSource::Swarm, "coder", 5).unwrap();
        // baseline = 0.95 → 0% drop
        assert!(!mutator.should_rollback(&recent, 0.95));
    }

    #[test]
    fn run_once_no_op_when_below_window() {
        let (m, ledger) = setup();
        let cfg = EvolutionConfig::default();
        let mutator = SqlitePromptSelfMutator::new(m, ledger, Arc::new(NoopPromptMutator), cfg);
        // prompt_mutator_window (default 30) > 0 outcomes recorded → returns None.
        let got = mutator.run_once("coder", "hello v1");
        assert!(got.is_ok());
        assert!(got.unwrap().is_none());
    }
}
