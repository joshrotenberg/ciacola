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

use apalis_sqlite::SqlitePool;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tower_mcp::{CallToolResult, Tool, ToolBuilder};

use ciacola_core::agent::FlatError;
use ciacola_core::ledger::Ledger;

const MIN_EVERY_SECS: i64 = 10;
const MAX_EVERY_SECS: i64 = 86_400;

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

/// The loop. Wakes once a second; everything due becomes an ordinary
/// send. Deleting a schedule mid-sleep takes effect within a tick.
/// The loop. Wakes once a second; everything due is poked through
/// [`PluginContext::submit_turn`], which is the same call a webhook or
/// a file watcher makes. Cron's only distinctive job is deciding *when*.
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
                let outcome = ctx
                    .submit_turn(
                        &schedule.agent_id,
                        &schedule.text,
                        &format!("schedule {}", schedule.schedule_id),
                    )
                    .await;
                // A skip is counted, not retried: next_fire advances
                // either way so a busy agent cannot build a backlog.
                let skipped = outcome.submitted().is_none();
                if let Err(e) = schedules.advance(&schedule.schedule_id, skipped).await {
                    eprintln!("[cron] advance {}: {e}", schedule.schedule_id);
                }
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
                    if !(MIN_EVERY_SECS..=MAX_EVERY_SECS).contains(&args.every_secs) {
                        return Ok(CallToolResult::error(format!(
                            "every_secs must be {MIN_EVERY_SECS}..={MAX_EVERY_SECS}"
                        )));
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

use ciacola_core::plugin::{BoxFut, Migration, Plugin, PluginContext, Section, Surface};

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
                    id = ciacola_core::board::esc(&s.agent_id),
                    id6 =
                        ciacola_core::board::esc(&s.agent_id[s.agent_id.len().saturating_sub(6)..]),
                    text = ciacola_core::board::esc(&s.text.chars().take(60).collect::<String>()),
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
