use std::collections::{HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use super::sqlite_store::SqliteStore;
use super::types::RelationKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphSearchResult {
    pub memory_id: String,
    pub hops: u32,
    pub path: Vec<String>,
    pub relation_kinds: Vec<RelationKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphSearchConfig {
    pub max_hops: u32,
    pub max_results: usize,
    pub max_branch_per_level: usize,
    pub relation_filter: Option<Vec<RelationKind>>,
}

impl Default for GraphSearchConfig {
    fn default() -> Self {
        Self {
            max_hops: 3,
            max_results: 50,
            max_branch_per_level: 100,
            relation_filter: None,
        }
    }
}

pub struct GraphSearchEngine {
    store: SqliteStore,
}

impl GraphSearchEngine {
    pub fn new(store: SqliteStore) -> Self {
        Self { store }
    }

    pub fn traverse(
        &self,
        seed_ids: &[String],
        config: &GraphSearchConfig,
    ) -> Vec<GraphSearchResult> {
        let mut results: Vec<GraphSearchResult> = Vec::new();
        let mut visited: HashSet<String> = seed_ids.iter().cloned().collect();
        let mut queue: VecDeque<(String, u32, Vec<String>, Vec<RelationKind>)> = VecDeque::new();

        for id in seed_ids {
            queue.push_back((id.clone(), 0, vec![id.clone()], Vec::new()));
        }

        while let Some((current_id, hops, path, rel_kinds)) = queue.pop_front() {
            if hops > 0 {
                results.push(GraphSearchResult {
                    memory_id: current_id.clone(),
                    hops,
                    path: path.clone(),
                    relation_kinds: rel_kinds.clone(),
                });
                if results.len() >= config.max_results {
                    break;
                }
            }

            if hops >= config.max_hops {
                continue;
            }

            let relations = match self.store.get_relations(&current_id) {
                Ok(r) => r,
                Err(_) => continue,
            };

            let mut branch_count = 0usize;
            for rel in relations.iter() {
                if branch_count >= config.max_branch_per_level {
                    break;
                }

                if let Some(ref filter) = config.relation_filter {
                    if !filter.contains(&rel.kind) {
                        continue;
                    }
                }

                let neighbor_id = if rel.src_id == current_id {
                    &rel.dst_id
                } else {
                    &rel.src_id
                };

                if visited.contains(neighbor_id) {
                    continue;
                }

                visited.insert(neighbor_id.clone());

                let mut new_path = path.clone();
                new_path.push(neighbor_id.clone());
                let mut new_kinds = rel_kinds.clone();
                new_kinds.push(rel.kind);

                queue.push_back((neighbor_id.clone(), hops + 1, new_path, new_kinds));
                branch_count += 1;
            }
        }

        results.truncate(config.max_results);
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let cfg = GraphSearchConfig::default();
        assert_eq!(cfg.max_hops, 3);
        assert_eq!(cfg.max_results, 50);
        assert_eq!(cfg.max_branch_per_level, 100);
        assert!(cfg.relation_filter.is_none());
    }
}
