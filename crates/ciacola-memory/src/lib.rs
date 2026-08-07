//! Server memory: knowledge that outlives every agent.
//!
//! A namespaced key-value store beside the ledger. Conversations die,
//! managers rotate, spokes retire; what the system has *learned* lands
//! here and accumulates. The introspection loop closes through it: a
//! correction observed in one wake becomes a `remember` under a
//! namespaced key, and any later agent, including a freshly rotated
//! manager with no conversation history, provisions right the first
//! time by `recall`ing it.
//!
//! Deliberately a spike-simple store: LIKE-match recall, no embeddings,
//! no curation. Known gaps, recorded: any agent can write (no trust
//! boundary, so memory is a prompt-injection surface between agents),
//! and nothing consolidates or expires entries yet.

use apalis_sqlite::SqlitePool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tower_mcp::{
    CallToolResult, ReadResourceResult, Resource, ResourceBuilder, ResourceContent, Tool,
    ToolBuilder,
};

use ciacola_core::agent::FlatError;
use ciacola_core::time::now_unix;

const MAX_RECALL: i64 = 50;

#[derive(Clone)]
pub struct Memory {
    pool: SqlitePool,
}

#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub key: String,
    pub value: String,
    pub author: Option<String>,
    pub updated_unix: i64,
}

type Row = (String, String, Option<String>, i64);

fn row(t: Row) -> MemoryRow {
    let (key, value, author, updated_unix) = t;
    MemoryRow {
        key,
        value,
        author,
        updated_unix,
    }
}

impl Memory {
    /// Wrap an already-migrated pool. Schema is the plugin's
    /// `migrations()`, not this constructor.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn setup(pool: SqlitePool) -> Result<Self, FlatError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS memory (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL,
                 author TEXT,
                 updated_unix INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await?;
        Ok(Self { pool })
    }

    pub async fn remember(
        &self,
        key: &str,
        value: &str,
        author: Option<&str>,
    ) -> Result<(), FlatError> {
        sqlx::query(
            "INSERT INTO memory (key, value, author, updated_unix)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET
                 value = ?2, author = ?3, updated_unix = ?4",
        )
        .bind(key)
        .bind(value)
        .bind(author)
        .bind(now_unix())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn forget(&self, key: &str) -> Result<bool, FlatError> {
        let done = sqlx::query("DELETE FROM memory WHERE key = ?1")
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() == 1)
    }

    /// LIKE-match over keys and values; empty query returns everything,
    /// newest first, capped.
    pub async fn recall(&self, query: Option<&str>) -> Result<Vec<MemoryRow>, FlatError> {
        let rows: Vec<Row> = match query {
            Some(q) if !q.is_empty() => {
                let needle = format!("%{q}%");
                sqlx::query_as(
                    "SELECT key, value, author, updated_unix FROM memory
                     WHERE key LIKE ?1 OR value LIKE ?1
                     ORDER BY updated_unix DESC LIMIT ?2",
                )
                .bind(needle)
                .bind(MAX_RECALL)
                .fetch_all(&self.pool)
                .await?
            }
            _ => {
                sqlx::query_as(
                    "SELECT key, value, author, updated_unix FROM memory
                     ORDER BY updated_unix DESC LIMIT ?1",
                )
                .bind(MAX_RECALL)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.into_iter().map(row).collect())
    }
}

fn memory_json(m: &MemoryRow) -> serde_json::Value {
    json!({
        "key": m.key,
        "value": m.value,
        "author": m.author,
        "updated_unix": m.updated_unix,
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RememberArgs {
    /// Namespaced key, e.g. "spokes/provisioning/pr-summary".
    key: String,
    /// The knowledge itself. Overwrites what was there.
    value: String,
    /// Your agent_id, so lessons carry provenance.
    author: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RecallArgs {
    /// Substring matched against keys and values. Omit for everything
    /// (newest first, capped).
    query: Option<String>,
}

pub fn tools(memory: Memory) -> Vec<Tool> {
    let remember = {
        let memory = memory.clone();
        ToolBuilder::new("remember")
            .description(
                "Store knowledge that should outlive this conversation: \
                 lessons from corrections, facts about repos, decisions. \
                 Namespace the key; later agents recall it.",
            )
            .non_destructive()
            .handler(move |args: RememberArgs| {
                let memory = memory.clone();
                async move {
                    match memory
                        .remember(&args.key, &args.value, args.author.as_deref())
                        .await
                    {
                        Ok(()) => Ok(CallToolResult::json(json!({ "remembered": args.key }))),
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                }
            })
            .build()
    };

    let recall = ToolBuilder::new("recall")
        .description(
            "Search server memory. Do this before provisioning spokes or \
             making decisions a past wake may already have learned about.",
        )
        .read_only()
        .handler(move |args: RecallArgs| {
            let memory = memory.clone();
            async move {
                match memory.recall(args.query.as_deref()).await {
                    Ok(rows) => Ok(CallToolResult::json(json!({
                        "memories": rows.iter().map(memory_json).collect::<Vec<_>>()
                    }))),
                    Err(e) => Ok(CallToolResult::error(e.to_string())),
                }
            }
        })
        .build();

    vec![remember, recall]
}

pub fn resources(memory: Memory) -> Vec<Resource> {
    let all = ResourceBuilder::new("ciacola://memory")
        .name("memory")
        .description("Everything the server has learned, newest first.")
        .mime_type("application/json")
        .handler(move || {
            let memory = memory.clone();
            async move {
                let rows = memory.recall(None).await.unwrap_or_default();
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "ciacola://memory".to_string(),
                        mime_type: Some("application/json".to_string()),
                        text: Some(
                            json!(rows.iter().map(memory_json).collect::<Vec<_>>()).to_string(),
                        ),
                        blob: None,
                        meta: None,
                    }],
                    ..Default::default()
                })
            }
        })
        .build();
    vec![all]
}

// --- plugin ---

use ciacola_core::plugin::{BoxFut, Migration, Plugin, PluginContext, Surface};

/// Memory as a plugin. No board section: memory is for agents to read,
/// and the operator reads it through the resource when they want to.
#[derive(Default)]
pub struct MemoryPlugin {
    memory: Option<Memory>,
}

impl MemoryPlugin {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Plugin for MemoryPlugin {
    fn tables(&self) -> &'static [&'static str] {
        &["memory"]
    }

    fn migrations(&self) -> &'static [Migration] {
        {
            const M: &[Migration] = &[Migration::new(
                "0001_memory",
                "CREATE TABLE IF NOT EXISTS memory (
                 key TEXT PRIMARY KEY, value TEXT NOT NULL, author TEXT,
                 updated_unix INTEGER NOT NULL);",
            )];
            M
        }
    }

    fn name(&self) -> &'static str {
        "memory"
    }

    fn setup<'a>(&'a mut self, ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async move {
            self.memory = Some(Memory::new(ctx.pool.clone()));
            Ok(())
        })
    }

    fn tools(&self, _surface: Surface) -> Vec<Tool> {
        self.memory
            .as_ref()
            .map(|m| tools(m.clone()))
            .unwrap_or_default()
    }

    fn resources(&self) -> Vec<Resource> {
        self.memory
            .as_ref()
            .map(|m| resources(m.clone()))
            .unwrap_or_default()
    }

    fn health(&self) -> BoxFut<'_, serde_json::Value> {
        Box::pin(async move {
            let Some(memory) = self.memory.as_ref() else {
                return json!({});
            };
            let rows = memory.recall(None).await.unwrap_or_default();
            let bytes: usize = rows.iter().map(|r| r.key.len() + r.value.len()).sum();
            // Memory is read into an agent's context on every wake, so
            // its size is context cost, not just disk.
            json!({ "entries": rows.len(), "bytes": bytes })
        })
    }
}
