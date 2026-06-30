//! v0.5: built-in writing templates.
//!
//! The Writing mode ships with a small library of skeleton templates that
//! the user can drop into a fresh document.  Each template is a plain
//! Markdown string with `{{placeholder}}` slots the front-end can fill in
//! at create time.  The template metadata (id, label, description, icon)
//! drives the template-picker UI in `WritingMode.tsx`.
//!
//! The v0.5 set covers the most common content shapes the swarm
//! agents are expected to produce; expanding the library in v1.0 is a
//! pure-data exercise and does not require Rust changes.

use serde::{Deserialize, Serialize};

/// One template entry as returned to the front-end.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WritingTemplate {
    /// Stable id (e.g. `"tech-blog"`).
    pub id: String,
    /// Short Chinese label shown in the picker.
    pub label: String,
    /// One-sentence description.
    pub description: String,
    /// Emoji icon.
    pub icon: String,
    /// Markdown body with `{{placeholders}}`.
    pub body: String,
    /// Placeholders surfaced in the UI for fast filling.  Order is the
    /// insertion order in the editor.
    pub placeholders: Vec<TemplatePlaceholder>,
}

/// One `{{placeholder}}` slot in a template body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TemplatePlaceholder {
    /// The token name (matches `{{name}}` in the body).
    pub name: String,
    /// Human-readable hint shown in the UI.
    pub hint: String,
    /// `true` if this is multi-line.
    pub multiline: bool,
}

/// Look up a template by id.
pub fn find(id: &str) -> Option<WritingTemplate> {
    library().into_iter().find(|t| t.id == id)
}

/// Return the entire v0.5 template library.
pub fn library() -> Vec<WritingTemplate> {
    vec![
        tech_blog(),
        marketing_copy(),
        academic_paper(),
        business_email(),
        meeting_notes(),
        novel_chapter(),
    ]
}

// ---------------------------------------------------------------------
// Individual templates
// ---------------------------------------------------------------------

fn tech_blog() -> WritingTemplate {
    WritingTemplate {
        id: "tech-blog".into(),
        label: "技术博客".into(),
        description: "带 TL;DR / 背景 / 实现 / 总结的标准技术文章骨架".into(),
        icon: "📝".into(),
        placeholders: vec![
            TemplatePlaceholder {
                name: "title".into(),
                hint: "标题".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "summary".into(),
                hint: "一句话 TL;DR".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "background".into(),
                hint: "问题背景".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "approach".into(),
                hint: "解决思路".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "code".into(),
                hint: "核心代码".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "results".into(),
                hint: "效果 / 性能".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "takeaways".into(),
                hint: "关键收获".into(),
                multiline: true,
            },
        ],
        body: r#"# {{title}}

> **TL;DR** — {{summary}}

## 背景

{{background}}

## 解决思路

{{approach}}

## 实现

```rust
{{code}}
```

## 效果

{{results}}

## 总结

{{takeaways}}
"#
        .into(),
    }
}

fn marketing_copy() -> WritingTemplate {
    WritingTemplate {
        id: "marketing-copy".into(),
        label: "营销文案".into(),
        description: "痛点 → 方案 → 价值 → CTA 经典四段式".into(),
        icon: "📣".into(),
        placeholders: vec![
            TemplatePlaceholder {
                name: "product".into(),
                hint: "产品名".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "audience".into(),
                hint: "目标用户".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "pain".into(),
                hint: "核心痛点".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "promise".into(),
                hint: "价值承诺".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "proof".into(),
                hint: "数据 / 案例 / 证言".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "cta".into(),
                hint: "行动号召".into(),
                multiline: false,
            },
        ],
        body: r#"# {{product}} —— 给 {{audience}} 的更好选择

## 你是否也遇到这些问题？

{{pain}}

## 我们能给你什么？

{{promise}}

## 谁在用？效果如何？

{{proof}}

## 现在就试试

{{cta}}
"#
        .into(),
    }
}

fn academic_paper() -> WritingTemplate {
    WritingTemplate {
        id: "academic-paper".into(),
        label: "学术论文".into(),
        description: "IMRaD 结构的论文骨架（中文版）".into(),
        icon: "🎓".into(),
        placeholders: vec![
            TemplatePlaceholder {
                name: "title".into(),
                hint: "题目".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "abstract".into(),
                hint: "摘要".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "keywords".into(),
                hint: "关键词（逗号分隔）".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "introduction".into(),
                hint: "引言".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "method".into(),
                hint: "方法".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "results".into(),
                hint: "结果".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "discussion".into(),
                hint: "讨论".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "conclusion".into(),
                hint: "结论".into(),
                multiline: true,
            },
        ],
        body: r#"# {{title}}

**摘要**：{{abstract}}

**关键词**：{{keywords}}

## 1. 引言

{{introduction}}

## 2. 方法

{{method}}

## 3. 结果

{{results}}

## 4. 讨论

{{discussion}}

## 5. 结论

{{conclusion}}
"#
        .into(),
    }
}

fn business_email() -> WritingTemplate {
    WritingTemplate {
        id: "business-email".into(),
        label: "工作邮件".into(),
        description: "简洁专业的商务邮件骨架".into(),
        icon: "📧".into(),
        placeholders: vec![
            TemplatePlaceholder {
                name: "subject".into(),
                hint: "邮件主题".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "recipient".into(),
                hint: "收件人称呼".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "context".into(),
                hint: "背景 / 来意".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "request".into(),
                hint: "希望对方做什么".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "deadline".into(),
                hint: "时间节点".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "signoff".into(),
                hint: "署名".into(),
                multiline: false,
            },
        ],
        body: r#"**主题**：{{subject}}

{{recipient}} 您好，

{{context}}

{{request}}

如能在 {{deadline}} 前回复，我将非常感谢。如有任何问题，欢迎随时联系。

此致
敬礼

{{signoff}}
"#
        .into(),
    }
}

fn meeting_notes() -> WritingTemplate {
    WritingTemplate {
        id: "meeting-notes".into(),
        label: "会议纪要".into(),
        description: "时间 / 参与人 / 议题 / 决议 / 行动项".into(),
        icon: "🗒️".into(),
        placeholders: vec![
            TemplatePlaceholder {
                name: "title".into(),
                hint: "会议名".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "datetime".into(),
                hint: "时间".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "attendees".into(),
                hint: "参与人".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "agenda".into(),
                hint: "议程".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "decisions".into(),
                hint: "已达成决议".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "actions".into(),
                hint: "行动项（谁 / 做什么 / 何时）".into(),
                multiline: true,
            },
        ],
        body: r#"# {{title}} 会议纪要

- **时间**：{{datetime}}
- **参与人**：{{attendees}}

## 议程

{{agenda}}

## 决议

{{decisions}}

## 行动项

{{actions}}
"#
        .into(),
    }
}

fn novel_chapter() -> WritingTemplate {
    WritingTemplate {
        id: "novel-chapter".into(),
        label: "小说章节".into(),
        description: "场景 → 冲突 → 转折 → 钩子 四段式小说骨架".into(),
        icon: "📖".into(),
        placeholders: vec![
            TemplatePlaceholder {
                name: "chapter".into(),
                hint: "章节名 / 序号".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "pov".into(),
                hint: "POV 角色".into(),
                multiline: false,
            },
            TemplatePlaceholder {
                name: "setting".into(),
                hint: "场景设定".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "conflict".into(),
                hint: "冲突 / 紧张".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "turn".into(),
                hint: "转折 / 揭示".into(),
                multiline: true,
            },
            TemplatePlaceholder {
                name: "hook".into(),
                hint: "章末钩子".into(),
                multiline: true,
            },
        ],
        body: r#"# {{chapter}}

> POV: {{pov}}

{{setting}}

{{conflict}}

{{turn}}

{{hook}}
"#
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_returns_six_templates() {
        let lib = library();
        assert_eq!(lib.len(), 6);
        let ids: Vec<&str> = lib.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&"tech-blog"));
        assert!(ids.contains(&"marketing-copy"));
        assert!(ids.contains(&"academic-paper"));
        assert!(ids.contains(&"business-email"));
        assert!(ids.contains(&"meeting-notes"));
        assert!(ids.contains(&"novel-chapter"));
    }

    #[test]
    fn library_has_unique_ids() {
        let lib = library();
        let mut ids: Vec<&str> = lib.iter().map(|t| t.id.as_str()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), lib.len(), "duplicate template ids");
    }

    #[test]
    fn find_returns_known_template() {
        let t = find("tech-blog").expect("tech-blog must exist");
        assert_eq!(t.label, "技术博客");
        assert!(t.body.contains("{{title}}"));
    }

    #[test]
    fn find_returns_none_for_unknown() {
        assert!(find("does-not-exist").is_none());
    }

    #[test]
    fn placeholders_match_body_tokens() {
        for t in library() {
            for p in &t.placeholders {
                let token = format!("{{{{{}}}}}", p.name);
                assert!(
                    t.body.contains(&token),
                    "template {} is missing body token {}",
                    t.id,
                    token
                );
            }
        }
    }
}
