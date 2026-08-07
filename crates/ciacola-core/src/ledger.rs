//! The product ledger: agents and turns, in sqlite, beside no queue.
//!
//! Stage 9 established the rule this module lives by: the application's
//! own record is the source of truth, and anything a turn *learns* (the
//! session id above all) is written here the moment it is known. The
//! queue-shaped system needed this ledger *in addition to* apalis; the
//! flat system needs only this.
//!
//! Turn states: `queued -> running -> ok | failed | killed`. An agent's
//! state is derived from its turns, never stored, so it cannot drift.

use apalis_sqlite::SqlitePool;
use serde::Serialize;

use crate::agent::{AgentDef, Exchange, FlatError};
use crate::plugin::Migration;

#[derive(Clone)]
pub struct Ledger {
    pool: SqlitePool,
    /// Server-wide defaults stamped onto every agent at creation.
    ///
    /// Held here rather than applied by each caller because there are
    /// three creation paths (config, `spawn_role`, raw `spawn`) and the
    /// first version applied them in two. The raw path silently
    /// produced agents with no isolation and no house rules, which a
    /// probe caught by quoting a rule from the operator's ambient
    /// CLAUDE.md that hermetic was supposed to have sealed off. Same
    /// lesson as the spend limit: a guard on some paths is not a guard.
    runtime: crate::roles::Runtime,
    house_rules: Option<String>,
}

/// An agent as the ledger sees it: definition plus everything learned.
#[derive(Debug, Clone, Serialize)]
pub struct AgentRow {
    pub agent_id: String,
    pub name: String,
    pub def: AgentDef,
    pub session: Option<String>,
    pub cost_micro_usd: i64,
    /// Derived: `running` if any turn runs, `queued` if one waits,
    /// otherwise `idle`.
    pub state: String,
    pub turns: i64,
    /// Who spawned this agent, if an agent did. Identity, not
    /// definition, so it lives beside the def rather than in it.
    /// Honor-system for now: the loopback has no caller identity (a
    /// recorded gap), so orchestrators are instructed to pass their
    /// own id.
    pub spawned_by: Option<String>,
    pub retired: bool,
    /// When this agent last finished anything, unix seconds. Zero for
    /// an agent that has never run.
    pub last_active_unix: i64,
    /// Seq of the first turn in the current provider session. Turns
    /// since then is what the rotation policy measures.
    pub session_started_seq: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnRow {
    pub agent_id: String,
    pub seq: i64,
    pub prompt: String,
    pub state: String,
    pub reply: Option<String>,
    pub error: Option<String>,
    pub cost_micro_usd: i64,
    pub elapsed_ms: i64,
    pub tokens_in: i64,
    pub tokens_out: i64,
}

type AgentTuple = (
    String,
    String,
    String,
    Option<String>,
    i64,
    String,
    i64,
    Option<String>,
    i64,
    i64,
    i64,
);
type TurnTuple = (
    String,
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    i64,
    i64,
);

const AGENT_SELECT: &str = "\
    SELECT a.agent_id, a.name, a.def, a.session, a.cost_micro_usd,
           CASE
             WHEN EXISTS (SELECT 1 FROM turns t
                          WHERE t.agent_id = a.agent_id AND t.state = 'running')
               THEN 'running'
             WHEN EXISTS (SELECT 1 FROM turns t
                          WHERE t.agent_id = a.agent_id AND t.state = 'queued')
               THEN 'queued'
             ELSE 'idle'
           END,
           (SELECT COUNT(*) FROM turns t WHERE t.agent_id = a.agent_id),
           a.spawned_by, a.retired, a.session_started_seq,
           (SELECT COALESCE(MAX(at_unix), 0) FROM turns t WHERE t.agent_id = a.agent_id)
    FROM agents a";

const TURN_SELECT: &str = "\
    SELECT agent_id, seq, prompt, state, reply, error, cost_micro_usd, elapsed_ms,
           tokens_in, tokens_out
    FROM turns";

fn agent_row(t: AgentTuple) -> Result<AgentRow, FlatError> {
    let (
        agent_id,
        name,
        def,
        session,
        cost_micro_usd,
        state,
        turns,
        spawned_by,
        retired,
        session_started_seq,
        last_active_unix,
    ) = t;
    Ok(AgentRow {
        agent_id,
        name,
        def: serde_json::from_str(&def)?,
        session,
        cost_micro_usd,
        state,
        turns,
        spawned_by,
        retired: retired != 0,
        session_started_seq,
        last_active_unix,
    })
}

fn turn_row(t: TurnTuple) -> TurnRow {
    let (
        agent_id,
        seq,
        prompt,
        state,
        reply,
        error,
        cost_micro_usd,
        elapsed_ms,
        tokens_in,
        tokens_out,
    ) = t;
    TurnRow {
        agent_id,
        seq,
        prompt,
        state,
        reply,
        error,
        cost_micro_usd,
        elapsed_ms,
        tokens_in,
        tokens_out,
    }
}

impl Ledger {
    /// Read access for plugins that analyse core's tables rather than
    /// owning their own, like `tuning`. Deliberately not a general
    /// escape hatch: a plugin that writes here is reaching.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Core's own schema, through the same migration mechanism the
    /// plugins use. This replaces a loop of `ALTER TABLE` statements
    /// run with `let _ =`, which swallowed every error including the
    /// ones that matter (a locked or full database looked identical to
    /// a column that already existed).
    pub async fn setup(pool: SqlitePool) -> Result<Self, FlatError> {
        const MIGRATIONS: &[Migration] = &[
            Migration::new(
                "0001_agents_turns",
                "CREATE TABLE IF NOT EXISTS agents (
                     agent_id TEXT PRIMARY KEY,
                     name TEXT NOT NULL,
                     def TEXT NOT NULL,
                     session TEXT,
                     cost_micro_usd INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE IF NOT EXISTS turns (
                     agent_id TEXT NOT NULL,
                     seq INTEGER NOT NULL,
                     prompt TEXT NOT NULL,
                     state TEXT NOT NULL,
                     reply TEXT,
                     error TEXT,
                     cost_micro_usd INTEGER NOT NULL DEFAULT 0,
                     elapsed_ms INTEGER NOT NULL DEFAULT 0,
                     PRIMARY KEY (agent_id, seq));",
            ),
            Migration::add_column(
                "0002_agents_spawned_by",
                "ALTER TABLE agents ADD COLUMN spawned_by TEXT",
            ),
            Migration::add_column(
                "0003_agents_retired",
                "ALTER TABLE agents ADD COLUMN retired INTEGER NOT NULL DEFAULT 0",
            ),
            Migration::add_column(
                "0004_agents_session_started_seq",
                "ALTER TABLE agents ADD COLUMN session_started_seq INTEGER NOT NULL DEFAULT 0",
            ),
            Migration::add_column(
                "0005_turns_at_unix",
                "ALTER TABLE turns ADD COLUMN at_unix INTEGER NOT NULL DEFAULT 0",
            ),
            Migration::add_column(
                "0006_turns_tokens_in",
                "ALTER TABLE turns ADD COLUMN tokens_in INTEGER NOT NULL DEFAULT 0",
            ),
            Migration::add_column(
                "0007_turns_tokens_out",
                "ALTER TABLE turns ADD COLUMN tokens_out INTEGER NOT NULL DEFAULT 0",
            ),
            Migration::add_column(
                "0008_turns_tokens_cached",
                "ALTER TABLE turns ADD COLUMN tokens_cached INTEGER NOT NULL DEFAULT 0",
            ),
        ];
        crate::plugin::apply_migrations(&pool, "core", MIGRATIONS).await?;
        Ok(Self {
            pool,
            runtime: Default::default(),
            house_rules: None,
        })
    }

    /// Attach the server-wide defaults. Called once at boot, before any
    /// agent exists.
    pub fn with_runtime(mut self, runtime: crate::roles::Runtime) -> Result<Self, FlatError> {
        self.house_rules = runtime.resolved_house_rules()?;
        self.runtime = runtime;
        Ok(self)
    }

    pub async fn create_agent(
        &self,
        def: &AgentDef,
        spawned_by: Option<&str>,
    ) -> Result<String, FlatError> {
        let agent_id = ulid::Ulid::new().to_string();
        // A definition that does not say otherwise inherits the
        // server's isolation and rules. Explicit values win, so a role
        // can still opt out.
        let mut def = def.clone();
        if def.hermetic.is_none() {
            def.hermetic = self.runtime.hermetic.clone();
        }
        if def.claude_home.is_none() {
            def.claude_home = self.runtime.claude_home.clone();
        }
        if def.house_rules.is_none() {
            def.house_rules = self.house_rules.clone();
        }
        if def.token_env.is_none() {
            def.token_env = self.runtime.token_env.clone();
        }
        sqlx::query("INSERT INTO agents (agent_id, name, def, spawned_by) VALUES (?1, ?2, ?3, ?4)")
            .bind(&agent_id)
            .bind(&def.name)
            .bind(serde_json::to_string(&def)?)
            .bind(spawned_by)
            .execute(&self.pool)
            .await?;
        Ok(agent_id)
    }

    /// Replace an agent's definition, keeping its identity, session,
    /// and history. This is what makes config-born agents updatable:
    /// the def follows the config file; the conversation persists.
    pub async fn update_agent_def(&self, agent_id: &str, def: &AgentDef) -> Result<(), FlatError> {
        sqlx::query("UPDATE agents SET name = ?2, def = ?3 WHERE agent_id = ?1")
            .bind(agent_id)
            .bind(&def.name)
            .bind(serde_json::to_string(def)?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Find an active (non-retired) agent by name. Names are not unique
    /// in general; config-born agents treat them as such.
    pub async fn find_active_by_name(&self, name: &str) -> Result<Option<AgentRow>, FlatError> {
        let row: Option<AgentTuple> = sqlx::query_as(&format!(
            "{AGENT_SELECT} WHERE a.name = ?1 AND a.retired = 0 LIMIT 1"
        ))
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(agent_row).transpose()
    }

    /// Retire an agent: it stops being sendable and drops out of list,
    /// but its conversation stays in the ledger. Refused while a turn
    /// is queued or running; the guard is in the UPDATE itself.
    pub async fn retire_agent(&self, agent_id: &str) -> Result<bool, FlatError> {
        let done = sqlx::query(
            "UPDATE agents SET retired = 1
             WHERE agent_id = ?1 AND retired = 0
               AND NOT EXISTS (SELECT 1 FROM turns
                               WHERE agent_id = ?1 AND state IN ('queued', 'running'))",
        )
        .bind(agent_id)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// How many spawns separate this agent from a root. A root is 0.
    /// The walk is bounded so a cycle in `spawned_by`, which nothing
    /// should create but a bug could, cannot hang a submission.
    pub async fn spawn_depth(&self, agent_id: &str) -> Result<i64, FlatError> {
        let mut depth = 0;
        let mut current = agent_id.to_string();
        for _ in 0..64 {
            let row: Option<(Option<String>,)> =
                sqlx::query_as("SELECT spawned_by FROM agents WHERE agent_id = ?1")
                    .bind(&current)
                    .fetch_optional(&self.pool)
                    .await?;
            match row.and_then(|(parent,)| parent) {
                Some(parent) => {
                    depth += 1;
                    current = parent;
                }
                None => return Ok(depth),
            }
        }
        Ok(depth)
    }

    /// Total tokens in and out across every turn.
    pub async fn token_totals(&self) -> Result<(i64, i64), FlatError> {
        let row: (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(tokens_in), 0), COALESCE(SUM(tokens_out), 0) FROM turns",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Spend since a cutoff. "What did today cost" is the question an
    /// unattended system actually gets asked, and a cumulative total
    /// cannot answer it.
    pub async fn spend_since(&self, since_unix: i64) -> Result<i64, FlatError> {
        let (cost,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(SUM(cost_micro_usd), 0) FROM turns WHERE at_unix >= ?1",
        )
        .bind(since_unix)
        .fetch_one(&self.pool)
        .await?;
        Ok(cost)
    }

    /// Spend and turn totals across every agent ever, retired included.
    /// Retirement hides an agent from the list; it must never hide the
    /// money it cost.
    pub async fn totals(&self) -> Result<(i64, i64), FlatError> {
        let (cost,): (i64,) = sqlx::query_as("SELECT COALESCE(SUM(cost_micro_usd), 0) FROM agents")
            .fetch_one(&self.pool)
            .await?;
        let (turns,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM turns")
            .fetch_one(&self.pool)
            .await?;
        Ok((cost, turns))
    }

    pub async fn retired_count(&self) -> Result<i64, FlatError> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM agents WHERE retired = 1")
            .fetch_one(&self.pool)
            .await?;
        Ok(count)
    }

    pub async fn get_agent(&self, agent_id: &str) -> Result<Option<AgentRow>, FlatError> {
        let row: Option<AgentTuple> =
            sqlx::query_as(&format!("{AGENT_SELECT} WHERE a.agent_id = ?1"))
                .bind(agent_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(agent_row).transpose()
    }

    /// Active agents only; the retired stay in the ledger, not the list.
    pub async fn list_agents(&self) -> Result<Vec<AgentRow>, FlatError> {
        let rows: Vec<AgentTuple> = sqlx::query_as(&format!(
            "{AGENT_SELECT} WHERE a.retired = 0 ORDER BY a.agent_id"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(agent_row).collect()
    }

    /// Record the next turn as queued, or say why not. One turn in
    /// flight per agent is the ledger's one admission rule: the guard is
    /// inside the INSERT itself, so two concurrent sends cannot both
    /// pass it.
    pub async fn enqueue_turn(&self, agent_id: &str, prompt: &str) -> Result<i64, FlatError> {
        // RETURNING keeps the seq in the same statement as the guarded
        // INSERT; a separate MAX(seq) read could hand back another
        // sender's turn number under a kill-plus-resend race.
        let row: Option<(i64,)> = sqlx::query_as(
            "INSERT INTO turns (agent_id, seq, prompt, state, at_unix)
             SELECT ?1,
                    (SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE agent_id = ?1),
                    ?2, 'queued', ?3
             WHERE EXISTS (SELECT 1 FROM agents WHERE agent_id = ?1 AND retired = 0)
               AND NOT EXISTS (SELECT 1 FROM turns
                               WHERE agent_id = ?1 AND state IN ('queued', 'running'))
             RETURNING seq",
        )
        .bind(agent_id)
        .bind(prompt)
        .bind(crate::time::now_unix())
        .fetch_optional(&self.pool)
        .await?;
        match row {
            Some((seq,)) => Ok(seq),
            None => match self.get_agent(agent_id).await? {
                None => Err(format!("no agent '{agent_id}'").into()),
                Some(agent) if agent.retired => {
                    Err(format!("agent '{agent_id}' is retired").into())
                }
                Some(_) => Err(format!("agent '{agent_id}' already has a turn in flight").into()),
            },
        }
    }

    /// `queued -> running`, exactly once. Returning false means someone
    /// else already ran (or is running) this turn, and the caller must
    /// not run it again; this is what makes redelivery safe.
    pub async fn claim_turn(&self, agent_id: &str, seq: i64) -> Result<bool, FlatError> {
        let done = sqlx::query(
            "UPDATE turns SET state = 'running'
             WHERE agent_id = ?1 AND seq = ?2 AND state = 'queued'",
        )
        .bind(agent_id)
        .bind(seq)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Record a finished exchange, including the one thing resume needs:
    /// the session id, written to the agent the moment it is known.
    ///
    /// One transaction, and the agent is touched only if this call is
    /// the one that settled the turn. A turn already killed or failed
    /// must not advance the session or bill the agent, and a crash
    /// cannot land between the two writes. Returns whether it recorded.
    pub async fn complete_turn(
        &self,
        agent_id: &str,
        seq: i64,
        exchange: &Exchange,
    ) -> Result<bool, FlatError> {
        let mut tx = self.pool.begin().await?;
        let done = sqlx::query(
            "UPDATE turns SET state = 'ok', reply = ?3, cost_micro_usd = ?4, elapsed_ms = ?5,
                 tokens_in = ?6, tokens_out = ?7, tokens_cached = ?8
             WHERE agent_id = ?1 AND seq = ?2 AND state = 'running'",
        )
        .bind(agent_id)
        .bind(seq)
        .bind(&exchange.reply)
        .bind(exchange.cost_micro_usd as i64)
        .bind(exchange.elapsed_ms as i64)
        .bind(exchange.tokens_in as i64)
        .bind(exchange.tokens_out as i64)
        .bind(exchange.tokens_cached as i64)
        .execute(&mut *tx)
        .await?;
        let recorded = done.rows_affected() == 1;
        if recorded {
            // session_started_seq maintains itself: a session id that
            // differs from the stored one means this turn opened a new
            // session, so rotation needs no flag threaded through.
            sqlx::query(
                "UPDATE agents SET session = ?2, cost_micro_usd = cost_micro_usd + ?3,
                     session_started_seq = CASE
                         WHEN session IS NULL OR session <> ?2 THEN ?4
                         ELSE session_started_seq END
                 WHERE agent_id = ?1",
            )
            .bind(agent_id)
            .bind(&exchange.session)
            .bind(exchange.cost_micro_usd as i64)
            .bind(seq)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(recorded)
    }

    /// Mark a turn failed or killed. Touches nothing if the turn already
    /// completed, so a late kill cannot overwrite a result. A failed
    /// exchange can still have cost money and learned a session id; both
    /// are recorded on the agent so spend is never under-reported and a
    /// half-finished conversation stays resumable.
    pub async fn fail_turn(
        &self,
        agent_id: &str,
        seq: i64,
        state: &str,
        error: &str,
        cost_micro_usd: i64,
        session: Option<&str>,
    ) -> Result<bool, FlatError> {
        let mut tx = self.pool.begin().await?;
        let done = sqlx::query(
            "UPDATE turns SET state = ?3, error = ?4, cost_micro_usd = ?5
             WHERE agent_id = ?1 AND seq = ?2 AND state IN ('queued', 'running')",
        )
        .bind(agent_id)
        .bind(seq)
        .bind(state)
        .bind(error)
        .bind(cost_micro_usd)
        .execute(&mut *tx)
        .await?;
        let recorded = done.rows_affected() == 1;
        if recorded && (cost_micro_usd > 0 || session.is_some()) {
            sqlx::query(
                "UPDATE agents SET session = COALESCE(?2, session),
                     cost_micro_usd = cost_micro_usd + ?3
                 WHERE agent_id = ?1",
            )
            .bind(agent_id)
            .bind(session)
            .bind(cost_micro_usd)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(recorded)
    }

    pub async fn get_turn(&self, agent_id: &str, seq: i64) -> Result<Option<TurnRow>, FlatError> {
        let row: Option<TurnTuple> =
            sqlx::query_as(&format!("{TURN_SELECT} WHERE agent_id = ?1 AND seq = ?2"))
                .bind(agent_id)
                .bind(seq)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(turn_row))
    }

    pub async fn conversation(&self, agent_id: &str) -> Result<Vec<TurnRow>, FlatError> {
        let rows: Vec<TurnTuple> =
            sqlx::query_as(&format!("{TURN_SELECT} WHERE agent_id = ?1 ORDER BY seq"))
                .bind(agent_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(turn_row).collect())
    }

    /// Turns in a given state, across all agents. This is the whole
    /// recovery API: after a restart, `queued` turns are resubmitted and
    /// `running` turns are the crashed ones to adjudicate.
    pub async fn turns_in_state(&self, state: &str) -> Result<Vec<TurnRow>, FlatError> {
        let rows: Vec<TurnTuple> = sqlx::query_as(&format!(
            "{TURN_SELECT} WHERE state = ?1 ORDER BY agent_id, seq"
        ))
        .bind(state)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(turn_row).collect())
    }
}
