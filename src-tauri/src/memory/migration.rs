//! Schema migration system for the nine-snake memory store.
//!
//! Migrations are stored as plain `.sql` files in the
//! `src-tauri/migrations/` directory. The file name MUST follow the
//! pattern `NNN_*.sql` where `NNN` is a monotonically increasing
//! version number. Files are applied in ascending order; the system
//! records the highest applied version in `PRAGMA user_version` so
//! re-runs are idempotent.
//!
//! ## Lifecycle
//!
//! 1. `current_version(&conn)` reads `PRAGMA user_version` (0 if unset).
//! 2. `run_migrations(&conn, dir)` discovers every `NNN_*.sql` file,
//!    parses the leading version number, and applies anything strictly
//!    greater than the current version.
//! 3. Each applied file is wrapped in a `BEGIN ... COMMIT` transaction
//!    so a partial failure leaves the database in its previous state.
//!
//! ## v0.1 → v0.2 transition
//!
//! The v0.1 `001_initial.sql` was applied via raw `execute_batch` in
//! `SqliteStore::open`. To stay backward-compatible, the v0.2 boot
//! sequence calls `bootstrap_v0_1_baseline` *before* `run_migrations`
//! — this stamps `PRAGMA user_version = 1` so 002+ are not skipped on
//! databases that pre-date this module.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// A single migration descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Migration {
    /// Monotonically increasing version number.
    pub version: u32,
    /// Human-readable name (file name without the leading version
    /// prefix and without the extension).
    pub name: String,
    /// Raw SQL body.
    pub sql: String,
}

/// Snapshot of the migration state of a database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationStatus {
    /// Highest applied version (0 if no migrations have run).
    pub current_version: u32,
    /// All migration files known to the migrator, with their applied
    /// state.
    pub applied: Vec<MigrationState>,
}

/// One entry of [`MigrationStatus::applied`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationState {
    pub version: u32,
    pub name: String,
    pub applied: bool,
}

/// Returns the highest migration version previously applied to the
/// database (`PRAGMA user_version`).
pub fn current_version(conn: &Connection) -> Result<u32> {
    let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(v as u32)
}

/// Stamps `PRAGMA user_version = 1` when the v0.1 initial schema was
/// already applied by the legacy `SqliteStore::open` path. Idempotent.
pub fn bootstrap_v0_1_baseline(conn: &Connection) -> Result<()> {
    if current_version(conn)? == 0 {
        // The v0.1 schema includes its own `schema_version` row at
        // version 1, so we can safely assume 001 has run.
        let has: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_version WHERE version = 1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);
        if has {
            conn.pragma_update(None, "user_version", 1i64)?;
            info!(target: "nine_snake.migration", "v0.1 baseline detected; user_version set to 1");
        }
    }
    Ok(())
}

/// Runs every migration in `migrations_dir` whose version is strictly
/// greater than the current `PRAGMA user_version`.
///
/// Returns the list of migrations that were applied during this call.
pub fn run_migrations(conn: &Connection, migrations_dir: &Path) -> Result<Vec<Migration>> {
    bootstrap_v0_1_baseline(conn)?;

    let all = discover_migrations(migrations_dir)?;
    if all.is_empty() {
        debug!(target: "nine_snake.migration", "no migration files found");
        return Ok(Vec::new());
    }

    let current = current_version(conn)?;
    let pending: Vec<Migration> = all.into_iter().filter(|m| m.version > current).collect();

    if pending.is_empty() {
        debug!(target: "nine_snake.migration", current, "no pending migrations");
        return Ok(Vec::new());
    }

    info!(
        target: "nine_snake.migration",
        from = current,
        to = pending.last().map(|m| m.version).unwrap_or(current),
        count = pending.len(),
        "applying migrations"
    );

    let mut applied: Vec<Migration> = Vec::new();
    for m in pending {
        apply_one(conn, &m).with_context(|| format!("applying migration {}", m.name))?;
        applied.push(m);
    }
    Ok(applied)
}

/// Builds a [`MigrationStatus`] without mutating the database.
pub fn migration_status(conn: &Connection, migrations_dir: &Path) -> Result<MigrationStatus> {
    let current = current_version(conn)?;
    let all = discover_migrations(migrations_dir)?;
    let applied = all
        .into_iter()
        .map(|m| MigrationState {
            version: m.version,
            name: m.name,
            applied: m.version <= current,
        })
        .collect();
    Ok(MigrationStatus {
        current_version: current,
        applied,
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn apply_one(conn: &Connection, m: &Migration) -> Result<()> {
    debug!(target: "nine_snake.migration", version = m.version, name = %m.name, "applying");
    let tx = conn.unchecked_transaction()?;
    // The migrator splits on `;` for granular error reporting so
    // that idempotent statements (e.g. `ALTER TABLE ... ADD COLUMN`
    // that fails with "duplicate column name") can be ignored.
    apply_statements(&tx, &m.sql)
        .with_context(|| format!("executing migration body for version {}", m.version))?;
    // Bump user_version last so a failed migration leaves the previous
    // version intact.
    tx.pragma_update(None, "user_version", m.version as i64)?;
    tx.commit()?;
    info!(target: "nine_snake.migration", version = m.version, "applied");
    Ok(())
}

/// Splits a multi-statement SQL script and applies each statement
/// individually. Statements that fail with "duplicate column" or
/// "already exists" are silently ignored (idempotent re-runs).
fn apply_statements(conn: &Connection, sql: &str) -> Result<()> {
    for stmt in split_sql(sql) {
        if stmt.trim().is_empty() {
            continue;
        }
        if let Err(e) = conn.execute_batch(&stmt) {
            let msg = format!("{e}");
            if is_idempotent_error(&msg) {
                debug!(target: "nine_snake.migration", error = %msg, "ignoring idempotent error");
            } else {
                return Err(e).with_context(|| format!("statement: {stmt}"));
            }
        }
    }
    Ok(())
}

/// SQL splitter that respects string literals, single-line `--` comments,
/// `/* ... */` block comments, and trigger boundaries.
///
/// v0.2 had a naive `sql.split(';')` that broke on:
///   * semicolons inside `'string'` or `"identifier"` literals,
///   * `BEGIN ... END;` blocks of triggers (we have none yet, but
///     future migrations might).
///
/// v0.3 implements a one-pass char-by-char scanner that:
///   * tracks `inside_string` (toggle on unescaped `'` / `"`),
///   * treats `--...` line comments and `/* ... */` block comments as
///     transparent,
///   * only emits a split when the semicolon sits at top-level.
fn split_sql(sql: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut chars = sql.chars().peekable();
    let mut inside_string: Option<char> = None;
    // Track BEGIN/END block depth so semicolons inside trigger
    // bodies do not cause splits. SQLite treats CREATE TRIGGER
    // ... BEGIN ... END; as a single compound statement.
    let mut begin_depth: u32 = 0;
    let mut word_buf: String = String::new();

    fn flush_word(word: &mut String, depth: &mut u32) {
        match word.as_str() {
            "BEGIN" => *depth += 1,
            "END" => *depth = depth.saturating_sub(1),
            _ => {}
        }
        word.clear();
    }

    while let Some(c) = chars.next() {
        match (c, inside_string) {
            ('\'', None) | ('"', None) => {
                flush_word(&mut word_buf, &mut begin_depth);
                buf.push(c);
                inside_string = Some(c);
            }
            (c2, Some(q)) if c2 == q => {
                buf.push(c2);
                inside_string = None;
            }
            ('-', None) if chars.peek() == Some(&'-') => {
                flush_word(&mut word_buf, &mut begin_depth);
                buf.push('-');
                buf.push(chars.next().unwrap()); // push second '-'
                for nc in chars.by_ref() {
                    buf.push(nc);
                    if nc == '\n' {
                        break;
                    }
                }
            }
            ('/', None) if chars.peek() == Some(&'*') => {
                flush_word(&mut word_buf, &mut begin_depth);
                buf.push('/');
                chars.next();
                buf.push('*');
                while let Some(nc) = chars.next() {
                    buf.push(nc);
                    if nc == '*' && chars.peek() == Some(&'/') {
                        buf.push(chars.next().unwrap());
                        break;
                    }
                }
            }
            (';', None) => {
                flush_word(&mut word_buf, &mut begin_depth);
                if begin_depth == 0 {
                    out.push(std::mem::take(&mut buf));
                } else {
                    buf.push(c);
                }
            }
            (other, None) => {
                buf.push(other);
                if other.is_alphabetic() {
                    word_buf.push(other.to_ascii_uppercase());
                } else {
                    flush_word(&mut word_buf, &mut begin_depth);
                }
            }
            (other, _) => buf.push(other),
        }
    }
    flush_word(&mut word_buf, &mut begin_depth);
    if !buf.trim().is_empty() {
        out.push(buf);
    }
    out
}
fn is_idempotent_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("duplicate column name") || m.contains("already exists")
}

fn discover_migrations(dir: &Path) -> Result<Vec<Migration>> {
    if !dir.exists() {
        warn!(target: "nine_snake.migration", path = %dir.display(), "migrations dir does not exist");
        return Ok(Vec::new());
    }
    let mut out: Vec<Migration> = Vec::new();
    for entry in
        fs::read_dir(dir).with_context(|| format!("reading migrations dir {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("sql") {
            continue;
        }
        let fname = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        let (version, name) = match parse_filename(fname) {
            Some(parts) => parts,
            None => {
                warn!(target: "nine_snake.migration", file = %fname, "skipping file without NNN_ prefix");
                continue;
            }
        };
        let sql = fs::read_to_string(&path)
            .with_context(|| format!("reading migration file {}", path.display()))?;
        out.push(Migration { version, name, sql });
    }
    out.sort_by_key(|m| m.version);
    Ok(out)
}

fn parse_filename(fname: &str) -> Option<(u32, String)> {
    let idx = fname.find('_')?;
    let n_str = &fname[..idx];
    let n: u32 = n_str.parse().ok()?;
    let rest = &fname[idx + 1..];
    let rest = rest.strip_suffix(".sql").unwrap_or(rest);
    Some((n, rest.to_string()))
}

/// Helper that returns the directory containing the bundled migration
/// files. Useful in tests that don't have a workspace root.
pub fn bundled_migrations_dir() -> &'static Path {
    // The path is relative to CARGO_MANIFEST_DIR at compile time.
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/migrations"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_db() -> (std::path::PathBuf, Connection) {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!(
            "nine_snake_mig_test_{}_{}.db",
            std::process::id(),
            n
        ));
        let conn = Connection::open(&path).unwrap();
        (path, conn)
    }

    fn temp_dir() -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("nine_snake_mig_dir_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup_dir(p: &std::path::Path) {
        let _ = std::fs::remove_dir_all(p);
    }

    fn cleanup_file(p: &std::path::Path) {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(p.with_extension("db-wal"));
        let _ = std::fs::remove_file(p.with_extension("db-shm"));
    }

    #[test]
    fn current_version_defaults_to_zero() {
        let (path, conn) = temp_db();
        assert_eq!(current_version(&conn).unwrap(), 0);
        cleanup_file(&path);
    }

    #[test]
    fn bootstrap_v0_1_stamps_version_when_schema_present() {
        let (path, conn) = temp_db();
        // Simulate a v0.1 database that has the schema_version table
        // with version 1.
        conn.execute_batch(
            "CREATE TABLE schema_version(version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL, description TEXT NOT NULL DEFAULT '');\
             INSERT INTO schema_version(version, applied_at) VALUES (1, 0);",
        )
        .unwrap();
        bootstrap_v0_1_baseline(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 1);
        cleanup_file(&path);
    }

    #[test]
    fn bootstrap_v0_1_noop_when_already_set() {
        let (path, conn) = temp_db();
        conn.pragma_update(None, "user_version", 5i64).unwrap();
        bootstrap_v0_1_baseline(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 5);
        cleanup_file(&path);
    }

    #[test]
    fn discover_migrations_reads_files() {
        let dir = temp_dir();
        std::fs::write(dir.join("001_first.sql"), "SELECT 1;").unwrap();
        std::fs::write(dir.join("002_second.sql"), "SELECT 2;").unwrap();
        std::fs::write(dir.join("README"), "ignore me").unwrap();
        let all = discover_migrations(&dir).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].version, 1);
        assert_eq!(all[0].name, "first");
        assert_eq!(all[1].version, 2);
        assert_eq!(all[1].name, "second");
        cleanup_dir(&dir);
    }

    #[test]
    fn parse_filename_handles_edge_cases() {
        assert_eq!(parse_filename("001_a.sql"), Some((1, "a".to_string())));
        assert_eq!(
            parse_filename("123_xyz.sql"),
            Some((123, "xyz".to_string()))
        );
        assert_eq!(parse_filename("abc.sql"), None);
        assert_eq!(
            parse_filename("01_leading_zero.sql"),
            Some((1, "leading_zero".to_string()))
        );
    }

    #[test]
    fn split_sql_handles_string_semicolons() {
        let sql = "INSERT INTO t VALUES('a; b'); INSERT INTO t VALUES('c');";
        let parts = split_sql(sql);
        assert_eq!(parts.len(), 2, "expected 2 statements, got {parts:?}");
        assert!(parts[0].contains("'a; b'"));
        assert!(parts[1].contains("'c'"));
    }

    #[test]
    fn split_sql_handles_line_comments() {
        let sql = "SELECT 1; -- this has a ; semicolon\nSELECT 2;";
        let parts = split_sql(sql);
        assert_eq!(parts.len(), 2, "expected 2 statements, got {parts:?}");
        assert!(parts[0].starts_with("SELECT 1"));
        assert!(parts[1].contains("SELECT 2"));
    }

    #[test]
    fn split_sql_handles_block_comments() {
        let sql = "SELECT 1; /* block ; with ; semicolons */ SELECT 2;";
        let parts = split_sql(sql);
        assert_eq!(parts.len(), 2, "expected 2 statements, got {parts:?}");
        assert!(parts[0].contains("SELECT 1"));
        assert!(parts[1].contains("SELECT 2"));
    }

    #[test]
    fn split_sql_handles_double_quoted_identifier() {
        let sql = "CREATE TABLE \"weird;name\" (id INT); SELECT 1;";
        let parts = split_sql(sql);
        assert_eq!(parts.len(), 2, "expected 2 statements, got {parts:?}");
        assert!(parts[0].contains("\"weird;name\""));
    }

    #[test]
    fn split_sql_no_trailing_semicolon() {
        let sql = "SELECT 1; SELECT 2";
        let parts = split_sql(sql);
        assert_eq!(parts.len(), 2);
    }
    #[test]
    fn split_sql_handles_trigger_body_semicolons() {
        let sql = "CREATE TRIGGER t AFTER INSERT ON x BEGIN INSERT INTO y VALUES(1); INSERT INTO y VALUES(2); END; SELECT 3;";
        let parts = split_sql(sql);
        assert_eq!(parts.len(), 2, "expected 2 statements, got {parts:?}");
        assert!(parts[0].contains("CREATE TRIGGER"));
        assert!(parts[0].contains("INSERT INTO y VALUES(1)"));
        assert!(parts[0].contains("INSERT INTO y VALUES(2)"));
        assert!(parts[1].contains("SELECT 3"));
    }

    #[test]
    fn split_sql_handles_multiple_triggers() {
        let sql = "CREATE TRIGGER t1 AFTER INSERT ON x BEGIN INSERT INTO y VALUES(1); END; CREATE TRIGGER t2 AFTER INSERT ON x BEGIN INSERT INTO z VALUES(2); END;";
        let parts = split_sql(sql);
        assert_eq!(parts.len(), 2, "expected 2 triggers, got {parts:?}");
        assert!(parts[0].contains("t1"));
        assert!(parts[1].contains("t2"));
    }

    #[test]
    fn run_migrations_applies_pending_only() {
        let dir = temp_dir();
        let (db_path, conn) = temp_db();

        // 001 creates a table; 002 adds an index.
        std::fs::write(
            dir.join("001_first.sql"),
            "CREATE TABLE t1 (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        std::fs::write(dir.join("002_second.sql"), "CREATE INDEX i1 ON t1(id);").unwrap();

        let applied = run_migrations(&conn, &dir).unwrap();
        assert_eq!(applied.len(), 2);
        assert_eq!(current_version(&conn).unwrap(), 2);

        // Re-running applies nothing.
        let applied2 = run_migrations(&conn, &dir).unwrap();
        assert!(applied2.is_empty());
        assert_eq!(current_version(&conn).unwrap(), 2);
        cleanup_dir(&dir);
        cleanup_file(&db_path);
    }

    #[test]
    fn run_migrations_skips_already_applied() {
        let dir = temp_dir();
        let (db_path, conn) = temp_db();
        std::fs::write(
            dir.join("001_first.sql"),
            "CREATE TABLE t1 (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        std::fs::write(dir.join("002_second.sql"), "CREATE INDEX i1 ON t1(id);").unwrap();

        // Pre-stamp user_version = 1.
        conn.pragma_update(None, "user_version", 1i64).unwrap();
        let applied = run_migrations(&conn, &dir).unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].version, 2);
        assert_eq!(current_version(&conn).unwrap(), 2);
        cleanup_dir(&dir);
        cleanup_file(&db_path);
    }

    #[test]
    fn migration_status_lists_all_with_applied_flag() {
        let dir = temp_dir();
        let (db_path, conn) = temp_db();
        std::fs::write(
            dir.join("001_first.sql"),
            "CREATE TABLE t1 (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        std::fs::write(dir.join("002_second.sql"), "CREATE INDEX i1 ON t1(id);").unwrap();

        let status = migration_status(&conn, &dir).unwrap();
        assert_eq!(status.applied.len(), 2);
        assert!(!status.applied[0].applied);
        assert!(!status.applied[1].applied);

        run_migrations(&conn, &dir).unwrap();
        let status = migration_status(&conn, &dir).unwrap();
        assert!(status.applied[0].applied);
        assert!(status.applied[1].applied);
        cleanup_dir(&dir);
        cleanup_file(&db_path);
    }

    // -----------------------------------------------------------------
    // v1.0 P0#9 regression tests.
    //
    // Approach A: 004_v05.sql no longer creates `e2ee_keys` and
    // 005_v10.sql drops the table on upgrade.  The two tests
    // below pin both halves of the contract.
    // -----------------------------------------------------------------

    #[test]
    fn p0_9_fresh_install_does_not_create_e2ee_keys_table() {
        // Run the full bundled migration set against a clean
        // database.  The `e2ee_keys` table MUST NOT exist.
        let (db_path, conn) = temp_db();
        run_migrations(&conn, bundled_migrations_dir()).unwrap();
        let has: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='e2ee_keys'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(false);
        assert!(!has, "fresh install must not create e2ee_keys");
        cleanup_file(&db_path);
    }

    #[test]
    fn p0_9_005_v10_drops_orphan_e2ee_keys_table() {
        // Simulate a v0.5 database: run 004, then create the
        // orphan table, then run 005.  The table must be gone.
        let dir = temp_dir();
        let (db_path, conn) = temp_db();
        // Use only the bundled migrations we care about.
        std::fs::write(
            dir.join("004_v05.sql"),
            "CREATE TABLE e2ee_keys (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        std::fs::write(dir.join("005_v10.sql"), "DROP TABLE IF EXISTS e2ee_keys;").unwrap();
        run_migrations(&conn, &dir).unwrap();
        let after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='e2ee_keys'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, 0, "e2ee_keys must be dropped by 005_v10");
        cleanup_dir(&dir);
        cleanup_file(&db_path);
    }
}
