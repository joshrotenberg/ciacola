//! The product ledger: agents and turns, in sqlite, beside no queue.
//!
//! The rule this module lives by: the application's own record is the
//! source of truth, and anything a turn *learns* (the session id above
//! all) is written here the moment it is known. A queue-shaped design
//! needs this ledger *in addition to* its queue; ciacola needs only
//! this, which is why there is no queue at the centre.
//!
//! Turn states: `queued -> running -> ok | failed | killed`. An agent's
//! state is derived from its turns, never stored, so it cannot drift.

use serde::Serialize;
use sqlx::SqlitePool;

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
}

/// A session id we choose, rather than one we learn.
///
/// `--session-id` wants a UUID and ciacola already depends on `ulid`,
/// which is also 128 bits, so the bytes are reshaped rather than adding
/// a crate for it. Version and variant nibbles are set so the result is
/// a well-formed v4 and nothing downstream has to be lenient.
pub fn new_session_id() -> String {
    let mut b = ulid::Ulid::new().to_bytes();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        h(&b[0..4]),
        h(&b[4..6]),
        h(&b[6..8]),
        h(&b[8..10]),
        h(&b[10..16])
    )
}

impl Ledger {
    /// Point an agent at a session it has not used yet.
    ///
    /// `session_started_seq` goes back to 0, which is what marks the id
    /// as assigned-but-unopened. Called at creation and again on
    /// rotation, and in both cases the write lands *before* the provider
    /// runs, which is the whole point: a turn that dies mid-flight
    /// leaves an id behind that recovery can actually resume.
    pub async fn assign_session(&self, agent_id: &str, session: &str) -> Result<(), FlatError> {
        sqlx::query("UPDATE agents SET session = ?2, session_started_seq = 0 WHERE agent_id = ?1")
            .bind(agent_id)
            .bind(session)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

impl Ledger {
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
        sqlx::query(
            "INSERT INTO agents (agent_id, name, def, spawned_by, session) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&agent_id)
        .bind(&def.name)
        .bind(serde_json::to_string(&def)?)
        .bind(spawned_by)
        .bind(new_session_id())
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
            // session_started_seq marks where the current session
            // began, and rotation measures from it. Two ways in: the id
            // changed (a session we did not assign), or the id was
            // assigned and this is the turn that opened it, which is
            // what `session_started_seq = 0` means. Without the second
            // clause an assigned id never records its start, because it
            // never differs, and rotation then counts from 0 and fires
            // one turn early on every agent for the rest of its life.
            sqlx::query(
                "UPDATE agents SET session = ?2, cost_micro_usd = cost_micro_usd + ?3,
                     session_started_seq = CASE
                         WHEN session IS NULL OR session <> ?2
                              OR session_started_seq = 0 THEN ?4
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
    /// Settle a turn that did not succeed.
    ///
    /// `elapsed_ms` is wall clock for the attempt, which is measurable
    /// whatever went wrong and used to be discarded: a five minute
    /// failure read as 0ms, so the runs most worth investigating looked
    /// like the cheapest ones on the board.
    // Eight positional arguments is past where this stays readable.
    // Left alone rather than wrapped in a struct because every caller
    // is in this crate and the next change here is likely a
    // claimed-at column, which is the thing that would make a struct
    // worth it.
    #[allow(clippy::too_many_arguments)]
    pub async fn fail_turn(
        &self,
        agent_id: &str,
        seq: i64,
        state: &str,
        error: &str,
        cost_micro_usd: i64,
        elapsed_ms: i64,
        session: Option<&str>,
    ) -> Result<bool, FlatError> {
        let mut tx = self.pool.begin().await?;
        let done = sqlx::query(
            "UPDATE turns SET state = ?3, error = ?4, cost_micro_usd = ?5,
                 elapsed_ms = ?6
             WHERE agent_id = ?1 AND seq = ?2 AND state IN ('queued', 'running')",
        )
        .bind(agent_id)
        .bind(seq)
        .bind(state)
        .bind(error)
        .bind(cost_micro_usd)
        .bind(elapsed_ms)
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

#[cfg(test)]
mod session_tests {
    use super::*;
    use crate::agent::{AgentDef, Exchange};

    async fn ledger() -> Ledger {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        Ledger::setup(pool).await.expect("ledger")
    }

    fn exchange(session: &str) -> Exchange {
        Exchange {
            reply: "ok".into(),
            session: session.into(),
            cost_micro_usd: 1,
            tokens_in: 1,
            tokens_out: 1,
            tokens_cached: 0,
            num_turns: 1,
            elapsed_ms: 1,
            error: None,
        }
    }

    /// The point of the whole change: an agent has a resumable id
    /// before it has ever run, so a crash mid-turn cannot lose it.
    #[tokio::test]
    async fn an_agent_has_a_session_before_its_first_turn() {
        let l = ledger().await;
        let id = l
            .create_agent(&AgentDef::new("a", "sys"), None)
            .await
            .expect("create");
        let a = l.get_agent(&id).await.expect("get").expect("some");
        assert!(a.session.is_some(), "no session assigned at creation");
        assert_eq!(
            a.session_started_seq, 0,
            "assigned but unopened must read as 0"
        );
    }

    #[test]
    fn assigned_ids_are_well_formed_uuids_and_distinct() {
        let (a, b) = (new_session_id(), new_session_id());
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        let parts: Vec<_> = a.split('-').map(str::len).collect();
        assert_eq!(parts, vec![8, 4, 4, 4, 12]);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_eq!(&a[14..15], "4", "version nibble");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"), "variant");
    }

    /// The regression this change could most easily have introduced.
    /// `session_started_seq` used to be set only when the id *changed*,
    /// which never happens once we assign it, so it would have stayed 0
    /// and rotation would measure from 0 forever.
    #[tokio::test]
    async fn first_turn_records_where_the_session_started() {
        let l = ledger().await;
        let id = l
            .create_agent(&AgentDef::new("a", "sys"), None)
            .await
            .expect("create");
        let assigned = l
            .get_agent(&id)
            .await
            .expect("get")
            .expect("some")
            .session
            .expect("assigned");

        l.enqueue_turn(&id, "hi").await.expect("enqueue");
        assert!(l.claim_turn(&id, 1).await.expect("claim"));
        l.complete_turn(&id, 1, &exchange(&assigned))
            .await
            .expect("complete");

        let a = l.get_agent(&id).await.expect("get").expect("some");
        assert_eq!(a.session.as_deref(), Some(assigned.as_str()));
        assert_eq!(
            a.session_started_seq, 1,
            "an assigned session must record its start, or rotation counts from zero"
        );
    }

    /// The arithmetic that depends on the above. `in_session` is
    /// `seq - session_started_seq`, so a session_started_seq left at 0
    /// makes turn 2 look like 2 turns in and fires rotation early.
    #[tokio::test]
    async fn turns_in_session_counts_from_the_opening_turn() {
        let l = ledger().await;
        let id = l
            .create_agent(&AgentDef::new("a", "sys"), None)
            .await
            .expect("create");
        let assigned = l
            .get_agent(&id)
            .await
            .expect("get")
            .expect("some")
            .session
            .expect("assigned");
        l.enqueue_turn(&id, "one").await.expect("enqueue");
        l.claim_turn(&id, 1).await.expect("claim");
        l.complete_turn(&id, 1, &exchange(&assigned))
            .await
            .expect("complete");

        let a = l.get_agent(&id).await.expect("get").expect("some");
        assert_eq!(2 - a.session_started_seq, 1, "turn 2 is one turn in");
    }

    /// Rotation assigns the next id up front for the same reason
    /// creation does, and resets the marker so the new session records
    /// its own start.
    #[tokio::test]
    async fn assign_session_replaces_the_id_and_reopens_it() {
        let l = ledger().await;
        let id = l
            .create_agent(&AgentDef::new("a", "sys"), None)
            .await
            .expect("create");
        let first = l
            .get_agent(&id)
            .await
            .expect("get")
            .expect("some")
            .session
            .expect("assigned");
        l.enqueue_turn(&id, "one").await.expect("enqueue");
        l.claim_turn(&id, 1).await.expect("claim");
        l.complete_turn(&id, 1, &exchange(&first))
            .await
            .expect("complete");

        let next = new_session_id();
        l.assign_session(&id, &next).await.expect("assign");

        let a = l.get_agent(&id).await.expect("get").expect("some");
        assert_eq!(a.session.as_deref(), Some(next.as_str()));
        assert_ne!(a.session.as_deref(), Some(first.as_str()));
        assert_eq!(a.session_started_seq, 0, "a rotated session is unopened");
    }

    /// The failure this issue was filed for: a turn that dies leaves
    /// the agent resumable rather than blank.
    #[tokio::test]
    async fn an_orphaned_turn_leaves_the_session_behind() {
        let l = ledger().await;
        let id = l
            .create_agent(&AgentDef::new("a", "sys"), None)
            .await
            .expect("create");
        let assigned = l
            .get_agent(&id)
            .await
            .expect("get")
            .expect("some")
            .session
            .expect("assigned");

        // Claimed and then abandoned, exactly as a killed server does.
        l.enqueue_turn(&id, "hi").await.expect("enqueue");
        l.claim_turn(&id, 1).await.expect("claim");
        l.fail_turn(&id, 1, "failed", "orphaned by server crash", 0, 0, None)
            .await
            .expect("fail");

        let a = l.get_agent(&id).await.expect("get").expect("some");
        assert_eq!(
            a.session.as_deref(),
            Some(assigned.as_str()),
            "the session must survive an orphaned turn"
        );
    }
}
