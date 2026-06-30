//! Integration tests for the v0.3 skills subsystem.

use std::collections::HashMap;
use std::sync::Arc;

use nine_snake_lib::llm::{LlmGateway, OllamaClient};
use nine_snake_lib::memory::sqlite_store::SqliteStore;
use nine_snake_lib::skills::engine::SkillEngine;
use nine_snake_lib::skills::store::SkillStore;
use nine_snake_lib::skills::types as skill_types;

fn temp_sqlite() -> (std::path::PathBuf, Arc<SqliteStore>) {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "nine_snake_skills_test_{}.db",
        uuid::Uuid::new_v4()
    ));
    let sqlite = Arc::new(SqliteStore::open(&p).unwrap());
    {
        let conn = sqlite.raw_connection();
        let g = conn.lock();
        nine_snake_lib::memory::migration::run_migrations(
            &g,
            nine_snake_lib::memory::migration::bundled_migrations_dir(),
        )
        .unwrap();
    }
    (p, sqlite)
}

fn llm() -> Arc<LlmGateway> {
    let client = Arc::new(OllamaClient::new("http://127.0.0.1:1"));
    Arc::new(LlmGateway::new(client, "m", None, None, None))
}

fn cleanup(p: &std::path::Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(p.with_extension("db-wal"));
    let _ = std::fs::remove_file(p.with_extension("db-shm"));
}

#[test]
fn create_skill_round_trips_through_engine() {
    let (p, sqlite) = temp_sqlite();
    let eng = SkillEngine::new(sqlite, llm());

    let created = eng
        .create_skill(skill_types::CreateSkillRequest {
            name: "greet".to_string(),
            description: "say hello".to_string(),
            code: "print('hello')".to_string(),
            language: "python".to_string(),
            tags: vec!["demo".to_string()],
            source_memory_id: None,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(created.name, "greet");
    assert!(created.created_at > 0);
    assert!(created.updated_at > 0);

    let listed = eng
        .list_skills(skill_types::ListSkillsRequest {
            language: Some("python".to_string()),
            tag: None,
            limit: 10,
        })
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.id);

    cleanup(&p);
}

#[test]
fn rate_skill_accumulates_with_weighted_average() {
    let (p, sqlite) = temp_sqlite();
    let eng = SkillEngine::new(sqlite, llm());

    let s = eng
        .create_skill(skill_types::CreateSkillRequest {
            name: "rated".to_string(),
            description: "x".to_string(),
            code: "x".to_string(),
            language: "rust".to_string(),
            tags: vec![],
            source_memory_id: None,
            ..Default::default()
        })
        .unwrap();

    eng.rate_skill(skill_types::RateSkillRequest {
        id: s.id.clone(),
        rating: 5.0,
    })
    .unwrap();
    eng.rate_skill(skill_types::RateSkillRequest {
        id: s.id.clone(),
        rating: 1.0,
    })
    .unwrap();
    let got = eng
        .list_skills(skill_types::ListSkillsRequest {
            language: None,
            tag: None,
            limit: 10,
        })
        .unwrap()
        .into_iter()
        .find(|x| x.id == s.id)
        .unwrap();
    assert_eq!(got.rating_count, 2);
    assert!(
        (got.avg_rating - 3.0).abs() < 1e-6,
        "got avg={}",
        got.avg_rating
    );

    cleanup(&p);
}

#[test]
fn skill_search_finds_by_name_and_tag() {
    let (p, sqlite) = temp_sqlite();
    let eng = SkillEngine::new(sqlite, llm());

    eng.create_skill(skill_types::CreateSkillRequest {
        name: "palindrome".into(),
        description: "x".into(),
        code: "x".into(),
        language: "rust".into(),
        tags: vec!["string".into()],
        source_memory_id: None,
        ..Default::default()
    })
    .unwrap();
    eng.create_skill(skill_types::CreateSkillRequest {
        name: "fibonacci".into(),
        description: "x".into(),
        code: "x".into(),
        language: "rust".into(),
        tags: vec!["math".into()],
        source_memory_id: None,
        ..Default::default()
    })
    .unwrap();

    let hits = eng
        .search_skills(skill_types::SkillSearchRequest {
            query: "palindrome".to_string(),
            limit: 10,
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "palindrome");

    cleanup(&p);
}

#[tokio::test]
async fn use_skill_with_unknown_language_returns_friendly_error() {
    let (p, sqlite) = temp_sqlite();
    let eng = SkillEngine::new(sqlite, llm());

    let s = eng
        .create_skill(skill_types::CreateSkillRequest {
            name: "rusty".into(),
            description: "x".into(),
            code: "fn main(){}".into(),
            language: "rust".into(),
            tags: vec![],
            source_memory_id: None,
            ..Default::default()
        })
        .unwrap();

    let res = eng
        .use_skill(skill_types::UseSkillRequest {
            id: s.id,
            params: HashMap::new(),
        })
        .await;
    let err = res.expect_err("expected error for unsupported shell language");
    let msg = format!("{err}");
    assert!(
        msg.contains("not supported") || msg.contains("v0.5"),
        "unexpected error: {msg}"
    );

    cleanup(&p);
}

#[test]
fn skill_store_count_increments_with_inserts() {
    let (p, sqlite) = temp_sqlite();
    let store = SkillStore::new((*sqlite).clone()).unwrap();
    let before = store.count().unwrap();
    for i in 0..5 {
        let mut s = skill_types::Skill {
            id: format!("s-{i}"),
            name: format!("s{i}"),
            description: String::new(),
            code: String::new(),
            language: "rust".to_string(),
            tags: vec![],
            usage_count: 0,
            avg_rating: 0.0,
            rating_count: 0,
            created_at: 0,
            updated_at: 0,
            source_memory_id: None,
            activation_condition: None,
            platform: None,
            min_confidence: None,
        };
        s.code = "x".to_string();
        store.insert(&s).unwrap();
    }
    assert_eq!(store.count().unwrap() - before, 5);
    cleanup(&p);
}
