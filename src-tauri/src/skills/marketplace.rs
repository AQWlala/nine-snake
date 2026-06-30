//! v1.3 P2-7: 技能市场 — 搜索/安装/更新/发布基础设施。
//!
//! 与 `SkillStore`（CRUD 持久化）和 `SkillImporter`（外部导入）配合，
//! 提供：索引构建、全文搜索、一键安装、更新检查、发布协议。

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::skills::importer::{ImportResult, SkillImporter};
use crate::skills::store::SkillStore;

// ---------------------------------------------------------------------------
// 数据模型
// ---------------------------------------------------------------------------

/// 技能市场元数据 — 统一本地和远程技能信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
    pub version: String,
    pub author: String,
    pub rating: f32,
    pub rating_count: u32,
    pub install_count: u32,
    pub icon_url: Option<String>,
    pub source: String,
    pub import_identifier: Option<String>,
    pub installed: bool,
    pub installed_version: Option<String>,
    pub update_available: bool,
    pub size_bytes: u64,
    pub updated_at: i64,
}

/// 搜索查询参数。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MarketplaceQuery {
    pub text: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub available_only: bool,
    #[serde(default)]
    pub installed_only: bool,
    #[serde(default)]
    pub updates_only: bool,
    #[serde(default)]
    pub min_rating: f32,
    #[serde(default)]
    pub sort: SortBy,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    20
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SortBy {
    #[default]
    Relevance,
    Name,
    Rating,
    Installs,
    UpdatedAt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub entry: SkillEntry,
    pub relevance: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceResponse {
    pub results: Vec<SearchHit>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceStats {
    pub total_available: usize,
    pub total_installed: usize,
    pub updates_available: usize,
    pub by_source: HashMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub skill_id: String,
    pub name: String,
    pub current_version: String,
    pub latest_version: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishManifest {
    pub manifest_version: String,
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub tags: Vec<String>,
    pub source_url: Option<String>,
    pub dependencies: Vec<String>,
    pub min_nine_snake_version: Option<String>,
    pub extra: HashMap<String, serde_json::Value>,
}

impl Default for PublishManifest {
    fn default() -> Self {
        Self {
            manifest_version: "1.0".into(),
            id: String::new(),
            name: String::new(),
            version: "0.1.0".into(),
            description: String::new(),
            author: String::new(),
            tags: vec![],
            source_url: None,
            dependencies: vec![],
            min_nine_snake_version: None,
            extra: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// 倒排索引
// ---------------------------------------------------------------------------

struct InvertedIndex {
    inverted: HashMap<String, Vec<(usize, f32)>>,
    entries: Vec<SkillEntry>,
    avg_doc_len: f32,
}

impl InvertedIndex {
    fn new() -> Self {
        Self {
            inverted: HashMap::new(),
            entries: Vec::new(),
            avg_doc_len: 0.0,
        }
    }

    fn build(&mut self, entries: Vec<SkillEntry>) {
        self.entries = entries;
        self.inverted.clear();
        let mut total_len = 0usize;
        for (idx, entry) in self.entries.iter().enumerate() {
            let tokens = tokenize(&entry.name, &entry.description, &entry.tags);
            total_len += tokens.len();
            let tf = term_frequencies(&tokens);
            for (tok, freq) in tf {
                self.inverted.entry(tok).or_default().push((idx, freq));
            }
        }
        self.avg_doc_len = if self.entries.is_empty() {
            0.0
        } else {
            total_len as f32 / self.entries.len() as f32
        };
    }

    fn search(&self, query: &str, top_k: usize) -> Vec<(usize, f32)> {
        let query_tokens = tokenize_simple(query);
        if query_tokens.is_empty() || self.entries.is_empty() {
            return Vec::new();
        }
        let n_docs = self.entries.len() as f32;
        let mut scores: Vec<f32> = vec![0.0; n_docs as usize];
        for qtok in &query_tokens {
            if let Some(postings) = self.inverted.get(qtok) {
                let idf = ((n_docs - postings.len() as f32 + 0.5) / (postings.len() as f32 + 0.5)
                    + 1.0)
                    .ln();
                for &(idx, tf) in postings {
                    scores[idx] += tf * idf;
                }
            }
        }
        let max_score = scores.iter().cloned().fold(0.0f32, f32::max);
        if max_score > 0.0 {
            for s in &mut scores {
                *s /= max_score;
            }
        }
        let mut ranked: Vec<(usize, f32)> = scores
            .into_iter()
            .enumerate()
            .filter(|(_, s)| *s > 0.0)
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(top_k);
        ranked
    }
}

fn tokenize(name: &str, description: &str, tags: &[String]) -> Vec<String> {
    let text = format!("{name} {description} {}", tags.join(" "));
    let mut tokens: Vec<String> = text
        .to_lowercase()
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '.' || c == ':')
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_'))
        .filter(|t| !t.is_empty() && t.len() >= 2)
        .map(|t| t.to_string())
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

fn tokenize_simple(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = text
        .to_lowercase()
        .split(char::is_whitespace)
        .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_string())
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

fn term_frequencies(tokens: &[String]) -> Vec<(String, f32)> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for t in tokens {
        *counts.entry(t.as_str()).or_default() += 1;
    }
    let total = tokens.len() as f32;
    counts
        .into_iter()
        .map(|(t, c)| (t.to_string(), c as f32 / total))
        .collect()
}

// ---------------------------------------------------------------------------
// SkillMarketplace
// ---------------------------------------------------------------------------

pub struct SkillMarketplace {
    store: Arc<SkillStore>,
    importer: Arc<SkillImporter>,
    index: std::sync::RwLock<InvertedIndex>,
    entries: std::sync::RwLock<Vec<SkillEntry>>,
}

impl SkillMarketplace {
    pub fn new(store: Arc<SkillStore>, importer: Arc<SkillImporter>) -> Self {
        Self {
            store,
            importer,
            index: std::sync::RwLock::new(InvertedIndex::new()),
            entries: std::sync::RwLock::new(Vec::new()),
        }
    }

    /// Build/refresh the index from the local SkillStore.
    pub fn refresh(&self) -> Result<MarketplaceStats, anyhow::Error> {
        let local_skills = self.store.list(None, None, 1000)?;
        let mut entries: Vec<SkillEntry> = Vec::new();
        let installed_ids: HashSet<String> = local_skills.iter().map(|s| s.id.clone()).collect();

        for s in &local_skills {
            entries.push(SkillEntry {
                id: s.id.clone(),
                name: s.name.clone(),
                description: s.description.clone(),
                tags: s.tags.clone(),
                version: "1.0.0".into(),
                author: "local".into(),
                rating: s.avg_rating,
                rating_count: s.rating_count,
                install_count: s.usage_count,
                icon_url: None,
                source: "local".into(),
                import_identifier: None,
                installed: true,
                installed_version: Some("1.0.0".into()),
                update_available: false,
                size_bytes: s.code.len() as u64,
                updated_at: s.updated_at,
            });
        }

        let mut index = InvertedIndex::new();
        index.build(entries.clone());
        *self.index.write().unwrap() = index;
        *self.entries.write().unwrap() = entries.clone();

        let installed_count = installed_ids.len();
        let mut by_source = HashMap::new();
        by_source.insert("local".into(), entries.len());

        Ok(MarketplaceStats {
            total_available: entries.len(),
            total_installed: installed_count,
            updates_available: 0,
            by_source,
        })
    }

    /// Full-text search with filters and pagination.
    pub fn search(&self, query: &MarketplaceQuery) -> Result<MarketplaceResponse, anyhow::Error> {
        let index = self.index.read().unwrap();
        let entries = self.entries.read().unwrap();

        let mut candidates: Vec<(usize, f32)> = if let Some(ref text) = query.text {
            let hits = index.search(text, 200);
            if hits.is_empty() {
                entries.iter().enumerate().map(|(i, _)| (i, 0.0)).collect()
            } else {
                hits
            }
        } else {
            entries.iter().enumerate().map(|(i, _)| (i, 0.0)).collect()
        };

        candidates.retain(|(idx, _)| {
            let e = &entries[*idx];
            if !query.tags.is_empty()
                && !query
                    .tags
                    .iter()
                    .all(|t| e.tags.iter().any(|et| et.eq_ignore_ascii_case(t)))
            {
                return false;
            }
            if !query.sources.is_empty()
                && !query
                    .sources
                    .iter()
                    .any(|s| e.source.eq_ignore_ascii_case(s))
            {
                return false;
            }
            if query.available_only && e.installed {
                return false;
            }
            if query.installed_only && !e.installed {
                return false;
            }
            if query.updates_only && !e.update_available {
                return false;
            }
            if e.rating < query.min_rating {
                return false;
            }
            true
        });

        match query.sort {
            SortBy::Relevance => candidates
                .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)),
            SortBy::Name => candidates.sort_by(|a, b| entries[a.0].name.cmp(&entries[b.0].name)),
            SortBy::Rating => candidates.sort_by(|a, b| {
                entries[b.0]
                    .rating
                    .partial_cmp(&entries[a.0].rating)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
            SortBy::Installs => candidates
                .sort_by(|a, b| entries[b.0].install_count.cmp(&entries[a.0].install_count)),
            SortBy::UpdatedAt => {
                candidates.sort_by(|a, b| entries[b.0].updated_at.cmp(&entries[a.0].updated_at))
            }
        }

        let total = candidates.len();
        let page: Vec<SearchHit> = candidates
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
            .map(|(idx, rel)| SearchHit {
                entry: entries[idx].clone(),
                relevance: rel,
            })
            .collect();

        Ok(MarketplaceResponse {
            results: page,
            total,
            offset: query.offset,
            limit: query.limit,
        })
    }

    /// One-click install from a remote source.
    pub fn install(&self, source: &str, _identifier: &str) -> Result<SkillEntry, anyhow::Error> {
        // Delegate to SkillImporter based on source type.
        let result: ImportResult = match source {
            "agentskills" => {
                // For URL-based import, identifier is the URL
                // We use spawn_blocking since import_from_url is async
                let url = _identifier.to_string();
                let importer = self.importer.clone();
                tokio::runtime::Handle::current()
                    .block_on(async move { importer.import_from_url(&url).await })
            }
            "clawhub" => {
                let slug = _identifier.to_string();
                let importer = self.importer.clone();
                tokio::runtime::Handle::current()
                    .block_on(async move { importer.import_from_clawhub(&slug).await })
            }
            "teamskillshub" => {
                let asset_id = _identifier.to_string();
                let importer = self.importer.clone();
                tokio::runtime::Handle::current()
                    .block_on(async move { importer.import_from_teamskillshub(&asset_id).await })
            }
            other => anyhow::bail!("unknown source: {other}"),
        };

        // Refresh index after install.
        self.refresh()?;

        let entries = self.entries.read().unwrap();
        let skill_id = result
            .skill
            .as_ref()
            .map(|s| s.id.clone())
            .unwrap_or_default();
        let skill_name = result
            .skill
            .as_ref()
            .map(|s| s.name.clone())
            .unwrap_or_default();
        let skill_tags = result
            .skill
            .as_ref()
            .map(|s| s.tags.clone())
            .unwrap_or_default();

        let e = entries
            .iter()
            .find(|e| e.id == skill_id)
            .cloned()
            .unwrap_or_else(|| SkillEntry {
                id: skill_id,
                name: skill_name,
                description: String::new(),
                tags: skill_tags,
                version: "1.0.0".into(),
                author: source.into(),
                rating: 0.0,
                rating_count: 0,
                install_count: 1,
                icon_url: None,
                source: source.into(),
                import_identifier: Some(_identifier.into()),
                installed: true,
                installed_version: Some("1.0.0".into()),
                update_available: false,
                size_bytes: 0,
                updated_at: 0,
            });
        Ok(e)
    }

    pub fn all_tags(&self) -> Vec<(String, usize)> {
        let entries = self.entries.read().unwrap();
        let mut counts: HashMap<String, usize> = HashMap::new();
        for e in entries.iter() {
            for t in &e.tags {
                *counts.entry(t.clone()).or_default() += 1;
            }
        }
        let mut tags: Vec<_> = counts.into_iter().collect();
        tags.sort_by(|a, b| b.1.cmp(&a.1));
        tags
    }

    pub fn stats(&self) -> MarketplaceStats {
        let entries = self.entries.read().unwrap();
        let installed = entries.iter().filter(|e| e.installed).count();
        let updates = entries.iter().filter(|e| e.update_available).count();
        let mut by_source = HashMap::new();
        for e in entries.iter() {
            *by_source.entry(e.source.clone()).or_default() += 1;
        }
        MarketplaceStats {
            total_available: entries.len(),
            total_installed: installed,
            updates_available: updates,
            by_source,
        }
    }

    pub fn check_updates(&self) -> Vec<UpdateInfo> {
        let entries = self.entries.read().unwrap();
        entries
            .iter()
            .filter(|e| e.update_available)
            .map(|e| UpdateInfo {
                skill_id: e.id.clone(),
                name: e.name.clone(),
                current_version: e.installed_version.clone().unwrap_or_default(),
                latest_version: e.version.clone(),
                source: e.source.clone(),
            })
            .collect()
    }

    pub fn generate_manifest(&self, skill_id: &str) -> Result<PublishManifest, anyhow::Error> {
        let skill = self
            .store
            .get(skill_id)?
            .ok_or_else(|| anyhow::anyhow!("skill not found: {skill_id}"))?;
        Ok(PublishManifest {
            manifest_version: "1.0".into(),
            id: skill.id,
            name: skill.name,
            version: "0.1.0".into(),
            description: skill.description,
            author: String::new(),
            tags: skill.tags,
            source_url: None,
            dependencies: vec![],
            min_nine_snake_version: Some("1.3.0".into()),
            extra: HashMap::new(),
        })
    }

    pub fn validate_manifest(manifest: &PublishManifest) -> Result<(), anyhow::Error> {
        if manifest.id.is_empty() {
            anyhow::bail!("id is required");
        }
        if manifest.name.is_empty() {
            anyhow::bail!("name is required");
        }
        if manifest.version.is_empty() {
            anyhow::bail!("version is required");
        }
        if manifest.description.is_empty() {
            anyhow::bail!("description is required");
        }
        let parts: Vec<&str> = manifest.version.split('.').collect();
        if parts.len() != 3 {
            anyhow::bail!("version must be semver (X.Y.Z)");
        }
        for p in parts {
            p.parse::<u32>()
                .map_err(|_| anyhow::anyhow!("version segment '{p}' is not a number"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_basic() {
        let tokens = tokenize(
            "Rust Skill",
            "A rust formatter",
            &["rust".into(), "tools".into()],
        );
        assert!(tokens.iter().any(|t| t == "rust"));
    }

    #[test]
    fn search_finds_entries() {
        let entries = vec![
            SkillEntry {
                id: "a".into(),
                name: "Python Formatter".into(),
                description: "Format Python code".into(),
                tags: vec!["python".into()],
                version: "1.0.0".into(),
                author: "test".into(),
                rating: 4.0,
                rating_count: 10,
                install_count: 100,
                icon_url: None,
                source: "local".into(),
                import_identifier: None,
                installed: true,
                installed_version: Some("1.0.0".into()),
                update_available: false,
                size_bytes: 0,
                updated_at: 0,
            },
            SkillEntry {
                id: "b".into(),
                name: "Rust Formatter".into(),
                description: "Format Rust code".into(),
                tags: vec!["rust".into()],
                version: "1.0.0".into(),
                author: "test".into(),
                rating: 0.0,
                rating_count: 0,
                install_count: 0,
                icon_url: None,
                source: "local".into(),
                import_identifier: None,
                installed: false,
                installed_version: None,
                update_available: false,
                size_bytes: 0,
                updated_at: 0,
            },
        ];
        let mut idx = InvertedIndex::new();
        idx.build(entries);
        let hits = idx.search("Rust", 10);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn validate_manifest_ok() {
        let m = PublishManifest {
            id: "my-skill".into(),
            name: "My Skill".into(),
            version: "1.2.3".into(),
            description: "A test skill".into(),
            ..Default::default()
        };
        assert!(SkillMarketplace::validate_manifest(&m).is_ok());
    }

    #[test]
    fn validate_manifest_bad_version() {
        let m = PublishManifest {
            id: "my-skill".into(),
            name: "My Skill".into(),
            version: "latest".into(),
            description: "A test skill".into(),
            ..Default::default()
        };
        assert!(SkillMarketplace::validate_manifest(&m).is_err());
    }
}
