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
                session: Option<String>| async move {
        if let Err(e) = ledger
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
        {
            eprintln!("[exec] record failure {agent_id}/{seq}: {e}");
        }
        tracing::warn!(agent = %agent_id, seq, state, %error, "turn settled badly");
        notify.turn(LogLevel::Error, agent_id, seq, state, &error);
    };

    let (def, session, started, prompt) = match load(ledger, agent_id, seq).await {
        Ok(loaded) => loaded,
        Err(e) => return fail("failed", e.to_string(), 0, 0, None).await,
    };

    let wall = std::time::Instant::now();
    match run_exchange(&def, session.as_deref(), started, &prompt).await {
        Ok(exchange) => {
            if let Some(error) = &exchange.error {
                // The provider ran and failed. The spend and any session
                // it learned are real; record both.
                let session = (!exchange.session.is_empty()).then(|| exchange.session.clone());
                return fail(
                    "failed",
                    error.clone(),
                    exchange.cost_micro_usd as i64,
                    exchange.elapsed_ms,
                    session,
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
                        exchange.cost_micro_usd as i64,
                        exchange.elapsed_ms,
                        Some(exchange.session.clone()),
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
            )
            .await
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

/// The agent's MCP config, with its token in every server's headers.
///
/// The base file names the surface (agent or operator mount) and is
/// shared; the token is per agent and secret; the file the provider
/// actually reads is therefore written per agent, here, at the last
/// moment before the turn runs. Failing loudly is deliberate: a base
/// config that cannot be parsed is an operator error, and a turn that
/// ran anonymously *instead* would be the kind of quiet downgrade this
/// codebase keeps paying for.
fn token_scoped_mcp_config(
    agent_id: &str,
    token: &str,
    base_path: &str,
) -> Result<String, crate::agent::FlatError> {
    let raw =
        std::fs::read_to_string(base_path).map_err(|e| format!("mcp config {base_path}: {e}"))?;
    let mut config: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("mcp config {base_path}: {e}"))?;
    let Some(servers) = config.get_mut("mcpServers").and_then(|s| s.as_object_mut()) else {
        return Err(format!("mcp config {base_path}: no mcpServers object").into());
    };
    for (_, server) in servers.iter_mut() {
        let headers = server
            .as_object_mut()
            .ok_or_else(|| format!("mcp config {base_path}: server entry is not an object"))?
            .entry("headers")
            .or_insert_with(|| serde_json::json!({}));
        headers
            .as_object_mut()
            .ok_or_else(|| format!("mcp config {base_path}: headers is not an object"))?
            .insert(
                crate::identity::TOKEN_HEADER.into(),
                serde_json::Value::String(token.into()),
            );
    }
    let path = std::env::temp_dir().join(format!("ciacola-agent-{agent_id}.json"));
    std::fs::write(&path, serde_json::to_string(&config)?)
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path.display().to_string())
}

async fn load(
    ledger: &Ledger,
    agent_id: &str,
    seq: i64,
) -> Result<(crate::agent::AgentDef, Option<String>, bool, String), crate::agent::FlatError> {
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
    let mut def = agent.def;
    if let Some(base) = def.mcp_config.clone() {
        if let Some(token) = ledger.token_of(agent_id).await? {
            def.mcp_config = Some(token_scoped_mcp_config(agent_id, &token, &base)?);
        }
    }
    let agent = crate::ledger::AgentRow { def, ..agent };

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
            Some(next),
            false,
            format!("{preamble}{}", turn.prompt),
        ));
    }
    Ok((agent.def, agent.session, started, turn.prompt))
}

type Kills = Arc<Mutex<HashMap<(String, i64), CancellationToken>>>;

/// The default executor: everything a work queue would do for us,
/// written out. A channel is the queue, a semaphore is the
/// concurrency limit, a cancellation token is the kill switch. Dropping
/// the turn's future kills the provider process group (claude-wrapper
/// sets kill_on_drop and its own process group per child).
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

    /// The base names the surface and is shared; the token is secret
    /// and per agent; the provider must read a file carrying both.
    #[test]
    fn the_token_landss_in_every_servers_headers() {
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

        let path = token_scoped_mcp_config("agent-1", "sekrit", &base.display().to_string())
            .expect("inject");
        let out: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        std::fs::remove_file(&base).ok();
        std::fs::remove_file(&path).ok();

        for name in ["ciacola", "other"] {
            assert_eq!(
                out["mcpServers"][name]["headers"][crate::identity::TOKEN_HEADER],
                serde_json::json!("sekrit"),
                "server {name} must carry the token"
            );
        }
        assert_eq!(
            out["mcpServers"]["other"]["headers"]["keep"],
            serde_json::json!("me"),
            "existing headers survive"
        );
        assert_eq!(
            out["mcpServers"]["ciacola"]["url"],
            serde_json::json!("http://127.0.0.1:1/mcp"),
            "everything else is untouched"
        );
    }

    /// Loud, not lenient: a base config that cannot be parsed is an
    /// operator error, and a turn that ran anonymously instead would be
    /// a quiet downgrade.
    #[test]
    fn a_broken_base_config_fails_rather_than_degrading() {
        let dir = std::env::temp_dir();
        let base = dir.join(format!("ciacola-test-broken-{}.json", std::process::id()));
        std::fs::write(&base, "not json").expect("write");
        let out = token_scoped_mcp_config("agent-1", "t", &base.display().to_string());
        std::fs::remove_file(&base).ok();
        assert!(out.is_err());
    }
}
