//! The core operation: define an agent, prompt it, prompt it again.
//!
//! Deliberately thin over [`ciacola_agent`], the provider contract. The
//! only state this layer adds is the session id (which makes the second
//! prompt a continuation rather than a fresh conversation) and the
//! record of what each turn cost. Everything else is translation.
//!
//! # What this module no longer knows
//!
//! Which backend runs the turn. An [`AgentDef`] carries a
//! [`ProviderKey`], a [`ProviderRegistry`] resolves it to an adapter,
//! and the adapter owns every CLI flag, wire format, and process.
//! Nothing here imports a wrapper, which is the property issue 53
//! exists to establish: a second backend arrives as an adapter crate
//! and a key, not as another round of conditionals in this file.

use std::path::PathBuf;

use ciacola_agent::{
    AgentError, Capabilities, Cost, Effort, Isolation, McpScope, ProviderKey, ProviderRegistry,
    ResumeId, TokenUsage, TurnEvents, TurnIntent, TurnOutcome, Usage,
};
use serde::{Deserialize, Serialize};

/// Errors are boxed strings for now; nothing downstream branches on
/// the variant yet.
pub type FlatError = Box<dyn std::error::Error + Send + Sync>;

/// What an agent *is*, before it has said anything. The knowledge lives
/// in `system_prompt`; everything else is provider plumbing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDef {
    pub name: String,
    /// Stable catalog role this definition was instantiated from.
    ///
    /// Kept separate from `name`: callers rename role instances (for
    /// example `impl-owner-repo-42`) while tuning and operator surfaces
    /// still need to know that the instance came from
    /// `issue-implementer`. Direct spawns and definitions written before
    /// this field existed have no catalog provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    catalog_role: Option<String>,
    pub system_prompt: String,
    /// Which backend runs this agent's turns.
    ///
    /// `#[serde(default)]` over a type whose `Default` is `claude` is
    /// the whole of the migration story for the live ledger: every
    /// agent row and every config file written before this field
    /// existed has no `provider` key at all, and each one must come
    /// back as a Claude agent that resumes its existing session rather
    /// than as a parse error or a fresh conversation.
    #[serde(default)]
    pub provider: ProviderKey,
    /// Whether the storage boundary should replace `provider` with the
    /// server-wide default. New definitions start inherited; selecting a
    /// provider makes it explicit. Stored definitions omit this transient
    /// bit, so changing a default never moves an existing conversation to a
    /// different backend by accident.
    #[serde(default, skip_serializing)]
    provider_inherited: bool,
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
    /// Use the provider's native tool and execution policy instead of a
    /// Claude-style named grant. Required for providers whose controls are
    /// not the same vocabulary; explicit because it hands policy to the
    /// adapter rather than pretending an empty Claude list is enforced.
    #[serde(default)]
    pub inherit_provider_tools: bool,
    /// Filesystem and network containment: read-only, workspace-write,
    /// workspace-write-no-network, or none.
    #[serde(default)]
    pub sandbox: Option<String>,
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
    /// Provider config, authentication, and session directory.
    ///
    /// Pointing it at the server's own directory keeps transcripts with
    /// the run that produced them rather than mixed into the operator's
    /// history, which is the precondition for mining them.
    ///
    /// **It also selects credentials.** The config directory is where the CLI
    /// keeps its login, so a fresh one authenticates as nobody unless the
    /// runtime supplies its provider credential through the dedicated startup
    /// descriptor. Log the directory in separately or configure that runtime
    /// credential before use; boot warns rather than quietly breaking the
    /// first turn.
    #[serde(default, alias = "claude_home")]
    pub config_home: Option<String>,
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
    /// Legacy name of an environment variable that held a long-lived token.
    ///
    /// Retained for backward decoding of persisted definitions. New runtime
    /// configuration no longer populates it: startup environment credentials
    /// are unsafe and are migrated to inherited descriptors. A non-empty
    /// legacy value reaches the turn intent so current adapters can refuse it
    /// before spawning rather than silently selecting another credential.
    /// Config-managed agents lose the marker when their current definition is
    /// reapplied; other persisted agents must be replaced or retired and
    /// recreated.
    #[serde(default)]
    pub token_env: Option<String>,
}

impl AgentDef {
    pub fn new(name: impl Into<String>, system_prompt: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            catalog_role: None,
            system_prompt: system_prompt.into(),
            provider: ProviderKey::claude(),
            provider_inherited: true,
            model: None,
            effort: None,
            allowed_tools: Vec::new(),
            inherit_provider_tools: false,
            sandbox: None,
            working_dir: None,
            max_turns: None,
            mcp_config: None,
            rotate_after_turns: None,
            hermetic: None,
            config_home: None,
            house_rules: None,
            token_env: None,
        }
    }

    /// Catalog role this definition was instantiated from, if any.
    ///
    /// This is read-only outside core. Role provenance is assigned by the
    /// catalog itself rather than accepted from raw spawn input.
    pub fn catalog_role(&self) -> Option<&str> {
        self.catalog_role.as_deref()
    }

    pub(crate) fn with_catalog_role(mut self, role: impl Into<String>) -> Self {
        self.catalog_role = Some(role.into());
        self
    }

    /// Choose the backend. Omitted definitions inherit the server default at
    /// the storage boundary; legacy stored definitions remain Claude.
    pub fn provider(mut self, provider: impl Into<ProviderKey>) -> Self {
        self.provider = provider.into();
        self.provider_inherited = false;
        self
    }

    /// Resolve an inherited provider at the storage boundary.
    pub(crate) fn resolve_provider(&mut self, provider: impl Into<ProviderKey>) {
        if self.provider_inherited {
            self.provider = provider.into();
            self.provider_inherited = false;
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
        self.inherit_provider_tools = false;
        self
    }

    /// Use the provider's native tool policy rather than a named grant.
    pub fn inherit_provider_tools(mut self) -> Self {
        self.allowed_tools.clear();
        self.inherit_provider_tools = true;
        self
    }

    pub fn sandbox(mut self, sandbox: impl Into<String>) -> Self {
        self.sandbox = Some(sandbox.into());
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

    pub fn config_home(mut self, dir: impl Into<String>) -> Self {
        self.config_home = Some(dir.into());
        self
    }

    pub fn house_rules(mut self, rules: impl Into<String>) -> Self {
        self.house_rules = Some(rules.into());
        self
    }

    /// Attach a legacy token source name for compatibility tests and stored
    /// definitions.
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
    /// Provider session id, assigned before the first turn and confirmed
    /// when the provider opens it. This is the recovery mechanism:
    /// anyone holding it can continue the conversation, from any process,
    /// at any later time.
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
    /// A conversation id learned from the provider. `None` must not
    /// erase an id already in the ledger.
    pub session: Option<String>,
    pub cost: Cost,
    /// Whether a reported cost came from the provider's terminal outcome
    /// rather than a best-known lower bound captured before a failure.
    ///
    /// A partial reported amount is still banked, but admission must treat
    /// it as a completeness gap under a configured USD stop.
    pub cost_complete: bool,
    /// Tokens, kept separately from cost because they are the portable
    /// measure: codex reports usage but no price, so a ledger that
    /// records only dollars goes blank the moment a second provider
    /// lands. Cached input is a subset of input, reported when the
    /// provider distinguishes it.
    pub usage: Usage,
    /// Whether `usage` came from the provider's terminal outcome rather
    /// than a best-known lower bound captured before a failure.
    ///
    /// `Usage::Reported` answers whether buckets were observed;
    /// this answers whether those buckets are authoritative for the
    /// whole turn. Admission needs both to fail closed without throwing
    /// away useful partial accounting.
    pub usage_complete: bool,
    pub provider_turns: Option<u32>,
    pub elapsed_ms: u64,
    /// The provider reported the run as an error. The exchange still
    /// happened: it cost money and may have advanced the session, so it
    /// comes back as data rather than as `Err`, which would throw both
    /// away. `Err` from [`run_exchange`] means the process could not be
    /// run at all.
    pub error: Option<String>,
    /// Typed reason a provider run ended badly. Kept beside `error` so a
    /// limit remains machine-readable through persistence rather than being
    /// inferred from provider prose.
    pub failure_kind: Option<ciacola_agent::FailureKind>,
}

impl Exchange {
    /// Reported money, flattened only for legacy aggregate columns.
    pub fn cost_micro_usd(&self) -> u64 {
        self.cost.micro_usd_or_zero()
    }

    /// Reported token buckets for callers that need the numeric view.
    pub fn tokens(&self) -> TokenUsage {
        self.usage.tokens().unwrap_or_default()
    }
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
    if def.inherit_provider_tools {
        lines.push(
            "Your tool and command policy is the provider-native policy selected for this agent."
                .into(),
        );
    } else if def.allowed_tools.is_empty() {
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
/// channel-driven executor, and the polling one.
#[tracing::instrument(
    skip_all,
    fields(
        agent = %def.name,
        provider = %def.provider,
        model = def.model.as_deref().unwrap_or("default"),
        effort = def.effort.as_deref().unwrap_or("default"),
        resumed = started && session.is_some(),
        cost_micro_usd = tracing::field::Empty,
        tokens_out = tracing::field::Empty,
    )
)]
pub async fn run_exchange(
    providers: &ProviderRegistry,
    def: &AgentDef,
    mcp: Option<McpScope>,
    session: Option<&str>,
    started: bool,
    text: &str,
    events: &dyn TurnEvents,
) -> Result<Exchange, FlatError> {
    run_exchange_with_ceiling(providers, def, mcp, session, started, text, None, events).await
}

/// Execute with the exact per-turn ceiling snapshot admitted by the ledger.
/// The public convenience path above remains unbounded for direct callers;
/// product execution always calls this persisted-policy path.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_exchange_with_ceiling(
    providers: &ProviderRegistry,
    def: &AgentDef,
    mcp: Option<McpScope>,
    session: Option<&str>,
    started: bool,
    text: &str,
    turn_ceiling: Option<ciacola_agent::TurnCeiling>,
    events: &dyn TurnEvents,
) -> Result<Exchange, FlatError> {
    let provider = providers.get(&def.provider)?;
    let mut intent = intent_for(def, mcp, session, started, text);
    intent.turn_ceiling = turn_ceiling;

    // Fail closed on anything that would widen what this agent can
    // reach or see; say the rest out loud and carry on. Where that line
    // falls is the contract's decision, not ours: see
    // `ciacola_agent::Constraint::security`.
    let capabilities = provider.capabilities();
    let validation = capabilities.validate(&intent);
    if let Some(blocking) = validation.blocking() {
        return Err(AgentError::Unsupported {
            provider: def.provider.clone(),
            constraint: blocking.constraint,
            detail: blocking.detail.clone(),
        }
        .into());
    }
    for warning in validation.warnings() {
        tracing::warn!(
            agent = %def.name,
            provider = %def.provider,
            constraint = ?warning.constraint,
            detail = %warning.detail,
            "provider cannot honour part of this turn; running without it"
        );
    }

    let started = std::time::Instant::now();
    match provider.run(&intent, events).await {
        Ok(outcome) => Ok(exchange_from(def, outcome)),
        // A failure that still knows what it spent comes back as an
        // exchange that errored, which is the path that already banks
        // spend and a session id. `Err` is reserved for a turn that
        // genuinely did not happen, because that is the only one with
        // nothing to bank.
        Err(e) => match partial_exchange(&e, &capabilities, started.elapsed()) {
            Some(exchange) => Ok(exchange),
            None => Err(e.into()),
        },
    }
}

/// Translate a definition and one thing to say into provider-neutral
/// intent.
///
/// Public to the crate so the ledger-to-provider resume invariant can be
/// tested without a provider, which is what the old `query_for_session`
/// existed for.
pub(crate) fn intent_for(
    def: &AgentDef,
    mcp: Option<McpScope>,
    session: Option<&str>,
    started: bool,
    text: &str,
) -> TurnIntent {
    let mut intent = TurnIntent::new(text);

    // The three session cases, unchanged from the pre-migration path.
    // `started` is whether the conversation has been opened at the
    // provider; an id is assigned before the first turn runs, so its
    // presence says nothing about that on its own.
    intent.resume = match (session, started) {
        // Open: continue it. The transcript already carries everything.
        (Some(id), true) => Some(ResumeId::ProviderAssigned(id.to_string())),
        // Named but not yet opened: this turn opens it under that name.
        (Some(id), false) => Some(ResumeId::ClientAssigned(id.to_string())),
        // Agents created before ids were assigned have none, and pick
        // one up from the provider.
        (None, _) => None,
    };
    // An open conversation already carries the instructions it was
    // opened with, and resending them would be a second system prompt.
    if !matches!(intent.resume, Some(ResumeId::ProviderAssigned(_))) {
        intent.instructions = Some(compose_system_prompt(def));
    }

    intent.model = def.model.clone();
    if let Some(effort) = &def.effort {
        match Effort::parse(effort) {
            Some(parsed) => intent.effort = Some(parsed),
            // Unknown values are ignored rather than fatal: config
            // should not brick an agent over a typo in an optional hint.
            None => eprintln!("[agent] unknown effort '{effort}', using provider default"),
        }
    }
    intent.working_dir = def.working_dir.clone();
    // Always `Some`, including `Some(empty)`. Every definition this
    // server produces is an explicit grant, and `None` would mean
    // "inherit whatever the backend gives you". Sending `None` for an
    // agent with no tools is precisely the pre-migration bug: the
    // system prompt told it that it had none while the command carried
    // no restriction at all.
    intent.allowed_tools = (!def.inherit_provider_tools).then(|| def.allowed_tools.clone());
    if let Some(sandbox) = &def.sandbox {
        match ciacola_agent::Sandbox::parse(sandbox) {
            Some(parsed) => intent.sandbox = parsed,
            None => {
                eprintln!("[agent] unknown sandbox '{sandbox}', falling back to read-only");
                intent.sandbox = ciacola_agent::Sandbox::ReadOnly;
            }
        }
    }
    intent.max_provider_turns = def.max_turns;
    intent.mcp = mcp;
    if let Some(scope) = &def.hermetic {
        match Isolation::parse(scope) {
            Some(parsed) => intent.isolation = parsed,
            None => eprintln!("[agent] unknown hermetic scope '{scope}', inheriting ambient"),
        }
    }
    intent.config_home = def.config_home.as_deref().map(expand_home_path);
    intent.token_env = def.token_env.clone();
    intent
}

fn expand_home_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME")
            .map(|home| format!("{home}/{rest}"))
            .unwrap_or_else(|_| path.to_string()),
        None => path.to_string(),
    }
}

/// Record a completed turn in the provider-neutral shape the ledger
/// stores. Money and usage keep their reported/unreported/not-tracked
/// state until persistence; only legacy aggregates flatten them.
fn exchange_from(def: &AgentDef, outcome: TurnOutcome) -> Exchange {
    if outcome.cost.is_missing() {
        tracing::warn!(
            agent = %def.name,
            provider = %def.provider,
            "provider prices its work but reported no cost for this turn"
        );
    }
    if outcome.usage.is_missing() {
        tracing::warn!(
            agent = %def.name,
            provider = %def.provider,
            "provider counts tokens but reported none for this turn"
        );
    }
    let cost_micro_usd = outcome.cost.micro_usd_or_zero();
    let tokens = outcome.usage.tokens().unwrap_or_default();
    let elapsed_ms = outcome.elapsed.as_millis() as u64;

    let span = tracing::Span::current();
    span.record("cost_micro_usd", cost_micro_usd);
    span.record("tokens_out", tokens.output);
    // An explicit event as well as the span fields: the span carries
    // structure for a real collector later, the event is what a person
    // reading stderr today actually sees.
    tracing::info!(
        agent = %def.name,
        provider = %def.provider,
        model = def.model.as_deref().unwrap_or("default"),
        cost_micro_usd,
        tokens_in = tokens.input,
        tokens_out = tokens.output,
        elapsed_ms,
        is_error = !outcome.succeeded(),
        "exchange complete"
    );

    let cost_complete = matches!(outcome.cost, Cost::Reported { .. });
    let usage_complete = matches!(outcome.usage, Usage::Reported(_));
    let failure_kind = outcome.failure.as_ref().map(|failure| failure.kind);
    Exchange {
        error: outcome.failure_message().map(str::to_string),
        failure_kind,
        reply: outcome.reply,
        session: outcome.resume.as_ref().map(|r| r.value().to_string()),
        cost: outcome.cost,
        cost_complete,
        usage: outcome.usage,
        usage_complete,
        provider_turns: outcome.provider_turns,
        elapsed_ms,
    }
}

/// A failure that still knows what it spent, rendered as the exchange it
/// was.
///
/// A cancelled or timed-out run may have worked for twenty minutes and
/// opened a conversation. Reporting it as `Err` and nothing else would
/// throw both away, which is the bug that made a long capped run land in
/// the ledger as free and unresumable. `None` means the turn really did
/// not happen: no process, no spend, nothing to record but the failure
/// itself.
fn partial_exchange(
    error: &AgentError,
    capabilities: &Capabilities,
    measured_elapsed: std::time::Duration,
) -> Option<Exchange> {
    let partial = error.partial()?;
    Some(Exchange {
        error: Some(error.to_string()),
        failure_kind: Some(ciacola_agent::FailureKind::Reported),
        reply: String::new(),
        session: partial.resume.as_ref().map(|r| r.value().to_string()),
        cost: partial.cost.unwrap_or(if capabilities.reports_cost {
            Cost::Unreported
        } else {
            Cost::NotPriced
        }),
        cost_complete: false,
        usage: partial
            .usage
            .map(Usage::Reported)
            .unwrap_or(if capabilities.reports_token_usage {
                Usage::Unreported
            } else {
                Usage::NotTracked
            }),
        usage_complete: false,
        // The contract carries no provider-turn count on a partial, and
        // inventing one would be worse than admitting the gap.
        provider_turns: None,
        elapsed_ms: partial.elapsed.unwrap_or(measured_elapsed).as_millis() as u64,
    })
}

/// Prompt the agent once. Resumes its session if it has one, starts the
/// conversation if not. On success the turn is recorded on the agent and
/// returned.
///
/// The cap handling that used to live beside this function is gone from
/// core, not lost: recognising a run that stopped at a ceiling is the
/// backend's job now, and the adapter returns it as a [`TurnOutcome`]
/// carrying its spend and its resume id rather than as an error.
pub async fn prompt<'a>(
    providers: &ProviderRegistry,
    agent: &'a mut Agent,
    text: &str,
) -> Result<&'a Turn, FlatError> {
    let started = agent.session.is_some();
    let exchange = run_exchange(
        providers,
        &agent.def,
        None,
        agent.session.as_deref(),
        started,
        text,
        &ciacola_agent::NoEvents,
    )
    .await?;
    if let Some(error) = exchange.error {
        return Err(error.into());
    }
    if let Some(session) = exchange.session.clone() {
        agent.session = Some(session);
    }
    agent.cost_micro_usd += exchange.cost_micro_usd();
    let tokens = exchange.tokens();
    let cost_micro_usd = exchange.cost_micro_usd();
    let seq = agent.turns.len() as u32 + 1;
    agent.turns.push(Turn {
        seq,
        prompt: text.to_string(),
        reply: exchange.reply,
        cost_micro_usd,
        tokens_in: tokens.input,
        tokens_out: tokens.output,
        num_turns: exchange.provider_turns.unwrap_or_default(),
        elapsed_ms: exchange.elapsed_ms,
    });
    Ok(agent.turns.last().expect("just pushed"))
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use ciacola_agent::{Cost, PartialTelemetry, TokenUsage, TurnFailure, Usage};
    use std::time::Duration;

    fn def() -> AgentDef {
        AgentDef::new("spoke", "do the thing")
    }

    /// The load-bearing compatibility property: every agent row and
    /// every config table written before this field existed has no
    /// `provider` key, and each one must come back as Claude rather than
    /// as a parse error.
    #[test]
    fn an_agent_def_without_a_provider_deserializes_as_claude() {
        let legacy = r#"{
            "name": "ciacola-manager",
            "system_prompt": "supervise",
            "model": null,
            "allowed_tools": ["Read"],
            "working_dir": null,
            "max_turns": null
        }"#;
        let def: AgentDef = serde_json::from_str(legacy).expect("a pre-provider row still parses");
        assert_eq!(def.provider, ProviderKey::claude());
        assert_eq!(def.name, "ciacola-manager");
        assert_eq!(def.catalog_role(), None);
        assert!(
            !def.provider_inherited,
            "a stored legacy definition must not follow a new runtime default"
        );
    }

    #[test]
    fn direct_definitions_omit_absent_catalog_provenance() {
        let value = serde_json::to_value(def()).expect("serialize");
        assert!(
            value.get("catalog_role").is_none(),
            "direct and legacy definitions should keep their old JSON shape: {value}"
        );
    }

    #[test]
    fn provider_policy_modes_replace_each_other() {
        let native = def().allowed_tools(["Read"]).inherit_provider_tools();
        assert!(native.inherit_provider_tools);
        assert!(native.allowed_tools.is_empty());

        let named = native.allowed_tools(["Read"]);
        assert!(!named.inherit_provider_tools);
        assert_eq!(named.allowed_tools, ["Read"]);
    }

    /// The three session cases, which are the ones that break live
    /// conversations if they move. An id we assigned but never opened is
    /// not the same request as one the provider already knows.
    #[test]
    fn the_three_session_cases_keep_their_exact_meaning() {
        let opened = intent_for(&def(), None, Some("sess-1"), true, "next");
        assert_eq!(
            opened.resume,
            Some(ResumeId::ProviderAssigned("sess-1".into()))
        );
        assert!(
            opened.instructions.is_none(),
            "an open conversation already carries its instructions"
        );

        let assigned = intent_for(&def(), None, Some("sess-1"), false, "first");
        assert_eq!(
            assigned.resume,
            Some(ResumeId::ClientAssigned("sess-1".into()))
        );
        assert!(
            assigned.instructions.is_some(),
            "the turn that opens a conversation carries the system prompt"
        );

        let fresh = intent_for(&def(), None, None, false, "first");
        assert!(fresh.resume.is_none());
        assert!(fresh.instructions.is_some());
    }

    /// The bug this migration fixes on the way past: a toolless agent
    /// was told it had no tools and then handed a command with no
    /// restriction on it. `None` means "inherit"; every definition here
    /// is an explicit grant, including the empty one.
    #[test]
    fn a_toolless_agent_sends_an_explicit_empty_grant_not_an_inherited_one() {
        let mut d = def();
        d.allowed_tools = Vec::new();
        let intent = intent_for(&d, None, None, false, "go");
        assert_eq!(
            intent.allowed_tools,
            Some(Vec::new()),
            "an explicit empty grant, never None"
        );

        d.allowed_tools = vec!["Read".into()];
        let intent = intent_for(&d, None, None, false, "go");
        assert_eq!(intent.allowed_tools, Some(vec!["Read".to_string()]));
    }

    #[test]
    fn unknown_sandbox_fails_closed_and_tilde_homes_expand() {
        let mut d = def().sandbox("typo").config_home("~/.ciacola-test-home");
        d.inherit_provider_tools = true;
        let intent = intent_for(&d, None, None, false, "go");
        assert_eq!(intent.sandbox, ciacola_agent::Sandbox::ReadOnly);
        assert_eq!(intent.allowed_tools, None);
        let expected = std::env::var("HOME")
            .map(|home| format!("{home}/.ciacola-test-home"))
            .unwrap_or_else(|_| "~/.ciacola-test-home".into());
        assert_eq!(intent.config_home.as_deref(), Some(expected.as_str()));
    }

    /// A capped run keeps its spend and its session, which is what makes
    /// it resumable and what keeps it visible to the spend limit. The
    /// recognition now happens in the adapter; this pins that core still
    /// records what comes back.
    #[test]
    fn a_failed_outcome_keeps_its_spend_and_session_in_the_ledger_shape() {
        let outcome = TurnOutcome {
            failure: Some(TurnFailure::limit("reached maximum number of turns (60)")),
            cost: Cost::Reported {
                micro_usd: 1_250_000,
            },
            usage: Usage::Reported(TokenUsage {
                input: 900,
                output: 12,
                cached_input: 0,
            }),
            resume: Some(ResumeId::ProviderAssigned("sess-1".into())),
            provider_turns: Some(60),
            elapsed: Duration::from_millis(323_000),
            ..TurnOutcome::ok("")
        };
        let x = exchange_from(&def(), outcome);
        assert_eq!(x.cost_micro_usd(), 1_250_000, "spend must survive");
        assert!(x.cost_complete, "terminal reported spend is authoritative");
        assert_eq!(
            x.session.as_deref(),
            Some("sess-1"),
            "session must survive, so it resumes"
        );
        assert_eq!(x.elapsed_ms, 323_000);
        assert_eq!(x.provider_turns, Some(60));
        assert_eq!(x.tokens().input, 900);
        assert_eq!(x.failure_kind, Some(ciacola_agent::FailureKind::Limit));
        assert!(
            x.error.is_some(),
            "it still failed, and must read as failed"
        );
    }

    /// A cancelled or timed-out run that already spent money is banked
    /// through the same path, rather than thrown away because it came
    /// back as `Err`.
    #[test]
    fn a_partial_failure_is_banked_rather_than_discarded() {
        let e = AgentError::Cancelled {
            provider: ProviderKey::claude(),
            partial: PartialTelemetry {
                resume: Some(ResumeId::ProviderAssigned("sess-mid".into())),
                cost: Some(Cost::Reported { micro_usd: 900_000 }),
                usage: Some(TokenUsage {
                    input: 40,
                    output: 2,
                    cached_input: 10,
                }),
                elapsed: Some(Duration::from_secs(1_200)),
            }
            .into(),
        };
        let mut capabilities = Capabilities::none(ProviderKey::claude());
        capabilities.reports_cost = true;
        capabilities.reports_token_usage = true;
        let x = partial_exchange(&e, &capabilities, Duration::from_secs(1))
            .expect("a run that spent money is an exchange");
        assert_eq!(x.cost_micro_usd(), 900_000);
        assert!(!x.cost_complete, "partial spend is only a lower bound");
        assert_eq!(x.session.as_deref(), Some("sess-mid"));
        assert_eq!(x.elapsed_ms, 1_200_000);
        assert_eq!(x.tokens().input, 40);
        assert!(!x.usage_complete, "partial buckets are only a lower bound");
        assert!(x.error.is_some());
    }

    /// A turn that genuinely did not happen has nothing to bank, and
    /// must keep propagating as an error rather than being downgraded
    /// into an empty exchange that merely looks like it failed.
    #[test]
    fn a_pre_launch_failure_is_not_treated_as_an_exchange() {
        let e = AgentError::NotFound {
            provider: ProviderKey::claude(),
            detail: "no binary".into(),
        };
        assert!(
            partial_exchange(
                &e,
                &Capabilities::none(ProviderKey::claude()),
                Duration::from_secs(1),
            )
            .is_none()
        );
    }

    /// The post-launch-capable variants remain attempts even when the
    /// adapter could not observe a session, price, or token count. Core
    /// still measured how long `Provider::run` was alive, and the
    /// provider's declared accounting gaps are not known zeros.
    #[test]
    fn an_empty_post_launch_failure_keeps_measured_time_and_unknown_accounting() {
        let empty = AgentError::Cancelled {
            provider: ProviderKey::claude(),
            partial: PartialTelemetry::none().into(),
        };
        let mut capabilities = Capabilities::none(ProviderKey::claude());
        capabilities.reports_cost = true;
        capabilities.reports_token_usage = true;
        let exchange = partial_exchange(&empty, &capabilities, Duration::from_millis(321))
            .expect("a possibly post-launch failure is an attempted exchange");
        assert_eq!(exchange.cost, Cost::Unreported);
        assert_eq!(exchange.usage, Usage::Unreported);
        assert_eq!(exchange.elapsed_ms, 321);
        assert!(exchange.error.is_some());
    }

    #[test]
    fn an_absent_resume_id_does_not_become_an_empty_session() {
        let exchange = exchange_from(&def(), TurnOutcome::ok("done"));
        assert_eq!(exchange.session, None);
    }
}
