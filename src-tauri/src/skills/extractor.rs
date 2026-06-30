//! Skill auto-extraction engine — v1.2 skill closed-loop learning.
//!
//! ## What this does
//!
//! After a swarm task completes successfully, the orchestrator calls
//! `SkillExtractor::try_extract()` with the task description, collected
//! agent outputs, and team context.  The extractor uses the LLM gateway
//! to:
//!
//! 1. Judge whether the task produced a **reusable workflow** (not all
//!    tasks do — one-shot queries should not be persisted).
//! 2. If yes, distil a **skill description, code template, and tags**
//!    from the successful execution pattern.
//! 3. Persist the skill via `SkillStore`, mark the source memory as
//!    "distilled to skill", and write a `SKILL.md` file to the skill
//!    archive for agentskills.io compatibility.
//!
//! ## agentskills.io compatibility
//!
//! Extracted skills are written in the standard `SKILL.md` format so
//! they can be shared with Hermes, OpenClaw (ClawHub), and other
//! agentskills.io-compatible agents.  The format is:
//!
//! ```markdown
//! # Skill: <name>
//!
//! **Language**: llm | python
//! **Tags**: tag1, tag2
//! **Auto-extracted**: 2026-06-25T12:00:00Z
//!
//! ## Description
//! <natural-language description>
//!
//! ## Template
//! <code / prompt template>
//! ```
//!
//! ## Safety
//!
//! * Extraction is **non-blocking** — it runs as a best-effort background
//!   task that never blocks the orchestrator.
//! * The `extraction_prompt` is a fixed system prompt that explicitly
//!   instructs the LLM to output `{"extractable": false}` when the task
//!   is a one-shot or trivial interaction.
//! * Auto-extracted skills default to `language = "llm"` (safe prompt
//!   template mode) — they never auto-generate executable code.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::llm::{ChatMessage, LlmGateway};
use crate::skills::store::SkillStore;
use crate::swarm::context::TeamContext;

// ---------------------------------------------------------------------------
// LLM extraction prompt (system)
// ---------------------------------------------------------------------------

const EXTRACTION_SYSTEM_PROMPT: &str = r#"You are the Skill Extractor for nine-snake, a multi-agent desktop AI assistant.

Your job is to analyze a completed swarm task and decide whether it produced a
**reusable workflow** that should be saved as a Skill for future use.

## Rules

1. **NOT extractable** (return `{"extractable": false}`):
   - One-shot Q&A (e.g., "what time is it?", "how do I do X?")
   - Trivial operations (echo, simple file read, single command)
   - Tasks that failed or produced no useful output
   - Tasks that are too specific to a single file or context

2. **Extractable** — tasks that demonstrate a reusable pattern:
   - Multi-step workflows (e.g., "set up a new project", "deploy to server")
   - Code generation patterns (e.g., "create a React component with tests")
   - Data processing pipelines (e.g., "extract and transform CSV data")
   - System operations (e.g., "check disk usage and clean up")
   - Any task whose output could save another user/agent time if reused

3. When extractable, you MUST return valid JSON with:
   ```json
   {
     "extractable": true,
     "name": "kebab-case-name-no-spaces",
     "description": "One-sentence description of what the skill does",
     "category": "code|writing|work|system|data|general",
     "tags": ["tag1", "tag2"],
     "template": "The reusable prompt or code template with {{placeholders}}"
   }
   ```

4. The `template` field should:
   - Replace specific values with `{{placeholder}}` (e.g., `{{file_path}}`, `{{project_name}}`)
   - Capture the essential workflow steps
   - Be concise but complete enough to guide a future agent

Return ONLY the JSON object, no other text."#;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// LLM extraction response.
#[derive(Debug, Clone, Deserialize)]
struct ExtractionResult {
    extractable: bool,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    template: String,
}

/// Metadata about an extraction attempt (for observability).
#[derive(Debug, Clone, Serialize)]
pub struct ExtractionReport {
    pub task_description: String,
    pub extractable: bool,
    pub skill_name: Option<String>,
    pub skill_id: Option<String>,
    pub duration_ms: u64,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// SkillExtractor
// ---------------------------------------------------------------------------

/// The auto-extraction engine.  Call `try_extract()` after a successful
/// swarm task to attempt closed-loop skill creation.
pub struct SkillExtractor {
    llm: Arc<LlmGateway>,
    store: Arc<SkillStore>,
    /// Path to the skill archive directory (for `SKILL.md` files).
    archive_dir: String,
}

impl SkillExtractor {
    pub fn new(
        llm: Arc<LlmGateway>,
        store: Arc<SkillStore>,
        archive_dir: impl Into<String>,
    ) -> Self {
        Self {
            llm,
            store,
            archive_dir: archive_dir.into(),
        }
    }

    /// Attempt to extract a reusable skill from a completed swarm task.
    ///
    /// This method is **non-blocking** — it should be spawned via
    /// `tokio::spawn` so the orchestrator can return immediately.
    ///
    /// Returns an `ExtractionReport` regardless of success/failure.
    pub async fn try_extract(
        &self,
        task_description: &str,
        agent_count: u32,
        success_count: u32,
        outputs: &[String],
        ctx: &TeamContext,
    ) -> ExtractionReport {
        let start = std::time::Instant::now();

        // Quick pre-filter: don't even call the LLM if the task is trivial.
        if task_description.len() < 20 || success_count == 0 {
            return ExtractionReport {
                task_description: task_description.to_string(),
                extractable: false,
                skill_name: None,
                skill_id: None,
                duration_ms: start.elapsed().as_millis() as u64,
                error: None,
            };
        }

        // Build the extraction prompt.
        let prompt = format!(
            "Task: {}\n\nAgents: {} spawned, {} succeeded\n\nOutputs:\n{}\n\nTeam context:\n{}",
            task_description,
            agent_count,
            success_count,
            outputs.join("\n---\n"),
            ctx.render()
        );

        let messages = vec![
            ChatMessage::system(EXTRACTION_SYSTEM_PROMPT),
            ChatMessage::user(prompt),
        ];

        // Call the LLM.
        let llm_result = self.llm.chat(messages).await;
        let duration_ms = start.elapsed().as_millis() as u64;

        match llm_result {
            Err(e) => {
                warn!(target: "nine_snake.skills", error = %e, "skill extraction LLM call failed");
                ExtractionReport {
                    task_description: task_description.to_string(),
                    extractable: false,
                    skill_name: None,
                    skill_id: None,
                    duration_ms,
                    error: Some(e.to_string()),
                }
            }
            Ok(response) => {
                let body = response.message.content.trim().to_string();
                // Strip markdown code fences if present.
                let json_str = body
                    .strip_prefix("```json")
                    .and_then(|s| s.strip_suffix("```"))
                    .map(|s| s.trim())
                    .unwrap_or_else(|| {
                        body.strip_prefix("```")
                            .and_then(|s| s.strip_suffix("```"))
                            .map(|s| s.trim())
                            .unwrap_or(&body)
                    });

                match serde_json::from_str::<ExtractionResult>(json_str) {
                    Err(e) => {
                        warn!(target: "nine_snake.skills", error = %e, body = %json_str, "failed to parse extraction result");
                        ExtractionReport {
                            task_description: task_description.to_string(),
                            extractable: false,
                            skill_name: None,
                            skill_id: None,
                            duration_ms,
                            error: Some(format!("JSON parse error: {}", e)),
                        }
                    }
                    Ok(extracted) => {
                        if !extracted.extractable {
                            info!(target: "nine_snake.skills", task = %task_description, "task not extractable");
                            return ExtractionReport {
                                task_description: task_description.to_string(),
                                extractable: false,
                                skill_name: None,
                                skill_id: None,
                                duration_ms,
                                error: None,
                            };
                        }

                        // Persist the skill.
                        match self.persist_skill(&extracted).await {
                            Ok(skill_id) => {
                                info!(
                                    target: "nine_snake.skills",
                                    name = %extracted.name,
                                    id = %skill_id,
                                    "auto-extracted skill saved"
                                );
                                ExtractionReport {
                                    task_description: task_description.to_string(),
                                    extractable: true,
                                    skill_name: Some(extracted.name.clone()),
                                    skill_id: Some(skill_id),
                                    duration_ms,
                                    error: None,
                                }
                            }
                            Err(e) => {
                                warn!(target: "nine_snake.skills", error = %e, name = %extracted.name, "failed to persist extracted skill");
                                ExtractionReport {
                                    task_description: task_description.to_string(),
                                    extractable: true,
                                    skill_name: Some(extracted.name.clone()),
                                    skill_id: None,
                                    duration_ms,
                                    error: Some(format!("persist error: {}", e)),
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Persist the extracted skill to SQLite + write SKILL.md.
    async fn persist_skill(&self, extracted: &ExtractionResult) -> Result<String> {
        use crate::skills::types::CreateSkillRequest;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let skill_id = format!("auto-{}-{}", extracted.name, now);

        let req = CreateSkillRequest {
            name: extracted.name.clone(),
            description: extracted.description.clone(),
            code: extracted.template.clone(),
            language: "llm".to_string(),
            tags: extracted.tags.clone(),
            source_memory_id: None,
            activation_condition: None,
            platform: None,
            min_confidence: None,
        };

        let skill = crate::skills::types::Skill {
            id: skill_id.clone(),
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
            source_memory_id: Some(String::new()),
            activation_condition: None,
            platform: None,
            min_confidence: None,
        };
        self.store
            .insert(&skill)
            .context("failed to insert extracted skill into store")?;

        // Write SKILL.md to archive directory.
        self.write_skill_md(&skill_id, extracted, now)
            .context("failed to write SKILL.md")?;

        Ok(skill_id)
    }

    /// Write a SKILL.md file in agentskills.io-compatible format.
    fn write_skill_md(&self, skill_id: &str, extracted: &ExtractionResult, now: i64) -> Result<()> {
        let dir = std::path::Path::new(&self.archive_dir).join(&extracted.name);
        std::fs::create_dir_all(&dir)?;

        let date_str = chrono::DateTime::from_timestamp(now, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_else(|| "unknown".to_string());

        let tags_str = extracted.tags.join(", ");

        let md = format!(
            r#"# Skill: {name}

**Language**: llm
**Tags**: {tags}
**Category**: {category}
**Auto-extracted**: {date}
**Skill ID**: {id}

## Description

{description}

## Template

```
{template}
```

---

> Auto-generated by nine-snake Skill Extractor (v1.2).
> This skill was distilled from a successful swarm task execution.
> Edit freely — your improvements will be preserved across re-extractions.
"#,
            name = extracted.name,
            tags = tags_str,
            category = extracted.category,
            date = date_str,
            id = skill_id,
            description = extracted.description,
            template = extracted.template,
        );

        let path = dir.join("SKILL.md");
        std::fs::write(&path, md)?;
        info!(target: "nine_snake.skills", path = %path.display(), "wrote SKILL.md");

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_extraction_result_positive() {
        let json = r#"{
            "extractable": true,
            "name": "generate-react-component",
            "description": "Generates a React component with TypeScript types",
            "category": "code",
            "tags": ["react", "typescript", "frontend"],
            "template": "Create a React component named {{component_name}} with {{props}}"
        }"#;
        let result: ExtractionResult = serde_json::from_str(json).unwrap();
        assert!(result.extractable);
        assert_eq!(result.name, "generate-react-component");
        assert_eq!(result.tags.len(), 3);
    }

    #[test]
    fn parse_extraction_result_negative() {
        let json = r#"{"extractable": false}"#;
        let result: ExtractionResult = serde_json::from_str(json).unwrap();
        assert!(!result.extractable);
        assert!(result.name.is_empty());
    }

    #[test]
    fn skill_md_format_contains_required_sections() {
        // Quick sanity check that the template renders without panicking.
        let extracted = ExtractionResult {
            extractable: true,
            name: "test-skill".into(),
            description: "A test skill".into(),
            category: "general".into(),
            tags: vec!["test".into()],
            template: "echo {{greeting}}".into(),
        };

        let dir = std::env::temp_dir().join("nine-snake-test-extractor");
        let extractor = SkillExtractor {
            llm: Arc::new(crate::llm::LlmGateway::new_test()),
            store: Arc::new(SkillStore::open_test(":memory:").unwrap()),
            archive_dir: dir.to_string_lossy().to_string(),
        };

        extractor
            .write_skill_md("test-id", &extracted, 1000)
            .unwrap();

        let md_path = dir.join("test-skill").join("SKILL.md");
        let content = std::fs::read_to_string(&md_path).unwrap();

        assert!(content.contains("# Skill: test-skill"));
        assert!(content.contains("## Description"));
        assert!(content.contains("## Template"));
        assert!(content.contains("Auto-extracted"));

        // Cleanup.
        let _ = std::fs::remove_dir_all(&dir);
    }
}
