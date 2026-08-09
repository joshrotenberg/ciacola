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
        ciacola_core::plugin::Submission::Submitted { seq, .. } => {
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
        ciacola_core::plugin::Submission::OverTokens {
            provider,
            used_tokens,
            limit_tokens,
        } => {
            stats.rejects += 1;
            stats.last_detail = Some(format!(
                "over {provider} token limit: {used_tokens} of {limit_tokens}"
            ));
            (
                StatusCode::TOO_MANY_REQUESTS,
                format!("over {provider} daily token limit ({used_tokens} of {limit_tokens})\n"),
            )
        }
        ciacola_core::plugin::Submission::Unobservable { provider, reason } => {
            stats.rejects += 1;
            stats.last_detail = Some(format!("{provider} automatic admission refused: {reason}"));
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("{provider} automatic admission refused: {reason}\n"),
            )
        }
        ciacola_core::plugin::Submission::Unguarded { provider, reason } => {
            stats.rejects += 1;
            stats.last_detail = Some(format!("{provider} automatic admission refused: {reason}"));
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("{provider} automatic admission refused: {reason}\n"),
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
                    path = ciacola_core::render::esc(&hook.path),
                    agent = ciacola_core::render::esc(&hook.agent),
                    fires = s.fires,
                    skips = s.skips,
                    last = ciacola_core::render::esc(
                        s.last_detail.as_deref().unwrap_or("never fired")
                    ),
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use sqlx::SqlitePool;

    use ciacola_core::agent::AgentDef;
    use ciacola_core::exec::TurnExecutor;
    use ciacola_core::ledger::Ledger;
    use ciacola_core::notify::Notifier;

    use super::*;

    struct ReportingProvider;

    impl ciacola_agent::Provider for ReportingProvider {
        fn key(&self) -> ciacola_agent::ProviderKey {
            ciacola_agent::ProviderKey::claude()
        }

        fn capabilities(&self) -> ciacola_agent::Capabilities {
            let mut capabilities = ciacola_agent::Capabilities::none(self.key());
            capabilities.reports_cost = true;
            capabilities.reports_token_usage = true;
            capabilities
        }

        fn run<'a>(
            &'a self,
            _intent: &'a ciacola_agent::TurnIntent,
            _events: &'a dyn ciacola_agent::TurnEvents,
        ) -> ciacola_agent::BoxFut<'a, Result<ciacola_agent::TurnOutcome, ciacola_agent::AgentError>>
        {
            Box::pin(async { unreachable!("webhook tests do not run providers") })
        }

        fn owns_process(&self, _ps_line: &str) -> bool {
            false
        }
    }

    struct UnpricedProvider;

    impl ciacola_agent::Provider for UnpricedProvider {
        fn key(&self) -> ciacola_agent::ProviderKey {
            ciacola_agent::ProviderKey::codex()
        }

        fn capabilities(&self) -> ciacola_agent::Capabilities {
            let mut capabilities = ciacola_agent::Capabilities::none(self.key());
            capabilities.reports_token_usage = true;
            capabilities
        }

        fn run<'a>(
            &'a self,
            _intent: &'a ciacola_agent::TurnIntent,
            _events: &'a dyn ciacola_agent::TurnEvents,
        ) -> ciacola_agent::BoxFut<'a, Result<ciacola_agent::TurnOutcome, ciacola_agent::AgentError>>
        {
            Box::pin(async { unreachable!("webhook tests do not run providers") })
        }

        fn owns_process(&self, _ps_line: &str) -> bool {
            false
        }
    }

    /// Records what it was handed and does nothing else, the same
    /// CI-safe stand-in `ciacola-schedule` uses: nothing here shells
    /// out to a provider, so a submitted turn stays `queued` forever,
    /// which is exactly what "the agent is busy" means to the
    /// admission guard in `Ledger::enqueue_turn`.
    #[derive(Default)]
    struct RecordingExecutor {
        submitted: Mutex<Vec<(String, i64)>>,
    }

    impl TurnExecutor for RecordingExecutor {
        fn submit(&self, agent_id: String, seq: i64) {
            self.submitted.lock().unwrap().push((agent_id, seq));
        }
        fn kill(&self, _agent_id: &str, _seq: i64) -> bool {
            false
        }
        fn name(&self) -> &'static str {
            "recording"
        }
    }

    /// `plugin_kv` is core's table, applied by `PluginHost::setup`
    /// before any plugin runs. Duplicated here rather than dragged in
    /// through a full `PluginContext`, the same shortcut `ciacola-refs`
    /// takes for its own `Store`-backed tests.
    async fn kv_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.expect("pool");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS plugin_kv (
                 plugin TEXT NOT NULL,
                 key TEXT NOT NULL,
                 value TEXT NOT NULL,
                 updated_unix INTEGER NOT NULL,
                 PRIMARY KEY (plugin, key));",
        )
        .execute(&pool)
        .await
        .expect("create plugin_kv");
        pool
    }

    async fn setup(hooks: Vec<Hook>) -> (HookState, Arc<RecordingExecutor>, Ledger, SqlitePool) {
        let pool = kv_pool().await;
        let providers = ciacola_agent::ProviderRegistry::new()
            .with(Arc::new(ReportingProvider))
            .and_then(|providers| providers.with(Arc::new(UnpricedProvider)))
            .expect("providers");
        let ledger = Ledger::setup(pool.clone())
            .await
            .expect("ledger")
            .with_providers(providers);
        let (tx, _rx) = tower_mcp::context::notification_channel(8);
        let exec = Arc::new(RecordingExecutor::default());
        let ctx = PluginContext {
            pool: pool.clone(),
            ledger: ledger.clone(),
            exec: exec.clone() as Arc<dyn TurnExecutor>,
            notify: Notifier(tx),
            db_path: String::new(),
            loopback_mcp_config: String::new(),
            operator_mcp_config: String::new(),
            plugin_config: toml::Value::Table(toml::map::Map::new()),
            limits: Default::default(),
            runtime: Default::default(),
        };
        let state = HookState {
            ctx,
            hooks: Arc::new(hooks),
            store: Store::new(pool.clone(), "webhook"),
        };
        (state, exec, ledger, pool)
    }

    async fn new_agent(ledger: &Ledger, name: &str) -> String {
        ledger
            .create_agent(&AgentDef::new(name, "sys"), None)
            .await
            .expect("create agent")
    }

    async fn turn_count(pool: &SqlitePool) -> i64 {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM turns")
            .fetch_one(pool)
            .await
            .expect("count");
        count
    }

    fn hook(path: &str, agent: &str) -> Hook {
        Hook {
            path: path.into(),
            agent: agent.into(),
            text: None,
        }
    }

    #[tokio::test]
    async fn configured_path_fires_only_its_named_agent() {
        let (state, exec, ledger, pool) = setup(vec![hook("issue", "a"), hook("other", "b")]).await;
        let a = new_agent(&ledger, "a").await;
        let _b = new_agent(&ledger, "b").await;

        let (status, body) = receive(State(state), Path("issue".into()), "hi".into()).await;

        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(
            exec.submitted.lock().unwrap().as_slice(),
            [(a.clone(), 1)],
            "a hit on one configured path must dispatch exactly the agent it names, and no other"
        );
        assert_eq!(
            turn_count(&pool).await,
            1,
            "one hit must produce exactly one turn total"
        );
    }

    #[tokio::test]
    async fn unconfigured_path_produces_nothing() {
        let (state, exec, _ledger, pool) = setup(vec![hook("issue", "a")]).await;

        let (status, body) = receive(State(state), Path("nope".into()), "hi".into()).await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(
            exec.submitted.lock().unwrap().is_empty(),
            "an unconfigured path must never reach the executor"
        );
        assert_eq!(
            turn_count(&pool).await,
            0,
            "an unconfigured path must produce nothing: not an error turn, not a default agent"
        );
    }

    #[tokio::test]
    async fn retired_target_agent_produces_nothing() {
        let (state, exec, ledger, pool) = setup(vec![hook("issue", "a")]).await;
        let a = new_agent(&ledger, "a").await;
        assert!(ledger.retire_agent(&a).await.expect("retire"));

        let (status, _body) = receive(State(state), Path("issue".into()), "hi".into()).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(exec.submitted.lock().unwrap().is_empty());
        assert_eq!(
            turn_count(&pool).await,
            0,
            "a hook whose target retired since config was read must not be bypassed into a turn"
        );
    }

    #[tokio::test]
    async fn busy_target_agent_is_skipped_through_the_same_admission_path() {
        let (state, exec, ledger, pool) = setup(vec![hook("issue", "a")]).await;
        let a = new_agent(&ledger, "a").await;
        ledger
            .enqueue_turn(&a, "already running")
            .await
            .expect("enqueue");

        let (status, _body) = receive(State(state), Path("issue".into()), "hi".into()).await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            exec.submitted.lock().unwrap().is_empty(),
            "a busy agent must be skipped, not queued a second time"
        );
        assert_eq!(
            turn_count(&pool).await,
            1,
            "the hook must go through the same admission as any other submission path, not bypass it"
        );
    }

    #[tokio::test]
    async fn over_budget_is_refused_through_the_same_admission_path() {
        let (mut state, exec, ledger, pool) = setup(vec![hook("issue", "a")]).await;
        state.ctx.limits.daily_stop_usd = Some(0.0);
        let _a = new_agent(&ledger, "a").await;

        let (status, body) = receive(State(state), Path("issue".into()), "hi".into()).await;

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        assert!(exec.submitted.lock().unwrap().is_empty());
        assert_eq!(
            turn_count(&pool).await,
            0,
            "the daily spend limit must be enforced on the webhook path exactly like any other, \
             per plugin::submit being the one convergence point"
        );
    }

    #[tokio::test]
    async fn unguarded_provider_is_a_non_retryable_configuration_error() {
        let (state, exec, ledger, pool) = setup(vec![hook("issue", "a")]).await;
        ledger
            .create_agent(&AgentDef::new("a", "sys").provider("codex"), None)
            .await
            .expect("create agent");

        let (status, body) = receive(State(state), Path("issue".into()), "hi".into()).await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(body.contains("unpriced provider"), "{body}");
        assert!(exec.submitted.lock().unwrap().is_empty());
        assert_eq!(turn_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn empty_body_and_no_configured_text_is_rejected_without_a_turn() {
        let (state, exec, ledger, pool) = setup(vec![hook("issue", "a")]).await;
        let _a = new_agent(&ledger, "a").await;

        let (status, body) = receive(State(state), Path("issue".into()), "   \n  ".into()).await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(exec.submitted.lock().unwrap().is_empty());
        assert_eq!(turn_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn request_body_round_trips_into_the_prompt() {
        let (state, _exec, ledger, _pool) = setup(vec![hook("issue", "a")]).await;
        let a = new_agent(&ledger, "a").await;

        let (status, body) = receive(
            State(state),
            Path("issue".into()),
            "  fix the thing  \n".into(),
        )
        .await;

        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let turn = ledger
            .get_turn(&a, 1)
            .await
            .expect("get_turn")
            .expect("turn exists");
        assert_eq!(
            turn.prompt, "fix the thing",
            "the body must round-trip, trimmed, into the prompt"
        );
    }

    #[tokio::test]
    async fn configured_text_overrides_the_request_body() {
        let (state, _exec, ledger, _pool) = setup(vec![Hook {
            path: "issue".into(),
            agent: "a".into(),
            text: Some("fixed prompt".into()),
        }])
        .await;
        let a = new_agent(&ledger, "a").await;

        let (status, _body) =
            receive(State(state), Path("issue".into()), "ignored body".into()).await;

        assert_eq!(status, StatusCode::ACCEPTED);
        let turn = ledger
            .get_turn(&a, 1)
            .await
            .expect("get_turn")
            .expect("turn exists");
        assert_eq!(turn.prompt, "fixed prompt");
    }
}
