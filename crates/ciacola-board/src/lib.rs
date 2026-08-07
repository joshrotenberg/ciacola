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

use ciacola_core::ledger::{Ledger, TurnRow};
use ciacola_core::plugin::PluginHost;
use ciacola_core::render::{ago, chip, esc, human_count, page_with, usd};

#[derive(Clone)]
struct BoardState {
    ledger: Ledger,
    host: Arc<PluginHost>,
    limits: ciacola_core::limits::Limits,
}

/// The board knows core (agents) and asks the host for everything
/// else. Adding a plugin adds a section with no change here, which is
/// what `Option<Items>`, `Option<Findings>`, and three constructors
/// were failing to achieve.
pub fn router(ledger: Ledger, host: Arc<PluginHost>) -> Router {
    router_with_limits(ledger, host, Default::default())
}

pub fn router_with_limits(
    ledger: Ledger,
    host: Arc<PluginHost>,
    limits: ciacola_core::limits::Limits,
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
        })
        .merge(plugin_routes)
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
         <span class=\"stat\"><b>{}</b><span>total spend</span></span>\
         <span class=\"stat\"><b>{}</b><span>last 24h{}</span></span>\
         <span class=\"stat\"><b>{}</b><span>tokens</span></span>\
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
        human_count(tokens.0 + tokens.1),
        retired,
    );

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
        "<h2>agents</h2><table><tr><th>name</th><th>state</th>\
        <th class=\"num\">turns</th><th class=\"num\">cost</th><th>last active</th>\
        <th>session</th></tr>",
    );
    // Families together: roots first, each followed by its children.
    let row_html = |agent: &ciacola_core::ledger::AgentRow, child: bool| {
        format!(
            "<tr><td>{indent}<a href=\"/board/agent/{id}\">{name}</a> <span class=\"dim mono\">{short}</span></td>\
             <td>{chip}</td><td class=\"num\">{turns}</td><td class=\"num\">{cost}</td>\
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
async fn events(
    State(state): State<BoardState>,
) -> Sse<impl futures_core::Stream<Item = Result<Event, std::convert::Infallible>>> {
    let stream = async_stream::stream! {
        let mut last = 0u64;
        loop {
            let version = version_of(&overview_body(&state).await);
            if version != last {
                last = version;
                yield Ok(Event::default().event("board").data(version.to_string()));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    };
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

fn turn_html(turn: &TurnRow) -> String {
    let mut out = format!(
        "<h2>turn {} {} <span class=\"dim\">{} · {:.1}s · {} in / {} out</span></h2>\
         <div class=\"msg them\">{}</div>",
        turn.seq,
        chip(&turn.state),
        usd(turn.cost_micro_usd),
        turn.elapsed_ms as f64 / 1000.0,
        turn.tokens_in,
        turn.tokens_out,
        esc(&turn.prompt),
    );
    if let Some(reply) = &turn.reply {
        out.push_str(&format!("<div class=\"msg it\">{}</div>", esc(reply)));
    }
    if let Some(error) = &turn.error {
        out.push_str(&format!("<div class=\"msg err\">{}</div>", esc(error)));
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
         <h1>{name} {chip} <span class=\"dim\">{turns} turns · {cost}</span></h1>\
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
