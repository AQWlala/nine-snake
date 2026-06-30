//! TaskOutcome DTO and Sqlite-backed OutcomeLedger.
//!
//! An `Outcome` is the single source of truth for "what actually
//! happened when a thing ran": a skill execution, a swarm
//! orchestration pass, or a chat round-trip.  SkillAutoEvolver and
//! PromptSelfMutator read from here.
//!
//! Schema: see `migrations/008_task_outcomes.sql`.  The migration is
//! "additive" (CREATE TABLE IF NOT EXISTS) so it is safe to ship and
//! to back-out.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// Status of a single task attempt.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Success,
    Fail,
    Timeout,
    Aborted,
    Cancelled,
}

impl OutcomeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OutcomeStatus::Success => "success",
            OutcomeStatus::Fail => "fail",
            OutcomeStatus::Timeout => "timeout",
            OutcomeStatus::Aborted => "aborted",
            OutcomeStatus::Cancelled => "cancelled",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "success" => Some(OutcomeStatus::Success),
            "fail" => Some(OutcomeStatus::Fail),
            "timeout" => Some(OutcomeStatus::Timeout),
            "aborted" => Some(OutcomeStatus::Aborted),
            "cancelled" => Some(OutcomeStatus::Cancelled),
            _ => None,
        }
    }
}

/// Where the outcome came from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeSource {
    Skill,
    Swarm,
    Chat,
    Other,
}

impl OutcomeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            OutcomeSource::Skill => "skill",
            OutcomeSource::Swarm => "swarm",
            OutcomeSource::Chat => "chat",
            OutcomeSource::Other => "other",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "skill" => Some(OutcomeSource::Skill),
            "swarm" => Some(OutcomeSource::Swarm),
            "chat" => Some(OutcomeSource::Chat),
            "other" => Some(OutcomeSource::Other),
            _ => None,
        }
    }
}

/// A single, persisted record of "what happened".
///
/// We keep the field set small on purpose — anything richer should be
/// a joined row, not a column.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Outcome {
    /// UUIDv4.
    pub id: String,
    /// Identifier of the source task/subject.  For skills this is a
    /// `skill_id`, for swarm this is the orchestrator session id, for
    /// chat this is the conversation id.
    pub source_id: String,
    /// Source kind.
    pub source: OutcomeSource,
    /// What happened.
    pub status: OutcomeStatus,
    /// Confidence in `[0.0, 1.0]`.  `1.0` ⇒ author was sure.  Set by
    /// the producer; for skills this is 1.0 on success / 0.0 on fail;
    /// for swarm this is the orchestrator-reported confidence; for
    /// chat this is 0.7 by default (timeout 0.0).
    pub confidence: f32,
    /// Free-form error message (or empty on success).
    pub error: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u32,
    /// Unix timestamp (seconds).
    pub created_at: i64,
}

/// Snapshot row used by query functions; identical to `Outcome`
/// shape, but kept distinct so future schema changes don't break
/// callers holding an `Outcome` by value.
#[derive(Debug, Clone)]
pub struct OutcomeRow {
    pub id: String,
    pub source_id: String,
    pub source: String,
    pub status: String,
    pub confidence: f32,
    pub error: String,
    pub duration_ms: u32,
    pub created_at: i64,
}

/// Trait abstraction so the ledger can be swapped (in-memory for
/// tests, sqlite for production).
pub trait OutcomeLedger: Send + Sync {
    fn record(&self, outcome: &Outcome) -> Result<()>;
    fn recent(&self, limit: usize) -> Result<Vec<Outcome>>;
    fn by_source(
        &self,
        source: OutcomeSource,
        source_id: &str,
        limit: usize,
    ) -> Result<Vec<Outcome>>;
}

/// Sqlite-backed implementation.  Holds a borrowed
/// `parking_lot::Mutex<Connection>` (same as `SkillStore`).
pub struct SqliteOutcomeLedger {
    conn: std::sync::Arc<parking_lot::Mutex<Connection>>,
}

impl SqliteOutcomeLedger {
    pub fn new(conn: std::sync::Arc<parking_lot::Mutex<Connection>>) -> Self {
        Self { conn }
    }
}

impl OutcomeLedger for SqliteOutcomeLedger {
    fn record(&self, outcome: &Outcome) -> Result<()> {
        let g = self.conn.lock();
        g.execute(
            "INSERT OR REPLACE INTO task_outcomes
                (id, source_id, source, status, confidence, error,
                 duration_ms, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                outcome.id,
                outcome.source_id,
                outcome.source.as_str(),
                outcome.status.as_str(),
                outcome.confidence,
                outcome.error,
                outcome.duration_ms as i64,
                outcome.created_at,
            ],
        )
        .map_err(|e| anyhow!("record outcome: {e}"))?;
        Ok(())
    }

    fn recent(&self, limit: usize) -> Result<Vec<Outcome>> {
        let g = self.conn.lock();
        let mut stmt = g.prepare(
            "SELECT id, source_id, source, status, confidence, error,
                    duration_ms, created_at
             FROM task_outcomes ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(OutcomeRow {
                id: row.get(0)?,
                source_id: row.get(1)?,
                source: row.get(2)?,
                status: row.get(3)?,
                confidence: row.get(4)?,
                error: row.get(5)?,
                duration_ms: row.get::<_, i64>(6)? as u32,
                created_at: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            let r = r?;
            out.push(Outcome {
                id: r.id,
                source_id: r.source_id,
                source: OutcomeSource::from_str(&r.source)
                    .ok_or_else(|| anyhow!("unknown source: {}", r.source))?,
                status: OutcomeStatus::from_str(&r.status)
                    .ok_or_else(|| anyhow!("unknown status: {}", r.status))?,
                confidence: r.confidence,
                error: r.error,
                duration_ms: r.duration_ms,
                created_at: r.created_at,
            });
        }
        Ok(out)
    }

    fn by_source(
        &self,
        source: OutcomeSource,
        source_id: &str,
        limit: usize,
    ) -> Result<Vec<Outcome>> {
        let g = self.conn.lock();
        let mut stmt = g.prepare(
            "SELECT id, source_id, source, status, confidence, error,
                    duration_ms, created_at
             FROM task_outcomes
             WHERE source = ?1 AND source_id = ?2
             ORDER BY created_at DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![source.as_str(), source_id, limit as i64], |row| {
            Ok(OutcomeRow {
                id: row.get(0)?,
                source_id: row.get(1)?,
                source: row.get(2)?,
                status: row.get(3)?,
                confidence: row.get(4)?,
                error: row.get(5)?,
                duration_ms: row.get::<_, i64>(6)? as u32,
                created_at: row.get(7)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            let r = r?;
            out.push(Outcome {
                id: r.id,
                source_id: r.source_id,
                source: OutcomeSource::from_str(&r.source)
                    .ok_or_else(|| anyhow!("unknown source: {}", r.source))?,
                status: OutcomeStatus::from_str(&r.status)
                    .ok_or_else(|| anyhow!("unknown status: {}", r.status))?,
                confidence: r.confidence,
                error: r.error,
                duration_ms: r.duration_ms,
                created_at: r.created_at,
            });
        }
        Ok(out)
    }
}

/// In-memory ledger for tests.
#[derive(Default)]
pub struct InMemoryOutcomeLedger {
    pub rows: parking_lot::Mutex<Vec<Outcome>>,
}

impl OutcomeLedger for InMemoryOutcomeLedger {
    fn record(&self, o: &Outcome) -> Result<()> {
        self.rows.lock().push(o.clone());
        Ok(())
    }
    fn recent(&self, limit: usize) -> Result<Vec<Outcome>> {
        let g = self.rows.lock();
        let mut out: Vec<Outcome> = g.iter().cloned().collect();
        out.sort_by_key(|o| std::cmp::Reverse(o.created_at));
        out.truncate(limit);
        Ok(out)
    }
    fn by_source(
        &self,
        source: OutcomeSource,
        source_id: &str,
        limit: usize,
    ) -> Result<Vec<Outcome>> {
        let g = self.rows.lock();
        let mut out: Vec<Outcome> = g
            .iter()
            .filter(|o| o.source == source && o.source_id == source_id)
            .cloned()
            .collect();
        out.sort_by_key(|o| std::cmp::Reverse(o.created_at));
        out.truncate(limit);
        Ok(out)
    }
}

/// Generate a fresh Outcome id without depending on `uuid` (the
/// project already uses `chrono::Utc::now().timestamp_nanos_opt` and
/// string formatting for ids in places — keep it predictable).
pub fn fresh_outcome_id() -> String {
    let ts = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    format!("out_{ts}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_status_roundtrip() {
        for s in [
            OutcomeStatus::Success,
            OutcomeStatus::Fail,
            OutcomeStatus::Timeout,
            OutcomeStatus::Aborted,
            OutcomeStatus::Cancelled,
        ] {
            assert_eq!(OutcomeStatus::from_str(s.as_str()), Some(s));
        }
    }

    #[test]
    fn outcome_source_roundtrip() {
        for s in [
            OutcomeSource::Skill,
            OutcomeSource::Swarm,
            OutcomeSource::Chat,
            OutcomeSource::Other,
        ] {
            assert_eq!(OutcomeSource::from_str(s.as_str()), Some(s));
        }
    }

    #[test]
    fn in_memory_ledger_records_and_queries() {
        let ledger = InMemoryOutcomeLedger::default();
        for i in 0..5 {
            ledger
                .record(&Outcome {
                    id: format!("o{i}"),
                    source_id: format!("skill_{}", i % 2),
                    source: OutcomeSource::Skill,
                    status: if i % 2 == 0 {
                        OutcomeStatus::Success
                    } else {
                        OutcomeStatus::Fail
                    },
                    confidence: if i % 2 == 0 { 1.0 } else { 0.0 },
                    error: String::new(),
                    duration_ms: 100 + i as u32,
                    created_at: i as i64,
                })
                .unwrap();
        }
        let r = ledger.recent(3).unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].id, "o4");
        let s = ledger
            .by_source(OutcomeSource::Skill, "skill_0", 100)
            .unwrap();
        assert_eq!(s.len(), 3); // i ∈ {0,2,4}
        assert!(s.iter().all(|o| o.source_id == "skill_0"));
    }

    #[test]
    fn fresh_outcome_id_is_unique_across_calls() {
        let a = fresh_outcome_id();
        let b = fresh_outcome_id();
        assert_ne!(a, b);
    }

    #[test]
    fn outcome_serializes_to_json() {
        let o = Outcome {
            id: "x".into(),
            source_id: "y".into(),
            source: OutcomeSource::Skill,
            status: OutcomeStatus::Success,
            confidence: 0.9,
            error: "".into(),
            duration_ms: 42,
            created_at: 1,
        };
        let s = serde_json::to_string(&o).unwrap();
        assert!(s.contains("\"source\":\"skill\""));
        assert!(s.contains("\"status\":\"success\""));
    }
}
