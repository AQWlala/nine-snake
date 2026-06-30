//! Memory flow integration test: store → search → compress → reflect.
//!
//! Validates the v0.2 happy path: a small corpus of L3 memories is
//! stored via the SqliteStore, "compressed" by toggling
//! `compressed_from`, and finally turned into an L5 reflection by the
//! reflection engine (with no real LLM attached, so the template
//! fallback path is exercised).

//! v0.3: shared helpers are declared once in the parent runner file
//! and accessed via `super::common`.

use nine_snake_lib::memory::migration;
use nine_snake_lib::memory::reflect::{ReflectConfig, ReflectionEngine};
use nine_snake_lib::memory::types::{Memory, MemoryLayer, MemoryType, SourceKind};

fn high_mem(id: &str, content: &str) -> Memory {
    let mut m = Memory::new(
        MemoryType::Semantic,
        MemoryLayer::L3,
        content,
        SourceKind::UserInput,
    );
    m.id = id.to_string();
    m.importance = 0.75;
    m
}

#[tokio::test]
async fn store_compress_reflect_pipeline() {
    let tmp = super::common::TmpStore::new();

    // 1. STORE — insert 4 high-importance L3 memories.
    let ids = ["m1", "m2", "m3", "m4"];
    for (i, id) in ids.iter().enumerate() {
        let m = high_mem(id, &format!("Tauri 启动问题 #{} — 端口/权限", i + 1));
        tmp.store.insert(&m).await.unwrap();
    }
    assert_eq!(tmp.store.count().await.unwrap(), 4);

    // 2. SEARCH-by-id (analog of Lance search) — all four are present
    //    in SQLite and unabsorbed.
    for id in &ids {
        let m = tmp.store.get(id).await.unwrap();
        assert!(m.is_some(), "memory {id} should be present");
    }

    // 3. COMPRESS — mark m1..m3 as compressed-from, leaving m4.
    tmp.store
        .update_compressed_from("m1", "summary-1")
        .await
        .unwrap();
    tmp.store
        .update_compressed_from("m2", "summary-1")
        .await
        .unwrap();
    tmp.store
        .update_compressed_from("m3", "summary-1")
        .await
        .unwrap();
    let remaining = tmp.store.list_recent(10).await.unwrap();
    let remaining_ids: Vec<String> = remaining.iter().map(|m| m.id.clone()).collect();
    assert_eq!(remaining_ids, vec!["m4".to_string()]);

    // 4. REFLECT — the template fallback should still produce a
    //    reflection on the one remaining high-importance memory.
    let engine = ReflectionEngine::new(tmp.store.clone(), None, ReflectConfig::default());
    let reflections = engine.reflect_now().await.unwrap();
    assert_eq!(reflections.len(), 1);
    assert_eq!(reflections[0].layer, MemoryLayer::L5);
    assert_eq!(reflections[0].memory_type, MemoryType::Metacognitive);
    assert_eq!(reflections[0].source_memories, vec!["m4".to_string()]);

    // 5. The reflection must be visible in migration_status as well
    //    as in the store's `reflections` table.
    let conn = tmp.store.raw_connection();
    let conn = conn.lock();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM reflections", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);

    let _ = migration::migration_status(&conn, migration::bundled_migrations_dir()).unwrap();
}
