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
    /// The backends this server was built with, by key.
    ///
    /// Held here because the ledger already reaches every path that runs
    /// or recovers a turn, and threading a second handle through all of
    /// them would be churn for its own sake. Core never constructs an
    /// adapter; the binary assembles this and hands it over, which is
    /// what keeps core free of any wrapper dependency.
    providers: ciacola_agent::ProviderRegistry,
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
    /// `reported`, `unreported`, `not_priced`, or `legacy` for rows
    /// written before the provider contract.
    pub cost_state: String,
    pub elapsed_ms: i64,
    /// `measured`, `upper_bound`, `unknown`, `not_attempted`, or
    /// `legacy` for rows written before elapsed provenance was tracked.
    pub elapsed_state: String,
    /// Wall-clock milliseconds when `queued -> running` won.
    ///
    /// `None` is a legacy row that predates durable claim timing, not a
    /// claim at the epoch. Queued turns also have no claim time.
    pub claimed_unix_ms: Option<i64>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub tokens_cached: i64,
    /// `reported`, `unreported`, `not_tracked`, or `legacy`.
    pub usage_state: String,
    pub provider_turns: Option<i64>,
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
    String,
    i64,
    String,
    Option<i64>,
    i64,
    i64,
    i64,
    String,
    Option<i64>,
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
    SELECT agent_id, seq, prompt, state, reply, error, cost_micro_usd, cost_state,
           elapsed_ms, elapsed_state, claimed_unix_ms, tokens_in, tokens_out,
           tokens_cached, usage_state, provider_turns
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
        cost_state,
        elapsed_ms,
        elapsed_state,
        claimed_unix_ms,
        tokens_in,
        tokens_out,
        tokens_cached,
        usage_state,
        provider_turns,
    ) = t;
    TurnRow {
        agent_id,
        seq,
        prompt,
        state,
        reply,
        error,
        cost_micro_usd,
        cost_state,
        elapsed_ms,
        elapsed_state,
        claimed_unix_ms,
        tokens_in,
        tokens_out,
        tokens_cached,
        usage_state,
        provider_turns,
    }
}

fn cost_state(cost: ciacola_agent::Cost) -> &'static str {
    match cost {
        ciacola_agent::Cost::Reported { .. } => "reported",
        ciacola_agent::Cost::Unreported => "unreported",
        ciacola_agent::Cost::NotPriced => "not_priced",
    }
}

fn usage_state(usage: ciacola_agent::Usage) -> &'static str {
    match usage {
        ciacola_agent::Usage::Reported(_) => "reported",
        ciacola_agent::Usage::Unreported => "unreported",
        ciacola_agent::Usage::NotTracked => "not_tracked",
    }
}

impl TurnRow {
    /// A monetary value known to be a measurement.
    ///
    /// A positive legacy value is unambiguous enough to preserve as a
    /// historical sample. Legacy zero is the old conflation this method
    /// exists to avoid and therefore remains unknown.
    pub fn reported_cost_micro_usd(&self) -> Option<i64> {
        (self.cost_state == "reported" || (self.cost_state == "legacy" && self.cost_micro_usd != 0))
            .then_some(self.cost_micro_usd)
    }

    /// Token buckets known to be measurements.
    ///
    /// As with cost, nonzero legacy buckets are useful history while an
    /// all-zero legacy row cannot say whether the provider measured zero
    /// or reported nothing.
    pub fn reported_tokens(&self) -> Option<(i64, i64, i64)> {
        (self.usage_state == "reported"
            || (self.usage_state == "legacy"
                && (self.tokens_in != 0 || self.tokens_out != 0 || self.tokens_cached != 0)))
            .then_some((self.tokens_in, self.tokens_out, self.tokens_cached))
    }
}

#[derive(Clone, Copy)]
enum InterruptionTiming {
    Live,
    Recovery,
}

impl InterruptionTiming {
    const fn as_sql(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Recovery => "recovery",
        }
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
            Migration::new(
                "0009_agents_token",
                "ALTER TABLE agents ADD COLUMN token TEXT",
            ),
            Migration::new(
                "0010_agents_token_index",
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_agents_token ON agents(token)",
            ),
            Migration::add_column(
                "0011_turns_cost_state",
                "ALTER TABLE turns ADD COLUMN cost_state TEXT NOT NULL DEFAULT 'legacy'",
            ),
            Migration::add_column(
                "0012_turns_usage_state",
                "ALTER TABLE turns ADD COLUMN usage_state TEXT NOT NULL DEFAULT 'legacy'",
            ),
            Migration::add_column(
                "0013_turns_provider_turns",
                "ALTER TABLE turns ADD COLUMN provider_turns INTEGER",
            ),
            Migration::add_column(
                "0014_turns_claimed_unix_ms",
                "ALTER TABLE turns ADD COLUMN claimed_unix_ms INTEGER",
            ),
            Migration::add_column(
                "0015_turns_elapsed_state",
                "ALTER TABLE turns ADD COLUMN elapsed_state TEXT NOT NULL DEFAULT 'legacy'",
            ),
        ];
        crate::plugin::apply_migrations(&pool, "core", MIGRATIONS).await?;
        let ledger = Self {
            pool,
            runtime: Default::default(),
            house_rules: None,
            providers: Default::default(),
        };
        ledger.backfill_tokens().await?;
        Ok(ledger)
    }

    /// Agents that predate identity get a token at the next boot.
    ///
    /// Sqlite cannot mint one per row in the migration itself, and an
    /// agent without a token cannot be recognised on the loopback: its
    /// calls would arrive anonymous forever, which is the state this
    /// whole mechanism exists to end.
    async fn backfill_tokens(&self) -> Result<(), FlatError> {
        let missing: Vec<(String,)> =
            sqlx::query_as("SELECT agent_id FROM agents WHERE token IS NULL")
                .fetch_all(&self.pool)
                .await?;
        for (agent_id,) in missing {
            sqlx::query("UPDATE agents SET token = ?2 WHERE agent_id = ?1")
                .bind(&agent_id)
                .bind(new_token())
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// Attach the server-wide defaults. Called once at boot, before any
    /// agent exists.
    pub fn with_runtime(mut self, runtime: crate::roles::Runtime) -> Result<Self, FlatError> {
        self.house_rules = runtime.resolved_house_rules()?;
        self.runtime = runtime;
        Ok(self)
    }

    /// Attach the backends this build was assembled with. Called once at
    /// boot by the binary, which is the only place that knows which
    /// adapter crates were linked in.
    pub fn with_providers(mut self, providers: ciacola_agent::ProviderRegistry) -> Self {
        self.providers = providers;
        self
    }

    /// The registered backends. An empty one cannot run a turn, and
    /// every attempt says so by name through
    /// [`ciacola_agent::AgentError::UnknownProvider`].
    pub fn providers(&self) -> &ciacola_agent::ProviderRegistry {
        &self.providers
    }

    /// The honest terminal accounting when a provider attempt ends
    /// without a result event.
    ///
    /// A backend that never prices or counts says so; one that normally
    /// reports a bucket but could not because it was killed says the
    /// bucket is unreported. If the configured backend is unavailable,
    /// neither claim can be made and both remain unreported.
    async fn unavailable_telemetry(
        &self,
        agent_id: &str,
    ) -> Result<(ciacola_agent::Cost, ciacola_agent::Usage), FlatError> {
        let capabilities = self
            .get_agent(agent_id)
            .await?
            .and_then(|agent| self.providers.get(&agent.def.provider).ok())
            .map(|provider| provider.capabilities());
        let cost = match capabilities.as_ref() {
            Some(capabilities) if !capabilities.reports_cost => ciacola_agent::Cost::NotPriced,
            _ => ciacola_agent::Cost::Unreported,
        };
        let usage = match capabilities.as_ref() {
            Some(capabilities) if !capabilities.reports_token_usage => {
                ciacola_agent::Usage::NotTracked
            }
            _ => ciacola_agent::Usage::Unreported,
        };
        Ok((cost, usage))
    }
}

/// A per-agent secret for the loopback.
///
/// Two ulids side by side: 160 bits of randomness against a loopback
/// listener on a laptop, which is plenty, without adding a crate for
/// it. It is a bearer secret, so it lives in the agents table and in
/// the one MCP config file written for its agent, and deliberately
/// never on [`AgentRow`]: what is not on the row cannot be serialized
/// into a tool result or a board page by accident.
pub fn new_token() -> String {
    format!("{}{}", ulid::Ulid::new(), ulid::Ulid::new()).to_lowercase()
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
    /// The caller behind a loopback token, if the token is real.
    ///
    /// This is the whole of authentication: the transport layer maps a
    /// header to an agent id with this, and everything downstream
    /// trusts the id because it was derived rather than claimed.
    pub async fn agent_id_by_token(&self, token: &str) -> Result<Option<String>, FlatError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT agent_id FROM agents WHERE token = ?1 AND retired = 0")
                .bind(token)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(id,)| id))
    }

    /// An agent's own token, for writing its MCP config. Deliberately a
    /// separate query rather than a field on [`AgentRow`]; see
    /// [`new_token`].
    pub async fn token_of(&self, agent_id: &str) -> Result<Option<String>, FlatError> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT token FROM agents WHERE agent_id = ?1")
                .bind(agent_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(t,)| t))
    }

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

    /// Persist an id the provider has confirmed is open during `seq`.
    /// Unlike [`assign_session`](Self::assign_session), this must not
    /// reset an already-open session's rotation origin to zero.
    pub async fn record_provider_session(
        &self,
        agent_id: &str,
        seq: i64,
        session: &str,
    ) -> Result<(), FlatError> {
        sqlx::query(
            "UPDATE agents SET session = ?3,
                 session_started_seq = CASE
                     WHEN session IS NULL OR session <> ?3 OR session_started_seq = 0
                         THEN ?2
                     ELSE session_started_seq END
             WHERE agent_id = ?1",
        )
        .bind(agent_id)
        .bind(seq)
        .bind(session)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Replace this turn's best-known cumulative token snapshot.
    ///
    /// Providers emit totals, never deltas, so replacement is both
    /// idempotent and immune to retry double-counting. A snapshot queued
    /// just before cancellation may reach SQLite after the kill update;
    /// claimed killed rows remain writable for exactly that drain window.
    /// A queued kill has no claim and can never be turned into a provider
    /// run by a stray event.
    pub async fn record_usage_snapshot(
        &self,
        agent_id: &str,
        seq: i64,
        usage: ciacola_agent::TokenUsage,
    ) -> Result<bool, FlatError> {
        let done = sqlx::query(
            "UPDATE turns SET tokens_in = ?3, tokens_out = ?4, tokens_cached = ?5,
                 usage_state = 'reported'
             WHERE agent_id = ?1 AND seq = ?2
               AND (state = 'running'
                    OR (state = 'killed' AND claimed_unix_ms IS NOT NULL))",
        )
        .bind(agent_id)
        .bind(seq)
        .bind(usage.input as i64)
        .bind(usage.output as i64)
        .bind(usage.cached_input as i64)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }
}

impl Ledger {
    /// Stamp server-wide defaults onto a definition at the storage
    /// boundary. Both creation and replacement pass here: config upsert
    /// and `spawn_role` replace a definition after the id is known, and a
    /// create-only guard let those writes erase isolation and credentials.
    fn normalized_def(&self, def: &AgentDef) -> AgentDef {
        let mut def = def.clone();
        def.resolve_provider(self.runtime.default_provider_key());
        if def.hermetic.is_none() {
            def.hermetic = self.runtime.hermetic.clone();
        }
        if def.sandbox.is_none() {
            def.sandbox = self.runtime.sandbox.clone();
        }
        let (home, token_env) = if def.provider.as_str() == "codex" {
            (&self.runtime.codex_home, &self.runtime.codex_token_env)
        } else {
            (&self.runtime.claude_home, &self.runtime.token_env)
        };
        if def.config_home.is_none() {
            def.config_home = home.clone();
        }
        if def.house_rules.is_none() {
            def.house_rules = self.house_rules.clone();
        }
        if def.token_env.is_none() {
            def.token_env = token_env.clone();
        }
        def
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
        let def = self.normalized_def(def);
        sqlx::query(
            "INSERT INTO agents (agent_id, name, def, spawned_by, session, token) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&agent_id)
        .bind(&def.name)
        .bind(serde_json::to_string(&def)?)
        .bind(spawned_by)
        .bind(new_session_id())
        .bind(new_token())
        .execute(&self.pool)
        .await?;
        Ok(agent_id)
    }

    /// Replace an agent's definition, keeping its identity, session,
    /// and history. This is what makes config-born agents updatable:
    /// the def follows the config file; the conversation persists.
    pub async fn update_agent_def(&self, agent_id: &str, def: &AgentDef) -> Result<(), FlatError> {
        let def = self.normalized_def(def);
        if let Some(existing) = self.get_agent(agent_id).await?
            && existing.turns > 0
            && existing.def.provider != def.provider
        {
            return Err(format!(
                "agent '{agent_id}' has {} recorded turn(s) with provider '{}'; refusing to move its conversation to '{}'. Retire it and create a new agent instead",
                existing.turns, existing.def.provider, def.provider
            )
            .into());
        }
        sqlx::query("UPDATE agents SET name = ?2, def = ?3 WHERE agent_id = ?1")
            .bind(agent_id)
            .bind(&def.name)
            .bind(serde_json::to_string(&def)?)
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
    /// not run it again; this is what makes redelivery safe. The claim
    /// time is part of the same write so a kill can never observe
    /// `running` without the clock it needs to settle elapsed time.
    pub async fn claim_turn(&self, agent_id: &str, seq: i64) -> Result<bool, FlatError> {
        let done = sqlx::query(
            "UPDATE turns SET state = 'running', claimed_unix_ms = ?3
             WHERE agent_id = ?1 AND seq = ?2 AND state = 'queued'",
        )
        .bind(agent_id)
        .bind(seq)
        .bind(crate::time::now_unix_ms())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Settle a queued or running turn from outside its provider future.
    ///
    /// This is the convergence point for an operator kill and restart
    /// recovery. The terminal transition and elapsed calculation are a
    /// single UPDATE: if a claim races the interruption, either the
    /// queued row is settled first as a known zero-cost non-attempt, or
    /// the claim lands first with a timestamp the interruption measures.
    /// There is no read gap in which a running attempt can become a
    /// false `0ms` provider run.
    ///
    /// A queued turn has known zero cost and usage because the provider
    /// was never touched. A running turn uses the selected provider's
    /// declared reporting capabilities. Numeric zeros remain storage
    /// defaults; their state columns say whether zero was measured,
    /// unavailable, or a bucket the provider never tracks. A legacy
    /// running row without a durable claim clock retains its old
    /// elapsed value with `unknown` provenance.
    pub async fn interrupt_turn(
        &self,
        agent_id: &str,
        seq: i64,
        state: &str,
        error: &str,
    ) -> Result<bool, FlatError> {
        self.settle_interruption(agent_id, seq, state, error, InterruptionTiming::Live)
            .await
    }

    /// Settle a turn found running during restart recovery.
    ///
    /// The durable claim-to-restart interval contains the provider run,
    /// but may also contain time after the process died. It is therefore
    /// an upper bound, not a measured provider duration. Legacy running
    /// rows without a claim timestamp retain their numeric value but are
    /// explicitly marked unknown.
    pub async fn recover_turn(
        &self,
        agent_id: &str,
        seq: i64,
        error: &str,
    ) -> Result<bool, FlatError> {
        self.settle_interruption(agent_id, seq, "failed", error, InterruptionTiming::Recovery)
            .await
    }

    async fn settle_interruption(
        &self,
        agent_id: &str,
        seq: i64,
        state: &str,
        error: &str,
        timing: InterruptionTiming,
    ) -> Result<bool, FlatError> {
        let (cost, usage) = self.unavailable_telemetry(agent_id).await?;
        let now_ms = crate::time::now_unix_ms();
        let done = sqlx::query(
            "UPDATE turns SET state = ?3, error = ?4,
                 cost_micro_usd = 0,
                 cost_state = CASE WHEN state = 'queued' THEN 'reported' ELSE ?5 END,
                 tokens_in = CASE
                     WHEN state = 'queued' OR usage_state <> 'reported' THEN 0
                     ELSE tokens_in
                 END,
                 tokens_out = CASE
                     WHEN state = 'queued' OR usage_state <> 'reported' THEN 0
                     ELSE tokens_out
                 END,
                 tokens_cached = CASE
                     WHEN state = 'queued' OR usage_state <> 'reported' THEN 0
                     ELSE tokens_cached
                 END,
                 usage_state = CASE
                     WHEN state = 'queued' THEN 'reported'
                     WHEN usage_state = 'reported' THEN 'reported'
                     ELSE ?6
                 END,
                 provider_turns = CASE WHEN state = 'queued' THEN 0 ELSE NULL END,
                 elapsed_state = CASE
                     WHEN state = 'queued' THEN 'not_attempted'
                     WHEN ?8 = 'recovery' AND claimed_unix_ms IS NOT NULL THEN 'upper_bound'
                     WHEN ?8 = 'recovery' THEN 'unknown'
                     WHEN claimed_unix_ms IS NULL THEN 'unknown'
                     ELSE 'measured'
                 END,
                 elapsed_ms = MAX(
                     elapsed_ms,
                     CASE
                         WHEN state = 'running' AND claimed_unix_ms IS NOT NULL
                             THEN MAX(?7 - claimed_unix_ms, 0)
                         ELSE 0
                     END)
             WHERE agent_id = ?1 AND seq = ?2 AND state IN ('queued', 'running')
               AND (?8 = 'live' OR state = 'running')",
        )
        .bind(agent_id)
        .bind(seq)
        .bind(state)
        .bind(error)
        .bind(cost_state(cost))
        .bind(usage_state(usage))
        .bind(now_ms)
        .bind(timing.as_sql())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Fail a claimed dispatch before its provider future was touched.
    ///
    /// This is narrower than interruption: it only accepts `running`,
    /// and records the known fact that no provider attempt, spend, usage,
    /// or runtime occurred. It is used when installing or validating the
    /// process-local kill registration itself fails.
    pub async fn abort_claimed_turn(
        &self,
        agent_id: &str,
        seq: i64,
        error: &str,
    ) -> Result<bool, FlatError> {
        let done = sqlx::query(
            "UPDATE turns SET state = 'failed', error = ?3,
                 cost_micro_usd = 0, cost_state = 'reported',
                 tokens_in = 0, tokens_out = 0, tokens_cached = 0,
                 usage_state = 'reported', provider_turns = 0,
                 elapsed_ms = 0, elapsed_state = 'not_attempted'
             WHERE agent_id = ?1 AND seq = ?2 AND state = 'running'",
        )
        .bind(agent_id)
        .bind(seq)
        .bind(error)
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
        let tokens = exchange.tokens();
        let mut tx = self.pool.begin().await?;
        let done = sqlx::query(
            "UPDATE turns SET state = 'ok', reply = ?3, cost_micro_usd = ?4,
                 cost_state = ?5, elapsed_ms = ?6, elapsed_state = 'measured',
                 tokens_in = CASE
                     WHEN ?10 = 'reported' OR usage_state <> 'reported' THEN ?7
                     ELSE tokens_in
                 END,
                 tokens_out = CASE
                     WHEN ?10 = 'reported' OR usage_state <> 'reported' THEN ?8
                     ELSE tokens_out
                 END,
                 tokens_cached = CASE
                     WHEN ?10 = 'reported' OR usage_state <> 'reported' THEN ?9
                     ELSE tokens_cached
                 END,
                 usage_state = CASE
                     WHEN ?10 = 'reported' OR usage_state <> 'reported' THEN ?10
                     ELSE usage_state
                 END,
                 provider_turns = ?11
             WHERE agent_id = ?1 AND seq = ?2 AND state = 'running'",
        )
        .bind(agent_id)
        .bind(seq)
        .bind(&exchange.reply)
        .bind(exchange.cost_micro_usd() as i64)
        .bind(cost_state(exchange.cost))
        .bind(exchange.elapsed_ms as i64)
        .bind(tokens.input as i64)
        .bind(tokens.output as i64)
        .bind(tokens.cached_input as i64)
        .bind(usage_state(exchange.usage))
        .bind(exchange.provider_turns.map(i64::from))
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
                "UPDATE agents SET session = COALESCE(?2, session),
                     cost_micro_usd = cost_micro_usd + ?3,
                     session_started_seq = CASE
                         WHEN ?2 IS NOT NULL
                              AND (session IS NULL OR session <> ?2
                                   OR session_started_seq = 0) THEN ?4
                         ELSE session_started_seq END
                 WHERE agent_id = ?1",
            )
            .bind(agent_id)
            .bind(exchange.session.as_deref())
            .bind(exchange.cost_micro_usd() as i64)
            .bind(seq)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(recorded)
    }

    /// Settle a turn that did not succeed.
    ///
    /// Touches nothing if the turn already completed. A failed exchange
    /// can still have cost money and learned a session id; both are
    /// recorded on the agent so spend is never under-reported and a
    /// half-finished conversation stays resumable.
    ///
    /// For a running attempt, `elapsed_ms` is measured wall clock. That
    /// used to be discarded on failure: a five minute failure read as
    /// 0ms, so the runs most worth investigating looked like the cheapest
    /// ones on the board. A queued row is explicitly `not_attempted`.
    // This compatibility path still accepts scalar telemetry. New provider
    // execution paths should prefer `fail_exchange`, which preserves the
    // provider's typed accounting states.
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
        let (unavailable_cost, unavailable_usage) = self.unavailable_telemetry(agent_id).await?;
        let cost = if cost_micro_usd > 0 {
            ciacola_agent::Cost::Reported {
                micro_usd: cost_micro_usd as u64,
            }
        } else {
            unavailable_cost
        };
        let mut tx = self.pool.begin().await?;
        let done = sqlx::query(
            "UPDATE turns SET state = ?3, error = ?4, cost_micro_usd = ?5,
                 cost_state = CASE
                     WHEN state = 'queued' AND ?5 = 0 THEN 'reported'
                     ELSE ?6
                 END,
                 elapsed_ms = ?7,
                 elapsed_state = CASE
                     WHEN state = 'queued' THEN 'not_attempted'
                     ELSE 'measured'
                 END,
                 tokens_in = CASE WHEN state = 'queued' THEN 0 ELSE tokens_in END,
                 tokens_out = CASE WHEN state = 'queued' THEN 0 ELSE tokens_out END,
                 tokens_cached = CASE WHEN state = 'queued' THEN 0 ELSE tokens_cached END,
                 usage_state = CASE
                     WHEN state = 'queued' THEN 'reported'
                     WHEN usage_state = 'reported' THEN 'reported'
                     ELSE ?8
                 END,
                 provider_turns = CASE WHEN state = 'queued' THEN 0 ELSE provider_turns END
             WHERE agent_id = ?1 AND seq = ?2 AND state IN ('queued', 'running')",
        )
        .bind(agent_id)
        .bind(seq)
        .bind(state)
        .bind(error)
        .bind(cost_micro_usd)
        .bind(cost_state(cost))
        .bind(elapsed_ms)
        .bind(usage_state(unavailable_usage))
        .execute(&mut *tx)
        .await?;
        let recorded = done.rows_affected() == 1;
        if recorded && (cost_micro_usd > 0 || session.is_some()) {
            sqlx::query(
                "UPDATE agents SET session = COALESCE(?2, session),
                     cost_micro_usd = cost_micro_usd + ?3,
                     session_started_seq = CASE
                         WHEN ?2 IS NOT NULL
                              AND (session IS NULL OR session <> ?2
                                   OR session_started_seq = 0) THEN ?4
                         ELSE session_started_seq END
                 WHERE agent_id = ?1",
            )
            .bind(agent_id)
            .bind(session)
            .bind(cost_micro_usd)
            .bind(seq)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(recorded)
    }

    /// Settle a provider turn that ran and returned portable telemetry.
    /// Unlike [`fail_turn`](Self::fail_turn), this preserves usage state,
    /// cached tokens, and provider-turn counts instead of reducing the
    /// result to cost and elapsed time.
    pub async fn fail_exchange(
        &self,
        agent_id: &str,
        seq: i64,
        state: &str,
        error: &str,
        exchange: &Exchange,
    ) -> Result<bool, FlatError> {
        let tokens = exchange.tokens();
        let cost = exchange.cost_micro_usd() as i64;
        let mut tx = self.pool.begin().await?;
        let done = sqlx::query(
            "UPDATE turns SET state = ?3, error = ?4, cost_micro_usd = ?5,
                 cost_state = ?6, elapsed_ms = ?7, elapsed_state = 'measured',
                 tokens_in = CASE
                     WHEN ?11 = 'reported' OR usage_state <> 'reported' THEN ?8
                     ELSE tokens_in
                 END,
                 tokens_out = CASE
                     WHEN ?11 = 'reported' OR usage_state <> 'reported' THEN ?9
                     ELSE tokens_out
                 END,
                 tokens_cached = CASE
                     WHEN ?11 = 'reported' OR usage_state <> 'reported' THEN ?10
                     ELSE tokens_cached
                 END,
                 usage_state = CASE
                     WHEN ?11 = 'reported' OR usage_state <> 'reported' THEN ?11
                     ELSE usage_state
                 END,
                 provider_turns = ?12
             WHERE agent_id = ?1 AND seq = ?2 AND state IN ('queued', 'running')",
        )
        .bind(agent_id)
        .bind(seq)
        .bind(state)
        .bind(error)
        .bind(cost)
        .bind(cost_state(exchange.cost))
        .bind(exchange.elapsed_ms as i64)
        .bind(tokens.input as i64)
        .bind(tokens.output as i64)
        .bind(tokens.cached_input as i64)
        .bind(usage_state(exchange.usage))
        .bind(exchange.provider_turns.map(i64::from))
        .execute(&mut *tx)
        .await?;
        let recorded = done.rows_affected() == 1;
        if recorded && (cost > 0 || exchange.session.is_some()) {
            sqlx::query(
                "UPDATE agents SET session = COALESCE(?2, session),
                     cost_micro_usd = cost_micro_usd + ?3,
                     session_started_seq = CASE
                         WHEN ?2 IS NOT NULL
                              AND (session IS NULL OR session <> ?2
                                   OR session_started_seq = 0) THEN ?4
                         ELSE session_started_seq END
                 WHERE agent_id = ?1",
            )
            .bind(agent_id)
            .bind(exchange.session.as_deref())
            .bind(cost)
            .bind(seq)
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
    use std::sync::Arc;

    async fn ledger() -> Ledger {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        Ledger::setup(pool).await.expect("ledger")
    }

    struct AccountingProvider {
        key: ciacola_agent::ProviderKey,
        reports_cost: bool,
        reports_usage: bool,
    }

    impl ciacola_agent::Provider for AccountingProvider {
        fn key(&self) -> ciacola_agent::ProviderKey {
            self.key.clone()
        }

        fn capabilities(&self) -> ciacola_agent::Capabilities {
            let mut capabilities = ciacola_agent::Capabilities::none(self.key.clone());
            capabilities.reports_cost = self.reports_cost;
            capabilities.reports_token_usage = self.reports_usage;
            capabilities
        }

        fn run<'a>(
            &'a self,
            _intent: &'a ciacola_agent::TurnIntent,
            _events: &'a dyn ciacola_agent::TurnEvents,
        ) -> ciacola_agent::BoxFut<'a, Result<ciacola_agent::TurnOutcome, ciacola_agent::AgentError>>
        {
            Box::pin(async { panic!("accounting-only provider must not run") })
        }

        fn owns_process(&self, _ps_line: &str) -> bool {
            false
        }
    }

    async fn ledger_with_provider(key: &str, reports_cost: bool, reports_usage: bool) -> Ledger {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let providers = ciacola_agent::ProviderRegistry::new()
            .with(Arc::new(AccountingProvider {
                key: ciacola_agent::ProviderKey::new(key),
                reports_cost,
                reports_usage,
            }))
            .expect("provider");
        Ledger::setup(pool)
            .await
            .expect("ledger")
            .with_providers(providers)
    }

    /// Upgrade a real pre-0014 shape rather than manufacturing legacy
    /// values after setup. Existing terminal and in-flight rows keep
    /// nullable claim time and explicit legacy elapsed provenance; a
    /// legacy running row recovered without a claim clock becomes
    /// `unknown`, never a fabricated zero-duration measurement.
    #[tokio::test]
    async fn pre_claim_timestamp_database_upgrades_without_rewriting_history() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        sqlx::query(
            "CREATE TABLE agents (
                 agent_id TEXT PRIMARY KEY, name TEXT NOT NULL, def TEXT NOT NULL,
                 session TEXT, cost_micro_usd INTEGER NOT NULL DEFAULT 0,
                 spawned_by TEXT, retired INTEGER NOT NULL DEFAULT 0,
                 session_started_seq INTEGER NOT NULL DEFAULT 0, token TEXT);
             CREATE UNIQUE INDEX idx_agents_token ON agents(token);
             CREATE TABLE turns (
                 agent_id TEXT NOT NULL, seq INTEGER NOT NULL, prompt TEXT NOT NULL,
                 state TEXT NOT NULL, reply TEXT, error TEXT,
                 cost_micro_usd INTEGER NOT NULL DEFAULT 0,
                 elapsed_ms INTEGER NOT NULL DEFAULT 0,
                 at_unix INTEGER NOT NULL DEFAULT 0,
                 tokens_in INTEGER NOT NULL DEFAULT 0,
                 tokens_out INTEGER NOT NULL DEFAULT 0,
                 tokens_cached INTEGER NOT NULL DEFAULT 0,
                 cost_state TEXT NOT NULL DEFAULT 'legacy',
                 usage_state TEXT NOT NULL DEFAULT 'legacy',
                 provider_turns INTEGER,
                 PRIMARY KEY (agent_id, seq));
             CREATE TABLE schema_migrations (
                 owner TEXT NOT NULL, name TEXT NOT NULL, applied_unix INTEGER NOT NULL,
                 PRIMARY KEY (owner, name));",
        )
        .execute(&pool)
        .await
        .expect("pre-0014 schema");
        for name in [
            "0001_agents_turns",
            "0002_agents_spawned_by",
            "0003_agents_retired",
            "0004_agents_session_started_seq",
            "0005_turns_at_unix",
            "0006_turns_tokens_in",
            "0007_turns_tokens_out",
            "0008_turns_tokens_cached",
            "0009_agents_token",
            "0010_agents_token_index",
            "0011_turns_cost_state",
            "0012_turns_usage_state",
            "0013_turns_provider_turns",
        ] {
            sqlx::query(
                "INSERT INTO schema_migrations (owner, name, applied_unix)
                 VALUES ('core', ?1, 1)",
            )
            .bind(name)
            .execute(&pool)
            .await
            .expect("migration marker");
        }

        let def = serde_json::to_string(&AgentDef::new("legacy", "sys")).expect("definition");
        sqlx::query(
            "INSERT INTO agents (agent_id, name, def, token)
             VALUES ('legacy-agent', 'legacy', ?1, 'legacy-token')",
        )
        .bind(def)
        .execute(&pool)
        .await
        .expect("agent");
        for (seq, state, elapsed_ms) in [
            (1, "ok", 2_000),
            (2, "running", 700),
            (3, "queued", 0),
            (4, "running", 900),
        ] {
            sqlx::query(
                "INSERT INTO turns (agent_id, seq, prompt, state, elapsed_ms, at_unix)
                 VALUES ('legacy-agent', ?1, 'work', ?2, ?3, 1)",
            )
            .bind(seq)
            .bind(state)
            .bind(elapsed_ms)
            .execute(&pool)
            .await
            .expect("turn");
        }

        let ledger = Ledger::setup(pool).await.expect("upgrade");
        for seq in 1..=4 {
            let turn = ledger
                .get_turn("legacy-agent", seq)
                .await
                .expect("turn query")
                .expect("turn");
            assert_eq!(turn.claimed_unix_ms, None);
            assert_eq!(turn.elapsed_state, "legacy");
        }

        assert!(
            ledger
                .recover_turn("legacy-agent", 2, "orphaned by restart")
                .await
                .expect("recover")
        );
        let recovered = ledger
            .get_turn("legacy-agent", 2)
            .await
            .expect("turn query")
            .expect("turn");
        assert_eq!(recovered.state, "failed");
        assert_eq!(recovered.elapsed_ms, 700);
        assert_eq!(recovered.elapsed_state, "unknown");
        assert!(
            ledger
                .interrupt_turn("legacy-agent", 4, "killed", "live kill without claim clock")
                .await
                .expect("interrupt")
        );
        let interrupted = ledger
            .get_turn("legacy-agent", 4)
            .await
            .expect("turn query")
            .expect("turn");
        assert_eq!(interrupted.state, "killed");
        assert_eq!(interrupted.elapsed_ms, 900);
        assert_eq!(interrupted.elapsed_state, "unknown");
        assert_eq!(
            ledger
                .get_turn("legacy-agent", 3)
                .await
                .expect("turn query")
                .expect("turn")
                .state,
            "queued"
        );
    }

    fn exchange(session: &str) -> Exchange {
        Exchange {
            reply: "ok".into(),
            session: Some(session.into()),
            cost: ciacola_agent::Cost::Reported { micro_usd: 1 },
            usage: ciacola_agent::Usage::Reported(ciacola_agent::TokenUsage {
                input: 1,
                output: 1,
                cached_input: 0,
            }),
            provider_turns: Some(1),
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

    /// A provider-reported failure is still an exchange. In particular,
    /// a max-turns result proves the preassigned session now exists at
    /// the provider, so the next turn must resume it rather than trying
    /// to create the same id again.
    #[tokio::test]
    async fn failed_first_exchange_records_where_the_session_started() {
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
        l.fail_turn(
            &id,
            1,
            "failed",
            "hit max turns",
            1_250_000,
            323_000,
            Some(&assigned),
        )
        .await
        .expect("fail");

        let a = l.get_agent(&id).await.expect("get").expect("some");
        assert_eq!(a.session.as_deref(), Some(assigned.as_str()));
        assert_eq!(
            a.session_started_seq, 1,
            "the next turn must use --resume, not reuse --session-id"
        );
        assert_eq!(a.cost_micro_usd, 1_250_000);
        assert_eq!(
            l.get_turn(&id, 1)
                .await
                .expect("turn query")
                .expect("turn")
                .elapsed_state,
            "measured"
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
        assert_eq!(
            a.session_started_seq, 0,
            "a pre-provider failure is not proof that the assigned session opened"
        );
    }

    /// The loopback credential, end to end: minted at birth, resolves
    /// to its agent, stops resolving at retirement, never repeats.
    #[tokio::test]
    async fn tokens_are_minted_resolved_and_die_with_retirement() {
        let l = ledger().await;
        let a = l
            .create_agent(&AgentDef::new("a", "s"), None)
            .await
            .expect("create");
        let b = l
            .create_agent(&AgentDef::new("b", "s"), None)
            .await
            .expect("create");
        let ta = l.token_of(&a).await.expect("q").expect("a has a token");
        let tb = l.token_of(&b).await.expect("q").expect("b has a token");
        assert_ne!(ta, tb);
        assert_eq!(
            l.agent_id_by_token(&ta).await.expect("q").as_deref(),
            Some(a.as_str())
        );
        assert_eq!(l.agent_id_by_token("no-such-token").await.expect("q"), None);

        l.retire_agent(&a).await.expect("retire");
        assert_eq!(
            l.agent_id_by_token(&ta).await.expect("q"),
            None,
            "a retired agent's token must stop authenticating"
        );
    }

    /// Agents that predate the token column get one at boot, because a
    /// tokenless agent would be anonymous on the loopback forever.
    #[tokio::test]
    async fn setup_backfills_agents_that_predate_tokens() {
        let l = ledger().await;
        let a = l
            .create_agent(&AgentDef::new("old", "s"), None)
            .await
            .expect("create");
        sqlx::query("UPDATE agents SET token = NULL WHERE agent_id = ?1")
            .bind(&a)
            .execute(&l.pool)
            .await
            .expect("null out");
        let l2 = Ledger::setup(l.pool.clone()).await.expect("re-setup");
        assert!(
            l2.token_of(&a).await.expect("q").is_some(),
            "boot must mint the missing token"
        );
    }

    /// Config upsert and `spawn_role` replace a definition after creation.
    /// That replacement used to erase the defaults create_agent had just
    /// stamped, reopening ambient config and dropping isolated-home auth.
    #[tokio::test]
    async fn replacing_a_definition_keeps_runtime_defaults() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let runtime = crate::roles::Runtime {
            default_provider: None,
            hermetic: Some("full".into()),
            sandbox: Some("read-only".into()),
            claude_home: Some("/tmp/ciacola-test-home".into()),
            codex_home: Some("/tmp/ciacola-test-codex-home".into()),
            house_rules: Some("keep the rule".into()),
            house_rules_file: None,
            token_env: Some("CIACOLA_TEST_TOKEN".into()),
            codex_token_env: Some("CIACOLA_TEST_CODEX_TOKEN".into()),
        };
        let l = Ledger::setup(pool)
            .await
            .expect("ledger")
            .with_runtime(runtime)
            .expect("runtime");
        let id = l
            .create_agent(&AgentDef::new("before", "s"), None)
            .await
            .expect("create");

        l.update_agent_def(&id, &AgentDef::new("after", "changed"))
            .await
            .expect("replace");
        let def = l.get_agent(&id).await.expect("get").expect("row").def;
        assert_eq!(def.hermetic.as_deref(), Some("full"));
        assert_eq!(def.sandbox.as_deref(), Some("read-only"));
        assert_eq!(def.config_home.as_deref(), Some("/tmp/ciacola-test-home"));
        assert_eq!(def.house_rules.as_deref(), Some("keep the rule"));
        assert_eq!(def.token_env.as_deref(), Some("CIACOLA_TEST_TOKEN"));
    }

    #[tokio::test]
    async fn runtime_default_applies_to_new_definitions_but_not_legacy_rows() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let runtime = crate::roles::Runtime {
            default_provider: Some("codex".into()),
            sandbox: Some("workspace-write-no-network".into()),
            claude_home: Some("/tmp/ciacola-legacy-claude-home".into()),
            codex_home: Some("/tmp/ciacola-default-codex-home".into()),
            token_env: Some("CIACOLA_LEGACY_CLAUDE_TOKEN".into()),
            codex_token_env: Some("CIACOLA_DEFAULT_CODEX_TOKEN".into()),
            ..Default::default()
        };
        let l = Ledger::setup(pool)
            .await
            .expect("ledger")
            .with_runtime(runtime)
            .expect("runtime");

        let current_id = l
            .create_agent(&AgentDef::new("current", "s"), None)
            .await
            .expect("current");
        let current = l
            .get_agent(&current_id)
            .await
            .expect("get")
            .expect("row")
            .def;
        assert_eq!(current.provider, ciacola_agent::ProviderKey::codex());
        assert_eq!(
            current.config_home.as_deref(),
            Some("/tmp/ciacola-default-codex-home")
        );
        assert_eq!(
            current.token_env.as_deref(),
            Some("CIACOLA_DEFAULT_CODEX_TOKEN")
        );
        assert_eq!(
            current.sandbox.as_deref(),
            Some("workspace-write-no-network")
        );

        let legacy: AgentDef = serde_json::from_str(
            r#"{
                "name": "legacy",
                "system_prompt": "s",
                "model": null,
                "allowed_tools": [],
                "working_dir": null,
                "max_turns": null
            }"#,
        )
        .expect("legacy definition");
        let legacy_id = l.create_agent(&legacy, None).await.expect("legacy");
        let legacy = l
            .get_agent(&legacy_id)
            .await
            .expect("get")
            .expect("row")
            .def;
        assert_eq!(legacy.provider, ciacola_agent::ProviderKey::claude());
        assert_eq!(
            legacy.config_home.as_deref(),
            Some("/tmp/ciacola-legacy-claude-home")
        );
        assert_eq!(
            legacy.token_env.as_deref(),
            Some("CIACOLA_LEGACY_CLAUDE_TOKEN")
        );
    }

    #[tokio::test]
    async fn a_recorded_conversation_cannot_move_between_providers() {
        let l = ledger().await;
        let id = l
            .create_agent(&AgentDef::new("claude-agent", "s"), None)
            .await
            .expect("create");
        l.enqueue_turn(&id, "queued once")
            .await
            .expect("record a turn");

        let error = l
            .update_agent_def(&id, &AgentDef::new("codex-agent", "s").provider("codex"))
            .await
            .expect_err("provider session ids are not portable");
        assert!(error.to_string().contains("refusing to move"), "{error}");

        let stored = l.get_agent(&id).await.expect("get").expect("row");
        assert_eq!(stored.def.provider, ciacola_agent::ProviderKey::claude());
        assert_eq!(stored.name, "claude-agent");
    }

    #[tokio::test]
    async fn a_success_without_a_resume_id_preserves_the_existing_session() {
        let l = ledger().await;
        let id = l
            .create_agent(&AgentDef::new("a", "s"), None)
            .await
            .expect("create");
        let existing = l.get_agent(&id).await.unwrap().unwrap().session.unwrap();
        let seq = l.enqueue_turn(&id, "go").await.unwrap();
        assert!(l.claim_turn(&id, seq).await.unwrap());
        let mut outcome = exchange("unused");
        outcome.session = None;
        assert!(l.complete_turn(&id, seq, &outcome).await.unwrap());
        assert_eq!(
            l.get_agent(&id).await.unwrap().unwrap().session.as_deref(),
            Some(existing.as_str())
        );
    }

    #[tokio::test]
    async fn a_failed_exchange_persists_portable_telemetry_states() {
        let l = ledger().await;
        let id = l
            .create_agent(&AgentDef::new("a", "s"), None)
            .await
            .expect("create");
        let seq = l.enqueue_turn(&id, "go").await.unwrap();
        assert!(l.claim_turn(&id, seq).await.unwrap());
        assert!(
            l.record_usage_snapshot(
                &id,
                seq,
                ciacola_agent::TokenUsage {
                    input: 99,
                    output: 98,
                    cached_input: 97,
                },
            )
            .await
            .expect("snapshot")
        );
        let exchange = Exchange {
            reply: String::new(),
            session: None,
            cost: ciacola_agent::Cost::Unreported,
            usage: ciacola_agent::Usage::Reported(ciacola_agent::TokenUsage {
                input: 12,
                output: 3,
                cached_input: 7,
            }),
            provider_turns: Some(9),
            elapsed_ms: 250,
            error: Some("limited".into()),
        };
        assert!(
            l.fail_exchange(&id, seq, "failed", "limited", &exchange)
                .await
                .unwrap()
        );
        let turn = l.get_turn(&id, seq).await.unwrap().unwrap();
        assert_eq!(turn.cost_state, "unreported");
        assert_eq!(turn.usage_state, "reported");
        assert_eq!(
            (turn.tokens_in, turn.tokens_out, turn.tokens_cached),
            (12, 3, 7)
        );
        assert_eq!(turn.provider_turns, Some(9));
        assert_eq!(turn.elapsed_state, "measured");
        assert_eq!(turn.reported_tokens(), Some((12, 3, 7)));
        assert_eq!(turn.reported_cost_micro_usd(), None);
    }

    #[tokio::test]
    async fn terminal_usage_gaps_do_not_erase_reported_snapshots() {
        let l = ledger().await;
        let id = l
            .create_agent(&AgentDef::new("a", "s"), None)
            .await
            .expect("agent");

        let success = l.enqueue_turn(&id, "success").await.expect("turn");
        assert!(l.claim_turn(&id, success).await.expect("claim"));
        assert!(
            l.record_usage_snapshot(
                &id,
                success,
                ciacola_agent::TokenUsage {
                    input: 30,
                    output: 5,
                    cached_input: 12,
                },
            )
            .await
            .expect("snapshot")
        );
        let no_terminal_usage = Exchange {
            reply: "done".into(),
            session: None,
            cost: ciacola_agent::Cost::Reported { micro_usd: 10 },
            usage: ciacola_agent::Usage::Unreported,
            provider_turns: Some(1),
            elapsed_ms: 100,
            error: None,
        };
        assert!(
            l.complete_turn(&id, success, &no_terminal_usage)
                .await
                .expect("complete")
        );
        assert_eq!(
            l.get_turn(&id, success)
                .await
                .unwrap()
                .unwrap()
                .reported_tokens(),
            Some((30, 5, 12))
        );

        let failed = l.enqueue_turn(&id, "failed").await.expect("turn");
        assert!(l.claim_turn(&id, failed).await.expect("claim"));
        assert!(
            l.record_usage_snapshot(
                &id,
                failed,
                ciacola_agent::TokenUsage {
                    input: 44,
                    output: 7,
                    cached_input: 18,
                },
            )
            .await
            .expect("snapshot")
        );
        let partial_without_usage = Exchange {
            reply: String::new(),
            session: None,
            cost: ciacola_agent::Cost::Unreported,
            usage: ciacola_agent::Usage::Unreported,
            provider_turns: None,
            elapsed_ms: 200,
            error: Some("timed out after launch".into()),
        };
        assert!(
            l.fail_exchange(
                &id,
                failed,
                "failed",
                "timed out after launch",
                &partial_without_usage,
            )
            .await
            .expect("fail")
        );
        assert_eq!(
            l.get_turn(&id, failed)
                .await
                .unwrap()
                .unwrap()
                .reported_tokens(),
            Some((44, 7, 18))
        );
    }

    /// The live-kill regression: elapsed time comes from the durable
    /// claim, and a provider that normally reports both buckets records
    /// their absence rather than manufacturing measured zeros.
    #[tokio::test]
    async fn interrupting_a_running_turn_keeps_elapsed_and_marks_gaps() {
        let l = ledger_with_provider("priced", true, true).await;
        let id = l
            .create_agent(&AgentDef::new("a", "s").provider("priced"), None)
            .await
            .expect("agent");
        let seq = l.enqueue_turn(&id, "work for a while").await.expect("turn");
        assert!(l.claim_turn(&id, seq).await.expect("claim"));
        let claimed = crate::time::now_unix_ms() - 2_000;
        sqlx::query("UPDATE turns SET claimed_unix_ms = ?3 WHERE agent_id = ?1 AND seq = ?2")
            .bind(&id)
            .bind(seq)
            .bind(claimed)
            .execute(l.pool())
            .await
            .expect("age claim");

        assert!(
            l.interrupt_turn(&id, seq, "killed", "killed by request")
                .await
                .expect("interrupt")
        );
        let turn = l.get_turn(&id, seq).await.unwrap().unwrap();
        assert_eq!(turn.state, "killed");
        assert_eq!(turn.claimed_unix_ms, Some(claimed));
        assert!(turn.elapsed_ms >= 2_000, "elapsed was {}", turn.elapsed_ms);
        assert_eq!(turn.elapsed_state, "measured");
        assert_eq!(turn.cost_state, "unreported");
        assert_eq!(turn.usage_state, "unreported");
        assert_eq!(turn.reported_cost_micro_usd(), None);
        assert_eq!(turn.reported_tokens(), None);
    }

    /// The other side of the kill/claim race. If interruption wins, the
    /// provider never starts and a later delivery cannot claim the row.
    #[tokio::test]
    async fn interrupting_a_queued_turn_prevents_claim_and_measures_zero_runtime() {
        let l = ledger_with_provider("priced", true, true).await;
        let id = l
            .create_agent(&AgentDef::new("a", "s").provider("priced"), None)
            .await
            .expect("agent");
        let seq = l.enqueue_turn(&id, "queued work").await.expect("turn");

        assert!(
            l.interrupt_turn(&id, seq, "killed", "killed by request")
                .await
                .expect("interrupt")
        );
        assert!(!l.claim_turn(&id, seq).await.expect("late claim"));
        let turn = l.get_turn(&id, seq).await.unwrap().unwrap();
        assert_eq!(turn.elapsed_ms, 0);
        assert_eq!(turn.claimed_unix_ms, None);
        assert_eq!(turn.elapsed_state, "not_attempted");
        assert_eq!(turn.cost_state, "reported");
        assert_eq!(turn.usage_state, "reported");
        assert_eq!(turn.provider_turns, Some(0));
        assert_eq!(turn.reported_cost_micro_usd(), Some(0));
        assert_eq!(turn.reported_tokens(), Some((0, 0, 0)));
        assert!(
            !l.record_usage_snapshot(
                &id,
                seq,
                ciacola_agent::TokenUsage {
                    input: 1,
                    output: 1,
                    cached_input: 0,
                },
            )
            .await
            .expect("queued kill rejects snapshots")
        );
    }

    /// Backend capability is part of the fact: Codex-like providers do
    /// not have a missing monetary price when they never produce one.
    #[tokio::test]
    async fn interruption_distinguishes_unpriced_from_unreported() {
        let l = ledger_with_provider("unpriced", false, true).await;
        let id = l
            .create_agent(&AgentDef::new("a", "s").provider("unpriced"), None)
            .await
            .expect("agent");
        let seq = l.enqueue_turn(&id, "work").await.expect("turn");
        assert!(l.claim_turn(&id, seq).await.expect("claim"));
        assert!(
            l.interrupt_turn(&id, seq, "killed", "killed by request")
                .await
                .expect("interrupt")
        );
        let turn = l.get_turn(&id, seq).await.unwrap().unwrap();
        assert_eq!(turn.cost_state, "not_priced");
        assert_eq!(turn.usage_state, "unreported");
        assert_eq!(turn.elapsed_state, "measured");
    }

    /// A provider can genuinely report zero. The state, not the number,
    /// is what makes that different from the interrupted rows above.
    #[tokio::test]
    async fn reported_zero_remains_a_measurement() {
        let l = ledger().await;
        let id = l
            .create_agent(&AgentDef::new("a", "s"), None)
            .await
            .expect("agent");
        let seq = l.enqueue_turn(&id, "work").await.expect("turn");
        assert!(l.claim_turn(&id, seq).await.expect("claim"));
        let outcome = Exchange {
            reply: "done".into(),
            session: None,
            cost: ciacola_agent::Cost::Reported { micro_usd: 0 },
            usage: ciacola_agent::Usage::Reported(ciacola_agent::TokenUsage::default()),
            provider_turns: Some(0),
            elapsed_ms: 0,
            error: None,
        };
        assert!(l.complete_turn(&id, seq, &outcome).await.expect("complete"));
        let turn = l.get_turn(&id, seq).await.unwrap().unwrap();
        assert_eq!(turn.reported_cost_micro_usd(), Some(0));
        assert_eq!(turn.reported_tokens(), Some((0, 0, 0)));
        assert_eq!(turn.elapsed_state, "measured");
    }

    #[tokio::test]
    async fn abort_before_provider_is_a_known_zero_non_attempt() {
        let l = ledger_with_provider("priced", true, true).await;
        let id = l
            .create_agent(&AgentDef::new("a", "s").provider("priced"), None)
            .await
            .expect("agent");
        let seq = l.enqueue_turn(&id, "work").await.expect("turn");
        assert!(l.claim_turn(&id, seq).await.expect("claim"));

        assert!(
            l.abort_claimed_turn(&id, seq, "dispatch could not start")
                .await
                .expect("abort")
        );
        let turn = l.get_turn(&id, seq).await.unwrap().unwrap();
        assert_eq!(turn.state, "failed");
        assert_eq!(turn.elapsed_state, "not_attempted");
        assert_eq!(turn.elapsed_ms, 0);
        assert_eq!(turn.reported_cost_micro_usd(), Some(0));
        assert_eq!(turn.reported_tokens(), Some((0, 0, 0)));
        assert_eq!(turn.provider_turns, Some(0));
    }

    #[tokio::test]
    async fn cumulative_usage_snapshot_survives_both_sides_of_a_live_kill() {
        let l = ledger_with_provider("priced", true, true).await;
        let id = l
            .create_agent(&AgentDef::new("a", "s").provider("priced"), None)
            .await
            .expect("agent");
        let seq = l.enqueue_turn(&id, "work").await.expect("turn");
        assert!(l.claim_turn(&id, seq).await.expect("claim"));

        assert!(
            l.record_usage_snapshot(
                &id,
                seq,
                ciacola_agent::TokenUsage {
                    input: 10,
                    output: 2,
                    cached_input: 3,
                },
            )
            .await
            .expect("running snapshot")
        );
        assert!(
            l.interrupt_turn(&id, seq, "killed", "stopped")
                .await
                .expect("kill")
        );
        assert!(
            l.record_usage_snapshot(
                &id,
                seq,
                ciacola_agent::TokenUsage {
                    input: 15,
                    output: 4,
                    cached_input: 5,
                },
            )
            .await
            .expect("drained snapshot after kill")
        );

        let turn = l.get_turn(&id, seq).await.unwrap().unwrap();
        assert_eq!(turn.reported_tokens(), Some((15, 4, 5)));
        assert_eq!(turn.cost_state, "unreported");
        assert_eq!(turn.reported_cost_micro_usd(), None);
    }

    #[tokio::test]
    async fn recovery_and_scalar_failure_preserve_reported_usage_snapshots() {
        let l = ledger_with_provider("priced", true, true).await;
        let id = l
            .create_agent(&AgentDef::new("a", "s").provider("priced"), None)
            .await
            .expect("agent");
        let usage = ciacola_agent::TokenUsage {
            input: 21,
            output: 8,
            cached_input: 13,
        };

        let recovered = l.enqueue_turn(&id, "recover").await.expect("turn");
        assert!(l.claim_turn(&id, recovered).await.expect("claim"));
        assert!(
            l.record_usage_snapshot(&id, recovered, usage)
                .await
                .expect("snapshot")
        );
        assert!(
            l.recover_turn(&id, recovered, "restart")
                .await
                .expect("recover")
        );
        assert_eq!(
            l.get_turn(&id, recovered)
                .await
                .unwrap()
                .unwrap()
                .reported_tokens(),
            Some((21, 8, 13))
        );

        let failed = l.enqueue_turn(&id, "fail").await.expect("turn");
        assert!(l.claim_turn(&id, failed).await.expect("claim"));
        assert!(
            l.record_usage_snapshot(&id, failed, usage)
                .await
                .expect("snapshot")
        );
        assert!(
            l.fail_turn(&id, failed, "failed", "boom", 0, 9, None)
                .await
                .expect("fail")
        );
        assert_eq!(
            l.get_turn(&id, failed)
                .await
                .unwrap()
                .unwrap()
                .reported_tokens(),
            Some((21, 8, 13))
        );
    }
}
