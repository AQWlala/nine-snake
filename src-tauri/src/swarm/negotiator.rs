use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::agents::AgentOutput;
use crate::llm::{ChatMessage, LlmGateway};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NegotiationResult {
    pub chosen: AgentOutput,
    pub method: NegotiationMethod,
    pub conflict_detected: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum NegotiationMethod {
    HighConfidence,
    LlmArbitration,
    FallbackHighestConfidence,
}

pub struct Negotiator {
    confidence_threshold: f32,
}

impl Negotiator {
    pub fn new() -> Self {
        Self {
            confidence_threshold: 0.8,
        }
    }

    pub fn with_confidence_threshold(mut self, threshold: f32) -> Self {
        self.confidence_threshold = threshold.clamp(0.0, 1.0);
        self
    }

    pub fn negotiate(&self, outputs: Vec<AgentOutput>) -> NegotiationResult {
        if outputs.len() <= 1 {
            return NegotiationResult {
                chosen: outputs.into_iter().next().unwrap_or_else(|| {
                    AgentOutput::new(
                        super::agents::AgentKind::Generic,
                        "system",
                        "no outputs to negotiate",
                    )
                }),
                method: NegotiationMethod::HighConfidence,
                conflict_detected: false,
            };
        }

        let has_conflict = self.has_conflict(&outputs);

        if !has_conflict {
            let best = self.highest_confidence(outputs);
            return NegotiationResult {
                chosen: best,
                method: NegotiationMethod::HighConfidence,
                conflict_detected: false,
            };
        }

        let best = self.highest_confidence(outputs.clone());
        if best.confidence >= self.confidence_threshold {
            info!(
                target: "nine_snake.negotiator",
                confidence = best.confidence,
                "high confidence output selected without arbitration"
            );
            return NegotiationResult {
                chosen: best,
                method: NegotiationMethod::HighConfidence,
                conflict_detected: true,
            };
        }

        NegotiationResult {
            chosen: best,
            method: NegotiationMethod::FallbackHighestConfidence,
            conflict_detected: true,
        }
    }

    pub async fn negotiate_with_arbitration(
        &self,
        outputs: Vec<AgentOutput>,
        llm: &LlmGateway,
    ) -> Result<NegotiationResult> {
        if outputs.len() <= 1 || !self.has_conflict(&outputs) {
            let result = self.negotiate(outputs);
            return Ok(result);
        }

        let best = self.highest_confidence(outputs.clone());
        if best.confidence >= self.confidence_threshold {
            return Ok(NegotiationResult {
                chosen: best,
                method: NegotiationMethod::HighConfidence,
                conflict_detected: true,
            });
        }

        match self.llm_arbitrate(outputs, llm).await {
            Ok(chosen) => Ok(NegotiationResult {
                chosen,
                method: NegotiationMethod::LlmArbitration,
                conflict_detected: true,
            }),
            Err(e) => {
                warn!(target: "nine_snake.negotiator", error = ?e, "LLM arbitration failed; falling back to highest confidence");
                Ok(NegotiationResult {
                    chosen: best,
                    method: NegotiationMethod::FallbackHighestConfidence,
                    conflict_detected: true,
                })
            }
        }
    }

    pub fn has_conflict(&self, outputs: &[AgentOutput]) -> bool {
        if outputs.len() < 2 {
            return false;
        }
        let bodies: Vec<&str> = outputs.iter().map(|o| o.body.as_str()).collect();
        let first = bodies[0];
        bodies[1..].iter().any(|b| text_similarity(first, b) < 0.5)
    }

    async fn llm_arbitrate(
        &self,
        outputs: Vec<AgentOutput>,
        llm: &LlmGateway,
    ) -> Result<AgentOutput> {
        let candidates: Vec<String> = outputs
            .iter()
            .enumerate()
            .map(|(i, o)| {
                format!(
                    "Candidate {}: [{}] confidence={:.2}\n{}",
                    i + 1,
                    o.kind.as_str(),
                    o.confidence,
                    o.body
                )
            })
            .collect();

        let prompt = format!(
            "You are an arbitration judge. Multiple AI agents produced conflicting results for the same task. \
             Select the best candidate or synthesize the best parts.\n\n\
             {}\n\n\
             Respond with the best answer. Do not explain your choice.",
            candidates.join("\n\n")
        );

        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: prompt,
        }];
        let response = llm.chat(messages).await?;

        Ok(AgentOutput::new(
            super::agents::AgentKind::Generic,
            "arbitrator",
            response.message.content,
        ))
    }

    fn highest_confidence(&self, outputs: Vec<AgentOutput>) -> AgentOutput {
        outputs
            .into_iter()
            .max_by(|a, b| {
                a.confidence
                    .partial_cmp(&b.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap_or_else(|| {
                AgentOutput::new(super::agents::AgentKind::Generic, "system", "no outputs")
            })
    }
}

impl Default for Negotiator {
    fn default() -> Self {
        Self::new()
    }
}

fn text_similarity(a: &str, b: &str) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let a_words: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let b_words: std::collections::HashSet<&str> = b.split_whitespace().collect();
    let intersection = a_words.intersection(&b_words).count();
    let union = a_words.union(&b_words).count();
    if union == 0 {
        return 1.0;
    }
    intersection as f32 / union as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_conflict_single_output() {
        let n = Negotiator::new();
        let outputs = vec![AgentOutput::new(
            super::super::agents::AgentKind::Generic,
            "a",
            "hello",
        )];
        let result = n.negotiate(outputs);
        assert!(!result.conflict_detected);
    }

    #[test]
    fn high_confidence_wins() {
        let n = Negotiator::new();
        let outputs = vec![
            AgentOutput {
                kind: super::super::agents::AgentKind::Generic,
                author: "a".into(),
                body: "answer a".into(),
                confidence: 0.9,
            },
            AgentOutput {
                kind: super::super::agents::AgentKind::Generic,
                author: "b".into(),
                body: "answer b".into(),
                confidence: 0.5,
            },
        ];
        let result = n.negotiate(outputs);
        assert_eq!(result.chosen.author, "a");
    }

    #[test]
    fn text_similarity_identical() {
        assert_eq!(text_similarity("hello world", "hello world"), 1.0);
    }

    #[test]
    fn text_similarity_different() {
        assert!(text_similarity("cat dog", "fish bird") < 0.3);
    }
}
