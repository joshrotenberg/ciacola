//! Our own cron: a schedule is a ledger row, a fire is an ordinary send.
//!
//! This is the whole argument against a scheduler framework. A schedule
//! is `(agent_id, text, every_secs, next_fire)`. The loop wakes once a
//! second, and anything due becomes a normal turn: same admission rule,
//! same executor, same notifications, same board. If the agent is still
//! busy with the previous fire, the fire is skipped and counted, not
//! queued into a pileup.
//!
//! Intervals, not cron expressions, on purpose: the mechanism is what
//! the spike is testing, and an interval demos in seconds. Real cron
//! syntax is one small parsing crate away (`cron`) and changes nothing
//! structural.
//!
//! Scheduled turns resume the agent's session like any other turn, so a
//! schedule is a *recurring conversation*, not a recurring job: the
//! agent can see its own previous fires.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use tower_mcp::{CallToolResult, Tool, ToolBuilder};

use ciacola_core::agent::FlatError;
use ciacola_core::ledger::Ledger;

const MIN_EVERY_SECS: i64 = 10;
const MAX_EVERY_SECS: i64 = 86_400;

fn validate_interval(every_secs: i64) -> Result<(), FlatError> {
    if (MIN_EVERY_SECS..=MAX_EVERY_SECS).contains(&every_secs) {
        Ok(())
    } else {
        Err(format!("every_secs must be {MIN_EVERY_SECS}..={MAX_EVERY_SECS}").into())
    }
}

/// The shape of `[agents.plugins.schedule]`. Owned here rather than by
/// the binary's config types, which is the point of the hook.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSchedule {
    pub every_secs: i64,
    pub text: String,
}

#[derive(Clone)]
pub struct Schedules {
    pool: SqlitePool,
}

#[derive(Debug, Clone)]
pub struct ScheduleRow {
    pub schedule_id: String,
    pub agent_id: String,
    pub text: String,
    pub every_secs: i64,
    pub next_fire_unix: i64,
    pub fires: i64,
    pub skips: i64,
}

type Row = (String, String, String, i64, i64, i64, i64);

fn row(t: Row) -> ScheduleRow {
    let (schedule_id, agent_id, text, every_secs, next_fire_unix, fires, skips) = t;
    ScheduleRow {
        schedule_id,
        agent_id,
        text,
        every_secs,
        next_fire_unix,
        fires,
        skips,
    }
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

impl Schedules {
    /// Wrap an already-migrated pool. Schema is the plugin's
    /// `migrations()`, not this constructor.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn setup(pool: SqlitePool) -> Result<Self, FlatError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS schedules (
                 schedule_id TEXT PRIMARY KEY,
                 agent_id TEXT NOT NULL,
                 text TEXT NOT NULL,
                 every_secs INTEGER NOT NULL,
                 next_fire_unix INTEGER NOT NULL,
                 fires INTEGER NOT NULL DEFAULT 0,
                 skips INTEGER NOT NULL DEFAULT 0)",
        )
        .execute(&pool)
        .await?;
        Ok(Self { pool })
    }

    pub async fn create(
        &self,
        agent_id: &str,
        text: &str,
        every_secs: i64,
    ) -> Result<ScheduleRow, FlatError> {
        validate_interval(every_secs)?;
        let schedule_id = ulid::Ulid::new().to_string();
        let next = now_unix() + every_secs;
        sqlx::query(
            "INSERT INTO schedules (schedule_id, agent_id, text, every_secs, next_fire_unix)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&schedule_id)
        .bind(agent_id)
        .bind(text)
        .bind(every_secs)
        .bind(next)
        .execute(&self.pool)
        .await?;
        Ok(ScheduleRow {
            schedule_id,
            agent_id: agent_id.into(),
            text: text.into(),
            every_secs,
            next_fire_unix: next,
            fires: 0,
            skips: 0,
        })
    }

    pub async fn delete(&self, schedule_id: &str) -> Result<bool, FlatError> {
        let done = sqlx::query("DELETE FROM schedules WHERE schedule_id = ?1")
            .bind(schedule_id)
            .execute(&self.pool)
            .await?;
        Ok(done.rows_affected() == 1)
    }

    pub async fn list(&self) -> Result<Vec<ScheduleRow>, FlatError> {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT schedule_id, agent_id, text, every_secs, next_fire_unix, fires, skips
             FROM schedules ORDER BY schedule_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row).collect())
    }

    pub async fn due(&self, now: i64) -> Result<Vec<ScheduleRow>, FlatError> {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT schedule_id, agent_id, text, every_secs, next_fire_unix, fires, skips
             FROM schedules WHERE next_fire_unix <= ?1",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row).collect())
    }

    /// Advance the schedule past a fire (or a skip). The next fire is
    /// computed from now, not from the intended time, so a stall does
    /// not produce a burst of catch-up fires.
    pub async fn advance(&self, schedule_id: &str, skipped: bool) -> Result<(), FlatError> {
        sqlx::query(&format!(
            "UPDATE schedules SET next_fire_unix = ?2 + every_secs, {} = {} + 1
             WHERE schedule_id = ?1",
            if skipped { "skips" } else { "fires" },
            if skipped { "skips" } else { "fires" },
        ))
        .bind(schedule_id)
        .bind(now_unix())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Fire one due schedule and advance past it (fired or skipped). Split
/// out of [`spawn_scheduler`]'s loop so the invariant that matters,
/// that busy skips rather than queues and `next_fire_unix` advances
/// either way, can be driven directly by a test without waiting on the
/// one-second tick.
async fn fire(schedules: &Schedules, ctx: &PluginContext, schedule: &ScheduleRow) -> Submission {
    let outcome = ctx
        .submit_turn(
            &schedule.agent_id,
            &schedule.text,
            &format!("schedule {}", schedule.schedule_id),
        )
        .await;
    // A skip is counted, not retried: next_fire advances either way so
    // a busy agent cannot build a backlog.
    let skipped = outcome.submitted().is_none();
    if let Err(e) = schedules.advance(&schedule.schedule_id, skipped).await {
        eprintln!("[cron] advance {}: {e}", schedule.schedule_id);
    }
    outcome
}

/// The loop. Wakes once a second; everything due is poked through
/// [`PluginContext::submit_turn`], which is the same call a webhook or
/// a file watcher makes. Cron's only distinctive job is deciding *when*.
/// Deleting a schedule mid-sleep takes effect within a tick.
pub fn spawn_scheduler(schedules: Schedules, ctx: PluginContext) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let due = match schedules.due(now_unix()).await {
                Ok(due) => due,
                Err(e) => {
                    eprintln!("[cron] scan: {e}");
                    continue;
                }
            };
            for schedule in due {
                fire(&schedules, &ctx, &schedule).await;
            }
        }
    });
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScheduleArgs {
    /// The agent to send to on each fire.
    agent_id: String,
    /// What to say each time. Fires resume the same conversation, so
    /// the agent can see its own previous fires.
    text: String,
    /// Interval in seconds, 10 to 86400.
    every_secs: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UnscheduleArgs {
    /// From `schedule` or `schedules`.
    schedule_id: String,
}

fn schedule_json(s: &ScheduleRow) -> serde_json::Value {
    json!({
        "schedule_id": s.schedule_id,
        "agent_id": s.agent_id,
        "text": s.text,
        "every_secs": s.every_secs,
        "next_fire_in_secs": (s.next_fire_unix - now_unix()).max(0),
        "fires": s.fires,
        "skips": s.skips,
    })
}

/// The three schedule tools. Stdio surface only: a standing spend
/// commitment stays a person's call, like kill.
pub fn tools(schedules: Schedules, ledger: Ledger) -> Vec<Tool> {
    let schedule = {
        let schedules = schedules.clone();
        let ledger = ledger.clone();
        ToolBuilder::new("schedule")
            .description(
                "Send the same text to an agent on an interval. Each fire \
                 is an ordinary turn in the agent's own conversation; if \
                 the agent is mid-turn when a fire comes due, that fire \
                 is skipped and counted.",
            )
            .non_destructive()
            .handler(move |args: ScheduleArgs| {
                let schedules = schedules.clone();
                let ledger = ledger.clone();
                async move {
                    if let Err(error) = validate_interval(args.every_secs) {
                        return Ok(CallToolResult::error(error.to_string()));
                    }
                    match ledger.get_agent(&args.agent_id).await {
                        Ok(Some(agent)) if agent.retired => {
                            return Ok(CallToolResult::error(format!(
                                "agent '{}' is retired",
                                args.agent_id
                            )));
                        }
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            return Ok(CallToolResult::error(format!(
                                "no agent '{}'",
                                args.agent_id
                            )));
                        }
                        Err(e) => return Ok(CallToolResult::error(e.to_string())),
                    }
                    match schedules
                        .create(&args.agent_id, &args.text, args.every_secs)
                        .await
                    {
                        Ok(s) => Ok(CallToolResult::json(schedule_json(&s))),
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                }
            })
            .build()
    };

    let unschedule = {
        let schedules = schedules.clone();
        ToolBuilder::new("unschedule")
            .description("Stop a schedule. The agent and its conversation remain.")
            .destructive()
            .handler(move |args: UnscheduleArgs| {
                let schedules = schedules.clone();
                async move {
                    match schedules.delete(&args.schedule_id).await {
                        Ok(true) => Ok(CallToolResult::json(json!({ "deleted": true }))),
                        Ok(false) => Ok(CallToolResult::error(format!(
                            "no schedule '{}'",
                            args.schedule_id
                        ))),
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                }
            })
            .build()
    };

    let list = ToolBuilder::new("schedules")
        .description("Every schedule, with next fire and fire/skip counts.")
        .read_only()
        .no_params_handler(move || {
            let schedules = schedules.clone();
            async move {
                match schedules.list().await {
                    Ok(all) => Ok(CallToolResult::json(json!({
                        "schedules": all.iter().map(schedule_json).collect::<Vec<_>>()
                    }))),
                    Err(e) => Ok(CallToolResult::error(e.to_string())),
                }
            }
        })
        .build();

    vec![schedule, unschedule, list]
}

// --- plugin ---

use ciacola_core::plugin::{
    BoxFut, Migration, Plugin, PluginContext, Section, Submission, Surface,
};

/// Cron as a plugin. The only one so far that needs `start`: its loop
/// is background work, and it is also the only plugin that submits
/// turns, which is why the executor is on [`PluginContext`].
///
/// Operator-only tools: a schedule is a standing commitment to spend,
/// so an agent may read the board but not arm one.
#[derive(Default)]
pub struct SchedulePlugin {
    schedules: Option<Schedules>,
    ledger: Option<Ledger>,
}

impl SchedulePlugin {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn schedules(&self) -> Option<&Schedules> {
        self.schedules.as_ref()
    }
}

impl Plugin for SchedulePlugin {
    fn tables(&self) -> &'static [&'static str] {
        &["schedules"]
    }

    fn migrations(&self) -> &'static [Migration] {
        {
            const M: &[Migration] = &[Migration::new(
                "0001_schedules",
                "CREATE TABLE IF NOT EXISTS schedules (
                 schedule_id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, text TEXT NOT NULL,
                 every_secs INTEGER NOT NULL, next_fire_unix INTEGER NOT NULL,
                 fires INTEGER NOT NULL DEFAULT 0, skips INTEGER NOT NULL DEFAULT 0);",
            )];
            M
        }
    }

    fn name(&self) -> &'static str {
        "schedule"
    }

    fn setup<'a>(&'a mut self, ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async move {
            self.schedules = Some(Schedules::new(ctx.pool.clone()));
            self.ledger = Some(ctx.ledger.clone());
            Ok(())
        })
    }

    /// The wake belongs here, not in the binary's config pass.
    ///
    /// `[agents.plugins.schedule]` is written where the agent is
    /// declared because that is where a person wants it, and it is this
    /// plugin's data. Until this hook existed, `config::apply` held a
    /// `Schedules` handle to do it, which is a privileged path core
    /// could not have offered a plugin it had never heard of.
    ///
    /// Replaces rather than adds, so the file stays the truth for the
    /// wake as it is for the definition, and so booting twice does not
    /// leave an agent with two.
    fn agent_config<'a>(
        &'a self,
        agent_id: &'a str,
        section: &'a toml::Value,
    ) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async move {
            let Some(schedules) = self.schedules.as_ref() else {
                return Err("schedule plugin has no handle; setup did not run".into());
            };
            let wake: ConfigSchedule = section
                .clone()
                .try_into()
                .map_err(|e| -> FlatError { format!("[agents.plugins.schedule]: {e}").into() })?;
            validate_interval(wake.every_secs)?;
            for existing in schedules.list().await? {
                if existing.agent_id == agent_id {
                    schedules.delete(&existing.schedule_id).await?;
                }
            }
            schedules
                .create(agent_id, &wake.text, wake.every_secs)
                .await?;
            Ok(())
        })
    }

    fn start(&self, ctx: &PluginContext) {
        if let Some(schedules) = self.schedules() {
            spawn_scheduler(schedules.clone(), ctx.clone());
        }
    }

    fn tools(&self, surface: Surface) -> Vec<Tool> {
        match (self.schedules(), &self.ledger, surface) {
            (Some(schedules), Some(ledger), Surface::Operator) => {
                tools(schedules.clone(), ledger.clone())
            }
            _ => Vec::new(),
        }
    }

    fn board_section(&self) -> BoxFut<'_, Option<Section>> {
        Box::pin(async move {
            let all = self.schedules()?.list().await.ok()?;
            if all.is_empty() {
                return None;
            }
            let now = now_unix();
            let mut html = String::from(
                "<table><tr><th>agent</th><th>says</th><th class=\"num\">every</th>\
                 <th class=\"num\">next fire</th><th class=\"num\">fires</th>\
                 <th class=\"num\">skips</th></tr>",
            );
            for s in &all {
                html.push_str(&format!(
                    "<tr><td><a href=\"/board/agent/{id}\">{id6}</a></td>\
                     <td class=\"dim\">{text}</td><td class=\"num\">{every}s</td>\
                     <td class=\"num\">{next}s</td><td class=\"num\">{fires}</td>\
                     <td class=\"num\">{skips}</td></tr>",
                    id = ciacola_core::render::esc(&s.agent_id),
                    id6 = ciacola_core::render::esc(
                        &s.agent_id[s.agent_id.len().saturating_sub(6)..]
                    ),
                    text = ciacola_core::render::esc(&s.text.chars().take(60).collect::<String>()),
                    every = s.every_secs,
                    next = (s.next_fire_unix - now).max(0),
                    fires = s.fires,
                    skips = s.skips,
                ));
            }
            html.push_str("</table>");
            Some(Section {
                title: "schedules".into(),
                html,
            })
        })
    }

    fn health(&self) -> BoxFut<'_, serde_json::Value> {
        Box::pin(async move {
            let Some(schedules) = self.schedules() else {
                return json!({});
            };
            let all = schedules.list().await.unwrap_or_default();
            json!({
                "schedules": all.len(),
                "fires": all.iter().map(|s| s.fires).sum::<i64>(),
                "skips": all.iter().map(|s| s.skips).sum::<i64>(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ciacola_core::agent::AgentDef;
    use ciacola_core::exec::TurnExecutor;
    use ciacola_core::ledger::Ledger;
    use ciacola_core::notify::Notifier;

    use super::*;

    struct ReportingProvider;

    impl ciacola_agent::Provider for ReportingProvider {
        fn key(&self) -> ciacola_agent::ProviderKey {
            ciacola_agent::ProviderKey::claude()
        }

        fn capabilities(&self) -> ciacola_agent::Capabilities {
            let mut capabilities = ciacola_agent::Capabilities::none(self.key());
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
            Box::pin(async { unreachable!("schedule tests do not run providers") })
        }

        fn owns_process(&self, _ps_line: &str) -> bool {
            false
        }
    }

    struct UnpricedProvider;

    impl ciacola_agent::Provider for UnpricedProvider {
        fn key(&self) -> ciacola_agent::ProviderKey {
            ciacola_agent::ProviderKey::codex()
        }

        fn capabilities(&self) -> ciacola_agent::Capabilities {
            let mut capabilities = ciacola_agent::Capabilities::none(self.key());
            capabilities.reports_token_usage = true;
            capabilities
        }

        fn run<'a>(
            &'a self,
            _intent: &'a ciacola_agent::TurnIntent,
            _events: &'a dyn ciacola_agent::TurnEvents,
        ) -> ciacola_agent::BoxFut<'a, Result<ciacola_agent::TurnOutcome, ciacola_agent::AgentError>>
        {
            Box::pin(async { unreachable!("schedule tests do not run providers") })
        }

        fn owns_process(&self, _ps_line: &str) -> bool {
            false
        }
    }

    /// Records what it was handed and does nothing else: no claim, no
    /// run, no completion. A submitted turn therefore stays `queued`
    /// forever, which is exactly what "the agent is busy" means to the
    /// admission guard in `Ledger::enqueue_turn`. CI-safe stand-in for a
    /// real executor; nothing here shells out to a provider.
    #[derive(Default)]
    struct RecordingExecutor {
        submitted: Mutex<Vec<(String, i64)>>,
    }

    impl TurnExecutor for RecordingExecutor {
        fn submit(&self, agent_id: String, seq: i64) {
            self.submitted.lock().unwrap().push((agent_id, seq));
        }
        fn kill(&self, _agent_id: &str, _seq: i64) -> bool {
            false
        }
        fn name(&self) -> &'static str {
            "recording"
        }
    }

    async fn setup() -> (Schedules, PluginContext, Arc<RecordingExecutor>, Ledger) {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let providers = ciacola_agent::ProviderRegistry::new()
            .with(Arc::new(ReportingProvider))
            .and_then(|providers| providers.with(Arc::new(UnpricedProvider)))
            .expect("providers");
        let ledger = Ledger::setup(pool.clone())
            .await
            .expect("ledger")
            .with_providers(providers);
        let schedules = Schedules::setup(pool.clone()).await.expect("schedules");
        let (tx, _rx) = tower_mcp::context::notification_channel(8);
        let exec = Arc::new(RecordingExecutor::default());
        let ctx = PluginContext {
            pool,
            ledger: ledger.clone(),
            exec: exec.clone() as Arc<dyn TurnExecutor>,
            notify: Notifier(tx),
            db_path: String::new(),
            loopback_mcp_config: String::new(),
            operator_mcp_config: String::new(),
            plugin_config: toml::Value::Table(toml::map::Map::new()),
            limits: Default::default(),
            runtime: Default::default(),
        };
        (schedules, ctx, exec, ledger)
    }

    async fn new_agent(ledger: &Ledger) -> String {
        ledger
            .create_agent(&AgentDef::new("a", "sys"), None)
            .await
            .expect("create agent")
    }

    /// Backdate a schedule's next fire without sleeping on the real
    /// clock, per CONTRIBUTING: drive time by writing `next_fire_unix`
    /// directly. `Schedules.pool` is private, but this module is a
    /// descendant of the one that defines it.
    async fn make_due(schedules: &Schedules, schedule_id: &str) {
        sqlx::query("UPDATE schedules SET next_fire_unix = ?1 WHERE schedule_id = ?2")
            .bind(now_unix() - 1)
            .bind(schedule_id)
            .execute(&schedules.pool)
            .await
            .expect("backdate");
    }

    async fn find(schedules: &Schedules, schedule_id: &str) -> ScheduleRow {
        schedules
            .list()
            .await
            .expect("list")
            .into_iter()
            .find(|s| s.schedule_id == schedule_id)
            .expect("schedule still exists")
    }

    #[tokio::test]
    async fn due_schedule_with_idle_agent_fires_and_advances() {
        let (schedules, ctx, exec, ledger) = setup().await;
        let agent_id = new_agent(&ledger).await;
        let s = schedules.create(&agent_id, "hi", 10).await.expect("create");
        make_due(&schedules, &s.schedule_id).await;

        let due = schedules.due(now_unix()).await.expect("due");
        assert_eq!(due.len(), 1, "backdated schedule must be due");

        let outcome = fire(&schedules, &ctx, &due[0]).await;
        assert!(
            matches!(outcome, Submission::Submitted { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            exec.submitted.lock().unwrap().as_slice(),
            [(agent_id.clone(), 1)],
            "an idle agent's fire must actually be dispatched"
        );

        let after = find(&schedules, &s.schedule_id).await;
        assert_eq!(after.fires, 1);
        assert_eq!(after.skips, 0);
        assert!(
            schedules.due(now_unix()).await.expect("due").is_empty(),
            "next_fire_unix must advance past now, or the schedule stays due forever"
        );
    }

    #[tokio::test]
    async fn due_schedule_with_busy_agent_skips_without_enqueuing() {
        let (schedules, ctx, exec, ledger) = setup().await;
        let agent_id = new_agent(&ledger).await;
        // Busy: a turn is already queued, same admission rule `send` uses.
        ledger
            .enqueue_turn(&agent_id, "already running")
            .await
            .expect("enqueue");
        let s = schedules.create(&agent_id, "hi", 10).await.expect("create");
        make_due(&schedules, &s.schedule_id).await;

        let due = schedules.due(now_unix()).await.expect("due");
        let outcome = fire(&schedules, &ctx, &due[0]).await;
        assert!(matches!(outcome, Submission::Busy { .. }), "{outcome:?}");
        assert!(
            exec.submitted.lock().unwrap().is_empty(),
            "a skip must never reach the executor; that is the pileup this exists to prevent"
        );

        let (turn_count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM turns WHERE agent_id = ?1")
                .bind(&agent_id)
                .fetch_one(&ctx.pool)
                .await
                .expect("count");
        assert_eq!(turn_count, 1, "no second turn must have been enqueued");

        let after = find(&schedules, &s.schedule_id).await;
        assert_eq!(after.fires, 0);
        assert_eq!(after.skips, 1);
        assert!(
            schedules.due(now_unix()).await.expect("due").is_empty(),
            "a skip that fails to advance leaves the schedule permanently due"
        );
    }

    #[tokio::test]
    async fn unattended_schedule_cannot_run_an_unpriced_unguarded_provider() {
        let (schedules, ctx, exec, ledger) = setup().await;
        let agent_id = ledger
            .create_agent(&AgentDef::new("codex", "sys").provider("codex"), None)
            .await
            .expect("create agent");
        let schedule = schedules
            .create(&agent_id, "hi", 10)
            .await
            .expect("create schedule");
        make_due(&schedules, &schedule.schedule_id).await;

        let due = schedules.due(now_unix()).await.expect("due");
        let outcome = fire(&schedules, &ctx, &due[0]).await;
        assert!(
            matches!(outcome, Submission::Unguarded { .. }),
            "{outcome:?}"
        );
        assert!(exec.submitted.lock().unwrap().is_empty());
        assert!(
            ledger
                .conversation(&agent_id)
                .await
                .expect("conversation")
                .is_empty()
        );
        let after = find(&schedules, &schedule.schedule_id).await;
        assert_eq!(after.fires, 0);
        assert_eq!(after.skips, 1);
    }

    #[tokio::test]
    async fn due_schedule_for_a_retired_agent_produces_no_work() {
        let (schedules, ctx, exec, ledger) = setup().await;
        let agent_id = new_agent(&ledger).await;
        let s = schedules.create(&agent_id, "hi", 10).await.expect("create");
        assert!(ledger.retire_agent(&agent_id).await.expect("retire"));
        make_due(&schedules, &s.schedule_id).await;

        let due = schedules.due(now_unix()).await.expect("due");
        let outcome = fire(&schedules, &ctx, &due[0]).await;
        assert!(
            outcome.submitted().is_none(),
            "a retired agent must not produce a submitted turn: {outcome:?}"
        );
        assert!(exec.submitted.lock().unwrap().is_empty());

        let after = find(&schedules, &s.schedule_id).await;
        assert_eq!(after.fires, 0);
        assert_eq!(after.skips, 1, "counted like any other skip, not queued");
    }

    #[tokio::test]
    async fn round_trip_schedule_then_list_then_unschedule() {
        let (schedules, _ctx, _exec, ledger) = setup().await;
        let agent_id = new_agent(&ledger).await;

        let s = schedules.create(&agent_id, "hi", 10).await.expect("create");
        let listed = schedules.list().await.expect("list");
        assert!(listed.iter().any(|r| r.schedule_id == s.schedule_id));

        assert!(
            schedules.delete(&s.schedule_id).await.expect("delete"),
            "first unschedule removes it"
        );
        let listed = schedules.list().await.expect("list");
        assert!(!listed.iter().any(|r| r.schedule_id == s.schedule_id));

        assert!(
            !schedules
                .delete(&s.schedule_id)
                .await
                .expect("delete again"),
            "a second unschedule of the same id is not an error"
        );
    }

    #[tokio::test]
    async fn every_creation_path_rejects_an_out_of_range_interval() {
        let (schedules, _ctx, _exec, ledger) = setup().await;
        let agent_id = new_agent(&ledger).await;

        let error = schedules
            .create(&agent_id, "too fast", MIN_EVERY_SECS - 1)
            .await
            .expect_err("config and tools converge on create");
        assert!(error.to_string().contains("10..=86400"), "{error}");
        assert!(schedules.list().await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn invalid_config_schedule_does_not_replace_the_previous_wake() {
        let (schedules, _ctx, _exec, ledger) = setup().await;
        let agent_id = new_agent(&ledger).await;
        schedules
            .create(&agent_id, "valid", 60)
            .await
            .expect("existing schedule");
        let plugin = SchedulePlugin {
            schedules: Some(schedules.clone()),
            ledger: Some(ledger),
        };
        let invalid: toml::Value = toml::from_str(
            r#"
                every_secs = 1
                text = "invalid"
            "#,
        )
        .expect("config section");

        let error = plugin
            .agent_config(&agent_id, &invalid)
            .await
            .expect_err("invalid config must fail at startup");
        assert!(error.to_string().contains("10..=86400"), "{error}");
        let remaining = schedules.list().await.expect("list");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].text, "valid");
    }
}
