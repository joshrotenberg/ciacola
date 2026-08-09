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
//! Catalog-role provenance survives instance renaming. Provider, model,
//! and effort still come from the agent's current persisted definition,
//! however, so redefining a persistent agent can reclassify its historical
//! turns. Per-turn provisioning snapshots are a separate future concern.
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
    /// Turns with an actual monetary measurement in the cost sample.
    pub cost_samples: usize,
    /// Turns from a cost-reporting provider whose price was unavailable.
    pub cost_unreported: usize,
    /// Turns from a provider that never reports money.
    pub cost_not_priced: usize,
    /// Legacy zero values whose measurement status is unknowable.
    pub cost_legacy_unknown: usize,
    pub median_cost_micro_usd: Option<i64>,
    /// Turns with a measured provider duration.
    pub duration_samples: usize,
    /// Crash-recovered attempts whose claim-to-restart time is only a bound.
    pub duration_upper_bound: usize,
    /// Attempts whose runtime provenance could not be reconstructed.
    pub duration_unknown: usize,
    /// Historical attempted turns retained as runs but not durations.
    pub duration_legacy_unknown: usize,
    pub median_secs: Option<i64>,
    pub total_reported_cost_micro_usd: i64,
}

/// `(provider, model, effort, role)`.
type BucketKey = (String, String, String, String);

#[derive(Default)]
struct Bucket {
    costs: Vec<i64>,
    secs: Vec<i64>,
    runs: usize,
    failures: usize,
    cost_unreported: usize,
    cost_not_priced: usize,
    cost_legacy_unknown: usize,
    duration_upper_bound: usize,
    duration_unknown: usize,
    duration_legacy_unknown: usize,
}

/// One finished turn, flattened for grouping.
struct Entry {
    provider: String,
    model: String,
    effort: String,
    role: String,
    cost: Option<i64>,
    cost_state: String,
    elapsed_state: String,
    secs: i64,
    failed: bool,
}

fn median(mut values: Vec<i64>) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

/// The aggregation itself, pulled out of `collect` so it can be tested
/// without a database: feed it entries directly and it groups them the
/// same way a real ledger read would.
fn aggregate(entries: Vec<Entry>) -> Vec<Stat> {
    let mut buckets: BTreeMap<BucketKey, Bucket> = BTreeMap::new();
    for item in entries {
        let entry = buckets
            .entry((item.provider, item.model, item.effort, item.role))
            .or_default();
        entry.runs += 1;
        match item.elapsed_state.as_str() {
            "measured" => entry.secs.push(item.secs),
            "upper_bound" => entry.duration_upper_bound += 1,
            "unknown" => entry.duration_unknown += 1,
            _ => entry.duration_legacy_unknown += 1,
        }
        if item.failed {
            entry.failures += 1;
        }
        match item.cost {
            Some(cost) => entry.costs.push(cost),
            None if item.cost_state == "not_priced" => entry.cost_not_priced += 1,
            None if item.cost_state == "legacy" => entry.cost_legacy_unknown += 1,
            None => entry.cost_unreported += 1,
        }
    }

    buckets
        .into_iter()
        .map(|((provider, model, effort, role), bucket)| Stat {
            provider,
            model,
            effort,
            role,
            runs: bucket.runs,
            failures: bucket.failures,
            cost_samples: bucket.costs.len(),
            cost_unreported: bucket.cost_unreported,
            cost_not_priced: bucket.cost_not_priced,
            cost_legacy_unknown: bucket.cost_legacy_unknown,
            total_reported_cost_micro_usd: bucket.costs.iter().sum(),
            median_cost_micro_usd: median(bucket.costs),
            duration_samples: bucket.secs.len(),
            duration_upper_bound: bucket.duration_upper_bound,
            duration_unknown: bucket.duration_unknown,
            duration_legacy_unknown: bucket.duration_legacy_unknown,
            median_secs: median(bucket.secs),
        })
        .collect()
}

async fn collect(ledger: &Ledger) -> Result<Vec<Stat>, FlatError> {
    // Retired agents are included on purpose: an ephemeral spoke is
    // retired the moment its work is accepted, so excluding them would
    // throw away exactly the population worth measuring.
    let rows: Vec<(String, i64, String, i64, String, String)> = sqlx::query_as(
        "SELECT a.def, t.cost_micro_usd, t.cost_state, t.elapsed_ms,
                t.elapsed_state, t.state
         FROM turns t JOIN agents a ON a.agent_id = t.agent_id
         WHERE t.state IN ('ok', 'failed', 'killed')
           AND t.elapsed_state <> 'not_attempted'",
    )
    .fetch_all(ledger.pool())
    .await?;

    let entries = rows
        .into_iter()
        .filter_map(
            |(def_json, cost, cost_state, elapsed_ms, elapsed_state, state)| {
                let def = serde_json::from_str::<ciacola_core::agent::AgentDef>(&def_json).ok()?;
                let measured_cost = (cost_state == "reported"
                    || (cost_state == "legacy" && cost != 0))
                    .then_some(cost);
                Some(Entry {
                    // The backend each agent actually ran on, read from its
                    // own definition. Rows written before the field existed
                    // deserialize as claude, so history keeps its bucket
                    // rather than shifting when a second provider lands.
                    provider: def.provider.to_string(),
                    model: def.model.clone().unwrap_or_else(|| "(default)".into()),
                    effort: def.effort.clone().unwrap_or_else(|| "(default)".into()),
                    role: def.catalog_role().unwrap_or(&def.name).to_string(),
                    cost: measured_cost,
                    cost_state,
                    elapsed_state,
                    secs: elapsed_ms / 1000,
                    failed: state != "ok",
                })
            },
        )
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
        "cost_samples": s.cost_samples,
        "cost_unreported": s.cost_unreported,
        "cost_not_priced": s.cost_not_priced,
        "cost_legacy_unknown": s.cost_legacy_unknown,
        "median_cost_usd": s.median_cost_micro_usd.map(|cost| cost as f64 / 1e6),
        "duration_samples": s.duration_samples,
        "duration_upper_bound": s.duration_upper_bound,
        "duration_unknown": s.duration_unknown,
        "duration_legacy_unknown": s.duration_legacy_unknown,
        "median_secs": s.median_secs,
        "total_reported_cost_usd": s.total_reported_cost_micro_usd as f64 / 1e6,
        "total_cost_usd": s.total_reported_cost_micro_usd as f64 / 1e6,
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
                "<table><tr><th>provider</th><th>role</th><th>model</th><th>effort</th>\
                 <th class=\"num\">runs</th><th class=\"num\">failed</th>\
                 <th class=\"num\">cost samples</th><th class=\"num\">without cost</th>\
                 <th class=\"num\">median</th><th class=\"num\">duration samples</th>\
                 <th class=\"num\">without duration</th><th class=\"num\">median s</th>\
                 <th class=\"num\">reported total</th></tr>",
            );
            for s in &stats {
                let mut without_cost = Vec::new();
                if s.cost_unreported > 0 {
                    without_cost.push(format!("{} unreported", s.cost_unreported));
                }
                if s.cost_not_priced > 0 {
                    without_cost.push(format!("{} unpriced", s.cost_not_priced));
                }
                if s.cost_legacy_unknown > 0 {
                    without_cost.push(format!("{} legacy unknown", s.cost_legacy_unknown));
                }
                let without_cost = if without_cost.is_empty() {
                    "-".into()
                } else {
                    without_cost.join(" / ")
                };
                let mut without_duration = Vec::new();
                if s.duration_upper_bound > 0 {
                    without_duration.push(format!("{} upper bound", s.duration_upper_bound));
                }
                if s.duration_unknown > 0 {
                    without_duration.push(format!("{} unknown", s.duration_unknown));
                }
                if s.duration_legacy_unknown > 0 {
                    without_duration.push(format!("{} legacy", s.duration_legacy_unknown));
                }
                let without_duration = if without_duration.is_empty() {
                    "-".into()
                } else {
                    without_duration.join(" / ")
                };
                html.push_str(&format!(
                    "<tr><td class=\"mono\">{provider}</td><td>{role}</td>\
                     <td class=\"mono\">{model}</td>\
                     <td class=\"dim\">{effort}</td><td class=\"num\">{runs}</td>\
                     <td class=\"num\">{failed}</td><td class=\"num\">{samples}</td>\
                     <td class=\"num\">{without_cost}</td><td class=\"num\">{median}</td>\
                     <td class=\"num\">{duration_samples}</td>\
                     <td class=\"num\">{without_duration}</td>\
                     <td class=\"num\">{secs}</td><td class=\"num\">{total}</td></tr>",
                    provider = ciacola_core::render::esc(&s.provider),
                    role = ciacola_core::render::esc(&s.role),
                    model = ciacola_core::render::esc(&s.model),
                    effort = ciacola_core::render::esc(&s.effort),
                    runs = s.runs,
                    failed = if s.failures > 0 {
                        format!("<span style=\"color:#f85149\">{}</span>", s.failures)
                    } else {
                        "0".into()
                    },
                    samples = s.cost_samples,
                    without_cost = without_cost,
                    median = s
                        .median_cost_micro_usd
                        .map(ciacola_core::render::usd)
                        .unwrap_or_else(|| "-".into()),
                    duration_samples = s.duration_samples,
                    without_duration = without_duration,
                    secs = s
                        .median_secs
                        .map(|seconds| seconds.to_string())
                        .unwrap_or_else(|| "-".into()),
                    total = ciacola_core::render::usd(s.total_reported_cost_micro_usd),
                ));
            }
            html.push_str("</table>");
            Some(Section {
                title: "reported cost by model".into(),
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
    use ciacola_core::roles::{Role, Roles};

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
                cost_complete: true,
                usage: ciacola_agent::Usage::Reported(ciacola_agent::TokenUsage {
                    input: 1,
                    output: 1,
                    cached_input: 0,
                }),
                usage_complete: true,
                provider_turns: Some(1),
                elapsed_ms: elapsed_ms as u64,
                error: None,
                failure_kind: None,
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

    #[tokio::test]
    async fn renamed_role_instance_is_filtered_by_its_catalog_role() {
        let l = ledger().await;
        let role: Role = serde_json::from_value(json!({
            "name": "issue-implementer",
            "description": "implements one issue",
            "system_prompt": "implement it"
        }))
        .expect("role");
        let roles = Roles::new(vec![role], "agent.json");
        let mut def = roles.to_def(
            roles.get("issue-implementer").expect("catalog role"),
            &std::collections::HashMap::new(),
        );
        def.name = "impl-owner-repo-74".into();
        let agent_id = l.create_agent(&def, None).await.expect("agent");
        run_ok_turn(&l, &agent_id, 100, 1_000).await;

        let plugin = TuningPlugin {
            ledger: Some(l.clone()),
        };
        let tool = plugin
            .tools(Surface::Operator)
            .into_iter()
            .find(|tool| tool.definition().name == "model_stats")
            .expect("model_stats");
        let result = tool.call(json!({"role": "issue-implementer"})).await;
        let rendered = serde_json::to_string(&result).expect("result");

        assert!(rendered.contains("issue-implementer"), "{rendered}");
        assert!(rendered.contains("\"runs\":1"), "{rendered}");
        assert!(!rendered.contains("impl-owner-repo-74"), "{rendered}");
    }

    #[tokio::test]
    async fn direct_and_legacy_definitions_still_group_by_instance_name() {
        let l = ledger().await;
        let agent_id = l
            .create_agent(&AgentDef::new("direct-worker", "sys"), None)
            .await
            .expect("agent");
        run_ok_turn(&l, &agent_id, 100, 1_000).await;

        let stats = collect(&l).await.expect("stats");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].role, "direct-worker");
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
        assert_eq!(s.total_reported_cost_micro_usd, 600);
        assert_eq!(s.cost_samples, 3);

        let h = stats.iter().find(|s| s.model == "haiku").expect("haiku");
        assert_eq!(h.runs, 2);
        assert_eq!(
            h.total_reported_cost_micro_usd, 30,
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
            s.total_reported_cost_micro_usd, 350,
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
            s.median_cost_micro_usd,
            Some(300),
            "median of the real costs, not a zeroed-out row"
        );
        assert_eq!(s.median_secs, Some(3));
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
            Entry {
                provider: "claude".into(),
                model: "sonnet".into(),
                effort: "high".into(),
                role: "role".into(),
                cost: Some(100),
                cost_state: "reported".into(),
                elapsed_state: "measured".into(),
                secs: 10,
                failed: false,
            },
            Entry {
                provider: "codex".into(),
                model: "sonnet".into(),
                effort: "high".into(),
                role: "role".into(),
                cost: Some(200),
                cost_state: "reported".into(),
                elapsed_state: "measured".into(),
                secs: 20,
                failed: false,
            },
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

    /// A real zero-cost report is a sample. An unavailable cost is a
    /// run and a failure, but it cannot pull the median down as though
    /// the provider had measured zero dollars.
    #[test]
    fn reported_zero_and_unknown_cost_are_distinct_samples() {
        let entries = vec![
            Entry {
                provider: "claude".into(),
                model: "sonnet".into(),
                effort: "high".into(),
                role: "role".into(),
                cost: Some(0),
                cost_state: "reported".into(),
                elapsed_state: "measured".into(),
                secs: 1,
                failed: false,
            },
            Entry {
                provider: "claude".into(),
                model: "sonnet".into(),
                effort: "high".into(),
                role: "role".into(),
                cost: None,
                cost_state: "unreported".into(),
                elapsed_state: "upper_bound".into(),
                secs: 3,
                failed: true,
            },
        ];

        let stat = aggregate(entries).pop().expect("one bucket");
        assert_eq!(stat.runs, 2);
        assert_eq!(stat.failures, 1);
        assert_eq!(stat.cost_samples, 1);
        assert_eq!(stat.cost_unreported, 1);
        assert_eq!(stat.cost_not_priced, 0);
        assert_eq!(stat.cost_legacy_unknown, 0);
        assert_eq!(stat.median_cost_micro_usd, Some(0));
        assert_eq!(stat.total_reported_cost_micro_usd, 0);
        assert_eq!(stat.duration_samples, 1);
        assert_eq!(stat.duration_upper_bound, 1);
        assert_eq!(stat.median_secs, Some(1));
        let json = stat_json(&stat);
        assert_eq!(json["total_cost_usd"], json["total_reported_cost_usd"]);
    }

    /// A queued kill is a terminal record but not a provider run. The
    /// explicit provenance excludes it without using a null claim-time
    /// heuristic, so pre-migration attempted history remains visible.
    #[tokio::test]
    async fn non_attempts_are_excluded_while_legacy_attempts_are_preserved() {
        let l = ledger().await;
        let agent_id = l
            .create_agent(&AgentDef::new("history", "sys"), None)
            .await
            .expect("agent");

        let queued = l
            .enqueue_turn(&agent_id, "never launched")
            .await
            .expect("turn");
        assert!(
            l.interrupt_turn(&agent_id, queued, "killed", "stopped while queued")
                .await
                .expect("interrupt")
        );
        assert!(collect(&l).await.expect("collect").is_empty());

        let legacy = l
            .enqueue_turn(&agent_id, "historical attempt")
            .await
            .expect("turn");
        sqlx::query(
            "UPDATE turns SET state = 'failed', error = 'legacy failure',
                 elapsed_ms = 4_000, elapsed_state = 'legacy', cost_state = 'legacy'
             WHERE agent_id = ?1 AND seq = ?2",
        )
        .bind(&agent_id)
        .bind(legacy)
        .execute(l.pool())
        .await
        .expect("simulate pre-migration row");

        let stats = collect(&l).await.expect("collect");
        assert_eq!(stats.len(), 1);
        let stat = &stats[0];
        assert_eq!(stat.runs, 1);
        assert_eq!(stat.failures, 1);
        assert_eq!(stat.cost_samples, 0);
        assert_eq!(stat.cost_legacy_unknown, 1);
        assert_eq!(stat.duration_samples, 0);
        assert_eq!(stat.duration_legacy_unknown, 1);
        assert_eq!(stat.median_secs, None);
    }
}
