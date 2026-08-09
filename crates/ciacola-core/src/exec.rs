//! The executor seam, and the spike-off it exists for.
//!
//! Everything above this line is identical in both systems: the ledger
//! admits one turn per agent, the executor claims it, runs the
//! exchange, records the outcome, and notifies. The only open question
//! is who *drives* the turn, and this trait is the whole of the
//! difference:
//!
//! - [`HandExecutor`] (here): a channel, a semaphore, a cancellation
//!   token per turn.
//! - `ciacola_apalis::ApalisExecutor`: push `{agent_id, seq}` to a
//!   queue and let a worker drive the same functions.
//!
//! `claim_turn` is what makes both safe: whoever delivers the work, and
//! however many times, the exchange runs at most once. Only the delivery
//! that wins the claim registers a kill token. It then rechecks the
//! durable state before touching the provider, which closes the small
//! claim-to-registration window in which a kill may already have settled
//! the row without finding a token to signal.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tower_mcp::LogLevel;

use crate::agent::run_exchange_with_ceiling;
use crate::ledger::Ledger;
use crate::notify::Notifier;

pub trait TurnExecutor: Send + Sync {
    /// The turn is already recorded as queued; arrange for it to run.
    fn submit(&self, agent_id: String, seq: i64);
    /// Best-effort stop of a running turn. True if something was
    /// signalled; the ledger record is the caller's job (and the caller
    /// must record BEFORE signalling, so a claim racing the signal
    /// finds the turn already settled).
    fn kill(&self, agent_id: &str, seq: i64) -> bool;
    /// For the server banner, so a client can see which system it is on.
    fn name(&self) -> &'static str;

    /// Stop accepting new turns and wait for in-flight ones, up to
    /// `grace`. Returns how many were still running when it gave up.
    ///
    /// Only the executor can do this, because only it knows what is in
    /// flight. Without it, Ctrl-C kills paid work mid-run: recovery
    /// cleans up on the next boot, but it cleans up by killing the
    /// provider process and marking the turn failed, so a twenty
    /// minute agent run is simply lost.
    fn drain(&self, _grace: Duration) -> crate::plugin::BoxFut<'_, usize> {
        Box::pin(async { 0 })
    }
}

/// Claim, then run. Both executors call exactly this.
#[tracing::instrument(skip_all, fields(agent = %agent_id, seq))]
pub async fn run_turn(ledger: &Ledger, notify: &Notifier, agent_id: &str, seq: i64) {
    match ledger.claim_turn(agent_id, seq).await {
        Ok(true) => run_claimed_turn(ledger, notify, agent_id, seq).await,
        // Already ran, is running, or was killed while queued: the claim
        // says no, so delivering the work twice runs it zero extra times.
        Ok(false) => {}
        Err(e) => eprintln!("[exec] claim {agent_id}/{seq}: {e}"),
    }
}

/// The post-claim half: run the exchange and record however it ends.
/// Callers must hold the claim. Every outcome path settles the turn, so
/// nothing can be left `running` by anything short of a crash.
#[tracing::instrument(skip_all, fields(agent = %agent_id, seq, outcome = tracing::field::Empty))]
pub async fn run_claimed_turn(ledger: &Ledger, notify: &Notifier, agent_id: &str, seq: i64) {
    let fail = |state: &'static str, error: String, exchange: crate::agent::Exchange| async move {
        let recorded = ledger
            .fail_exchange(agent_id, seq, state, &error, &exchange)
            .await;
        if let Err(e) = recorded {
            eprintln!("[exec] record failure {agent_id}/{seq}: {e}");
        }
        tracing::warn!(agent = %agent_id, seq, state, %error, "turn settled badly");
        notify.turn(LogLevel::Error, agent_id, seq, state, &error);
    };

    let (def, mcp, session, started, prompt, turn_ceiling) = match load(ledger, agent_id, seq).await
    {
        Ok(loaded) => loaded,
        Err(e) => {
            return abort_before_provider(ledger, notify, agent_id, seq, &e.to_string()).await;
        }
    };

    let events = SessionSink::new(ledger.clone(), agent_id.to_string(), seq);
    let result = run_exchange_with_ceiling(
        ledger.providers(),
        &def,
        mcp,
        session.as_deref(),
        started,
        &prompt,
        turn_ceiling,
        &events,
    )
    .await;
    // On a normal provider return, make every synchronously enqueued
    // usage observation durable before terminal settlement. If this
    // future is cancelled instead, dropping the sender closes the queue
    // and the detached writer drains it against the claimed killed row.
    events.drain().await;
    match result {
        Ok(exchange) => {
            if let Some(error) = &exchange.error {
                // The provider ran and failed. The spend and any session
                // it learned are real; record both.
                return fail("failed", error.clone(), exchange).await;
            }
            let detail: String = exchange.reply.chars().take(120).collect();
            match ledger.complete_turn(agent_id, seq, &exchange).await {
                Ok(true) => {
                    tracing::Span::current().record("outcome", "ok");
                    tracing::info!(agent = %agent_id, seq, "turn ok");
                    notify.turn(LogLevel::Info, agent_id, seq, "ok", &detail)
                }
                // Settled by someone else (a kill) while we ran; their
                // record stands and the "ok" would be a lie.
                Ok(false) => {}
                Err(e) => {
                    // A successful paid exchange that could not be
                    // recorded: do not leave it stuck running.
                    fail(
                        "failed",
                        format!("exchange succeeded but recording failed: {e}"),
                        exchange,
                    )
                    .await;
                }
            }
        }
        Err(e) => abort_before_provider(ledger, notify, agent_id, seq, &e.to_string()).await,
    }
}

/// Persists observations a backend reveals before its terminal outcome.
///
/// Ciacola assigns ids up front, so for Claude this usually confirms an
/// id already in the ledger and writes nothing new. It exists for the
/// case up-front assignment cannot cover: a backend that names its own
/// conversation tells us partway through a turn that may still have
/// twenty minutes to run, and a crash in those twenty minutes would
/// otherwise lose the id and make "send again" start over.
///
/// Usage snapshots have a tighter cancellation constraint: an async
/// callback could be dropped with the provider future before it writes.
/// The synchronous callback therefore only enqueues cumulative totals;
/// one detached writer owns SQLite ordering for this turn. Normal return
/// drains it explicitly, while cancellation drops the sender and leaves
/// the task to drain against the claimed killed row.
///
/// Neither observation ever fails a turn. Telemetry that cannot be
/// written is worth a log line, not an abandoned run already paid for.
struct SessionSink {
    ledger: Ledger,
    agent_id: String,
    seq: i64,
    usage_tx: mpsc::UnboundedSender<ciacola_agent::TokenUsage>,
    usage_writer: tokio::task::JoinHandle<()>,
}

impl SessionSink {
    fn new(ledger: Ledger, agent_id: String, seq: i64) -> Self {
        let (usage_tx, mut usage_rx) = mpsc::unbounded_channel();
        let writer_ledger = ledger.clone();
        let writer_agent = agent_id.clone();
        let usage_writer = tokio::spawn(async move {
            while let Some(usage) = usage_rx.recv().await {
                if let Err(e) = writer_ledger
                    .record_usage_snapshot(&writer_agent, seq, usage)
                    .await
                {
                    // Telemetry persistence is best effort and must never
                    // turn provider work into an application failure.
                    eprintln!("[exec] persist usage {writer_agent}/{seq}: {e}");
                }
            }
        });
        Self {
            ledger,
            agent_id,
            seq,
            usage_tx,
            usage_writer,
        }
    }

    async fn drain(self) {
        let Self {
            usage_tx,
            usage_writer,
            agent_id,
            seq,
            ..
        } = self;
        drop(usage_tx);
        if let Err(e) = usage_writer.await {
            eprintln!("[exec] usage writer {agent_id}/{seq}: {e}");
        }
    }
}

impl ciacola_agent::TurnEvents for SessionSink {
    fn resume_id<'a>(&'a self, id: &'a ciacola_agent::ResumeId) -> ciacola_agent::BoxFut<'a, ()> {
        Box::pin(async move {
            if let Err(e) = self
                .ledger
                .record_provider_session(&self.agent_id, self.seq, id.value())
                .await
            {
                eprintln!("[exec] persist session {}: {e}", self.agent_id);
            }
        })
    }

    fn usage_snapshot(&self, usage: ciacola_agent::TokenUsage) {
        if self.usage_tx.send(usage).is_err() {
            eprintln!(
                "[exec] usage writer closed before snapshot {}/{}",
                self.agent_id, self.seq
            );
        }
    }
}

/// The preamble a rotated agent gets in place of its lost transcript.
/// Deliberately short: it says where the durable state is, and the
/// role's system prompt (re-sent with the fresh session) says the rest.
fn rotation_preamble(name: &str, turns: i64) -> String {
    format!(
        "[Session rotated. You are {name}, continuing work that began before \
         this conversation: the previous session ran {turns} turns and has \
         been closed to keep your context bounded. Nothing is lost that \
         matters. Your durable state is in the system, not in memory of this \
         chat: read your work items (items), server memory (recall), and open \
         findings before acting, exactly as you would at the start of any \
         wake.]\n\n"
    )
}

/// The agent's MCP endpoints, with its token on ciacola's own entry.
///
/// The base file names the endpoints granted to this agent and is shared;
/// the token is per agent and secret. Ciacola's internal file names only the
/// ordinary agent mount. This reads the shared file and returns *intent*: a
/// list of endpoints the backend materialises however its own CLI wants them.
///
/// That is the fix for a real weakness, not a refactor. Core used to
/// write the merged config itself, to the predictable path
/// `$TMPDIR/ciacola-agent-<id>.json`, with `std::fs::write` and
/// therefore mode 0644. On systems with a shared temporary directory
/// that exposed the bearer to other local users; macOS's per-user temp
/// directory reduced the exposure but did not make the file mode or
/// predictable lifetime appropriate. The Claude adapter writes it
/// through `tempfile` instead: randomized name, mode 0600, removed when
/// the turn ends.
///
/// Failing loudly is deliberate and unchanged: a base config that cannot
/// be parsed is an operator error, and a turn that ran anonymously
/// *instead* would be the kind of quiet downgrade this codebase keeps
/// paying for.
fn token_scoped_mcp_scope(
    token: &str,
    base_path: &str,
) -> Result<ciacola_agent::McpScope, crate::agent::FlatError> {
    let raw =
        std::fs::read_to_string(base_path).map_err(|e| format!("mcp config {base_path}: {e}"))?;
    let config: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("mcp config {base_path}: {e}"))?;
    let Some(servers) = config.get("mcpServers").and_then(|s| s.as_object()) else {
        return Err(format!("mcp config {base_path}: no mcpServers object").into());
    };

    let mut endpoints = Vec::new();
    for (name, server) in servers {
        let server = server
            .as_object()
            .ok_or_else(|| format!("mcp config {base_path}: '{name}' is not an object"))?;
        // Only loopback HTTP has ever been emitted here, and the
        // contract carries only that. A stdio or otherwise-shaped entry
        // is refused rather than silently dropped: dropping it would
        // hand the agent a config missing a server it was granted.
        if let Some(kind) = server.get("type") {
            if kind.as_str() != Some("http") {
                return Err(format!(
                    "mcp config {base_path}: '{name}' has unsupported type {kind}; \
                     only http servers are supported"
                )
                .into());
            }
        }
        let url = server.get("url").and_then(|u| u.as_str()).ok_or_else(|| {
            format!("mcp config {base_path}: '{name}' has no url; only http servers are supported")
        })?;
        let mut headers = std::collections::BTreeMap::new();
        if let Some(raw_headers) = server.get("headers") {
            let raw_headers = raw_headers.as_object().ok_or_else(|| {
                format!("mcp config {base_path}: '{name}' headers is not an object")
            })?;
            for (key, value) in raw_headers {
                let value = value.as_str().ok_or_else(|| {
                    format!("mcp config {base_path}: '{name}' header '{key}' is not a string")
                })?;
                headers.insert(key.clone(), value.to_string());
            }
        }
        if name == "ciacola" {
            headers.insert(crate::identity::TOKEN_HEADER.into(), token.into());
        }
        endpoints.push(ciacola_agent::McpEndpoint {
            name: name.clone(),
            url: url.to_string(),
            headers,
        });
    }

    // Strict, as before: the endpoint an agent was handed is the whole
    // of its authority, and a non-exclusive list is not that list.
    Ok(ciacola_agent::McpScope {
        endpoints,
        strict: true,
    })
}

async fn load(
    ledger: &Ledger,
    agent_id: &str,
    seq: i64,
) -> Result<
    (
        crate::agent::AgentDef,
        Option<ciacola_agent::McpScope>,
        Option<String>,
        bool,
        String,
        Option<ciacola_agent::TurnCeiling>,
    ),
    crate::agent::FlatError,
> {
    let agent = ledger
        .get_agent(agent_id)
        .await?
        .ok_or_else(|| format!("no agent '{agent_id}'"))?;
    let turn = ledger
        .get_turn(agent_id, seq)
        .await?
        .ok_or_else(|| format!("no turn {agent_id}/{seq}"))?;

    if turn.provider != agent.def.provider.as_str() {
        return Err(format!(
            "turn provider '{}' no longer matches agent provider '{}'; resend the turn so current policy can be admitted and persisted",
            turn.provider, agent.def.provider
        )
        .into());
    }
    if turn.turn_protection_state == "legacy" {
        return Err(
            "legacy queued turn has no enforceable per-turn protection provenance; resend the turn so current policy can be admitted and persisted"
                .into(),
        );
    }
    let raw_protection = turn.turn_protection.as_deref().ok_or(
        "queued turn is missing its per-turn protection snapshot; resend the turn so current policy can be admitted and persisted",
    )?;
    let protection: crate::limits::TurnProtectionSnapshot = serde_json::from_str(raw_protection)
        .map_err(|error| {
            format!(
                "queued turn has a corrupt per-turn protection snapshot ({error}); resend the turn so current policy can be admitted and persisted"
            )
        })?;
    let live_capability = if protection.state == crate::limits::TurnProtectionState::Unbounded {
        None
    } else {
        ledger
            .providers()
            .get(&agent.def.provider)?
            .capabilities()
            .turn_ceiling
            .clone()
    };
    let turn_ceiling = protection
        .validate_for_execution(
            &turn.turn_protection_state,
            &turn.provider,
            live_capability.as_ref(),
        )
        .map_err(|error| -> crate::agent::FlatError { error.into() })?;

    // Rotate when this turn would exceed the policy for the current
    // session. Dropping the session is the whole mechanism; the
    // provider keeps the old transcript, we simply stop resuming it.
    // An id is assigned at birth, so `session.is_some()` no longer
    // means "has run". `session_started_seq` does: 0 until the turn
    // that opens the session records itself.
    let started = agent.session_started_seq > 0;
    let in_session = seq - agent.session_started_seq;
    let rotating = agent
        .def
        .rotate_after_turns
        .is_some_and(|limit| started && in_session > limit as i64);

    // Identity rides in the config, so the config becomes per agent
    // the moment the agent has a token. Agents that predate tokens are
    // backfilled at boot; a missing one here means only that the call
    // will arrive anonymous, which is survivable, unlike failing the
    // turn of an agent that worked yesterday.
    let mut mcp = None;
    if let Some(base) = agent.def.mcp_config.clone() {
        if let Some(token) = ledger.token_of(agent_id).await? {
            mcp = Some(token_scoped_mcp_scope(&token, &base)?);
        }
    }

    if rotating {
        eprintln!("[exec] rotating {agent_id} after {in_session} turns in session");
        // Persisted before the provider runs, for the same reason the
        // first one is: a rotation turn that dies mid-flight would
        // otherwise lose the id it was about to open.
        let next = crate::ledger::new_session_id();
        ledger.assign_session(agent_id, &next).await?;
        let preamble = rotation_preamble(&agent.def.name, in_session);
        return Ok((
            agent.def,
            mcp,
            Some(next),
            false,
            format!("{preamble}{}", turn.prompt),
            turn_ceiling,
        ));
    }
    Ok((
        agent.def,
        mcp,
        agent.session,
        started,
        turn.prompt,
        turn_ceiling,
    ))
}

pub(crate) type Kills = Arc<Mutex<HashMap<(String, i64), CancellationToken>>>;

/// A kill token installed for the one delivery that owns the claim.
/// Removing it in `Drop` keeps every early return on the safe path.
struct KillRegistration {
    kills: Kills,
    key: (String, i64),
    token: CancellationToken,
}

impl KillRegistration {
    fn install(kills: Kills, agent_id: &str, seq: i64) -> Option<Self> {
        let key = (agent_id.to_string(), seq);
        let token = CancellationToken::new();
        kills.lock().ok()?.insert(key.clone(), token.clone());
        Some(Self { kills, key, token })
    }
}

impl Drop for KillRegistration {
    fn drop(&mut self) {
        if let Ok(mut map) = self.kills.lock() {
            map.remove(&self.key);
        }
    }
}

/// Run a claimed turn behind its registered kill switch.
///
/// Registration happens before the durable-state read. If an operator
/// settled the turn between claim and registration, the read observes
/// that terminal state and the provider is never polled. If the kill
/// lands after registration, it can signal `token`; cancellation is
/// deliberately polled first so a ready token cannot lose a select race
/// and launch the provider anyway.
pub(crate) async fn run_claimed_turn_cancellable(
    ledger: &Ledger,
    notify: &Notifier,
    kills: Kills,
    agent_id: &str,
    seq: i64,
) {
    let Some(registration) = KillRegistration::install(kills, agent_id, seq) else {
        abort_before_provider(
            ledger,
            notify,
            agent_id,
            seq,
            "kill-token registry lock poisoned before provider launch",
        )
        .await;
        return;
    };

    match ledger.get_turn(agent_id, seq).await {
        Ok(Some(turn)) if turn.state == "running" => {}
        // A kill in the claim-to-registration gap already settled the
        // durable row. Its record wins, and no provider work may start.
        Ok(_) => return,
        Err(e) => {
            abort_before_provider(
                ledger,
                notify,
                agent_id,
                seq,
                &format!("could not validate turn state before provider launch: {e}"),
            )
            .await;
            return;
        }
    }

    run_registered_claimed_turn(ledger, notify, agent_id, seq, &registration).await;
}

async fn abort_before_provider(
    ledger: &Ledger,
    notify: &Notifier,
    agent_id: &str,
    seq: i64,
    error: &str,
) {
    match ledger.abort_claimed_turn(agent_id, seq, error).await {
        Ok(true) => {
            tracing::warn!(agent = %agent_id, seq, %error, "dispatch aborted before provider launch");
            notify.turn(LogLevel::Error, agent_id, seq, "failed", error);
        }
        Ok(false) => {}
        Err(settle) => {
            eprintln!(
                "[exec] abort before provider {agent_id}/{seq}: {error}; settlement failed: {settle}"
            );
        }
    }
}

async fn run_registered_claimed_turn(
    ledger: &Ledger,
    notify: &Notifier,
    agent_id: &str,
    seq: i64,
    registration: &KillRegistration,
) {
    tokio::select! {
        biased;
        () = registration.token.cancelled() => {}
        () = run_claimed_turn(ledger, notify, agent_id, seq) => {}
    }
}

/// The default executor: everything a work queue would do for us,
/// written out. A channel is the queue, a semaphore is the
/// concurrency limit, a cancellation token is the kill switch. Dropping
/// the turn's future kills the provider's process group, which is the
/// adapter's guarantee rather than this file's: core drops the future
/// and the backend owns what that costs its children. The Claude
/// adapter documents how it holds up its end.
pub struct HandExecutor {
    tx: mpsc::UnboundedSender<(String, i64)>,
    kills: Kills,
    /// Set on drain: the dispatcher stops claiming new turns, but
    /// anything already claimed runs to completion.
    stopping: Arc<AtomicBool>,
    inflight: Arc<AtomicUsize>,
}

impl HandExecutor {
    pub fn start(ledger: Ledger, notify: Notifier, concurrency: usize) -> Arc<Self> {
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, i64)>();
        let kills: Kills = Arc::new(Mutex::new(HashMap::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let inflight = Arc::new(AtomicUsize::new(0));
        let this = Arc::new(Self {
            tx,
            kills: kills.clone(),
            stopping: stopping.clone(),
            inflight: inflight.clone(),
        });

        tokio::spawn(async move {
            let limit = Arc::new(Semaphore::new(concurrency));
            while let Some((agent_id, seq)) = rx.recv().await {
                // The permit is acquired inside the task, so the
                // dispatcher never parks: a full pool delays turns, it
                // does not stop deliveries from being accepted.
                let limit = limit.clone();
                let ledger = ledger.clone();
                let notify = notify.clone();
                let kills = kills.clone();
                let stopping = stopping.clone();
                let inflight = inflight.clone();
                tokio::spawn(async move {
                    let Ok(_permit) = limit.acquire_owned().await else {
                        return;
                    };
                    // Checked after the permit, not before: a turn that
                    // waited behind a full pool while shutdown began
                    // should stay queued for the next boot rather than
                    // start a paid run nobody will collect.
                    if stopping.load(Ordering::SeqCst) {
                        return;
                    }
                    // Claim before registering the kill token: only the
                    // winning delivery owns the kill entry.
                    match ledger.claim_turn(&agent_id, seq).await {
                        Ok(true) => {}
                        Ok(false) => return,
                        Err(e) => {
                            eprintln!("[exec] claim {agent_id}/{seq}: {e}");
                            return;
                        }
                    }
                    inflight.fetch_add(1, Ordering::SeqCst);
                    run_claimed_turn_cancellable(&ledger, &notify, kills, &agent_id, seq).await;
                    inflight.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        this
    }
}

impl TurnExecutor for HandExecutor {
    fn submit(&self, agent_id: String, seq: i64) {
        let _ = self.tx.send((agent_id, seq));
    }

    fn kill(&self, agent_id: &str, seq: i64) -> bool {
        let Ok(map) = self.kills.lock() else {
            return false;
        };
        match map.get(&(agent_id.to_string(), seq)) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    fn name(&self) -> &'static str {
        "hand-rolled"
    }

    fn drain(&self, grace: Duration) -> crate::plugin::BoxFut<'_, usize> {
        Box::pin(async move {
            self.stopping.store(true, Ordering::SeqCst);
            let deadline = tokio::time::Instant::now() + grace;
            loop {
                let left = self.inflight.load(Ordering::SeqCst);
                if left == 0 || tokio::time::Instant::now() >= deadline {
                    return left;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
    }
}

#[cfg(test)]
mod config_injection_tests {
    use super::*;

    struct FakeProvider {
        seen: Arc<Mutex<Option<ciacola_agent::TurnIntent>>>,
        turn_ceiling: Option<ciacola_agent::CeilingCapability>,
    }

    impl ciacola_agent::Provider for FakeProvider {
        fn key(&self) -> ciacola_agent::ProviderKey {
            ciacola_agent::ProviderKey::new("fake")
        }

        fn capabilities(&self) -> ciacola_agent::Capabilities {
            let mut capabilities =
                ciacola_agent::Capabilities::none(ciacola_agent::ProviderKey::new("fake"));
            capabilities.client_assigned_resume = true;
            capabilities.allowed_tools = true;
            capabilities.reports_cost = true;
            capabilities.reports_token_usage = true;
            capabilities.reports_provider_turns = true;
            capabilities.turn_ceiling = self.turn_ceiling.clone();
            capabilities
        }

        fn run<'a>(
            &'a self,
            intent: &'a ciacola_agent::TurnIntent,
            events: &'a dyn ciacola_agent::TurnEvents,
        ) -> ciacola_agent::BoxFut<'a, Result<ciacola_agent::TurnOutcome, ciacola_agent::AgentError>>
        {
            *self.seen.lock().expect("seen lock") = Some(intent.clone());
            Box::pin(async move {
                let resume = ciacola_agent::ResumeId::ProviderAssigned("fake-session".to_string());
                events.resume_id(&resume).await;
                Ok(ciacola_agent::TurnOutcome {
                    reply: "provider reply".to_string(),
                    resume: Some(resume),
                    cost: ciacola_agent::Cost::Reported { micro_usd: 321 },
                    usage: ciacola_agent::Usage::Reported(ciacola_agent::TokenUsage {
                        input: 12,
                        output: 3,
                        cached_input: 7,
                    }),
                    provider_turns: Some(4),
                    elapsed: Duration::from_millis(25),
                    metadata: Default::default(),
                    failure: None,
                })
            })
        }

        fn owns_process(&self, ps_line: &str) -> bool {
            ps_line.contains("fake-provider")
        }
    }

    struct NoAttemptProvider;

    impl ciacola_agent::Provider for NoAttemptProvider {
        fn key(&self) -> ciacola_agent::ProviderKey {
            ciacola_agent::ProviderKey::new("no-attempt")
        }

        fn capabilities(&self) -> ciacola_agent::Capabilities {
            let mut capabilities =
                ciacola_agent::Capabilities::none(ciacola_agent::ProviderKey::new("no-attempt"));
            capabilities.client_assigned_resume = true;
            capabilities.allowed_tools = true;
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
            Box::pin(async {
                Err(ciacola_agent::AgentError::NotFound {
                    provider: ciacola_agent::ProviderKey::new("no-attempt"),
                    detail: "provider binary missing".into(),
                })
            })
        }

        fn owns_process(&self, _ps_line: &str) -> bool {
            false
        }
    }

    struct EmptyPostLaunchProvider;

    impl ciacola_agent::Provider for EmptyPostLaunchProvider {
        fn key(&self) -> ciacola_agent::ProviderKey {
            ciacola_agent::ProviderKey::new("empty-post-launch")
        }

        fn capabilities(&self) -> ciacola_agent::Capabilities {
            let mut capabilities = ciacola_agent::Capabilities::none(
                ciacola_agent::ProviderKey::new("empty-post-launch"),
            );
            capabilities.client_assigned_resume = true;
            capabilities.allowed_tools = true;
            capabilities.reports_cost = true;
            capabilities.reports_token_usage = true;
            capabilities
        }

        fn run<'a>(
            &'a self,
            _intent: &'a ciacola_agent::TurnIntent,
            events: &'a dyn ciacola_agent::TurnEvents,
        ) -> ciacola_agent::BoxFut<'a, Result<ciacola_agent::TurnOutcome, ciacola_agent::AgentError>>
        {
            Box::pin(async move {
                events.usage_snapshot(ciacola_agent::TokenUsage {
                    input: 17,
                    output: 3,
                    cached_input: 5,
                });
                tokio::time::sleep(Duration::from_millis(5)).await;
                Err(ciacola_agent::AgentError::Other {
                    provider: ciacola_agent::ProviderKey::new("empty-post-launch"),
                    detail: "provider output ended unexpectedly".into(),
                    partial: ciacola_agent::PartialTelemetry::none().into(),
                })
            })
        }

        fn owns_process(&self, _ps_line: &str) -> bool {
            false
        }
    }

    struct SnapshotBlockingProvider {
        started: Arc<tokio::sync::Notify>,
    }

    impl ciacola_agent::Provider for SnapshotBlockingProvider {
        fn key(&self) -> ciacola_agent::ProviderKey {
            ciacola_agent::ProviderKey::new("snapshot-blocking")
        }

        fn capabilities(&self) -> ciacola_agent::Capabilities {
            let mut capabilities = ciacola_agent::Capabilities::none(
                ciacola_agent::ProviderKey::new("snapshot-blocking"),
            );
            capabilities.client_assigned_resume = true;
            capabilities.allowed_tools = true;
            capabilities.reports_cost = true;
            capabilities.reports_token_usage = true;
            capabilities
        }

        fn run<'a>(
            &'a self,
            _intent: &'a ciacola_agent::TurnIntent,
            events: &'a dyn ciacola_agent::TurnEvents,
        ) -> ciacola_agent::BoxFut<'a, Result<ciacola_agent::TurnOutcome, ciacola_agent::AgentError>>
        {
            Box::pin(async move {
                events.usage_snapshot(ciacola_agent::TokenUsage {
                    input: 89,
                    output: 13,
                    cached_input: 34,
                });
                self.started.notify_one();
                std::future::pending::<()>().await;
                unreachable!("blocking provider only ends by cancellation")
            })
        }

        fn owns_process(&self, _ps_line: &str) -> bool {
            false
        }
    }

    fn endpoint<'a>(
        scope: &'a ciacola_agent::McpScope,
        name: &str,
    ) -> &'a ciacola_agent::McpEndpoint {
        scope
            .endpoints
            .iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("no endpoint '{name}'"))
    }

    fn fake_ceiling(meter: &str) -> ciacola_agent::CeilingCapability {
        ciacola_agent::CeilingCapability {
            meter: ciacola_agent::MeterId::new(meter),
            granularity: ciacola_agent::EnforcementGranularity::ProviderResponseBoundary,
            cache_treatment: ciacola_agent::CacheTreatment::Included,
        }
    }

    fn fake_registry(
        seen: Arc<Mutex<Option<ciacola_agent::TurnIntent>>>,
        meter: &str,
    ) -> ciacola_agent::ProviderRegistry {
        ciacola_agent::ProviderRegistry::new()
            .with(Arc::new(FakeProvider {
                seen,
                turn_ceiling: Some(fake_ceiling(meter)),
            }))
            .expect("unique provider")
    }

    fn fake_limits(limit: u64) -> crate::limits::Limits {
        crate::limits::Limits {
            providers: [(
                "fake".into(),
                crate::limits::ProviderLimits {
                    per_turn_ceiling: Some(limit),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        }
    }

    async fn claimed_fake_turn(
        seen: Arc<Mutex<Option<ciacola_agent::TurnIntent>>>,
    ) -> (Ledger, String, i64, Notifier) {
        let providers = ciacola_agent::ProviderRegistry::new()
            .with(Arc::new(FakeProvider {
                seen,
                turn_ceiling: Some(fake_ceiling("fake_units_v1")),
            }))
            .expect("unique provider");
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool)
            .await
            .expect("ledger")
            .with_providers(providers);
        let agent_id = ledger
            .create_agent(
                &crate::agent::AgentDef::new("a", "system").provider("fake"),
                None,
            )
            .await
            .expect("agent");
        let seq = ledger.enqueue_turn(&agent_id, "do it").await.expect("turn");
        assert!(ledger.claim_turn(&agent_id, seq).await.expect("claim"));
        let (tx, _rx) = tower_mcp::context::notification_channel(8);
        (ledger, agent_id, seq, Notifier(tx))
    }

    /// The integration proof for the runtime seam: selection comes out
    /// of the persisted definition, the registry resolves it, the
    /// provider receives neutral intent and emits a session event, and
    /// the complete portable outcome lands back in the ledger.
    #[tokio::test]
    async fn a_claimed_turn_runs_through_the_selected_provider_and_settles() {
        let seen = Arc::new(Mutex::new(None));
        let providers = ciacola_agent::ProviderRegistry::new()
            .with(Arc::new(FakeProvider {
                seen: seen.clone(),
                turn_ceiling: Some(fake_ceiling("fake_units_v1")),
            }))
            .expect("unique provider");
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool)
            .await
            .expect("ledger")
            .with_providers(providers);
        let agent_id = ledger
            .create_agent(
                &crate::agent::AgentDef::new("a", "system").provider("fake"),
                None,
            )
            .await
            .expect("agent");
        let assigned = ledger
            .get_agent(&agent_id)
            .await
            .expect("get")
            .expect("agent row")
            .session
            .expect("preassigned session");
        let seq = ledger.enqueue_turn(&agent_id, "do it").await.expect("turn");
        let (tx, _rx) = tower_mcp::context::notification_channel(8);

        run_turn(&ledger, &Notifier(tx), &agent_id, seq).await;

        let intent = seen.lock().expect("seen lock").clone().expect("ran");
        assert_eq!(intent.prompt, "do it");
        assert_eq!(intent.allowed_tools, Some(Vec::new()));
        assert_eq!(
            intent.resume,
            Some(ciacola_agent::ResumeId::ClientAssigned(assigned))
        );

        let turn = ledger
            .get_turn(&agent_id, seq)
            .await
            .expect("turn query")
            .expect("turn row");
        assert_eq!(turn.state, "ok");
        assert_eq!(turn.reply.as_deref(), Some("provider reply"));
        assert_eq!(
            (turn.cost_micro_usd, turn.cost_state.as_str()),
            (321, "reported")
        );
        assert_eq!(
            (
                turn.tokens_in,
                turn.tokens_out,
                turn.tokens_cached,
                turn.usage_state.as_str(),
                turn.provider_turns,
            ),
            (12, 3, 7, "reported", Some(4))
        );
        let agent = ledger
            .get_agent(&agent_id)
            .await
            .expect("agent query")
            .expect("agent row");
        assert_eq!(agent.session.as_deref(), Some("fake-session"));
        assert_eq!(agent.session_started_seq, seq);
        assert_eq!(agent.cost_micro_usd, 321);
        assert_eq!(turn.failure_kind, "none");
        assert_eq!(turn.provider_session.as_deref(), Some("fake-session"));
    }

    #[tokio::test]
    async fn persisted_ceiling_survives_restart_and_is_applied_to_open_and_resume() {
        let seen = Arc::new(Mutex::new(None));
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool.clone())
            .await
            .expect("ledger")
            .with_providers(fake_registry(seen.clone(), "fake_units_v1"));
        let agent_id = ledger
            .create_agent(
                &crate::agent::AgentDef::new("a", "system").provider("fake"),
                None,
            )
            .await
            .expect("agent");
        let first = match ledger
            .admit_turn(
                &fake_limits(77),
                crate::admission::AdmissionAuthority::Automatic,
                &agent_id,
                "first",
                "test",
            )
            .await
            .expect("admission")
        {
            crate::admission::AdmissionDecision::Admitted { seq, .. } => seq,
            decision => panic!("unexpected admission: {decision:?}"),
        };

        // Reopening with the same semantic adapter is enough to prove the
        // executor reads the row rather than an in-memory admission object.
        let reopened = Ledger::setup(pool.clone())
            .await
            .expect("reopen")
            .with_providers(fake_registry(seen.clone(), "fake_units_v1"));
        let (tx, _rx) = tower_mcp::context::notification_channel(8);
        run_turn(&reopened, &Notifier(tx), &agent_id, first).await;
        let opening = seen.lock().expect("seen").take().expect("opening intent");
        assert_eq!(opening.turn_ceiling.as_ref().map(|c| c.limit), Some(77));
        assert!(matches!(
            opening.resume,
            Some(ciacola_agent::ResumeId::ClientAssigned(_))
        ));

        let second = match reopened
            .admit_turn(
                &fake_limits(33),
                crate::admission::AdmissionAuthority::Automatic,
                &agent_id,
                "second",
                "test",
            )
            .await
            .expect("admission")
        {
            crate::admission::AdmissionDecision::Admitted { seq, .. } => seq,
            decision => panic!("unexpected admission: {decision:?}"),
        };
        let (tx, _rx) = tower_mcp::context::notification_channel(8);
        run_turn(&reopened, &Notifier(tx), &agent_id, second).await;
        let resumed = seen.lock().expect("seen").take().expect("resume intent");
        assert_eq!(resumed.turn_ceiling.as_ref().map(|c| c.limit), Some(33));
        assert!(matches!(
            resumed.resume,
            Some(ciacola_agent::ResumeId::ProviderAssigned(_))
        ));
    }

    #[tokio::test]
    async fn capability_drift_fails_known_zero_without_calling_the_provider() {
        let seen = Arc::new(Mutex::new(None));
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let admitting = Ledger::setup(pool.clone())
            .await
            .expect("ledger")
            .with_providers(fake_registry(seen.clone(), "fake_units_v1"));
        let agent_id = admitting
            .create_agent(
                &crate::agent::AgentDef::new("a", "system").provider("fake"),
                None,
            )
            .await
            .expect("agent");
        let seq = match admitting
            .admit_turn(
                &fake_limits(55),
                crate::admission::AdmissionAuthority::Automatic,
                &agent_id,
                "work",
                "test",
            )
            .await
            .expect("admission")
        {
            crate::admission::AdmissionDecision::Admitted { seq, .. } => seq,
            decision => panic!("unexpected admission: {decision:?}"),
        };
        let drifted = Ledger::setup(pool)
            .await
            .expect("reopen")
            .with_providers(fake_registry(seen.clone(), "fake_units_v2"));
        let (tx, _rx) = tower_mcp::context::notification_channel(8);
        run_turn(&drifted, &Notifier(tx), &agent_id, seq).await;

        assert!(seen.lock().expect("seen").is_none());
        let turn = drifted.get_turn(&agent_id, seq).await.unwrap().unwrap();
        assert_eq!(turn.state, "failed");
        assert_eq!(turn.failure_kind, "not_attempted");
        assert_eq!(turn.elapsed_state, "not_attempted");
        assert_eq!(turn.reported_cost_micro_usd(), Some(0));
        assert_eq!(turn.reported_tokens(), Some((0, 0, 0)));
        assert!(turn.error.as_deref().is_some_and(|e| e.contains("resend")));
    }

    #[tokio::test]
    async fn legacy_and_corrupt_protection_rows_never_call_the_provider() {
        for corrupt in [false, true] {
            let seen = Arc::new(Mutex::new(None));
            let pool = sqlx::SqlitePool::connect("sqlite::memory:")
                .await
                .expect("pool");
            let ledger = Ledger::setup(pool)
                .await
                .expect("ledger")
                .with_providers(fake_registry(seen.clone(), "fake_units_v1"));
            let agent_id = ledger
                .create_agent(
                    &crate::agent::AgentDef::new("a", "system").provider("fake"),
                    None,
                )
                .await
                .expect("agent");
            let seq = ledger.enqueue_turn(&agent_id, "work").await.expect("turn");
            if corrupt {
                sqlx::query(
                    "UPDATE turns SET turn_protection_state = 'enforced', turn_protection = '{broken' WHERE agent_id = ?1 AND seq = ?2",
                )
                .bind(&agent_id)
                .bind(seq)
                .execute(ledger.pool())
                .await
                .expect("corrupt");
            } else {
                sqlx::query(
                    "UPDATE turns SET turn_protection_state = 'legacy', turn_protection = NULL WHERE agent_id = ?1 AND seq = ?2",
                )
                .bind(&agent_id)
                .bind(seq)
                .execute(ledger.pool())
                .await
                .expect("legacy");
            }
            let (tx, _rx) = tower_mcp::context::notification_channel(8);
            run_turn(&ledger, &Notifier(tx), &agent_id, seq).await;
            assert!(seen.lock().expect("seen").is_none());
            let turn = ledger.get_turn(&agent_id, seq).await.unwrap().unwrap();
            assert_eq!(turn.failure_kind, "not_attempted");
            assert_eq!(turn.elapsed_state, "not_attempted");
            assert!(turn.error.as_deref().is_some_and(|e| e.contains("resend")));
        }
    }

    /// HandExecutor and PollingExecutor both enter the provider through
    /// `run_claimed_turn_cancellable`. This reproduces their old gap
    /// deterministically: claim first, settle the kill while no token is
    /// registered, then resume dispatch. The durable recheck must stop
    /// before `Provider::run` is even called.
    #[tokio::test]
    async fn a_kill_in_the_registration_gap_never_launches_the_provider() {
        let seen = Arc::new(Mutex::new(None));
        let (ledger, agent_id, seq, notify) = claimed_fake_turn(seen.clone()).await;
        assert!(
            ledger
                .interrupt_turn(&agent_id, seq, "killed", "killed in registration gap")
                .await
                .expect("kill")
        );

        let kills: Kills = Arc::new(Mutex::new(HashMap::new()));
        run_claimed_turn_cancellable(&ledger, &notify, kills.clone(), &agent_id, seq).await;

        assert!(seen.lock().expect("seen lock").is_none());
        assert!(kills.lock().expect("kills lock").is_empty());
        assert_eq!(
            ledger
                .get_turn(&agent_id, seq)
                .await
                .expect("turn query")
                .expect("turn")
                .state,
            "killed"
        );
    }

    /// A token can become ready after the durable recheck and before
    /// `select!` starts polling. Cancellation is the biased first branch,
    /// so even that exact ordering cannot poll the provider future once.
    #[tokio::test]
    async fn a_pre_cancelled_registration_never_polls_the_provider() {
        let seen = Arc::new(Mutex::new(None));
        let (ledger, agent_id, seq, notify) = claimed_fake_turn(seen.clone()).await;
        let kills: Kills = Arc::new(Mutex::new(HashMap::new()));
        let registration = KillRegistration::install(kills, &agent_id, seq).expect("register");
        registration.token.cancel();

        run_registered_claimed_turn(&ledger, &notify, &agent_id, seq, &registration).await;

        assert!(seen.lock().expect("seen lock").is_none());
    }

    #[tokio::test]
    async fn a_broken_kill_registry_fails_as_a_known_non_attempt() {
        let seen = Arc::new(Mutex::new(None));
        let (ledger, agent_id, seq, notify) = claimed_fake_turn(seen.clone()).await;
        let kills: Kills = Arc::new(Mutex::new(HashMap::new()));
        let poison = kills.clone();
        std::thread::spawn(move || {
            let _guard = poison.lock().expect("initial lock");
            panic!("poison registry for the regression");
        })
        .join()
        .expect_err("thread must poison the registry");

        run_claimed_turn_cancellable(&ledger, &notify, kills, &agent_id, seq).await;

        assert!(seen.lock().expect("seen lock").is_none());
        let turn = ledger
            .get_turn(&agent_id, seq)
            .await
            .expect("turn query")
            .expect("turn");
        assert_eq!(turn.state, "failed");
        assert_eq!(turn.elapsed_state, "not_attempted");
        assert_eq!(turn.reported_cost_micro_usd(), Some(0));
        assert_eq!(turn.reported_tokens(), Some((0, 0, 0)));
    }

    #[tokio::test]
    async fn a_provider_error_without_partial_telemetry_is_a_known_non_attempt() {
        let providers = ciacola_agent::ProviderRegistry::new()
            .with(Arc::new(NoAttemptProvider))
            .expect("provider");
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool)
            .await
            .expect("ledger")
            .with_providers(providers);
        let agent_id = ledger
            .create_agent(
                &crate::agent::AgentDef::new("a", "system").provider("no-attempt"),
                None,
            )
            .await
            .expect("agent");
        let seq = ledger.enqueue_turn(&agent_id, "do it").await.expect("turn");
        let (tx, _rx) = tower_mcp::context::notification_channel(8);

        run_turn(&ledger, &Notifier(tx), &agent_id, seq).await;

        let turn = ledger
            .get_turn(&agent_id, seq)
            .await
            .expect("turn query")
            .expect("turn");
        assert_eq!(turn.state, "failed");
        assert_eq!(turn.elapsed_state, "not_attempted");
        assert_eq!(turn.reported_cost_micro_usd(), Some(0));
        assert_eq!(turn.reported_tokens(), Some((0, 0, 0)));
        assert_eq!(turn.provider_turns, Some(0));
    }

    #[tokio::test]
    async fn an_empty_post_launch_error_remains_an_attempt_with_unknown_accounting() {
        let providers = ciacola_agent::ProviderRegistry::new()
            .with(Arc::new(EmptyPostLaunchProvider))
            .expect("provider");
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool)
            .await
            .expect("ledger")
            .with_providers(providers);
        let agent_id = ledger
            .create_agent(
                &crate::agent::AgentDef::new("a", "system").provider("empty-post-launch"),
                None,
            )
            .await
            .expect("agent");
        let seq = ledger.enqueue_turn(&agent_id, "do it").await.expect("turn");
        let (tx, _rx) = tower_mcp::context::notification_channel(8);

        run_turn(&ledger, &Notifier(tx), &agent_id, seq).await;

        let turn = ledger
            .get_turn(&agent_id, seq)
            .await
            .expect("turn query")
            .expect("turn");
        assert_eq!(turn.state, "failed");
        assert_eq!(turn.elapsed_state, "measured");
        assert!(turn.elapsed_ms >= 1, "elapsed was {}", turn.elapsed_ms);
        assert_eq!(turn.cost_state, "unreported");
        assert_eq!(turn.usage_state, "reported");
        assert_eq!(turn.reported_cost_micro_usd(), None);
        assert_eq!(turn.reported_tokens(), Some((17, 3, 5)));
    }

    /// Exercises the real hand executor ordering, including dropping the
    /// provider future. The snapshot is synchronously queued immediately
    /// before the provider blocks; kill may beat or lose the SQLite writer,
    /// and both schedules must converge on reported usage without inventing
    /// a monetary measurement.
    #[tokio::test]
    async fn hand_executor_kill_drains_the_last_usage_snapshot() {
        let started = Arc::new(tokio::sync::Notify::new());
        let providers = ciacola_agent::ProviderRegistry::new()
            .with(Arc::new(SnapshotBlockingProvider {
                started: started.clone(),
            }))
            .expect("provider");
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool)
            .await
            .expect("ledger")
            .with_providers(providers);
        let agent_id = ledger
            .create_agent(
                &crate::agent::AgentDef::new("a", "system").provider("snapshot-blocking"),
                None,
            )
            .await
            .expect("agent");
        let seq = ledger.enqueue_turn(&agent_id, "work").await.expect("turn");
        let (tx, _rx) = tower_mcp::context::notification_channel(8);
        let executor = HandExecutor::start(ledger.clone(), Notifier(tx), 1);
        executor.submit(agent_id.clone(), seq);

        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("provider never emitted its snapshot");
        assert!(
            ledger
                .interrupt_turn(&agent_id, seq, "killed", "test kill")
                .await
                .expect("record kill")
        );
        assert!(
            executor.kill(&agent_id, seq),
            "live token was not signalled"
        );

        let turn = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let turn = ledger
                    .get_turn(&agent_id, seq)
                    .await
                    .expect("turn query")
                    .expect("turn");
                if turn.reported_tokens() == Some((89, 13, 34)) {
                    break turn;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached usage writer did not drain");
        assert_eq!(turn.state, "killed");
        assert_eq!(turn.cost_state, "unreported");
        assert_eq!(turn.reported_cost_micro_usd(), None);
        assert_eq!(executor.drain(Duration::from_secs(2)).await, 0);
    }

    /// The base names the surface and is shared; the token is secret
    /// and per agent; the backend must receive both.
    #[test]
    fn the_token_lands_only_in_ciacolas_headers() {
        let dir = std::env::temp_dir();
        let base = dir.join(format!("ciacola-test-base-{}.json", std::process::id()));
        std::fs::write(
            &base,
            r#"{"mcpServers": {
                "ciacola": {"type": "http", "url": "http://127.0.0.1:1/mcp"},
                "other": {"type": "http", "url": "http://x/", "headers": {"keep": "me"}}
            }}"#,
        )
        .expect("write base");

        let scope =
            token_scoped_mcp_scope("sekrit", &base.display().to_string()).expect("build scope");
        std::fs::remove_file(&base).ok();

        assert_eq!(
            endpoint(&scope, "ciacola").headers[crate::identity::TOKEN_HEADER],
            "sekrit",
            "the loopback server must carry the token"
        );
        assert!(
            !endpoint(&scope, "other")
                .headers
                .contains_key(crate::identity::TOKEN_HEADER),
            "an unrelated server must never receive ciacola's bearer secret"
        );
        assert_eq!(
            endpoint(&scope, "other").headers["keep"],
            "me",
            "existing headers survive"
        );
        assert_eq!(
            endpoint(&scope, "ciacola").url,
            "http://127.0.0.1:1/mcp",
            "everything else is untouched"
        );
        assert!(
            scope.strict,
            "the endpoint list an agent was handed is the whole of its authority"
        );
    }

    /// Core hands the backend intent and never writes the secret itself.
    /// The old path wrote it to `$TMPDIR/ciacola-agent-<id>.json` at mode
    /// 0644; the adapter now writes a randomized 0600 file instead. This
    /// pins the half core owns:
    /// the value is carried, and it does not survive a debug print.
    #[test]
    fn the_secret_is_carried_as_intent_and_never_printed() {
        let dir = std::env::temp_dir();
        let base = dir.join(format!("ciacola-test-print-{}.json", std::process::id()));
        std::fs::write(
            &base,
            r#"{"mcpServers": {"ciacola": {"type": "http", "url": "http://127.0.0.1:1/mcp"}}}"#,
        )
        .expect("write base");
        let scope =
            token_scoped_mcp_scope("top-secret", &base.display().to_string()).expect("scope");
        std::fs::remove_file(&base).ok();

        let printed = format!("{scope:?}");
        assert!(
            !printed.contains("top-secret"),
            "the token must not reach a log line or panic message: {printed}"
        );
    }

    /// Loud, not lenient: a base config that cannot be parsed is an
    /// operator error, and a turn that ran anonymously instead would be
    /// a quiet downgrade. Same for a server shape the contract cannot
    /// carry, which must be refused rather than silently dropped from
    /// the list the agent was granted.
    #[test]
    fn a_broken_base_config_fails_rather_than_degrading() {
        let dir = std::env::temp_dir();
        let base = dir.join(format!("ciacola-test-broken-{}.json", std::process::id()));
        std::fs::write(&base, "not json").expect("write");
        let out = token_scoped_mcp_scope("t", &base.display().to_string());
        std::fs::remove_file(&base).ok();
        assert!(out.is_err());

        let stdio = dir.join(format!("ciacola-test-stdio-{}.json", std::process::id()));
        std::fs::write(
            &stdio,
            r#"{"mcpServers": {"local": {"command": "some-server", "args": []}}}"#,
        )
        .expect("write");
        let out = token_scoped_mcp_scope("t", &stdio.display().to_string());
        std::fs::remove_file(&stdio).ok();
        assert!(
            out.is_err(),
            "a server the contract cannot express must be refused, not dropped"
        );

        let bad_header = dir.join(format!("ciacola-test-header-{}.json", std::process::id()));
        std::fs::write(
            &bad_header,
            r#"{"mcpServers": {"remote": {"type": "http", "url": "http://x/", "headers": {"x": 7}}}}"#,
        )
        .expect("write");
        let out = token_scoped_mcp_scope("t", &bad_header.display().to_string());
        std::fs::remove_file(&bad_header).ok();
        assert!(
            out.is_err(),
            "a non-string header must not be dropped silently"
        );
    }

    /// The complete regression from the dogfood run: a capped first
    /// turn is failed in the ledger, then the next queued prompt must
    /// ask to *resume* the id the provider already created, never to
    /// open it a second time. Asserted on the intent rather than on
    /// argv, because which flag that becomes is the adapter's business
    /// now; the meaning is core's, and the meaning is what broke.
    #[tokio::test]
    async fn a_capped_first_turn_makes_the_second_query_resume() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool).await.expect("ledger");
        let agent_id = ledger
            .create_agent(&crate::agent::AgentDef::new("a", "sys"), None)
            .await
            .expect("agent");
        let assigned = ledger
            .get_agent(&agent_id)
            .await
            .expect("get")
            .expect("agent row")
            .session
            .expect("preassigned session");

        let first = ledger
            .enqueue_turn(&agent_id, "first")
            .await
            .expect("first turn");
        assert!(ledger.claim_turn(&agent_id, first).await.expect("claim"));
        assert!(
            ledger
                .fail_turn(
                    &agent_id,
                    first,
                    "failed",
                    "hit max turns",
                    1,
                    1,
                    Some(&assigned),
                )
                .await
                .expect("record cap")
        );

        let second = ledger
            .enqueue_turn(&agent_id, "continue")
            .await
            .expect("second turn");
        let (def, mcp, session, started, prompt, _turn_ceiling) =
            load(&ledger, &agent_id, second).await.expect("load second");
        let intent = crate::agent::intent_for(&def, mcp, session.as_deref(), started, &prompt);

        assert_eq!(
            intent.resume,
            Some(ciacola_agent::ResumeId::ProviderAssigned(assigned.clone())),
            "the second turn must continue the conversation the cap opened"
        );
        assert!(
            intent.instructions.is_none(),
            "an open conversation already carries its instructions"
        );
    }

    /// The opening turn is the other half of the same invariant: an id
    /// we assigned but the provider has not seen yet must be sent as a
    /// request to open that conversation, carrying the system prompt.
    #[tokio::test]
    async fn a_first_turn_opens_the_preassigned_session_with_instructions() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool).await.expect("ledger");
        let agent_id = ledger
            .create_agent(&crate::agent::AgentDef::new("a", "sys"), None)
            .await
            .expect("agent");
        let assigned = ledger
            .get_agent(&agent_id)
            .await
            .expect("get")
            .expect("agent row")
            .session
            .expect("preassigned session");

        let first = ledger.enqueue_turn(&agent_id, "first").await.expect("turn");
        let (def, mcp, session, started, prompt, _turn_ceiling) =
            load(&ledger, &agent_id, first).await.expect("load first");
        let intent = crate::agent::intent_for(&def, mcp, session.as_deref(), started, &prompt);

        assert_eq!(
            intent.resume,
            Some(ciacola_agent::ResumeId::ClientAssigned(assigned)),
            "the opening turn names the conversation it is about to open"
        );
        assert!(
            intent.instructions.is_some(),
            "the turn that opens a conversation carries the system prompt"
        );
    }

    /// A resume event means the provider has opened the conversation.
    /// Re-confirming the same id on later turns must not reset the
    /// session origin, or rotation will count from zero forever.
    #[tokio::test]
    async fn provider_session_events_open_once_and_preserve_rotation_origin() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool).await.expect("ledger");
        let agent_id = ledger
            .create_agent(&crate::agent::AgentDef::new("a", "sys"), None)
            .await
            .expect("agent");
        let assigned = ledger
            .get_agent(&agent_id)
            .await
            .expect("get")
            .expect("row")
            .session
            .expect("assigned id");

        let first = SessionSink::new(ledger.clone(), agent_id.clone(), 1);
        ciacola_agent::TurnEvents::resume_id(
            &first,
            &ciacola_agent::ResumeId::ProviderAssigned(assigned.clone()),
        )
        .await;
        first.drain().await;
        assert_eq!(
            ledger
                .get_agent(&agent_id)
                .await
                .unwrap()
                .unwrap()
                .session_started_seq,
            1
        );

        let later = SessionSink::new(ledger.clone(), agent_id.clone(), 2);
        ciacola_agent::TurnEvents::resume_id(
            &later,
            &ciacola_agent::ResumeId::ProviderAssigned(assigned),
        )
        .await;
        later.drain().await;
        assert_eq!(
            ledger
                .get_agent(&agent_id)
                .await
                .unwrap()
                .unwrap()
                .session_started_seq,
            1,
            "re-confirming an open id must not reset rotation bookkeeping"
        );

        let newest = SessionSink::new(ledger.clone(), agent_id.clone(), 3);
        ciacola_agent::TurnEvents::resume_id(
            &newest,
            &ciacola_agent::ResumeId::ProviderAssigned("provider-new".into()),
        )
        .await;
        newest.drain().await;
        let row = ledger.get_agent(&agent_id).await.unwrap().unwrap();
        assert_eq!(row.session.as_deref(), Some("provider-new"));
        assert_eq!(row.session_started_seq, 3);
    }
}
