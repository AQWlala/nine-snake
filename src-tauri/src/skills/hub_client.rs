use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::security::SsrfGuard;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubSkillSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub author: String,
    pub rating: f32,
    pub downloads: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubSkillDetail {
    pub id: String,
    pub name: String,
    pub description: String,
    pub code: String,
    pub language: String,
    pub author: String,
    pub tags: Vec<String>,
}

pub struct TeamSkillsHubClient {
    client: Client,
    base_url: String,
}

impl TeamSkillsHubClient {
    pub fn new(base_url: &str) -> Self {
        let guard = SsrfGuard::new();
        guard
            .validate_url(base_url)
            .expect("TeamSkillsHub URL failed SSRF validation");
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<HubSkillSummary>> {
        let url = format!(
            "{}/api/skills/search?q={}&limit={}",
            self.base_url, query, limit
        );
        let guard = SsrfGuard::new();
        guard.validate_url(&url)?;
        info!(target: "nine_snake.hub", query = %query, "searching TeamSkillsHub");
        let resp = self.client.get(&url).send().await?;
        let skills: Vec<HubSkillSummary> = resp.json().await?;
        Ok(skills)
    }

    pub async fn get_skill(&self, skill_id: &str) -> Result<HubSkillDetail> {
        let url = format!("{}/api/skills/{}", self.base_url, skill_id);
        let guard = SsrfGuard::new();
        guard.validate_url(&url)?;
        let resp = self.client.get(&url).send().await?;
        let detail: HubSkillDetail = resp.json().await?;
        Ok(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_constructs_with_valid_url() {
        let _client = TeamSkillsHubClient::new("https://hub.example.com");
    }
}
