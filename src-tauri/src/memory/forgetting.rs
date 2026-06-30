use serde::{Deserialize, Serialize};
use tracing::info;

use super::layers::policy_for;
use super::types::MemoryLayer;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgettingConfig {
    pub importance_threshold: f32,
    pub dry_run: bool,
}

impl Default for ForgettingConfig {
    fn default() -> Self {
        Self {
            importance_threshold: 0.3,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgettingCandidate {
    pub id: String,
    pub layer: MemoryLayer,
    pub importance: f32,
    pub last_access: i64,
    pub ttl_days: u32,
    pub reason: String,
}

pub struct ForgettingEngine {
    config: ForgettingConfig,
}

impl ForgettingEngine {
    pub fn new(config: ForgettingConfig) -> Self {
        Self { config }
    }

    pub fn scan_for_archive(
        &self,
        memories: Vec<(String, MemoryLayer, f32, i64, bool)>,
        now: i64,
    ) -> Vec<ForgettingCandidate> {
        let mut candidates = Vec::new();

        for (id, layer, importance, last_access, pinned) in memories {
            if pinned {
                continue;
            }
            if layer == MemoryLayer::L7 {
                continue;
            }
            if importance > self.config.importance_threshold {
                continue;
            }

            let policy = policy_for(layer);
            if policy.ttl_days == 0 {
                continue;
            }

            let ttl_secs = policy.ttl_days as i64 * 24 * 3600;
            let age = now - last_access;
            if age < ttl_secs {
                continue;
            }

            candidates.push(ForgettingCandidate {
                id,
                layer,
                importance,
                last_access,
                ttl_days: policy.ttl_days,
                reason: format!(
                    "importance={:.2} < threshold={:.2}, age={}d > ttl={}d",
                    importance,
                    self.config.importance_threshold,
                    age / 86400,
                    policy.ttl_days,
                ),
            });
        }

        if !candidates.is_empty() {
            info!(
                target: "nine_snake.forgetting",
                count = candidates.len(),
                dry_run = self.config.dry_run,
                "identified memories for archival"
            );
        }

        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_importance_old_memory_is_candidate() {
        let engine = ForgettingEngine::new(ForgettingConfig::default());
        let now = chrono::Utc::now().timestamp();
        let memories = vec![(
            "mem-1".to_string(),
            MemoryLayer::L1,
            0.1,
            now - 2 * 86400,
            false,
        )];
        let candidates = engine.scan_for_archive(memories, now);
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn pinned_memory_is_not_candidate() {
        let engine = ForgettingEngine::new(ForgettingConfig::default());
        let now = chrono::Utc::now().timestamp();
        let memories = vec![(
            "mem-1".to_string(),
            MemoryLayer::L1,
            0.1,
            now - 2 * 86400,
            true,
        )];
        let candidates = engine.scan_for_archive(memories, now);
        assert!(candidates.is_empty());
    }

    #[test]
    fn l7_is_not_candidate() {
        let engine = ForgettingEngine::new(ForgettingConfig::default());
        let now = chrono::Utc::now().timestamp();
        let memories = vec![(
            "mem-1".to_string(),
            MemoryLayer::L7,
            0.1,
            now - 365 * 86400,
            false,
        )];
        let candidates = engine.scan_for_archive(memories, now);
        assert!(candidates.is_empty());
    }

    #[test]
    fn high_importance_is_not_candidate() {
        let engine = ForgettingEngine::new(ForgettingConfig::default());
        let now = chrono::Utc::now().timestamp();
        let memories = vec![(
            "mem-1".to_string(),
            MemoryLayer::L1,
            0.8,
            now - 2 * 86400,
            false,
        )];
        let candidates = engine.scan_for_archive(memories, now);
        assert!(candidates.is_empty());
    }
}
