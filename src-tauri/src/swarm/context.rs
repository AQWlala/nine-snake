//! Shared team context that every agent in a swarm reads from and writes
//! to. The context is in-process; persistence to memory is handled by
//! the orchestrator after the team finishes its work.

use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// One entry inside the shared team context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEntry {
    /// Author agent (or "user" / "system").
    pub author: String,
    /// Optional human-readable label.
    pub label: String,
    /// Free-form text body.
    pub body: String,
    /// Unix timestamp (seconds).
    pub created_at: i64,
}

impl ContextEntry {
    pub fn new(
        author: impl Into<String>,
        label: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            author: author.into(),
            label: label.into(),
            body: body.into(),
            created_at: chrono::Utc::now().timestamp(),
        }
    }
}

/// Thread-safe team context.
#[derive(Clone, Default)]
pub struct TeamContext {
    inner: Arc<RwLock<Vec<ContextEntry>>>,
}

impl TeamContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a new entry to the context.
    pub fn push(&self, entry: ContextEntry) {
        self.inner.write().push(entry);
    }

    /// Convenience: appends a `ContextEntry` constructed from raw parts.
    pub fn push_str(&self, author: &str, label: &str, body: &str) {
        self.push(ContextEntry::new(author, label, body));
    }

    /// Returns a snapshot of the current context.
    pub fn snapshot(&self) -> Vec<ContextEntry> {
        self.inner.read().clone()
    }

    /// Returns the most recent `n` entries (most-recent-last).
    pub fn tail(&self, n: usize) -> Vec<ContextEntry> {
        let g = self.inner.read();
        let start = g.len().saturating_sub(n);
        g[start..].to_vec()
    }

    /// Renders the context as a single string suitable for inclusion in
    /// an LLM prompt. Entries are joined with `\n---\n` separators.
    pub fn render(&self) -> String {
        let g = self.inner.read();
        g.iter()
            .map(|e| format!("[{}] {}: {}", e.created_at, e.author, e.body))
            .collect::<Vec<_>>()
            .join("\n---\n")
    }

    /// Number of entries currently in the context.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// True when the context contains no entries.
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_snapshot_round_trip() {
        let ctx = TeamContext::new();
        ctx.push_str("user", "q", "what is Rust?");
        ctx.push_str("coder", "draft", "Rust is a systems language.");
        let snap = ctx.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].author, "user");
    }

    #[test]
    fn tail_returns_most_recent() {
        let ctx = TeamContext::new();
        for i in 0..5 {
            ctx.push_str("a", &format!("n{i}"), "x");
        }
        let t = ctx.tail(2);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].label, "n3");
        assert_eq!(t[1].label, "n4");
    }

    #[test]
    fn render_is_human_readable() {
        let ctx = TeamContext::new();
        ctx.push_str("a", "l", "b");
        let r = ctx.render();
        assert!(r.contains("["));
        assert!(r.contains("a"));
    }
}
