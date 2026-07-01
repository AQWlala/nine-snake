//! v0.5: Work mode backend — kanban-style task management.
//!
//! Tasks live in the `work_tasks` table introduced by
//! `004_v05.sql`.  The engine exposes a small CRUD surface plus a
//! `recommend_priority` helper that uses simple heuristics (due date
//! + description keyword weighting) to suggest a priority level.  The
//! AI-backed priority scoring is layered on top in v1.0.
//!
//! Time tracking is per-task: callers use `start_timer` /
//! `stop_timer` to accumulate `time_spent_ms`.  A single active
//! timer at a time is enforced via an in-process `Mutex<Option<String>>`
//! on the engine itself.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use tracing::{info, instrument};
use uuid::Uuid;

use crate::memory::sqlite_store::SqliteStore;

/// Task status enum matching the CHECK constraint on
/// `work_tasks.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Todo,
    Doing,
    Done,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Todo => "todo",
            TaskStatus::Doing => "doing",
            TaskStatus::Done => "done",
        }
    }
    pub fn from_str(s: &str) -> Result<Self> {
        match s {
            "todo" => Ok(TaskStatus::Todo),
            "doing" => Ok(TaskStatus::Doing),
            "done" => Ok(TaskStatus::Done),
            other => Err(anyhow!("unknown task status: {other}")),
        }
    }
}

/// A persisted work task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkTask {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: TaskStatus,
    pub priority: i32,
    pub due_at: Option<i64>,
    pub time_spent_ms: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
    pub metadata: Option<serde_json::Value>,
}

/// Work engine — owns task CRUD, kanban moves, time tracking and
/// priority recommendation.
pub struct WorkEngine {
    sqlite: Arc<SqliteStore>,
    /// The id of the task whose timer is currently running (if any).
    active_timer: Mutex<Option<String>>,
}

impl WorkEngine {
    pub fn new(sqlite: Arc<SqliteStore>) -> Self {
        Self {
            sqlite,
            active_timer: Mutex::new(None),
        }
    }

    /// Creates a new task in the `todo` column.
    #[instrument(skip(self, description))]
    pub fn create_task(
        &self,
        title: String,
        description: String,
        priority: Option<i32>,
        due_at: Option<i64>,
    ) -> Result<WorkTask> {
        if title.trim().is_empty() {
            return Err(anyhow!("task title must not be empty"));
        }
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().timestamp();
        let priority = priority.unwrap_or(0).clamp(0, 3);
        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        conn.execute(
            "INSERT INTO work_tasks (id, title, description, status, priority, due_at, time_spent_ms, created_at, updated_at, completed_at, metadata) \
             VALUES (?1, ?2, ?3, 'todo', ?4, ?5, 0, ?6, ?6, NULL, NULL)",
            params![id, title, description, priority, due_at, now],
        )
        .context("inserting work task")?;
        drop(conn);
        info!(target: "nine_snake.work", id = %id, "task created");
        Ok(WorkTask {
            id,
            title,
            description,
            status: TaskStatus::Todo,
            priority,
            due_at,
            time_spent_ms: 0,
            created_at: now,
            updated_at: now,
            completed_at: None,
            metadata: None,
        })
    }

    /// Fetches a task by id.
    pub fn get_task(&self, id: &str) -> Result<Option<WorkTask>> {
        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        row_to_task(
            &conn,
            "SELECT id, title, description, status, priority, due_at, time_spent_ms, created_at, updated_at, completed_at, metadata FROM work_tasks WHERE id = ?1",
            params![id],
        )
    }

    /// Lists tasks, optionally filtered by status.  `limit` defaults to 200.
    pub fn list_tasks(
        &self,
        status: Option<TaskStatus>,
        limit: Option<usize>,
    ) -> Result<Vec<WorkTask>> {
        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        let lim = limit.unwrap_or(200).max(1) as i64;
        if let Some(s) = status {
            row_to_tasks(
                &conn,
                "SELECT id, title, description, status, priority, due_at, time_spent_ms, created_at, updated_at, completed_at, metadata FROM work_tasks WHERE status = ?1 ORDER BY priority DESC, updated_at DESC LIMIT ?2",
                params![s.as_str(), lim],
            )
        } else {
            row_to_tasks(
                &conn,
                "SELECT id, title, description, status, priority, due_at, time_spent_ms, created_at, updated_at, completed_at, metadata FROM work_tasks ORDER BY priority DESC, updated_at DESC LIMIT ?1",
                params![lim],
            )
        }
    }

    /// Moves a task to a new status.  When the destination is `done`,
    /// `completed_at` is set to now.
    pub fn set_status(&self, id: &str, status: TaskStatus) -> Result<WorkTask> {
        let now = Utc::now().timestamp();
        let completed_at = if status == TaskStatus::Done {
            Some(now)
        } else {
            None
        };
        {
            let conn = self.sqlite.raw_connection();
            let conn = conn.lock();
            let n = conn.execute(
                "UPDATE work_tasks SET status = ?1, updated_at = ?2, completed_at = ?3 WHERE id = ?4",
                params![status.as_str(), now, completed_at, id],
            )?;
            if n == 0 {
                return Err(anyhow!("task not found: {id}"));
            }
        } // conn dropped here — lock released before calling self.get_task()
          // Stop the timer if this task was being timed.
        let mut active = self.active_timer.lock();
        if active.as_deref() == Some(id) {
            *active = None;
        }
        drop(active);

        self.get_task(id)?
            .ok_or_else(|| anyhow!("task vanished after update: {id}"))
    }

    /// Updates a task's title and description.
    pub fn update_task(
        &self,
        id: &str,
        title: Option<String>,
        description: Option<String>,
        priority: Option<i32>,
        due_at: Option<Option<i64>>,
    ) -> Result<WorkTask> {
        let now = Utc::now().timestamp();
        // Load first so we can merge — call get_task BEFORE locking
        // to avoid deadlocking on the non-reentrant parking_lot Mutex.
        let current = self
            .get_task(id)?
            .ok_or_else(|| anyhow!("task not found: {id}"))?;
        let new_title = title.unwrap_or(current.title);
        let new_desc = description.unwrap_or(current.description);
        let new_priority = priority.map(|p| p.clamp(0, 3)).unwrap_or(current.priority);
        let new_due = due_at.unwrap_or(current.due_at);
        {
            let conn = self.sqlite.raw_connection();
            let conn = conn.lock();
            conn.execute(
                "UPDATE work_tasks SET title = ?1, description = ?2, priority = ?3, due_at = ?4, updated_at = ?5 WHERE id = ?6",
                params![new_title, new_desc, new_priority, new_due, now, id],
            )?;
        } // conn dropped here — lock released before calling self.get_task()
        self.get_task(id)?
            .ok_or_else(|| anyhow!("task vanished after update: {id}"))
    }

    /// Hard-deletes a task.  Returns `true` if a row was removed.
    pub fn delete_task(&self, id: &str) -> Result<bool> {
        let mut active = self.active_timer.lock();
        if active.as_deref() == Some(id) {
            *active = None;
        }
        drop(active);
        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        let n = conn.execute("DELETE FROM work_tasks WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Starts the timer for a task.  If another task is currently
    /// being timed, its timer is stopped first (the elapsed
    /// accumulator is finalised).
    pub fn start_timer(&self, id: &str) -> Result<WorkTask> {
        // First stop any other running timer.
        if let Some(prev) = self.active_timer.lock().clone() {
            if prev != id {
                self.stop_timer()?;
            }
        }
        let now = Utc::now().timestamp();
        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        // Store the timer-start timestamp in metadata; we use a
        // dedicated column-free approach for simplicity (the field
        // `time_spent_ms` is the accumulator).  For v0.5 we just
        // record that the timer is running via the in-memory mutex.
        let _ = conn.execute(
            "UPDATE work_tasks SET status = CASE WHEN status = 'todo' THEN 'doing' ELSE status END, updated_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        *self.active_timer.lock() = Some(id.to_string());
        drop(conn);
        self.get_task(id)?
            .ok_or_else(|| anyhow!("task vanished after start_timer: {id}"))
    }

    /// Stops the active timer (if any) and finalises the accumulator.
    /// Returns the task that was being timed, or `None` if no timer
    /// was running.
    pub fn stop_timer(&self) -> Result<Option<WorkTask>> {
        let id = { self.active_timer.lock().clone() };
        let Some(id) = id else {
            return Ok(None);
        };
        // In v0.5 we just clear the active pointer.  The accumulator
        // is updated by `add_time` from the front-end (which knows
        // the elapsed wall-clock duration).
        *self.active_timer.lock() = None;
        let task = self.get_task(&id)?;
        Ok(task)
    }

    /// Returns the id of the task currently being timed (if any).
    pub fn active_timer(&self) -> Option<String> {
        self.active_timer.lock().clone()
    }

    /// Adds `elapsed_ms` to the task's `time_spent_ms` accumulator.
    pub fn add_time(&self, id: &str, elapsed_ms: i64) -> Result<WorkTask> {
        if elapsed_ms < 0 {
            return Err(anyhow!("elapsed_ms must be non-negative"));
        }
        let now = Utc::now().timestamp();
        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        let n = conn.execute(
            "UPDATE work_tasks SET time_spent_ms = time_spent_ms + ?1, updated_at = ?2 WHERE id = ?3",
            params![elapsed_ms, now, id],
        )?;
        if n == 0 {
            return Err(anyhow!("task not found: {id}"));
        }
        drop(conn);
        self.get_task(id)?
            .ok_or_else(|| anyhow!("task vanished after add_time: {id}"))
    }

    /// Heuristic priority recommendation.  Scoring:
    ///
    /// * base 0
    /// * +2 if a `due_at` is set and is within 24h
    /// * +1 if `due_at` is set and is within 7d
    /// * +1 if the title contains any of the urgency keywords
    ///   ("紧急", "urgent", "asap", "!!!").
    ///
    /// The result is clamped to `[0, 3]` to match the priority range.
    pub fn recommend_priority(&self, title: &str, due_at: Option<i64>) -> i32 {
        recommend_priority(title, due_at)
    }

    /// Generates a meeting-minutes-style summary from a free-form
    /// transcript.  The implementation is intentionally simple:
    /// split on sentence boundaries, take the first 5 sentences as
    /// "decisions" and any line starting with `-` as an "action item".
    pub fn summarise_meeting(&self, transcript: &str) -> MeetingMinutes {
        summarise_meeting(transcript)
    }
}

/// Heuristic priority recommendation.  Pure function; safe to call
/// without a database.
pub fn recommend_priority(title: &str, due_at: Option<i64>) -> i32 {
    let mut score = 0i32;
    if let Some(due) = due_at {
        let now = Utc::now().timestamp();
        let delta = due - now;
        if delta < 24 * 3600 {
            score += 2;
        } else if delta < 7 * 24 * 3600 {
            score += 1;
        }
    }
    let lower = title.to_ascii_lowercase();
    if lower.contains("紧急")
        || lower.contains("urgent")
        || lower.contains("asap")
        || lower.contains("!!!")
    {
        score += 1;
    }
    score.clamp(0, 3)
}

/// Splits a meeting transcript into decisions and action items.  Pure
/// function; safe to call without a database.
pub fn summarise_meeting(transcript: &str) -> MeetingMinutes {
    let mut decisions: Vec<String> = Vec::new();
    let mut actions: Vec<String> = Vec::new();
    for raw in transcript.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('-') || line.starts_with('*') {
            actions.push(line.trim_start_matches(['-', '*', ' ']).to_string());
        } else if decisions.len() < 5 {
            decisions.push(line.to_string());
        }
    }
    MeetingMinutes { decisions, actions }
}

/// Auto-generated meeting minutes used by the Work UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeetingMinutes {
    pub decisions: Vec<String>,
    pub actions: Vec<String>,
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn row_to_task(
    conn: &rusqlite::Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Option<WorkTask>> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params)?;
    if let Some(row) = rows.next()? {
        Ok(Some(map_row(row)?))
    } else {
        Ok(None)
    }
}

fn row_to_tasks(
    conn: &rusqlite::Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<WorkTask>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, map_row)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkTask> {
    let status_s: String = row.get(3)?;
    let meta: Option<String> = row.get(10)?;
    Ok(WorkTask {
        id: row.get(0)?,
        title: row.get(1)?,
        description: row.get(2)?,
        status: TaskStatus::from_str(&status_s).unwrap_or(TaskStatus::Todo),
        priority: row.get(4)?,
        due_at: row.get(5)?,
        time_spent_ms: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        completed_at: row.get(9)?,
        metadata: meta.and_then(|m| serde_json::from_str(&m).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommend_priority_clamps_to_three() {
        let due = Utc::now().timestamp() + 3600;
        let p = recommend_priority("紧急: 修复 bug!!!", Some(due));
        assert!(p >= 2 && p <= 3, "expected >=2, got {p}");
    }

    #[test]
    fn recommend_priority_zero_for_normal_task() {
        assert_eq!(recommend_priority("写周报", None), 0);
    }

    #[test]
    fn meeting_minutes_extracts_actions() {
        let mm = summarise_meeting(
            "我们决定使用 Rust 重写。\n- 张三 完成方案设计\n- 李四 评审 PR\n我们将在下周一发布。\n",
        );
        assert_eq!(mm.actions.len(), 2);
        assert!(mm.decisions.iter().any(|d| d.contains("Rust")));
    }

    #[test]
    fn task_status_round_trip() {
        for s in [TaskStatus::Todo, TaskStatus::Doing, TaskStatus::Done] {
            assert_eq!(TaskStatus::from_str(s.as_str()).unwrap(), s);
        }
        assert!(TaskStatus::from_str("nope").is_err());
    }
}
