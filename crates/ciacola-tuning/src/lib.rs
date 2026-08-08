//! What each model and effort level actually cost and achieved.
//!
//! Model choice is currently a guess frozen into a role. The interesting
//! version is a persistent agent choosing for its ephemerals, which is
//! mechanically trivial (`spawn_role` takes overrides) and useless
//! without evidence: an agent asked to pick a model with nothing to go
//! on will pick whatever sounds impressive, which is how you end up
//! running sonnet to summarize a two-line diff.
//!
//! So this is the evidence, not the decision. It reads the ledger and
//! groups finished turns by `(provider, model, effort, role)`, giving
//! runs, failure rate, median cost, and median duration. A manager reads
//! it before spawning; the operator reads it to notice that a role is
//! provisioned two tiers above what it needs.
//!
//! Stateless, like `git`: no tables, no migrations, no store. Every
//! answer is a query over turns and agent definitions.
//!
//! Keyed on provider from the start even though there is only one
//! today. The cross-provider question ("why would Claude hand this to
//! Codex?") is the one worth being able to answer eventually, and a
//! statistic keyed only on model name cannot be retrofitted to answer
//! it.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tower_mcp::{
    CallToolResult, ReadResourceResult, Resource, ResourceBuilder, ResourceContent, Tool,
    ToolBuilder,
};

use ciacola_core::agent::FlatError;
use ciacola_core::ledger::Ledger;
use ciacola_core::plugin::{BoxFut, Plugin, PluginContext, Section, Surface};

/// One `(provider, model, effort, role)` bucket.
#[derive(Debug, Clone)]
pub struct Stat {
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub role: String,
    pub runs: usize,
    pub failures: usize,
    pub median_cost_micro_usd: i64,
    pub median_secs: i64,
    pub total_cost_micro_usd: i64,
}

/// `(provider, model, effort, role)`.
type BucketKey = (String, String, String, String);
/// Costs, durations, failure count.
type Bucket = (Vec<i64>, Vec<i64>, usize);
/// One finished turn, flattened for grouping: provider, model, effort,
/// role, cost, duration in seconds, and whether it failed.
type Entry = (String, String, String, String, i64, i64, bool);

fn median(mut values: Vec<i64>) -> i64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

/// The aggregation itself, pulled out of `collect` so it can be tested
/// without a database: feed it entries directly and it groups them the
/// same way a real ledger read would.
fn aggregate(entries: Vec<Entry>) -> Vec<Stat> {
    let mut buckets: BTreeMap<BucketKey, Bucket> = BTreeMap::new();
    for (provider, model, effort, role, cost, secs, failed) in entries {
        let entry = buckets.entry((provider, model, effort, role)).or_default();
        entry.0.push(cost);
        entry.1.push(secs);
        if failed {
            entry.2 += 1;
        }
    }

    buckets
        .into_iter()
        .map(
            |((provider, model, effort, role), (costs, secs, failures))| Stat {
                provider,
                model,
                effort,
                role,
                runs: costs.len(),
                failures,
                total_cost_micro_usd: costs.iter().sum(),
                median_cost_micro_usd: median(costs),
                median_secs: median(secs),
            },
        )
        .collect()
}

async fn collect(ledger: &Ledger) -> Result<Vec<Stat>, FlatError> {
    // Retired agents are included on purpose: an ephemeral spoke is
    // retired the moment its work is accepted, so excluding them would
    // throw away exactly the population worth measuring.
    let rows: Vec<(String, i64, i64, String)> = sqlx::query_as(
        "SELECT a.def, t.cost_micro_usd, t.elapsed_ms, t.state
         FROM turns t JOIN agents a ON a.agent_id = t.agent_id
         WHERE t.state IN ('ok', 'failed', 'killed')",
    )
    .fetch_all(ledger.pool())
    .await?;

    let entries = rows
        .into_iter()
        .filter_map(|(def_json, cost, elapsed_ms, state)| {
            let def = serde_json::from_str::<ciacola_core::agent::AgentDef>(&def_json).ok()?;
            Some((
                // The backend each agent actually ran on, read from its
                // own definition. Rows written before the field existed
                // deserialize as claude, so history keeps its bucket
                // rather than shifting when a second provider lands.
                def.provider.to_string(),
                def.model.clone().unwrap_or_else(|| "(default)".into()),
                def.effort.clone().unwrap_or_else(|| "(default)".into()),
                def.name.clone(),
                cost,
                elapsed_ms / 1000,
                state != "ok",
            ))
        })
        .collect();

    Ok(aggregate(entries))
}

fn stat_json(s: &Stat) -> serde_json::Value {
    json!({
        "provider": s.provider,
        "model": s.model,
        "effort": s.effort,
        "role": s.role,
        "runs": s.runs,
        "failures": s.failures,
        "median_cost_usd": s.median_cost_micro_usd as f64 / 1e6,
        "median_secs": s.median_secs,
        "total_cost_usd": s.total_cost_micro_usd as f64 / 1e6,
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatsArgs {
    /// Only buckets for this role name. Omit for all.
    role: Option<String>,
}

#[derive(Default)]
pub struct TuningPlugin {
    ledger: Option<Ledger>,
}

impl Plugin for TuningPlugin {
    fn name(&self) -> &'static str {
        "tuning"
    }

    fn setup<'a>(&'a mut self, ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async move {
            self.ledger = Some(ctx.ledger.clone());
            Ok(())
        })
    }

    fn tools(&self, _surface: Surface) -> Vec<Tool> {
        let ledger = self.ledger.clone();
        vec![
            ToolBuilder::new("model_stats")
                .description(
                    "What each model and effort level has actually cost and \
                     achieved, by role: runs, failures, median cost, median \
                     duration. Read this before choosing a model for an agent \
                     you are about to spawn; a role provisioned two tiers \
                     above what its numbers justify is pure waste, and one \
                     provisioned below shows up as a failure rate.",
                )
                .read_only()
                .handler(move |args: StatsArgs| {
                    let ledger = ledger.clone();
                    async move {
                        let Some(ledger) = ledger else {
                            return Ok(CallToolResult::json(json!({ "stats": [] })));
                        };
                        match collect(&ledger).await {
                            Ok(stats) => Ok(CallToolResult::json(json!({
                                "stats": stats
                                    .iter()
                                    .filter(|s| args.role.as_ref().is_none_or(|r| *r == s.role))
                                    .map(stat_json)
                                    .collect::<Vec<_>>()
                            }))),
                            Err(e) => Ok(CallToolResult::error(e.to_string())),
                        }
                    }
                })
                .build(),
        ]
    }

    fn resources(&self) -> Vec<Resource> {
        let ledger = self.ledger.clone();
        vec![
            ResourceBuilder::new("ciacola://model-stats")
                .name("model-stats")
                .description("Cost and outcome by provider, model, effort, and role.")
                .mime_type("application/json")
                .handler(move || {
                    let ledger = ledger.clone();
                    async move {
                        let stats = match &ledger {
                            Some(l) => collect(l).await.unwrap_or_default(),
                            None => Vec::new(),
                        };
                        Ok(ReadResourceResult {
                            contents: vec![ResourceContent {
                                uri: "ciacola://model-stats".to_string(),
                                mime_type: Some("application/json".to_string()),
                                text: Some(
                                    json!(stats.iter().map(stat_json).collect::<Vec<_>>())
                                        .to_string(),
                                ),
                                blob: None,
                                meta: None,
                            }],
                            ..Default::default()
                        })
                    }
                })
                .build(),
        ]
    }

    fn board_section(&self) -> BoxFut<'_, Option<Section>> {
        Box::pin(async move {
            let stats = collect(self.ledger.as_ref()?).await.ok()?;
            if stats.is_empty() {
                return None;
            }
            let mut html = String::from(
                "<table><tr><th>role</th><th>model</th><th>effort</th>\
                 <th class=\"num\">runs</th><th class=\"num\">failed</th>\
                 <th class=\"num\">median</th><th class=\"num\">median s</th>\
                 <th class=\"num\">total</th></tr>",
            );
            for s in &stats {
                html.push_str(&format!(
                    "<tr><td>{role}</td><td class=\"mono\">{model}</td>\
                     <td class=\"dim\">{effort}</td><td class=\"num\">{runs}</td>\
                     <td class=\"num\">{failed}</td><td class=\"num\">{median}</td>\
                     <td class=\"num\">{secs}</td><td class=\"num\">{total}</td></tr>",
                    role = ciacola_core::render::esc(&s.role),
                    model = ciacola_core::render::esc(&s.model),
                    effort = ciacola_core::render::esc(&s.effort),
                    runs = s.runs,
                    failed = if s.failures > 0 {
                        format!("<span style=\"color:#f85149\">{}</span>", s.failures)
                    } else {
                        "0".into()
                    },
                    median = ciacola_core::render::usd(s.median_cost_micro_usd),
                    secs = s.median_secs,
                    total = ciacola_core::render::usd(s.total_cost_micro_usd),
                ));
            }
            html.push_str("</table>");
            Some(Section {
                title: "cost by model".into(),
                html,
            })
        })
    }

    fn health(&self) -> BoxFut<'_, serde_json::Value> {
        Box::pin(async move {
            let stats = match &self.ledger {
                Some(l) => collect(l).await.unwrap_or_default(),
                None => Vec::new(),
            };
            json!({
                "buckets": stats.len(),
                "models": stats
                    .iter()
                    .map(|s| s.model.clone())
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciacola_core::agent::{AgentDef, Exchange};

    async fn ledger() -> Ledger {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        Ledger::setup(pool).await.expect("ledger")
    }

    async fn run_ok_turn(l: &Ledger, agent_id: &str, cost_micro_usd: i64, elapsed_ms: i64) {
        let seq = l.enqueue_turn(agent_id, "hi").await.expect("enqueue");
        assert!(l.claim_turn(agent_id, seq).await.expect("claim"));
        l.complete_turn(
            agent_id,
            seq,
            &Exchange {
                reply: "ok".into(),
                session: Some("s".into()),
                cost: ciacola_agent::Cost::Reported {
                    micro_usd: cost_micro_usd as u64,
                },
                usage: ciacola_agent::Usage::Reported(ciacola_agent::TokenUsage {
                    input: 1,
                    output: 1,
                    cached_input: 0,
                }),
                provider_turns: Some(1),
                elapsed_ms: elapsed_ms as u64,
                error: None,
            },
        )
        .await
        .expect("complete");
    }

    async fn run_failed_turn(
        l: &Ledger,
        agent_id: &str,
        state: &str,
        cost_micro_usd: i64,
        elapsed_ms: i64,
    ) {
        let seq = l.enqueue_turn(agent_id, "hi").await.expect("enqueue");
        assert!(l.claim_turn(agent_id, seq).await.expect("claim"));
        l.fail_turn(
            agent_id,
            seq,
            state,
            "boom",
            cost_micro_usd,
            elapsed_ms,
            None,
        )
        .await
        .expect("fail");
    }

    /// The point of the whole module: two models must keep their own
    /// totals rather than being summed into one row.
    #[tokio::test]
    async fn two_models_are_aggregated_separately() {
        let l = ledger().await;
        let sonnet = l
            .create_agent(&AgentDef::new("alpha", "sys").model("sonnet"), None)
            .await
            .expect("create sonnet agent");
        let haiku = l
            .create_agent(&AgentDef::new("beta", "sys").model("haiku"), None)
            .await
            .expect("create haiku agent");

        for cost in [100, 200, 300] {
            run_ok_turn(&l, &sonnet, cost, 1000).await;
        }
        for cost in [10, 20] {
            run_ok_turn(&l, &haiku, cost, 1000).await;
        }

        let stats = collect(&l).await.expect("collect");
        assert_eq!(stats.len(), 2, "two models must not merge into one bucket");

        let s = stats.iter().find(|s| s.model == "sonnet").expect("sonnet");
        assert_eq!(s.runs, 3);
        assert_eq!(s.total_cost_micro_usd, 600);

        let h = stats.iter().find(|s| s.model == "haiku").expect("haiku");
        assert_eq!(h.runs, 2);
        assert_eq!(
            h.total_cost_micro_usd, 30,
            "haiku's spend must not include sonnet's"
        );
    }

    /// Pins the decision the issue asked to pin: a failed turn still
    /// counts as a run and its cost still counts as spend, since a
    /// capped run can fail after money is already spent. It just also
    /// counts as a failure.
    #[tokio::test]
    async fn failed_turns_count_toward_runs_and_spend() {
        let l = ledger().await;
        let a = l
            .create_agent(&AgentDef::new("gamma", "sys").model("opus"), None)
            .await
            .expect("create");

        run_ok_turn(&l, &a, 100, 1000).await;
        run_ok_turn(&l, &a, 200, 1000).await;
        run_failed_turn(&l, &a, "failed", 50, 500).await;

        let stats = collect(&l).await.expect("collect");
        assert_eq!(stats.len(), 1);
        let s = &stats[0];
        assert_eq!(s.runs, 3, "a failed turn still counts as a run");
        assert_eq!(s.failures, 1);
        assert_eq!(
            s.total_cost_micro_usd, 350,
            "a failure that cost money still counts toward spend"
        );
    }

    /// No turns at all: an empty result, not a division by zero inside
    /// `median` or a row of nulls.
    #[tokio::test]
    async fn empty_ledger_produces_no_stats() {
        let l = ledger().await;
        let stats = collect(&l).await.expect("collect");
        assert!(stats.is_empty());
    }

    /// An agent that exists but has never finished a turn must not
    /// appear either; queued and running turns are excluded on purpose.
    #[tokio::test]
    async fn an_agent_with_no_finished_turns_produces_no_stats() {
        let l = ledger().await;
        l.create_agent(&AgentDef::new("idle", "sys"), None)
            .await
            .expect("create");
        let stats = collect(&l).await.expect("collect");
        assert!(stats.is_empty());
    }

    /// Same "empty" guarantee for a model whose turns all failed: it
    /// must still report a real bucket with a real median, not a zeroed
    /// or null row standing in for "no data".
    #[tokio::test]
    async fn a_model_with_only_failures_still_reports_a_real_median() {
        let l = ledger().await;
        let a = l
            .create_agent(&AgentDef::new("delta", "sys").model("sonnet"), None)
            .await
            .expect("create");

        run_failed_turn(&l, &a, "failed", 100, 1000).await;
        run_failed_turn(&l, &a, "killed", 300, 3000).await;

        let stats = collect(&l).await.expect("collect");
        assert_eq!(
            stats.len(),
            1,
            "an all-failed model must still produce a bucket, not vanish"
        );
        let s = &stats[0];
        assert_eq!(s.runs, 2);
        assert_eq!(s.failures, 2, "both failed and killed count as failures");
        assert_eq!(
            s.median_cost_micro_usd, 300,
            "median of the real costs, not a zeroed-out row"
        );
        assert_eq!(s.median_secs, 3);
    }

    /// The bucket key is `(provider, model, effort, role)` and only one
    /// provider exists in production today, so this path has never run
    /// through `collect`. `aggregate` is the pure grouping logic pulled
    /// out for exactly this: feed it two rows that agree on model,
    /// effort, and role but differ on provider, and confirm they land
    /// in separate buckets rather than merging.
    #[test]
    fn aggregate_keys_on_provider() {
        let entries = vec![
            (
                "claude".to_string(),
                "sonnet".to_string(),
                "high".to_string(),
                "role".to_string(),
                100,
                10,
                false,
            ),
            (
                "codex".to_string(),
                "sonnet".to_string(),
                "high".to_string(),
                "role".to_string(),
                200,
                20,
                false,
            ),
        ];

        let stats = aggregate(entries);
        assert_eq!(
            stats.len(),
            2,
            "identical model/effort/role but different providers must not merge"
        );
        let providers: std::collections::BTreeSet<_> =
            stats.iter().map(|s| s.provider.clone()).collect();
        assert!(providers.contains("claude"));
        assert!(providers.contains("codex"));
    }
}
