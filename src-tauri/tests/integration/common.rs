//! Integration test shared helpers.
//!
//! The v0.3 integration suite (`tests/integration/`) needs to spin up
//! a real `SqliteStore` + `LanceStore` in a temporary directory. The
//! helpers here centralise the boilerplate so each test can be a
//! straight-line sequence of assertions.
//!
//! v0.3 layout: the file lives at `tests/integration/common.rs` (no
//! nested `common/` directory) and is included with `mod common;` per
//! the modern Rust integration-test convention. This is what
//! `tests/integration.rs` does at the top level.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::TempDir;

use nine_snake_lib::memory::sqlite_store::SqliteStore;

/// A pair of (tempdir, sqlite store) used by the integration tests.
/// Holding onto `TempDir` keeps the directory alive for the duration
/// of the test.
pub struct TmpStore {
    pub dir: TempDir,
    pub db_path: PathBuf,
    pub store: Arc<SqliteStore>,
}

impl TmpStore {
    /// Opens a fresh store and runs the v0.2 migrations on top of the
    /// v0.1 baseline schema.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("create tempdir");
        let db_path = dir.path().join("nine_snake_test.db");
        let store = Arc::new(SqliteStore::open(&db_path).expect("open sqlite"));
        // Apply v0.2 migrations on top of the v0.1 baseline.
        {
            let conn = store.raw_connection();
            let conn = conn.lock();
            nine_snake_lib::memory::migration::run_migrations(
                &conn,
                nine_snake_lib::memory::migration::bundled_migrations_dir(),
            )
            .expect("apply migrations");
        }
        Self {
            dir,
            db_path,
            store,
        }
    }

    /// Returns the path to a fresh tempdir that the caller can use
    /// for a Lance store, etc.
    pub fn child_dir(&self, name: &str) -> PathBuf {
        let p = self.dir.path().join(name);
        std::fs::create_dir_all(&p).expect("create child dir");
        p
    }
}
