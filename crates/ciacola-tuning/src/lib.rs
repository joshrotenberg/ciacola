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

fn median(mut values: Vec<i64>) -> i64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
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

    let mut buckets: BTreeMap<BucketKey, Bucket> = BTreeMap::new();
    for (def_json, cost, elapsed_ms, state) in rows {
        let Ok(def) = serde_json::from_str::<ciacola_core::agent::AgentDef>(&def_json) else {
            continue;
        };
        let key = (
            // Only one provider so far; the field exists so the shape
            // survives a second one.
            "claude".to_string(),
            def.model.clone().unwrap_or_else(|| "(default)".into()),
            def.effort.clone().unwrap_or_else(|| "(default)".into()),
            def.name.clone(),
        );
        let entry = buckets.entry(key).or_default();
        entry.0.push(cost);
        entry.1.push(elapsed_ms / 1000);
        if state != "ok" {
            entry.2 += 1;
        }
    }

    Ok(buckets
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
        .collect())
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
                    role = ciacola_core::board::esc(&s.role),
                    model = ciacola_core::board::esc(&s.model),
                    effort = ciacola_core::board::esc(&s.effort),
                    runs = s.runs,
                    failed = if s.failures > 0 {
                        format!("<span style=\"color:#f85149\">{}</span>", s.failures)
                    } else {
                        "0".into()
                    },
                    median = ciacola_core::board::usd(s.median_cost_micro_usd),
                    secs = s.median_secs,
                    total = ciacola_core::board::usd(s.total_cost_micro_usd),
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
