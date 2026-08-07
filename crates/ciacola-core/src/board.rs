//! The board: the ledger, rendered.
//!
//! Two pages of server-rendered HTML over the same tables every MCP
//! tool reads, mounted on the HTTP port flat6 already serves. No
//! framework, no build step, no new dependency beyond axum, which
//! tower-mcp's http transport already pulls in.
//!
//! The point being tested: the board a queue framework sells shows the
//! wrong nouns for this system (task statuses a crash makes lie), and
//! the board over the ledger, agents, conversations, spend, schedules,
//! is a page per SELECT.

use axum::Router;
use axum::extract::{Path, State};
use axum::response::{Html, Redirect};
use axum::routing::get;

use std::sync::Arc;

use crate::ledger::{Ledger, TurnRow};
use crate::plugin::PluginHost;

#[derive(Clone)]
struct BoardState {
    ledger: Ledger,
    host: Arc<PluginHost>,
    limits: crate::limits::Limits,
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
    limits: crate::limits::Limits,
) -> Router {
    let plugin_routes = host.routes();
    Router::new()
        .route("/", get(|| async { Redirect::to("/board") }))
        .route("/board", get(overview))
        .route("/board/agent/{agent_id}", get(agent_page))
        .with_state(BoardState {
            ledger,
            host,
            limits,
        })
        .merge(plugin_routes)
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn usd(micro: i64) -> String {
    format!("${:.4}", micro as f64 / 1e6)
}

/// "4m ago", not a timestamp. An unattended system is read to answer
/// "is this stuck", and a wall-clock time makes the reader do the
/// arithmetic. Zero means never.
pub fn ago(unix: i64) -> String {
    if unix == 0 {
        return "never".into();
    }
    let secs = (crate::time::now_unix() - unix).max(0);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// 595.7k rather than 595713: the header is read at a glance.
fn human_count(n: i64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

pub fn chip(state: &str) -> String {
    let color = match state {
        "ok" | "idle" => "#3fb950",
        "running" => "#58a6ff",
        "queued" | "skipped" => "#d29922",
        "failed" => "#f85149",
        "killed" => "#8b949e",
        _ => "#8b949e",
    };
    format!(
        "<span class=\"chip\" style=\"border-color:{color};color:{color}\">{}</span>",
        esc(state)
    )
}

pub fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"refresh\" content=\"5\">\
         <title>{}</title><style>\
         body{{background:#0d1117;color:#e6edf3;font:14px/1.5 -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;margin:0;padding:2rem;max-width:70rem}}\
         a{{color:#58a6ff;text-decoration:none}} a:hover{{text-decoration:underline}}\
         h1{{font-size:1.2rem;margin:0 0 1.5rem}} h2{{font-size:0.85rem;text-transform:uppercase;letter-spacing:0.08em;color:#8b949e;margin:2rem 0 0.5rem}}\
         table{{border-collapse:collapse;width:100%}}\
         th{{text-align:left;font-size:0.75rem;text-transform:uppercase;letter-spacing:0.05em;color:#8b949e;font-weight:500;padding:0.4rem 0.75rem;border-bottom:1px solid #21262d}}\
         td{{padding:0.5rem 0.75rem;border-bottom:1px solid #21262d;vertical-align:top}}\
         .chip{{border:1px solid;border-radius:1rem;padding:0.05rem 0.6rem;font-size:0.75rem;white-space:nowrap}}\
         .num{{text-align:right;font-variant-numeric:tabular-nums}}\
         .dim{{color:#8b949e}} .mono{{font-family:ui-monospace,monospace;font-size:0.85em}}\
         .stat{{display:inline-block;margin-right:2.5rem}} .stat b{{display:block;font-size:1.4rem;font-weight:600}} .stat span{{color:#8b949e;font-size:0.8rem}}\
         .msg{{margin:0.25rem 0 0.75rem;padding:0.6rem 0.9rem;border-left:3px solid #21262d;white-space:pre-wrap}}\
         .msg.them{{border-color:#58a6ff}} .msg.it{{border-color:#3fb950}} .msg.err{{border-color:#f85149}}\
         .kanban{{display:flex;gap:1rem;align-items:flex-start}}\
         .lane{{flex:1;min-width:0;background:#161b22;border:1px solid #21262d;border-radius:6px;padding:0.6rem}}\
         .lane h3{{font-size:0.72rem;text-transform:uppercase;letter-spacing:0.08em;color:#8b949e;margin:0 0 0.5rem;font-weight:600}}\
         .card{{background:#0d1117;border:1px solid #21262d;border-radius:6px;padding:0.5rem 0.6rem;margin-bottom:0.5rem}}\
         .card b{{font-weight:600;font-size:0.85rem}}\
         .card .dim{{font-size:0.78rem;display:block;margin-top:0.15rem}}\
         </style></head><body>{}</body></html>",
        esc(title),
        body
    ))
}

async fn overview(State(state): State<BoardState>) -> Html<String> {
    let agents = match state.ledger.list_agents().await {
        Ok(agents) => agents,
        Err(e) => {
            return page(
                "board",
                &format!("<p>ledger error: {}</p>", esc(&e.to_string())),
            );
        }
    };
    let retired = state.ledger.retired_count().await.unwrap_or_default();

    // Totals include the retired: retirement hides agents, never money.
    let (total_cost, total_turns) = state.ledger.totals().await.unwrap_or_default();
    let running = agents.iter().filter(|a| a.state == "running").count();
    let day_cost = state
        .ledger
        .spend_since(crate::time::now_unix() - 86_400)
        .await
        .unwrap_or_default();
    let tokens = state.ledger.token_totals().await.unwrap_or_default();

    let mut body = format!(
        "<h1>flat board</h1>\
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
    let row_html = |agent: &crate::ledger::AgentRow, child: bool| {
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
    let is_root = |a: &&crate::ledger::AgentRow| {
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

    page("flat board", &body)
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
        return page(
            "not found",
            "<p>no such agent. <a href=\"/board\">back</a></p>",
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
    page(&agent.name, &body)
}
