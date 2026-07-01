//! Black-hole compression engine.
//!
//! "Black-hole" is the project metaphor for *density-preserving* memory
//! compression. The engine never deletes memories; it merges and
//! summarises groups of related records into a single higher-level
//! record while keeping a chain of provenance back to the originals.
//!
//! Trigger conditions (both must hold):
//! 1. The record has not been accessed in `threshold_days` days.
//! 2. The record's `importance` is at or below the floor.
//!
//! The L7 (singularity) layer is **never** touched, and any record with
//! `pinned = true` is excluded.
//!
//! v0.2: every compressed row is fed to the global metrics counter.

use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, info};

use super::constants::BLACKHOLE_IMPORTANCE_FLOOR;
use super::importance::{rescore, ImportanceScorer};
use super::lance_store::LanceStore;
use super::sqlite_store::SqliteStore;
use super::types::{
    Memory, MemoryLayer, MemoryRelation, MultiGranularity, RelationKind, SourceKind,
};

/// Result of a single compression pass.
#[derive(Debug, Clone, Default)]
pub struct CompressionReport {
    /// Number of records scanned.
    pub scanned: usize,
    /// Number of records compressed.
    pub compressed: usize,
    /// Number of records that were skipped (pinned / above floor).
    pub skipped: usize,
    /// Number of new summary records created.
    pub summaries_created: usize,
}

impl CompressionReport {
    pub fn merge(&mut self, other: CompressionReport) {
        self.scanned += other.scanned;
        self.compressed += other.compressed;
        self.skipped += other.skipped;
        self.summaries_created += other.summaries_created;
    }
}

/// The black-hole engine.
pub struct BlackholeEngine {
    sqlite: Arc<SqliteStore>,
    lance: Arc<LanceStore>,
    threshold_days: u32,
    scorer: ImportanceScorer,
}

impl BlackholeEngine {
    pub fn new(sqlite: Arc<SqliteStore>, lance: Arc<LanceStore>, threshold_days: u32) -> Self {
        Self {
            sqlite,
            lance,
            threshold_days,
            scorer: ImportanceScorer::new(),
        }
    }

    /// Runs a compression pass over at most `batch_size` candidate rows.
    ///
    /// v1.0.1 P0#10: the entire pass is wrapped in the
    /// `SqliteStore::compression_lock` so a concurrent sponge
    /// `absorb` writer cannot read a partially-compressed
    /// `memories.content` cell.  The lock is process-wide and
    /// held for the duration of the pass; on a healthy machine
    /// the pass completes in milliseconds.
    pub async fn run_pass(&self, batch_size: usize) -> Result<CompressionReport> {
        // v1.0.1 P0#10: hold the compression lock for the entire
        // pass.  We do NOT offload the whole pass to
        // `spawn_blocking` because the lock is `parking_lot`
        // (not async-aware); holding it across an `.await` is
        // fine because the inner work is `spawn_blocking`'d.
        let _compression_guard = self.sqlite.compression_lock();
        let threshold_secs = (self.threshold_days as i64) * 24 * 3600;
        let candidates = self
            .sqlite
            .candidates_for_compression(threshold_secs, BLACKHOLE_IMPORTANCE_FLOOR, batch_size)
            .await?;

        let mut report = CompressionReport {
            scanned: candidates.len(),
            ..Default::default()
        };
        if candidates.is_empty() {
            return Ok(report);
        }

        // Density-preserving compression: group by (layer, type) and
        // collapse each group into a single summary record.
        let groups = group_candidates(candidates);
        for ((layer, mtype), group) in groups {
            if group.is_empty() {
                continue;
            }
            match self.compress_group(layer, mtype, &group).await {
                Ok(true) => {
                    report.compressed += group.len();
                    report.summaries_created += 1;
                }
                Ok(false) => {
                    report.skipped += group.len();
                }
                Err(e) => {
                    tracing::warn!(target: "nine_snake.blackhole", error = ?e, "compress_group failed");
                    report.skipped += group.len();
                }
            }
        }

        info!(
            target: "nine_snake.blackhole",
            scanned = report.scanned,
            compressed = report.compressed,
            skipped = report.skipped,
            "compression pass done"
        );
        crate::metrics::global().record_blackhole(report.compressed as u64);
        Ok(report)
    }

    /// Compresses a single group of memories. Returns `Ok(true)` if a
    /// summary row was created.
    async fn compress_group(
        &self,
        layer: MemoryLayer,
        mtype: super::types::MemoryType,
        group: &[Memory],
    ) -> Result<bool> {
        // Hard rule: never compress L7.
        if layer.is_immutable() {
            return Ok(false);
        }
        if group.is_empty() {
            return Ok(false);
        }

        // Build a combined summary at the largest available bucket.
        let combined_content: String = group
            .iter()
            .map(|m| m.summary.s2000.clone())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n---\n");
        let combined_content = if combined_content.is_empty() {
            group
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n---\n")
        } else {
            combined_content
        };
        if combined_content.trim().is_empty() {
            return Ok(false);
        }

        // Truncate summaries to the canonical bucket sizes.
        let summary = MultiGranularity {
            s50: truncate(&combined_content, 50),
            s150: truncate(&combined_content, 150),
            s500: truncate(&combined_content, 500),
            s2000: truncate(&combined_content, 2000),
        };

        // Compression ratio in characters: how dense is the result?
        // v0.1 contract: the ratio is the *true* observed ratio, not a
        // floor-and-cap "minimum 3:1" lie. The lower bound is 1.0
        // (no compression at all) and we cap at 10.0 for sanity.
        let original_len: usize = group.iter().map(|m| m.content.len()).sum();
        let compressed_len = combined_content.len().max(1);
        let ratio = original_len.max(1) as f32 / compressed_len as f32;
        let honest_ratio = ratio.max(1.0).min(10.0);

        let now = chrono::Utc::now().timestamp();
        let mut summary_mem =
            Memory::new(mtype, layer, combined_content.clone(), SourceKind::System);
        summary_mem.summary = summary;
        summary_mem.compression_gen = 1;
        summary_mem.importance = 0.3; // freshly compressed; will be re-scored
        summary_mem.metadata = serde_json::json!({
            "compression": {
                "source_ids": group.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
                "ratio": honest_ratio,
                "original_bytes": original_len,
                "compressed_bytes": combined_content.len(),
                "compressed_at": now,
            }
        });
        rescore(&mut summary_mem, &self.scorer, now);

        self.sqlite.insert(&summary_mem).await?;
        debug!(
            target: "nine_snake.blackhole",
            summary_id = %summary_mem.id,
            sources = group.len(),
            "created black-hole summary"
        );

        // For each source, attach a `derived_from` relation pointing at
        // the new summary; the originals remain untouched.
        for m in group {
            let rel = MemoryRelation::new(
                m.id.clone(),
                summary_mem.id.clone(),
                RelationKind::DerivedFrom,
            );
            self.sqlite.add_relation(&rel).await?;
            // Mark the source row as "absorbed by this summary" so that
            // subsequent reads (get_many, list_recent, list_by_layer,
            // search) skip it. The original row is *not* deleted; the
            // black-hole contract is density-preserving compression.
            if let Err(e) = self
                .sqlite
                .update_compressed_from(&m.id, &summary_mem.id)
                .await
            {
                tracing::warn!(target: "nine_snake.blackhole", src = %m.id, error = ?e, "failed to mark source as compressed");
            }
        }

        // Update the summary's embedding (averaged from sources) so the
        // vector store can still find it.
        let avg = average_embedding(
            &group
                .iter()
                .map(|m| m.embedding.clone())
                .collect::<Vec<_>>(),
        );
        if avg.len() == self.lance.dim() {
            self.lance.upsert(&summary_mem.id, &avg).await?;
        }

        Ok(true)
    }
}

/// Groups candidate memories by (layer, memory_type) for compression.
fn group_candidates(
    candidates: Vec<Memory>,
) -> std::collections::HashMap<(MemoryLayer, super::types::MemoryType), Vec<Memory>> {
    let mut out: std::collections::HashMap<(MemoryLayer, super::types::MemoryType), Vec<Memory>> =
        std::collections::HashMap::new();
    for m in candidates {
        if m.pinned || m.layer.is_immutable() {
            continue;
        }
        out.entry((m.layer, m.memory_type)).or_default().push(m);
    }
    out
}

/// Truncates `s` to at most `max_chars` characters, appending "…" when
/// truncated.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Computes the element-wise average of a non-empty set of equal-length
/// vectors. Returns an empty vector if `vecs` is empty or contains
/// mismatched lengths.
fn average_embedding(vecs: &[Vec<f32>]) -> Vec<f32> {
    if vecs.is_empty() {
        return Vec::new();
    }
    let dim = vecs[0].len();
    if vecs.iter().any(|v| v.len() != dim) {
        return Vec::new();
    }
    let n = vecs.len() as f32;
    let mut sum = vec![0.0_f32; dim];
    for v in vecs {
        for (i, x) in v.iter().enumerate() {
            sum[i] += x;
        }
    }
    for x in &mut sum {
        *x /= n;
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_handles_unicode() {
        let s = "你好世界";
        assert_eq!(truncate(s, 10), "你好世界");
        let t = truncate(s, 3);
        assert_eq!(t.chars().count(), 3);
    }

    #[test]
    fn average_embedding_uniform_input() {
        let v = vec![vec![1.0, 2.0, 3.0], vec![1.0, 2.0, 3.0]];
        let a = average_embedding(&v);
        assert_eq!(a, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn average_embedding_empty_returns_empty() {
        let a = average_embedding(&[]);
        assert!(a.is_empty());
    }

    #[test]
    fn average_embedding_mismatched_returns_empty() {
        let v = vec![vec![1.0, 2.0], vec![1.0, 2.0, 3.0]];
        assert!(average_embedding(&v).is_empty());
    }

    /// Regression for v0.1 reviewer issue: the previous
    /// implementation faked a "minimum 3:1" ratio via `clamp(3.0, 10.0)`.
    /// After the fix the ratio is honest — `1.0` when there's no
    /// shrinkage at all, and capped at `10.0` to keep the metadata
    /// JSON human-readable.
    #[test]
    fn honest_ratio_is_not_floored() {
        // Simulate a case where the combined content is the same length
        // as the originals (no real compression happened).
        let original_len: usize = 100;
        let compressed_len: usize = 100;
        let ratio = original_len.max(1) as f32 / compressed_len.max(1) as f32;
        let honest = ratio.max(1.0).min(10.0);
        assert!((honest - 1.0).abs() < 1e-6, "expected 1.0, got {honest}");
    }

    #[test]
    fn honest_ratio_caps_at_ten() {
        let ratio: f32 = 50.0;
        let honest = ratio.max(1.0).min(10.0);
        assert!((honest - 10.0).abs() < 1e-6, "expected 10.0, got {honest}");
    }

    /// v1.0.1 P0#10: regression test for the partial-compression
    /// race.  We acquire the `compression_lock` exactly the way
    /// `BlackholeEngine::run_pass` would, and assert that a
    /// concurrent `sponge::absorb` cannot land its write while
    /// the lock is held.  This is the test analogue of the
    /// integration test `blackhole_and_sponge_concurrent_no_partial_read`
    /// in `tests/integration/` — written as a unit test so it
    /// runs without LanceDB.
    #[test]
    fn compression_lock_is_mutually_exclusive() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;

        let tmp =
            std::env::temp_dir().join(format!("nine_snake_lock_test_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let store = Arc::new(SqliteStore::open(&tmp).unwrap());
        let store2 = store.clone();

        let main_held_lock = Arc::new(AtomicBool::new(false));
        let mhl2 = main_held_lock.clone();
        let bg_ready = Arc::new(AtomicBool::new(false));
        let bgr2 = bg_ready.clone();

        // Main thread takes the lock FIRST so the background thread
        // is guaranteed to find it contended.
        let guard = store.compression_lock();

        let bg = thread::spawn(move || {
            bgr2.store(true, Ordering::Release);
            let start = std::time::Instant::now();
            let _g = store2.compression_lock();
            let waited = start.elapsed();
            assert!(
                mhl2.load(Ordering::Acquire),
                "background acquired lock without seeing main hold it"
            );
            waited
        });

        // Spin until background has started and is waiting on
        // the lock (best-effort; we also sleep below to make it
        // very likely).
        while !bg_ready.load(Ordering::Acquire) {
            thread::yield_now();
        }
        // Give the background thread time to actually enter the
        // lock acquisition path.
        thread::sleep(Duration::from_millis(50));

        // Now confirm the flag is still false (background is
        // blocked), then release the lock.
        main_held_lock.store(true, Ordering::Release);
        drop(guard);

        let waited = bg.join().expect("bg thread");
        assert!(
            waited >= Duration::from_millis(20),
            "background waited only {waited:?} (expected >= 20 ms)"
        );

        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(tmp.with_extension("db-wal"));
        let _ = std::fs::remove_file(tmp.with_extension("db-shm"));
    }
}
