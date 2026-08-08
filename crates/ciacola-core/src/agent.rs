//! The core operation: define an agent, prompt it, prompt it again.
//!
//! Deliberately a thin wrapper over `claude-wrapper`. The only state this
//! layer adds is the session id (which makes the second prompt a
//! continuation rather than a fresh conversation) and the record of what
//! each turn cost. Everything else is passthrough.

use std::path::PathBuf;

use claude_wrapper::{Claude, QueryCommand};
use serde::{Deserialize, Serialize};

/// Errors are boxed strings for now; this is a spike, and nothing
/// downstream branches on the variant yet.
pub type FlatError = Box<dyn std::error::Error + Send + Sync>;

/// What an agent *is*, before it has said anything. The knowledge lives
/// in `system_prompt`; everything else is provider plumbing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDef {
    pub name: String,
    pub system_prompt: String,
    /// Provider model. `None` means the provider's default.
    pub model: Option<String>,
    /// How hard to think: low, medium, high, xhigh, max. Paired with
    /// model because that is how it is actually chosen, and worth
    /// having per role: a manager reasoning about supervision wants
    /// more than a spoke summarizing a diff.
    #[serde(default)]
    pub effort: Option<String>,
    /// Tools the agent may use. Empty means none beyond conversation.
    pub allowed_tools: Vec<String>,
    /// Where the agent works. `None` means it does not touch a filesystem.
    pub working_dir: Option<PathBuf>,
    /// Cap on provider-internal turns per prompt. `None` means provider
    /// default.
    pub max_turns: Option<u32>,
    /// Path to an MCP config file handed to the provider CLI. This is
    /// the recursion mechanism: point it at a server (this one, say)
    /// and the agent can drive agents with the same verbs a person
    /// uses. Applied strictly, so the agent sees only these servers.
    #[serde(default)]
    pub mcp_config: Option<String>,
    /// Start a fresh provider session after this many turns in the
    /// current one. `None` means never, which is right for short-lived
    /// agents and wrong for anything that runs forever: a provider
    /// session grows without bound and eventually cannot be resumed.
    /// Rotation is cheap here because durable state lives in the
    /// ledger, not the conversation.
    #[serde(default)]
    pub rotate_after_turns: Option<u32>,
    /// Seal the ambient provider config: "full" drops user, project,
    /// and local settings; "project" keeps the user's global ones.
    ///
    /// Worth defaulting on for unattended work. An interactive session
    /// inherits a CLAUDE.md, skills, and settings that the person can
    /// see and reason about; a scheduled agent inherits them invisibly,
    /// so its behaviour depends on files nobody remembered were there.
    #[serde(default)]
    pub hermetic: Option<String>,
    /// Provider config and session directory (`CLAUDE_CONFIG_DIR`).
    ///
    /// Pointing it at the server's own directory keeps transcripts with
    /// the run that produced them rather than mixed into the operator's
    /// history, which is the precondition for mining them.
    ///
    /// **It also isolates credentials.** The config directory is where
    /// the CLI keeps its login, so a fresh one authenticates as nobody
    /// and every run fails with "Not logged in". That directory has to
    /// be logged in separately before this is usable, which is why it
    /// is off by default and warned about at boot rather than quietly
    /// breaking the first turn.
    #[serde(default)]
    pub claude_home: Option<String>,
    /// Operator house rules, prepended to every agent's system prompt.
    ///
    /// A hermetic agent inherits no ambient `CLAUDE.md`, which is the
    /// point and also the trap: the first real run committed a
    /// `Co-Authored-By` trailer the operator's global rules forbid,
    /// because isolation had removed the file those rules lived in.
    /// Isolation has to be paired with a deliberate way to put the
    /// rules back.
    #[serde(default)]
    pub house_rules: Option<String>,
    /// Name of an environment variable holding a long-lived token, read
    /// from the server's own environment and passed to the provider.
    ///
    /// This is what makes `claude_home` usable: `CLAUDE_CONFIG_DIR`
    /// isolates the login along with the session data, so a fresh
    /// directory authenticates as nobody. `claude setup-token` mints a
    /// token for exactly this, and `CLAUDE_CODE_OAUTH_TOKEN` is where
    /// the provider looks for it, ahead of the subscription login it
    /// can no longer find.
    ///
    /// The *name* rather than the value, so a token never lands in the
    /// config file, the ledger, or the board.
    #[serde(default)]
    pub token_env: Option<String>,
}

impl AgentDef {
    pub fn new(name: impl Into<String>, system_prompt: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            system_prompt: system_prompt.into(),
            model: None,
            effort: None,
            allowed_tools: Vec::new(),
            working_dir: None,
            max_turns: None,
            mcp_config: None,
            rotate_after_turns: None,
            hermetic: None,
            claude_home: None,
            house_rules: None,
            token_env: None,
        }
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    pub fn allowed_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn working_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.working_dir = Some(dir.into());
        self
    }

    pub fn max_turns(mut self, turns: u32) -> Self {
        self.max_turns = Some(turns);
        self
    }

    pub fn mcp_config(mut self, path: impl Into<String>) -> Self {
        self.mcp_config = Some(path.into());
        self
    }

    pub fn rotate_after_turns(mut self, turns: u32) -> Self {
        self.rotate_after_turns = Some(turns);
        self
    }

    pub fn hermetic(mut self, scope: impl Into<String>) -> Self {
        self.hermetic = Some(scope.into());
        self
    }

    pub fn claude_home(mut self, dir: impl Into<String>) -> Self {
        self.claude_home = Some(dir.into());
        self
    }

    pub fn house_rules(mut self, rules: impl Into<String>) -> Self {
        self.house_rules = Some(rules.into());
        self
    }

    pub fn token_env(mut self, var: impl Into<String>) -> Self {
        self.token_env = Some(var.into());
        self
    }
}

/// A durable conversation. Can exist while nothing is running; `session`
/// is the whole of what makes it resumable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub def: AgentDef,
    /// Provider session id, set by the first completed turn. This is the
    /// recovery mechanism: anyone holding it can continue the
    /// conversation, from any process, at any later time.
    pub session: Option<String>,
    pub turns: Vec<Turn>,
    pub cost_micro_usd: u64,
}

impl Agent {
    pub fn new(def: AgentDef) -> Self {
        Self {
            id: ulid::Ulid::new().to_string(),
            def,
            session: None,
            turns: Vec::new(),
            cost_micro_usd: 0,
        }
    }
}

/// One exchange: our prompt, the agent's reply, what it cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub seq: u32,
    pub prompt: String,
    pub reply: String,
    pub cost_micro_usd: u64,
    #[serde(default)]
    pub tokens_in: u64,
    #[serde(default)]
    pub tokens_out: u64,
    /// Provider-internal turns spent producing the reply (tool calls etc).
    pub num_turns: u32,
    pub elapsed_ms: u64,
}

/// The outcome of one exchange with the provider, before anything is
/// recorded anywhere.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub reply: String,
    pub session: String,
    pub cost_micro_usd: u64,
    /// Tokens, kept separately from cost because they are the portable
    /// measure: codex reports usage but no price, so a ledger that
    /// records only dollars goes blank the moment a second provider
    /// lands. Cached input is a subset of input, reported when the
    /// provider distinguishes it.
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens_cached: u64,
    pub num_turns: u32,
    pub elapsed_ms: u64,
    /// The provider reported the run as an error. The exchange still
    /// happened: it cost money and may have advanced the session, so it
    /// comes back as data rather than as `Err`, which would throw both
    /// away. `Err` from [`run_exchange`] means the process could not be
    /// run at all.
    pub error: Option<String>,
}

/// What is true of every agent here and of no interactive session.
///
/// Written by the system rather than configured, because these are
/// facts about the environment an agent cannot discover and must not
/// be told wrongly.
const SYSTEM_PREAMBLE: &str = "\
You are running inside an autonomous agent server, not an interactive \
session. Some consequences that do not hold elsewhere:

- Nobody is necessarily watching. Your reply may be read minutes or \
  hours from now, or only by another agent.
- You cannot ask a clarifying question and get an answer in this turn. \
  If something is genuinely ambiguous, do the part that is not, and say \
  plainly what you did not do and why.
- This conversation is not your memory. Your durable state lives in the \
  server: work items, server memory, and findings. Your provider session \
  may be closed and reopened at any point, and when that happens you \
  will be told so.
- Report honestly. An overstated result is worse than a partial one, \
  because nobody is here to catch it.";

/// Facts about this agent's own provisioning, generated rather than
/// written.
///
/// A prompt that describes tools the agent was not granted is the
/// failure the findings queue caught twice: a toolless spoke does not
/// refuse, it fabricates. Generating this block from the definition
/// means the description cannot drift from the grant.
fn capability_block(def: &AgentDef) -> String {
    let mut lines = vec![format!("You are '{}'.", def.name)];
    if def.allowed_tools.is_empty() {
        lines.push(
            "You have NO tools: no file access, no shell, no network. You \
             can only reason and reply. If a task needs any of those, say \
             so and stop rather than guessing at what you would have found."
                .into(),
        );
    } else {
        lines.push(format!(
            "Your tools, and you have no others: {}.",
            def.allowed_tools.join(", ")
        ));
    }
    match &def.working_dir {
        Some(dir) => lines.push(format!("You work in {}.", dir.display())),
        None => lines.push("You have no working directory.".into()),
    }
    if let Some(turns) = def.max_turns {
        lines.push(format!(
            "You have at most {turns} internal turns for this reply; spend \
             them on the task rather than on preamble."
        ));
    }
    lines.join("\n")
}

/// System prompt, layered: what the system knows, what the operator
/// requires, what this agent has, and what its role is.
///
/// Composed here rather than at definition time because this is where
/// every path converges. The budget circuit breaker taught that lesson
/// the expensive way: a guard on one path is not a guard.
pub fn compose_system_prompt(def: &AgentDef) -> String {
    let mut parts = vec![SYSTEM_PREAMBLE.to_string()];
    if let Some(rules) = &def.house_rules {
        if !rules.trim().is_empty() {
            parts.push(format!("## House rules\n\n{}", rules.trim()));
        }
    }
    parts.push(format!("## You\n\n{}", capability_block(def)));
    parts.push(format!("## Your role\n\n{}", def.system_prompt.trim()));
    parts.join("\n\n")
}

/// One exchange with the provider: say `text` to the conversation
/// identified by `session` (or start one under `def`'s system prompt).
///
/// `started` is whether that session has been opened at the provider
/// yet. An id is assigned before the first turn runs, so its presence
/// says nothing about whether a conversation exists behind it.
///
/// Pure in the sense that matters: it mutates nothing of ours. Every
/// caller decides for itself where the outcome is recorded, which is
/// what lets the same function serve the in-memory `prompt`, the
/// hand-rolled executor, and the apalis-driven one.
#[tracing::instrument(
    skip_all,
    fields(
        agent = %def.name,
        model = def.model.as_deref().unwrap_or("default"),
        effort = def.effort.as_deref().unwrap_or("default"),
        resumed = started && session.is_some(),
        cost_micro_usd = tracing::field::Empty,
        tokens_out = tracing::field::Empty,
    )
)]
pub async fn run_exchange(
    def: &AgentDef,
    session: Option<&str>,
    started: bool,
    text: &str,
) -> Result<Exchange, FlatError> {
    let mut builder = Claude::builder();
    if let Some(dir) = &def.working_dir {
        builder = builder.working_dir(dir);
    }
    if let Some(home) = &def.claude_home {
        // The CLI reads its config and writes its sessions here.
        std::fs::create_dir_all(home)?;
        builder = builder.env("CLAUDE_CONFIG_DIR", home);
    }
    if let Some(var) = &def.token_env {
        match std::env::var(var) {
            Ok(token) if !token.is_empty() => {
                builder = builder.env("CLAUDE_CODE_OAUTH_TOKEN", token);
            }
            _ => {
                tracing::warn!(var, "token_env is set but the variable is empty or unset");
            }
        }
    }
    let claude = builder.build()?;

    let mut command = query_for_session(def, session, started, text);
    if let Some(model) = &def.model {
        command = command.model(model);
    }
    if let Some(effort) = &def.effort {
        match effort.to_ascii_lowercase().as_str() {
            "low" => command = command.effort(claude_wrapper::Effort::Low),
            "medium" => command = command.effort(claude_wrapper::Effort::Medium),
            "high" => command = command.effort(claude_wrapper::Effort::High),
            "xhigh" => command = command.effort(claude_wrapper::Effort::Xhigh),
            "max" => command = command.effort(claude_wrapper::Effort::Max),
            // Unknown values are ignored rather than fatal: config
            // should not brick an agent over a typo in an optional hint.
            other => eprintln!("[agent] unknown effort '{other}', using provider default"),
        }
    }
    if !def.allowed_tools.is_empty() {
        command = command.allowed_tools(def.allowed_tools.clone());
    }
    if let Some(max_turns) = def.max_turns {
        command = command.max_turns(max_turns);
    }
    if let Some(mcp_config) = &def.mcp_config {
        command = command.mcp_config(mcp_config).strict_mcp_config();
    }
    if let Some(scope) = &def.hermetic {
        match scope.to_ascii_lowercase().as_str() {
            "full" | "true" => {
                command = command.hermetic_scoped(claude_wrapper::HermeticScope::Full)
            }
            "project" => command = command.hermetic_scoped(claude_wrapper::HermeticScope::Project),
            "none" | "false" => {}
            other => eprintln!("[agent] unknown hermetic scope '{other}', inheriting ambient"),
        }
    }

    let started = std::time::Instant::now();
    let result = match command.execute_json(&claude).await {
        Ok(result) => result,
        Err(e) => match capped(&e, started.elapsed().as_millis() as u64, session) {
            Some(exchange) => return Ok(exchange),
            None => return Err(e.into()),
        },
    };

    let usage = result.usage.unwrap_or_default();
    let span = tracing::Span::current();
    span.record(
        "cost_micro_usd",
        result
            .cost_usd
            .map(|u| (u * 1e6) as u64)
            .unwrap_or_default(),
    );
    span.record("tokens_out", usage.output_tokens.unwrap_or_default());
    // An explicit event as well as the span fields: the span carries
    // structure for a real collector later, the event is what a person
    // reading stderr today actually sees.
    tracing::info!(
        agent = %def.name,
        model = def.model.as_deref().unwrap_or("default"),
        cost_micro_usd = result.cost_usd.map(|u| (u * 1e6) as u64).unwrap_or_default(),
        tokens_in = usage.input_tokens.unwrap_or_default(),
        tokens_out = usage.output_tokens.unwrap_or_default(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        is_error = result.is_error,
        "exchange complete"
    );
    Ok(Exchange {
        error: result.is_error.then(|| result.result.trim().to_string()),
        reply: result.result.trim().to_string(),
        session: result.session_id,
        cost_micro_usd: result
            .cost_usd
            .map(|usd| (usd * 1_000_000.0) as u64)
            .unwrap_or_default(),
        tokens_in: usage.input_tokens.unwrap_or_default(),
        tokens_out: usage.output_tokens.unwrap_or_default(),
        tokens_cached: usage.cached_input_tokens.unwrap_or_default(),
        num_turns: result.num_turns.unwrap_or_default(),
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

/// Apply the one session-state choice before adding the rest of an
/// agent's provider options. Kept separate so the ledger-to-provider
/// resume invariant can be tested without spawning the CLI.
pub(crate) fn query_for_session(
    def: &AgentDef,
    session: Option<&str>,
    started: bool,
    text: &str,
) -> QueryCommand {
    match (session, started) {
        // The session carries the system prompt and everything said so
        // far; the resumed prompt is just the next thing we say.
        (Some(session), true) => QueryCommand::new(text).resume(session),
        // Assigned but not yet opened: name it, and send the system
        // prompt that the session will carry from here on.
        (Some(session), false) => QueryCommand::new(text)
            .session_id(session)
            .system_prompt(compose_system_prompt(def)),
        // Agents created before ids were assigned have none. They keep
        // the old behaviour and pick one up from the provider.
        (None, _) => QueryCommand::new(text).system_prompt(compose_system_prompt(def)),
    }
}

/// A run that hit a ceiling we set, rendered as the exchange it was.
///
/// The provider ran, at length, and stopped at a cap. That is not the
/// same as failing to run, and the difference is worth real money: the
/// wrapper keeps `cost_usd` and `session_id` off the terminal result
/// event, and stringifying the error throws both away. A five minute
/// run then lands in the ledger as costing nothing, which is invisible
/// to the spend limit and to anything reading the board.
///
/// Returning an `Exchange` with `error` set routes it through the path
/// that already records spend for a run that errored, rather than
/// adding a second one. Both variants are `#[non_exhaustive]`, hence
/// the `..`.
fn capped(
    e: &claude_wrapper::Error,
    elapsed_ms: u64,
    assigned_session: Option<&str>,
) -> Option<Exchange> {
    let (cost_usd, num_turns, session_id) = match e {
        claude_wrapper::Error::MaxTurnsExceeded {
            cost_usd,
            num_turns,
            session_id,
            ..
        }
        | claude_wrapper::Error::MaxBudgetExceeded {
            cost_usd,
            num_turns,
            session_id,
            ..
        } => (*cost_usd, *num_turns, session_id.clone()),
        _ => return None,
    };
    Some(Exchange {
        error: Some(e.to_string()),
        reply: String::new(),
        // A cap is a terminal provider result, so it proves that the
        // assigned session was opened even if this CLI version omitted
        // the id from its error payload.
        session: session_id
            .or_else(|| assigned_session.map(str::to_string))
            .unwrap_or_default(),
        cost_micro_usd: cost_usd
            .map(|u| (u * 1_000_000.0) as u64)
            .unwrap_or_default(),
        // The cap events carry no usage breakdown, so tokens stay zero
        // rather than being invented. Cost is the number that matters
        // here and it is real.
        tokens_in: 0,
        tokens_out: 0,
        tokens_cached: 0,
        num_turns: num_turns.unwrap_or_default(),
        elapsed_ms,
    })
}

/// Prompt the agent once. Resumes its session if it has one, starts the
/// conversation if not. On success the turn is recorded on the agent and
/// returned.
pub async fn prompt<'a>(agent: &'a mut Agent, text: &str) -> Result<&'a Turn, FlatError> {
    let started = agent.session.is_some();
    let exchange = run_exchange(&agent.def, agent.session.as_deref(), started, text).await?;
    if let Some(error) = exchange.error {
        return Err(error.into());
    }
    agent.session = Some(exchange.session);
    agent.cost_micro_usd += exchange.cost_micro_usd;
    let seq = agent.turns.len() as u32 + 1;
    agent.turns.push(Turn {
        seq,
        prompt: text.to_string(),
        reply: exchange.reply,
        cost_micro_usd: exchange.cost_micro_usd,
        tokens_in: exchange.tokens_in,
        tokens_out: exchange.tokens_out,
        num_turns: exchange.num_turns,
        elapsed_ms: exchange.elapsed_ms,
    });
    Ok(agent.turns.last().expect("just pushed"))
}

#[cfg(test)]
mod cap_tests {
    use super::*;

    /// Built through the wrapper's own classifier rather than by naming
    /// the variant, which is `#[non_exhaustive]` and cannot be
    /// constructed here anyway. The upside is that this drives the real
    /// detection path from the bytes the CLI actually emits, so it also
    /// fails if that classification ever changes shape.
    fn capped_error(result_json: &str) -> claude_wrapper::Error {
        claude_wrapper::Error::from_command_failure(
            "claude -p ...".into(),
            1,
            result_json.into(),
            String::new(),
            None,
        )
    }

    /// The bug this exists for: a run that worked for minutes and hit
    /// its cap was recorded as costing nothing, which is invisible to
    /// the spend limit.
    #[test]
    fn a_capped_run_keeps_its_spend_and_session() {
        let e = capped_error(
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,
                "total_cost_usd":1.25,"num_turns":60,"session_id":"sess-1",
                "errors":["Reached maximum number of turns (60)"]}"#,
        );
        let x = capped(&e, 323_000, None).expect("a cap is an exchange, not a failure to run");
        assert_eq!(x.cost_micro_usd, 1_250_000, "spend must survive the cap");
        assert_eq!(
            x.session, "sess-1",
            "session must survive, so it can resume"
        );
        assert_eq!(
            x.elapsed_ms, 323_000,
            "wall clock is measured here regardless"
        );
        assert!(
            x.error.is_some(),
            "it still failed, and must read as failed"
        );
    }

    /// Cost is reported only when the result event carried it. Absent
    /// means zero, not a panic and not an invented number.
    #[test]
    fn a_capped_run_without_a_reported_cost_is_zero() {
        let e = capped_error(
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,
                "errors":["Reached maximum number of turns (60)"]}"#,
        );
        let x = capped(&e, 10, Some("assigned-1")).expect("still an exchange");
        assert_eq!(x.cost_micro_usd, 0);
        assert_eq!(
            x.session, "assigned-1",
            "the cap itself proves the preassigned session opened"
        );
        assert!(x.error.is_some());
    }

    /// Everything else is still a failure to run and must keep
    /// propagating, or a real error would be quietly downgraded into an
    /// empty exchange that looks like it merely errored.
    #[test]
    fn an_ordinary_failure_is_not_treated_as_an_exchange() {
        let e = capped_error("command not found");
        assert!(capped(&e, 10, Some("unopened")).is_none());
    }
}
