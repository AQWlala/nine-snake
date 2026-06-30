use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::debug;

use crate::memory::types::sensitive_text_predicate;

const MAX_SUMMARY_LEN: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillAuditEntry {
    pub id: String,
    pub skill_id: String,
    pub executed_at: i64,
    pub input_summary: String,
    pub output_summary: String,
    pub duration_ms: u64,
    pub sandbox_type: String,
    pub security_scan_result: String,
    pub success: bool,
}

pub struct SkillAuditLogger {
    conn: Arc<parking_lot::Mutex<Connection>>,
}

impl SkillAuditLogger {
    pub fn new(conn: Arc<parking_lot::Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub fn log(&self, entry: &SkillAuditEntry) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO skill_audit_log
                (id, skill_id, executed_at, input_summary, output_summary,
                 duration_ms, sandbox_type, security_scan_result, success)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                entry.id,
                entry.skill_id,
                entry.executed_at,
                entry.input_summary,
                entry.output_summary,
                entry.duration_ms as i64,
                entry.sandbox_type,
                entry.security_scan_result,
                entry.success,
            ],
        )?;
        debug!(target: "nine_snake.audit", skill_id = %entry.skill_id, success = entry.success, "audit log recorded");
        Ok(())
    }

    pub fn list(&self, limit: usize) -> Result<Vec<SkillAuditEntry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, skill_id, executed_at, input_summary, output_summary,
                    duration_ms, sandbox_type, security_scan_result, success
             FROM skill_audit_log ORDER BY executed_at DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(SkillAuditEntry {
                    id: row.get(0)?,
                    skill_id: row.get(1)?,
                    executed_at: row.get(2)?,
                    input_summary: row.get(3)?,
                    output_summary: row.get(4)?,
                    duration_ms: row.get::<_, i64>(5)? as u64,
                    sandbox_type: row.get(6)?,
                    security_scan_result: row.get(7)?,
                    success: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_for_skill(&self, skill_id: &str, limit: usize) -> Result<Vec<SkillAuditEntry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, skill_id, executed_at, input_summary, output_summary,
                    duration_ms, sandbox_type, security_scan_result, success
             FROM skill_audit_log WHERE skill_id = ?1 ORDER BY executed_at DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![skill_id, limit as i64], |row| {
                Ok(SkillAuditEntry {
                    id: row.get(0)?,
                    skill_id: row.get(1)?,
                    executed_at: row.get(2)?,
                    input_summary: row.get(3)?,
                    output_summary: row.get(4)?,
                    duration_ms: row.get::<_, i64>(5)? as u64,
                    sandbox_type: row.get(6)?,
                    security_scan_result: row.get(7)?,
                    success: row.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

pub fn truncate_summary(text: &str) -> String {
    if text.len() <= MAX_SUMMARY_LEN {
        text.to_string()
    } else {
        text[..MAX_SUMMARY_LEN].to_string()
    }
}

pub fn redact_if_sensitive(text: &str) -> String {
    if sensitive_text_predicate(text) {
        let truncated = truncate_summary(text);
        truncated
            .chars()
            .map(|c| if c.is_whitespace() { c } else { 'X' })
            .collect()
    } else {
        truncate_summary(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_long_text() {
        let long = "a".repeat(300);
        let result = truncate_summary(&long);
        assert_eq!(result.len(), 200);
    }

    #[test]
    fn truncate_short_text_unchanged() {
        let short = "hello";
        assert_eq!(truncate_summary(short), "hello");
    }

    #[test]
    fn redact_sensitive_text() {
        let sensitive = "api_key=sk-abc123def456ghi789jkl012mno345pqr678stu901vwx";
        let result = redact_if_sensitive(sensitive);
        assert!(!result.contains("sk-abc"));
        assert!(result.contains("X"));
    }

    #[test]
    fn normal_text_not_redacted() {
        let normal = "the quick brown fox";
        let result = redact_if_sensitive(normal);
        assert_eq!(result, normal);
    }
}
