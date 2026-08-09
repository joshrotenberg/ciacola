//! The read side, rendered.
//!
//! Server-rendered HTML over the same tables every MCP tool reads.
//! Optional by construction: nothing in core depends on this crate, so
//! a server that does not want a board simply does not merge its
//! router.
//!
//! # Live without a build step
//!
//! The board used to carry `<meta http-equiv="refresh" content="5">`,
//! which reloads the whole page: scroll position lost, a flash every
//! five seconds, and a reload whether or not anything changed.
//!
//! Instead the server holds an SSE connection, renders the body on a
//! tick, and sends a version only when the rendering actually differs.
//! The client refetches one fragment and swaps it. Scroll survives,
//! nothing flashes, and an idle system sends nothing at all.
//!
//! That is the LiveView shape (server owns the state, client is dumb)
//! reached with about forty lines of vanilla JavaScript and no
//! dependency. It is deliberately not htmx or Leptos yet: the state
//! here is entirely server-side and the interactions are coarse, so
//! neither has anything to add until the board grows client state that
//! a round trip cannot serve. Whatever replaces this should have to
//! justify a build step first.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, State};
use axum::response::sse::{Event, Sse};
use axum::response::{Html, Redirect};
use axum::routing::get;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use ciacola_core::ledger::{Ledger, TurnRow};
use ciacola_core::limits::{AdmissionReport, AdmissionState, ProviderAdmissionStatus};
use ciacola_core::plugin::PluginHost;
use ciacola_core::render::{ago, chip, esc, human_count, page_with, usd};

#[derive(Clone)]
struct BoardState {
    ledger: Ledger,
    host: Arc<PluginHost>,
    limits: ciacola_core::limits::Limits,
    shutdown: CancellationToken,
}

/// The board knows core (agents) and asks the host for everything
/// else. Adding a plugin adds a section with no change here, which is
/// what `Option<Items>`, `Option<Findings>`, and three constructors
/// were failing to achieve.
pub fn router(ledger: Ledger, host: Arc<PluginHost>) -> Router {
    router_with_limits(ledger, host, Default::default(), CancellationToken::new())
}

/// `shutdown` is the same token the caller cancels to start graceful
/// shutdown of the whole server. The board's own long-lived response,
/// `/board/events`, watches it too: without that, the SSE loop never
/// ends on its own and a graceful shutdown waits on it forever.
pub fn router_with_limits(
    ledger: Ledger,
    host: Arc<PluginHost>,
    limits: ciacola_core::limits::Limits,
    shutdown: CancellationToken,
) -> Router {
    let plugin_routes = host.routes();
    Router::new()
        .route("/", get(|| async { Redirect::to("/board") }))
        .route("/board", get(overview))
        .route("/board/fragment", get(overview_fragment))
        .route("/board/events", get(events))
        .route("/board/agent/{agent_id}", get(agent_page))
        .with_state(BoardState {
            ledger,
            host,
            limits,
            shutdown,
        })
        .merge(plugin_routes)
}

fn human_u64_count(value: u64) -> String {
    match value {
        0..=999 => value.to_string(),
        1_000..=999_999 => format!("{:.1}k", value as f64 / 1e3),
        _ => format!("{:.1}M", value as f64 / 1e6),
    }
}

fn usd_u64(micro_usd: u64) -> String {
    format!("${:.4}", micro_usd as f64 / 1e6)
}

fn optional_count(value: Option<u64>) -> String {
    value
        .map(human_u64_count)
        .unwrap_or_else(|| "&mdash;".into())
}

fn optional_usd(value: Option<u64>) -> String {
    value.map(usd_u64).unwrap_or_else(|| "&mdash;".into())
}

fn telemetry_gaps(status: &ProviderAdmissionStatus) -> String {
    let accounting = &status.accounting;
    let mut gaps = Vec::new();
    if accounting.usage_incomplete_turns > 0 {
        gaps.push(format!(
            "tokens partial {}",
            human_u64_count(accounting.usage_incomplete_turns)
        ));
    }
    if accounting.usage_unreported_turns > 0 {
        gaps.push(format!(
            "tokens unreported {}",
            human_u64_count(accounting.usage_unreported_turns)
        ));
    }
    if accounting.usage_not_tracked_turns > 0 {
        gaps.push(format!(
            "tokens not tracked {}",
            human_u64_count(accounting.usage_not_tracked_turns)
        ));
    }
    if accounting.usage_legacy_unknown_turns > 0 {
        gaps.push(format!(
            "tokens legacy {}",
            human_u64_count(accounting.usage_legacy_unknown_turns)
        ));
    }
    if accounting.cost_unreported_turns > 0 {
        gaps.push(format!(
            "cost unreported {}",
            human_u64_count(accounting.cost_unreported_turns)
        ));
    }
    if accounting.cost_not_priced_turns > 0 && accounting.reports_cost {
        gaps.push(format!(
            "cost not priced {}",
            human_u64_count(accounting.cost_not_priced_turns)
        ));
    }
    if accounting.cost_legacy_unknown_turns > 0 {
        gaps.push(format!(
            "cost legacy {}",
            human_u64_count(accounting.cost_legacy_unknown_turns)
        ));
    }
    if accounting.running_partial_turns > 0 {
        gaps.push(format!(
            "running partial {}",
            human_u64_count(accounting.running_partial_turns)
        ));
    }
    if gaps.is_empty() {
        "none".into()
    } else {
        esc(&gaps.join("; "))
    }
}

fn admission_state(status: &ProviderAdmissionStatus) -> String {
    if status.accounting.active_agents == 0 {
        return format!(
            "INACTIVE <span class=\"dim\">{} if selected</span>",
            status.state.as_str()
        );
    }
    let label = match status.state {
        AdmissionState::Stopped => "<b style=\"color:#f85149\">STOPPED</b>".to_string(),
        AdmissionState::Warning => "<b style=\"color:#d29922\">WARNING</b>".to_string(),
        AdmissionState::Unobservable => concat!(
            "<b style=\"color:#f85149\">AUTO BLOCKED</b>",
            " <span class=\"dim\">unobservable</span>"
        )
        .to_string(),
        AdmissionState::Unguarded => concat!(
            "<b style=\"color:#f85149\">AUTO BLOCKED</b>",
            " <span class=\"dim\">unguarded</span>"
        )
        .to_string(),
        AdmissionState::Ok => "OK".to_string(),
    };
    match &status.detail {
        Some(detail) => format!("{label}<br><span class=\"dim\">{}</span>", esc(detail)),
        None => label,
    }
}

fn global_admission_state(state: AdmissionState) -> String {
    match state {
        AdmissionState::Stopped => "<b style=\"color:#f85149\">STOPPED</b>".into(),
        AdmissionState::Warning => "<b style=\"color:#d29922\">WARNING</b>".into(),
        AdmissionState::Unobservable => "<b style=\"color:#f85149\">UNOBSERVABLE</b>".into(),
        AdmissionState::Unguarded => "<b style=\"color:#f85149\">UNGUARDED</b>".into(),
        AdmissionState::Ok => "OK".into(),
    }
}

fn admission_section(report: Result<AdmissionReport, ciacola_core::FlatError>) -> String {
    let report = match report {
        Ok(report) => report,
        Err(error) => {
            return format!(
                "<h2>admission <span class=\"dim\">rolling 24h</span></h2>\
                 <div class=\"msg err\">admission report error: {}</div>",
                esc(&error.to_string())
            );
        }
    };

    let mut html = format!(
        "<h2>admission <span class=\"dim\">rolling 24h</span></h2>\
         <p class=\"dim\">USD admission: <b>{spend}</b> reported &middot; warn {warn} &middot; \
         stop {stop} &middot; cost telemetry gaps {gaps} &middot; state {state}</p>\
         <table><tr><th>provider</th><th class=\"num\">active agents</th>\
         <th class=\"num\">reported total</th>\
         <th class=\"num\">input</th><th class=\"num\">output</th>\
         <th class=\"num\">cached <span class=\"dim\">(included in input)</span></th>\
         <th class=\"num\">warn</th><th class=\"num\">stop</th>\
         <th>telemetry gaps</th><th>state</th><th>automatic</th></tr>",
        spend = usd_u64(report.global.reported_spend_micro_usd),
        warn = optional_usd(report.global.daily_warn_micro_usd),
        stop = optional_usd(report.global.daily_stop_micro_usd),
        gaps = human_u64_count(report.global.cost_gaps),
        state = global_admission_state(report.global.state),
    );
    for status in &report.providers {
        let accounting = &status.accounting;
        html.push_str(&format!(
            "<tr><td>{provider}</td><td class=\"num\">{active}</td>\
             <td class=\"num\">{total}</td>\
             <td class=\"num\">{input}</td><td class=\"num\">{output}</td>\
             <td class=\"num\">{cached}</td><td class=\"num\">{warn}</td>\
             <td class=\"num\">{stop}</td><td class=\"dim\">{gaps}</td>\
             <td>{state}</td><td>{automatic}</td></tr>",
            provider = esc(&accounting.provider),
            active = human_u64_count(accounting.active_agents),
            total = human_u64_count(accounting.total_tokens()),
            input = human_u64_count(accounting.tokens_in),
            output = human_u64_count(accounting.tokens_out),
            cached = human_u64_count(accounting.tokens_cached),
            warn = optional_count(status.daily_warn_tokens),
            stop = optional_count(status.daily_stop_tokens),
            gaps = telemetry_gaps(status),
            state = admission_state(status),
            automatic = if accounting.active_agents == 0 {
                "<span class=\"dim\">n/a</span>"
            } else if status.automatic_allowed {
                "yes"
            } else {
                "<b style=\"color:#f85149\">no</b>"
            },
        ));
    }
    html.push_str("</table>");
    html
}

async fn overview_body(state: &BoardState) -> String {
    let agents = match state.ledger.list_agents().await {
        Ok(agents) => agents,
        Err(e) => return format!("<p>ledger error: {}</p>", esc(&e.to_string())),
    };
    let retired = state.ledger.retired_count().await.unwrap_or_default();

    // Totals include the retired: retirement hides agents, never money.
    let (total_cost, total_turns) = state.ledger.totals().await.unwrap_or_default();
    let running = agents.iter().filter(|a| a.state == "running").count();
    let day_cost = state
        .ledger
        .spend_since(ciacola_core::time::now_unix() - 86_400)
        .await
        .unwrap_or_default();
    let tokens = state.ledger.token_totals().await.unwrap_or_default();

    let mut body = format!(
        "<h1>ciacola</h1>\
         <div><span class=\"stat\"><b>{}</b><span>agents</span></span>\
         <span class=\"stat\"><b>{}</b><span>running</span></span>\
         <span class=\"stat\"><b>{}</b><span>turns</span></span>\
         <span class=\"stat\"><b>{}</b><span>reported spend</span></span>\
         <span class=\"stat\"><b>{}</b><span>reported last 24h{}</span></span>\
         <span class=\"stat\"><b>{}</b><span>reported tokens</span></span>\
         <span class=\"stat\"><b>{}</b><span>retired</span></span></div>",
        agents.len(),
        running,
        total_turns,
        usd(total_cost),
        usd(day_cost),
        // The limit rides on the number it governs, and turns amber
        // past the warning, red past the stop.
        match (state.limits.warn_micro_usd(), state.limits.stop_micro_usd()) {
            (_, Some(stop)) if day_cost >= stop => format!(
                " <span style=\"color:#f85149\">of {} STOPPED</span>",
                usd(stop)
            ),
            (Some(warn), Some(stop)) if day_cost >= warn =>
                format!(" <span style=\"color:#d29922\">of {}</span>", usd(stop)),
            (_, Some(stop)) => format!(" <span class=\"dim\">of {}</span>", usd(stop)),
            _ => String::new(),
        },
        human_count(tokens.0.saturating_add(tokens.1)),
        retired,
    );

    body.push_str(&admission_section(
        state.ledger.admission_report(&state.limits).await,
    ));

    // Whatever the plugins contribute, in registration order.
    for section in state.host.board_sections().await {
        body.push_str(&format!("<h2>{}</h2>{}", esc(&section.title), section.html));
    }

    // Attention first: agents whose latest turn went wrong. The
    // proto-needs-you list; gates will feed this for real.
    let mut attention = String::new();
    for agent in &agents {
        if let Ok(Some(turn)) = state.ledger.get_turn(&agent.agent_id, agent.turns).await {
            if turn.state == "failed" || turn.state == "killed" {
                attention.push_str(&format!(
                    "<tr><td><a href=\"/board/agent/{id}\">{name}</a></td><td>{chip}</td>\
                     <td class=\"dim\">{err}</td></tr>",
                    id = esc(&agent.agent_id),
                    name = esc(&agent.name),
                    chip = chip(&turn.state),
                    err = esc(turn.error.as_deref().unwrap_or("")),
                ));
            }
        }
    }
    if !attention.is_empty() {
        body.push_str(&format!(
            "<h2>needs a look</h2><table><tr><th>agent</th><th>last turn</th><th>why</th></tr>{attention}</table>"
        ));
    }

    body.push_str(
        "<h2>agents</h2><table><tr><th>name</th><th>state</th><th>provider</th>\
        <th class=\"num\">turns</th><th class=\"num\">reported cost</th><th>last active</th>\
        <th>session</th></tr>",
    );
    // Families together: roots first, each followed by its children.
    let row_html = |agent: &ciacola_core::ledger::AgentRow, child: bool| {
        format!(
            "<tr><td>{indent}<a href=\"/board/agent/{id}\">{name}</a> <span class=\"dim mono\">{short}</span></td>\
             <td>{chip}</td><td class=\"dim\">{provider}</td><td class=\"num\">{turns}</td><td class=\"num\">{cost}</td>\
             <td class=\"dim\">{active}</td><td class=\"dim mono\">{session}</td></tr>",
            indent = if child {
                "<span class=\"dim\">&nbsp;&nbsp;&#8627;&nbsp;</span>"
            } else {
                ""
            },
            id = esc(&agent.agent_id),
            name = esc(&agent.name),
            short = esc(&agent.agent_id[agent.agent_id.len().saturating_sub(6)..]),
            chip = chip(&agent.state),
            provider = esc(agent.def.provider.as_str()),
            turns = agent.turns,
            cost = usd(agent.cost_micro_usd),
            active = ago(agent.last_active_unix),
            session = esc(agent
                .session
                .as_deref()
                .map(|s| &s[..s.len().min(8)])
                .unwrap_or("-")),
        )
    };
    let is_root = |a: &&ciacola_core::ledger::AgentRow| {
        a.spawned_by.is_none()
            || !agents
                .iter()
                .any(|p| Some(&p.agent_id) == a.spawned_by.as_ref())
    };
    for root in agents.iter().filter(is_root) {
        body.push_str(&row_html(root, false));
        for child in agents
            .iter()
            .filter(|a| a.spawned_by.as_ref() == Some(&root.agent_id))
        {
            body.push_str(&row_html(child, true));
        }
    }
    body.push_str("</table>");

    body
}

async fn overview(State(state): State<BoardState>) -> Html<String> {
    page_with("ciacola", &overview_body(&state).await, true)
}

/// Just the body, for the client to swap in without a reload.
async fn overview_fragment(State(state): State<BoardState>) -> Html<String> {
    Html(overview_body(&state).await)
}

fn version_of(body: &str) -> u64 {
    let mut h = DefaultHasher::new();
    body.hash(&mut h);
    h.finish()
}

/// A version number whenever the rendered board actually changes.
///
/// Rendering server-side to decide is the honest cheap trick: it costs
/// a few queries a second at this scale, and it means an idle system
/// sends nothing, so an open board is not a busy one.
/// The stream half of `/board/events`, split out so a test can drive it
/// without a live HTTP connection.
///
/// The loop is otherwise infinite (an open board is a long-lived
/// response by design), so it has to race its own sleep against
/// `shutdown`: without that, a graceful shutdown that waits for
/// in-flight responses to finish would wait on this one forever.
fn board_event_stream(
    state: BoardState,
) -> impl futures_core::Stream<Item = Result<Event, std::convert::Infallible>> {
    async_stream::stream! {
        let mut last = 0u64;
        loop {
            let version = version_of(&overview_body(&state).await);
            if version != last {
                last = version;
                yield Ok(Event::default().event("board").data(version.to_string()));
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = state.shutdown.cancelled() => break,
            }
        }
    }
}

async fn events(
    State(state): State<BoardState>,
) -> Sse<impl futures_core::Stream<Item = Result<Event, std::convert::Infallible>>> {
    Sse::new(board_event_stream(state)).keep_alive(axum::response::sse::KeepAlive::default())
}

fn turn_cost(turn: &TurnRow) -> String {
    match turn.reported_cost_micro_usd() {
        Some(cost) => usd(cost),
        None => match turn.cost_state.as_str() {
            "not_priced" => "unpriced".into(),
            "unreported" => "cost unreported".into(),
            "legacy" => "cost unknown (legacy)".into(),
            state => format!("cost {state}"),
        },
    }
}

fn turn_usage(turn: &TurnRow) -> String {
    match turn.reported_tokens() {
        Some((input, output, _cached)) if turn.usage_complete => {
            format!("{input} in / {output} out")
        }
        Some((input, output, _cached)) => format!("{input} in / {output} out (partial)"),
        None => match turn.usage_state.as_str() {
            "not_tracked" => "tokens not tracked".into(),
            "unreported" => "tokens unreported".into(),
            "legacy" => "tokens unknown (legacy)".into(),
            state => format!("tokens {state}"),
        },
    }
}

fn turn_elapsed(turn: &TurnRow) -> String {
    let seconds = turn.elapsed_ms as f64 / 1000.0;
    match turn.elapsed_state.as_str() {
        "measured" => format!("{seconds:.1}s"),
        "upper_bound" => format!("≤{seconds:.1}s upper bound"),
        "not_attempted" => "not attempted".into(),
        "unknown" => "runtime unknown".into(),
        "legacy" => format!("{seconds:.1}s legacy"),
        state => format!("{seconds:.1}s {state}"),
    }
}

fn turn_html(turn: &TurnRow) -> String {
    let mut out = format!(
        "<h2>turn {} {} <span class=\"dim\">{} · {} · {}</span></h2>\
         <div class=\"msg them\">{}</div>",
        turn.seq,
        chip(&turn.state),
        turn_cost(turn),
        turn_elapsed(turn),
        turn_usage(turn),
        esc(&turn.prompt),
    );
    if let Some(reply) = &turn.reply {
        out.push_str(&format!("<div class=\"msg it\">{}</div>", esc(reply)));
    }
    if let Some(error) = &turn.error {
        out.push_str(&format!("<div class=\"msg err\">{}</div>", esc(error)));
    }
    if let Some(admission_override) = &turn.admission_override {
        let detail = serde_json::from_str::<serde_json::Value>(admission_override)
            .ok()
            .and_then(|value| {
                let kind = value.get("kind")?.as_str()?;
                let reason = value.get("reason")?.as_str()?;
                Some(format!("supervised {kind} override: {reason}"))
            })
            .unwrap_or_else(|| "supervised admission override (invalid audit record)".into());
        out.push_str(&format!("<p class=\"dim\">{}</p>", esc(&detail)));
    }
    out
}

async fn agent_page(State(state): State<BoardState>, Path(agent_id): Path<String>) -> Html<String> {
    let Ok(Some(agent)) = state.ledger.get_agent(&agent_id).await else {
        return page_with(
            "not found",
            "<p>no such agent. <a href=\"/board\">back</a></p>",
            false,
        );
    };
    let turns = state
        .ledger
        .conversation(&agent_id)
        .await
        .unwrap_or_default();

    let mut body = format!(
        "<p><a href=\"/board\">&larr; board</a></p>\
         <h1>{name} {chip} <span class=\"dim\">{turns} turns · {cost} reported</span></h1>\
         <p class=\"dim mono\">{id}<br>session {session}</p>",
        name = esc(&agent.name),
        chip = chip(&agent.state),
        turns = agent.turns,
        cost = usd(agent.cost_micro_usd),
        id = esc(&agent.agent_id),
        session = esc(agent.session.as_deref().unwrap_or("-")),
    );
    body.push_str(&format!(
        "<h2>provisioning</h2><table>\
         <tr><th>model</th><th>effort</th><th>max turns</th><th>rotates</th>\
         <th>working dir</th></tr>\
         <tr><td>{model}</td><td>{effort}</td><td>{max_turns}</td><td>{rotate}</td>\
         <td class=\"mono dim\">{dir}</td></tr></table>\
         <p class=\"dim\">tools: {tools}</p>",
        model = esc(agent.def.model.as_deref().unwrap_or("(default)")),
        effort = esc(agent.def.effort.as_deref().unwrap_or("(default)")),
        max_turns = agent
            .def
            .max_turns
            .map(|t| t.to_string())
            .unwrap_or_else(|| "(default)".into()),
        rotate = agent
            .def
            .rotate_after_turns
            .map(|t| format!("every {t} turns"))
            .unwrap_or_else(|| "never".into()),
        dir = esc(&agent
            .def
            .working_dir
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "-".into())),
        tools = if agent.def.allowed_tools.is_empty() {
            // The flat11 finding in one line: a toolless spoke does not
            // refuse, it fabricates. Worth seeing at a glance.
            "<span style=\"color:#d29922\">none</span>".to_string()
        } else {
            esc(&agent.def.allowed_tools.join(", "))
        },
    ));

    if let Some(parent) = &agent.spawned_by {
        body.push_str(&format!(
            "<p class=\"dim\">spawned by <a class=\"mono\" href=\"/board/agent/{p}\">{p}</a></p>",
            p = esc(parent)
        ));
    }
    for turn in &turns {
        body.push_str(&turn_html(turn));
    }
    page_with(&agent.name, &body, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt;

    fn admission_report(providers: Vec<ProviderAdmissionStatus>) -> AdmissionReport {
        AdmissionReport {
            window_seconds: 86_400,
            checked_unix: 100_000,
            since_unix: 13_600,
            global: ciacola_core::limits::GlobalAdmissionStatus {
                reported_spend_micro_usd: 0,
                daily_warn_micro_usd: None,
                daily_stop_micro_usd: None,
                cost_gaps: 0,
                state: AdmissionState::Ok,
            },
            providers,
        }
    }

    fn admission_status(
        provider: &str,
        state: AdmissionState,
        automatic_allowed: bool,
    ) -> ProviderAdmissionStatus {
        ProviderAdmissionStatus {
            accounting: ciacola_core::limits::ProviderAccounting {
                provider: provider.into(),
                active_agents: 1,
                reports_token_usage: true,
                tokens_in: 80,
                tokens_out: 20,
                tokens_cached: 50,
                usage_complete_turns: 1,
                ..Default::default()
            },
            daily_warn_tokens: Some(75),
            daily_stop_tokens: Some(100),
            state,
            automatic_allowed,
            detail: None,
        }
    }

    async fn state() -> BoardState {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool.clone()).await.expect("ledger");
        let (tx, _rx) = tower_mcp::context::notification_channel(8);
        let notify = ciacola_core::Notifier(tx);
        let exec = ciacola_core::HandExecutor::start(ledger.clone(), notify.clone(), 1);
        let ctx = ciacola_core::PluginContext {
            pool,
            ledger: ledger.clone(),
            exec,
            notify,
            db_path: String::new(),
            loopback_mcp_config: String::new(),
            operator_mcp_config: String::new(),
            plugin_config: toml::Value::Table(toml::map::Map::new()),
            limits: Default::default(),
            runtime: Default::default(),
            roles: ciacola_core::roles::Roles::new(Vec::new(), String::new()),
        };
        let host = Arc::new(PluginHost::setup(vec![], &ctx).await.expect("host"));
        BoardState {
            ledger,
            host,
            limits: Default::default(),
            shutdown: CancellationToken::new(),
        }
    }

    fn turn(cost_state: &str, usage_state: &str) -> TurnRow {
        TurnRow {
            agent_id: "agent".into(),
            seq: 1,
            prompt: "work".into(),
            state: "killed".into(),
            reply: None,
            error: Some("stopped".into()),
            cost_micro_usd: 0,
            cost_state: cost_state.into(),
            elapsed_ms: 1_000,
            elapsed_state: "measured".into(),
            claimed_unix_ms: Some(1),
            tokens_in: 0,
            tokens_out: 0,
            tokens_cached: 0,
            usage_state: usage_state.into(),
            usage_complete: usage_state == "reported",
            provider_turns: None,
            provider: "claude".into(),
            settled_unix: Some(1),
            admission_override: None,
        }
    }

    #[test]
    fn turn_rendering_distinguishes_unknown_from_reported_zero() {
        let unknown = turn_html(&turn("unreported", "unreported"));
        assert!(unknown.contains("cost unreported"), "{unknown}");
        assert!(unknown.contains("tokens unreported"), "{unknown}");

        let measured = turn_html(&turn("reported", "reported"));
        assert!(measured.contains("$0.0000"), "{measured}");
        assert!(measured.contains("0 in / 0 out"), "{measured}");
    }

    #[test]
    fn turn_rendering_names_elapsed_provenance() {
        let mut row = turn("reported", "reported");
        row.elapsed_state = "upper_bound".into();
        assert!(turn_html(&row).contains("≤1.0s upper bound"));

        row.elapsed_state = "not_attempted".into();
        assert!(turn_html(&row).contains("not attempted"));

        row.elapsed_state = "unknown".into();
        assert!(turn_html(&row).contains("runtime unknown"));

        row.elapsed_state = "legacy".into();
        assert!(turn_html(&row).contains("1.0s legacy"));
    }

    #[test]
    fn admission_table_keeps_cached_tokens_inside_reported_total() {
        let html = admission_section(Ok(admission_report(vec![admission_status(
            "codex",
            AdmissionState::Stopped,
            false,
        )])));

        assert!(html.contains("cached <span class=\"dim\">(included in input)</span>"));
        assert!(html.contains(
            "<td>codex</td><td class=\"num\">1</td><td class=\"num\">100</td><td class=\"num\">80</td><td class=\"num\">20</td><td class=\"num\">50</td>"
        ));
        assert!(html.contains("STOPPED"), "{html}");
        assert!(html.contains(">no</b>"), "{html}");
    }

    #[test]
    fn admission_table_summarizes_global_usd_policy_from_the_same_report() {
        let mut report = admission_report(vec![admission_status(
            "claude",
            AdmissionState::Warning,
            true,
        )]);
        report.global.reported_spend_micro_usd = 12_500_000;
        report.global.daily_warn_micro_usd = Some(10_000_000);
        report.global.daily_stop_micro_usd = Some(20_000_000);
        report.global.cost_gaps = 2;
        report.global.state = AdmissionState::Warning;

        let html = admission_section(Ok(report));
        assert!(html.contains("USD admission: <b>$12.5000</b> reported"));
        assert!(html.contains("warn $10.0000"));
        assert!(html.contains("stop $20.0000"));
        assert!(html.contains("cost telemetry gaps 2"));
        assert!(html.contains("state <b style=\"color:#d29922\">WARNING</b>"));
    }

    #[test]
    fn admission_table_distinguishes_warning_blocked_and_telemetry_gaps() {
        let warning = admission_status("claude", AdmissionState::Warning, true);
        let mut blocked = admission_status("codex", AdmissionState::Unobservable, false);
        blocked.accounting.tokens_in = 0;
        blocked.accounting.tokens_out = 0;
        blocked.accounting.tokens_cached = 0;
        blocked.accounting.usage_unreported_turns = 1;
        blocked.accounting.usage_not_tracked_turns = 2;
        blocked.detail = Some("token accounting is incomplete".into());

        let html = admission_section(Ok(admission_report(vec![warning, blocked])));
        assert!(html.contains("WARNING"), "{html}");
        assert!(html.contains("AUTO BLOCKED"), "{html}");
        assert!(html.contains("tokens unreported 1"), "{html}");
        assert!(html.contains("tokens not tracked 2"), "{html}");
        assert!(
            html.contains(
                "<td>codex</td><td class=\"num\">1</td><td class=\"num\">0</td><td class=\"num\">0</td>"
            ),
            "{html}"
        );
    }

    #[test]
    fn unused_registered_provider_is_neutral_until_an_agent_selects_it() {
        let mut unused = admission_status("codex", AdmissionState::Unguarded, false);
        unused.accounting.active_agents = 0;
        let html = admission_section(Ok(admission_report(vec![unused])));
        assert!(html.contains("INACTIVE"), "{html}");
        assert!(html.contains("unguarded if selected"), "{html}");
        assert!(html.contains("<span class=\"dim\">n/a</span>"), "{html}");
        assert!(!html.contains("AUTO BLOCKED"), "{html}");
    }

    #[test]
    fn admission_report_errors_are_visible_instead_of_rendered_as_zero() {
        let html = admission_section(Err("broken admission query".into()));
        assert!(html.contains("admission report error: broken admission query"));
        assert!(!html.contains("<table>"));
    }

    /// `/board/events` is a long-lived SSE response by design: without
    /// this, `axum::serve(...).with_graceful_shutdown(...)` would wait
    /// on it forever, because a graceful shutdown waits for in-flight
    /// responses to finish rather than cutting them off.
    #[tokio::test]
    async fn event_stream_ends_when_shutdown_is_cancelled() {
        let state = state().await;
        let shutdown = state.shutdown.clone();
        let mut stream = Box::pin(board_event_stream(state));

        // The board always renders as different from the sentinel
        // `last = 0`, so the first tick yields immediately; drain it
        // before cancelling so the test exercises the loop, not just
        // its first iteration.
        let _ = stream.next().await.expect("first tick");

        shutdown.cancel();

        tokio::time::timeout(Duration::from_secs(5), async {
            while stream.next().await.is_some() {}
        })
        .await
        .expect("stream did not end after shutdown was cancelled");
    }
}
