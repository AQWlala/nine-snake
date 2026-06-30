//! Bootstrap → health check integration test.
//!
//! Verifies the v0.2 boot path:
//!   1. `AppConfig::from_env` (with overrides) returns a usable config.
//!   2. `SqliteStore::open` succeeds against a tempdir.
//!   3. `migration::run_migrations` applies 002 on a fresh database.
//!   4. `migration::migration_status` reports `current_version = 2`.

//! v0.3: shared helpers are declared once in the parent runner file
//! and accessed via `super::common`.

use nine_snake_lib::memory::migration;

#[test]
fn bootstrap_health_check() {
    let tmp = super::common::TmpStore::new();

    // Schema must include both the v0.1 baseline tables and the v0.2
    // `memory_reflections` table.
    let conn = tmp.store.raw_connection();
    let conn = conn.lock();
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert!(
        v >= 2,
        "user_version must be ≥ 2 after run_migrations, got {v}"
    );

    let has_reflections: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='reflections'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_reflections, 1, "reflections table missing");

    let has_join: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_reflections'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has_join, 1, "memory_reflections table missing");

    let status = migration::migration_status(&conn, migration::bundled_migrations_dir()).unwrap();
    assert!(status.current_version >= 2);
    assert!(status.applied.iter().any(|m| m.version == 2 && m.applied));
}

#[test]
fn bootstrap_is_idempotent() {
    // Re-running run_migrations on a database that already has 002
    // applied must apply nothing and bump nothing.
    let tmp = super::common::TmpStore::new();
    let conn = tmp.store.raw_connection();
    let conn = conn.lock();
    let applied = migration::run_migrations(&conn, migration::bundled_migrations_dir()).unwrap();
    assert!(
        applied.is_empty(),
        "expected no new migrations, got {applied:?}"
    );
}
