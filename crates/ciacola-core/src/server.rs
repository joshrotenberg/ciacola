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
use tower_mcp::extract::{Context, Json, State};
use tower_mcp::{CallToolResult, LogLevel, McpRouter, ToolBuilder};

use crate::agent::AgentDef;
use crate::exec::TurnExecutor;
use crate::identity::{AgentIdentity, grant_child_tools};
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
        "provider": agent.def.provider,
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

/// What a person sees on connecting. A client that renders server
/// instructions (mcp-repl markdown-renders them into its banner) gets a
/// front door without either side knowing about the other, which is the
/// same bet [`crate::complete`] makes: enrich the server, and generic
/// clients become specific ones for free.
///
/// Kept to what is not derivable from the schema. The argument names are
/// in `tools/list`; the fact that `send` does not block is not.
const OPERATOR: &str = "\
ciacola runs coding agents as durable conversations. An agent exists \
while nothing is running; a *turn* is one execution against it.

- `spawn` defines an agent. It runs nothing and costs nothing.
- `send` returns a turn number immediately. It does not block.
- `wait` blocks for one. `list` shows what is in flight.
- `kill` stops a turn. The agent survives it, and can be sent to again.

`agent_id` completes on the agent's *name*, so type `alpha` rather than \
a ULID. The board is at `/board`.";

/// The same, for the loopback surface agents are handed. It answers the
/// question an agent cannot answer from a schema: that these tools point
/// back at the system running it, and what that costs.
const AGENT: &str = "\
You are connected to the server running you. These tools spawn and \
drive other agents, which is all that delegation requires here: a \
conductor spawning debaters is a prompt, not a framework.

- Work a child does is charged to the conversation that spawned it.
- Spawn depth is capped, and a spawn past the cap is refused with the \
reason.
- `kill` is deliberately absent. Stopping paid work stays a person's \
call.";

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

/// The spawn tool, on its own so the identity policy is testable
/// without a transport: tests hand it a RequestContext directly.
fn spawn_tool(ledger: Ledger, max_depth: i64, operator_surface: bool) -> tower_mcp::Tool {
    ToolBuilder::new("spawn")
            .description(
                "Define an agent: a durable conversation with a system \
                 prompt. Spawning runs nothing and costs nothing.",
            )
            .non_destructive()
            .extractor_handler(
                ledger,
                move |State(ledger): State<Ledger>,
                      ctx: Context,
                      Json(args): Json<SpawnArgs>| async move {
                    // Derived beats claimed. An authenticated caller IS
                    // the parent, whatever it says; an anonymous caller
                    // on the agent surface can claim nothing; only the
                    // operator's own terminal, where there is no HTTP
                    // request to authenticate, is taken at its word.
                    let caller = ctx.extension::<AgentIdentity>().map(|i| i.0.clone());
                    let spawned_by = match (&caller, operator_surface) {
                        (Some(id), _) => Some(id.clone()),
                        (None, true) => args.spawned_by.clone(),
                        (None, false) => None,
                    };

                    let mut def = AgentDef::new(args.name, args.system_prompt);
                    if let Some(model) = args.model {
                        def = def.model(model);
                    }
                    if let Some(max_turns) = args.max_turns {
                        def = def.max_turns(max_turns);
                    }

                    // The capability ceiling. A child's tools are at
                    // most its parent's, so breadth is bounded the way
                    // depth already was; anything requested past the
                    // ceiling is reported, not silently dropped. And an
                    // authenticated caller does not get to pick the
                    // child's MCP config: the loopback the child
                    // inherits is decided by this server, or handing an
                    // agent the operator surface would be one guessable
                    // path away.
                    let mut denied = Vec::new();
                    if let Some(requested) = args.allowed_tools {
                        match grant_child_tools(&ledger, caller.as_deref(), requested).await {
                            Ok(grant) => {
                                denied = grant.denied;
                                def = def.allowed_tools(grant.granted);
                            }
                            Err(e) => return Ok(CallToolResult::error(e.to_string())),
                        }
                    }
                    match (&caller, args.mcp_config) {
                        (Some(_), Some(_)) => {
                            return Ok(CallToolResult::error(
                                "an agent does not choose its child's mcp_config; \
                                 use a server-configured role when the child needs loopback tools"
                                    .to_string(),
                            ));
                        }
                        (None, Some(mcp_config)) => {
                            def = def.mcp_config(mcp_config);
                        }
                        (_, None) => {}
                    }

                    if let Some(reason) =
                        depth_refusal(&ledger, spawned_by.as_deref(), max_depth).await
                    {
                        return Ok(CallToolResult::error(reason));
                    }
                    match ledger.create_agent(&def, spawned_by.as_deref()).await {
                        Ok(agent_id) => {
                            let mut out = json!({ "agent_id": agent_id });
                            if !denied.is_empty() {
                                out["tools_denied"] = json!(denied);
                                out["note"] = json!(
                                    "denied tools are ones you do not hold yourself; \
                                     a child's reach is at most its parent's"
                                );
                            }
                            Ok(CallToolResult::json(out))
                        }
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                },
            )
            .build()
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
    // The operator surface is the one carrying kill; the same flag says
    // whose word to take about parentage when a call arrives without an
    // identity, so it is threaded into spawn under its real meaning.
    let spawn = spawn_tool(ledger.clone(), max_depth, include_kill);

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
                            // A kill settles the row from outside the
                            // run, so the elapsed the executor was
                            // measuring is not reachable here. Would
                            // need a claimed-at column to do better.
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
        .instructions(if include_kill { OPERATOR } else { AGENT })
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

#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::identity::AgentIdentity;
    use std::sync::Arc;
    use tower_mcp::context::{Extensions, RequestContext};
    use tower_mcp::protocol::RequestId;

    async fn ledger() -> Ledger {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        Ledger::setup(pool).await.expect("ledger")
    }

    fn ctx_as(agent_id: Option<&str>) -> RequestContext {
        let mut ext = Extensions::new();
        if let Some(id) = agent_id {
            ext.insert(AgentIdentity(id.to_string()));
        }
        RequestContext::new(RequestId::Number(1)).with_extensions(Arc::new(ext))
    }

    fn structured(result: &CallToolResult) -> serde_json::Value {
        serde_json::to_value(result)
            .ok()
            .and_then(|v| v.get("structuredContent").cloned())
            .unwrap_or(serde_json::Value::Null)
    }

    fn rendered(result: &CallToolResult) -> String {
        serde_json::to_string(result).unwrap_or_default()
    }

    /// The hole this closes: an authenticated caller claimed to be
    /// someone else, and the claim used to be believed. Lineage drives
    /// the cost rollup and the depth cap, so the claim was load-bearing.
    #[tokio::test]
    async fn an_authenticated_caller_cannot_claim_another_parent() {
        let l = ledger().await;
        let real = l
            .create_agent(&AgentDef::new("real-parent", "s"), None)
            .await
            .expect("parent");
        let spawn = spawn_tool(l.clone(), 3, false);
        let out = spawn
            .call_with_context(
                ctx_as(Some(&real)),
                serde_json::json!({
                    "name": "child",
                    "system_prompt": "s",
                    "spawned_by": "someone-else-entirely"
                }),
            )
            .await;
        let child_id = structured(&out)["agent_id"]
            .as_str()
            .expect("agent_id")
            .to_string();
        let child = l.get_agent(&child_id).await.expect("get").expect("row");
        assert_eq!(
            child.spawned_by.as_deref(),
            Some(real.as_str()),
            "identity must beat the claimed parent"
        );
    }

    /// Anonymous on the agent surface: the claim is discarded, and the
    /// child is a visible orphan rather than a forged descendant.
    #[tokio::test]
    async fn an_anonymous_agent_surface_caller_gets_no_parentage() {
        let l = ledger().await;
        let spawn = spawn_tool(l.clone(), 3, false);
        let out = spawn
            .call_with_context(
                ctx_as(None),
                serde_json::json!({
                    "name": "child",
                    "system_prompt": "s",
                    "spawned_by": "invented"
                }),
            )
            .await;
        let child_id = structured(&out)["agent_id"]
            .as_str()
            .expect("agent_id")
            .to_string();
        let child = l.get_agent(&child_id).await.expect("get").expect("row");
        assert_eq!(child.spawned_by, None, "anonymous callers claim nothing");
    }

    /// The operator's terminal has no identity to check and stays
    /// trusted; drivers and boot-applied config attribute this way.
    #[tokio::test]
    async fn the_operator_surface_still_takes_the_claim() {
        let l = ledger().await;
        let parent = l
            .create_agent(&AgentDef::new("p", "s"), None)
            .await
            .expect("parent");
        let spawn = spawn_tool(l.clone(), 3, true);
        let out = spawn
            .call_with_context(
                ctx_as(None),
                serde_json::json!({
                    "name": "child",
                    "system_prompt": "s",
                    "spawned_by": parent
                }),
            )
            .await;
        let child_id = structured(&out)["agent_id"]
            .as_str()
            .expect("agent_id")
            .to_string();
        let child = l.get_agent(&child_id).await.expect("get").expect("row");
        assert_eq!(child.spawned_by.as_deref(), Some(parent.as_str()));
    }

    /// The ceiling: a child's tools are at most its parent's, and what
    /// was refused is named rather than silently dropped.
    #[tokio::test]
    async fn a_child_cannot_out_reach_its_parent() {
        let l = ledger().await;
        let parent = l
            .create_agent(
                &AgentDef::new("p", "s").allowed_tools(["Read".to_string(), "Grep".to_string()]),
                None,
            )
            .await
            .expect("parent");
        let spawn = spawn_tool(l.clone(), 3, false);
        let out = spawn
            .call_with_context(
                ctx_as(Some(&parent)),
                serde_json::json!({
                    "name": "child",
                    "system_prompt": "s",
                    "allowed_tools": ["Read", "Bash(rm:*)"]
                }),
            )
            .await;
        let sc = structured(&out);
        let child_id = sc["agent_id"].as_str().expect("agent_id");
        assert_eq!(
            sc["tools_denied"].as_array().map(|a| a.len()),
            Some(1),
            "the refused tool is reported: {sc}"
        );
        let child = l.get_agent(child_id).await.expect("get").expect("row");
        assert_eq!(child.def.allowed_tools, vec!["Read".to_string()]);
    }

    /// An authenticated caller does not pick its child's MCP config.
    /// The operator config path is guessable, and this is the door it
    /// would walk through.
    #[tokio::test]
    async fn an_authenticated_caller_cannot_choose_the_childs_mcp_config() {
        let l = ledger().await;
        let parent = l
            .create_agent(&AgentDef::new("p", "s"), None)
            .await
            .expect("parent");
        let spawn = spawn_tool(l.clone(), 3, false);
        let out = spawn
            .call_with_context(
                ctx_as(Some(&parent)),
                serde_json::json!({
                    "name": "child",
                    "system_prompt": "s",
                    "mcp_config": "/tmp/ciacola-mcp-operator.json"
                }),
            )
            .await;
        assert!(
            rendered(&out).contains("does not choose"),
            "must refuse: {}",
            rendered(&out)
        );
    }

    /// An agent never mints a supervisor, even through a role that
    /// carries one.
    #[tokio::test]
    async fn an_agent_cannot_spawn_an_operator_surface_role() {
        let l = ledger().await;
        let parent = l
            .create_agent(&AgentDef::new("p", "s"), None)
            .await
            .expect("parent");
        let role = crate::roles::Role {
            name: "boss".into(),
            description: "d".into(),
            model: None,
            effort: None,
            hermetic: None,
            working_dir: None,
            allowed_tools: Vec::new(),
            max_turns: None,
            rotate_after_turns: None,
            loopback: true,
            surface: Some("operator".into()),
            arguments: Vec::new(),
            system_prompt: "s".into(),
        };
        let roles = crate::roles::Roles::new(vec![role], "agent.json");
        let tools = crate::roles::tools_with_depth(roles, l.clone(), 3, false);
        let spawn_role = tools
            .iter()
            .find(|t| t.definition().name == "spawn_role")
            .expect("spawn_role");
        let out = spawn_role
            .call_with_context(
                ctx_as(Some(&parent)),
                serde_json::json!({"role": "boss", "arguments": {}}),
            )
            .await;
        assert!(
            rendered(&out).contains("may not spawn it"),
            "must refuse: {}",
            rendered(&out)
        );
    }

    /// A role is another creation path, not an exemption from the
    /// capability ceiling. Refuse rather than silently trim it because a
    /// role's prompt is written against its complete provisioned bundle.
    #[tokio::test]
    async fn a_role_cannot_out_reach_its_authenticated_parent() {
        let l = ledger().await;
        let parent = l
            .create_agent(&AgentDef::new("p", "s").allowed_tools(["Read"]), None)
            .await
            .expect("parent");
        let role = crate::roles::Role {
            name: "writer".into(),
            description: "d".into(),
            model: None,
            effort: None,
            hermetic: None,
            working_dir: None,
            allowed_tools: vec!["Read".into(), "Edit".into()],
            max_turns: None,
            rotate_after_turns: None,
            loopback: false,
            surface: None,
            arguments: Vec::new(),
            system_prompt: "s".into(),
        };
        let roles = crate::roles::Roles::new(vec![role], "agent.json");
        let spawn_role = crate::roles::tools_with_depth(roles, l.clone(), 3, false)
            .into_iter()
            .find(|t| t.definition().name == "spawn_role")
            .expect("spawn_role");
        let out = spawn_role
            .call_with_context(
                ctx_as(Some(&parent)),
                serde_json::json!({"role": "writer", "arguments": {}}),
            )
            .await;
        assert!(
            rendered(&out).contains("needs tools its parent does not hold: Edit"),
            "must refuse: {}",
            rendered(&out)
        );
        assert_eq!(l.list_agents().await.expect("list").len(), 1);
    }

    /// Missing identity on the agent mount is not operator authority. The
    /// known anonymous-operator gap belongs to the operator mount alone.
    #[tokio::test]
    async fn an_anonymous_agent_surface_caller_cannot_spawn_an_operator_role() {
        let l = ledger().await;
        let role = crate::roles::Role {
            name: "boss".into(),
            description: "d".into(),
            model: None,
            effort: None,
            hermetic: None,
            working_dir: None,
            allowed_tools: Vec::new(),
            max_turns: None,
            rotate_after_turns: None,
            loopback: true,
            surface: Some("operator".into()),
            arguments: Vec::new(),
            system_prompt: "s".into(),
        };
        let roles = crate::roles::Roles::new(vec![role], "agent.json")
            .with_operator_mcp_config("operator.json");
        let spawn_role = crate::roles::tools_with_depth(roles, l.clone(), 3, false)
            .into_iter()
            .find(|t| t.definition().name == "spawn_role")
            .expect("spawn_role");
        let out = spawn_role
            .call_with_context(
                ctx_as(None),
                serde_json::json!({"role": "boss", "arguments": {}}),
            )
            .await;
        assert!(rendered(&out).contains("may not spawn it"));
        assert!(l.list_agents().await.expect("list").is_empty());
    }
}
