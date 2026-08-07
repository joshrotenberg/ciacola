//! The apalis half of the spike-off: the middle-ground architecture.
//!
//! Same ledger, same turn functions, same six tools. The queue carries
//! only `{agent_id, seq}`, both immutable, so the frozen-args bug
//! family is structurally impossible: there is nothing in the payload
//! to go stale.
//!
//! Departures from how the stage1-10 system used apalis, plus what the
//! adversarial review forced:
//!
//! - **No `max_attempts(1)`, no `AbortError`.** The old system fought
//!   redelivery at 18 sites because re-running paid work is a bug. Here
//!   `claim_turn` makes delivery idempotent, so apalis can retry as
//!   much as it likes: every delivery after the first claims nothing
//!   and runs nothing.
//! - **The queue's verdict is ignored.** The handler returns Ok even
//!   for a killed turn, because the ledger already holds the truth and
//!   the task row records only that delivery was handled.
//! - **No idempotency key.** The ledger already owns that invariant,
//!   and flat5 caught the key silently swallowing a recovery resubmit
//!   against a zombie row locked by a dead worker.
//! - **The kill token is registered only by the delivery that wins the
//!   claim** (review finding: registering before the claim let a
//!   duplicate delivery clobber and then delete the live turn's token,
//!   leaving a paid run unkillable).
//! - **Construction and worker start are separate** (review finding:
//!   starting the worker before `recover()` let it redeliver a
//!   pre-crash task whose freshly claimed turn recovery would then
//!   misread as a crash orphan and kill).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use apalis::prelude::*;
use apalis_sqlite::{SqliteContext, SqlitePool, SqliteStorage};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tower_mcp::LogLevel;

use ciacola_core::exec::{TurnExecutor, run_claimed_turn};
use ciacola_core::ledger::Ledger;
use ciacola_core::notify::Notifier;

type JsonCodec = apalis_codec::json::JsonCodec<Vec<u8>>;
type Fetcher = apalis_sqlite::fetcher::SqliteFetcher;
type Store = SqliteStorage<TurnJob, JsonCodec, Fetcher>;

const QUEUE: &str = "turns";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TurnJob {
    agent_id: String,
    seq: i64,
}

type Kills = Arc<Mutex<HashMap<(String, i64), CancellationToken>>>;

struct Ctx {
    ledger: Ledger,
    notify: Notifier,
    kills: Kills,
}

pub struct ApalisExecutor {
    pool: SqlitePool,
    ctx: Arc<Ctx>,
}

async fn handle(job: TurnJob, ctx: Data<Arc<Ctx>>) -> Result<(), BoxDynError> {
    // Claim first. A losing delivery touches nothing: not the ledger,
    // not the kill registry.
    match ctx.ledger.claim_turn(&job.agent_id, job.seq).await {
        Ok(true) => {}
        Ok(false) => return Ok(()),
        Err(e) => {
            eprintln!("[apalis] claim {}/{}: {e}", job.agent_id, job.seq);
            return Ok(());
        }
    }
    let key = (job.agent_id.clone(), job.seq);
    let token = CancellationToken::new();
    if let Ok(mut kills) = ctx.kills.lock() {
        kills.insert(key.clone(), token.clone());
    }
    tokio::select! {
        () = run_claimed_turn(&ctx.ledger, &ctx.notify, &job.agent_id, job.seq) => {}
        // The kill tool recorded the turn killed before cancelling.
        // Dropping the branch above drops the exchange future, which
        // kills the provider process group.
        () = token.cancelled() => {}
    }
    if let Ok(mut kills) = ctx.kills.lock() {
        kills.remove(&key);
    }
    Ok(())
}

impl ApalisExecutor {
    /// Construct without starting the worker. `submit` works from here
    /// (it only writes to the queue), which is exactly what recovery
    /// needs: resubmit everything first, then start delivery with
    /// [`Self::spawn_worker`].
    pub fn new(pool: SqlitePool, ledger: Ledger, notify: Notifier) -> Arc<Self> {
        let ctx = Arc::new(Ctx {
            ledger,
            notify,
            kills: Arc::new(Mutex::new(HashMap::new())),
        });
        Arc::new(Self { pool, ctx })
    }

    /// Start delivering. Call after recovery has settled the ledger.
    pub fn spawn_worker(self: &Arc<Self>, concurrency: usize) {
        let ctx = self.ctx.clone();
        let worker_pool = self.pool.clone();
        tokio::spawn(async move {
            let monitor = Monitor::new().register(move |_| {
                WorkerBuilder::new(QUEUE)
                    .backend(SqliteStorage::<TurnJob, _, _>::new_in_queue(
                        &worker_pool,
                        QUEUE,
                    ))
                    .data(ctx.clone())
                    .concurrency(concurrency)
                    .build(handle)
            });
            if let Err(e) = monitor.run().await {
                eprintln!("[apalis] monitor: {e}");
            }
        });
    }
}

impl TurnExecutor for ApalisExecutor {
    fn submit(&self, agent_id: String, seq: i64) {
        let mut sink: Store = SqliteStorage::new_in_queue(&self.pool, QUEUE);
        let ctx = self.ctx.clone();
        tokio::spawn(async move {
            let pushed = sink
                .push_task(
                    TaskBuilder::new(TurnJob {
                        agent_id: agent_id.clone(),
                        seq,
                    })
                    .with_ctx(SqliteContext::new())
                    .with_task_id(TaskId::new(ulid::Ulid::new()))
                    .build(),
                )
                .await;
            if let Err(e) = pushed {
                // send already returned success to the client; a turn
                // whose push failed must not sit queued forever with
                // the agent unsendable.
                let error = format!("queue push failed: {e}");
                if let Err(e) = ctx
                    .ledger
                    .fail_turn(&agent_id, seq, "failed", &error, 0, None)
                    .await
                {
                    eprintln!("[apalis] record push failure {agent_id}/{seq}: {e}");
                }
                ctx.notify
                    .turn(LogLevel::Error, &agent_id, seq, "failed", &error);
            }
        });
    }

    fn kill(&self, agent_id: &str, seq: i64) -> bool {
        let Ok(kills) = self.ctx.kills.lock() else {
            return false;
        };
        match kills.get(&(agent_id.to_string(), seq)) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    fn name(&self) -> &'static str {
        "apalis"
    }
}
