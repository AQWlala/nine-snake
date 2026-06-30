//! Sponge absorption engine.
//!
//! The sponge is the *entry point* for new memories. When a new
//! [`Memory`] is absorbed it is:
//!
//! 1. Embedded (via the shared [`Embedder`]).
//! 2. Compared against existing memories in the vector store; if a
//!    cosine similarity is above `SPONGE_MERGE_THRESHOLD` the two
//!    records are merged instead of being inserted as a duplicate.
//! 3. Tagged with multi-granularity summaries derived from the content.
//! 4. Persisted to SQLite and to LanceDB.
//!
//! The engine is stateless: callers pass in the `Memory` and receive a
//! [`SpongeResult`] describing what happened.

use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, warn};

use super::constants::SPONGE_MERGE_THRESHOLD;
use super::constants::SUMMARY_BUCKETS;
use super::embedder::Embedder;
use super::entity_extractor::EntityExtractor;
use super::graph_search::{GraphSearchConfig, GraphSearchEngine};
use super::lance_store::LanceStore;
use super::sqlite_store::SqliteStore;
use super::types::{Memory, MemoryRelation, MultiGranularity, RelationKind, SourceKind};
use crate::llm::LlmGateway;
use crate::security::SensitiveScanner;

/// What happened when a memory was absorbed.
#[derive(Debug, Clone)]
pub enum SpongeResult {
    /// The new memory was inserted as a brand new record.
    Inserted { id: String },
    /// The new memory was merged into an existing one. The existing
    /// record's `id` is returned.
    Merged { id: String, similarity: f32 },
    /// The new memory was a perfect duplicate; the existing record was
    /// just touched.
    Duplicate { id: String },
}

impl SpongeResult {
    pub fn id(&self) -> &str {
        match self {
            SpongeResult::Inserted { id }
            | SpongeResult::Merged { id, .. }
            | SpongeResult::Duplicate { id } => id,
        }
    }
}

/// Sponge absorption engine.
pub struct SpongeEngine {
    sqlite: Arc<SqliteStore>,
    lance: Arc<LanceStore>,
    embedder: Arc<Embedder>,
    sensitive_scanner: SensitiveScanner,
    entity_extractor: Option<EntityExtractor>,
}

impl SpongeEngine {
    pub fn new(sqlite: Arc<SqliteStore>, lance: Arc<LanceStore>, embedder: Arc<Embedder>) -> Self {
        Self {
            sqlite,
            lance,
            embedder,
            sensitive_scanner: SensitiveScanner::new(),
            entity_extractor: None,
        }
    }

    pub fn with_llm(mut self, llm: LlmGateway) -> Self {
        self.entity_extractor = Some(EntityExtractor::new(llm));
        self
    }

    /// Absorbs a freshly created memory into the system.
    ///
    /// The supplied [`Memory`] may have an empty `embedding` field; this
    /// function will populate it.
    ///
    /// v1.0.1 P0#10: every write into the `memories` table
    /// (duplicate-touch, merge, or fresh-insert) is now wrapped
    /// in the process-wide `compression_lock` so a
    /// `BlackholeEngine::run_pass` can't be in the middle of
    /// rewriting the same row.  The cost is that absorb briefly
    /// serialises with compression; in exchange the reader in
    /// `sponge::absorb` can no longer observe a half-rewritten
    /// `memories.content` cell.  The lock is held only across
    /// the SQLite write 鈥?not across the (slow) embedding call
    /// 鈥?so latency is bounded.
    pub async fn absorb(&self, mut mem: Memory) -> Result<SpongeResult> {
        // 1. Normalise / strip.
        mem.content = normalise(&mem.content);
        mem.summary = derive_summaries(&mem.content);

        // v1.1 P1-4: 鍦ㄥ惛鏀跺墠鎵弿鏁忔劅鏁版嵁
        let (redacted_content, sensitive_categories) = self.sensitive_scanner.scan(&mem.content);
        if !sensitive_categories.is_empty() {
            tracing::warn!(
                target: "nine_snake.memory",
                ?sensitive_categories,
                "sensitive data detected in memory; redacted before storage"
            );
            mem.content = redacted_content;
        }

        // 2. Embed.
        if mem.embedding.is_empty() {
            mem.embedding = self.embedder.embed(&mem.content).await?;
        }
        if mem.embedding.len() != self.lance.dim() {
            // Embedder enforces dim; this is defensive.
            anyhow::bail!("embedding dim mismatch with vector store");
        }

        // 3. De-duplicate via the vector store.
        let top = self.lance.search(&mem.embedding, 3).await?;
        if let Some((existing_id, sim)) = top.first().cloned() {
            if sim >= SPONGE_MERGE_THRESHOLD {
                if sim > 0.99 {
                    // Effectively identical. Touch and bail.
                    // v1.0.1 P0#10: hold the compression lock only
                    // around the SQLite write.
                    if let Some(mut existing) = self.sqlite.get(&existing_id).await? {
                        let now = chrono::Utc::now().timestamp();
                        existing.touch(now);
                        self.sqlite.update_guarded_spawn(&existing).await?;
                        debug!(target: "nine_sponge", id = %existing_id, sim, "duplicate absorbed");
                        return Ok(SpongeResult::Duplicate { id: existing_id });
                    }
                }
                // Merge: append content, keep the higher-importance
                // slot. The original record is preserved (pinned or
                // not) so the sponge never destroys data 鈥?the black
                // hole is the only engine that compresses.
                if let Some(mut existing) = self.sqlite.get(&existing_id).await? {
                    existing.content = merge_content(&existing.content, &mem.content);
                    existing.summary = derive_summaries(&existing.content);
                    if mem.importance > existing.importance {
                        existing.importance = mem.importance;
                    }
                    existing.access_count = existing.access_count.saturating_add(1);
                    let now = chrono::Utc::now().timestamp();
                    existing.last_access = now;
                    // v1.0.1 P0#10: hold the compression lock for
                    // the duration of the merge write.  We do NOT
                    // re-embed while holding the lock because the
                    // embed call is async and would block
                    // compress for its full duration.
                    let (new_emb, updated_id) = {
                        self.sqlite.update_guarded_spawn(&existing).await?;
                        (existing.content.clone(), existing.id.clone())
                    };
                    // Re-embed merged content after the lock is
                    // released.  The vector store upsert is
                    // independent of the SQLite row state.
                    let new_emb_vec = self.embedder.embed(&new_emb).await?;
                    self.lance.upsert(&updated_id, &new_emb_vec).await?;
                    debug!(target: "nine_sponge", id = %updated_id, sim, "merged into existing");
                    return Ok(SpongeResult::Merged {
                        id: updated_id,
                        similarity: sim,
                    });
                }
            }
        }

        // 4. Insert fresh.
        let now = chrono::Utc::now().timestamp();
        if mem.last_access == 0 {
            mem.last_access = now;
        }
        if mem.created_at == 0 {
            mem.created_at = now;
        }
        // Tag with source metadata so the front-end can show provenance.
        if mem.metadata.get("absorbed_at").is_none() {
            if let serde_json::Value::Object(ref mut map) = mem.metadata {
                map.insert("absorbed_at".to_string(), serde_json::Value::from(now));
                map.insert(
                    "absorbed_via".to_string(),
                    serde_json::Value::from("sponge"),
                );
            }
        }

        // v1.0.1 P0#10: hold the compression lock around the
        // SQLite insert + commit log.
        //
        // v1.0.1 fix B: split the locked section into two
        // halves so the `parking_lot::MutexGuard` (which is
        // `!Send`) is **never** held across an `.await`.
        // `self.lance.upsert(...).await` is the only async
        // call between the SQLite write and the commit log,
        // so we drop the lock around it.  The brief
        // window where neither lock nor the await is held
        // is acceptable: the blackhole pass would still see
        // either the pre-insert state (row missing) or the
        // post-insert state (row present), never a
        // half-written one, because the insert is a single
        // SQL statement that is atomic at the SQLite level.
        {
            if mem.is_sensitive() {
                mem.summary.s2000.clear();
                mem.summary.s500 = redact_marker(&mem.summary.s500);
                mem.summary.s150 = redact_marker(&mem.summary.s150);
                mem.summary.s50 = redact_marker(&mem.summary.s50);
                if let serde_json::Value::Object(ref mut map) = mem.metadata {
                    map.insert("masked".to_string(), serde_json::Value::from(true));
                    map.insert(
                        "mask_reason".to_string(),
                        serde_json::Value::from("sensitive-content-predicate"),
                    );
                }
            }
            self.sqlite.insert_guarded_spawn(&mem).await?;
        }
        // Async write to the vector index 鈥?outside the
        // parking_lot lock so the future stays `Send`.
        self.lance.upsert(&mem.id, &mem.embedding).await?;
        self.sqlite
            .log_commit(
                &uuid::Uuid::new_v4().to_string(),
                None,
                "store",
                &mem.id,
                &serde_json::json!({
                    "source": mem.source.as_str(),
                    "layer": mem.layer.as_str(),
                    "masked": mem.is_sensitive(),
                }),
                "sponge",
                "absorbed new memory",
            )
            .await?;

        // If there are near neighbours below the merge threshold, link
        // them with a "references" relation so the knowledge graph grows.
        for (nid, nsim) in top.iter() {
            if *nsim >= 0.6 && *nsim < SPONGE_MERGE_THRESHOLD {
                let mut rel =
                    MemoryRelation::new(mem.id.clone(), nid.clone(), RelationKind::References);
                rel.weight = *nsim;
                let _ = self.sqlite.add_relation(&rel).await;
            }
        }

        // V2-T-22: LLM-driven entity extraction for richer relations.
        if let Some(ref extractor) = self.entity_extractor {
            let existing_ids: Vec<String> = top.iter().map(|(id, _)| id.clone()).collect();
            match extractor
                .extract(&mem.id, &mem.content, &existing_ids)
                .await
            {
                Ok(extracted) => {
                    for er in extracted {
                        let mut rel = MemoryRelation::new(er.from_id, er.to_id, er.relation);
                        if let Some(evidence) = er.evidence {
                            rel = rel.with_evidence(evidence);
                        }
                        if let Err(e) = self.sqlite.add_relation(&rel).await {
                            warn!(target: "nine_sponge", error = %e, "failed to insert extracted relation");
                        }
                    }
                }
                Err(e) => {
                    warn!(target: "nine_sponge", error = %e, "entity extraction failed; continuing with cosine-based relations");
                }
            }
        }

        debug!(target: "nine_sponge", id = %mem.id, "absorbed new memory");
        Ok(SpongeResult::Inserted { id: mem.id })
    }

    /// Convenience: build a fresh [`Memory`] from raw inputs and absorb
    /// it. Useful for the Tauri command layer.
    pub async fn absorb_text(
        &self,
        memory_type: super::types::MemoryType,
        layer: super::types::MemoryLayer,
        content: impl Into<String>,
        source: SourceKind,
    ) -> Result<SpongeResult> {
        let m = Memory::new(memory_type, layer, content, source);
        self.absorb(m).await
    }

    /// Hybrid search: vector similarity + optional graph traversal expansion.
    ///
    /// Returns memory IDs from both vector search and graph expansion.
    /// The graph traversal uses BFS from the seed IDs found by vector
    /// search, following `MemoryRelation` edges.
    pub async fn search_with_graph(
        &self,
        query: &str,
        k: usize,
        graph_config: Option<GraphSearchConfig>,
    ) -> Result<Vec<(String, f32)>> {
        let query_emb = self.embedder.embed(query).await?;
        let mut hits = self.lance.search(&query_emb, k).await?;

        if let Some(ref cfg) = graph_config {
            let seed_ids: Vec<String> = hits.iter().map(|(id, _)| id.clone()).collect();
            if !seed_ids.is_empty() {
                let graph_engine = GraphSearchEngine::new((*self.sqlite).clone());
                let graph_results = graph_engine.traverse(&seed_ids, cfg);
                for gr in graph_results {
                    if !hits.iter().any(|(id, _)| id == &gr.memory_id) {
                        let score = 1.0 / (1.0 + gr.hops as f32);
                        hits.push((gr.memory_id, score));
                    }
                }
            }
        }

        hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(hits)
    }
}

/// Normalises a piece of text: trims, collapses internal whitespace.
fn normalise(s: &str) -> String {
    let trimmed = s.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut prev_ws = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

/// Produces four summaries at the canonical bucket sizes by truncating
/// the content. The longest bucket (`2000`) is the content itself when
/// shorter than 2000 chars.
fn derive_summaries(content: &str) -> MultiGranularity {
    let mut buckets = vec![String::new(); 4];
    for (i, target) in SUMMARY_BUCKETS.iter().enumerate() {
        buckets[i] = truncate_chars(content, *target);
    }
    MultiGranularity {
        s50: buckets[0].clone(),
        s150: buckets[1].clone(),
        s500: buckets[2].clone(),
        s2000: buckets[3].clone(),
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Merges two content blobs by appending a separator.
fn merge_content(a: &str, b: &str) -> String {
    if a.is_empty() {
        return b.to_string();
    }
    if b.is_empty() {
        return a.to_string();
    }
    format!("{a}\n---\n{b}")
}

/// v1.0.1 P0#12: short, neutral replacement shown to the user
/// in place of a redacted summary.  We intentionally do NOT
/// include the trigger token (e.g. "secret") in the marker so a
/// future log search doesn't accidentally confirm the
/// redaction was triggered.
const REDACT_MARKER: &str = "[redacted: sensitive content]";

/// Returns a redacted replacement for `s`.  If `s` is already
/// empty we keep it empty; otherwise we collapse the whole
/// summary to the canonical marker.
fn redact_marker(s: &str) -> String {
    if s.is_empty() {
        String::new()
    } else {
        REDACT_MARKER.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // v1.0.1 P0#12: the masking test needs the type constructors
    // that the production `use super::types::*` line is the only
    // thing that brings into scope.  Re-import them locally so the
    // test doesn't depend on the parent module's use list.
    use crate::memory::types::{MemoryLayer, MemoryType};

    #[test]
    fn normalise_collapses_whitespace() {
        let n = normalise("  hello   world\n\nfoo  ");
        assert_eq!(n, "hello world foo");
    }

    #[test]
    fn derive_summaries_respects_bucket_sizes() {
        let long: String = "a".repeat(3000);
        let s = derive_summaries(&long);
        assert!(s.s50.chars().count() <= 50);
        assert!(s.s150.chars().count() <= 150);
        assert!(s.s500.chars().count() <= 500);
        assert!(s.s2000.chars().count() <= 2000);
    }

    #[test]
    fn merge_content_dedupes_empty() {
        assert_eq!(merge_content("", "x"), "x");
        assert_eq!(merge_content("x", ""), "x");
        assert!(merge_content("a", "b").contains("---"));
    }

    /// v1.0.1 P0#12: a sensitive `Memory` (one whose `content`
    /// matches the predicate) must have its `s2000` summary
    /// blanked out before the row is written.  We test the
    /// `redact_marker` helper and the predicate plumbing
    /// without standing up the full SpongeEngine (which
    /// requires a LanceDB instance).
    #[test]
    fn summary_masks_api_key_pattern() {
        // Build a sensitive memory.
        let mut m = Memory::new(
            MemoryType::Semantic,
            MemoryLayer::L3,
            "MY_API_KEY=sk-abc123def456ghi789jkl012mno345pqr678stu901vwx",
            SourceKind::UserInput,
        );
        m.summary = MultiGranularity {
            s50: "MY_API_KEY=sk-abc123def456ghi789jkl012mno345pqr678stu901vwx".into(),
            s150: "MY_API_KEY=sk-abc123def456ghi789jkl012mno345pqr678stu901vwx".into(),
            s500: "MY_API_KEY=sk-abc123def456ghi789jkl012mno345pqr678stu901vwx".into(),
            s2000: "MY_API_KEY=sk-abc123def456ghi789jkl012mno345pqr678stu901vwx".into(),
        };
        // The predicate must flag the content.
        assert!(m.is_sensitive(), "predicate missed a clear api_key match");
        // Apply the masking pipeline (mirrors what sponge::absorb
        // does at write time).
        m.summary.s2000.clear();
        m.summary.s500 = redact_marker(&m.summary.s500);
        m.summary.s150 = redact_marker(&m.summary.s150);
        m.summary.s50 = redact_marker(&m.summary.s50);
        // s2000 must be empty (the secret is gone).
        assert!(m.summary.s2000.is_empty(), "s2000 must be cleared");
        // The shorter summaries are replaced with the marker,
        // not the secret.
        for s in [&m.summary.s50, &m.summary.s150, &m.summary.s500] {
            assert!(!s.contains("sk-abc"), "summary leaked secret: {s}");
            assert!(
                s.contains("redacted") || s.is_empty(),
                "summary should be marker or empty, got: {s}"
            );
        }
        // The raw `content` is left untouched 鈥?the masking
        // affects the persisted summaries, not the in-memory
        // record (the latter is for the engine's own use).
        assert!(m.content.contains("sk-abc"));
    }
}
