//! P0#8 regression test: `documents.memory_id` is a real foreign key
//! with `ON DELETE SET NULL` semantics.
//!
//! Verifies that:
//!   1. Migration 006 is applied at boot (the constraint is
//!      visible via `PRAGMA foreign_key_list(documents)`).
//!   2. Inserting a document with a `memory_id` that does not exist
//!      is **rejected** (FK enforcement is on, not a no-op).
//!   3. Deleting the parent memory row sets the document's
//!      `memory_id` to NULL (cascade behaviour) instead of leaving
//!      a dangling reference.

use nine_snake_lib::memory::migration;

#[test]
fn documents_memory_id_is_real_foreign_key() {
    let tmp = super::common::TmpStore::new();

    // 1) The constraint must be registered. SQLite's
    //    ALTER TABLE ... ADD CONSTRAINT does NOT rewrite the
    //    `sql` column of sqlite_master, so we check via
    //    `pragma foreign_key_list` which is the canonical
    //    source of truth for the live FKs on a table.
    //
    //    `PRAGMA foreign_key_list(table)` returns one row per
    //    FK with columns: id, seq, table, from, to, on_update,
    //    on_delete, match. Both action columns are TEXT
    //    ("NO ACTION", "SET NULL", "CASCADE", etc.).
    let conn = tmp.store.raw_connection();
    let conn = conn.lock();
    let mut stmt = conn
        .prepare("PRAGMA foreign_key_list(documents)")
        .expect("prepare foreign_key_list");
    let fks: Vec<(
        i64,
        i64,
        String,
        String,
        Option<String>,
        String,
        String,
        String,
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            ))
        })
        .expect("query foreign_key_list")
        .filter_map(|r| r.ok())
        .collect();
    assert!(
        !fks.is_empty(),
        "documents has no FK registered — migration 006 did not run or failed"
    );
    let memory_fk = fks
        .iter()
        .find(|fk| fk.3 == "memory_id" && fk.2 == "memories");
    assert!(
        memory_fk.is_some(),
        "documents.memory_id → memories.id FK missing (found FKs: {fks:#?})"
    );
    // ON DELETE SET NULL is column index 6 in
    // `foreign_key_list` (the "on_delete" action).
    let on_delete = &memory_fk.unwrap().6;
    assert_eq!(
        on_delete, "SET NULL",
        "expected ON DELETE SET NULL, got {on_delete:?}"
    );
    drop(stmt);
    drop(conn);

    // 2) Insert a parent memory (use a direct row so we can delete
    //    it without going through the full Sponge plumbing).
    let mem_id = "mem-p0-8-target";
    tmp.store
        .raw_connection()
        .lock()
        .execute(
            "INSERT INTO memories
                (id, memory_type, layer, content, last_access, created_at)
             VALUES (?1, 'Semantic', 'L3', 'p0#8 fixture', 0, 0)",
            [mem_id],
        )
        .expect("insert parent memory");

    // 3) Insert a document pointing at that memory. This must
    //    succeed.
    let doc_id = "doc-p0-8-child";
    tmp.store
        .raw_connection()
        .lock()
        .execute(
            "INSERT INTO documents
                (id, title, template_id, content, memory_id, created_at, updated_at)
             VALUES (?1, 'p0#8', 'tech-blog', 'body', ?2, 0, 0)",
            rusqlite::params![doc_id, mem_id],
        )
        .expect("insert child document");

    // 4) FK enforcement must be ON for this connection (or for the
    //    connection the runtime uses).
    let fk_on: i64 = tmp
        .store
        .raw_connection()
        .lock()
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(fk_on, 1, "PRAGMA foreign_keys must be ON");

    // 5) Inserting a document with a non-existent memory_id must be
    //    rejected.
    let bad_insert = tmp.store.raw_connection().lock().execute(
        "INSERT INTO documents
            (id, title, template_id, content, memory_id, created_at, updated_at)
         VALUES ('doc-bad', 'bad', 'tech-blog', 'x', 'no-such-mem', 0, 0)",
        [],
    );
    assert!(
        bad_insert.is_err(),
        "FK should reject documents.memory_id pointing at a non-existent memory"
    );

    // 6) Deleting the parent memory must cascade-set-NULL on the
    //    child document, NOT remove the document row.
    tmp.store
        .raw_connection()
        .lock()
        .execute("DELETE FROM memories WHERE id = ?1", [mem_id])
        .expect("delete parent memory");

    let (still_present, mem_after): (i64, Option<String>) = tmp
        .store
        .raw_connection()
        .lock()
        .query_row(
            "SELECT 1, memory_id FROM documents WHERE id = ?1",
            [doc_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("document row survives the cascade");
    assert_eq!(still_present, 1, "document row should be preserved");
    assert!(
        mem_after.is_none(),
        "documents.memory_id should be NULL after parent delete, got {mem_after:?}"
    );
}

#[test]
fn migration_006_is_discovered_and_idempotent() {
    let tmp = super::common::TmpStore::new();
    let conn = tmp.store.raw_connection();
    let conn = conn.lock();

    // The migrator must surface 006 in the status list.
    let status = migration::migration_status(&conn, migration::bundled_migrations_dir())
        .expect("migration_status");
    let has_006 = status
        .applied
        .iter()
        .any(|m| m.version == 6 && m.name.contains("documents_fk"));
    assert!(has_006, "migration 006 not in status: {:?}", status.applied);

    // Re-running applies nothing (idempotent).
    let applied = migration::run_migrations(&conn, migration::bundled_migrations_dir())
        .expect("re-run migrations");
    assert!(
        applied.is_empty(),
        "re-run should be a no-op, applied: {applied:?}"
    );
}
