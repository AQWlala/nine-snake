use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrdtVersion {
    pub memory_id: String,
    pub version: u64,
    pub device_id: String,
    pub timestamp: i64,
    pub field_changes: Vec<FieldChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldChange {
    pub field: String,
    pub old_value: serde_json::Value,
    pub new_value: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrdtMergeResult {
    pub winner: CrdtVersion,
    pub loser: Option<CrdtVersion>,
    pub merged_fields: Vec<FieldChange>,
}

pub struct CrdtEngine;

impl Default for CrdtEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl CrdtEngine {
    pub fn new() -> Self {
        Self
    }

    pub fn merge_lww(&self, local: &CrdtVersion, remote: &CrdtVersion) -> CrdtMergeResult {
        match remote.timestamp.cmp(&local.timestamp) {
            Ordering::Greater => CrdtMergeResult {
                winner: remote.clone(),
                loser: Some(local.clone()),
                merged_fields: remote.field_changes.clone(),
            },
            Ordering::Less => CrdtMergeResult {
                winner: local.clone(),
                loser: Some(remote.clone()),
                merged_fields: local.field_changes.clone(),
            },
            Ordering::Equal => {
                let winner = if remote.device_id > local.device_id {
                    remote
                } else {
                    local
                };
                let loser = if remote.device_id > local.device_id {
                    local
                } else {
                    remote
                };
                CrdtMergeResult {
                    winner: winner.clone(),
                    loser: Some(loser.clone()),
                    merged_fields: winner.field_changes.clone(),
                }
            }
        }
    }

    pub fn merge_fields(&self, local: &CrdtVersion, remote: &CrdtVersion) -> CrdtMergeResult {
        let local_fields: std::collections::HashMap<&str, &FieldChange> = local
            .field_changes
            .iter()
            .map(|fc| (fc.field.as_str(), fc))
            .collect();

        let remote_fields: std::collections::HashMap<&str, &FieldChange> = remote
            .field_changes
            .iter()
            .map(|fc| (fc.field.as_str(), fc))
            .collect();

        let mut merged: Vec<FieldChange> = Vec::new();
        let mut all_keys: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for k in local_fields.keys() {
            all_keys.insert(k);
        }
        for k in remote_fields.keys() {
            all_keys.insert(k);
        }

        for key in all_keys {
            match (local_fields.get(key), remote_fields.get(key)) {
                (Some(local_fc), Some(remote_fc)) => {
                    if remote_fc.new_value == local_fc.new_value {
                        merged.push((*local_fc).clone());
                    } else {
                        match remote.timestamp.cmp(&local.timestamp) {
                            Ordering::Greater => merged.push((*remote_fc).clone()),
                            Ordering::Less => merged.push((*local_fc).clone()),
                            Ordering::Equal => {
                                if remote.device_id > local.device_id {
                                    merged.push((*remote_fc).clone());
                                } else {
                                    merged.push((*local_fc).clone());
                                }
                            }
                        }
                    }
                }
                (Some(local_fc), None) => {
                    merged.push((*local_fc).clone());
                }
                (None, Some(remote_fc)) => {
                    merged.push((*remote_fc).clone());
                }
                (None, None) => {}
            }
        }

        let winner_version = local.version.max(remote.version) + 1;
        let winner_ts = local.timestamp.max(remote.timestamp);
        let winner_device = if remote.timestamp > local.timestamp {
            remote.device_id.clone()
        } else {
            local.device_id.clone()
        };

        CrdtMergeResult {
            winner: CrdtVersion {
                memory_id: local.memory_id.clone(),
                version: winner_version,
                device_id: winner_device,
                timestamp: winner_ts,
                field_changes: merged,
            },
            loser: None,
            merged_fields: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_version(
        memory_id: &str,
        version: u64,
        device_id: &str,
        ts: i64,
        fields: Vec<FieldChange>,
    ) -> CrdtVersion {
        CrdtVersion {
            memory_id: memory_id.to_string(),
            version,
            device_id: device_id.to_string(),
            timestamp: ts,
            field_changes: fields,
        }
    }

    #[test]
    fn lww_newer_timestamp_wins() {
        let local = make_version("m1", 1, "dev-a", 100, vec![]);
        let remote = make_version("m1", 1, "dev-b", 200, vec![]);
        let result = CrdtEngine::new().merge_lww(&local, &remote);
        assert_eq!(result.winner.device_id, "dev-b");
        assert!(result.loser.is_some());
    }

    #[test]
    fn lww_tie_breaker_uses_device_id() {
        let local = make_version("m1", 1, "dev-a", 100, vec![]);
        let remote = make_version("m1", 1, "dev-b", 100, vec![]);
        let result = CrdtEngine::new().merge_lww(&local, &remote);
        assert_eq!(result.winner.device_id, "dev-b");
    }

    #[test]
    fn field_level_merge_keeps_both_sides() {
        let local = make_version(
            "m1",
            1,
            "dev-a",
            100,
            vec![FieldChange {
                field: "content".into(),
                old_value: json!("old"),
                new_value: json!("local-content"),
            }],
        );
        let remote = make_version(
            "m1",
            1,
            "dev-b",
            100,
            vec![FieldChange {
                field: "importance".into(),
                old_value: json!(0.5),
                new_value: json!(0.9),
            }],
        );
        let result = CrdtEngine::new().merge_fields(&local, &remote);
        assert_eq!(result.winner.field_changes.len(), 2);
    }

    #[test]
    fn field_level_merge_same_field_uses_lww() {
        let local = make_version(
            "m1",
            1,
            "dev-a",
            100,
            vec![FieldChange {
                field: "content".into(),
                old_value: json!("old"),
                new_value: json!("local"),
            }],
        );
        let remote = make_version(
            "m1",
            1,
            "dev-b",
            200,
            vec![FieldChange {
                field: "content".into(),
                old_value: json!("old"),
                new_value: json!("remote"),
            }],
        );
        let result = CrdtEngine::new().merge_fields(&local, &remote);
        assert_eq!(result.winner.field_changes.len(), 1);
        assert_eq!(result.winner.field_changes[0].new_value, json!("remote"));
    }
}
