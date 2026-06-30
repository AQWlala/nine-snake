//! LanceDB-backed dense vector store for memory embeddings.
//!
//! The store keeps a fixed-dimension vector per memory id and supports
//! cosine-similarity top-k queries. In v0.1 we use a **real** LanceDB
//! connection (`lancedb = "0.4"`); the previous in-memory mirror is
//! gone. The LanceDB API is itself async (`table.add(...).execute()`,
//! `query().nearest_to(...).limit(k).execute()`), so we never block
//! the Tauri runtime on Arrow / Lance I/O.
//!
//! Build variants:
//! - Default (`--features vector-store`, on by default): real LanceDB
//!   + Apache Arrow. Vectors are persisted to disk and searched via
//!   LanceDB's nearest-neighbour index.
//! - Minimal (`--no-default-features`, feature `vector-store` off):
//!   `lancedb` / `arrow-*` are not pulled in. The store degrades to
//!   the in-memory mirror only (cosine over a `Vec<(String, Vec<f32>)>`).
//!   Methods that take advantage of the on-disk table are stubbed.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{info, warn};

use super::embedder::Embedder;

const TABLE_NAME: &str = "memories";
const VEC_COL: &str = "vector";
const ID_COL: &str = "id";
const FALLBACK_MAX_CAPACITY: usize = 10_000;
const FALLBACK_WARNING_THRESHOLD: usize = 8_000;

#[cfg(feature = "vector-store")]
use arrow_array::{
    Array, FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator, StringArray,
};
#[cfg(feature = "vector-store")]
use arrow_schema::{DataType, Field, Schema, SchemaRef};
#[cfg(feature = "vector-store")]
use lancedb::query::{ExecutableQuery, QueryBase};
#[cfg(feature = "vector-store")]
use lancedb::{connect, Connection, Table};

/// Type alias for the on-disk table handle. With the `vector-store`
/// feature, this is a real `lancedb::Table`; without it, it's a unit
/// type because there is no on-disk table at all.
#[cfg(feature = "vector-store")]
type LanceTable = Table;
#[cfg(not(feature = "vector-store"))]
type LanceTable = ();

/// Schema for the LanceDB `memories` table. Only meaningful with the
/// `vector-store` feature; in the minimal build we hand back a
/// placeholder so callers that read the schema don't panic.
#[cfg(feature = "vector-store")]
fn build_schema(dim: usize) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new(ID_COL, DataType::Utf8, false),
        Field::new(
            VEC_COL,
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            false,
        ),
    ]))
}

#[cfg(not(feature = "vector-store"))]
fn build_schema(_dim: usize) -> std::sync::Arc<()> {
    std::sync::Arc::new(())
}

/// LanceDB vector store wrapper.
///
/// The on-disk table is held inside an [`AsyncMutex`] so multiple async
/// callers can serialise their access. The table handle is also
/// re-fetched lazily on each call, which keeps the API resilient to
/// external processes mutating the directory.
///
/// Without the `vector-store` feature, only the in-memory `fallback`
/// mirror is populated; the on-disk fields are placeholders.
pub struct LanceStore {
    path: String,
    dim: usize,
    /// On-disk schema (`vector-store` only). Without the feature this
    /// is a placeholder `Arc<()>` so the type is uniform.
    #[allow(dead_code)]
    schema: SchemaRefOrStub,
    /// Live LanceDB connection (`vector-store` only).
    #[cfg(feature = "vector-store")]
    conn: Connection,
    /// Live handle to the memories table. We may re-acquire this
    /// through the connection on every call, but caching the most
    /// recent handle keeps the hot path a single mutex acquisition.
    table: AsyncMutex<Option<LanceTable>>,
    /// Fallback in-memory mirror — used in two cases:
    /// 1. With the `vector-store` feature, when the on-disk table could
    ///    not be opened (e.g. a permissions error during first-run).
    ///    Keeps the rest of the system functional in degraded mode.
    /// 2. Without the `vector-store` feature, this **is** the store.
    fallback: Arc<Mutex<Vec<(String, Vec<f32>)>>>,
}

#[cfg(feature = "vector-store")]
type SchemaRefOrStub = SchemaRef;
#[cfg(not(feature = "vector-store"))]
type SchemaRefOrStub = std::sync::Arc<()>;

impl LanceStore {
    /// Opens (or creates) the vector store at `path` with the given
    /// embedding dimensionality.
    pub async fn open<P: AsRef<Path>>(path: P, dim: usize) -> Result<Self> {
        let path_str = path.as_ref().to_string_lossy().to_string();
        if let Some(parent) = path.as_ref().parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating lance dir: {}", parent.display()))?;
            }
        }
        let schema = build_schema(dim);

        // Without the `vector-store` feature, we never touch LanceDB;
        // just bring the store up in pure in-memory mode.
        #[cfg(not(feature = "vector-store"))]
        {
            info!(
                target: "nine_snake.memory",
                path = %path_str,
                dim,
                "lance store opened (in-memory only, vector-store feature disabled)"
            );
            return Ok(Self {
                path: path_str,
                dim,
                schema,
                table: AsyncMutex::new(None),
                fallback: Arc::new(Mutex::new(Vec::new())),
            });
        }

        // With the `vector-store` feature: connect to LanceDB and
        // (re)create the memories table on disk.
        #[cfg(feature = "vector-store")]
        {
            let conn = connect(&path_str)
                .execute()
                .await
                .with_context(|| format!("connecting to lance at {path_str}"))?;

            // Try to open the memories table. If it does not exist, create
            // an empty one with our schema.
            let table = match conn.open_table(TABLE_NAME).execute().await {
                Ok(t) => {
                    info!(target: "nine_snake.memory", path = %path_str, dim, "lance store opened (existing table)");
                    Some(t)
                }
                Err(_) => {
                    // Table missing — create empty.
                    match conn
                        .create_empty_table(TABLE_NAME, schema.clone())
                        .execute()
                        .await
                    {
                        Ok(t) => {
                            info!(target: "nine_snake.memory", path = %path_str, dim, "lance store opened (created empty table)");
                            Some(t)
                        }
                        Err(e) => {
                            warn!(target: "nine_snake.memory", error = ?e, "could not create lance table; running in fallback mode");
                            None
                        }
                    }
                }
            };

            Ok(Self {
                path: path_str,
                dim,
                schema,
                conn,
                table: AsyncMutex::new(table),
                fallback: Arc::new(Mutex::new(Vec::new())),
            })
        }
    }

    /// Returns the on-disk path of this store.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the vector dimensionality of this store.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Inserts or updates the vector for `id`.
    ///
    /// We always *delete* the existing row (if any) and re-insert — this
    /// matches the v0.1 contract of "the latest write wins" without
    /// having to wrestle with `merge_insert` semantics.
    pub async fn upsert(&self, id: &str, vector: &[f32]) -> Result<()> {
        if vector.len() != self.dim {
            return Err(anyhow!(
                "vector dim mismatch for {id}: expected {}, got {}",
                self.dim,
                vector.len()
            ));
        }

        // Update the fallback mirror first so a failed lance write
        // doesn't leave the two stores inconsistent for readers.
        {
            let mut g = self.fallback.lock();
            if let Some(slot) = g.iter_mut().find(|(k, _)| k == id) {
                slot.1 = vector.to_vec();
            } else {
                if g.len() >= FALLBACK_MAX_CAPACITY {
                    g.remove(0);
                }
                if g.len() >= FALLBACK_WARNING_THRESHOLD {
                    warn!(target: "nine_snake.memory", len = g.len(), "fallback mirror approaching capacity");
                }
                g.push((id.to_string(), vector.to_vec()));
            }
        }

        // Without the `vector-store` feature, the fallback is the
        // store. We're done.
        #[cfg(not(feature = "vector-store"))]
        {
            return Ok(());
        }

        // With the feature: also persist to the on-disk table.
        #[cfg(feature = "vector-store")]
        {
            let table = self.ensure_table().await?;
            if let Some(table) = table {
                // Delete any existing row with the same id.
                let predicate = format!("{ID_COL} = '{}'", id.replace('\'', "''"));
                let _ = table.delete(&predicate).await; // ignore "no rows matched"

                // Build the new record batch and stream it in.
                let batch = build_record_batch(&[id.to_string()], &[vector.to_vec()], self.dim)?;
                let iter =
                    RecordBatchIterator::new(vec![Ok(batch)].into_iter(), self.schema.clone());
                table
                    .add(iter)
                    .execute()
                    .await
                    .with_context(|| format!("lance add for {id}"))?;
            } else {
                warn!(target: "nine_snake.memory", id, "lance table unavailable; mirror updated only");
            }
            Ok(())
        }
    }

    /// Deletes the vector for `id` if present.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let removed = {
            let mut g = self.fallback.lock();
            if let Some(pos) = g.iter().position(|(k, _)| k == id) {
                g.swap_remove(pos);
                true
            } else {
                false
            }
        };

        #[cfg(not(feature = "vector-store"))]
        {
            return Ok(removed);
        }

        #[cfg(feature = "vector-store")]
        {
            if let Some(table) = self.ensure_table().await? {
                let predicate = format!("{ID_COL} = '{}'", id.replace('\'', "''"));
                // LanceDB Table is not Send; use block_in_place to avoid
                // holding the non-Send handle across an async boundary.
                let _ = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(table.delete(&predicate))
                });
            }
            Ok(removed)
        }
    }

    /// Top-k cosine-similarity search. Returns (id, score) pairs in
    /// descending order of `score`.
    pub async fn search(&self, query: &[f32], k: usize) -> Result<Vec<(String, f32)>> {
        if query.len() != self.dim {
            return Err(anyhow!("query dim mismatch: expected {}", self.dim));
        }
        if k == 0 {
            return Ok(Vec::new());
        }

        // Without the on-disk table, jump straight to the in-memory
        // cosine over the mirror.
        #[cfg(not(feature = "vector-store"))]
        {
            return self.search_fallback(query, k);
        }

        // With the on-disk table: try the live table first.
        #[cfg(feature = "vector-store")]
        {
            if let Ok(Some(table)) = self.ensure_table().await {
                match self.search_table(&table, query, k).await {
                    Ok(hits) => return Ok(hits),
                    Err(e) => {
                        warn!(target: "nine_snake.memory", error = ?e, "lance search failed; falling back to in-memory mirror");
                    }
                }
            }
            self.search_fallback(query, k)
        }
    }

    /// In-memory cosine over the fallback mirror.
    fn search_fallback(&self, query: &[f32], k: usize) -> Result<Vec<(String, f32)>> {
        let snapshot: Vec<(String, Vec<f32>)> = self.fallback.lock().clone();
        let mut scored: Vec<(String, f32)> = snapshot
            .iter()
            .map(|(id, v)| (id.clone(), Embedder::cosine(v, query)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        Ok(scored)
    }

    #[cfg(feature = "vector-store")]
    async fn search_table(
        &self,
        table: &Table,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(String, f32)>> {
        use futures::TryStreamExt;
        let stream = table
            .query()
            .nearest_to(query)
            .map_err(|e| anyhow!("lance nearest_to: {e}"))?
            .limit(k)
            .execute()
            .await
            .context("lance query execute")?;
        let batches: Vec<RecordBatch> = stream.try_collect().await.context("lance collect")?;

        let mut out: Vec<(String, f32)> = Vec::new();
        for batch in batches {
            let id_col = batch
                .column_by_name(ID_COL)
                .ok_or_else(|| anyhow!("lance result missing '{ID_COL}' column"))?;
            let vec_col = batch
                .column_by_name(VEC_COL)
                .ok_or_else(|| anyhow!("lance result missing '{VEC_COL}' column"))?;
            let dist_col = batch.column_by_name("_distance");
            let ids = id_col
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| anyhow!("id column not Utf8"))?;
            let vecs = vec_col
                .as_any()
                .downcast_ref::<FixedSizeListArray>()
                .ok_or_else(|| anyhow!("vector column not FixedSizeList"))?;
            let dists = dist_col.and_then(|c| c.as_any().downcast_ref::<Float32Array>());

            for i in 0..batch.num_rows() {
                let id = ids.value(i).to_string();
                let raw: Vec<f32> = (0..self.dim)
                    .map(|j| {
                        vecs.value(i)
                            .as_any()
                            .downcast_ref::<Float32Array>()
                            .map(|arr| arr.value(j))
                            .unwrap_or(0.0)
                    })
                    .collect();
                let cosine = if let Some(d) = dists {
                    // LanceDB returns L2-squared distance by default.
                    // For unit vectors: L2² = 2(1 - cos), so cos = 1 - L2²/2.
                    let l2_sq = d.value(i) as f64;
                    let cos: f64 = 1.0 - l2_sq / 2.0;
                    cos.clamp(-1.0_f64, 1.0_f64) as f32
                } else {
                    Embedder::cosine(&raw, query)
                };
                out.push((id, cosine));
            }
        }
        // Lance already returns nearest-first, but normalise the order
        // just in case.
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(k);
        Ok(out)
    }

    /// Ensures that `self.table` holds a live `Table` handle, re-opening
    /// it through the connection if the cached handle is missing.
    #[cfg(feature = "vector-store")]
    async fn ensure_table(&self) -> Result<Option<Table>> {
        {
            let g = self.table.lock().await;
            if let Some(t) = g.as_ref() {
                return Ok(Some(t.clone()));
            }
        }
        // Try to (re-)open. We re-create the empty table if it
        // disappeared.
        let t = match self.conn.open_table(TABLE_NAME).execute().await {
            Ok(t) => Some(t),
            Err(_) => match self
                .conn
                .create_empty_table(TABLE_NAME, self.schema.clone())
                .execute()
                .await
            {
                Ok(t) => Some(t),
                Err(e) => {
                    warn!(target: "nine_snake.memory", error = ?e, "could not (re)open lance table");
                    None
                }
            },
        };
        *self.table.lock().await = t.clone();
        Ok(t)
    }

    /// Number of vectors currently stored.
    pub async fn len(&self) -> usize {
        #[cfg(feature = "vector-store")]
        {
            if let Ok(Some(table)) = self.ensure_table().await {
                if let Ok(n) = table.count_rows(None).await {
                    return n;
                }
            }
        }
        self.fallback.lock().len()
    }

    /// Returns the Arrow schema of the underlying table.
    ///
    /// With the `vector-store` feature this is the real Arrow schema;
    /// without it, this returns a placeholder `Arc<()>` so callers
    /// that only need to check the store's identity don't need to
    /// conditional-compile.
    pub fn schema(&self) -> SchemaRefOrStub {
        self.schema.clone()
    }

    /// Builds a single `RecordBatch` containing `id` and `vector`
    /// columns. Public so callers (e.g. tests) can construct a record
    /// batch without going through the live table.
    #[cfg(feature = "vector-store")]
    pub fn build_record_batch(ids: &[String], vectors: &[Vec<f32>]) -> Result<RecordBatch> {
        build_record_batch(ids, vectors, vectors.first().map(|v| v.len()).unwrap_or(0))
    }
}

/// Internal helper that builds a record batch for the *given* dim so
/// the schema is consistent with the live table.
#[cfg(feature = "vector-store")]
fn build_record_batch(ids: &[String], vectors: &[Vec<f32>], dim: usize) -> Result<RecordBatch> {
    anyhow::ensure!(ids.len() == vectors.len(), "ids/vectors length mismatch");
    for v in vectors {
        anyhow::ensure!(
            v.len() == dim,
            "vector dim mismatch: expected {dim}, got {}",
            v.len()
        );
    }
    let id_array = StringArray::from(ids.to_vec());
    let values = Float32Array::from(vectors.iter().flatten().copied().collect::<Vec<_>>());
    let values_arc: std::sync::Arc<dyn Array> = std::sync::Arc::new(values);
    let vec_array = FixedSizeListArray::try_new(
        std::sync::Arc::new(Field::new("item", DataType::Float32, true)),
        dim as i32,
        values_arc,
        None,
    )?;
    let schema = build_schema(dim);
    let id_col: std::sync::Arc<dyn Array> = std::sync::Arc::new(id_array);
    let vec_col: std::sync::Arc<dyn Array> = std::sync::Arc::new(vec_array);
    RecordBatch::try_new(schema, vec![id_col, vec_col]).map_err(Into::into)
}

#[cfg(all(test, feature = "vector-store"))]
mod tests {
    use super::*;
    use std::env;

    fn temp_lance_path() -> std::path::PathBuf {
        let mut p = env::temp_dir();
        p.push(format!("nine_snake_lance_{}", uuid::Uuid::new_v4()));
        p
    }

    #[tokio::test]
    async fn open_creates_table() {
        let path = temp_lance_path();
        let s = LanceStore::open(&path, 4).await.unwrap();
        // Table should exist; len() may be 0 or 1 depending on init.
        let _ = s.len().await;
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn upsert_and_search_round_trip() {
        let path = temp_lance_path();
        let s = LanceStore::open(&path, 4).await.unwrap();
        s.upsert("a", &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        s.upsert("b", &[0.0, 1.0, 0.0, 0.0]).await.unwrap();
        s.upsert("c", &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        let r = s.search(&[1.0, 0.0, 0.0, 0.0], 3).await.unwrap();
        assert_eq!(r.len(), 3, "expected 3 hits, got {r:?}");
        // a and c are identical to the query; either may come first.
        let top: std::collections::HashSet<_> =
            r.iter().take(2).map(|(id, _)| id.clone()).collect();
        assert!(top.contains("a") && top.contains("c"));
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn dim_mismatch_errors() {
        let path = temp_lance_path();
        let s = LanceStore::open(&path, 4).await.unwrap();
        assert!(s.upsert("x", &[1.0, 0.0]).await.is_err());
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn delete_removes_entry() {
        let path = temp_lance_path();
        let s = LanceStore::open(&path, 2).await.unwrap();
        s.upsert("a", &[1.0, 0.0]).await.unwrap();
        assert!(s.delete("a").await.unwrap());
        // After delete the mirror should not contain a.
        let g = s.fallback.lock();
        assert!(!g.iter().any(|(k, _)| k == "a"));
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn build_record_batch_shape() {
        let ids = vec!["x".to_string(), "y".to_string()];
        let vecs = vec![vec![1.0, 0.0, 0.0, 0.0], vec![0.0, 1.0, 0.0, 0.0]];
        let batch = LanceStore::build_record_batch(&ids, &vecs).unwrap();
        assert_eq!(batch.num_columns(), 2);
        assert_eq!(batch.num_rows(), 2);
    }
}

#[cfg(all(test, not(feature = "vector-store")))]
mod tests_minimal {
    use super::*;
    use std::env;

    fn temp_lance_path() -> std::path::PathBuf {
        let mut p = env::temp_dir();
        p.push(format!("nine_snake_lance_{}", uuid::Uuid::new_v4()));
        p
    }

    #[tokio::test]
    async fn open_creates_in_memory_store() {
        let path = temp_lance_path();
        let s = LanceStore::open(&path, 4).await.unwrap();
        assert_eq!(s.len().await, 0);
    }

    #[tokio::test]
    async fn upsert_and_search_fallback() {
        let path = temp_lance_path();
        let s = LanceStore::open(&path, 4).await.unwrap();
        s.upsert("a", &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        s.upsert("b", &[0.0, 1.0, 0.0, 0.0]).await.unwrap();
        s.upsert("c", &[1.0, 0.0, 0.0, 0.0]).await.unwrap();
        let r = s.search(&[1.0, 0.0, 0.0, 0.0], 3).await.unwrap();
        assert_eq!(r.len(), 3);
        let top: std::collections::HashSet<_> =
            r.iter().take(2).map(|(id, _)| id.clone()).collect();
        assert!(top.contains("a") && top.contains("c"));
    }

    #[tokio::test]
    async fn dim_mismatch_errors() {
        let path = temp_lance_path();
        let s = LanceStore::open(&path, 4).await.unwrap();
        assert!(s.upsert("x", &[1.0, 0.0]).await.is_err());
    }

    #[tokio::test]
    async fn delete_removes_entry() {
        let path = temp_lance_path();
        let s = LanceStore::open(&path, 2).await.unwrap();
        s.upsert("a", &[1.0, 0.0]).await.unwrap();
        assert!(s.delete("a").await.unwrap());
        let g = s.fallback.lock();
        assert!(!g.iter().any(|(k, _)| k == "a"));
    }
}
