//! Reflection engine integration test.
//!
//! End-to-end: seed the store with a few high-importance memories,
//! run the engine, and assert that a reflection row + join rows land
//! in the database. The LLM is not configured, so the template
//! fallback path is exercised.

//! v0.3: shared helpers are declared once in the parent runner file
//! and accessed via `super::common`.

use nine_snake_lib::memory::reflect::{ReflectConfig, ReflectionEngine};
use nine_snake_lib::memory::types::{Memory, MemoryLayer, MemoryType, SourceKind};

fn high(id: &str, content: &str) -> Memory {
    let mut m = Memory::new(
        MemoryType::Semantic,
        MemoryLayer::L3,
        content,
        SourceKind::UserInput,
    );
    m.id = id.to_string();
    m.importance = 0.8;
    m
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reflection_round_trip_through_database() {
    let tmp = super::common::TmpStore::new();

    for (i, c) in ["alpha", "beta", "gamma"].iter().enumerate() {
        let id = format!("seed-{i}");
        let mut m = high(&id, c);
        m.importance = 0.9;
        tmp.store.insert(&m).await.unwrap();
    }

    let engine = ReflectionEngine::new(tmp.store.clone(), None, ReflectConfig::default());
    let reflections = engine.reflect_now().await.unwrap();
    assert_eq!(reflections.len(), 1);
    let r = &reflections[0];
    assert_eq!(r.layer, MemoryLayer::L5);
    assert_eq!(r.memory_type, MemoryType::Metacognitive);
    assert_eq!(r.source_memories.len(), 3);

    // Persistence: the reflection + join rows must be readable.
    let conn = tmp.store.raw_connection();
    let conn = conn.lock();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM reflections WHERE id = ?1",
            rusqlite::params![r.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
    let joins: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_reflections WHERE reflection_id = ?1",
            rusqlite::params![r.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(joins, 3);

    // List path: round-trip via the engine.
    let listed = engine.list_recent(10).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, r.id);
    assert_eq!(listed[0].source_memories.len(), 3);
}

#[tokio::test]
async fn reflection_with_no_candidates_is_empty() {
    let tmp = super::common::TmpStore::new();
    let engine = ReflectionEngine::new(tmp.store.clone(), None, ReflectConfig::default());
    let r = engine.reflect_now().await.unwrap();
    assert!(r.is_empty());
}
