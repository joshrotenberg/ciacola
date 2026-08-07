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
use tokio_util::sync::CancellationToken;

use crate::exec::{TurnExecutor, run_claimed_turn};
use crate::ledger::Ledger;
use crate::notify::Notifier;
use crate::plugin::BoxFut;

type Kills = Arc<Mutex<HashMap<(String, i64), CancellationToken>>>;

pub struct PollingExecutor {
    /// Woken by `submit` so a fresh turn does not wait for the tick.
    /// Losing a nudge costs latency, never correctness.
    nudge: Arc<Notify>,
    kills: Kills,
    stopping: Arc<AtomicBool>,
    inflight: Arc<AtomicUsize>,
}

impl PollingExecutor {
    pub fn start(
        ledger: Ledger,
        notify: Notifier,
        concurrency: usize,
        interval: Duration,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            nudge: Arc::new(Notify::new()),
            kills: Arc::new(Mutex::new(HashMap::new())),
            stopping: Arc::new(AtomicBool::new(false)),
            inflight: Arc::new(AtomicUsize::new(0)),
        });

        let (nudge, kills) = (this.nudge.clone(), this.kills.clone());
        let (stopping, inflight) = (this.stopping.clone(), this.inflight.clone());
        tokio::spawn(async move {
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
                    let (ledger, notify) = (ledger.clone(), notify.clone());
                    let (kills, inflight) = (kills.clone(), inflight.clone());
                    let (agent_id, seq) = (turn.agent_id.clone(), turn.seq);
                    tokio::spawn(async move {
                        let _permit = permit;
                        match ledger.claim_turn(&agent_id, seq).await {
                            Ok(true) => {}
                            Ok(false) => return,
                            Err(e) => {
                                tracing::warn!(agent = %agent_id, seq, %e, "claim failed");
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
                            () = token.cancelled() => {}
                        }
                        inflight.fetch_sub(1, Ordering::SeqCst);
                        if let Ok(mut map) = kills.lock() {
                            map.remove(&key);
                        }
                    });
                }
            }
        });
        this
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
