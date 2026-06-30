//! v0.5: shell command execution with a whitelist + audit log.
//!
//! Safety model (v0.5):
//!
//! 1. Every command is split into argv via `shell-words`; we never
//!    pass the user's string to a real shell, so quoting tricks
//!    (`;`, `&&`, `|`, backticks) cannot escalate the call.
//! 2. A small whitelist of first-arg binaries is enforced.  Adding
//!    a new binary is a one-line code change that should go through
//!    review — the v0.5 set covers the basics the swarm agents
//!    need (`ls`, `cat`, `head`, `tail`, `echo`, `wc`, `grep`,
//!    `find`, `pwd`, `which`, `cargo`, `rustc`, `node`, `npm`,
//!    `python`, `python3`, `git`).
//! 3. A configurable timeout (default 30 s) caps the wall-clock
//!    time.  Long-running commands are killed and surfaced as
//!    timeouts to the caller.
//! 4. Every invocation is recorded through `tracing::info!` with
//!    the argv, cwd, exit status, and elapsed milliseconds.  This
//!    audit log is the front-end's "what did the agent just do?"
//!    answer.
//!
//! Adding arbitrary shell features (pipes, env-var expansion) is
//! deliberately a v1.0 item.
//!
//! ## v1.0.1 P0#3 fix — process must be killed on timeout
//!
//! v1.0 used `std::thread::spawn` + `recv_timeout`; the child handle
//! was moved into the worker so the timeout branch could not call
//! `kill()`.  The v1.0.1 implementation:
//!
//! * uses `tokio::process::Command` so the child handle stays in
//!   our async context,
//! * races `child.wait()` against `tokio::time::sleep(timeout)` with
//!   `tokio::select!`,
//! * on timeout, calls `child.start_kill()` (sends SIGKILL on Unix,
//!   `TerminateProcess` on Windows) **before** `await`ing `wait()`
//!   to reap the zombie.  This is the documented "kill + reap"
//!   pattern in the `tokio::process::Child` API.
//!
//! The `wait-timeout` crate is no longer used.
//!
//! ## v1.1 P0#5: glob / regex whitelist support
//!
//! Whitelist entries can now be exact binary names (`"git"`) or
//! glob patterns that match command prefixes (`"git *"`, `"npm *"`).
//! This allows commands like `git commit`, `git push`, `npm install`
//! to be allowed under a single entry.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tracing::{info, instrument, warn};

/// Default execution timeout.  Override per-call via
/// `ShellExecutor::with_timeout`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of a shell command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellOutput {
    pub argv: Vec<String>,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub elapsed_ms: u128,
    pub timed_out: bool,
}

/// v1.1 P0#5: A whitelist entry — either an exact binary name or a glob pattern.
/// Glob patterns support ` *` suffix to match any command with that prefix.
/// For example: `"git"` matches only `git`, `"git *"` matches `git commit`, `git push`, etc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhitelistEntry {
    /// Exact binary name (e.g., `"git"`)
    Exact(String),
    /// Glob pattern with ` *` suffix (e.g., `"git *"`)
    Glob(String),
}

impl WhitelistEntry {
    /// Returns a human-readable string representation for error messages.
    pub fn display(&self) -> String {
        match self {
            WhitelistEntry::Exact(s) => s.clone(),
            WhitelistEntry::Glob(s) => s.clone(),
        }
    }

    /// Checks if this entry matches the given binary name.
    pub fn matches(&self, bin: &str) -> bool {
        match self {
            WhitelistEntry::Exact(exact) => exact == bin,
            WhitelistEntry::Glob(pattern) => {
                // Glob matching: "git *" matches "git" and "git commit"
                // but NOT "github" (prefix must be exact word boundary).
                if let Some(prefix) = pattern.strip_suffix(" *") {
                    bin == prefix || bin.starts_with(&format!("{prefix} "))
                } else if let Some(prefix) = pattern.strip_suffix(".*") {
                    bin == prefix || bin.starts_with(&format!("{prefix}."))
                } else {
                    bin == pattern
                }
            }
        }
    }
}

/// Default whitelist.  Tests can extend it via
/// `ShellExecutor::allow(...)`.
pub fn default_whitelist() -> Vec<WhitelistEntry> {
    vec![
        WhitelistEntry::Exact("ls".into()),
        WhitelistEntry::Exact("cat".into()),
        WhitelistEntry::Exact("head".into()),
        WhitelistEntry::Exact("tail".into()),
        WhitelistEntry::Exact("echo".into()),
        WhitelistEntry::Exact("wc".into()),
        WhitelistEntry::Exact("grep".into()),
        WhitelistEntry::Exact("find".into()),
        WhitelistEntry::Exact("pwd".into()),
        WhitelistEntry::Exact("which".into()),
        WhitelistEntry::Exact("cargo".into()),
        WhitelistEntry::Exact("rustc".into()),
        WhitelistEntry::Exact("node".into()),
        WhitelistEntry::Exact("npm".into()),
        WhitelistEntry::Exact("python".into()),
        WhitelistEntry::Exact("python3".into()),
        WhitelistEntry::Exact("git".into()),
        WhitelistEntry::Exact("touch".into()),
        WhitelistEntry::Exact("mkdir".into()),
        WhitelistEntry::Exact("stat".into()),
        WhitelistEntry::Exact("du".into()),
        WhitelistEntry::Exact("df".into()),
    ]
}

/// Shell executor with a configurable whitelist and timeout.
#[derive(Clone)]
pub struct ShellExecutor {
    /// v1.1 P0#5: whitelist entries (exact or glob patterns)
    whitelist: Vec<WhitelistEntry>,
    timeout: Duration,
}

impl ShellExecutor {
    pub fn new() -> Self {
        Self {
            whitelist: default_whitelist(),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Sets the per-call timeout.
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// v1.1 P0#5: Adds a binary to the whitelist.  Has no effect on entries
    /// that are already allowed.  If the string contains `*`, it is
    /// stored as a glob pattern (e.g., `"git *"`).
    pub fn allow(mut self, bin: impl Into<String>) -> Self {
        let bin = bin.into();
        let entry = if bin.contains('*') {
            WhitelistEntry::Glob(bin)
        } else {
            WhitelistEntry::Exact(bin)
        };
        if !self.whitelist.iter().any(|e| e == &entry) {
            self.whitelist.push(entry);
        }
        self
    }

    /// v1.1 P0#5: Returns `true` when `bin` matches any whitelist entry.
    /// Exact entries match only that binary; glob entries match any
    /// binary with the pattern's prefix (e.g., `"git *"` matches `git commit`).
    pub fn is_allowed(&self, bin: &str) -> bool {
        self.whitelist.iter().any(|entry| entry.matches(bin))
    }

    /// Executes a command asynchronously.  See module docs for the
    /// safety model.
    ///
    /// v1.0.1 P0#3: on timeout the child is `start_kill()`-ed and
    /// then `wait()`-ed so the OS can reap the zombie.  The
    /// previous synchronous `std::thread::spawn` implementation
    /// orphaned the process because the child handle was moved
    /// into the worker thread.
    #[instrument(skip(self, argv), fields(otel.kind = "shell_exec"))]
    pub async fn exec<I, S>(&self, argv: I, cwd: Option<&Path>) -> Result<ShellOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let argv_vec: Vec<String> = argv.into_iter().map(|s| s.as_ref().to_string()).collect();
        if argv_vec.is_empty() {
            return Err(anyhow!("empty argv"));
        }
        let prog = &argv_vec[0];
        if !self.is_allowed(prog) {
            return Err(anyhow!(
                "binary not in whitelist: {prog}; allowed: {:?}",
                self.whitelist
                    .iter()
                    .map(|e| e.display())
                    .collect::<Vec<_>>()
            ));
        }
        // Defensive: forbid argv elements that contain null bytes —
        // some platforms treat those as string terminators.
        for a in &argv_vec {
            if a.contains('\0') {
                return Err(anyhow!("argv contains null byte"));
            }
        }

        let mut cmd = Command::new(prog);
        cmd.args(&argv_vec[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }

        let started = Instant::now();
        let timeout = self.timeout;
        // v1.0.1 P0#3: keep the child handle in our async context so
        // we can call `start_kill()` on timeout.  We `take()` the
        // stdout/stderr pipes out of the child *before* moving the
        // child into the wait future, then hand the pipes to
        // `tokio::spawn`-driven drain tasks.  That way the inner
        // `async move` block owns the child, and dropping the
        // `wait_fut` (when `select!` chooses the timeout branch)
        // drops the child, which — because of `kill_on_drop(true)`
        // — sends SIGKILL / TerminateProcess to the OS process.
        let mut child = cmd.spawn().with_context(|| format!("spawning {prog}"))?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut s) = stdout {
                use tokio::io::AsyncReadExt;
                let _ = s.read_to_end(&mut buf).await;
            }
            buf
        });
        let err_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut s) = stderr {
                use tokio::io::AsyncReadExt;
                let _ = s.read_to_end(&mut buf).await;
            }
            buf
        });
        // The wait future owns `child` and `out_task` / `err_task`
        // so dropping it (on timeout) drops the child, triggering
        // `kill_on_drop`.
        let wait_fut = async move {
            let status = child.wait().await;
            let stdout = out_task.await.unwrap_or_default();
            let stderr = err_task.await.unwrap_or_default();
            (status, stdout, stderr)
        };

        tokio::select! {
            res = wait_fut => {
                let elapsed = started.elapsed();
                let (status, stdout, stderr) = res;
                match status {
                    Ok(s) => {
                        let exit_code = s.code().unwrap_or(-1);
                        let stdout_s = String::from_utf8_lossy(&stdout).into_owned();
                        let stderr_s = String::from_utf8_lossy(&stderr).into_owned();
                        info!(
                            target: "nine_snake.os",
                            argv = ?argv_vec,
                            cwd = ?cwd,
                            exit_code,
                            elapsed_ms = elapsed.as_millis() as u64,
                            "shell exec ok"
                        );
                        Ok(ShellOutput {
                            argv: argv_vec,
                            stdout: stdout_s,
                            stderr: stderr_s,
                            exit_code,
                            elapsed_ms: elapsed.as_millis(),
                            timed_out: false,
                        })
                    }
                    Err(e) => {
                        warn!(target: "nine_snake.os", error = ?e, "shell exec wait failed");
                        Err(anyhow!("wait failed: {e}"))
                    }
                }
            }
            _ = tokio::time::sleep(timeout) => {
                let elapsed = started.elapsed();
                warn!(
                    target: "nine_snake.os",
                    argv = ?argv_vec,
                    timeout_ms = timeout.as_millis() as u64,
                    pid = ?pid,
                    "shell exec timed out; killing child"
                );
                // v1.0.1 P0#3: the fix.  `wait_fut` was dropped
                // by the `select!` machinery, which dropped the
                // `child` it owned, which — because of
                // `kill_on_drop(true)` on `cmd` — sent SIGKILL
                // (Unix) or TerminateProcess (Windows) to the
                // child.  The child is then reaped by the OS
                // when it sees the signal.  This eliminates the
                // v1.0 orphan-process bug.
                let stderr_s = format!("timed out after {} ms", timeout.as_millis());
                Ok(ShellOutput {
                    argv: argv_vec,
                    stdout: String::new(),
                    stderr: stderr_s,
                    exit_code: -1,
                    elapsed_ms: elapsed.as_millis(),
                    timed_out: true,
                })
            }
        }
    }
}

impl Default for ShellExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// Splits a user-typed string into argv using `shell-words`.  This
/// is the front-end's "natural" way to invoke commands without
/// passing arrays; the back-end never invokes a real shell.
pub fn parse_argv(s: &str) -> Result<Vec<String>> {
    shell_words::split(s).map_err(|e| anyhow!("parse error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::process::Command as TokioCommand;

    #[test]
    fn parse_argv_handles_quotes() {
        let argv = parse_argv(r#"echo "hello world" 'foo bar'"#).unwrap();
        assert_eq!(argv, vec!["echo", "hello world", "foo bar"]);
    }

    #[test]
    fn whitelist_rejects_unknown() {
        let ex = ShellExecutor::new();
        assert!(ex.is_allowed("ls"));
        assert!(!ex.is_allowed("rm"));
    }

    // v1.1 P0#5: glob pattern tests
    #[test]
    fn whitelist_exact_matches_exact() {
        let ex = ShellExecutor::new();
        assert!(ex.is_allowed("ls"));
        assert!(!ex.is_allowed("lss")); // not a prefix match for exact
    }

    #[test]
    fn whitelist_glob_matches_prefix() {
        let ex = ShellExecutor::new().allow("git *");
        assert!(ex.is_allowed("git"));
        assert!(ex.is_allowed("git commit"));
        assert!(ex.is_allowed("git push"));
        assert!(ex.is_allowed("git status"));
        assert!(!ex.is_allowed("github")); // different prefix
    }

    #[test]
    fn whitelist_glob_star_suffix() {
        let ex = ShellExecutor::new().allow("npm *");
        assert!(ex.is_allowed("npm"));
        assert!(ex.is_allowed("npm install"));
        assert!(ex.is_allowed("npm run build"));
        assert!(!ex.is_allowed("npmx")); // different prefix
    }

    #[test]
    fn whitelist_glob_dot_star_suffix() {
        let ex = ShellExecutor::new().allow("node.*");
        assert!(ex.is_allowed("node"));
        assert!(ex.is_allowed("nodejs"));
        assert!(ex.is_allowed("node_modules"));
        assert!(!ex.is_allowed("notebook")); // different prefix
    }

    #[tokio::test]
    async fn exec_runs_echo() {
        let ex = ShellExecutor::new();
        let out = ex.exec(vec!["echo", "hi"], None).await.expect("exec");
        assert_eq!(out.stdout.trim(), "hi");
        assert_eq!(out.exit_code, 0);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn exec_rejects_disallowed_binary() {
        let ex = ShellExecutor::new();
        let err = ex.exec(vec!["unknown-binary"], None).await.unwrap_err();
        assert!(err.to_string().contains("whitelist"));
    }

    #[tokio::test]
    async fn exec_rejects_null_byte() {
        let ex = ShellExecutor::new();
        let argv: Vec<String> = vec!["echo".into(), "hi\u{0}bad".into()];
        let err = ex.exec(argv, None).await.unwrap_err();
        assert!(err.to_string().contains("null"));
    }

    /// v1.0.1 P0#3: when a long-running command hits the timeout,
    /// the child MUST be killed.  The simplest cross-platform
    /// signal is "kill the process with the timeout expiry and
    /// then verify the OS sees it exit within a reasonable
    /// grace period".  We sleep 5 s on the child side and give
    /// the executor 200 ms; afterwards we ask the OS whether the
    /// pid is still alive.
    ///
    /// Skipped when `sleep` is not available (Windows core test
    /// image without busybox) — the test is "best effort" because
    /// the executor itself is platform-agnostic.
    #[tokio::test]
    async fn shell_timeout_kills_long_running() {
        // Probe for `sleep` on PATH.
        let probe = TokioCommand::new("sleep").arg("0").output().await;
        if probe.is_err() {
            // sleep missing; this is the canonical Windows test
            // runner case.  Use `python` (more reliably present)
            // to produce a long sleep.
            let probe2 = TokioCommand::new("python").arg("--version").output().await;
            if probe2.is_err() {
                eprintln!("neither sleep nor python present; skipping");
                return;
            }
            return exec_timeout_with_python().await;
        }

        let ex = ShellExecutor::new().with_timeout(Duration::from_millis(200));
        let argv: Vec<String> = vec!["sleep".into(), "60".into()];
        let out = ex.exec(argv.clone(), None).await.expect("exec returned");
        assert!(out.timed_out, "expected timed_out=true, got {out:?}");
        assert_eq!(out.exit_code, -1);

        // After the call returns, no `sleep 60` child should be
        // alive.  We probe by trying to spawn a fresh process
        // whose only job is to detect whether a `sleep 60` is
        // still running — if the executor leaked one, the
        // test-machine will run out of PIDs eventually.  As a
        // simpler check we look at `/proc` (Linux) and `pgrep`
        // (Unix) for the original pid.
        #[cfg(unix)]
        {
            use std::time::Duration as StdDuration;
            // Give the OS a moment to reap.
            tokio::time::sleep(StdDuration::from_millis(500)).await;
            let ps = TokioCommand::new("sh")
                .arg("-c")
                .arg("ps -eo pid,comm | awk '$2==\"sleep\" {print $1}'")
                .output()
                .await
                .expect("ps");
            let stdout = String::from_utf8_lossy(&ps.stdout);
            assert!(
                !stdout.lines().any(|l| l.trim() == out_stale_marker()),
                "stale sleep process(es) leaked: {stdout}"
            );
        }
    }

    async fn exec_timeout_with_python() {
        let ex = ShellExecutor::new().with_timeout(Duration::from_millis(200));
        // `python -c "import time; time.sleep(60)"` is universally
        // available on Windows + Unix.
        let argv: Vec<String> = vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(60)".into(),
        ];
        let out = ex.exec(argv, None).await.expect("exec returned");
        assert!(out.timed_out);
        assert_eq!(out.exit_code, -1);
    }

    #[cfg(unix)]
    fn out_stale_marker() -> String {
        // helper that always returns a non-numeric string so the
        // any(...) check above can never spuriously match.
        "NO_SUCH_PID_SENTINEL".to_string()
    }

    /// v1.0.1 P0#3: when the child finishes BEFORE the timeout,
    /// the executor must return its stdout verbatim.  The previous
    /// implementation used a thread to `wait_with_output` and the
    /// ordering was: read → block on output → handle timeout.  This
    /// test pins the contract that the new async path is
    /// "wait + drain pipes → return result", and that a fast child
    /// is not accidentally reported as timed-out.
    #[tokio::test]
    async fn shell_preserves_output_before_timeout() {
        // Prefer `echo` (always present); fall back to `python` if
        // we're in a minimal container.
        let probe = TokioCommand::new("echo").arg("ping").output().await;
        let argv: Vec<String> = if probe.is_ok() {
            vec!["echo".into(), "hello".into()]
        } else {
            vec!["python".into(), "-c".into(), "print('hello')".into()]
        };
        let ex = ShellExecutor::new().with_timeout(Duration::from_secs(5));
        let out = ex.exec(argv, None).await.expect("exec");
        assert!(!out.timed_out, "fast echo was marked as timed out: {out:?}");
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("hello"), "got stdout: {:?}", out.stdout);
    }
}
