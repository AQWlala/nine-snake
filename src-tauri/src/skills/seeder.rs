//! Built-in demo skills that ship with nine-snake.
//!
//! These skills are seeded into the skill store on first bootstrap
//! so the Skill Browser is never empty. Each skill is intentionally
//! simple — they serve as examples users can inspect, modify, or
//! use as templates for their own skills.

use tracing::info;

use crate::skills::types::{CreateSkillRequest, Skill};

/// Seed three demo skills into the skill engine.
///
/// Idempotent: skips any skill whose name already exists in the store.
pub fn seed_demo_skills(engine: &crate::skills::engine::SkillEngine) -> anyhow::Result<Vec<Skill>> {
    let demo_skills: Vec<CreateSkillRequest> = vec![
        // Skill 1: Hello World (Python)
        CreateSkillRequest {
            name: "hello-world".into(),
            description: "Prints a greeting. The simplest possible skill — use it to verify the skill engine works.".into(),
            code: r#"print("Hello from nine-snake!")
import platform
print(f"Python {platform.python_version()} on {platform.system()}")
"#.into(),
            language: "python".into(),
            tags: vec!["demo".into(), "beginner".into()],
            source_memory_id: None,
            ..Default::default()
        },
        // Skill 2: File Summary (Python)
        CreateSkillRequest {
            name: "file-summary".into(),
            description: "Reads a file and prints line count, word count, and byte size. Useful for quick file inspection.".into(),
            code: r#"import os
import sys

# Read FILENAME from params or use first argument
filename = os.environ.get("SKILL_FILE", "")
if not filename and len(sys.argv) > 1:
    filename = sys.argv[1]

if not filename:
    print("Usage: set SKILL_FILE=/path/to/file or pass as argument")
    exit(1)

if not os.path.exists(filename):
    print(f"File not found: {filename}")
    exit(1)

size = os.path.getsize(filename)
with open(filename, "r", encoding="utf-8", errors="replace") as f:
    lines = f.readlines()

words = sum(len(line.split()) for line in lines)
print(f"File: {os.path.basename(filename)}")
print(f"Lines: {len(lines)}")
print(f"Words: {words}")
print(f"Size:  {size} bytes")
"#.into(),
            language: "python".into(),
            tags: vec!["demo".into(), "file".into(), "utility".into()],
            source_memory_id: None,
            ..Default::default()
        },
        // Skill 3: Code Review Prompt (LLM)
        CreateSkillRequest {
            name: "code-review".into(),
            description: "Generates a structured code-review prompt for the given code snippet. Paste code into the skill input and send to any LLM agent.".into(),
            code: r#"You are a senior code reviewer. Review the following code and provide:

1. **Bugs & Edge Cases**: What could go wrong?
2. **Style & Readability**: Naming, structure, comments
3. **Performance**: Bottlenecks or unnecessary work
4. **Suggestions**: Concrete improvements with before/after examples

Be concise. Flag severity: [critical] [warning] [nit].

--- CODE TO REVIEW ---
{{INPUT}}
"#.into(),
            language: "llm".into(),
            tags: vec!["demo".into(), "code".into(), "review".into()],
            source_memory_id: None,
            ..Default::default()
        },
    ];

    let mut created = Vec::new();

    for req in demo_skills {
        // Idempotent: skip if already seeded
        let existing = engine.list_skills(crate::skills::types::ListSkillsRequest {
            language: None,
            tag: None,
            limit: 100,
        })?;

        if existing.iter().any(|s| s.name == req.name) {
            info!(
                target: "nine_snake.skills.seed",
                name = %req.name,
                "demo skill already exists, skipping"
            );
            continue;
        }

        match engine.create_skill(req.clone()) {
            Ok(skill) => {
                info!(
                    target: "nine_snake.skills.seed",
                    name = %skill.name,
                    id = %skill.id,
                    "seeded demo skill"
                );
                created.push(skill);
            }
            Err(e) => {
                // Don't fail bootstrap for a demo skill
                tracing::warn!(
                    target: "nine_snake.skills.seed",
                    name = %req.name,
                    error = ?e,
                    "failed to seed demo skill"
                );
            }
        }
    }

    Ok(created)
}
