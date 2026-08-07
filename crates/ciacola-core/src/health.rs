//! What the system knows about its own size, and how to shrink it.
//!
//! Everything here accumulates: turns hold full prompt and reply text
//! forever, done items and resolved findings never leave, memory has no
//! eviction, and provider sessions grow until they cannot be resumed.
//! Three of those four are storage; the fourth is context, and rotation
//! (see `exec::load`) handles it.
//!
//! The design choice worth naming: **nothing prunes automatically.**
//! Deleting an agent's history is exactly the kind of irreversible act
//! that should be a decision, so `prune` is an operator tool and the
//! health surface is how the system asks for it. Making the system
//! legible about its own growth also means an agent can read
//! `ciacola://health`, notice the trend, and file a finding about it,
//! which is the introspection loop pointed inward.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use tower_mcp::{
    CallToolResult, ReadResourceResult, Resource, ResourceBuilder, ResourceContent, Tool,
    ToolBuilder,
};

use std::sync::Arc;

use crate::agent::FlatError;
use crate::plugin::PluginHost;
use crate::time::now_unix;

const DAY_SECS: i64 = 86_400;
const MIN_PRUNE_DAYS: i64 = 1;

#[derive(Clone)]
pub struct Health {
    pool: SqlitePool,
    db_path: String,
    /// Set after the host exists, so health can ask every plugin for
    /// its own slice instead of knowing their tables. Before this,
    /// prune deleted from `work_items` and `findings` directly.
    host: Option<Arc<PluginHost>>,
}

impl Health {
    pub fn new(pool: SqlitePool, db_path: impl Into<String>) -> Self {
        Self {
            pool,
            db_path: db_path.into(),
            host: None,
        }
    }

    pub fn with_host(mut self, host: Arc<PluginHost>) -> Self {
        self.host = Some(host);
        self
    }

    async fn count(&self, sql: &str) -> i64 {
        sqlx::query_as::<_, (i64,)>(sql)
            .fetch_one(&self.pool)
            .await
            .map(|(n,)| n)
            .unwrap_or_default()
    }

    /// Bytes on disk, including the write-ahead log if there is one.
    fn db_bytes(&self) -> u64 {
        [".", "-wal", "-shm"]
            .iter()
            .map(|suffix| {
                let path = if *suffix == "." {
                    self.db_path.clone()
                } else {
                    format!("{}{suffix}", self.db_path)
                };
                std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
            })
            .sum()
    }

    pub async fn report(&self) -> serde_json::Value {
        let now = now_unix();
        let oldest_turn =
            sqlx::query_as::<_, (Option<i64>,)>("SELECT MIN(at_unix) FROM turns WHERE at_unix > 0")
                .fetch_one(&self.pool)
                .await
                .ok()
                .and_then(|(v,)| v);

        // Agents whose current session is long. This is the number that
        // predicts a hard failure rather than a slow one: an unrotated
        // session eventually exceeds the provider's context.
        let sessions = sqlx::query_as::<_, (String, String, i64)>(
            "SELECT a.agent_id, a.name,
                    (SELECT COALESCE(MAX(seq), 0) FROM turns t WHERE t.agent_id = a.agent_id)
                        - a.session_started_seq
             FROM agents a WHERE a.retired = 0
             ORDER BY 3 DESC LIMIT 5",
        )
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let plugins = match &self.host {
            Some(host) => host.health().await,
            None => json!({}),
        };
        json!({
            "db_bytes": self.db_bytes(),
            "plugins": plugins,
            "agents_active": self.count("SELECT COUNT(*) FROM agents WHERE retired = 0").await,
            "agents_retired": self.count("SELECT COUNT(*) FROM agents WHERE retired = 1").await,
            "turns": self.count("SELECT COUNT(*) FROM turns").await,
            "turns_with_text": self
                .count("SELECT COUNT(*) FROM turns WHERE prompt <> '' OR reply IS NOT NULL")
                .await,
            "tokens_in": self
                .count("SELECT COALESCE(SUM(tokens_in), 0) FROM turns")
                .await,
            "tokens_out": self
                .count("SELECT COALESCE(SUM(tokens_out), 0) FROM turns")
                .await,
            "tokens_cached": self
                .count("SELECT COALESCE(SUM(tokens_cached), 0) FROM turns")
                .await,
            "turn_text_bytes": self
                .count(
                    "SELECT COALESCE(SUM(LENGTH(prompt) + LENGTH(COALESCE(reply, ''))), 0)
                     FROM turns",
                )
                .await,
            "oldest_turn_age_days": oldest_turn.map(|t| (now - t) / DAY_SECS),
            "longest_sessions": sessions
                .iter()
                .map(|(agent_id, name, turns)| json!({
                    "agent_id": agent_id,
                    "name": name,
                    "turns_in_session": turns,
                }))
                .collect::<Vec<_>>(),
        })
    }

    /// Drop what is safely droppable, keeping what is evidence.
    ///
    /// Turn text is blanked rather than deleted: the row, its state,
    /// cost, and timing survive, so spend history and the shape of what
    /// happened stay intact while the bulk goes. Closed items and
    /// resolved findings are deleted outright.
    pub async fn prune(&self, older_than_days: i64) -> Result<serde_json::Value, FlatError> {
        let cutoff = now_unix() - older_than_days.max(MIN_PRUNE_DAYS) * DAY_SECS;
        let before = self.db_bytes();

        let turns = sqlx::query(
            "UPDATE turns SET prompt = '', reply = CASE WHEN reply IS NULL THEN NULL ELSE '' END
             WHERE at_unix > 0 AND at_unix < ?1
               AND state NOT IN ('queued', 'running')
               AND (prompt <> '' OR reply <> '')",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?
        .rows_affected();

        // Each plugin prunes what it owns. Core knows only about turns.
        let plugins = match &self.host {
            Some(host) => host.prune(cutoff).await,
            None => json!({}),
        };

        sqlx::query("VACUUM").execute(&self.pool).await?;

        Ok(json!({
            "older_than_days": older_than_days.max(MIN_PRUNE_DAYS),
            "turns_blanked": turns,
            "plugins": plugins,
            "db_bytes_before": before,
            "db_bytes_after": self.db_bytes(),
        }))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PruneArgs {
    /// Only touch records older than this. Minimum 1.
    older_than_days: i64,
}

/// Read-only health, for both surfaces: an agent that can see the
/// system growing can file a finding about it.
pub fn tools(health: Health) -> Vec<Tool> {
    let report = ToolBuilder::new("health")
        .description(
            "How big the system has become: rows, bytes, memory size, \
             and which agents have the longest running sessions.",
        )
        .read_only()
        .no_params_handler(move || {
            let health = health.clone();
            async move { Ok(CallToolResult::json(health.report().await)) }
        })
        .build();
    vec![report]
}

/// Pruning is destructive and deliberate: operator surface only.
pub fn operator_tools(health: Health) -> Vec<Tool> {
    let prune = ToolBuilder::new("prune")
        .description(
            "Blank the text of old finished turns and delete old closed \
             items and resolved findings, then vacuum. Costs, states, \
             and timings survive; only the bulk text goes.",
        )
        .destructive()
        .handler(move |args: PruneArgs| {
            let health = health.clone();
            async move {
                match health.prune(args.older_than_days).await {
                    Ok(report) => Ok(CallToolResult::json(report)),
                    Err(e) => Ok(CallToolResult::error(e.to_string())),
                }
            }
        })
        .build();
    vec![prune]
}

pub fn resources(health: Health) -> Vec<Resource> {
    let resource = ResourceBuilder::new("ciacola://health")
        .name("health")
        .description("System size and session lengths.")
        .mime_type("application/json")
        .handler(move || {
            let health = health.clone();
            async move {
                Ok(ReadResourceResult {
                    contents: vec![ResourceContent {
                        uri: "ciacola://health".to_string(),
                        mime_type: Some("application/json".to_string()),
                        text: Some(health.report().await.to_string()),
                        blob: None,
                        meta: None,
                    }],
                    ..Default::default()
                })
            }
        })
        .build();
    vec![resource]
}
