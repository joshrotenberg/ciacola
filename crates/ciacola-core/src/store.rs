//! The easy path: typed key-value for plugins that do not want SQL.
//!
//! Offered, not enforced. A plugin that needs UPSERT-with-COALESCE,
//! `MAX(seq) + 1`, LIKE search, or an aggregate still takes
//! [`PluginContext::pool`] and writes exactly the SQL it needs; that is
//! why the pool is on the context at all. This is for everything else.
//!
//! What it buys, concretely: a plugin using only [`Store`] declares no
//! tables and ships no migrations. Core owns the one table, so the
//! whole schema story disappears for simple plugins.
//!
//! Values are serde, so plugins store their own structs rather than
//! strings, and keys are namespaced by plugin name so two plugins
//! cannot collide by accident.
//!
//! The caveat, stated plainly: nothing here is a security boundary. A
//! compile-in plugin holds the whole process. `Store` scopes its own
//! queries to one plugin, but a plugin that asks for someone else's
//! namespace, or reaches past this into the pool, will get what it
//! asked for. If you need a real boundary, run a separate MCP server.

use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::SqlitePool;

use crate::agent::FlatError;
use crate::plugin::Migration;
use crate::time::now_unix;

/// Core owns this table so that a key-value plugin owns nothing.
pub(super) const MIGRATIONS: &[Migration] = &[Migration::new(
    "0001_plugin_kv",
    "CREATE TABLE IF NOT EXISTS plugin_kv (
         plugin TEXT NOT NULL,
         key TEXT NOT NULL,
         value TEXT NOT NULL,
         updated_unix INTEGER NOT NULL,
         PRIMARY KEY (plugin, key));",
)];

/// A plugin's slice of the shared key-value table.
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    plugin: String,
}

impl Store {
    pub fn new(pool: SqlitePool, plugin: impl Into<String>) -> Self {
        Self {
            pool,
            plugin: plugin.into(),
        }
    }

    /// Write, replacing whatever was there.
    pub async fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<(), FlatError> {
        sqlx::query(
            "INSERT INTO plugin_kv (plugin, key, value, updated_unix) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(plugin, key) DO UPDATE SET value = ?3, updated_unix = ?4",
        )
        .bind(&self.plugin)
        .bind(key)
        .bind(serde_json::to_string(value)?)
        .bind(now_unix())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>, FlatError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM plugin_kv WHERE plugin = ?1 AND key = ?2")
                .bind(&self.plugin)
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        match row {
            Some((value,)) => Ok(Some(serde_json::from_str(&value)?)),
            None => Ok(None),
        }
    }

    pub async fn delete(&self, key: &str) -> Result<bool, FlatError> {
        let done = sqlx::query("DELETE FROM plugin_kv WHERE plugin = ?1 AND key = ?2")
            .bind(&self.plugin)
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Every entry, newest first, optionally filtered to a key prefix.
    /// Prefixes are how a key-value plugin gets collections: store
    /// `note/2026-08-07/x` and list `note/`.
    pub async fn list<T: DeserializeOwned>(
        &self,
        prefix: Option<&str>,
    ) -> Result<Vec<(String, T)>, FlatError> {
        let rows: Vec<(String, String)> = match prefix {
            Some(prefix) => {
                sqlx::query_as(
                    "SELECT key, value FROM plugin_kv
                     WHERE plugin = ?1 AND key LIKE ?2 || '%'
                     ORDER BY updated_unix DESC",
                )
                .bind(&self.plugin)
                .bind(prefix)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as(
                    "SELECT key, value FROM plugin_kv WHERE plugin = ?1
                     ORDER BY updated_unix DESC",
                )
                .bind(&self.plugin)
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.into_iter()
            .map(|(key, value)| Ok((key, serde_json::from_str(&value)?)))
            .collect()
    }

    pub async fn count(&self) -> Result<i64, FlatError> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM plugin_kv WHERE plugin = ?1")
            .bind(&self.plugin)
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    pub async fn bytes(&self) -> Result<i64, FlatError> {
        let (bytes,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(LENGTH(key) + LENGTH(value)), 0) FROM plugin_kv
             WHERE plugin = ?1",
        )
        .bind(&self.plugin)
        .fetch_one(&self.pool)
        .await?;
        Ok(bytes)
    }

    /// Entries untouched since `cutoff`, for a plugin whose `prune`
    /// wants the obvious policy.
    pub async fn prune_before(&self, cutoff: i64) -> Result<u64, FlatError> {
        Ok(
            sqlx::query("DELETE FROM plugin_kv WHERE plugin = ?1 AND updated_unix < ?2")
                .bind(&self.plugin)
                .bind(cutoff)
                .execute(&self.pool)
                .await?
                .rows_affected(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Note {
        text: String,
        n: i32,
    }

    async fn store(plugin: &str, pool: SqlitePool) -> Store {
        crate::plugin::apply_migrations(&pool, "store", MIGRATIONS)
            .await
            .expect("migrations");
        Store::new(pool, plugin)
    }

    #[tokio::test]
    async fn round_trips_typed_values_and_scopes_by_plugin() {
        let pool = SqlitePool::connect("sqlite::memory:").await.expect("pool");
        let alpha = store("alpha", pool.clone()).await;
        let beta = store("beta", pool).await;

        let note = Note {
            text: "hello".into(),
            n: 1,
        };
        alpha.put("note/a", &note).await.expect("put");
        assert_eq!(alpha.get::<Note>("note/a").await.expect("get"), Some(note));

        // Same key, different plugin: no collision, no visibility.
        assert_eq!(beta.get::<Note>("note/a").await.expect("get"), None);
        assert_eq!(beta.count().await.expect("count"), 0);
        assert_eq!(alpha.count().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn lists_by_prefix_and_deletes() {
        let pool = SqlitePool::connect("sqlite::memory:").await.expect("pool");
        let s = store("alpha", pool).await;
        for (key, n) in [("note/a", 1), ("note/b", 2), ("other/c", 3)] {
            s.put(
                key,
                &Note {
                    text: key.into(),
                    n,
                },
            )
            .await
            .expect("put");
        }
        assert_eq!(s.list::<Note>(Some("note/")).await.expect("list").len(), 2);
        assert_eq!(s.list::<Note>(None).await.expect("list").len(), 3);
        assert!(s.delete("note/a").await.expect("delete"));
        assert!(!s.delete("note/a").await.expect("delete twice"));
        assert_eq!(s.list::<Note>(Some("note/")).await.expect("list").len(), 1);
    }
}
