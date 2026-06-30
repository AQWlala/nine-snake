//! Skill-related DTOs.
//!
//! These types are the wire shape for both the Tauri command layer and
//! the gRPC SkillService. They map 1:1 onto the gRPC proto messages
//! in `proto/nine_snake.proto`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ActivationCondition {
    #[serde(rename = "keyword")]
    Keyword { pattern: String },
    #[serde(rename = "intent")]
    Intent { category: String },
    #[serde(rename = "context")]
    Context { key: String, value: String },
    #[serde(rename = "always")]
    Always,
}

impl ActivationCondition {
    pub fn matches(
        &self,
        input: &str,
        context: &std::collections::HashMap<String, String>,
    ) -> bool {
        match self {
            ActivationCondition::Always => true,
            ActivationCondition::Keyword { pattern } => {
                input.to_lowercase().contains(&pattern.to_lowercase())
            }
            ActivationCondition::Intent { category } => {
                input.to_lowercase().contains(&category.to_lowercase())
            }
            ActivationCondition::Context { key, value } => {
                context.get(key).map(|v| v == value).unwrap_or(false)
            }
        }
    }
}

/// A skill record. Persisted in the `skills` table (see
/// `migrations/001_initial.sql`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub code: String,
    pub language: String,
    pub tags: Vec<String>,
    pub usage_count: u32,
    pub avg_rating: f32,
    pub rating_count: u32,
    pub created_at: i64,
    pub updated_at: i64,
    pub source_memory_id: Option<String>,
    #[serde(default)]
    pub activation_condition: Option<ActivationCondition>,
    #[serde(default)]
    pub platform: Option<Vec<String>>,
    #[serde(default)]
    pub min_confidence: Option<f32>,
}

/// The output of running a skill. `execution_time_ms` is wall-clock
/// time on the local machine; `tokens_used` is only populated for
/// LLM-driven skills.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillResult {
    pub skill_id: String,
    pub output: String,
    pub execution_time_ms: u64,
    pub tokens_used: u32,
}

// ---------------------------------------------------------------------------
// Request / response envelopes (DTOs that flow over the Tauri boundary).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateSkillRequest {
    pub name: String,
    pub description: String,
    pub code: String,
    pub language: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub source_memory_id: Option<String>,
    #[serde(default)]
    pub activation_condition: Option<ActivationCondition>,
    #[serde(default)]
    pub platform: Option<Vec<String>>,
    #[serde(default)]
    pub min_confidence: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UseSkillRequest {
    pub id: String,
    #[serde(default)]
    pub params: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateSkillRequest {
    pub id: String,
    pub rating: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ListSkillsRequest {
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub tag: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSearchRequest {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_serializes_with_snake_case_keys() {
        let s = Skill {
            id: "x".to_string(),
            name: "n".to_string(),
            description: "d".to_string(),
            code: "c".to_string(),
            language: "rust".to_string(),
            tags: vec!["a".to_string()],
            usage_count: 1,
            avg_rating: 0.5,
            rating_count: 1,
            created_at: 1,
            updated_at: 1,
            source_memory_id: None,
            activation_condition: None,
            platform: None,
            min_confidence: None,
        };
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"avg_rating\":0.5"));
        assert!(j.contains("\"usage_count\":1"));
    }
}
