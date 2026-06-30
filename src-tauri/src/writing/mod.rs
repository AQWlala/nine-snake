//! v0.5: Writing mode backend.
//!
//! Owns the persistence of long-form writing artifacts produced by the
//! front-end `WritingMode.tsx`.  The engine is intentionally thin:
//!
//! * documents are stored in the `documents` table (created by
//!   migration `004_v05.sql`),
//! * every save mirrors the body to an L3 memory row so the
//!   `Sponge`/`Reflection` engines can find it,
//! * exports are recorded in `document_exports` and the rendered
//!   output is returned to the caller (the front-end chooses how to
//!   persist it to disk).
//!
//! The export formats (Markdown and HTML) are both pure data
//! transforms — no network calls, no LLM involvement.

pub mod templates;

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use tracing::{info, instrument};
use uuid::Uuid;

pub use templates::{TemplatePlaceholder, WritingTemplate};

use crate::memory::sponge::SpongeEngine;
use crate::memory::sqlite_store::SqliteStore;
use crate::memory::types::{Memory, MemoryLayer, MemoryType, SourceKind};

/// A persisted writing document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: String,
    pub title: String,
    pub template_id: String,
    pub content: String,
    pub word_count: usize,
    pub memory_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub metadata: Option<serde_json::Value>,
}

/// A rendered export artifact returned to the front-end.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentExport {
    pub id: String,
    pub document_id: String,
    pub format: ExportFormat,
    pub body: String,
    pub byte_size: usize,
    pub exported_at: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Markdown,
    Html,
}

impl ExportFormat {
    fn as_str(self) -> &'static str {
        match self {
            ExportFormat::Markdown => "markdown",
            ExportFormat::Html => "html",
        }
    }
    pub fn from_str(s: &str) -> Result<Self> {
        match s {
            "markdown" | "md" => Ok(ExportFormat::Markdown),
            "html" | "htm" => Ok(ExportFormat::Html),
            other => Err(anyhow!("unknown export format: {other}")),
        }
    }
}

/// Writing engine — owns document CRUD, template application, export
/// and the L3 mirror to the memory sponge.
pub struct WritingEngine {
    sqlite: Arc<SqliteStore>,
    /// Optional sponge reference; if present, every save mirrors the
    /// document to an L3 memory row.
    sponge: Option<Arc<SpongeEngine>>,
}

impl WritingEngine {
    pub fn new(sqlite: Arc<SqliteStore>, sponge: Option<Arc<SpongeEngine>>) -> Self {
        Self { sqlite, sponge }
    }

    /// Returns the full template library (delegated to
    /// `templates::library`).
    pub fn list_templates(&self) -> Vec<WritingTemplate> {
        templates::library()
    }

    /// Fetches a single template by id.
    pub fn get_template(&self, id: &str) -> Option<WritingTemplate> {
        templates::find(id)
    }

    /// Applies the given placeholder values to a template body and
    /// returns the rendered string.  Unknown placeholders are left
    /// untouched so the user can see what's missing.
    pub fn apply_template(
        &self,
        template_id: &str,
        values: &std::collections::HashMap<String, String>,
    ) -> Result<(String, String)> {
        let tpl = templates::find(template_id)
            .ok_or_else(|| anyhow!("unknown template: {template_id}"))?;
        let mut body = tpl.body.clone();
        for (k, v) in values {
            let token = format!("{{{{{k}}}}}");
            body = body.replace(&token, v);
        }
        let title = values
            .get("title")
            .cloned()
            .unwrap_or_else(|| tpl.label.clone());
        Ok((title, body))
    }

    /// Creates a new document, optionally from a template.  If
    /// `from_template` is `Some`, placeholder values must be supplied.
    // v1.0.1 fix: the `#[instrument]` skip list referenced
    // `values` and `content`, but `create_document` has no
    // `values` parameter (the function that takes a `values`
    // hash-map is `apply_template` above).  Restrict the skip
    // list to the actually-present parameters so the macro
    // can verify them.
    #[instrument(skip(self, content, metadata), fields(template_id = %template_id))]
    pub fn create_document(
        &self,
        title: String,
        template_id: String,
        content: String,
        metadata: Option<serde_json::Value>,
    ) -> Result<Document> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().timestamp();
        let word_count = count_words(&content);
        let meta_json = metadata
            .as_ref()
            .map(|m| serde_json::to_string(m).unwrap_or_else(|_| "{}".into()));

        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        conn.execute(
            "INSERT INTO documents (id, title, template_id, content, word_count, memory_id, created_at, updated_at, metadata) \
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?6, ?7)",
            params![id, title, template_id, content, word_count as i64, now, meta_json],
        )
        .context("inserting document")?;

        drop(conn);

        // Mirror to L3 if a sponge is configured.  This is best-effort:
        // a failure to write the memory row should not fail the
        // document save.
        let memory_id = self
            .sponge
            .as_ref()
            .and_then(|sp| match futures::executor::block_on(mirror_to_l3(
                sp,
                &id,
                &title,
                &template_id,
                &content,
            )) {
                Ok(id) => Some(id),
                Err(e) => {
                    tracing::warn!(target: "nine_snake.writing", error = ?e, "failed to mirror document to L3 memory");
                    None
                }
            });

        if let Some(mid) = &memory_id {
            let conn = self.sqlite.raw_connection();
            let conn = conn.lock();
            conn.execute(
                "UPDATE documents SET memory_id = ?1 WHERE id = ?2",
                params![mid, id],
            )
            .ok();
        }

        info!(target: "nine_snake.writing", id = %id, title = %title, "document created");
        Ok(Document {
            id,
            title,
            template_id,
            content,
            word_count,
            memory_id,
            created_at: now,
            updated_at: now,
            metadata,
        })
    }

    /// Updates an existing document's content.  Word count and
    /// `updated_at` are refreshed; the L3 memory row is rewritten
    /// (the sponge de-duplicates so the change is incremental).
    #[instrument(skip(self, content), fields(id = %id))]
    pub fn update_document(&self, id: &str, content: String) -> Result<Document> {
        let now = Utc::now().timestamp();
        let word_count = count_words(&content);

        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        let n = conn
            .execute(
                "UPDATE documents SET content = ?1, word_count = ?2, updated_at = ?3 WHERE id = ?4",
                params![content, word_count as i64, now, id],
            )
            .context("updating document")?;
        if n == 0 {
            return Err(anyhow!("document not found: {id}"));
        }
        let doc: Document = conn
            .query_row(
                "SELECT id, title, template_id, content, word_count, memory_id, created_at, updated_at, metadata \
                 FROM documents WHERE id = ?1",
                params![id],
                |row| {
                    let meta: Option<String> = row.get(8)?;
                    Ok(Document {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        template_id: row.get(2)?,
                        content: row.get(3)?,
                        word_count: row.get::<_, i64>(4)? as usize,
                        memory_id: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                        metadata: meta.and_then(|m| serde_json::from_str(&m).ok()),
                    })
                },
            )
            .context("reloading updated document")?;
        drop(conn);

        // Best-effort L3 update.
        if let Some(sp) = &self.sponge {
            let _ = futures::executor::block_on(mirror_to_l3(
                sp,
                &doc.id,
                &doc.title,
                &doc.template_id,
                &content,
            ));
        }

        Ok(doc)
    }

    /// Fetches a document by id.
    pub fn get_document(&self, id: &str) -> Result<Option<Document>> {
        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        let doc = conn
            .query_row(
                "SELECT id, title, template_id, content, word_count, memory_id, created_at, updated_at, metadata \
                 FROM documents WHERE id = ?1",
                params![id],
                |row| {
                    let meta: Option<String> = row.get(8)?;
                    Ok(Document {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        template_id: row.get(2)?,
                        content: row.get(3)?,
                        word_count: row.get::<_, i64>(4)? as usize,
                        memory_id: row.get(5)?,
                        created_at: row.get(6)?,
                        updated_at: row.get(7)?,
                        metadata: meta.and_then(|m| serde_json::from_str(&m).ok()),
                    })
                },
            )
            .optional()
            .context("querying document")?;
        Ok(doc)
    }

    /// Lists the most recently updated documents, capped at `limit`.
    pub fn list_documents(&self, limit: usize) -> Result<Vec<Document>> {
        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, title, template_id, content, word_count, memory_id, created_at, updated_at, metadata \
             FROM documents ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let docs = stmt
            .query_map(params![limit.max(1) as i64], |row| {
                let meta: Option<String> = row.get(8)?;
                Ok(Document {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    template_id: row.get(2)?,
                    content: row.get(3)?,
                    word_count: row.get::<_, i64>(4)? as usize,
                    memory_id: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                    metadata: meta.and_then(|m| serde_json::from_str(&m).ok()),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(docs)
    }

    /// Deletes a document by id.  Returns the number of rows removed.
    pub fn delete_document(&self, id: &str) -> Result<bool> {
        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        let n = conn.execute("DELETE FROM documents WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    /// Renders the document to the requested format and records the
    /// export.  The returned `body` is the rendered text; the
    /// front-end decides whether to download it, copy it to the
    /// clipboard, or hand it off to the OS file system.
    pub fn export(&self, id: &str, format: ExportFormat) -> Result<DocumentExport> {
        let doc = self
            .get_document(id)?
            .ok_or_else(|| anyhow!("document not found: {id}"))?;
        let body = match format {
            ExportFormat::Markdown => render_markdown(&doc),
            ExportFormat::Html => render_html(&doc),
        };
        let byte_size = body.len();
        let now = Utc::now().timestamp();
        let export_id = Uuid::new_v4().to_string();

        let conn = self.sqlite.raw_connection();
        let conn = conn.lock();
        conn.execute(
            "INSERT INTO document_exports (id, document_id, format, byte_size, exported_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![export_id, id, format.as_str(), byte_size as i64, now],
        )
        .context("recording document export")?;

        Ok(DocumentExport {
            id: export_id,
            document_id: id.to_string(),
            format,
            body,
            byte_size,
            exported_at: now,
        })
    }
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

/// Mirrors a document body to an L3 Semantic memory.  Best-effort:
/// errors bubble up but callers should log-and-continue.
async fn mirror_to_l3(
    sponge: &SpongeEngine,
    document_id: &str,
    title: &str,
    template_id: &str,
    content: &str,
) -> Result<String> {
    let mut mem = Memory::new(
        MemoryType::Semantic,
        MemoryLayer::L3,
        format!("[document:{}] {}", title, content),
        SourceKind::AgentOutput,
    );
    mem.importance = 0.6;
    if let serde_json::Value::Object(ref mut map) = mem.metadata {
        map.insert(
            "document_id".to_string(),
            serde_json::Value::from(document_id),
        );
        map.insert(
            "template_id".to_string(),
            serde_json::Value::from(template_id),
        );
        map.insert("kind".to_string(), serde_json::Value::from("writing"));
    }
    let res = sponge.absorb(mem).await?;
    Ok(res.id().to_string())
}

/// Counts words using a Unicode-aware split.  CJK characters are
/// counted individually; Latin words are split on whitespace.
pub fn count_words(s: &str) -> usize {
    let mut n = 0usize;
    let mut in_latin = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            in_latin = false;
        } else if ch.is_ascii_alphanumeric() {
            if !in_latin {
                n += 1;
                in_latin = true;
            }
        } else {
            // CJK and other scripts: each character is a word.
            n += 1;
            in_latin = false;
        }
    }
    n
}

/// Estimates reading time at 300 wpm.  Returns whole minutes, minimum 1.
pub fn estimate_reading_minutes(word_count: usize) -> u32 {
    ((word_count as u32) / 300).max(1)
}

fn render_markdown(doc: &Document) -> String {
    // The body is already Markdown; the export is the body verbatim.
    // A future v1.0 renderer could attach front-matter here.
    let mut out = String::with_capacity(doc.content.len() + 64);
    out.push_str(&format!(
        "<!-- generated by nine-snake v0.5, template={} -->\n\n",
        doc.template_id
    ));
    out.push_str(&doc.content);
    out
}

fn render_html(doc: &Document) -> String {
    // Minimal, dependency-free Markdown → HTML for headings, bold,
    // italic, code blocks, and paragraphs.  Good enough for the
    // v0.5 export use-case; a real renderer (pulldown-cmark) is
    // planned for v1.0.
    let mut html = String::with_capacity(doc.content.len() * 2);
    html.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
    html.push_str(&format!("<title>{}</title>", escape_html(&doc.title)));
    html.push_str("<style>body{font-family:-apple-system,BlinkMacSystemFont,sans-serif;max-width:760px;margin:40px auto;padding:0 20px;line-height:1.7;color:#222}pre{background:#f4f4f4;padding:12px;border-radius:6px;overflow:auto}code{background:#f4f4f4;padding:1px 4px;border-radius:3px}</style>");
    html.push_str("</head><body>");
    html.push_str(&md_to_html(&doc.content));
    html.push_str("</body></html>");
    html
}

fn md_to_html(src: &str) -> String {
    let mut out = String::new();
    let mut in_code = false;
    let mut paragraph = String::new();
    for line in src.lines() {
        if line.trim_start().starts_with("```") {
            if in_code {
                out.push_str("</code></pre>");
                in_code = false;
            } else {
                flush_paragraph(&mut out, &mut paragraph);
                out.push_str("<pre><code>");
                in_code = true;
            }
            continue;
        }
        if in_code {
            out.push_str(&escape_html(line));
            out.push('\n');
            continue;
        }
        if line.starts_with("# ") {
            flush_paragraph(&mut out, &mut paragraph);
            out.push_str(&format!("<h1>{}</h1>", inline(&line[2..])));
        } else if line.starts_with("## ") {
            flush_paragraph(&mut out, &mut paragraph);
            out.push_str(&format!("<h2>{}</h2>", inline(&line[3..])));
        } else if line.starts_with("### ") {
            flush_paragraph(&mut out, &mut paragraph);
            out.push_str(&format!("<h3>{}</h3>", inline(&line[4..])));
        } else if line.starts_with("> ") {
            flush_paragraph(&mut out, &mut paragraph);
            out.push_str(&format!("<blockquote>{}</blockquote>", inline(&line[2..])));
        } else if line.trim().is_empty() {
            flush_paragraph(&mut out, &mut paragraph);
        } else {
            paragraph.push_str(line);
            paragraph.push('\n');
        }
    }
    if in_code {
        out.push_str("</code></pre>");
    }
    flush_paragraph(&mut out, &mut paragraph);
    out
}

fn flush_paragraph(out: &mut String, buf: &mut String) {
    if buf.trim().is_empty() {
        buf.clear();
        return;
    }
    out.push_str(&format!("<p>{}</p>", inline(buf.trim())));
    buf.clear();
}

fn inline(s: &str) -> String {
    // Order matters: code first (so we don't interpret backticks
    // inside `code`).
    let mut out = escape_html(s);
    // Toggle `code` between <code>...</code> spans.
    let mut result = String::with_capacity(out.len());
    let mut in_code = false;
    let mut buf = String::new();
    for ch in out.chars() {
        if ch == '`' {
            if in_code {
                result.push_str("<code>");
                result.push_str(&buf);
                result.push_str("</code>");
                buf.clear();
                in_code = false;
            } else {
                result.push_str(&buf);
                buf.clear();
                in_code = true;
            }
        } else if in_code {
            buf.push(ch);
        } else {
            result.push(ch);
        }
    }
    result.push_str(&buf);
    out = result;
    // Bold: **text** → <strong>text</strong>
    out = replace_pair(&out, "**", "<strong>", "</strong>");
    // Italic: *text* → <em>text</em>
    out = replace_pair(&out, "*", "<em>", "</em>");
    out
}

/// Replaces every opening marker with `<open>` and every closing
/// marker with `</close>`, alternating.  The implementation is
/// intentionally tolerant of unmatched markers (a stray `*` is
/// rendered literally).
fn replace_pair(s: &str, marker: &str, open: &str, close: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut inside = false;
    let mut buf = String::new();
    let bytes = s.as_bytes();
    let m = marker.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + m.len() <= bytes.len() && &bytes[i..i + m.len()] == m {
            if inside {
                out.push_str(&buf);
                out.push_str(close);
                buf.clear();
                inside = false;
            } else {
                out.push_str(&buf);
                out.push_str(open);
                buf.clear();
                inside = true;
            }
            i += m.len();
        } else {
            // Push the UTF-8 char at position i.  We need to handle
            // multi-byte characters; the safest path is to walk the
            // string as chars instead of bytes.
            let remaining = &s[i..];
            let ch = remaining.chars().next().unwrap();
            buf.push(ch);
            i += ch.len_utf8();
        }
    }
    out.push_str(&buf);
    out
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_words_handles_cjk_and_latin() {
        // "Hello world 你好世界" → 2 latin words + 4 CJK chars = 6
        assert_eq!(count_words("Hello world 你好世界"), 6);
    }

    #[test]
    fn count_words_empty() {
        assert_eq!(count_words(""), 0);
        assert_eq!(count_words("   \n  \t  "), 0);
    }

    #[test]
    fn reading_minutes_minimum_one() {
        assert_eq!(estimate_reading_minutes(0), 1);
        assert_eq!(estimate_reading_minutes(150), 1);
        assert_eq!(estimate_reading_minutes(300), 1);
        assert_eq!(estimate_reading_minutes(600), 2);
    }

    #[test]
    fn html_export_contains_doctype_and_title() {
        let doc = Document {
            id: "d1".into(),
            title: "Test & <Demo>".into(),
            template_id: "tech-blog".into(),
            content: "# Hello\n\nThis is a test.".into(),
            word_count: 5,
            memory_id: None,
            created_at: 0,
            updated_at: 0,
            metadata: None,
        };
        let html = render_html(&doc);
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("Test &amp; &lt;Demo&gt;"));
        assert!(html.contains("<h1>Hello</h1>"));
        assert!(html.contains("<p>This is a test.</p>"));
    }
}
