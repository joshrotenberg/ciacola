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
//! however many times, the exchange runs at most once. The claim also
//! gates the kill registry: only the delivery that wins the claim
//! registers a token, so a duplicate delivery can never clobber the
//! live turn's kill switch (a defect the adversarial review caught in
//! the first version, where the token was registered before the claim).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tower_mcp::LogLevel;

use crate::agent::run_exchange;
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
    let fail = |state: &'static str,
                error: String,
                cost: i64,
                elapsed_ms: u64,
                session: Option<String>,
                exchange: Option<crate::agent::Exchange>| async move {
        let recorded = match exchange.as_ref() {
            Some(exchange) => {
                ledger
                    .fail_exchange(agent_id, seq, state, &error, exchange)
                    .await
            }
            None => {
                ledger
                    .fail_turn(
                        agent_id,
                        seq,
                        state,
                        &error,
                        cost,
                        elapsed_ms as i64,
                        session.as_deref(),
                    )
                    .await
            }
        };
        if let Err(e) = recorded {
            eprintln!("[exec] record failure {agent_id}/{seq}: {e}");
        }
        tracing::warn!(agent = %agent_id, seq, state, %error, "turn settled badly");
        notify.turn(LogLevel::Error, agent_id, seq, state, &error);
    };

    let (def, mcp, session, started, prompt) = match load(ledger, agent_id, seq).await {
        Ok(loaded) => loaded,
        Err(e) => return fail("failed", e.to_string(), 0, 0, None, None).await,
    };

    let wall = std::time::Instant::now();
    let events = SessionSink {
        ledger: ledger.clone(),
        agent_id: agent_id.to_string(),
        seq,
    };
    match run_exchange(
        ledger.providers(),
        &def,
        mcp,
        session.as_deref(),
        started,
        &prompt,
        &events,
    )
    .await
    {
        Ok(exchange) => {
            if let Some(error) = &exchange.error {
                // The provider ran and failed. The spend and any session
                // it learned are real; record both.
                return fail(
                    "failed",
                    error.clone(),
                    exchange.cost_micro_usd() as i64,
                    exchange.elapsed_ms,
                    exchange.session.clone(),
                    Some(exchange),
                )
                .await;
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
                        exchange.cost_micro_usd() as i64,
                        exchange.elapsed_ms,
                        exchange.session.clone(),
                        Some(exchange),
                    )
                    .await;
                }
            }
        }
        Err(e) => {
            fail(
                "failed",
                e.to_string(),
                0,
                wall.elapsed().as_millis() as u64,
                None,
                None,
            )
            .await
        }
    }
}

/// Persists a conversation id the moment a backend reveals one.
///
/// Ciacola assigns ids up front, so for Claude this usually confirms an
/// id already in the ledger and writes nothing new. It exists for the
/// case up-front assignment cannot cover: a backend that names its own
/// conversation tells us partway through a turn that may still have
/// twenty minutes to run, and a crash in those twenty minutes would
/// otherwise lose the id and make "send again" start over.
///
/// Never fails a turn. An id that cannot be written is worth a log line,
/// not an abandoned run that has already been paid for.
struct SessionSink {
    ledger: Ledger,
    agent_id: String,
    seq: i64,
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
/// The base file names the surface (agent or operator mount) and is
/// shared; the token is per agent and secret. This reads the shared file
/// and returns *intent*: a list of endpoints the backend materialises
/// however its own CLI wants them.
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
        ));
    }
    Ok((agent.def, mcp, agent.session, started, turn.prompt))
}

type Kills = Arc<Mutex<HashMap<(String, i64), CancellationToken>>>;

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
                    let key = (agent_id.clone(), seq);
                    let token = CancellationToken::new();
                    if let Ok(mut map) = kills.lock() {
                        map.insert(key.clone(), token.clone());
                    }
                    inflight.fetch_add(1, Ordering::SeqCst);
                    tokio::select! {
                        () = run_claimed_turn(&ledger, &notify, &agent_id, seq) => {}
                        // The kill tool recorded the turn killed before
                        // cancelling; dropping the branch above kills
                        // the provider process group.
                        () = token.cancelled() => {}
                    }
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    if let Ok(mut map) = kills.lock() {
                        map.remove(&key);
                    }
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
        let (def, mcp, session, started, prompt) =
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
        let (def, mcp, session, started, prompt) =
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

        let first = SessionSink {
            ledger: ledger.clone(),
            agent_id: agent_id.clone(),
            seq: 1,
        };
        ciacola_agent::TurnEvents::resume_id(
            &first,
            &ciacola_agent::ResumeId::ProviderAssigned(assigned.clone()),
        )
        .await;
        assert_eq!(
            ledger
                .get_agent(&agent_id)
                .await
                .unwrap()
                .unwrap()
                .session_started_seq,
            1
        );

        let later = SessionSink {
            ledger: ledger.clone(),
            agent_id: agent_id.clone(),
            seq: 2,
        };
        ciacola_agent::TurnEvents::resume_id(
            &later,
            &ciacola_agent::ResumeId::ProviderAssigned(assigned),
        )
        .await;
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

        ciacola_agent::TurnEvents::resume_id(
            &SessionSink {
                ledger: ledger.clone(),
                agent_id: agent_id.clone(),
                seq: 3,
            },
            &ciacola_agent::ResumeId::ProviderAssigned("provider-new".into()),
        )
        .await;
        let row = ledger.get_agent(&agent_id).await.unwrap().unwrap();
        assert_eq!(row.session.as_deref(), Some("provider-new"));
        assert_eq!(row.session_started_seq, 3);
    }
}
