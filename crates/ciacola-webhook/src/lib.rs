//! A second event source, to prove the category is real.
//!
//! The original product sketch named three ways work arrives: submit it
//! over MCP, **ping the server**, or let cron find it. MCP and cron were
//! built; this is the ping.
//!
//! The point is what it did *not* need. There is no event-source trait,
//! no event bus, no typed event enum. A webhook is a plugin that owns a
//! route and calls [`PluginContext::submit_turn`], which is the same
//! call cron makes. They differ only in what wakes them. Someone who
//! wants a GitHub poller instead of, or alongside, either one writes a
//! third plugin that ends at the same line.
//!
//! Configured under `[plugins.webhook]`, so installing it is config
//! rather than an edit to `main`:
//!
//! ```toml
//! [plugins.webhook]
//! # POST /hook/<name> pokes the named agent with the request body.
//! hooks = [{ path = "issue", agent = "tower-mcp-manager" }]
//! ```
//!
//! Deliberately unauthenticated and bound to loopback, like everything
//! else here. A real deployment wants a shared secret at minimum; that
//! is a config field and a header check, and it is not built because
//! nothing has asked for it yet.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::routing::post;
use serde::Deserialize;
use serde_json::json;
use tower_mcp::{
    CallToolResult, ReadResourceResult, Resource, ResourceBuilder, ResourceContent, Tool,
    ToolBuilder,
};

use ciacola_core::agent::FlatError;
use ciacola_core::plugin::{BoxFut, Plugin, PluginContext, Section, Surface};
use ciacola_core::store::Store;

#[derive(Debug, Clone, Deserialize)]
pub struct Hook {
    /// URL segment: `POST /hook/<path>`.
    pub path: String,
    /// Agent name to poke. Resolved at fire time, so an agent
    /// recreated by config still receives its hooks.
    pub agent: String,
    /// Sent instead of the request body, when the body is noise.
    pub text: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WebhookConfig {
    #[serde(default)]
    hooks: Vec<Hook>,
}

#[derive(Clone)]
struct HookState {
    ctx: PluginContext,
    hooks: Arc<Vec<Hook>>,
    store: Store,
}

/// Fires are counted in the store so the board can show a hook that is
/// configured but has never been hit, which is the usual bug.
#[derive(Debug, Default, serde::Serialize, Deserialize)]
struct HookStats {
    fires: u64,
    skips: u64,
    rejects: u64,
    last_detail: Option<String>,
}

async fn receive(
    State(state): State<HookState>,
    Path(path): Path<String>,
    body: String,
) -> (axum::http::StatusCode, String) {
    use axum::http::StatusCode;

    let Some(hook) = state.hooks.iter().find(|h| h.path == path) else {
        return (StatusCode::NOT_FOUND, format!("no hook '{path}'\n"));
    };
    let Ok(Some(agent)) = state.ctx.ledger.find_active_by_name(&hook.agent).await else {
        return (
            StatusCode::NOT_FOUND,
            format!("hook '{path}' targets unknown agent '{}'\n", hook.agent),
        );
    };

    let text = hook
        .text
        .clone()
        .unwrap_or_else(|| body.trim().to_string())
        .trim()
        .to_string();
    if text.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "empty body and no configured text\n".to_string(),
        );
    }

    let outcome = state
        .ctx
        .submit_turn(&agent.agent_id, &text, &format!("webhook {path}"))
        .await;

    let key = format!("hook/{path}");
    let mut stats: HookStats = state
        .store
        .get(&key)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let (code, message) = match &outcome {
        ciacola_core::plugin::Submission::Submitted { seq } => {
            stats.fires += 1;
            stats.last_detail = Some(format!("submitted turn {seq}"));
            (StatusCode::ACCEPTED, format!("{} {seq}\n", agent.agent_id))
        }
        // Busy is 409, not 500: the caller may reasonably retry later,
        // and nothing went wrong.
        ciacola_core::plugin::Submission::Busy { .. } => {
            stats.skips += 1;
            stats.last_detail = Some("skipped, agent busy".into());
            (StatusCode::CONFLICT, "agent busy\n".to_string())
        }
        ciacola_core::plugin::Submission::OverBudget {
            spent_usd,
            limit_usd,
        } => {
            stats.rejects += 1;
            stats.last_detail = Some(format!("over budget: ${spent_usd:.2} of ${limit_usd:.2}"));
            // 429: the caller is not wrong, the system is saturated.
            (
                StatusCode::TOO_MANY_REQUESTS,
                format!("over daily budget (${spent_usd:.2} of ${limit_usd:.2})\n"),
            )
        }
        ciacola_core::plugin::Submission::Failed { reason } => {
            stats.rejects += 1;
            stats.last_detail = Some(format!("rejected: {reason}"));
            (StatusCode::UNPROCESSABLE_ENTITY, format!("{reason}\n"))
        }
    };
    let _ = state.store.put(&key, &stats).await;
    (code, message)
}

#[derive(Default)]
pub struct WebhookPlugin {
    hooks: Arc<Vec<Hook>>,
    ctx: Option<PluginContext>,
    store: Option<Store>,
}

impl WebhookPlugin {
    async fn stats(&self) -> Vec<(Hook, HookStats)> {
        let Some(store) = &self.store else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for hook in self.hooks.iter() {
            let stats = store
                .get::<HookStats>(&format!("hook/{}", hook.path))
                .await
                .ok()
                .flatten()
                .unwrap_or_default();
            out.push((hook.clone(), stats));
        }
        out
    }
}

impl Plugin for WebhookPlugin {
    fn name(&self) -> &'static str {
        "webhook"
    }

    // Key-value only, like refs: no tables, no migrations.

    fn setup<'a>(&'a mut self, ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async move {
            // Reads its own config section; main never sees these fields.
            let config: WebhookConfig = match ctx.config_for(self.name()) {
                Some(value) => value.clone().try_into()?,
                None => WebhookConfig::default(),
            };
            self.hooks = Arc::new(config.hooks);
            self.store = Some(Store::new(ctx.pool.clone(), self.name()));
            self.ctx = Some(ctx.clone());
            Ok(())
        })
    }

    fn routes(&self) -> Option<Router> {
        let (ctx, store) = (self.ctx.clone()?, self.store.clone()?);
        Some(
            Router::new()
                .route("/hook/{path}", post(receive))
                .with_state(HookState {
                    ctx,
                    hooks: self.hooks.clone(),
                    store,
                }),
        )
    }

    fn tools(&self, _surface: Surface) -> Vec<Tool> {
        let hooks = self.hooks.clone();
        vec![
            ToolBuilder::new("hooks")
                .description("Configured inbound webhooks and how often each has fired.")
                .read_only()
                .no_params_handler(move || {
                    let hooks = hooks.clone();
                    async move {
                        Ok(CallToolResult::json(json!({
                            "hooks": hooks
                                .iter()
                                .map(|h| json!({
                                    "path": h.path,
                                    "url": format!("POST /hook/{}", h.path),
                                    "agent": h.agent,
                                }))
                                .collect::<Vec<_>>()
                        })))
                    }
                })
                .build(),
        ]
    }

    fn resources(&self) -> Vec<Resource> {
        let hooks = self.hooks.clone();
        vec![
            ResourceBuilder::new("ciacola://hooks")
                .name("hooks")
                .description("Inbound webhook endpoints.")
                .mime_type("application/json")
                .handler(move || {
                    let hooks = hooks.clone();
                    async move {
                        Ok(ReadResourceResult {
                            contents: vec![ResourceContent {
                                uri: "ciacola://hooks".to_string(),
                                mime_type: Some("application/json".to_string()),
                                text: Some(
                                    json!(
                                        hooks
                                            .iter()
                                            .map(|h| json!({
                                                "path": h.path,
                                                "agent": h.agent
                                            }))
                                            .collect::<Vec<_>>()
                                    )
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
            let stats = self.stats().await;
            if stats.is_empty() {
                return None;
            }
            let mut html = String::from(
                "<table><tr><th>endpoint</th><th>agent</th><th class=\"num\">fires</th>\
                 <th class=\"num\">skips</th><th>last</th></tr>",
            );
            for (hook, s) in &stats {
                html.push_str(&format!(
                    "<tr><td class=\"mono\">POST /hook/{path}</td><td>{agent}</td>\
                     <td class=\"num\">{fires}</td><td class=\"num\">{skips}</td>\
                     <td class=\"dim\">{last}</td></tr>",
                    path = ciacola_core::board::esc(&hook.path),
                    agent = ciacola_core::board::esc(&hook.agent),
                    fires = s.fires,
                    skips = s.skips,
                    last =
                        ciacola_core::board::esc(s.last_detail.as_deref().unwrap_or("never fired")),
                ));
            }
            html.push_str("</table>");
            Some(Section {
                title: "webhooks".into(),
                html,
            })
        })
    }

    fn health(&self) -> BoxFut<'_, serde_json::Value> {
        Box::pin(async move {
            let stats = self.stats().await;
            json!({
                "hooks": stats.len(),
                "fires": stats.iter().map(|(_, s)| s.fires).sum::<u64>(),
                "skips": stats.iter().map(|(_, s)| s.skips).sum::<u64>(),
            })
        })
    }
}
