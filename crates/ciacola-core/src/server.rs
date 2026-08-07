//! The six verbs, as MCP tools: spawn, send, get, list, wait, kill.
//!
//! Built once and parameterised only by who executes turns, so the
//! executor is swappable without the client noticing. Every tool is a
//! shell over the ledger, and the executor appears in exactly two of
//! them (`send` submits, `kill` signals), which is the measure of how
//! thin that seam is.

use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tower_mcp::{CallToolResult, LogLevel, McpRouter, ToolBuilder};

use crate::agent::AgentDef;
use crate::exec::TurnExecutor;
use crate::ledger::{AgentRow, Ledger, TurnRow};
use crate::notify::Notifier;

const MAX_WAIT_SECS: u64 = 600;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SpawnArgs {
    /// Short human label, not an id.
    name: String,
    /// The agent's standing knowledge: who it is, what good looks like.
    system_prompt: String,
    /// Provider model, e.g. "haiku". Omit for the provider default.
    model: Option<String>,
    /// Tool names the agent may use. Omit for none.
    allowed_tools: Option<Vec<String>>,
    /// Cap on provider-internal turns per prompt.
    max_turns: Option<u32>,
    /// Path to an MCP config file for the agent's own tools. Point it
    /// at this server and the agent can spawn and drive agents itself.
    mcp_config: Option<String>,
    /// Your own agent_id, if you are an agent spawning a helper. Makes
    /// the family visible on the board and lets the helper be traced
    /// back to you.
    spawned_by: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    /// The agent to speak to.
    agent_id: String,
    /// What to say.
    text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AgentArgs {
    /// The agent to look at.
    agent_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    /// The agent whose turn to wait for.
    agent_id: String,
    /// The turn to wait for; from `send`. Omit for the latest.
    seq: Option<i64>,
    /// Give up after this long. Default 120, max 600.
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ResendArgs {
    /// The agent whose turn to send again.
    agent_id: String,
    /// The turn to repeat. Its prompt is reused verbatim.
    seq: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RetireArgs {
    /// The agent to retire. Must be idle; its conversation stays in
    /// the ledger.
    agent_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct KillArgs {
    /// The agent whose running turn to kill.
    agent_id: String,
    /// The turn to kill; from `send`.
    seq: i64,
}

/// One rendering of a submission for every tool that makes one.
fn submission_result(agent_id: &str, outcome: crate::plugin::Submission) -> CallToolResult {
    use crate::plugin::Submission;
    match outcome {
        Submission::Submitted { seq } => CallToolResult::json(json!({
            "agent_id": agent_id,
            "seq": seq,
        })),
        Submission::Busy { reason } => CallToolResult::error(reason),
        Submission::OverBudget {
            spent_usd,
            limit_usd,
        } => CallToolResult::error(format!(
            "over daily budget: ${spent_usd:.2} spent, limit ${limit_usd:.2}. \
             Running turns finish; new ones resume when the rolling day falls below."
        )),
        Submission::Failed { reason } => CallToolResult::error(reason),
    }
}

/// `Some(reason)` when a spawn by this parent would go too deep. A
/// runaway spawns faster than any spend check can notice, so this is
/// the control that actually stops one.
pub async fn depth_refusal(
    ledger: &Ledger,
    spawned_by: Option<&str>,
    max_depth: i64,
) -> Option<String> {
    if max_depth <= 0 {
        return None;
    }
    let parent = spawned_by?;
    let depth = ledger.spawn_depth(parent).await.ok()? + 1;
    (depth > max_depth).then(|| {
        format!(
            "refused: spawning here would be depth {depth}, past the limit of {max_depth}. \
             Do the work yourself or ask the operator to raise max_spawn_depth."
        )
    })
}

fn agent_json(agent: &AgentRow) -> serde_json::Value {
    json!({
        "agent_id": agent.agent_id,
        "name": agent.name,
        "model": agent.def.model,
        "effort": agent.def.effort,
        "state": agent.state,
        "session": agent.session,
        "turns": agent.turns,
        "cost_usd": agent.cost_micro_usd as f64 / 1e6,
        "spawned_by": agent.spawned_by,
    })
}

fn turn_json(turn: &TurnRow) -> serde_json::Value {
    json!({
        "agent_id": turn.agent_id,
        "seq": turn.seq,
        "state": turn.state,
        "prompt": turn.prompt,
        "reply": turn.reply,
        "error": turn.error,
        "cost_usd": turn.cost_micro_usd as f64 / 1e6,
        "tokens_in": turn.tokens_in,
        "tokens_out": turn.tokens_out,
        "elapsed_ms": turn.elapsed_ms,
    })
}

/// The whole server, parameterised only by who executes turns.
pub fn router(ledger: Ledger, exec: Arc<dyn TurnExecutor>, notify: Notifier) -> McpRouter {
    router_with(ledger, exec, notify, true)
}

/// The same server with the tool set chosen by the caller. `include_kill:
/// false` is for surfaces handed to agents (flat6's loopback): everything
/// else is theirs, but stopping paid work stays a person's call.
pub fn router_with(
    ledger: Ledger,
    exec: Arc<dyn TurnExecutor>,
    notify: Notifier,
    include_kill: bool,
) -> McpRouter {
    router_with_limits(ledger, exec, notify, include_kill, Default::default())
}

/// Depth is checked here rather than in the ledger because it is a
/// policy, not an invariant: a spawn that would exceed it is refused
/// with an explanation the calling agent can act on.
pub fn router_with_limits(
    ledger: Ledger,
    exec: Arc<dyn TurnExecutor>,
    notify: Notifier,
    include_kill: bool,
    limits: crate::limits::Limits,
) -> McpRouter {
    let max_depth = limits.max_spawn_depth;
    let spawn = {
        let ledger = ledger.clone();
        ToolBuilder::new("spawn")
            .description(
                "Define an agent: a durable conversation with a system \
                 prompt. Spawning runs nothing and costs nothing.",
            )
            .non_destructive()
            .handler(move |args: SpawnArgs| {
                let ledger = ledger.clone();
                async move {
                    let mut def = AgentDef::new(args.name, args.system_prompt);
                    if let Some(model) = args.model {
                        def = def.model(model);
                    }
                    if let Some(tools) = args.allowed_tools {
                        def = def.allowed_tools(tools);
                    }
                    if let Some(max_turns) = args.max_turns {
                        def = def.max_turns(max_turns);
                    }
                    if let Some(mcp_config) = args.mcp_config {
                        def = def.mcp_config(mcp_config);
                    }
                    if let Some(reason) =
                        depth_refusal(&ledger, args.spawned_by.as_deref(), max_depth).await
                    {
                        return Ok(CallToolResult::error(reason));
                    }
                    match ledger.create_agent(&def, args.spawned_by.as_deref()).await {
                        Ok(agent_id) => Ok(CallToolResult::json(json!({ "agent_id": agent_id }))),
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                }
            })
            .build()
    };

    let send = {
        let ledger = ledger.clone();
        let exec = exec.clone();
        let notify = notify.clone();
        let limits = limits.clone();
        ToolBuilder::new("send")
            .description(
                "Say something to an agent and return immediately with a \
                 turn number. The agent works without you; a notification \
                 arrives when the turn ends, or use wait.",
            )
            .non_destructive()
            .handler(move |args: SendArgs| {
                let ledger = ledger.clone();
                let exec = exec.clone();
                let notify = notify.clone();
                let limits = limits.clone();
                async move {
                    let outcome = crate::plugin::submit(
                        &ledger,
                        exec.as_ref(),
                        &notify,
                        &limits,
                        &args.agent_id,
                        &args.text,
                        "send",
                    )
                    .await;
                    Ok(submission_result(&args.agent_id, outcome))
                }
            })
            .build()
    };

    let get = {
        let ledger = ledger.clone();
        ToolBuilder::new("get")
            .description("One agent, with its full conversation so far.")
            .read_only()
            .handler(move |args: AgentArgs| {
                let ledger = ledger.clone();
                async move {
                    match ledger.get_agent(&args.agent_id).await {
                        Ok(Some(agent)) => {
                            let mut view = agent_json(&agent);
                            match ledger.conversation(&args.agent_id).await {
                                Ok(turns) => {
                                    view["conversation"] =
                                        json!(turns.iter().map(turn_json).collect::<Vec<_>>());
                                    Ok(CallToolResult::json(view))
                                }
                                Err(e) => Ok(CallToolResult::error(e.to_string())),
                            }
                        }
                        Ok(None) => Ok(CallToolResult::error(format!(
                            "no agent '{}'",
                            args.agent_id
                        ))),
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                }
            })
            .build()
    };

    let list = {
        let ledger = ledger.clone();
        ToolBuilder::new("list")
            .description("Every agent, with derived state: idle, queued, or running.")
            .read_only()
            .no_params_handler(move || {
                let ledger = ledger.clone();
                async move {
                    match ledger.list_agents().await {
                        // structuredContent must be an object, not an
                        // array; both flat10 managers found and filed
                        // the bare-array version of this as a bug.
                        Ok(agents) => Ok(CallToolResult::json(json!({
                            "agents": agents.iter().map(agent_json).collect::<Vec<_>>()
                        }))),
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                }
            })
            .build()
    };

    let wait = {
        let ledger = ledger.clone();
        ToolBuilder::new("wait")
            .description("Block until a turn reaches a terminal state, then return it.")
            .read_only()
            .handler(move |args: WaitArgs| {
                let ledger = ledger.clone();
                async move {
                    let seq = match args.seq {
                        Some(seq) => seq,
                        None => match ledger.get_agent(&args.agent_id).await {
                            Ok(Some(agent)) => agent.turns,
                            Ok(None) => {
                                return Ok(CallToolResult::error(format!(
                                    "no agent '{}'",
                                    args.agent_id
                                )));
                            }
                            Err(e) => return Ok(CallToolResult::error(e.to_string())),
                        },
                    };
                    let timeout =
                        Duration::from_secs(args.timeout_secs.unwrap_or(120).min(MAX_WAIT_SECS));
                    let deadline = tokio::time::Instant::now() + timeout;
                    loop {
                        match ledger.get_turn(&args.agent_id, seq).await {
                            Ok(Some(turn)) if turn.state != "queued" && turn.state != "running" => {
                                return Ok(CallToolResult::json(turn_json(&turn)));
                            }
                            Ok(Some(_)) => {}
                            Ok(None) => {
                                return Ok(CallToolResult::error(format!(
                                    "no turn {}/{}",
                                    args.agent_id, seq
                                )));
                            }
                            Err(e) => return Ok(CallToolResult::error(e.to_string())),
                        }
                        if tokio::time::Instant::now() >= deadline {
                            return Ok(CallToolResult::error(format!(
                                "turn {}/{} still not finished after {timeout:?}",
                                args.agent_id, seq
                            )));
                        }
                        tokio::time::sleep(Duration::from_millis(300)).await;
                    }
                }
            })
            .build()
    };

    let resend = {
        let ledger = ledger.clone();
        let exec = exec.clone();
        let notify = notify.clone();
        let limits = limits.clone();
        ToolBuilder::new("resend")
            .description(
                "Send an earlier turn's prompt again, as a new turn.                  For a turn that failed or was killed: the wording is                  already right, and retyping it invites a typo.",
            )
            .non_destructive()
            .handler(move |args: ResendArgs| {
                let ledger = ledger.clone();
                let exec = exec.clone();
                let notify = notify.clone();
                let limits = limits.clone();
                async move {
                    let Ok(Some(turn)) = ledger.get_turn(&args.agent_id, args.seq).await else {
                        return Ok(CallToolResult::error(format!(
                            "no turn {}/{}",
                            args.agent_id, args.seq
                        )));
                    };
                    let outcome = crate::plugin::submit(
                        &ledger,
                        exec.as_ref(),
                        &notify,
                        &limits,
                        &args.agent_id,
                        &turn.prompt,
                        "resend",
                    )
                    .await;
                    Ok(submission_result(&args.agent_id, outcome))
                }
            })
            .build()
    };

    let retire = {
        let ledger = ledger.clone();
        ToolBuilder::new("retire")
            .description(
                "Retire an idle agent: it stops being sendable and drops                  out of list, but its conversation stays in the ledger.                  Spawn helpers, use them, retire them.",
            )
            .non_destructive()
            .handler(move |args: RetireArgs| {
                let ledger = ledger.clone();
                async move {
                    match ledger.retire_agent(&args.agent_id).await {
                        Ok(true) => Ok(CallToolResult::json(json!({ "retired": true }))),
                        Ok(false) => Ok(CallToolResult::error(format!(
                            "agent '{}' not retired: unknown, already retired, or mid-turn",
                            args.agent_id
                        ))),
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                }
            })
            .build()
    };

    let kill = {
        let ledger = ledger.clone();
        let exec = exec.clone();
        let notify = notify.clone();
        ToolBuilder::new("kill")
            .description(
                "Stop a running turn. The provider process dies with it; \
                 the agent survives and can be sent to again.",
            )
            .destructive()
            .handler(move |args: KillArgs| {
                let ledger = ledger.clone();
                let exec = exec.clone();
                let notify = notify.clone();
                async move {
                    // Record BEFORE signalling: a queued turn is settled
                    // in the ledger first, so a delivery racing this kill
                    // finds the claim already refused. Then signal, so a
                    // turn that had already claimed still gets stopped.
                    match ledger
                        .fail_turn(
                            &args.agent_id,
                            args.seq,
                            "killed",
                            "killed by request",
                            0,
                            None,
                        )
                        .await
                    {
                        Ok(recorded) => {
                            let signalled = exec.kill(&args.agent_id, args.seq);
                            if recorded {
                                notify.turn(
                                    LogLevel::Warning,
                                    &args.agent_id,
                                    args.seq,
                                    "killed",
                                    "killed by request",
                                );
                            }
                            Ok(CallToolResult::json(json!({
                                "agent_id": args.agent_id,
                                "seq": args.seq,
                                "signalled": signalled,
                                "recorded": recorded,
                            })))
                        }
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                }
            })
            .build()
    };

    let router = McpRouter::new()
        .server_info(format!("ciacola-{}", exec.name()), "0.0.0")
        .tool(spawn)
        .tool(send)
        .tool(get)
        .tool(list)
        .tool(wait)
        .tool(resend)
        .tool(retire);
    if include_kill {
        router.tool(kill)
    } else {
        router
    }
}
