//! A durable executor, from the record that was already durable.
//!
//! [`HandExecutor`](crate::exec::HandExecutor) dispatches over an
//! in-memory channel, so a crash loses whatever had been submitted but
//! not started. The usual answer is a work queue, and this project
//! spent a while with one before measuring what it bought.
//!
//! It bought nothing. A turn is written to the ledger as `queued`
//! *before* anyone is told to run it, so the durable record already
//! exists; the queue was a second copy of it. Worse, when the
//! queue-backed executor was killed mid-run, its backend never
//! reclaimed the dead worker's locks, so the tasks it was holding were
//! never redelivered and the ledger's own recovery pass did all the
//! work anyway.
//!
//! So: poll the ledger. `submit` only nudges, because the row that
//! matters is already committed, and the loop would find it on the next
//! tick regardless. That is the whole difference between this and a
//! queue, and it is why this file is short.
//!
//! What it gains over the channel:
//!
//! - **Survives a crash without help.** Queued turns are found on the
//!   next tick, whether or not recovery ran.
//! - **Safe across processes.** Two servers on one database cannot
//!   double-run a turn, because [`claim_turn`](crate::ledger::Ledger::claim_turn)
//!   settles that and always did.
//!
//! What it costs: a query per tick, and up to one tick of latency if a
//! nudge is missed. Both are cheap at the scale this runs at.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{Notify, Semaphore};

use crate::exec::{
    DispatchReadiness, InflightGuard, Kills, TurnExecutor, run_claimed_turn_cancellable,
};
use crate::ledger::Ledger;
use crate::notify::Notifier;
use crate::plugin::BoxFut;

#[cfg(test)]
struct WorkerStopSignal(tokio_util::sync::CancellationToken);

#[cfg(test)]
impl Drop for WorkerStopSignal {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub struct PollingExecutor {
    /// Woken by `submit` so a fresh turn does not wait for the tick.
    /// Losing a nudge costs latency, never correctness.
    nudge: Arc<Notify>,
    kills: Kills,
    stopping: Arc<AtomicBool>,
    inflight: Arc<AtomicUsize>,
    #[cfg(test)]
    worker_stopped: tokio_util::sync::CancellationToken,
}

impl PollingExecutor {
    pub fn start(
        ledger: Ledger,
        notify: Notifier,
        concurrency: usize,
        interval: Duration,
    ) -> Arc<Self> {
        let readiness = DispatchReadiness::closed();
        readiness.open();
        Self::start_gated(ledger, notify, concurrency, interval, readiness)
    }

    /// Construct a poller whose timer and submission nudges remain dormant
    /// until `readiness` opens.
    pub fn start_gated(
        ledger: Ledger,
        notify: Notifier,
        concurrency: usize,
        interval: Duration,
        readiness: DispatchReadiness,
    ) -> Arc<Self> {
        #[cfg(test)]
        let worker_stopped = tokio_util::sync::CancellationToken::new();
        let this = Arc::new(Self {
            nudge: Arc::new(Notify::new()),
            kills: Arc::new(Mutex::new(HashMap::new())),
            stopping: Arc::new(AtomicBool::new(false)),
            inflight: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            worker_stopped,
        });

        let (nudge, kills) = (this.nudge.clone(), this.kills.clone());
        let (stopping, inflight) = (this.stopping.clone(), this.inflight.clone());
        #[cfg(test)]
        let worker_stopped = this.worker_stopped.clone();
        tokio::spawn(async move {
            #[cfg(test)]
            let _worker_stop_signal = WorkerStopSignal(worker_stopped);
            // The durable ledger can contain work before this executor is
            // constructed, so gating only `submit` would be insufficient:
            // the independent timer must not begin until startup is ready.
            readiness.wait().await;
            if stopping.load(Ordering::SeqCst) {
                return;
            }
            let limit = Arc::new(Semaphore::new(concurrency));
            loop {
                tokio::select! {
                    () = nudge.notified() => {}
                    () = tokio::time::sleep(interval) => {}
                }
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                let queued = match ledger.turns_in_state("queued").await {
                    Ok(queued) => queued,
                    Err(e) => {
                        tracing::warn!(%e, "poll failed");
                        continue;
                    }
                };
                for turn in queued {
                    // Claiming is what makes a duplicate free, so an
                    // overlapping tick, a second process, or a nudge
                    // racing the timer all cost one wasted UPDATE.
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(permit) = limit.clone().acquire_owned().await else {
                        return;
                    };
                    // This both accounts for the dispatch-startup gap
                    // and re-checks shutdown after a potentially long
                    // permit wait. On refusal, both guards drop and the
                    // ledger row stays queued for the next boot.
                    let Some(inflight_guard) =
                        InflightGuard::after_permit(&stopping, inflight.clone())
                    else {
                        break;
                    };
                    let (ledger, notify) = (ledger.clone(), notify.clone());
                    let kills = kills.clone();
                    let (agent_id, seq) = (turn.agent_id.clone(), turn.seq);
                    tokio::spawn(async move {
                        let _permit = permit;
                        let _inflight = inflight_guard;
                        match ledger.claim_turn(&agent_id, seq).await {
                            Ok(true) => {}
                            Ok(false) => return,
                            Err(e) => {
                                tracing::warn!(agent = %agent_id, seq, %e, "claim failed");
                                return;
                            }
                        }
                        run_claimed_turn_cancellable(&ledger, &notify, kills, &agent_id, seq).await;
                    });
                }
            }
        });
        this
    }

    #[cfg(test)]
    async fn wait_until_worker_stopped(&self) {
        self.worker_stopped.cancelled().await;
    }
}

impl TurnExecutor for PollingExecutor {
    /// A nudge, not a handoff. The turn is already `queued` in the
    /// ledger, which is the only place it needs to be.
    fn submit(&self, _agent_id: String, _seq: i64) {
        self.nudge.notify_one();
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
        "polling"
    }

    fn drain(&self, grace: Duration) -> BoxFut<'_, usize> {
        Box::pin(async move {
            self.stopping.store(true, Ordering::SeqCst);
            self.nudge.notify_waiters();
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
mod tests {
    use super::*;

    struct CountingProvider {
        calls: Arc<AtomicUsize>,
    }

    impl ciacola_agent::Provider for CountingProvider {
        fn key(&self) -> ciacola_agent::ProviderKey {
            ciacola_agent::ProviderKey::new("poll-counting")
        }

        fn capabilities(&self) -> ciacola_agent::Capabilities {
            let mut capabilities =
                ciacola_agent::Capabilities::none(ciacola_agent::ProviderKey::new("poll-counting"));
            capabilities.client_assigned_resume = true;
            capabilities.allowed_tools = true;
            capabilities.reports_cost = true;
            capabilities.reports_token_usage = true;
            capabilities.reports_provider_turns = true;
            capabilities
        }

        fn run<'a>(
            &'a self,
            _intent: &'a ciacola_agent::TurnIntent,
            _events: &'a dyn ciacola_agent::TurnEvents,
        ) -> ciacola_agent::BoxFut<'a, Result<ciacola_agent::TurnOutcome, ciacola_agent::AgentError>>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(ciacola_agent::TurnOutcome {
                    reply: "counted".to_string(),
                    resume: None,
                    cost: ciacola_agent::Cost::Reported { micro_usd: 0 },
                    usage: ciacola_agent::Usage::Reported(ciacola_agent::TokenUsage::default()),
                    provider_turns: Some(1),
                    elapsed: Duration::from_millis(1),
                    metadata: Default::default(),
                    failure: None,
                })
            })
        }

        fn owns_process(&self, _ps_line: &str) -> bool {
            false
        }
    }

    async fn queued_counting_turn() -> (Ledger, String, i64, Notifier, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let providers = ciacola_agent::ProviderRegistry::new()
            .with(Arc::new(CountingProvider {
                calls: calls.clone(),
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
                &crate::agent::AgentDef::new("readiness", "system").provider("poll-counting"),
                None,
            )
            .await
            .expect("agent");
        let seq = ledger
            .enqueue_turn(&agent_id, "wait for readiness")
            .await
            .expect("turn");
        let (tx, _rx) = tower_mcp::context::notification_channel(8);
        (ledger, agent_id, seq, Notifier(tx), calls)
    }

    async fn wait_for_state(ledger: &Ledger, agent_id: &str, seq: i64, expected: &str) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let turn = ledger
                    .get_turn(agent_id, seq)
                    .await
                    .expect("turn query")
                    .expect("turn");
                if turn.state == expected {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("turn never reached {expected}"));
    }

    fn executor(stopping: Arc<AtomicBool>, inflight: Arc<AtomicUsize>) -> PollingExecutor {
        PollingExecutor {
            nudge: Arc::new(Notify::new()),
            kills: Arc::new(Mutex::new(HashMap::new())),
            stopping,
            inflight,
            worker_stopped: tokio_util::sync::CancellationToken::new(),
        }
    }

    #[test]
    fn shutdown_after_a_permit_refuses_the_dispatch() {
        let stopping = AtomicBool::new(true);
        let inflight = Arc::new(AtomicUsize::new(0));

        let reservation = InflightGuard::after_permit(&stopping, inflight.clone());

        assert!(reservation.is_none());
        assert_eq!(inflight.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn drain_sees_a_dispatch_before_its_task_starts() {
        let stopping = Arc::new(AtomicBool::new(false));
        let inflight = Arc::new(AtomicUsize::new(0));
        let reservation = InflightGuard::after_permit(&stopping, inflight.clone())
            .expect("dispatch reserved before task spawn");
        let executor = executor(stopping, inflight.clone());

        assert_eq!(executor.drain(Duration::ZERO).await, 1);

        drop(reservation);
        assert_eq!(inflight.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn polling_and_recovery_stay_closed_then_dispatch_exactly_once_after_open() {
        let (ledger, agent_id, seq, notify, calls) = queued_counting_turn().await;
        let readiness = DispatchReadiness::closed();
        let interval = Duration::from_millis(5);
        let executor =
            PollingExecutor::start_gated(ledger.clone(), notify, 1, interval, readiness.clone());
        readiness.wait_until_worker_is_parked().await;

        // More than one polling interval passes without a ledger scan or
        // claim, proving the independent timer is behind the same gate as a
        // submission nudge.
        tokio::time::sleep(interval * 3).await;
        let turn = ledger
            .get_turn(&agent_id, seq)
            .await
            .expect("turn query")
            .expect("turn");
        assert_eq!(turn.state, "queued");
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let report = crate::recover::recover(&ledger, executor.as_ref())
            .await
            .expect("recover while closed");
        assert_eq!(report.resubmitted, 1);
        executor.submit(agent_id.clone(), seq);
        let turn = ledger
            .get_turn(&agent_id, seq)
            .await
            .expect("turn query")
            .expect("turn");
        assert_eq!(turn.state, "queued");
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        readiness.open();
        wait_for_state(&ledger, &agent_id, seq, "ok").await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(executor.drain(Duration::from_secs(1)).await, 0);
    }

    #[tokio::test]
    async fn draining_a_closed_poller_prevents_a_later_open_from_claiming() {
        let (ledger, agent_id, seq, notify, calls) = queued_counting_turn().await;
        let readiness = DispatchReadiness::closed();
        let executor = PollingExecutor::start_gated(
            ledger.clone(),
            notify,
            1,
            Duration::from_millis(5),
            readiness.clone(),
        );
        readiness.wait_until_worker_is_parked().await;

        assert_eq!(executor.drain(Duration::ZERO).await, 0);
        readiness.open();
        tokio::time::timeout(Duration::from_secs(1), executor.wait_until_worker_stopped())
            .await
            .expect("polling worker did not stop after readiness opened");

        let turn = ledger
            .get_turn(&agent_id, seq)
            .await
            .expect("turn query")
            .expect("turn");
        assert_eq!(turn.state, "queued");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
