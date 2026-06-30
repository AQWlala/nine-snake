//! Integration tests for the v0.5 Writing engine.
//!
//! Covers: template listing / application, document CRUD,
//! Markdown export, HTML export.  The L3 memory mirror is
//! best-effort and is exercised only when the SpongeEngine is
//! supplied; here we pass `None` to keep the test isolated from the
//! rest of the memory stack.

use super::common::TmpStore;
use nine_snake_lib::writing::{ExportFormat, WritingEngine};

fn engine(store: &TmpStore) -> WritingEngine {
    WritingEngine::new(store.store.clone(), None)
}

#[test]
fn template_library_is_not_empty() {
    let tmp = TmpStore::new();
    let eng = engine(&tmp);
    let lib = eng.list_templates();
    assert!(
        lib.len() >= 6,
        "expected at least 6 templates, got {}",
        lib.len()
    );
    // Tech blog is part of the v0.5 library.
    assert!(lib.iter().any(|t| t.id == "tech-blog"));
}

#[test]
fn apply_template_substitutes_placeholders() {
    let tmp = TmpStore::new();
    let eng = engine(&tmp);
    let mut values = std::collections::HashMap::new();
    values.insert("title".to_string(), "Hello".to_string());
    values.insert("summary".to_string(), "A short summary".to_string());
    values.insert("background".to_string(), "background text".to_string());
    let (title, body) = eng.apply_template("tech-blog", &values).expect("apply");
    assert_eq!(title, "Hello");
    assert!(body.contains("Hello"));
    assert!(!body.contains("{{title}}"));
}

#[test]
fn create_update_delete_document_lifecycle() {
    let tmp = TmpStore::new();
    let eng = engine(&tmp);
    let doc = eng
        .create_document(
            "Test doc".to_string(),
            "tech-blog".to_string(),
            "# Hello\n\nWorld".to_string(),
            None,
        )
        .expect("create");
    assert_eq!(doc.title, "Test doc");
    assert!(doc.word_count >= 2);

    let fetched = eng.get_document(&doc.id).expect("get").expect("exists");
    assert_eq!(fetched.id, doc.id);

    let updated = eng
        .update_document(&doc.id, "# Hello\n\nWorld\n\nMore content".to_string())
        .expect("update");
    assert!(updated.word_count > doc.word_count);

    let removed = eng.delete_document(&doc.id).expect("delete");
    assert!(removed);
    assert!(eng.get_document(&doc.id).expect("get").is_none());
}

#[test]
fn list_documents_orders_by_updated_at() {
    let tmp = TmpStore::new();
    let eng = engine(&tmp);
    let a = eng
        .create_document("A".into(), "blank".into(), "a".into(), None)
        .expect("a");
    let b = eng
        .create_document("B".into(), "blank".into(), "b".into(), None)
        .expect("b");
    eng.update_document(&a.id, "a v2".into()).expect("update a");
    let list = eng.list_documents(10).expect("list");
    assert!(list.len() >= 2);
    // The updated document should be first.
    assert_eq!(list[0].id, a.id);
    let _ = b;
}

#[test]
fn export_markdown_contains_body() {
    let tmp = TmpStore::new();
    let eng = engine(&tmp);
    let doc = eng
        .create_document("Hi".into(), "blank".into(), "body text".into(), None)
        .expect("create");
    let exp = eng.export(&doc.id, ExportFormat::Markdown).expect("export");
    assert!(exp.body.contains("body text"));
    assert!(exp.body.contains("template=blank"));
}

#[test]
fn export_html_is_a_full_document() {
    let tmp = TmpStore::new();
    let eng = engine(&tmp);
    let doc = eng
        .create_document(
            "Hi".into(),
            "blank".into(),
            "# Heading\n\nParagraph".into(),
            None,
        )
        .expect("create");
    let exp = eng.export(&doc.id, ExportFormat::Html).expect("export");
    assert!(exp.body.starts_with("<!doctype html>"));
    assert!(exp.body.contains("<h1>Heading</h1>"));
    assert!(exp.body.contains("<p>Paragraph</p>"));
}
