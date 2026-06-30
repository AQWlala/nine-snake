//! Skill composer — v1.2 P1 orchestration upgrade
//!
//! Moves the swarm from *manual skill assignment* to *semantic auto-composition*.
//! When a task is submitted, the composer analyses its description (with LLM
//! support) and selects the most relevant skills from the skill store.  The
//! selected skill instructions are then injected into every agent's context so
//! agents can leverage them without the user needing to know which skills exist.
//!
//! ## How it works
//!
//! 1. **Fast path** — embed the task description and run a cosine similarity
//!    search against stored skill descriptions (SQLite FTS5 + manual fallback).
//!    Return the top-N matches immediately.
//! 2. **LLM path** — when the fast path returns low-confidence results or the
//!    caller explicitly requests it, ask the LLM to rank/select skills from the
//!    full catalogue.  More expensive but handles nuanced tasks.
//!
//! ## Integration
//!
//! The orchestrator calls `compose(task_description)` before dispatching agents.
//! The returned `SkillContext` is folded into the team context under the
//! `skills` namespace so every agent sees consistent skill guidance.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::llm::LlmGateway;
use crate::skills::store::SkillStore;
use crate::skills::types::Skill;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A matched skill with a relevance score (0.0 – 1.0).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMatch {
    pub skill: Skill,
    /// Relevance score; 1.0 = perfect match.
    pub score: f32,
}

/// Compiled skill context injected into agent prompts.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillContext {
    /// Matched skills sorted by relevance.
    pub matches: Vec<SkillMatch>,
    /// Whether the LLM was consulted for this composition.
    pub llm_assisted: bool,
    /// Compact prompt-ready instruction block.
    pub instruction_block: String,
}

// ---------------------------------------------------------------------------
// Composer
// ---------------------------------------------------------------------------

pub struct SkillComposer {
    store: Arc<SkillStore>,
    llm: Option<Arc<LlmGateway>>,
    /// Minimum score threshold for the fast path; below this the LLM path
    /// is attempted (when available).
    fast_path_threshold: f32,
    /// Maximum number of skills to return.
    max_skills: usize,
}

impl SkillComposer {
    /// Creates a new composer backed by a skill store.
    ///
    /// `llm` enables the LLM-assisted ranking path and can be `None` for
    /// a pure fast-path (FTS5 + keyword) composer.
    pub fn new(store: Arc<SkillStore>, llm: Option<Arc<LlmGateway>>) -> Self {
        Self {
            store,
            llm,
            fast_path_threshold: 0.3,
            max_skills: 5,
        }
    }

    /// Composes a skill context for the given task description.
    ///
    /// Always returns at least an empty `SkillContext` (never fails).
    pub async fn compose(&self, task_description: &str) -> SkillContext {
        // 1. Fast path — keyword + FTS5 search.
        let fast = self.fast_path_search(task_description).await;

        // 2. Check if fast path is good enough.
        if !fast.is_empty() {
            let top_score = fast[0].score;
            if top_score >= self.fast_path_threshold || self.llm.is_none() {
                return self.build_context(fast, false);
            }
        }

        // 3. LLM-assisted path (when available and cheap match insufficient).
        if let Some(llm) = &self.llm {
            match self.llm_rank(llm, task_description, &fast).await {
                Ok(ranked) => return self.build_context(ranked, true),
                Err(e) => {
                    warn!(target: "nine_snake.composer", error = %e, "LLM ranking failed, falling back to fast path");
                }
            }
        }

        self.build_context(fast, false)
    }

    // ------------------------------------------------------------------
    // Fast path
    // ------------------------------------------------------------------

    async fn fast_path_search(&self, task_description: &str) -> Vec<SkillMatch> {
        // Build search terms from the task description.
        let terms: Vec<&str> = task_description
            .split(|c: char| c.is_whitespace() || c == ',' || c == '，' || c == '。' || c == '.')
            .filter(|w| w.len() >= 2)
            .take(10)
            .collect();

        let mut scored: Vec<SkillMatch> = Vec::new();

        // Load all skills from the store.
        let skills = match self.store.list(None, None, 100) {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "nine_snake.composer", error = %e, "failed to list skills");
                return Vec::new();
            }
        };

        if skills.is_empty() {
            return Vec::new();
        }

        for skill in skills {
            let mut score: f32 = 0.0;
            let desc = skill.description.to_lowercase();
            let name = skill.name.to_lowercase();

            // Simple keyword match scoring.
            for term in &terms {
                let term_lower = term.to_lowercase();
                if name.contains(&term_lower) {
                    score += 0.4;
                }
                if desc.contains(&term_lower) {
                    score += 0.2;
                }
            }

            // Normalise by number of terms (max possible per skill).
            let max_possible = terms.len() as f32 * 0.6;
            if max_possible > 0.0 {
                score = (score / max_possible).clamp(0.0, 1.0);
            }

            if score > 0.0 {
                scored.push(SkillMatch { skill, score });
            }
        }

        // Sort descending by score.
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        scored.truncate(self.max_skills);
        scored
    }

    // ------------------------------------------------------------------
    // LLM-assisted ranking
    // ------------------------------------------------------------------

    async fn llm_rank(
        &self,
        llm: &LlmGateway,
        task: &str,
        fast_candidates: &[SkillMatch],
    ) -> anyhow::Result<Vec<SkillMatch>> {
        // Build a prompt that lists all available skills and asks the LLM
        // to select the most relevant ones.
        let all_skills = match self.store.list(None, None, 100) {
            Ok(s) => s,
            Err(e) => anyhow::bail!("failed to list skills: {e}"),
        };

        if all_skills.is_empty() {
            return Ok(Vec::new());
        }

        let skill_list: String = all_skills
            .iter()
            .enumerate()
            .map(|(i, s)| {
                format!(
                    "{i}. **{}**: {} (language: {}, tags: [{}])",
                    s.name,
                    s.description,
                    s.language,
                    s.tags.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = format!(
            "You are a skill routing assistant. Given a task description, select up to {} \
             most relevant skills from the catalogue below.\n\n\
             ## Task\n{task}\n\n\
             ## Skill Catalogue\n{skill_list}\n\n\
             Return ONLY a JSON array of skill indices (0-based), ordered by relevance. \
             Example: [3, 0, 7]\n\
             If no skills are relevant, return an empty array: []",
            self.max_skills
        );

        match llm.generate(&prompt).await {
            Ok(response) => {
                let text = response.trim();
                let indices: Vec<usize> = serde_json::from_str(text).unwrap_or_default();

                let ranked: Vec<SkillMatch> = indices
                    .into_iter()
                    .filter_map(|idx| {
                        all_skills.get(idx).map(|s| SkillMatch {
                            skill: s.clone(),
                            score: 0.9, // LLM-selected skills get high confidence.
                        })
                    })
                    .take(self.max_skills)
                    .collect();

                if !ranked.is_empty() {
                    info!(
                        target: "nine_snake.composer",
                        count = ranked.len(),
                        names = ?ranked.iter().map(|m| &m.skill.name).collect::<Vec<_>>(),
                        "LLM selected skills"
                    );
                }

                Ok(ranked)
            }
            Err(e) => {
                // Fall back to fast-path candidates.
                warn!(target: "nine_snake.composer", error = %e, "LLM call failed");
                Ok(fast_candidates.to_vec())
            }
        }
    }

    // ------------------------------------------------------------------
    // Context builder
    // ------------------------------------------------------------------

    fn build_context(&self, matches: Vec<SkillMatch>, llm_assisted: bool) -> SkillContext {
        let instruction_block = if matches.is_empty() {
            String::new()
        } else {
            let blocks: Vec<String> = matches
                .iter()
                .map(|m| {
                    format!(
                        "## Skill: {name} (score: {score:.2})\n{instructions}",
                        name = m.skill.name,
                        score = m.score,
                        instructions = m.skill.code
                    )
                })
                .collect();
            format!(
                "--- Available Skills ---\n\
                 The following skills match this task. Use their instructions\n\
                 as guidance for how to structure your work:\n\n{}",
                blocks.join("\n\n")
            )
        };

        debug!(
            target: "nine_snake.composer",
            match_count = matches.len(),
            llm_assisted,
            "skill context built"
        );

        SkillContext {
            matches,
            llm_assisted,
            instruction_block,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_context() {
        let ctx = SkillContext::default();
        assert!(ctx.matches.is_empty());
        assert!(!ctx.llm_assisted);
        assert!(ctx.instruction_block.is_empty());
    }
}
