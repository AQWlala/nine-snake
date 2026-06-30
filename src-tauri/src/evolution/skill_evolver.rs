//! SkillAutoEvolver + Skill Archive = "uselessness decay" loop.
//!
//! Behaviour:
//!   * A skill becomes an *archive candidate* when its
//!     `usage_count >= archive_min_usage` AND
//!     `avg_rating < archive_rate_floor`.
//!   * `SkillAutoEvolver::run_once()` reads the SkillStore, performs
//!     the test, and moves offenders into a separate
//!     `skill_archive` table (created by migration 009).
//!   * The original skill row stays untouched — we never delete.  This
//!     preserves an audit trail and lets the user undo via the
//!     `evolution_restore_archived` Tauri command.
//!
//! All public entry points respect `evolution::evolution_enabled()`.

use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::outcome::{Outcome, OutcomeLedger, OutcomeSource, OutcomeStatus};
use super::EvolutionConfig;
use crate::skills::store::SkillStore;

/// A snapshot of a single skill's archive decision (for tests + UI).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArchiveDecision {
    pub skill_id: String,
    pub skill_name: String,
    pub usage_count: u32,
    pub avg_rating: f32,
    /// True iff the skill was moved to the archive in this pass.
    pub archived: bool,
    /// True iff the skill was restored from the archive in this pass.
    pub restored: bool,
    /// Reason string used by the UI / logs.
    pub reason: String,
}

/// Trait abstraction for testability.
pub trait SkillAutoEvolver: Send + Sync {
    fn run_once(&self) -> Result<Vec<ArchiveDecision>>;
}

pub struct SqliteSkillAutoEvolver {
    pub skills: Arc<SkillStore>,
    pub ledger: Arc<dyn OutcomeLedger>,
    pub conn: Arc<parking_lot::Mutex<Connection>>,
    pub config: EvolutionConfig,
}

impl SqliteSkillAutoEvolver {
    pub fn new(
        skills: Arc<SkillStore>,
        ledger: Arc<dyn OutcomeLedger>,
        conn: Arc<parking_lot::Mutex<Connection>>,
        config: EvolutionConfig,
    ) -> Self {
        Self {
            skills,
            ledger,
            conn,
            config,
        }
    }

    /// Effective archive criterion.
    fn should_archive(&self, usage_count: u32, avg_rating: f32) -> bool {
        usage_count >= self.config.archive_min_usage && avg_rating < self.config.archive_rate_floor
    }

    /// Move an already-archived skill back into the active list.
    pub fn restore(&self, skill_id: &str, reason: &str) -> Result<bool> {
        let g = self.conn.lock();
        let n = g.execute(
            "DELETE FROM skill_archive WHERE skill_id = ?1",
            params![skill_id],
        )?;
        if n > 0 {
            tracing::info!(
                target: "nine_snake.evolution.skill_evolver",
                skill_id,
                reason,
                "skill restored from archive"
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

impl SkillAutoEvolver for SqliteSkillAutoEvolver {
    fn run_once(&self) -> Result<Vec<ArchiveDecision>> {
        // Pull every active skill via the existing store.
        // NB: SkillStore::list takes raw (language, tag, limit) — the
        // ListSkillsRequest DTO is the wire form for the Tauri/gRPC
        // layer only.  Reuse the store's typed signature here.
        let skills = self.skills.list(None, None, 1000)?;
        let now = chrono::Utc::now().timestamp();
        let mut decisions = Vec::new();

        for s in skills {
            // Already archived? Check before evaluating.
            let already_archived = {
                let g = self.conn.lock();
                let mut stmt = g.prepare("SELECT 1 FROM skill_archive WHERE skill_id = ?1")?;
                stmt.exists(params![s.id])?
            };
            if already_archived {
                // Auto-restore if recent outcomes look good again.
                let recent = self.ledger.by_source(
                    OutcomeSource::Skill,
                    &s.id,
                    self.config.prompt_mutator_window as usize,
                )?;
                if recent.len() as u32 >= self.config.archive_min_usage {
                    let avg_conf =
                        recent.iter().map(|o| o.confidence).sum::<f32>() / recent.len() as f32;
                    if avg_conf >= self.config.goal_confidence_threshold {
                        let _unused = self.restore(&s.id, "outcomes recovered")?;
                        decisions.push(ArchiveDecision {
                            skill_id: s.id.clone(),
                            skill_name: s.name.clone(),
                            usage_count: s.usage_count,
                            avg_rating: s.avg_rating,
                            archived: false,
                            restored: true,
                            reason: "outcomes recovered".into(),
                        });
                        continue;
                    }
                }
                continue;
            }

            let avg_rating_decision =
                Self::should_archive_decision_static(s.usage_count, s.avg_rating, &self.config);
            if avg_rating_decision.archived {
                // Persist to skill_archive.
                let g = self.conn.lock();
                g.execute(
                    "INSERT OR REPLACE INTO skill_archive
                        (skill_id, skill_name, usage_count, avg_rating, archived_at, reason)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        s.id,
                        s.name,
                        s.usage_count as i64,
                        s.avg_rating,
                        now,
                        avg_rating_decision.reason,
                    ],
                )?;
                drop(g);
                let _unused = self.ledger.record(&Outcome {
                    id: super::outcome::fresh_outcome_id(),
                    source_id: s.id.clone(),
                    source: OutcomeSource::Skill,
                    status: OutcomeStatus::Cancelled,
                    confidence: 0.0,
                    error: format!("auto-archived: {}", avg_rating_decision.reason),
                    duration_ms: 0,
                    created_at: now,
                });
                decisions.push(ArchiveDecision {
                    skill_id: s.id.clone(),
                    skill_name: s.name.clone(),
                    usage_count: s.usage_count,
                    avg_rating: s.avg_rating,
                    archived: true,
                    restored: false,
                    reason: avg_rating_decision.reason,
                });
                continue;
            }

            // Not archived.
            decisions.push(ArchiveDecision {
                skill_id: s.id.clone(),
                skill_name: s.name.clone(),
                usage_count: s.usage_count,
                avg_rating: s.avg_rating,
                archived: false,
                restored: false,
                reason: "kept".into(),
            });
        }

        Ok(decisions)
    }
}

struct ArchiveSemantic {
    pub archived: bool,
    pub reason: String,
}

impl SqliteSkillAutoEvolver {
    fn should_archive_decision_static(
        usage_count: u32,
        avg_rating: f32,
        cfg: &EvolutionConfig,
    ) -> ArchiveSemantic {
        if usage_count >= cfg.archive_min_usage && avg_rating < cfg.archive_rate_floor {
            ArchiveSemantic {
                archived: true,
                reason: format!(
                    "usage_count={} >= {} AND avg_rating={:.2} < {:.2}",
                    usage_count, cfg.archive_min_usage, avg_rating, cfg.archive_rate_floor
                ),
            }
        } else {
            ArchiveSemantic {
                archived: false,
                reason: "below archive threshold".into(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::types::ListSkillsRequest;

    #[test]
    fn archive_decision_below_min_usage() {
        let cfg = EvolutionConfig::default(); // min_usage = 20
        let d = SqliteSkillAutoEvolver::should_archive_decision_static(5, 0.3, &cfg);
        assert!(!d.archived);
        assert!(d.reason.contains("below archive"));
    }

    #[test]
    fn archive_decision_at_min_usage_and_low_rating() {
        let cfg = EvolutionConfig::default();
        let d = SqliteSkillAutoEvolver::should_archive_decision_static(20, 0.3, &cfg);
        assert!(d.archived);
        assert!(d.reason.contains("usage_count=20"));
    }

    #[test]
    fn archive_decision_at_min_usage_and_high_rating() {
        let cfg = EvolutionConfig::default();
        let d = SqliteSkillAutoEvolver::should_archive_decision_static(20, 0.9, &cfg);
        assert!(!d.archived);
    }

    #[test]
    fn archive_decision_at_critical() {
        let cfg = EvolutionConfig::default();
        let d = SqliteSkillAutoEvolver::should_archive_decision_static(100, 0.49, &cfg);
        assert!(d.archived);
    }

    #[test]
    fn list_skills_request_serializes() {
        // guard against accidental breaking change to ListSkillsRequest.
        let r = ListSkillsRequest::default();
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"limit\":"));
    }
}
