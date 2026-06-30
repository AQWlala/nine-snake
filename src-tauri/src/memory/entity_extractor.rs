use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::types::RelationKind;
use crate::llm::{ChatMessage, LlmGateway};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedRelation {
    pub from_id: String,
    pub to_id: String,
    pub relation: RelationKind,
    pub evidence: Option<String>,
}

pub struct EntityExtractor {
    llm: LlmGateway,
}

impl EntityExtractor {
    pub fn new(llm: LlmGateway) -> Self {
        Self { llm }
    }

    pub async fn extract(
        &self,
        memory_id: &str,
        content: &str,
        existing_ids: &[String],
    ) -> Result<Vec<ExtractedRelation>> {
        if existing_ids.is_empty() {
            return Ok(Vec::new());
        }

        let ids_list = existing_ids.join(", ");
        let prompt = format!(
            "Analyze the following text and identify relationships between entities.\n\
             Current memory ID: {memory_id}\n\
             Known memory IDs: [{ids_list}]\n\
             Text: {content}\n\n\
             For each relationship found, output a JSON array where each element has:\n\
             - \"to_id\": one of the known memory IDs\n\
             - \"relation\": one of \"causes\", \"supports\", \"contradicts\", \"references\", \"derived_from\"\n\
             - \"evidence\": brief quote from the text supporting this relation\n\n\
             Output ONLY the JSON array, nothing else. If no relationships found, output []."
        );

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: prompt,
        }];

        let response = match self.llm.chat(messages).await {
            Ok(r) => r.message.content,
            Err(e) => {
                warn!(target: "nine_snake.entity_extractor", error = ?e, "LLM call failed; returning empty relations");
                return Ok(Vec::new());
            }
        };

        let relations = self.parse_response(memory_id, &response);

        if !relations.is_empty() {
            info!(
                target: "nine_snake.entity_extractor",
                memory_id,
                count = relations.len(),
                "extracted entity relations"
            );
        }

        Ok(relations)
    }

    fn parse_response(&self, memory_id: &str, response: &str) -> Vec<ExtractedRelation> {
        let trimmed = response.trim();
        let json_str = if trimmed.starts_with("```") {
            trimmed
                .lines()
                .skip(1)
                .take_while(|l| !l.starts_with("```"))
                .collect::<Vec<_>>()
                .join("")
        } else {
            trimmed.to_string()
        };

        #[derive(Deserialize)]
        struct RawRelation {
            to_id: String,
            relation: String,
            evidence: Option<String>,
        }

        let parsed: Vec<RawRelation> = match serde_json::from_str(&json_str) {
            Ok(v) => v,
            Err(e) => {
                warn!(target: "nine_snake.entity_extractor", error = %e, "failed to parse LLM relation output");
                return Vec::new();
            }
        };

        parsed
            .into_iter()
            .filter_map(|r| {
                let relation = match r.relation.as_str() {
                    "causes" => RelationKind::Causes,
                    "supports" => RelationKind::Supports,
                    "contradicts" => RelationKind::Contradicts,
                    "references" => RelationKind::References,
                    "derived_from" => RelationKind::DerivedFrom,
                    _ => return None,
                };
                Some(ExtractedRelation {
                    from_id: memory_id.to_string(),
                    to_id: r.to_id,
                    relation,
                    evidence: r.evidence,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_extractor() -> EntityExtractor {
        EntityExtractor::new(crate::llm::LlmGateway::new_test())
    }

    #[test]
    fn parse_empty_array() {
        let extractor = test_extractor();
        let result = extractor.parse_response("mem-1", "[]");
        assert!(result.is_empty());
    }

    #[test]
    fn parse_valid_relations() {
        let extractor = test_extractor();
        let json = r#"[{"to_id":"mem-2","relation":"causes","evidence":"because of"}]"#;
        let result = extractor.parse_response("mem-1", json);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].from_id, "mem-1");
        assert_eq!(result[0].to_id, "mem-2");
        assert!(matches!(result[0].relation, RelationKind::Causes));
    }

    #[test]
    fn parse_invalid_relation_type_skipped() {
        let extractor = test_extractor();
        let json = r#"[{"to_id":"mem-2","relation":"unknown","evidence":"test"}]"#;
        let result = extractor.parse_response("mem-1", json);
        assert!(result.is_empty());
    }
}
