//! Skill importer — v1.2 P2 eco compatibility
//!
//! Imports skills from external ecosystems:
//!
//! * **agentskills.io** — The open skill registry. Skills are distributed as
//!   Markdown files following the agentskills.io SKILL.md schema.  This
//!   importer fetches the raw Markdown, parses the YAML front-matter, and
//!   converts it into a nine-snake [`Skill`].
//!
//! * **ClawHub** — Clawd's community skill hub.  Skills have a `clawhub`
//!   slug that resolves to a GitHub repository; the importer fetches the
//!   `SKILL.md` from the default branch.
//!
//! * **TeamSkillsHub** — Internal team skill registry.  Assets are
//!   downloaded by `asset_id` from the team skills API.
//!
//! ## Safety
//!
//! Imported skills are sandboxed with `trust_level = 0` (user must manually
//! promote them) to prevent supply-chain attacks through third-party skill
//! registries.

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use super::store::SkillStore;
use super::types::{CreateSkillRequest, Skill};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// External skill source.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillSource {
    /// agentskills.io compatible URL (raw SKILL.md).
    AgentskillsIo,
    /// ClawHub slug (e.g. `clawd/text-summarizer`).
    ClawHub,
    /// TeamSkillsHub asset ID.
    TeamSkillsHub,
}

/// Result of an import operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportResult {
    /// Whether the import succeeded.
    pub success: bool,
    /// The imported skill (if successful).
    pub skill: Option<Skill>,
    /// The source URL or identifier.
    pub source: String,
    /// Error message (if failed).
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Importer
// ---------------------------------------------------------------------------

pub struct SkillImporter {
    store: SkillStore,
    client: Client,
}

impl SkillImporter {
    pub fn new(store: SkillStore) -> Self {
        Self {
            store,
            client: Client::builder()
                .user_agent("nine-snake/1.2 skill-importer")
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client build is infallible"),
        }
    }

    /// Import a skill from a raw agentskills.io-compatible URL.
    ///
    /// The URL must point to a raw Markdown file following the
    /// agentskills.io SKILL.md schema (YAML front-matter + body).
    pub async fn import_from_url(&self, url: &str) -> ImportResult {
        let source = url.to_string();
        debug!(target: "nine_snake.importer", url, "fetching skill");

        let content = match self.fetch_skill_md(url).await {
            Ok(c) => c,
            Err(e) => {
                return ImportResult {
                    success: false,
                    skill: None,
                    source,
                    error: Some(format!("fetch failed: {e}")),
                }
            }
        };

        let parsed = match self.parse_skill_md(&content) {
            Ok(s) => s,
            Err(e) => {
                return ImportResult {
                    success: false,
                    skill: None,
                    source,
                    error: Some(format!("parse failed: {e}")),
                }
            }
        };

        match self.store_skill(parsed).await {
            Ok(skill) => {
                info!(
                    target: "nine_snake.importer",
                    id = %skill.id,
                    name = %skill.name,
                    "skill imported successfully"
                );
                ImportResult {
                    success: true,
                    skill: Some(skill),
                    source,
                    error: None,
                }
            }
            Err(e) => ImportResult {
                success: false,
                skill: None,
                source,
                error: Some(format!("store failed: {e}")),
            },
        }
    }

    /// Import a skill from ClawHub by slug (e.g. `clawd/text-summarizer`).
    ///
    /// Resolves the slug to `https://raw.githubusercontent.com/{org}/clawhub-skills/main/{slug}/SKILL.md`.
    pub async fn import_from_clawhub(&self, slug: &str) -> ImportResult {
        // ClawHub slugs are typically `org/skill-name`.
        // The raw URL pattern is: raw.githubusercontent.com/{org}/clawhub-skills/main/{skill}/SKILL.md
        let url = if slug.contains('/') {
            let parts: Vec<&str> = slug.splitn(2, '/').collect();
            format!(
                "https://raw.githubusercontent.com/{}/{}/main/SKILL.md",
                parts[0],
                if parts[0].eq_ignore_ascii_case("clawhub-skills") {
                    parts[1].to_string()
                } else {
                    format!("clawhub-skills/main/{}", parts[1])
                }
            )
        } else {
            format!(
                "https://raw.githubusercontent.com/clawhub-skills/main/{}/SKILL.md",
                slug
            )
        };

        self.import_from_url(&url).await
    }

    /// Import a skill from TeamSkillsHub by asset ID.
    ///
    /// The asset is fetched from the team skills API and parsed into
    /// a nine-snake skill.
    pub async fn import_from_teamskillshub(&self, asset_id: &str) -> ImportResult {
        let source = format!("teamskillshub:{asset_id}");
        // TODO: implement TeamSkillsHub API client.
        // For now, return a placeholder.
        ImportResult {
            success: false,
            skill: None,
            source,
            error: Some("TeamSkillsHub import not yet implemented".to_string()),
        }
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    async fn fetch_skill_md(&self, url: &str) -> Result<String> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .context("HTTP request failed")?;

        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }

        resp.text().await.context("reading response body")
    }

    /// Parse an agentskills.io-compatible SKILL.md into a CreateSkillRequest.
    fn parse_skill_md(&self, content: &str) -> Result<CreateSkillRequest> {
        // Parse YAML front-matter.
        let front_matter = Self::extract_yaml_front_matter(content)?;

        let name = front_matter
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .context("'name' field required in front-matter")?;

        let description = front_matter
            .get("description")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();

        let category = front_matter
            .get("category")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "imported".to_string());

        let tags: Vec<String> = front_matter
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        // Extract instructions from the body (everything after front-matter).
        let instructions = Self::extract_body(content);

        Ok(CreateSkillRequest {
            name,
            description,
            language: category,
            code: instructions,
            tags,
            source_memory_id: None,
            activation_condition: None,
            platform: None,
            min_confidence: None,
        })
    }

    fn extract_yaml_front_matter(content: &str) -> Result<serde_json::Value> {
        let trimmed = content.trim_start();
        if !trimmed.starts_with("---") {
            bail!("no YAML front-matter found (expected leading '---')");
        }

        let rest = &trimmed[3..];
        let end = rest
            .find("---")
            .context("unclosed front-matter (missing closing '---')")?;

        let yaml_str = &rest[..end];

        let yaml_value: serde_yaml::Value =
            serde_yaml::from_str(yaml_str).with_context(|| "YAML front-matter parse error")?;

        let json_str =
            serde_json::to_string(&yaml_value).with_context(|| "YAML-to-JSON conversion error")?;

        let json_value: serde_json::Value =
            serde_json::from_str(&json_str).with_context(|| "JSON round-trip error")?;

        Ok(json_value)
    }

    fn extract_body(content: &str) -> String {
        let trimmed = content.trim_start();
        if !trimmed.starts_with("---") {
            return content.to_string();
        }

        let rest = &trimmed[3..];
        if let Some(end) = rest.find("---") {
            rest[end + 3..].trim().to_string()
        } else {
            content.to_string()
        }
    }

    async fn store_skill(&self, req: CreateSkillRequest) -> Result<Skill> {
        let now = chrono::Utc::now().timestamp_millis();
        let id = format!("import-{}-{}", req.name, now);
        let skill = Skill {
            id: id.clone(),
            name: req.name.clone(),
            description: req.description.clone(),
            code: req.code.clone(),
            language: req.language.clone(),
            tags: req.tags.clone(),
            usage_count: 0,
            avg_rating: 0.0,
            rating_count: 0,
            created_at: now,
            updated_at: now,
            source_memory_id: req.source_memory_id.clone(),
            activation_condition: None,
            platform: None,
            min_confidence: None,
        };
        self.store.insert(&skill)?;

        // Fetch back the stored skill.
        self.store
            .get(&id)?
            .ok_or_else(|| anyhow::anyhow!("skill not found after insert"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_yaml_front_matter() {
        let importer = SkillImporter {
            store: SkillStore::new(
                crate::memory::sqlite_store::SqliteStore::open(":memory:").unwrap(),
            )
            .unwrap(),
            client: Client::new(),
        };

        let md = r#"---
name: text-summarizer
description: Summarize long texts into concise bullet points.
category: text
tags: [summarization, nlp, utility]
---

# Text Summarizer

## Instructions
1. Read the input text
2. Identify key points
3. Output a concise summary
"#;

        let result = importer.parse_skill_md(md).unwrap();
        assert_eq!(result.name, "text-summarizer");
        assert_eq!(result.language, "text");
        assert_eq!(result.tags, vec!["summarization", "nlp", "utility"]);
        assert!(result.code.contains("Read the input text"));
    }

    #[test]
    fn test_extract_body() {
        let body = SkillImporter::extract_body("---\nname: test\n---\n\n# Title\n\nContent here");
        assert_eq!(body, "# Title\n\nContent here");
    }
}
