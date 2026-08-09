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
use tower_mcp::{
    CallToolResult, ReadResourceResult, Resource, ResourceBuilder, ResourceContent, Tool,
    ToolBuilder,
};

use std::sync::Arc;

use crate::agent::FlatError;
use crate::ledger::Ledger;
use crate::limits::Limits;
use crate::plugin::PluginHost;
use crate::time::now_unix;

const DAY_SECS: i64 = 86_400;
const MIN_PRUNE_DAYS: i64 = 1;

#[derive(Clone)]
pub struct Health {
    ledger: Ledger,
    db_path: String,
    /// The backends this build was assembled with. Reported so an
    /// operator can see what a `provider` key on an agent will actually
    /// resolve to, rather than discovering it on the first failed turn.
    providers: Vec<String>,
    /// The same admission policy every submission boundary enforces.
    /// Keeping it here makes health a view of live policy, rather than
    /// a second interpretation of the configuration.
    limits: Limits,
    /// Set after the host exists, so health can ask every plugin for
    /// its own slice instead of knowing their tables. Before this,
    /// prune deleted from `work_items` and `findings` directly.
    host: Option<Arc<PluginHost>>,
}

impl Health {
    pub fn new(ledger: Ledger, db_path: impl Into<String>) -> Self {
        let providers = ledger.providers().keys();
        Self {
            ledger,
            db_path: db_path.into(),
            providers,
            limits: Limits::default(),
            host: None,
        }
    }

    pub fn with_host(mut self, host: Arc<PluginHost>) -> Self {
        self.host = Some(host);
        self
    }

    /// Report which backends are registered.
    pub fn with_providers(mut self, providers: &ciacola_agent::ProviderRegistry) -> Self {
        self.providers = providers.keys();
        self.ledger = self.ledger.with_providers(providers.clone());
        self
    }

    /// Report admission against the same limits used to accept work.
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    async fn count(&self, sql: &str) -> i64 {
        sqlx::query_as::<_, (i64,)>(sql)
            .fetch_one(self.ledger.pool())
            .await
            .map(|(n,)| n)
            .unwrap_or_default()
    }

    /// Sum non-negative ledger counters without SQLite's signed-integer
    /// aggregate overflow turning an extreme-but-valid provider report
    /// into a query error (and, historically, a misleading zero).
    async fn sum_nonnegative(&self, sql: &str) -> i64 {
        sqlx::query_as::<_, (i64,)>(sql)
            .fetch_all(self.ledger.pool())
            .await
            .map(|rows| {
                rows.into_iter()
                    .fold(0_i64, |total, (value,)| total.saturating_add(value.max(0)))
            })
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
        let admission = match self.ledger.admission_report(&self.limits).await {
            Ok(report) => json!(report),
            Err(error) => json!({ "error": error.to_string() }),
        };
        let oldest_turn =
            sqlx::query_as::<_, (Option<i64>,)>("SELECT MIN(at_unix) FROM turns WHERE at_unix > 0")
                .fetch_one(self.ledger.pool())
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
        .fetch_all(self.ledger.pool())
        .await
        .unwrap_or_default();

        let plugins = match &self.host {
            Some(host) => host.health().await,
            None => json!({}),
        };
        let cost_states = json!({
            "reported": self.count("SELECT COUNT(*) FROM turns WHERE state IN ('ok', 'failed', 'killed') AND cost_state = 'reported'").await,
            "unreported": self.count("SELECT COUNT(*) FROM turns WHERE state IN ('ok', 'failed', 'killed') AND cost_state = 'unreported'").await,
            "not_priced": self.count("SELECT COUNT(*) FROM turns WHERE state IN ('ok', 'failed', 'killed') AND cost_state = 'not_priced'").await,
            "legacy": self.count("SELECT COUNT(*) FROM turns WHERE state IN ('ok', 'failed', 'killed') AND cost_state = 'legacy'").await,
        });
        let usage_states = json!({
            "reported": self.count("SELECT COUNT(*) FROM turns WHERE state IN ('ok', 'failed', 'killed') AND usage_state = 'reported'").await,
            "unreported": self.count("SELECT COUNT(*) FROM turns WHERE state IN ('ok', 'failed', 'killed') AND usage_state = 'unreported'").await,
            "not_tracked": self.count("SELECT COUNT(*) FROM turns WHERE state IN ('ok', 'failed', 'killed') AND usage_state = 'not_tracked'").await,
            "legacy": self.count("SELECT COUNT(*) FROM turns WHERE state IN ('ok', 'failed', 'killed') AND usage_state = 'legacy'").await,
        });
        let elapsed_states = json!({
            "measured": self.count("SELECT COUNT(*) FROM turns WHERE state IN ('ok', 'failed', 'killed') AND elapsed_state = 'measured'").await,
            "upper_bound": self.count("SELECT COUNT(*) FROM turns WHERE state IN ('ok', 'failed', 'killed') AND elapsed_state = 'upper_bound'").await,
            "unknown": self.count("SELECT COUNT(*) FROM turns WHERE state IN ('ok', 'failed', 'killed') AND elapsed_state = 'unknown'").await,
            "not_attempted": self.count("SELECT COUNT(*) FROM turns WHERE state IN ('ok', 'failed', 'killed') AND elapsed_state = 'not_attempted'").await,
            "legacy": self.count("SELECT COUNT(*) FROM turns WHERE state IN ('ok', 'failed', 'killed') AND elapsed_state = 'legacy'").await,
        });
        json!({
            "db_bytes": self.db_bytes(),
            "providers": self.providers,
            "admission": admission,
            "plugins": plugins,
            "agents_active": self.count("SELECT COUNT(*) FROM agents WHERE retired = 0").await,
            "agents_retired": self.count("SELECT COUNT(*) FROM agents WHERE retired = 1").await,
            "turns": self.count("SELECT COUNT(*) FROM turns").await,
            "turns_with_text": self
                .count("SELECT COUNT(*) FROM turns WHERE prompt <> '' OR reply IS NOT NULL")
                .await,
            "reported_cost_micro_usd": self
                .sum_nonnegative(
                    "SELECT cost_micro_usd FROM turns
                     WHERE state IN ('ok', 'failed', 'killed')
                       AND (cost_state = 'reported'
                            OR (cost_state = 'legacy' AND cost_micro_usd <> 0))"
                )
                .await,
            "tokens_in": self
                .sum_nonnegative(
                    "SELECT tokens_in FROM turns
                     WHERE state IN ('ok', 'failed', 'killed')
                       AND (usage_state = 'reported'
                            OR (usage_state = 'legacy'
                                AND (tokens_in <> 0 OR tokens_out <> 0 OR tokens_cached <> 0)))"
                )
                .await,
            "tokens_out": self
                .sum_nonnegative(
                    "SELECT tokens_out FROM turns
                     WHERE state IN ('ok', 'failed', 'killed')
                       AND (usage_state = 'reported'
                            OR (usage_state = 'legacy'
                                AND (tokens_in <> 0 OR tokens_out <> 0 OR tokens_cached <> 0)))"
                )
                .await,
            "tokens_cached": self
                .sum_nonnegative(
                    "SELECT tokens_cached FROM turns
                     WHERE state IN ('ok', 'failed', 'killed')
                       AND (usage_state = 'reported'
                            OR (usage_state = 'legacy'
                                AND (tokens_in <> 0 OR tokens_out <> 0 OR tokens_cached <> 0)))"
                )
                .await,
            "telemetry": {
                "cost_states": cost_states,
                "usage_states": usage_states,
                "elapsed_states": elapsed_states,
            },
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
        .execute(self.ledger.pool())
        .await?
        .rows_affected();

        // Each plugin prunes what it owns. Core knows only about turns.
        let plugins = match &self.host {
            Some(host) => host.prune(cutoff).await,
            None => json!({}),
        };

        sqlx::query("VACUUM").execute(self.ledger.pool()).await?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentDef, Exchange};
    use crate::ledger::Ledger;

    #[tokio::test]
    async fn health_counts_reported_zero_separately_from_unreported() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool.clone()).await.expect("ledger");

        let measured = ledger
            .create_agent(&AgentDef::new("measured", "s"), None)
            .await
            .expect("agent");
        let seq = ledger.enqueue_turn(&measured, "work").await.expect("turn");
        assert!(ledger.claim_turn(&measured, seq).await.expect("claim"));
        assert!(
            ledger
                .complete_turn(
                    &measured,
                    seq,
                    &Exchange {
                        reply: "done".into(),
                        session: None,
                        cost: ciacola_agent::Cost::Reported { micro_usd: 0 },
                        usage: ciacola_agent::Usage::Reported(
                            ciacola_agent::TokenUsage::default(),
                        ),
                        usage_complete: true,
                        provider_turns: Some(0),
                        elapsed_ms: 1,
                        error: None,
                    },
                )
                .await
                .expect("complete")
        );

        let unknown = ledger
            .create_agent(&AgentDef::new("unknown", "s"), None)
            .await
            .expect("agent");
        let seq = ledger.enqueue_turn(&unknown, "work").await.expect("turn");
        assert!(ledger.claim_turn(&unknown, seq).await.expect("claim"));
        assert!(
            ledger
                .interrupt_turn(&unknown, seq, "killed", "stopped")
                .await
                .expect("interrupt")
        );

        let not_attempted = ledger
            .create_agent(&AgentDef::new("not-attempted", "s"), None)
            .await
            .expect("agent");
        let seq = ledger
            .enqueue_turn(&not_attempted, "queued")
            .await
            .expect("turn");
        assert!(
            ledger
                .interrupt_turn(&not_attempted, seq, "killed", "stopped while queued")
                .await
                .expect("interrupt")
        );

        let report = Health::new(ledger, "").report().await;
        assert_eq!(report["reported_cost_micro_usd"], 0);
        assert_eq!(report["telemetry"]["cost_states"]["reported"], 2);
        assert_eq!(report["telemetry"]["cost_states"]["unreported"], 1);
        assert_eq!(report["telemetry"]["usage_states"]["reported"], 2);
        assert_eq!(report["telemetry"]["usage_states"]["unreported"], 1);
        assert_eq!(report["telemetry"]["elapsed_states"]["measured"], 2);
        assert_eq!(report["telemetry"]["elapsed_states"]["not_attempted"], 1);
        assert_eq!(report["admission"]["window_seconds"], DAY_SECS);
        assert_eq!(report["admission"]["providers"][0]["provider"], "claude");
        assert_eq!(report["admission"]["providers"][0]["tokens_in"], 0);
        assert_eq!(
            report["admission"]["providers"][0]["usage_unreported_turns"],
            1
        );
    }
}
