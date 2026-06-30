//! Embedding client that wraps the local Ollama HTTP endpoint.
//!
//! BGE-small-zh-v1.5 produces 512-dimensional vectors and is small enough
//! to run on a laptop GPU. We keep a tiny in-process LRU cache (using
//! the `lru` crate) so that re-embedding the same chunk of text does
//! not hit the network twice.
//!
//! v0.2: cache hits and misses are also fed to the global metrics
//! counter so the front-end can plot the cache effectiveness.

use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use lru::LruCache;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::llm::ollama::OllamaClient;

/// Configuration payload for an embedding request (matches Ollama's
/// `/api/embeddings` endpoint).
#[derive(Debug, Clone, Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    prompt: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
struct EmbedResponse {
    embedding: Vec<f32>,
}

/// Default capacity for the LRU embedding cache.
const CACHE_CAPACITY: usize = 512;

/// Embedding cache + Ollama client glue.
pub struct Embedder {
    client: OllamaClient,
    model: String,
    dim: usize,
    /// Bounded LRU cache: when it overflows the least-recently-used
    /// entry is evicted in O(1). The `parking_lot::Mutex` provides
    /// interior mutability (and `Send + Sync` since the cache itself
    /// is `Send`).
    cache: Arc<Mutex<LruCache<String, Vec<f32>>>>,
}

impl Embedder {
    /// Creates a new embedder. `dim` is the expected vector length.
    pub fn new(client: OllamaClient, model: impl Into<String>, dim: usize) -> Self {
        let cap = NonZeroUsize::new(CACHE_CAPACITY).unwrap_or(NonZeroUsize::new(1).unwrap());
        Self {
            client,
            model: model.into(),
            dim,
            cache: Arc::new(Mutex::new(LruCache::new(cap))),
        }
    }

    /// Returns the configured vector dimensionality.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Computes the embedding of `text`. Cached on second call.
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if let Some(v) = self.cache.lock().get(text).cloned() {
            crate::metrics::global().record_embedding_hit();
            return Ok(v);
        }

        crate::metrics::global().record_embedding_miss();
        let req = EmbedRequest {
            model: &self.model,
            prompt: text,
        };
        let url = format!("{}/api/embeddings", self.client.base_url());
        let resp: EmbedResponse = self
            .client
            .http()
            .post(&url)
            .json(&req)
            .send()
            .await
            .context("sending embedding request to ollama")?
            .error_for_status()
            .context("ollama returned non-2xx for embedding")?
            .json()
            .await
            .context("parsing ollama embedding response")?;

        if resp.embedding.len() != self.dim {
            return Err(anyhow!(
                "embedding dim mismatch: expected {}, got {}",
                self.dim,
                resp.embedding.len()
            ));
        }
        debug!(target: "nine_snake.embedder", dim = self.dim, "embedded text");

        // LRU insertion — evicts the least-recently-used entry when the
        // cache is full.
        self.cache
            .lock()
            .put(text.to_string(), resp.embedding.clone());
        Ok(resp.embedding)
    }

    /// Computes embeddings for a batch of texts sequentially. Sequential
    /// (rather than concurrent) to avoid overloading a local Ollama
    /// server.
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            out.push(self.embed(t).await?);
        }
        Ok(out)
    }

    /// Cosine similarity between two equal-length vectors. Returns 0 if
    /// either is the zero vector.
    pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }
        let (mut dot, mut na, mut nb) = (0.0_f64, 0.0_f64, 0.0_f64);
        for (x, y) in a.iter().zip(b.iter()) {
            let (x, y) = (*x as f64, *y as f64);
            dot += x * y;
            na += x * x;
            nb += y * y;
        }
        let denom = (na * nb).sqrt();
        if denom == 0.0 {
            0.0
        } else {
            (dot / denom) as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_is_one() {
        let v = vec![1.0, 2.0, 3.0];
        assert!((Embedder::cosine(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(Embedder::cosine(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_zero_vector_is_zero() {
        let a = vec![0.0, 0.0];
        let b = vec![1.0, 1.0];
        assert_eq!(Embedder::cosine(&a, &b), 0.0);
    }

    #[test]
    fn cosine_mismatched_lengths_is_zero() {
        let a = vec![1.0, 2.0];
        let b = vec![1.0, 2.0, 3.0];
        assert_eq!(Embedder::cosine(&a, &b), 0.0);
    }

    #[test]
    fn lru_cache_evicts_oldest() {
        // Test the LRU behaviour directly without touching the network.
        use lru::LruCache;
        use std::num::NonZeroUsize;
        let mut cache: LruCache<String, Vec<f32>> = LruCache::new(NonZeroUsize::new(2).unwrap());
        cache.put("a".to_string(), vec![1.0]);
        cache.put("b".to_string(), vec![2.0]);
        // Touch "a" so it becomes most-recently used.
        let _ = cache.get(&"a".to_string());
        cache.put("c".to_string(), vec![3.0]);
        assert!(cache.get(&"a".to_string()).is_some());
        assert!(
            cache.get(&"b".to_string()).is_none(),
            "b should have been evicted"
        );
        assert!(cache.get(&"c".to_string()).is_some());
    }
}
