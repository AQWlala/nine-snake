//! SQLite-backed structured store for [`Memory`] records.
//!
//! The store uses `rusqlite` which is synchronous, so all database
//! I/O operations are wrapped in `tokio::task::spawn_blocking` to
//! avoid blocking the tokio worker threads. WAL mode is enabled for
//! concurrent readers.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Row};
use tracing::{debug, info};

use super::types::{
    Memory, MemoryLayer, MemoryRelation, MemoryType, MultiGranularity, RelationKind, SourceKind,
};

const MEMORY_COLUMNS: &str = "id, memory_type, layer, content, summary_50, summary_150, summary_500, summary_2000, importance, access_count, last_access, created_at, source, metadata, compressed_from, compression_gen, pinned, archived";

macro_rules! sel_mem {
    ($rest:expr) => {
        concat!("SELECT id, memory_type, layer, content, summary_50, summary_150, summary_500, summary_2000, importance, access_count, last_access, created_at, source, metadata, compressed_from, compression_gen, pinned, archived FROM ", $rest)
    };
}

/// Thread-safe SQLite store for memory records.
///
/// Cloning is cheap: the underlying [`Connection`] is wrapped in an
/// `Arc<Mutex<_>>` so multiple Tauri commands can issue queries
/// concurrently.
#[derive(Clone)]
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
    /// v1.0.1 P0#10: an additional process-wide lock acquired
    /// around the *whole* blackhole compression pass and the
    /// sponge `absorb` write path.  This is **not** the same as
    /// `conn` — `conn` is per-statement, this is a cross-call
    /// guard that ensures a sponge read cannot observe a row
    /// that's in the middle of being rewritten by a blackhole
    /// pass (the "partial compression" race).
    ///
    /// Trade-off: we briefly serialise absorb / compress, which
    /// is a small latency cost.  In exchange, the sponge
    /// `absorb` reader can no longer race against a `compress`
    /// writer for the same `memories.content` cell.  The cost is
    /// paid in milliseconds at most (the compress_group inner
    /// work is local); the alternative — a partial read of a
    /// half-compressed cell — is a correctness bug.
    compression_lock: Arc<Mutex<()>>,
}

impl SqliteStore {
    /// Opens (or creates) the database at `path` and runs the bundled
    /// migration SQL.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating parent dir for sqlite db: {}", parent.display())
                })?;
            }
        }

        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db at {}", path.display()))?;
        // Performance & correctness pragmas.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "mmap_size", 268_435_456_i64)?;

        // Apply the bundled migration. We embed the SQL at compile time.
        const SCHEMA: &str = include_str!("../../migrations/001_initial.sql");
        conn.execute_batch(SCHEMA)
            .context("applying initial migration")?;

        info!(target: "nine_snake.memory", path = %path.display(), "sqlite store ready");
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            compression_lock: Arc::new(Mutex::new(())),
        })
    }

    /// v1.0.1 P0#10: process-wide compression lock.
    ///
    /// Held by `BlackholeEngine::run_pass` for the duration of a
    /// compression pass, and by `sponge::absorb` for the
    /// duration of a merge write.  Acquired as a `MutexGuard` so
    /// callers can use `let _g = store.compression_lock();` to
    /// scope the critical section.
    pub fn compression_lock(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.compression_lock.lock()
    }

    /// v1.1: Insert a memory under the compression lock.
    /// This is a synchronous method intended to be called inside
    /// `spawn_blocking` so the lock is held for the duration of
    /// the SQLite write and released before any `.await` point.
    pub fn insert_guarded(&self, m: &Memory) -> Result<()> {
        let _g = self.compression_lock.lock();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO memories (
                id, memory_type, layer, content,
                summary_50, summary_150, summary_500, summary_2000,
                importance, access_count, last_access, created_at,
                source, metadata, compressed_from, compression_gen, pinned, archived
            ) VALUES (
                ?1, ?2, ?3, ?4,
                ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16, ?17, ?18
            )",
            params![
                m.id,
                m.memory_type.as_str(),
                m.layer.as_str(),
                m.content,
                m.summary.s50,
                m.summary.s150,
                m.summary.s500,
                m.summary.s2000,
                m.importance,
                m.access_count,
                m.last_access,
                m.created_at,
                m.source.as_str(),
                m.metadata.to_string(),
                m.compressed_from,
                m.compression_gen,
                m.pinned as i32,
                m.archived as i32,
            ],
        )
        .map_err(|e| anyhow!("sqlite insert_guarded error: {e}"))?;
        debug!(target: "nine_snake.memory", id = %m.id, layer = %m.layer, "inserted memory (guarded)");
        Ok(())
    }

    /// v1.1: Update a memory under the compression lock.
    /// Same rationale as `insert_guarded`.
    pub fn update_guarded(&self, m: &Memory) -> Result<()> {
        let _g = self.compression_lock.lock();
        let conn = self.conn.lock();
        let affected = conn
            .execute(
                "UPDATE memories SET
                memory_type = ?2,
                layer = ?3,
                content = ?4,
                summary_50 = ?5,
                summary_150 = ?6,
                summary_500 = ?7,
                summary_2000 = ?8,
                importance = ?9,
                access_count = ?10,
                last_access = ?11,
                source = ?12,
                metadata = ?13,
                compressed_from = ?14,
                compression_gen = ?15,
                pinned = ?16
             WHERE id = ?1",
                params![
                    m.id,
                    m.memory_type.as_str(),
                    m.layer.as_str(),
                    m.content,
                    m.summary.s50,
                    m.summary.s150,
                    m.summary.s500,
                    m.summary.s2000,
                    m.importance,
                    m.access_count,
                    m.last_access,
                    m.source.as_str(),
                    m.metadata.to_string(),
                    m.compressed_from,
                    m.compression_gen,
                    m.pinned as i32,
                ],
            )
            .map_err(|e| anyhow!("sqlite update_guarded error: {e}"))?;
        if affected == 0 {
            return Err(anyhow!("memory not found"));
        }
        Ok(())
    }

    pub async fn insert_guarded_spawn(&self, m: &Memory) -> Result<()> {
        let this = self.clone();
        let m = m.clone();
        tokio::task::spawn_blocking(move || this.insert_guarded(&m))
            .await
            .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    pub async fn update_guarded_spawn(&self, m: &Memory) -> Result<()> {
        let this = self.clone();
        let m = m.clone();
        tokio::task::spawn_blocking(move || this.update_guarded(&m))
            .await
            .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }
    // spawn_blocking to avoid blocking tokio worker threads.
    // The blocking SQLite calls are isolated in spawn_blocking closures.

    /// Inserts a new memory. The caller is expected to have already filled in the
    /// embedding and summaries.
    // v1.1 P1#3: async + spawn_blocking
    pub async fn insert(&self, m: &Memory) -> Result<()> {
        let conn = self.conn.clone();
        let m_id = m.id.clone();
        let m_type = m.memory_type.as_str().to_string();
        let m_layer = m.layer.as_str().to_string();
        let m_content = m.content.clone();
        let m_s50 = m.summary.s50.clone();
        let m_s150 = m.summary.s150.clone();
        let m_s500 = m.summary.s500.clone();
        let m_s2000 = m.summary.s2000.clone();
        let m_importance = m.importance;
        let m_access_count = m.access_count;
        let m_last_access = m.last_access;
        let m_created_at = m.created_at;
        let m_source = m.source.as_str().to_string();
        let m_metadata = m.metadata.to_string();
        let m_compressed_from = m.compressed_from.clone();
        let m_compression_gen = m.compression_gen;
        let m_pinned = m.pinned as i32;
        let m_archived = m.archived as i32;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            conn.execute(
                "INSERT INTO memories (
                    id, memory_type, layer, content,
                    summary_50, summary_150, summary_500, summary_2000,
                    importance, access_count, last_access, created_at,
                    source, metadata, compressed_from, compression_gen, pinned, archived
                ) VALUES (
                    ?1, ?2, ?3, ?4,
                    ?5, ?6, ?7, ?8,
                    ?9, ?10, ?11, ?12,
                    ?13, ?14, ?15, ?16, ?17, ?18
                )",
                params![
                    m_id,
                    m_type,
                    m_layer,
                    m_content,
                    m_s50,
                    m_s150,
                    m_s500,
                    m_s2000,
                    m_importance,
                    m_access_count,
                    m_last_access,
                    m_created_at,
                    m_source,
                    m_metadata,
                    m_compressed_from,
                    m_compression_gen,
                    m_pinned,
                    m_archived,
                ],
            )
            .map_err(|e| anyhow!("sqlite insert error: {e}"))?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))??;
        debug!(target: "nine_snake.memory", id = %m.id, layer = %m.layer, "inserted memory");
        Ok(())
    }

    /// Updates an existing record in place. Returns `Err` if the row
    /// does not exist.
    // v1.1 P1#3: async + spawn_blocking
    pub async fn update(&self, m: &Memory) -> Result<()> {
        let conn = self.conn.clone();
        let m_id = m.id.clone();
        let m_type = m.memory_type.as_str().to_string();
        let m_layer = m.layer.as_str().to_string();
        let m_content = m.content.clone();
        let m_s50 = m.summary.s50.clone();
        let m_s150 = m.summary.s150.clone();
        let m_s500 = m.summary.s500.clone();
        let m_s2000 = m.summary.s2000.clone();
        let m_importance = m.importance;
        let m_access_count = m.access_count;
        let m_last_access = m.last_access;
        let m_source = m.source.as_str().to_string();
        let m_metadata = m.metadata.to_string();
        let m_compressed_from = m.compressed_from.clone();
        let m_compression_gen = m.compression_gen;
        let m_pinned = m.pinned as i32;

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            let affected = conn
                .execute(
                    "UPDATE memories SET
                    memory_type = ?2,
                    layer = ?3,
                    content = ?4,
                    summary_50 = ?5,
                    summary_150 = ?6,
                    summary_500 = ?7,
                    summary_2000 = ?8,
                    importance = ?9,
                    access_count = ?10,
                    last_access = ?11,
                    source = ?12,
                    metadata = ?13,
                    compressed_from = ?14,
                    compression_gen = ?15,
                    pinned = ?16
                 WHERE id = ?1",
                    params![
                        m_id,
                        m_type,
                        m_layer,
                        m_content,
                        m_s50,
                        m_s150,
                        m_s500,
                        m_s2000,
                        m_importance,
                        m_access_count,
                        m_last_access,
                        m_source,
                        m_metadata,
                        m_compressed_from,
                        m_compression_gen,
                        m_pinned,
                    ],
                )
                .map_err(|e| anyhow!("sqlite update error: {e}"))?;
            if affected == 0 {
                return Err(anyhow!("memory not found"));
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))??;
        Ok(())
    }

    /// Fetches a memory by id. Returns `Ok(None)` if absent.
    // v1.1 P1#3: async + spawn_blocking.
    //
    // v1.3 fix (cargo compat): the pthread-style `let conn = self.conn.lock();`
    // pattern holds a `MutexGuard` (`*mut ()`) across `.await`, which is
    // rejected by the post-tokio-1.36 `Send` bound for `JoinHandle`. The
    // guard must only live on the worker thread. We instead clone the
    // `Arc<Mutex<Connection>>` outside the future and re-acquire the lock
    // inside `spawn_blocking`'s closure, so the guard is born-and-destroyed
    // on a single thread.
    pub async fn get(&self, id: &str) -> Result<Option<Memory>> {
        let id_owned = id.to_string();
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            let row = conn
                .query_row(
                    sel_mem!("memories WHERE id = ?1"),
                    params![id_owned],
                    row_to_memory,
                )
                .optional()
                .map_err(|e| anyhow!("sqlite get error: {e}"))?;
            Ok(row)
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    /// Fetches many memories in a single `WHERE id IN (...)` query.
    ///
    /// v0.1 used a per-hit `get()` round-trip from `memory_search`
    /// which was O(N) and blocked the async runtime; this method is the
    /// fix for both. The order of the returned vector follows the
    /// natural `IN` order (which is implementation-defined for SQLite
    /// but consistent within a version), so callers should not rely on
    /// a specific position.
    // v1.1 P1#3: async + spawn_blocking
    pub async fn get_many(&self, ids: &[String]) -> Result<Vec<Memory>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        let ids_owned = ids.to_vec();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            // Build "?, ?, ?" placeholders dynamically.
            let placeholders = std::iter::repeat("?")
                .take(ids_owned.len())
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT {MEMORY_COLUMNS} FROM memories WHERE id IN ({placeholders}) \
                 AND compressed_from IS NULL"
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| anyhow!("sqlite prepare error: {e}"))?;
            let params_vec: Vec<&dyn rusqlite::ToSql> = ids_owned
                .iter()
                .map(|s| s as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt
                .query_map(params_vec.as_slice(), row_to_memory)
                .map_err(|e| anyhow!("sqlite query error: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow!("sqlite row error: {e}"))?;
            Ok(rows)
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    /// Marks a memory row as "compressed from this id" by setting
    /// `compressed_from = summary_id`. The original record is *not*
    /// deleted — the v0.1 black-hole contract is "density-preserving
    /// compression". Subsequent `get_many` / `list_recent` /
    /// `list_by_layer` calls will exclude the row.
    // v1.1 P1#3: async + spawn_blocking
    pub async fn update_compressed_from(&self, source_id: &str, summary_id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let src_owned = source_id.to_string();
        let sum_owned = summary_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            let n = conn
                .execute(
                    "UPDATE memories SET compressed_from = ?2 WHERE id = ?1",
                    params![src_owned, sum_owned],
                )
                .map_err(|e| anyhow!("sqlite update_compressed_from error: {e}"))?;
            if n == 0 {
                return Err(anyhow!("memory not found for compress"));
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    /// Deletes a memory by id. The black-hole engine never calls this;
    /// it is reserved for explicit user actions.
    // v1.1 P1#3: async + spawn_blocking
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let conn = self.conn.clone();
        let id_owned = id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            let n = conn
                .execute("DELETE FROM memories WHERE id = ?1", params![id_owned])
                .map_err(|e| anyhow!("sqlite delete error: {e}"))?;
            Ok(n > 0)
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    /// Lists the most recent memories (newest first), limited to `limit`.
    /// Excludes rows that have been absorbed by the black-hole engine
    /// (`compressed_from IS NOT NULL`).
    // v1.1 P1#3: async + spawn_blocking
    pub async fn list_recent(&self, limit: usize) -> Result<Vec<Memory>> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            let mut stmt = conn
                .prepare(sel_mem!(
                    "memories WHERE compressed_from IS NULL \
                 ORDER BY created_at DESC LIMIT ?1"
                ))
                .map_err(|e| anyhow!("sqlite prepare error: {e}"))?;
            let rows = stmt
                .query_map(params![limit as i64], row_to_memory)
                .map_err(|e| anyhow!("sqlite query error: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow!("sqlite row error: {e}"))?;
            Ok(rows)
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    /// v0.3: update a memory's `importance` in-place. Returns the
    /// refreshed row. Errors if the id is unknown.
    // v1.1 P1#3: async + spawn_blocking
    pub async fn update_importance(&self, id: &str, importance: f32) -> Result<Memory> {
        let importance = importance.clamp(0.0, 1.0);
        let conn = self.conn.clone();
        let id_owned = id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            let n = conn
                .execute(
                    "UPDATE memories SET importance = ?2 WHERE id = ?1",
                    params![id_owned, importance],
                )
                .map_err(|e| anyhow!("sqlite update_importance error: {e}"))?;
            if n == 0 {
                return Err(anyhow!("memory not found"));
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))??;

        // Re-fetch the updated row (this is a second async call but importance updates are rare)
        self.get(id)
            .await?
            .ok_or_else(|| anyhow!("memory {id} disappeared after update"))
    }

    /// v0.3: per-layer memory counts. Returns a `MemoryLayer -> count`
    /// map. Rows that have been absorbed by the black-hole engine are
    /// excluded.
    // v1.1 P1#3: async + spawn_blocking
    pub async fn counts_per_layer(&self) -> Result<std::collections::HashMap<MemoryLayer, u64>> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            let mut stmt = conn.prepare(
                "SELECT layer, COUNT(*) FROM memories WHERE compressed_from IS NULL GROUP BY layer",
            ).map_err(|e| anyhow!("sqlite prepare error: {e}"))?;
            let rows = stmt
                .query_map([], |r| {
                    let layer_s: String = r.get(0)?;
                    let n: i64 = r.get(1)?;
                    Ok((layer_s, n as u64))
                })
                .map_err(|e| anyhow!("sqlite query error: {e}"))?;
            let mut out = std::collections::HashMap::new();
            for row in rows {
                let (layer_s, n) = row.map_err(|e| anyhow!("sqlite row error: {e}"))?;
                if let Ok(layer) = MemoryLayer::from_str(&layer_s) {
                    out.insert(layer, n);
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    /// Lists memories within a given layer, newest first. Excludes rows
    /// that have been absorbed by the black-hole engine.
    // v1.1 P1#3: async + spawn_blocking
    pub async fn list_by_layer(&self, layer: MemoryLayer, limit: usize) -> Result<Vec<Memory>> {
        let conn = self.conn.clone();
        let layer_str = layer.as_str().to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            let mut stmt = conn
                .prepare(sel_mem!(
                    "memories WHERE layer = ?1 \
                 AND compressed_from IS NULL \
                 ORDER BY created_at DESC LIMIT ?2"
                ))
                .map_err(|e| anyhow!("sqlite prepare error: {e}"))?;
            let rows = stmt
                .query_map(params![layer_str, limit as i64], row_to_memory)
                .map_err(|e| anyhow!("sqlite query error: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow!("sqlite row error: {e}"))?;
            Ok(rows)
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    /// Returns memories older than `now - threshold_secs` whose
    /// importance is at or below `importance_ceiling`. The black-hole
    /// engine uses this to find candidates.
    // v1.1 P1#3: async + spawn_blocking
    pub async fn candidates_for_compression(
        &self,
        threshold_secs: i64,
        importance_ceiling: f32,
        limit: usize,
    ) -> Result<Vec<Memory>> {
        let now = chrono::Utc::now().timestamp();
        let cutoff = now - threshold_secs;
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            let mut stmt = conn
                .prepare(sel_mem!(
                    "memories
                 WHERE pinned = 0
                   AND compressed_from IS NULL
                   AND importance <= ?1
                   AND last_access <= ?2
                 ORDER BY last_access ASC
                 LIMIT ?3"
                ))
                .map_err(|e| anyhow!("sqlite prepare error: {e}"))?;
            let rows = stmt
                .query_map(
                    params![importance_ceiling, cutoff, limit as i64],
                    row_to_memory,
                )
                .map_err(|e| anyhow!("sqlite query error: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow!("sqlite row error: {e}"))?;
            Ok(rows)
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    /// Inserts a graph edge between two memories.
    // v1.1 P1#3: async + spawn_blocking
    pub async fn add_relation(&self, rel: &MemoryRelation) -> Result<()> {
        let conn = self.conn.clone();
        let rel_id = rel.id.clone();
        let rel_src = rel.src_id.clone();
        let rel_dst = rel.dst_id.clone();
        let rel_kind = rel.kind.as_str().to_string();
        let rel_weight = rel.weight;
        let rel_created = rel.created_at;
        let rel_evidence = rel.evidence.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            conn.execute(
                "INSERT OR REPLACE INTO memory_relations
                    (id, src_id, dst_id, relation, weight, created_at, evidence)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    rel_id,
                    rel_src,
                    rel_dst,
                    rel_kind,
                    rel_weight,
                    rel_created,
                    rel_evidence,
                ],
            )
            .map_err(|e| anyhow!("sqlite add_relation error: {e}"))?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    pub fn insert_relation(&self, rel: &MemoryRelation) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR REPLACE INTO memory_relations
                (id, src_id, dst_id, relation, weight, created_at, evidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                rel.id,
                rel.src_id,
                rel.dst_id,
                rel.kind.as_str(),
                rel.weight,
                rel.created_at,
                rel.evidence,
            ],
        )
        .map_err(|e| anyhow!("sqlite insert_relation error: {e}"))?;
        Ok(())
    }

    pub async fn insert_relation_spawn(&self, rel: &MemoryRelation) -> Result<()> {
        let store = self.clone();
        let rel = rel.clone();
        tokio::task::spawn_blocking(move || store.insert_relation(&rel))
            .await
            .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    pub fn get_relations(&self, memory_id: &str) -> Result<Vec<MemoryRelation>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, src_id, dst_id, relation, weight, created_at, evidence
             FROM memory_relations WHERE src_id = ?1 OR dst_id = ?1",
            )
            .map_err(|e| anyhow!("sqlite prepare error: {e}"))?;
        let id_owned = memory_id.to_string();
        let rows = stmt
            .query_map(params![id_owned], |row| {
                Ok(MemoryRelation {
                    id: row.get(0)?,
                    src_id: row.get(1)?,
                    dst_id: row.get(2)?,
                    kind: RelationKind::from_str(row.get::<_, String>(3)?.as_str())
                        .unwrap_or(RelationKind::References),
                    weight: row.get(4)?,
                    created_at: row.get(5)?,
                    evidence: row.get(6)?,
                })
            })
            .map_err(|e| anyhow!("sqlite query error: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("sqlite row error: {e}"))?;
        Ok(rows)
    }

    pub fn list_all_relations(&self) -> Result<Vec<MemoryRelation>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id, src_id, dst_id, relation, weight, created_at, evidence
             FROM memory_relations",
            )
            .map_err(|e| anyhow!("sqlite prepare error: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(MemoryRelation {
                    id: row.get(0)?,
                    src_id: row.get(1)?,
                    dst_id: row.get(2)?,
                    kind: RelationKind::from_str(row.get::<_, String>(3)?.as_str())
                        .unwrap_or(RelationKind::References),
                    weight: row.get(4)?,
                    created_at: row.get(5)?,
                    evidence: row.get(6)?,
                })
            })
            .map_err(|e| anyhow!("sqlite query error: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("sqlite row error: {e}"))?;
        Ok(rows)
    }

    /// Lists outgoing relations for a given memory.
    // v1.1 P1#3: async + spawn_blocking
    pub async fn relations_from(&self, src_id: &str) -> Result<Vec<MemoryRelation>> {
        let conn = self.conn.clone();
        let src_owned = src_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, src_id, dst_id, relation, weight, created_at, evidence
                 FROM memory_relations WHERE src_id = ?1",
                )
                .map_err(|e| anyhow!("sqlite prepare error: {e}"))?;
            let rows = stmt
                .query_map(params![src_owned], |row| {
                    Ok(MemoryRelation {
                        id: row.get(0)?,
                        src_id: row.get(1)?,
                        dst_id: row.get(2)?,
                        kind: RelationKind::from_str(row.get::<_, String>(3)?.as_str())
                            .unwrap_or(RelationKind::References),
                        weight: row.get(4)?,
                        created_at: row.get(5)?,
                        evidence: row.get(6)?,
                    })
                })
                .map_err(|e| anyhow!("sqlite query error: {e}"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow!("sqlite row error: {e}"))?;
            Ok(rows)
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    /// Records a `memory_commits` row. Used as an append-only audit log.
    // TODO(v0.5): add automatic commit creation on every insert / update
    // and a batch reconciliation worker that replays the log to rebuild
    // derived state (e.g. importance aggregates, layer counters).
    // v1.1 P1#3: async + spawn_blocking
    pub async fn log_commit(
        &self,
        commit_id: &str,
        parent_id: Option<&str>,
        action: &str,
        target_id: &str,
        payload: &serde_json::Value,
        author: &str,
        message: &str,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let cid = commit_id.to_string();
        let pid = parent_id.map(String::from);
        let act = action.to_string();
        let tid = target_id.to_string();
        let pay = payload.to_string();
        let auth = author.to_string();
        let msg = message.to_string();
        let now = chrono::Utc::now().timestamp();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            conn.execute(
                "INSERT INTO memory_commits
                    (id, parent_id, action, target_id, payload, author, message, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![cid, pid, act, tid, pay, auth, msg, now],
            )
            .map_err(|e| anyhow!("sqlite log_commit error: {e}"))?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    /// Returns the total number of stored memories.
    // v1.1 P1#3: async + spawn_blocking
    pub async fn count(&self) -> Result<i64> {
        let conn = self.conn.clone();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
                .map_err(|e| anyhow!("sqlite count error: {e}"))?;
            Ok(n)
        })
        .await
        .map_err(|e| anyhow!("spawn_blocking join error: {e}"))?
    }

    /// Returns a clone of the inner `Arc<Mutex<Connection>>`. Useful
    /// for callers (e.g. the reflection engine, the migration runner)
    /// that need to issue queries outside the public API surface.
    pub fn raw_connection(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }

    pub fn insert_acl(
        &self,
        id: &str,
        principal: &str,
        resource: &str,
        permission: &str,
        effect: &str,
    ) -> Result<()> {
        let conn = self.conn.lock();
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT OR REPLACE INTO memory_acl (id, principal, resource, permission, effect, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, principal, resource, permission, effect, now],
        ).map_err(|e| anyhow!("sqlite insert_acl error: {e}"))?;
        Ok(())
    }

    pub fn list_acl(&self) -> Result<Vec<(String, String, String, String, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT id, principal, resource, permission, effect FROM memory_acl")
            .map_err(|e| anyhow!("sqlite prepare error: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(|e| anyhow!("sqlite query error: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow!("sqlite row error: {e}"))?;
        Ok(rows)
    }

    pub fn remove_acl(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM memory_acl WHERE id = ?1", params![id])
            .map_err(|e| anyhow!("sqlite remove_acl error: {e}"))?;
        Ok(())
    }
}

/// Row-to-memory conversion shared by all `SELECT` paths.
fn row_to_memory(row: &Row<'_>) -> rusqlite::Result<Memory> {
    let memory_type_s: String = row.get("memory_type")?;
    let layer_s: String = row.get("layer")?;
    let source_s: String = row.get("source")?;
    let metadata_s: String = row.get("metadata")?;
    let pinned: i32 = row.get("pinned")?;
    let archived: i32 = row.get("archived")?;

    let memory_type = MemoryType::from_str(&memory_type_s).map_err(|e| {
        rusqlite::Error::InvalidColumnType(1, e.to_string(), rusqlite::types::Type::Text)
    })?;
    let layer = MemoryLayer::from_str(&layer_s).map_err(|e| {
        rusqlite::Error::InvalidColumnType(2, e.to_string(), rusqlite::types::Type::Text)
    })?;
    let source = SourceKind::from_str(&source_s).map_err(|e| {
        rusqlite::Error::InvalidColumnType(12, e.to_string(), rusqlite::types::Type::Text)
    })?;
    let metadata: serde_json::Value = serde_json::from_str(&metadata_s).map_err(|e| {
        rusqlite::Error::InvalidColumnType(13, e.to_string(), rusqlite::types::Type::Text)
    })?;

    Ok(Memory {
        id: row.get("id")?,
        memory_type,
        layer,
        content: row.get("content")?,
        summary: MultiGranularity {
            s50: row.get("summary_50")?,
            s150: row.get("summary_150")?,
            s500: row.get("summary_500")?,
            s2000: row.get("summary_2000")?,
        },
        embedding: Vec::new(), // embeddings live in LanceDB; SQLite is metadata only.
        importance: row.get("importance")?,
        access_count: row.get("access_count")?,
        last_access: row.get("last_access")?,
        created_at: row.get("created_at")?,
        source,
        metadata,
        compressed_from: row.get("compressed_from")?,
        compression_gen: row.get("compression_gen")?,
        pinned: pinned != 0,
        archived: archived != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    fn temp_db_path() -> std::path::PathBuf {
        let mut p = env::temp_dir();
        p.push(format!("nine_snake_test_{}.db", uuid::Uuid::new_v4()));
        p
    }

    fn sample() -> Memory {
        let mut m = Memory::new(
            MemoryType::Semantic,
            MemoryLayer::L3,
            "the quick brown fox",
            SourceKind::UserInput,
        );
        m.summary = MultiGranularity::new(
            "fox",
            "the quick brown fox",
            "the quick brown fox jumps over",
            "the quick brown fox jumps over the lazy dog",
        );
        m.embedding = vec![0.0; 4];
        m.importance = 0.42;
        m
    }

    #[tokio::test]
    async fn insert_and_get_round_trip() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let m = sample();
        store.insert(&m).await.unwrap();
        let got = store.get(&m.id).await.unwrap().unwrap();
        assert_eq!(got.id, m.id);
        assert_eq!(got.layer, MemoryLayer::L3);
        assert!((got.importance - 0.42).abs() < 1e-6);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn candidates_for_compression_skips_pinned() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let mut m = sample();
        m.pinned = true;
        store.insert(&m).await.unwrap();
        let cands = store.candidates_for_compression(0, 1.0, 100).await.unwrap();
        assert!(
            cands.is_empty(),
            "pinned memories must never be compression candidates"
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn candidates_for_compression_skips_already_compressed() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let mut a = sample();
        a.id = "absorbed".to_string();
        a.importance = 0.1;
        a.last_access = 0;
        let mut b = sample();
        b.id = "fresh".to_string();
        b.importance = 0.1;
        b.last_access = 0;
        store.insert(&a).await.unwrap();
        store.insert(&b).await.unwrap();
        store
            .update_compressed_from("absorbed", "summary-a")
            .await
            .unwrap();
        let cands = store.candidates_for_compression(0, 1.0, 100).await.unwrap();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].id, "fresh");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn list_by_layer_returns_matching_rows() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let mut a = sample();
        a.layer = MemoryLayer::L2;
        let mut b = sample();
        b.layer = MemoryLayer::L3;
        store.insert(&a).await.unwrap();
        store.insert(&b).await.unwrap();
        let l2 = store.list_by_layer(MemoryLayer::L2, 10).await.unwrap();
        assert_eq!(l2.len(), 1);
        assert_eq!(l2[0].id, a.id);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn get_many_returns_only_uncompressed() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let mut a = sample();
        a.id = "id-a".to_string();
        let mut b = sample();
        b.id = "id-b".to_string();
        store.insert(&a).await.unwrap();
        store.insert(&b).await.unwrap();
        store
            .update_compressed_from("id-a", "summary-x")
            .await
            .unwrap();

        let hits = store
            .get_many(&["id-a".to_string(), "id-b".to_string()])
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "compressed source must be excluded");
        assert_eq!(hits[0].id, "id-b");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn list_recent_excludes_compressed_rows() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let mut a = sample();
        a.id = "kept".to_string();
        let mut b = sample();
        b.id = "gone".to_string();
        store.insert(&a).await.unwrap();
        store.insert(&b).await.unwrap();
        store
            .update_compressed_from("gone", "summary-z")
            .await
            .unwrap();
        let recent = store.list_recent(10).await.unwrap();
        assert!(recent.iter().all(|m| m.id != "gone"));
        assert!(recent.iter().any(|m| m.id == "kept"));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn list_by_layer_excludes_compressed_rows() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let mut a = sample();
        a.id = "alive-l2".to_string();
        a.layer = MemoryLayer::L2;
        let mut b = sample();
        b.id = "dead-l2".to_string();
        b.layer = MemoryLayer::L2;
        store.insert(&a).await.unwrap();
        store.insert(&b).await.unwrap();
        store
            .update_compressed_from("dead-l2", "summary-l2")
            .await
            .unwrap();
        let l2 = store.list_by_layer(MemoryLayer::L2, 10).await.unwrap();
        assert_eq!(l2.len(), 1);
        assert_eq!(l2[0].id, "alive-l2");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn update_compressed_from_unknown_errors() {
        let path = temp_db_path();
        let store = SqliteStore::open(&path).unwrap();
        let res = store.update_compressed_from("nope", "sum").await;
        assert!(res.is_err());
        let _ = std::fs::remove_file(path);
    }
}
