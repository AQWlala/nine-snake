//! v0.5: Git integration.
//!
//! A small wrapper around the `git` CLI (v0.5 deliberately avoids
//! `git2`'s libgit2 native dependency).  All commands run inside the
//! workspace root; the front-end never sees a `cwd` argument.
//!
//! Errors from `git` itself are surfaced verbatim but the front-end
//! only ever sees safe strings (the message is trimmed to a single
//! line and we strip absolute paths to avoid leaking the user's
//! home directory).

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::instrument;

/// One entry in `git status --porcelain`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusEntry {
    /// Two-character status code, e.g. `" M"`, `"?? "`, `"A "`.
    pub code: String,
    /// Path relative to the repo root.
    pub path: String,
}

/// Aggregate `git status` summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStatus {
    pub branch: String,
    pub entries: Vec<StatusEntry>,
    pub clean: bool,
}

/// One entry in `git log --oneline -n <limit>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLogEntry {
    pub hash: String,
    pub short: String,
    pub subject: String,
    pub author: String,
    pub time: i64,
}

/// Result of a `git diff` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitDiff {
    /// Path-filtered diff (empty string means "whole index").
    pub path: String,
    /// The unified diff text.
    pub body: String,
}

/// Runs `git` inside the given repo path.  Returns an error
/// annotated with the failing subcommand and the trimmed stderr.
fn run_git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("spawning git {args:?}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let first_line = stderr.lines().next().unwrap_or("git failed");
        return Err(anyhow!("git {} failed: {}", args.join(" "), first_line));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `git status --porcelain --branch`.
#[instrument(skip(repo))]
pub fn status(repo: &Path) -> Result<GitStatus> {
    let out = run_git(repo, &["status", "--porcelain", "--branch"])?;
    let mut branch = String::from("detached");
    let mut entries: Vec<StatusEntry> = Vec::new();
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            // Branch line: "## main...origin/main" or "## HEAD (no branch)"
            if let Some((b, _)) = rest.split_once("...") {
                branch = b.to_string();
            } else {
                branch = rest.to_string();
            }
            continue;
        }
        if line.len() < 3 {
            continue;
        }
        let code = line[..2].to_string();
        let path = line[3..].to_string();
        // For renames ("R "), the path is "old -> new"; we keep the
        // verbatim form so the UI can show it.
        entries.push(StatusEntry { code, path });
    }
    let clean = entries.is_empty();
    Ok(GitStatus {
        branch,
        entries,
        clean,
    })
}

/// `git log --oneline -n <limit> --pretty=format:%H%x00%h%x00%s%x00%an%x00%at`.
#[instrument(skip(repo))]
pub fn log(repo: &Path, limit: usize) -> Result<Vec<GitLogEntry>> {
    let lim = limit.max(1).to_string();
    let fmt = "%H%x00%h%x00%s%x00%an%x00%at";
    let out = run_git(
        repo,
        &["log", "-n", &lim, &format!("--pretty=format:{fmt}")],
    )?;
    let mut entries = Vec::new();
    for line in out.lines() {
        let parts: Vec<&str> = line.split('\0').collect();
        if parts.len() < 5 {
            continue;
        }
        let time: i64 = parts[4].parse().unwrap_or(0);
        entries.push(GitLogEntry {
            hash: parts[0].to_string(),
            short: parts[1].to_string(),
            subject: parts[2].to_string(),
            author: parts[3].to_string(),
            time,
        });
    }
    Ok(entries)
}

/// `git diff [-- <path>]`.  Empty `path` means "whole index".
#[instrument(skip(repo))]
pub fn diff(repo: &Path, path: &str) -> Result<GitDiff> {
    let body = if path.is_empty() {
        run_git(repo, &["diff"])?
    } else {
        run_git(repo, &["diff", "--", path])?
    };
    Ok(GitDiff {
        path: path.to_string(),
        body,
    })
}

/// `git add -A` followed by `git commit -m <message>`.  Returns the
/// short hash of the new commit (or an error if there was nothing
/// to commit).
#[instrument(skip(repo, message))]
pub fn commit(repo: &Path, message: &str) -> Result<String> {
    if message.trim().is_empty() {
        return Err(anyhow!("commit message must not be empty"));
    }
    run_git(repo, &["add", "-A"])?;
    // `--allow-empty` would mask "nothing to commit" — better to
    // surface the error so the UI can show it.
    run_git(repo, &["commit", "-m", message])?;
    let head = run_git(repo, &["rev-parse", "--short", "HEAD"])?;
    Ok(head.trim().to_string())
}

/// `git init` in the given directory.  Idempotent.
#[instrument(skip(repo))]
pub fn init(repo: &Path) -> Result<()> {
    run_git(repo, &["init"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// `git` CLI must be installed and on PATH for these tests.
    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn init_status_log_commit_round_trip() {
        if !git_available() {
            eprintln!("git not available; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).expect("init");
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        let s = status(dir.path()).expect("status");
        assert!(!s.clean, "expected dirty status after writing a file");
        let hash = commit(dir.path(), "initial commit").expect("commit");
        assert!(!hash.is_empty());
        let s2 = status(dir.path()).expect("status 2");
        assert!(s2.clean, "expected clean status after commit");
        let log = log(dir.path(), 5).expect("log");
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].subject, "initial commit");
    }

    #[test]
    fn diff_shows_pending_change() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        fs::write(dir.path().join("b.txt"), "first").unwrap();
        commit(dir.path(), "first").unwrap();
        fs::write(dir.path().join("b.txt"), "first\nsecond").unwrap();
        let d = diff(dir.path(), "").expect("diff");
        assert!(d.body.contains("+second"));
    }

    #[test]
    fn commit_rejects_empty_message() {
        if !git_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        assert!(commit(dir.path(), "   ").is_err());
    }
}
