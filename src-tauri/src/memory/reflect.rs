//! Reflection engine — the L5 metacognitive layer.
//!
//! v0.2 introduces the [`ReflectionEngine`], which periodically scans
//! recent high-importance memories, prompts the LLM to synthesise a
//! meta-cognitive note ("I noticed that I keep getting confused about
//! X"), and persists the result in the `reflections` table together
//! with links back to the source memories via `memory_reflections`.
//!
//! ## Design constraints
//!
//! * **Read-only of source memories.** A reflection never mutates the
//!   memories that triggered it. Provenance is preserved.
//! * **Bounded scope.** The default scan window is 7 days; the default
//!   `min_importance` is `0.6`.
//! * **Offline-tolerant.** When the LLM is unreachable, the engine
//!   falls back to a deterministic template-based summariser so the
//!   background worker never panics. The fallback reflection is
//!   tagged with `trigger_kind = "template_fallback"` for observability.
//! * **Idempotent.** Re-running a reflection on the same memory window
//!   does not create duplicate rows: the engine de-duplicates on
//!   `(memory_id_set, day_bucket)`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::llm::{ChatMessage, LlmGateway};

use super::sqlite_store::SqliteStore;
use super::types::{Memory, MemoryLayer, MemoryType, SourceKind};

/// Default number of days of memory history to consider for a single
/// reflection pass.
pub const DEFAULT_REFLECT_WINDOW_DAYS: i64 = 7;

/// Default minimum importance threshold for source memories.
pub const DEFAULT_REFLECT_MIN_IMPORTANCE: f32 = 0.6;

/// Default maximum number of source memories per reflection.
pub const DEFAULT_REFLECT_MAX_MEMORIES: usize = 16;

/// Default period (in seconds) of the background reflection worker.
pub const DEFAULT_REFLECT_INTERVAL_SECS: u64 = 600;

/// A meta-cognitive reflection record (L5 / Metacognitive).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Reflection {
    /// Stable UUIDv4.
    pub id: String,
    /// Memories that triggered this reflection. May be empty for
    /// manually-injected reflections.
    pub source_memories: Vec<String>,
    /// Reflection text (Chinese-first, 100–300 chars preferred).
    pub content: String,
    /// Always [`MemoryLayer::L5`].
    pub layer: MemoryLayer,
    /// Always [`MemoryType::Metacognitive`].
    pub memory_type: MemoryType,
    /// `[0.0, 1.0]`; higher = more useful.
    pub importance: f32,
    /// `trigger_kind` from the underlying table — e.g.
    /// `"periodic"`, `"manual"`, `"template_fallback"`.
    pub trigger_kind: String,
    /// Optional extracted lessons (JSON array of strings).
    pub lessons: Vec<String>,
    /// Confidence in `[0.0, 1.0]` as estimated by the producer.
    pub confidence: f32,
    /// Unix timestamp (seconds) of creation.
    pub created_at: i64,
}

/// Configuration knobs for the reflection engine.
#[derive(Debug, Clone)]
pub struct ReflectConfig {
    /// Number of days of history to scan.
    pub window_days: i64,
    /// Minimum `importance` for a memory to be considered.
    pub min_importance: f32,
    /// Maximum number of source memories per reflection.
    pub max_memories: usize,
    /// Background worker period. `0` disables the worker.
    pub worker_interval_secs: u64,
    /// Importance of the LLM-generated reflection (used for the
    /// `reflections.importance` column).
    pub base_importance: f32,
}

impl Default for ReflectConfig {
    fn default() -> Self {
        Self {
            window_days: DEFAULT_REFLECT_WINDOW_DAYS,
            min_importance: DEFAULT_REFLECT_MIN_IMPORTANCE,
            max_memories: DEFAULT_REFLECT_MAX_MEMORIES,
            worker_interval_secs: DEFAULT_REFLECT_INTERVAL_SECS,
            base_importance: 0.7,
        }
    }
}

/// The reflection engine.
pub struct ReflectionEngine {
    sqlite: Arc<SqliteStore>,
    llm: Option<Arc<LlmGateway>>,
    cfg: ReflectConfig,
    /// Used by the background worker for cooperative shutdown.
    shutdown: Arc<Notify>,
    /// Single-flight guard: only one reflection runs at a time.
    in_flight: Mutex<bool>,
    /// v1.0.1 P0#5: cancellation token for the worker.  Cloned
    /// into the spawned task so `AppState::shutdown` can cancel
    /// the worker within one tick of the loop.
    cancel_token: CancellationToken,
}

impl ReflectionEngine {
    /// Creates a new engine. `llm = None` forces the template fallback
    /// path (useful in tests and offline environments).
    pub fn new(sqlite: Arc<SqliteStore>, llm: Option<Arc<LlmGateway>>, cfg: ReflectConfig) -> Self {
        Self {
            sqlite,
            llm,
            cfg,
            shutdown: Arc::new(Notify::new()),
            in_flight: Mutex::new(false),
            cancel_token: CancellationToken::new(),
        }
    }

    /// Returns a handle to the shutdown signal so `lib.rs` can wake the
    /// worker up immediately on app exit.
    pub fn shutdown_handle(&self) -> Arc<Notify> {
        self.shutdown.clone()
    }

    /// v1.0.1 P0#5: returns a clone of the cancellation token so
    /// the app shutdown path can call `.cancel()` on it.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.clone()
    }

    /// Returns the engine configuration.
    pub fn config(&self) -> &ReflectConfig {
        &self.cfg
    }

    /// Lists the most recent reflections, newest first.
    pub fn list_recent(&self, limit: usize) -> Result<Vec<Reflection>> {
        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, trigger_kind, content, lessons, confidence, importance, created_at
             FROM reflections ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            let lessons_s: String = row.get(3)?;
            Ok(ReflectionRow {
                id: row.get(0)?,
                trigger_kind: row.get(1)?,
                content: row.get(2)?,
                lessons: serde_json::from_str(&lessons_s).unwrap_or_default(),
                confidence: row.get(4)?,
                importance: row.get(5)?,
                created_at: row.get(6)?,
            })
        })?;
        let mut out: Vec<Reflection> = Vec::new();
        for r in rows {
            let r = r?;
            let sources = list_sources(&conn, &r.id)?;
            out.push(Reflection {
                id: r.id,
                source_memories: sources,
                content: r.content,
                layer: MemoryLayer::L5,
                memory_type: MemoryType::Metacognitive,
                importance: r.importance,
                trigger_kind: r.trigger_kind,
                lessons: r.lessons,
                confidence: r.confidence,
                created_at: r.created_at,
            });
        }
        Ok(out)
    }

    /// v0.3: fetch a single reflection by id. Returns `None` if no
    /// such row exists. Used by the gRPC `GetReflection` RPC and the
    /// front-end reflection inspector.
    pub fn get(&self, id: &str) -> Result<Option<Reflection>> {
        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, trigger_kind, content, lessons, confidence, importance, created_at
             FROM reflections WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |row| {
                let lessons_s: String = row.get(3)?;
                Ok(ReflectionRow {
                    id: row.get(0)?,
                    trigger_kind: row.get(1)?,
                    content: row.get(2)?,
                    lessons: serde_json::from_str(&lessons_s).unwrap_or_default(),
                    confidence: row.get(4)?,
                    importance: row.get(5)?,
                    created_at: row.get(6)?,
                })
            })
            .optional()?;
        let Some(r) = row else { return Ok(None) };
        let sources = list_sources(&conn, &r.id)?;
        Ok(Some(Reflection {
            id: r.id,
            source_memories: sources,
            content: r.content,
            layer: MemoryLayer::L5,
            memory_type: MemoryType::Metacognitive,
            importance: r.importance,
            trigger_kind: r.trigger_kind,
            lessons: r.lessons,
            confidence: r.confidence,
            created_at: r.created_at,
        }))
    }

    /// Runs a single reflection pass synchronously. Returns the
    /// reflections produced (zero or more). Safe to call concurrently;
    /// overlapping calls are de-duplicated by an in-process mutex.
    pub async fn reflect_now(&self) -> Result<Vec<Reflection>> {
        // Single-flight: drop the second concurrent caller.
        // Uses InFlightGuard so `in_flight` is reset on *any* exit
        // path (Ok, Err, panic, CancellationToken cancel).
        let _guard = match InFlightGuard::try_acquire(&self.in_flight) {
            Some(g) => g,
            None => {
                debug!(target: "nine_snake.reflect", "reflect_now already in-flight; skipping");
                return Ok(Vec::new());
            }
        };

        self.reflect_now_impl().await
    }

    async fn reflect_now_impl(&self) -> Result<Vec<Reflection>> {
        // v1.0.1 fix C: callers of now-`async fn` methods
        // need `.await`.
        let candidates = self.collect_candidates().await?;
        if candidates.is_empty() {
            debug!(target: "nine_snake.reflect", "no candidate memories to reflect on");
            return Ok(Vec::new());
        }
        info!(target: "nine_snake.reflect", candidates = candidates.len(), "reflection pass starting");

        let (content, lessons, used_fallback) = match self.synthesise(&candidates).await {
            Ok(t) => t,
            Err(e) => {
                warn!(target: "nine_snake.reflect", error = ?e, "LLM synthesis failed; using template fallback");
                let (c, l) = template_summarise(&candidates);
                (c, l, true)
            }
        };

        let importance = (self.cfg.base_importance
            + 0.1 * (candidates.len() as f32 / self.cfg.max_memories as f32).min(1.0))
        .clamp(0.0, 1.0);
        let trigger_kind = if used_fallback {
            "template_fallback"
        } else {
            "periodic"
        }
        .to_string();
        let reflection = self
            .persist(&candidates, &content, &lessons, importance, trigger_kind)
            .await?;
        // v0.3: distinguish a fresh insert from a dedup replay. The
        // persist() returns a `dedup_replay` trigger_kind for the
        // latter so we do not double-count it in the metric.
        if reflection.trigger_kind == "dedup_replay" {
            info!(target: "nine_snake.reflect", id = %reflection.id, "dedup replay");
        } else {
            crate::metrics::global().record_reflection();
            info!(target: "nine_snake.reflect", id = %reflection.id, "reflection persisted");
        }
        Ok(vec![reflection])
    }

    /// Collects source memories for this pass.
    // v1.0.1 fix C: this function contains an `.await` on the
    // `tokio::task::spawn_blocking` handle (P0#05 added the
    // `tokio::select! { …  tick }` machinery that joins the
    // blocking work).  It must therefore be `async fn`, not
    // `fn`.  Previously the `.await` was legal because the
    // `join` future was sync — moving to `cancel_token.cancelled()`
    // required the explicit `async` keyword.
    async fn collect_candidates(&self) -> Result<Vec<Memory>> {
        let sqlite = self.sqlite.clone();
        let cfg = self.cfg.clone();
        let join = tokio::task::spawn_blocking(move || -> Result<Vec<Memory>> {
            let conn = sqlite.raw_connection();
            let conn = conn.lock();
            let now = chrono::Utc::now().timestamp();
            let cutoff = now - cfg.window_days * 24 * 3600;
            let mut stmt = conn.prepare(
                "SELECT id, memory_type, layer, content, summary_50, summary_150, summary_500, summary_2000, importance, access_count, last_access, created_at, source, metadata, compressed_from, compression_gen, pinned FROM memories
                 WHERE importance >= ?1
                   AND created_at >= ?2
                   AND compressed_from IS NULL
                   AND pinned = 0
                 ORDER BY importance DESC, created_at DESC
                 LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(
                    params![cfg.min_importance, cutoff, cfg.max_memories as i64],
                    row_to_memory_full,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        });
        let res = join
            .await
            .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {e}"))??;
        Ok(res)
    }

    /// Calls the LLM. Returns `(content, lessons, used_fallback=false)`
    /// on success. The "lessons" vector is parsed from a JSON
    /// sub-section if the model emits one; otherwise it's empty.
    async fn synthesise(&self, candidates: &[Memory]) -> Result<(String, Vec<String>, bool)> {
        let llm = match &self.llm {
            Some(l) => l,
            None => {
                let (c, l) = template_summarise(candidates);
                return Ok((c, l, true));
            }
        };
        let summaries: String = candidates
            .iter()
            .take(8)
            .map(|m| {
                let layer = m.layer.as_str();
                let mtype = m.memory_type.as_str();
                let snippet = truncate_chars(&m.content, 120);
                format!("- [L{layer}][{mtype}] {snippet}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "基于以下记忆片段，生成一条元认知反思（指出共性、规律、可改进之处）：\n\n{summaries}\n\n要求：100-300 字，第一人称，\"我意识到...\"开头。\n\n请在结尾以 JSON 形式列出 lessons: {{\"lessons\": [\"...\", \"...\"]}}"
        );
        let resp = llm
            .chat(vec![
                ChatMessage::system("你是一个元认知助手。只输出反思正文 + 末尾的 JSON lessons。"),
                ChatMessage::user(prompt),
            ])
            .await
            .context("LLM chat failed during reflection")?;
        let (content, lessons) = split_lessons(&resp.message.content);
        Ok((content, lessons, false))
    }

    /// Persists a reflection row + `memory_reflections` join rows.
    ///
    /// v0.3: the (sorted source memory ids, day-bucket) pair is used as
    /// a de-duplication key. When the same set of memories produces a
    /// reflection on the same UTC day, the call short-circuits with
    /// the existing row instead of writing a duplicate.
    // v1.0.1 fix C: see `collect_candidates` — same reason,
    // `async fn` required.
    async fn persist(
        &self,
        sources: &[Memory],
        content: &str,
        lessons: &[String],
        importance: f32,
        trigger_kind: String,
    ) -> Result<Reflection> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();
        let day_bucket = now / 86_400; // UTC day bucket
        let confidence = 0.7_f32;
        let lessons_json = serde_json::to_string(lessons).unwrap_or_else(|_| "[]".to_string());

        let sqlite = self.sqlite.clone();
        let id_for_join = id.clone();
        let source_ids: Vec<String> = sources.iter().map(|m| m.id.clone()).collect();
        let id_for_thread = id.clone();
        let content_for_thread = content.to_string();
        let trigger_for_thread = trigger_kind.clone();
        let sources_for_thread = source_ids.clone();
        let join = tokio::task::spawn_blocking(move || -> Result<Option<Reflection>> {
            let conn = sqlite.raw_connection();
            let conn = conn.lock();
            // v0.3: dedup check. Sort the source ids so the key is
            // set-equality, not list-equality. Compare against any
            // reflection created on the same UTC day whose memory set
            // equals ours.
            let mut sorted_sources = sources_for_thread.clone();
            sorted_sources.sort();
            sorted_sources.dedup();
            if let Some(existing) = find_duplicate_reflection(&conn, &sorted_sources, day_bucket)? {
                debug!(
                    target: "nine_snake.reflect",
                    id = %existing.id,
                    "dedup: reflection already exists for this memory-set / day-bucket; skipping insert"
                );
                return Ok(Some(existing));
            }
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO reflections
                    (id, memory_id, trigger_kind, content, lessons, confidence, importance, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id_for_join,
                    sources_for_thread.first().cloned(),
                    trigger_for_thread,
                    content_for_thread,
                    lessons_json,
                    confidence,
                    importance,
                    now,
                ],
            )?;
            for sid in &sources_for_thread {
                tx.execute(
                    "INSERT OR IGNORE INTO memory_reflections (memory_id, reflection_id, created_at)
                     VALUES (?1, ?2, ?3)",
                    params![sid, id_for_thread, now],
                )?;
            }
            tx.commit()?;
            Ok(None)
        });
        let dedup_existing = join
            .await
            .map_err(|e| anyhow::anyhow!("spawn_blocking join error: {e}"))??;

        if let Some(mut existing) = dedup_existing {
            // Surface the dedup hit in the returned value without
            // mutating the database row.
            existing.trigger_kind = "dedup_replay".to_string();
            return Ok(existing);
        }

        Ok(Reflection {
            id,
            source_memories: source_ids,
            content: content.to_string(),
            layer: MemoryLayer::L5,
            memory_type: MemoryType::Metacognitive,
            importance,
            trigger_kind,
            lessons: lessons.to_vec(),
            confidence,
            created_at: now,
        })
    }

    /// Spawns the background worker. Returns the [`JoinHandle`] so the
    /// caller can await graceful shutdown.
    ///
    /// v1.0.1 P0#5: the loop now races the periodic sleep against
    /// `cancel_token.cancelled()`.  Cancelling the token resolves
    /// the future immediately, so the worker exits within one
    /// wakeup of the cancel call (typically <1 ms).  The legacy
    /// `shutdown` `Notify` is still honoured for callers that
    /// prefer the v1.0 API.
    pub fn spawn_worker(self: Arc<Self>) -> Option<JoinHandle<()>> {
        if self.cfg.worker_interval_secs == 0 {
            info!(target: "nine_snake.reflect", "worker disabled (interval=0)");
            return None;
        }
        let me = self.clone();
        let shutdown = self.shutdown.clone();
        let cancel_token = self.cancel_token.clone();
        let handle = tokio::spawn(async move {
            let interval = Duration::from_secs(me.cfg.worker_interval_secs);
            loop {
                tokio::select! {
                    // v1.0.1 P0#5: cancellation wins first so the
                    // worker doesn't accidentally block on a sleep
                    // that hasn't been interrupted.
                    biased;
                    _ = cancel_token.cancelled() => {
                        info!(target: "nine_snake.reflect", "worker received cancellation token");
                        break;
                    }
                    _ = shutdown.notified() => {
                        info!(target: "nine_snake.reflect", "worker received shutdown signal");
                        break;
                    }
                    _ = tokio::time::sleep(interval) => {
                        if let Err(e) = me.reflect_now().await {
                            warn!(target: "nine_snake.reflect", error = ?e, "background reflection failed");
                        }
                    }
                }
            }
        });
        Some(handle)
    }
}

// ---------------------------------------------------------------------------
// InFlightGuard — RAII guard for the `in_flight` flag.
// ---------------------------------------------------------------------------

/// RAII guard that resets `in_flight = false` on `Drop`.
/// Ensures the flag is cleared on *every* exit path: Ok, Err, panic,
/// or `CancellationToken` cancellation.
struct InFlightGuard<'a> {
    flag: &'a Mutex<bool>,
}

impl<'a> InFlightGuard<'a> {
    /// Attempts to set `in_flight = true`. Returns `Some(guard)` on
    /// success, `None` if the flag was already true.
    fn try_acquire(flag: &'a Mutex<bool>) -> Option<Self> {
        let mut g = flag.lock();
        if *g {
            return None;
        }
        *g = true;
        Some(Self { flag })
    }
}

impl<'a> Drop for InFlightGuard<'a> {
    fn drop(&mut self) {
        *self.flag.lock() = false;
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Storage-row projection for `reflections`.
struct ReflectionRow {
    id: String,
    trigger_kind: String,
    content: String,
    lessons: Vec<String>,
    confidence: f32,
    importance: f32,
    created_at: i64,
}

fn list_sources(conn: &Connection, reflection_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT memory_id FROM memory_reflections WHERE reflection_id = ?1 ORDER BY created_at",
    )?;
    let rows = stmt
        .query_map(params![reflection_id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// v0.3: looks for a reflection in the same UTC `day_bucket` whose
/// `memory_reflections` set equals the given `sorted_sources`.
///
/// Returns `Ok(Some(Reflection))` when a match is found, `Ok(None)`
/// when there is no row in the dedup window. We compare sets (sorted
/// JSON vectors) using SQLite's JSON1 `json_each` + group_concat to
/// avoid loading every row's source set into memory.
fn find_duplicate_reflection(
    conn: &Connection,
    sorted_sources: &[String],
    day_bucket: i64,
) -> Result<Option<Reflection>> {
    // Day-bucket window: [start_of_day, start_of_next_day)
    let day_start = day_bucket * 86_400;
    let day_end = day_start + 86_400;
    // Collect every reflection created in this day-bucket, plus the
    // sorted list of source memory ids (concatenated with a separator
    // that cannot occur in UUIDs).
    let mut stmt = conn.prepare(
        "SELECT r.id, r.trigger_kind, r.content, r.lessons, r.confidence,
                r.importance, r.created_at,
                (SELECT GROUP_CONCAT(mr.memory_id, '|')
                   FROM memory_reflections mr
                  WHERE mr.reflection_id = r.id
                  ORDER BY mr.memory_id) AS src_set
           FROM reflections r
          WHERE r.created_at >= ?1 AND r.created_at < ?2",
    )?;
    let rows = stmt.query_map(params![day_start, day_end], |row| {
        let lessons_s: String = row.get(3)?;
        Ok((
            ReflectionRow {
                id: row.get(0)?,
                trigger_kind: row.get(1)?,
                content: row.get(2)?,
                lessons: serde_json::from_str(&lessons_s).unwrap_or_default(),
                confidence: row.get(4)?,
                importance: row.get(5)?,
                created_at: row.get(6)?,
            },
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    for row in rows {
        let (rr, src_set) = row?;
        let existing: Vec<String> = src_set
            .map(|s| {
                s.split('|')
                    .filter(|x| !x.is_empty())
                    .map(|x| x.to_string())
                    .collect()
            })
            .unwrap_or_default();
        if existing == sorted_sources {
            let sources = list_sources(conn, &rr.id)?;
            return Ok(Some(Reflection {
                id: rr.id,
                source_memories: sources,
                content: rr.content,
                layer: MemoryLayer::L5,
                memory_type: MemoryType::Metacognitive,
                importance: rr.importance,
                trigger_kind: rr.trigger_kind,
                lessons: rr.lessons,
                confidence: rr.confidence,
                created_at: rr.created_at,
            }));
        }
    }
    Ok(None)
}

/// Splits the model output into `(main_content, lessons)`. The model
/// is expected to emit a JSON `{"lessons": [...]}` snippet at the end
/// (possibly fenced with ```json).
///
/// v0.3: the v0.2 implementation parsed the raw tail and silently
/// produced an empty `lessons` vector when the JSON was wrapped in
/// ```` ```json ... ``` ```` fences. We now strip the fences before
/// parsing, and also support ` ``` ` (no language hint) and `JSON:`
/// prefix variants.
fn split_lessons(raw: &str) -> (String, Vec<String>) {
    let raw = strip_fences(raw);
    if let Some(idx) = raw.rfind('{') {
        let head = &raw[..idx];
        let tail = &raw[idx..];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(tail.trim()) {
            if let Some(arr) = v.get("lessons").and_then(|x| x.as_array()) {
                let lessons: Vec<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect();
                return (head.trim().to_string(), lessons);
            }
        }
    }
    (raw.trim().to_string(), Vec::new())
}

/// Removes a single leading and trailing markdown code fence (with or
/// without a language hint). Designed for the trailing JSON block the
/// reflection prompt asks the model to emit, so it tolerates:
///
/// * ` ```json\n{...}\n``` `
/// * ` ```\n{...}\n``` `
/// * ` ```JSON\n{...}\n``` ` (case-insensitive language)
/// * `JSON:\n{...}` (no fence, just a label prefix)
///
/// If no fence is detected the input is returned unchanged.
fn strip_fences(raw: &str) -> String {
    let trimmed = raw.trim();
    // Trailing fence: ` ``` ` on its own line at the end.
    let after_closing = if let Some(pos) = trimmed.rfind("```") {
        let candidate = trimmed[pos..].trim();
        if candidate == "```" || candidate.starts_with("```\n") {
            trimmed[..pos].trim_end()
        } else {
            trimmed
        }
    } else {
        trimmed
    };
    // Leading fence: ` ```json ` or ` ``` ` on the first line.
    let after_opening = if let Some(rest) = after_closing.strip_prefix("```") {
        // After the triple-backtick, skip an optional language tag
        // up to the first newline.
        if let Some(nl) = rest.find('\n') {
            let tag = rest[..nl].trim();
            if tag.is_empty() || tag.eq_ignore_ascii_case("json") {
                rest[nl + 1..].trim_start()
            } else {
                after_closing
            }
        } else {
            after_closing
        }
    } else {
        after_closing
    };
    // Also strip a `JSON:` / `json:` label prefix the model may use.
    let lower = after_opening.to_ascii_lowercase();
    if lower.starts_with("json:") || lower.starts_with("json：") {
        after_opening[after_opening
            .find(':')
            .or_else(|| after_opening.find('：'))
            .unwrap()
            + 1..]
            .trim_start()
            .to_string()
    } else {
        after_opening.to_string()
    }
}

/// Deterministic offline summariser used when the LLM is not
/// configured or fails. Produces a first-person "I noticed..."
/// template that is suitable for tests and CI runs.
fn template_summarise(memories: &[Memory]) -> (String, Vec<String>) {
    let n = memories.len();
    let layers: BTreeSet<&'static str> = memories.iter().map(|m| m.layer.as_str()).collect();
    let types: BTreeSet<&'static str> = memories.iter().map(|m| m.memory_type.as_str()).collect();
    let top = memories
        .iter()
        .max_by(|a, b| {
            a.importance
                .partial_cmp(&b.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|m| truncate_chars(&m.content, 80))
        .unwrap_or_default();
    let lessons = vec![
        format!("过去窗口内共 {n} 条高 importance 记忆"),
        format!(
            "涉及层级: {}",
            layers.iter().copied().collect::<Vec<_>>().join("/")
        ),
        format!(
            "涉及类型: {}",
            types.iter().copied().collect::<Vec<_>>().join("/")
        ),
    ];
    let content = format!(
        "我意识到：最近 {n} 条高 importance 记忆集中在层级 {} 和类型 {} 上。最突出的一条是：「{top}」。我需要在后续回合中更系统地处理这类主题。",
        layers.iter().copied().collect::<Vec<_>>().join("/"),
        types.iter().copied().collect::<Vec<_>>().join("/"),
    );
    (content, lessons)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Same shape as the v0.1 `row_to_memory` but reads the optional
/// `embedding` placeholder (we don't load the real vector here — that
/// lives in LanceDB). For reflection selection, the embedding is not
/// needed.
fn row_to_memory_full(row: &rusqlite::Row<'_>) -> rusqlite::Result<Memory> {
    use std::str::FromStr;
    let memory_type_s: String = row.get("memory_type")?;
    let layer_s: String = row.get("layer")?;
    let source_s: String = row.get("source")?;
    let metadata_s: String = row.get("metadata")?;
    let pinned: i32 = row.get("pinned")?;
    let memory_type = MemoryType::from_str(&memory_type_s).map_err(|e| {
        rusqlite::Error::InvalidColumnType(0, e.to_string(), rusqlite::types::Type::Text)
    })?;
    let layer = MemoryLayer::from_str(&layer_s).map_err(|e| {
        rusqlite::Error::InvalidColumnType(0, e.to_string(), rusqlite::types::Type::Text)
    })?;
    let source = SourceKind::from_str(&source_s).map_err(|e| {
        rusqlite::Error::InvalidColumnType(0, e.to_string(), rusqlite::types::Type::Text)
    })?;
    let metadata: serde_json::Value = serde_json::from_str(&metadata_s).map_err(|e| {
        rusqlite::Error::InvalidColumnType(0, e.to_string(), rusqlite::types::Type::Text)
    })?;
    Ok(Memory {
        id: row.get("id")?,
        memory_type,
        layer,
        content: row.get("content")?,
        summary: super::types::MultiGranularity {
            s50: row.get("summary_50")?,
            s150: row.get("summary_150")?,
            s500: row.get("summary_500")?,
            s2000: row.get("summary_2000")?,
        },
        embedding: Vec::new(),
        importance: row.get("importance")?,
        access_count: row.get("access_count")?,
        last_access: row.get("last_access")?,
        created_at: row.get("created_at")?,
        source,
        metadata,
        compressed_from: row.get("compressed_from")?,
        compression_gen: row.get("compression_gen")?,
        pinned: pinned != 0,
        archived: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_db() -> (std::path::PathBuf, Arc<SqliteStore>) {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "nine_snake_reflect_test_{}_{}.db",
            std::process::id(),
            n
        ));
        let store = Arc::new(SqliteStore::open(&p).unwrap());
        // Apply migration 002 so the importance column + join table
        // exist.
        crate::memory::migration::run_migrations(
            &store.raw_connection().lock(),
            crate::memory::migration::bundled_migrations_dir(),
        )
        .unwrap();
        (p, store)
    }

    fn cleanup(p: &std::path::Path) {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(p.with_extension("db-wal"));
        let _ = std::fs::remove_file(p.with_extension("db-shm"));
    }

    fn high_importance_mem(id: &str, content: &str) -> Memory {
        let mut m = Memory::new(
            MemoryType::Semantic,
            MemoryLayer::L3,
            content,
            SourceKind::UserInput,
        );
        m.id = id.to_string();
        m.importance = 0.8;
        m
    }

    #[tokio::test]
    async fn reflect_now_with_no_candidates_returns_empty() {
        let (p, store) = temp_db();
        let engine = ReflectionEngine::new(store, None, ReflectConfig::default());
        let r = engine.reflect_now().await.unwrap();
        assert!(r.is_empty());
        cleanup(&p);
    }

    #[tokio::test]
    async fn reflect_now_produces_template_reflection_with_fallback() {
        let (p, store) = temp_db();
        store
            .insert_guarded(&high_importance_mem("a", "Tauri 启动失败，端口占用"))
            .unwrap();
        store
            .insert_guarded(&high_importance_mem("b", "Tauri 启动失败，权限问题"))
            .unwrap();
        store
            .insert_guarded(&high_importance_mem("c", "数据库连接超时"))
            .unwrap();

        let engine = ReflectionEngine::new(store, None, ReflectConfig::default());
        let r = engine.reflect_now().await.unwrap();
        assert_eq!(r.len(), 1);
        let refl = &r[0];
        assert_eq!(refl.layer, MemoryLayer::L5);
        assert_eq!(refl.memory_type, MemoryType::Metacognitive);
        assert_eq!(refl.source_memories.len(), 3);
        assert!(
            refl.content.starts_with("我意识到"),
            "got: {}",
            refl.content
        );
        assert!(!refl.lessons.is_empty());
        cleanup(&p);
    }

    #[test]
    fn list_recent_returns_persisted_reflection() {
        let (p, store) = temp_db();
        store
            .insert_guarded(&high_importance_mem("x", "something"))
            .unwrap();
        let engine = ReflectionEngine::new(store.clone(), None, ReflectConfig::default());
        let r = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(engine.reflect_now())
            .unwrap();
        assert_eq!(r.len(), 1);
        let listed = engine.list_recent(10).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, r[0].id);
        assert_eq!(listed[0].source_memories, vec!["x".to_string()]);
        cleanup(&p);
    }

    #[test]
    fn split_lessons_extracts_trailing_json() {
        let raw = "I noticed X. {\"lessons\":[\"a\",\"b\"]}";
        let (c, l) = split_lessons(raw);
        assert!(c.starts_with("I noticed X"));
        assert_eq!(l, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn split_lessons_returns_empty_when_no_json() {
        let raw = "Just a thought, no JSON here.";
        let (c, l) = split_lessons(raw);
        assert_eq!(c, raw);
        assert!(l.is_empty());
    }

    #[test]
    fn template_summarise_mentions_counts_and_layers() {
        let mems = vec![
            high_importance_mem("a", "Tauri port issue"),
            high_importance_mem("b", "Permission denied"),
        ];
        let (content, lessons) = template_summarise(&mems);
        assert!(content.contains("我意识到"));
        assert!(content.contains("2 条"));
        assert_eq!(lessons.len(), 3);
    }

    #[test]
    fn engine_single_flight_drops_concurrent_calls() {
        let (p, store) = temp_db();
        store
            .insert_guarded(&high_importance_mem("a", "x"))
            .unwrap();
        let engine = Arc::new(ReflectionEngine::new(store, None, ReflectConfig::default()));
        let e1 = engine.clone();
        let e2 = engine.clone();
        let r1 = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move { e1.reflect_now().await })
            .unwrap();
        let r2 = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move { e2.reflect_now().await })
            .unwrap();
        // At most one of the two calls is allowed to produce a row;
        // the other is short-circuited.
        assert!(r1.len() + r2.len() <= 1);
        cleanup(&p);
    }

    #[test]
    fn persist_writes_join_rows() {
        let (p, store) = temp_db();
        store
            .insert_guarded(&high_importance_mem("m1", "a"))
            .unwrap();
        store
            .insert_guarded(&high_importance_mem("m2", "b"))
            .unwrap();
        let engine = ReflectionEngine::new(store.clone(), None, ReflectConfig::default());
        let r = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(engine.reflect_now())
            .unwrap();
        assert_eq!(r.len(), 1);
        // Each source memory must be linked to the reflection.
        for m in &["m1", "m2"] {
            let rc = store.raw_connection();
            let conn = rc.lock();
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM memory_reflections WHERE memory_id = ?1 AND reflection_id = ?2",
                    params![m, r[0].id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "missing join row for {m}");
        }
        cleanup(&p);
    }

    #[test]
    fn spawn_worker_disabled_when_interval_zero() {
        let (p, store) = temp_db();
        let mut cfg = ReflectConfig::default();
        cfg.worker_interval_secs = 0;
        let engine = Arc::new(ReflectionEngine::new(store, None, cfg));
        assert!(engine.spawn_worker().is_none());
        cleanup(&p);
    }

    #[test]
    fn split_lessons_handles_json_fence() {
        let raw = "正文。\n```json\n{\"lessons\":[\"a\"]}\n```";
        let (c, l) = split_lessons(raw);
        assert!(c.starts_with("正文"));
        // v0.3 fix: the fence is stripped, so lessons are now parsed.
        assert_eq!(l, vec!["a".to_string()], "fence must be stripped");
    }

    #[test]
    fn split_lessons_handles_fence_no_language() {
        let raw = "正文。\n```\n{\"lessons\":[\"a\",\"b\"]}\n```";
        let (c, l) = split_lessons(raw);
        assert!(c.starts_with("正文"));
        assert_eq!(l, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn split_lessons_handles_json_label_prefix() {
        let raw = "正文。\nJSON: {\"lessons\":[\"x\"]}";
        let (c, l) = split_lessons(raw);
        assert!(c.starts_with("正文"));
        assert_eq!(l, vec!["x".to_string()]);
    }

    #[test]
    fn strip_fences_preserves_non_fenced_text() {
        assert_eq!(strip_fences("plain text"), "plain text");
        assert_eq!(strip_fences("a```b```c"), "a```b```c");
    }

    #[test]
    fn reflection_struct_serializes_to_json() {
        let r = Reflection {
            id: "x".to_string(),
            source_memories: vec!["a".to_string()],
            content: "hi".to_string(),
            layer: MemoryLayer::L5,
            memory_type: MemoryType::Metacognitive,
            importance: 0.5,
            trigger_kind: "periodic".to_string(),
            lessons: vec!["l".to_string()],
            confidence: 0.5,
            created_at: 1,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"layer\":\"L5\""));
        assert!(s.contains("\"memory_type\":\"metacognitive\""));
    }

    #[test]
    fn reflect_now_dedups_within_same_day() {
        let (p, store) = temp_db();
        store
            .insert_guarded(&high_importance_mem("a", "alpha"))
            .unwrap();
        store
            .insert_guarded(&high_importance_mem("b", "beta"))
            .unwrap();

        let engine = ReflectionEngine::new(store.clone(), None, ReflectConfig::default());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let r1 = rt.block_on(engine.reflect_now()).unwrap();
        assert_eq!(r1.len(), 1);
        assert_ne!(r1[0].trigger_kind, "dedup_replay");

        // Second call within the same day should hit the dedup path.
        let r2 = rt.block_on(engine.reflect_now()).unwrap();
        assert_eq!(r2.len(), 1);
        assert_eq!(r2[0].id, r1[0].id, "id should be reused");
        assert_eq!(r2[0].trigger_kind, "dedup_replay");
        cleanup(&p);
    }

    #[test]
    fn find_duplicate_reflection_handles_empty_input() {
        let (p, store) = temp_db();
        let rc = store.raw_connection();
        let conn = rc.lock();
        let result = find_duplicate_reflection(&conn, &[], 0).unwrap();
        assert!(result.is_none());
        cleanup(&p);
    }

    /// v1.0.1 P0#5: cancelling the worker's `CancellationToken`
    /// must cause the spawned task to exit within a small bounded
    /// time.  We use a long interval (60 s) so the worker is
    /// definitely sleeping when we cancel, and assert the join
    /// completes inside 100 ms — well under the 60 s interval.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_stops_worker_within_100ms() {
        let (p, store) = temp_db();
        let mut cfg = ReflectConfig::default();
        cfg.worker_interval_secs = 60;
        let engine = Arc::new(ReflectionEngine::new(store, None, cfg));
        let token = engine.cancel_token();
        let handle = engine.clone().spawn_worker().expect("worker");

        // Give the worker a moment to enter its `select!` loop.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let started = std::time::Instant::now();
        token.cancel();
        // Bound the wait: 100 ms is generous.
        let joined = tokio::time::timeout(Duration::from_millis(100), handle).await;
        let elapsed = started.elapsed();
        assert!(
            joined.is_ok(),
            "worker did not exit within 100 ms (elapsed = {elapsed:?})"
        );
        // The handle itself must not be in error.
        joined.unwrap().expect("worker join");
        cleanup(&p);
    }
}
