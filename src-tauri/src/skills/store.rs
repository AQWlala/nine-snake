//! SQLite-backed CRUD over the `skills` table.
//!
//! The v0.1 schema reserved a `skills` table but no API ever wrote to
//! it. v0.3 promotes it to a first-class subsystem: this module
//! provides typed insert/get/list/rate primitives that the
//! [`SkillEngine`](crate::skills::engine::SkillEngine) and the
//! Tauri/gRPC command layers share.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, Row};
use tracing::debug;

use super::types::Skill;
use crate::memory::sqlite_store::SqliteStore;

/// Thread-safe CRUD wrapper for the `skills` table.
#[derive(Clone)]
pub struct SkillStore {
    conn: Arc<Mutex<Connection>>,
}

impl SkillStore {
    /// Opens (or re-uses) the `skills` table on the given [`SqliteStore`].
    /// The store is shared with the rest of the system so the
    /// connection + WAL mode are reused.
    pub fn new(sqlite: SqliteStore) -> Result<Self> {
        let conn = sqlite.raw_connection();
        // Quick sanity: the v0.1 schema must already include the
        // `skills` table. We don't re-run migrations here — the
        // bootstrap pipeline handles that.
        {
            let g = conn.lock();
            let present: bool = g
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skills'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .map(|n| n > 0)
                .unwrap_or(false);
            if !present {
                return Err(anyhow!(
                    "skills table not initialised; run migrations first"
                ));
            }
        }
        Ok(Self { conn })
    }

    /// Convenience for tests: opens a fresh DB file with the v0.1 +
    /// v0.2 + v0.3 migrations applied.
    pub fn open_test<P: AsRef<Path>>(path: P) -> Result<Self> {
        let sqlite = SqliteStore::open(&path)?;
        let conn = sqlite.raw_connection();
        {
            let g = conn.lock();
            crate::memory::migration::run_migrations(
                &g,
                crate::memory::migration::bundled_migrations_dir(),
            )?;
        }
        Self::new(sqlite)
    }

    /// Inserts a new skill. `now` is used as both `created_at` and
    /// `updated_at`. Returns the stored row.
    pub fn insert(&self, s: &Skill) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let tags_json = serde_json::to_string(&s.tags).unwrap_or_else(|_| "[]".to_string());
        let activation_json = s
            .activation_condition
            .as_ref()
            .map(|a| serde_json::to_string(a).unwrap_or_default());
        let platform_json = s
            .platform
            .as_ref()
            .map(|p| serde_json::to_string(p).unwrap_or_default());
        let g = self.conn.lock();
        // FK integrity: skills.memory_id REFERENCES memories(id).
        // When source_memory_id is None, create a minimal placeholder
        // memory so the FK constraint is satisfied.
        let memory_id = s.source_memory_id.clone().unwrap_or_else(|| s.id.clone());
        if s.source_memory_id.is_none() {
            g.execute(
                "INSERT OR IGNORE INTO memories (id, memory_type, layer, content, last_access, created_at) VALUES (?1, 'Procedural', 'L3', '', ?2, ?2)",
                params![memory_id, now],
            )?;
        }
        g.execute(
            "INSERT INTO skills
                (id, memory_id, name, description, steps, trigger,
                 success_count, failure_count, last_used, created_at,
                 code, language, tags, usage_count, avg_rating,
                 rating_count, updated_at, source_memory_id,
                 activation_condition, platform, min_confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                     ?7, ?8, ?9, ?10,
                     ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, ?21)",
            params![
                s.id,
                memory_id,
                s.name,
                s.description,
                "[]",
                "",
                0,
                0,
                0,
                now,
                s.code,
                s.language,
                tags_json,
                s.usage_count,
                s.avg_rating,
                s.rating_count,
                now,
                s.source_memory_id,
                activation_json,
                platform_json,
                s.min_confidence,
            ],
        )?;
        debug!(target: "nine_snake.skills", id = %s.id, name = %s.name, "inserted skill");
        Ok(())
    }

    /// Fetches a skill by id.
    pub fn get(&self, id: &str) -> Result<Option<Skill>> {
        let g = self.conn.lock();
        g.query_row(
            "SELECT id, name, description, code, language, tags,
                    usage_count, avg_rating, rating_count,
                    created_at, updated_at, source_memory_id,
                    activation_condition, platform, min_confidence
             FROM skills WHERE id = ?1",
            params![id],
            row_to_skill,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Lists skills, optionally filtered by language and tag.
    pub fn list(
        &self,
        language: Option<&str>,
        tag: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Skill>> {
        let g = self.conn.lock();
        // Build a dynamic WHERE clause. The `tags` column stores a
        // JSON array; we use `like '%"tag"%' as a cheap, index-free
        // filter — fine for the marketplace scale we expect.
        let mut sql = String::from(
            "SELECT id, name, description, code, language, tags,
                    usage_count, avg_rating, rating_count,
                    created_at, updated_at, source_memory_id,
                    activation_condition, platform, min_confidence
             FROM skills WHERE 1=1",
        );
        if language.is_some() {
            sql.push_str(" AND language = ?");
        }
        if tag.is_some() {
            sql.push_str(" AND tags LIKE ?");
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?");
        let mut stmt = g.prepare(&sql)?;
        let lang_p = language.map(|s| s.to_string());
        let tag_p = tag.map(|s| format!("\"{}\"", s));
        let lim_p = limit.max(1) as i64;
        let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::new();
        if let Some(ref s) = lang_p {
            params_vec.push(s as &dyn rusqlite::ToSql);
        }
        if let Some(ref s) = tag_p {
            params_vec.push(s as &dyn rusqlite::ToSql);
        }
        params_vec.push(&lim_p);
        let rows = stmt
            .query_map(params_vec.as_slice(), row_to_skill)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Atomically updates `usage_count`, `avg_rating` and
    /// `rating_count`. The new `avg_rating` is computed as a
    /// weighted average: `(old_avg * old_count + new_rating) /
    /// (old_count + 1)`.
    pub fn rate(&self, id: &str, rating: f32) -> Result<Skill> {
        let now = chrono::Utc::now().timestamp();
        let g = self.conn.lock();
        let tx = g.unchecked_transaction()?;
        // Insert the raw rating first.
        tx.execute(
            "INSERT INTO skill_ratings(skill_id, rating, created_at) VALUES (?1, ?2, ?3)",
            params![id, rating, now],
        )?;
        let (old_count, old_avg): (i64, f64) = tx
            .query_row(
                "SELECT rating_count, avg_rating FROM skills WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("skill not found: {id}"))?;
        let new_count = old_count + 1;
        let new_avg = if old_count == 0 {
            rating as f64
        } else {
            (old_avg * old_count as f64 + rating as f64) / new_count as f64
        };
        tx.execute(
            "UPDATE skills SET rating_count = ?2, avg_rating = ?3, updated_at = ?4
             WHERE id = ?1",
            params![id, new_count, new_avg, now],
        )?;
        tx.commit()?;
        self.get(id)?
            .ok_or_else(|| anyhow!("skill disappeared after update: {id}"))
            .context("rate")
    }

    /// Increments `usage_count` (called after a successful execution).
    pub fn bump_usage(&self, id: &str) -> Result<()> {
        let g = self.conn.lock();
        let n = g.execute(
            "UPDATE skills SET usage_count = usage_count + 1, last_used = ?2 WHERE id = ?1",
            params![id, chrono::Utc::now().timestamp()],
        )?;
        if n == 0 {
            return Err(anyhow!("skill not found: {id}"));
        }
        Ok(())
    }

    /// Searches skills by name / description substring. Vector search
    /// lives in [`crate::skills::engine::SkillEngine`] — this method
    /// is the cheap fallback.
    pub fn text_search(&self, query: &str, limit: u32) -> Result<Vec<Skill>> {
        let g = self.conn.lock();
        let mut stmt = g.prepare(
            "SELECT id, name, description, code, language, tags,
                    usage_count, avg_rating, rating_count,
                    created_at, updated_at, source_memory_id,
                    activation_condition, platform, min_confidence
             FROM skills
             WHERE name LIKE ?1 OR description LIKE ?1 OR tags LIKE ?1
             ORDER BY usage_count DESC, avg_rating DESC
             LIMIT ?2",
        )?;
        let pat = format!("%{}%", query);
        let rows = stmt
            .query_map(params![pat, limit.max(1) as i64], row_to_skill)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Returns the total number of skills.
    pub fn count(&self) -> Result<i64> {
        let g = self.conn.lock();
        let n: i64 = g.query_row("SELECT COUNT(*) FROM skills", [], |r| r.get(0))?;
        Ok(n)
    }
}

fn row_to_skill(row: &Row<'_>) -> rusqlite::Result<Skill> {
    let tags_s: String = row.get(5)?;
    let tags: Vec<String> = serde_json::from_str(&tags_s).unwrap_or_default();
    let updated_at: i64 = row.get(10)?;
    let source_memory_id: Option<String> = row.get(11)?;
    let activation_condition: Option<super::types::ActivationCondition> = row
        .get::<_, Option<String>>(12)?
        .and_then(|s| serde_json::from_str(&s).ok());
    let platform: Option<Vec<String>> = row
        .get::<_, Option<String>>(13)?
        .and_then(|s| serde_json::from_str(&s).ok());
    let min_confidence: Option<f32> = row.get(14)?;
    Ok(Skill {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        code: row.get(3)?,
        language: row.get(4)?,
        tags,
        usage_count: row
            .get::<_, u32>(6)
            .or_else(|_| row.get::<_, i64>(6).map(|v| v as u32))?,
        avg_rating: row.get(7)?,
        rating_count: row
            .get::<_, u32>(8)
            .or_else(|_| row.get::<_, i64>(8).map(|v| v as u32))?,
        created_at: row.get(9)?,
        updated_at: if updated_at == 0 {
            row.get(9)?
        } else {
            updated_at
        },
        source_memory_id,
        activation_condition,
        platform,
        min_confidence,
    })
}

// Silence the unused-import warning for `FromStr` when the test module
// is compiled out.
#[allow(dead_code)]
fn _fromstr_keep(s: &str) -> std::result::Result<(), String> {
    let _ = <String as FromStr>::from_str(s);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_db() -> (PathBuf, SkillStore) {
        let mut p = std::env::temp_dir();
        p.push(format!("nine_snake_skill_test_{}.db", uuid::Uuid::new_v4()));
        let s = SkillStore::open_test(&p).unwrap();
        (p, s)
    }

    fn sample() -> Skill {
        Skill {
            id: "sk-1".to_string(),
            name: "palindrome".to_string(),
            description: "checks if a string is a palindrome".to_string(),
            code: "fn is_pal(s: &str) -> bool { s.chars().rev().collect::<String>() == s }"
                .to_string(),
            language: "rust".to_string(),
            tags: vec!["string".to_string(), "utility".to_string()],
            usage_count: 0,
            avg_rating: 0.0,
            rating_count: 0,
            created_at: 0,
            updated_at: 0,
            source_memory_id: None,
            activation_condition: None,
            platform: None,
            min_confidence: None,
        }
    }

    fn cleanup(p: &Path) {
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(p.with_extension("db-wal"));
        let _ = std::fs::remove_file(p.with_extension("db-shm"));
    }

    #[test]
    fn insert_and_get_round_trip() {
        let (p, s) = temp_db();
        s.insert(&sample()).unwrap();
        let got = s.get("sk-1").unwrap().unwrap();
        assert_eq!(got.name, "palindrome");
        assert_eq!(got.tags, vec!["string".to_string(), "utility".to_string()]);
        assert!(got.created_at > 0);
        cleanup(&p);
    }

    #[test]
    fn list_filters_by_language_and_tag() {
        let (p, s) = temp_db();
        s.insert(&sample()).unwrap();
        let mut b = sample();
        b.id = "sk-2".to_string();
        b.language = "python".to_string();
        b.tags = vec!["math".to_string()];
        s.insert(&b).unwrap();

        let all = s.list(None, None, 10).unwrap();
        assert_eq!(all.len(), 2);
        let rust_only = s.list(Some("rust"), None, 10).unwrap();
        assert_eq!(rust_only.len(), 1);
        assert_eq!(rust_only[0].id, "sk-1");
        let math_only = s.list(None, Some("math"), 10).unwrap();
        assert_eq!(math_only.len(), 1);
        assert_eq!(math_only[0].id, "sk-2");
        cleanup(&p);
    }

    #[test]
    fn rate_updates_avg_atomically() {
        let (p, s) = temp_db();
        s.insert(&sample()).unwrap();
        s.rate("sk-1", 5.0).unwrap();
        s.rate("sk-1", 3.0).unwrap();
        let got = s.get("sk-1").unwrap().unwrap();
        assert_eq!(got.rating_count, 2);
        assert!((got.avg_rating - 4.0).abs() < 1e-6);
        cleanup(&p);
    }

    #[test]
    fn text_search_matches_name_and_tags() {
        let (p, s) = temp_db();
        s.insert(&sample()).unwrap();
        let hits = s.text_search("palindrome", 10).unwrap();
        assert_eq!(hits.len(), 1);
        let hits = s.text_search("string", 10).unwrap();
        assert_eq!(hits.len(), 1);
        cleanup(&p);
    }

    #[test]
    fn bump_usage_increments() {
        let (p, s) = temp_db();
        s.insert(&sample()).unwrap();
        s.bump_usage("sk-1").unwrap();
        s.bump_usage("sk-1").unwrap();
        let got = s.get("sk-1").unwrap().unwrap();
        assert_eq!(got.usage_count, 2);
        cleanup(&p);
    }
}
