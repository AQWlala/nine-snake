use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::sqlite_store::SqliteStore;
use super::types::Memory;
use crate::security::contains_sensitive;

const JSONLD_CONTEXT: &str = "https://schema.org";
const SCHEMA_VERSION: &str = "2.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportManifest {
    pub memory_count: usize,
    pub relation_count: usize,
    pub redacted_count: usize,
    pub exported_at: i64,
    pub schema_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    pub imported: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonLdMemory {
    #[serde(rename = "@context")]
    context: String,
    #[serde(rename = "@id")]
    id: String,
    #[serde(rename = "@type")]
    type_: String,
    content: String,
    layer: String,
    memory_type: String,
    importance: f32,
    source: String,
    created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_50: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_150: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_500: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary_2000: Option<String>,
}

pub struct DataExporter {
    sqlite: SqliteStore,
}

impl DataExporter {
    pub fn new(sqlite: SqliteStore) -> Self {
        Self { sqlite }
    }

    pub async fn export_jsonld(&self, path: &Path) -> Result<ExportManifest> {
        let memories = self.sqlite.list_recent(usize::MAX).await?;
        let mut redacted_count = 0;

        let jsonld_items: Vec<JsonLdMemory> = memories
            .iter()
            .map(|m| {
                let (content, s50, s150, s500, s2000) = if contains_sensitive(&m.content) {
                    redacted_count += 1;
                    (
                        "[REDACTED]".to_string(),
                        Some("[REDACTED]".to_string()),
                        Some("[REDACTED]".to_string()),
                        Some("[REDACTED]".to_string()),
                        Some("[REDACTED]".to_string()),
                    )
                } else {
                    (
                        m.content.clone(),
                        if m.summary.s50.is_empty() {
                            None
                        } else {
                            Some(m.summary.s50.clone())
                        },
                        if m.summary.s150.is_empty() {
                            None
                        } else {
                            Some(m.summary.s150.clone())
                        },
                        if m.summary.s500.is_empty() {
                            None
                        } else {
                            Some(m.summary.s500.clone())
                        },
                        if m.summary.s2000.is_empty() {
                            None
                        } else {
                            Some(m.summary.s2000.clone())
                        },
                    )
                };

                JsonLdMemory {
                    context: JSONLD_CONTEXT.to_string(),
                    id: format!("nine-snake:memory:{}", m.id),
                    type_: "MemoryEntity".to_string(),
                    content,
                    layer: m.layer.as_str().to_string(),
                    memory_type: m.memory_type.as_str().to_string(),
                    importance: m.importance,
                    source: m.source.as_str().to_string(),
                    created_at: m.created_at,
                    summary_50: s50,
                    summary_150: s150,
                    summary_500: s500,
                    summary_2000: s2000,
                }
            })
            .collect();

        let manifest = ExportManifest {
            memory_count: jsonld_items.len(),
            relation_count: 0,
            redacted_count,
            exported_at: chrono::Utc::now().timestamp(),
            schema_version: SCHEMA_VERSION.to_string(),
        };

        let output = serde_json::json!({
            "@context": JSONLD_CONTEXT,
            "@type": "MemoryCollection",
            "schema_version": SCHEMA_VERSION,
            "items": jsonld_items,
            "manifest": manifest,
        });

        let json = serde_json::to_string_pretty(&output).context("serializing JSON-LD export")?;
        std::fs::write(path, json.as_bytes())
            .with_context(|| format!("writing export to {}", path.display()))?;

        info!(
            target: "nine_snake.export",
            count = manifest.memory_count,
            redacted = manifest.redacted_count,
            "JSON-LD export complete"
        );

        Ok(manifest)
    }

    pub async fn import_jsonld(&self, path: &Path) -> Result<ImportResult> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading import file {}", path.display()))?;

        let parsed: serde_json::Value =
            serde_json::from_str(&content).with_context(|| "parsing JSON-LD import file")?;

        let items = parsed
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut imported = 0;
        let mut errors = 0;

        for item in &items {
            let item_type = item.get("@type").and_then(|v| v.as_str()).unwrap_or("");
            if item_type != "MemoryEntity" {
                warn!(target: "nine_snake.export", type_ = item_type, "skipping non-MemoryEntity item");
                errors += 1;
                continue;
            }

            let id = match item.get("@id").and_then(|v| v.as_str()) {
                Some(id) => id.replace("nine-snake:memory:", ""),
                None => {
                    errors += 1;
                    continue;
                }
            };

            let content_val = item
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if content_val == "[REDACTED]" {
                warn!(target: "nine_snake.export", id = %id, "skipping redacted memory");
                errors += 1;
                continue;
            }

            let mem = Memory {
                id,
                memory_type: item
                    .get("memory_type")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(super::types::MemoryType::Semantic),
                layer: item
                    .get("layer")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(super::types::MemoryLayer::L3),
                content: content_val,
                summary: super::types::MultiGranularity {
                    s50: item
                        .get("summary_50")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    s150: item
                        .get("summary_150")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    s500: item
                        .get("summary_500")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    s2000: item
                        .get("summary_2000")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                },
                importance: item
                    .get("importance")
                    .and_then(|v| v.as_f64())
                    .map(|f| f as f32)
                    .unwrap_or(0.5),
                access_count: 0,
                last_access: 0,
                created_at: item.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0),
                source: item
                    .get("source")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(super::types::SourceKind::External),
                metadata: serde_json::Value::Object(serde_json::Map::new()),
                compressed_from: None,
                compression_gen: 0,
                pinned: false,
                archived: false,
                embedding: Vec::new(),
            };

            match self.sqlite.insert_guarded_spawn(&mem).await {
                Ok(()) => imported += 1,
                Err(e) => {
                    warn!(target: "nine_snake.export", id = %mem.id, error = ?e, "import error");
                    errors += 1;
                }
            }
        }

        info!(
            target: "nine_snake.export",
            imported,
            errors,
            "JSON-LD import complete"
        );

        Ok(ImportResult { imported, errors })
    }
}
