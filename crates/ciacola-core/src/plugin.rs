//! The plugin facility, and the line between core and everything else.
//!
//! Six modules arrived independently at the same shape (setup, tools,
//! resources, sometimes prompts, sometimes a background loop), which is
//! a trait asking to be extracted. This is it.
//!
//! **Core** is what nothing works without: the provider seam, the
//! ledger of agents and turns, an executor, notifications, recovery,
//! and the six verbs. [`PluginContext`] is exactly that surface, so the
//! trait doubles as a statement of where the line falls.
//!
//! **Everything else is a plugin**, including the parts we depend on
//! most: kanban, memory, findings, schedules, roles. That is the point.
//! A built-in that takes a privileged path leaves the plugin API a
//! second-class citizen that rots; a built-in that must go through the
//! API keeps it honest.
//!
//! The design decision that makes this more than registration is
//! **contribution**. A plugin does not only add tools; it contributes a
//! slice of every cross-cutting surface:
//!
//! - [`Plugin::board_section`] so the board composes instead of
//!   importing every plugin (board.rs had grown `Option<Items>`,
//!   `Option<Findings>`, and three constructors before this);
//! - [`Plugin::health`] so the size report is the sum of what each
//!   plugin knows about itself;
//! - [`Plugin::prune`] so retention is each plugin's own business
//!   (health.rs previously deleted from `work_items` and `findings`,
//!   tables it had no business knowing).
//!
//! Compile-in, not dynamic loading: Rust has no stable ABI, and a
//! laptop-local server gains nothing from process isolation that it
//! does not lose twice over in complexity. Registration is a `Vec` in
//! `main`, the same shape as a tower layer stack.
//!
//! **This facility is not a lock-in, which is what lets it stay
//! small.** Agents are handed an MCP config; any other MCP server can
//! be added to it, and the agent cannot tell the difference. So a
//! plugin earns its keep only when it needs something core owns: the
//! ledger, the board, the health and retention passes, or the agent
//! lifecycle. Anything self-contained (a weather API, a company's
//! internal search) should simply be its own MCP server, and the
//! answer to "the plugin API cannot express my thing" is usually "then
//! do not use it" rather than a wider trait.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use axum::Router;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tower_mcp::{LogLevel, McpRouter, Prompt, Resource, Tool};

use crate::agent::FlatError;
use crate::exec::TurnExecutor;
use crate::ledger::Ledger;
use crate::notify::Notifier;

/// Boxed future, so the trait stays object-safe without an async-trait
/// dependency. Plugins write `Box::pin(async move { .. })`.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One schema change, applied once and recorded.
///
/// Plugins get the pool, not a CRUD facade, because every plugin here
/// needs SQL a key-value API cannot express: UPSERT with COALESCE,
/// `MAX(seq) + 1` sequence allocation, LIKE search, range scans for
/// due work, aggregates for health, subquery DELETE for retention. The
/// admission guard that makes turns correct is itself a subquery
/// inside an INSERT. Taking SQL away would push those into application
/// code and reintroduce the races the reviews already found.
///
/// What plugins *were* missing is this: schema evolution. Before it,
/// `Ledger::setup` ran `ALTER TABLE` statements with `let _ =`,
/// swallowing every error including the real ones.
pub struct Migration {
    /// Stable identifier, recorded once applied. Never rename one.
    pub name: &'static str,
    pub sql: &'static str,
    /// Tolerate "duplicate column name" from this statement, for
    /// `ALTER TABLE ADD COLUMN` against a database that predates
    /// migration tracking. Nothing else is ever tolerated.
    pub tolerate_existing_column: bool,
}

impl Migration {
    pub const fn new(name: &'static str, sql: &'static str) -> Self {
        Self {
            name,
            sql,
            tolerate_existing_column: false,
        }
    }

    pub const fn add_column(name: &'static str, sql: &'static str) -> Self {
        Self {
            name,
            sql,
            tolerate_existing_column: true,
        }
    }
}

/// Apply an owner's outstanding migrations in order, recording each.
/// Used by core for its own schema and by the host for every plugin,
/// so there is one mechanism rather than two.
pub async fn apply_migrations(
    pool: &SqlitePool,
    owner: &str,
    migrations: &[Migration],
) -> Result<usize, FlatError> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             owner TEXT NOT NULL,
             name TEXT NOT NULL,
             applied_unix INTEGER NOT NULL,
             PRIMARY KEY (owner, name))",
    )
    .execute(pool)
    .await?;

    let mut applied = 0;
    for migration in migrations {
        let (done,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM schema_migrations WHERE owner = ?1 AND name = ?2")
                .bind(owner)
                .bind(migration.name)
                .fetch_one(pool)
                .await?;
        if done > 0 {
            continue;
        }
        if let Err(e) = sqlx::query(migration.sql).execute(pool).await {
            let existing_column = migration.tolerate_existing_column
                && e.to_string().contains("duplicate column name");
            if !existing_column {
                return Err(format!("migration {owner}/{}: {e}", migration.name).into());
            }
        }
        sqlx::query(
            "INSERT INTO schema_migrations (owner, name, applied_unix) VALUES (?1, ?2, ?3)",
        )
        .bind(owner)
        .bind(migration.name)
        .bind(crate::time::now_unix())
        .execute(pool)
        .await?;
        applied += 1;
    }
    Ok(applied)
}

/// Which MCP surface a tool is being asked for.
///
/// This distinction is not theoretical: `kill`, `prune`,
/// `resolve_finding`, and the schedule tools all ended up
/// operator-only across earlier stages, each for its own reason
/// (stopping paid work, deleting history, adjudicating the system's
/// own reports, committing to standing spend). Encoding it in the
/// trait means a plugin declares its blast radius rather than the
/// server remembering to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// stdio: the person running the server.
    Operator,
    /// Loopback HTTP: the agents themselves.
    Agent,
}

/// One block of the board, pre-rendered.
///
/// HTML rather than a structured model on purpose: there is exactly
/// one renderer, and the structured path already exists as each
/// plugin's JSON resources. Machines read `resources()`; people read
/// this. If a second renderer ever appears, that is when the model
/// earns its keep.
pub struct Section {
    pub title: String,
    pub html: String,
}

/// The outcome of poking an agent from outside.
///
/// `Busy` is separated from `Failed` because it is not an error and
/// the cron plugin learned that the hard way: an agent already
/// mid-turn should be skipped and counted, never queued into a pileup
/// and never reported as a failure.
#[derive(Debug, Clone)]
pub enum Submission {
    Submitted {
        seq: i64,
    },
    Busy {
        reason: String,
    },
    /// The rolling day is over the stop threshold. Distinct from
    /// `Failed` because nothing is broken and the caller should try
    /// again tomorrow, or raise the limit.
    OverBudget {
        spent_usd: f64,
        limit_usd: f64,
    },
    Failed {
        reason: String,
    },
}

impl Submission {
    pub fn submitted(&self) -> Option<i64> {
        match self {
            Submission::Submitted { seq } => Some(*seq),
            _ => None,
        }
    }
}

/// What core hands a plugin. Also the definition of core: if a plugin
/// needs something not on here, either it belongs in core or the
/// plugin is reaching.
#[derive(Clone)]
pub struct PluginContext {
    pub pool: SqlitePool,
    pub ledger: Ledger,
    pub exec: Arc<dyn TurnExecutor>,
    pub notify: Notifier,
    /// Where the database lives, for anything that needs to size it.
    pub db_path: String,
    /// Path to the MCP config pointing back at this server, for
    /// plugins that provision agents with loopback access.
    pub loopback_mcp_config: String,
    /// The same server at its operator mount, where the tools that act
    /// on the world live (`kill`, `open_pr`, `prune`). Empty when the
    /// binary has not supplied one, and a role asking for it then falls
    /// back to the agent surface, which is the safe direction.
    pub operator_mcp_config: String,
    /// The `[plugins]` table from config, so a plugin reads its own
    /// settings without `main` knowing its shape. This is what makes a
    /// third-party plugin installable rather than wired in by hand.
    pub plugin_config: toml::Value,
    /// Circuit breakers, enforced in `submit_turn` so every event
    /// source inherits them without knowing they exist.
    pub limits: crate::limits::Limits,
    /// Server-wide agent defaults, so a plugin building its own roles
    /// inherits them instead of quietly opting out.
    pub runtime: crate::roles::Runtime,
}

impl PluginContext {
    /// This plugin's `[plugins.<name>]` section, if any.
    pub fn config_for(&self, plugin: &str) -> Option<&toml::Value> {
        self.plugin_config.get(plugin)
    }

    /// Poke an agent from outside. Thin wrapper over [`submit`]; see
    /// there for why every path must funnel through one function.
    pub async fn submit_turn(&self, agent_id: &str, text: &str, source: &str) -> Submission {
        submit(
            &self.ledger,
            self.exec.as_ref(),
            &self.notify,
            &self.limits,
            agent_id,
            text,
            source,
        )
        .await
    }
}

/// The single admission point: limits, enqueue, dispatch, notify.
///
/// This exists as a free function because the first version put the
/// budget check on `PluginContext` alone, and the `send` tool does not
/// have a `PluginContext`. The limit therefore governed cron and
/// webhooks while the primary path walked straight past it, and the
/// demo happily spent four times the configured stop. **A circuit
/// breaker on one path is not a circuit breaker.** Everything that can
/// start a turn calls this.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip_all, fields(agent = %agent_id, source, outcome = tracing::field::Empty))]
pub async fn submit(
    ledger: &Ledger,
    exec: &dyn TurnExecutor,
    notify: &Notifier,
    limits: &crate::limits::Limits,
    agent_id: &str,
    text: &str,
    source: &str,
) -> Submission {
    // Checked before enqueuing, never against running work: an
    // in-flight turn finishes and is recorded whatever the rolling
    // total says. Stopping mid-supervision would leave a half-finished
    // branch and an agent that never learns how it ended, which is
    // worse than the overspend.
    let warn = limits.warn_micro_usd();
    let stop = limits.stop_micro_usd();
    if warn.is_some() || stop.is_some() {
        let since = crate::time::now_unix() - 86_400;
        let spent = ledger.spend_since(since).await.unwrap_or_default();
        if let Some(limit) = stop.filter(|limit| spent >= *limit) {
            let (spent_usd, limit_usd) = (spent as f64 / 1e6, limit as f64 / 1e6);
            notify.turn(
                LogLevel::Error,
                agent_id,
                0,
                "over_budget",
                &format!(
                    "{source}: ${spent_usd:.2} spent in 24h, limit ${limit_usd:.2}; \
                     new submissions refused until it falls below"
                ),
            );
            return Submission::OverBudget {
                spent_usd,
                limit_usd,
            };
        }
        if let Some(warn) = warn.filter(|warn| spent >= *warn) {
            let stop_detail = stop
                .map(|limit| format!(", stop at ${:.2}", limit as f64 / 1e6))
                .unwrap_or_default();
            notify.turn(
                LogLevel::Warning,
                agent_id,
                0,
                "budget_warning",
                &format!(
                    "{source}: ${:.2} spent in 24h, warning at ${:.2}{stop_detail}",
                    spent as f64 / 1e6,
                    warn as f64 / 1e6,
                ),
            );
        }
    }

    match ledger.enqueue_turn(agent_id, text).await {
        Ok(seq) => {
            exec.submit(agent_id.to_string(), seq);
            tracing::Span::current().record("outcome", "submitted");
            tracing::info!(agent = %agent_id, seq, source, "turn submitted");
            notify.turn(LogLevel::Info, agent_id, seq, "submitted", source);
            Submission::Submitted { seq }
        }
        Err(e) => {
            let reason = e.to_string();
            if reason.contains("in flight") {
                notify.turn(
                    LogLevel::Warning,
                    agent_id,
                    0,
                    "skipped",
                    &format!("{source}: busy"),
                );
                Submission::Busy { reason }
            } else {
                notify.turn(
                    LogLevel::Error,
                    agent_id,
                    0,
                    "rejected",
                    &format!("{source}: {reason}"),
                );
                Submission::Failed { reason }
            }
        }
    }
}

pub trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;

    /// Tables this plugin owns. Declared so the host can reject two
    /// plugins claiming the same one, which is the failure that
    /// actually happens. It is hygiene, not a boundary: a compile-in
    /// plugin already has the whole process, so restricting its SQL
    /// would buy no safety. A real boundary is a separate MCP server.
    fn tables(&self) -> &'static [&'static str] {
        &[]
    }

    /// Schema, applied once each in order and recorded by
    /// `(plugin, name)`. Append; never rewrite or rename.
    fn migrations(&self) -> &'static [Migration] {
        &[]
    }

    /// Take handles from `ctx`. Runs after this plugin's migrations
    /// and before any surface is built. No DDL belongs here.
    fn setup<'a>(&'a mut self, ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>>;

    fn tools(&self, _surface: Surface) -> Vec<Tool> {
        Vec::new()
    }

    /// Claim part of a declared agent's configuration.
    ///
    /// `[[agents]]` describes an agent, and some of what it describes is
    /// not core's business. A schedule is the standing example: it is
    /// written where the agent is declared because that is where a
    /// person wants it, and it belongs to the schedule plugin.
    ///
    /// Without this the binary's config pass has to hold a handle to
    /// each plugin whose config it might encounter, which is a
    /// privileged path of exactly the kind this trait exists to avoid,
    /// and which core cannot extend to a plugin it has never heard of.
    ///
    /// Called once per declared agent, after the agent exists, with the
    /// value under `[agents.plugins.<name>]` if there is one. Absent
    /// means nothing to do, so the default is to do nothing.
    ///
    /// Should be idempotent: config is applied at every boot, and the
    /// agent it describes may already carry the result of the last one.
    fn agent_config<'a>(
        &'a self,
        _agent_id: &'a str,
        _section: &'a toml::Value,
    ) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async { Ok(()) })
    }

    fn resources(&self) -> Vec<Resource> {
        Vec::new()
    }

    fn prompts(&self) -> Vec<Prompt> {
        Vec::new()
    }

    /// Roles this plugin ships. Merged with the ones in config, which
    /// win on a name collision so an operator can always override.
    ///
    /// This is what lets a plugin be a whole capability rather than a
    /// tool bag: tools and the prompt that knows how to use them,
    /// installed together. The pairing is not cosmetic, since a prompt
    /// that assumes tools it was not given is exactly the failure the
    /// findings queue caught twice.
    fn roles(&self) -> Vec<crate::roles::Role> {
        Vec::new()
    }

    /// A block for the human view. `None` to contribute nothing.
    fn board_section(&self) -> BoxFut<'_, Option<Section>> {
        Box::pin(async { None })
    }

    /// HTTP routes this plugin owns, merged into the server's router.
    /// Two uses so far and they are the same mechanism: the kanban's
    /// detail page at `/board/item/{id}`, and a webhook's inbound
    /// endpoint. Named for what it is rather than for the board, since
    /// receiving a POST is as valid as rendering a page.
    fn routes(&self) -> Option<Router> {
        None
    }

    /// This plugin's slice of the size report, merged into one object
    /// under the plugin's name.
    fn health(&self) -> BoxFut<'_, Value> {
        Box::pin(async { json!({}) })
    }

    /// Drop or blank what is safely droppable older than `cutoff`
    /// (unix seconds). Retention belongs to whoever owns the table.
    fn prune(&self, _cutoff: i64) -> BoxFut<'_, Value> {
        Box::pin(async { json!({}) })
    }

    /// Start background work (a scheduler loop, a watcher). Called
    /// once, after every plugin's `setup`.
    fn start(&self, _ctx: &PluginContext) {}
}

/// Holds the registered plugins and fans the cross-cutting calls out.
pub struct PluginHost {
    plugins: Vec<Box<dyn Plugin>>,
}

impl PluginHost {
    /// Offer one declared agent's plugin sections to the plugins that
    /// own them.
    ///
    /// The binary calls this rather than holding a handle per plugin.
    /// A section naming a plugin that is not loaded is an error rather
    /// than a shrug: it is almost always a typo, and silently ignoring
    /// it means a schedule that never fires and no way to find out why.
    pub async fn apply_agent_config(
        &self,
        agent_id: &str,
        sections: &std::collections::BTreeMap<String, toml::Value>,
    ) -> Result<(), FlatError> {
        for (name, section) in sections {
            let plugin = self
                .plugins
                .iter()
                .find(|p| p.name() == name)
                .ok_or_else(|| -> FlatError {
                    format!(
                        "agent config names plugin '{name}', which is not loaded.                          Loaded: {}",
                        self.plugins
                            .iter()
                            .map(|p| p.name())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                    .into()
                })?;
            plugin.agent_config(agent_id, section).await?;
        }
        Ok(())
    }

    /// Set every plugin up in registration order, then start their
    /// background work. Setup is separated from start so a plugin's
    /// loop can rely on every other plugin's tables existing.
    pub async fn setup(
        mut plugins: Vec<Box<dyn Plugin>>,
        ctx: &PluginContext,
    ) -> Result<Self, FlatError> {
        // Reject collisions before touching the database: two plugins
        // owning one table is a bug that would otherwise show up as
        // mysterious data loss much later.
        let mut claimed: Vec<(&str, &str)> = Vec::new();
        for plugin in &plugins {
            for table in plugin.tables() {
                if let Some((other, _)) = claimed.iter().find(|(_, t)| t == table) {
                    return Err(format!(
                        "plugins '{}' and '{}' both claim table '{table}'",
                        other,
                        plugin.name()
                    )
                    .into());
                }
                claimed.push((plugin.name(), table));
            }
        }

        // A config section nobody claims is almost always a typo, and
        // it fails silently: the plugin sees no config and quietly
        // does nothing. Warn rather than refuse, because commenting a
        // plugin out of the list while keeping its config is a
        // legitimate thing to do mid-development. Malformed config for
        // a plugin that IS registered still refuses to boot, since
        // that plugin genuinely cannot function.
        if let Some(sections) = ctx.plugin_config.as_table() {
            let known: Vec<&str> = plugins.iter().map(|p| p.name()).collect();
            for section in sections.keys() {
                if !known.contains(&section.as_str()) {
                    eprintln!(
                        "[plugins] warning: config section '{section}' matches no registered \
                         plugin; known plugins are {}",
                        known.join(", ")
                    );
                }
            }
        }

        // The key-value table core lends to plugins that want the easy
        // path, applied before anyone asks for a Store.
        apply_migrations(&ctx.pool, "store", crate::store::MIGRATIONS).await?;

        for plugin in &mut plugins {
            let name = plugin.name();
            let applied = apply_migrations(&ctx.pool, name, plugin.migrations())
                .await
                .map_err(|e| -> FlatError { format!("plugin '{name}': {e}").into() })?;
            if applied > 0 {
                eprintln!("[plugins] {name}: applied {applied} migration(s)");
            }
            plugin
                .setup(ctx)
                .await
                .map_err(|e| -> FlatError { format!("plugin '{name}': {e}").into() })?;
        }
        for plugin in &plugins {
            plugin.start(ctx);
        }
        Ok(Self { plugins })
    }

    /// Roles contributed by plugins, in registration order. Config
    /// roles are merged over these by the caller, so an operator can
    /// override a shipped role by declaring one with the same name.
    pub fn roles(&self) -> Vec<crate::roles::Role> {
        self.plugins.iter().flat_map(|p| p.roles()).collect()
    }

    /// Plugin roles plus config roles, config winning on name.
    pub fn merged_roles(&self, config_roles: &[crate::roles::Role]) -> Vec<crate::roles::Role> {
        let mut merged: Vec<crate::roles::Role> = self
            .roles()
            .into_iter()
            .filter(|shipped| !config_roles.iter().any(|c| c.name == shipped.name))
            .collect();
        merged.extend(config_roles.iter().cloned());
        merged
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.plugins.iter().map(|p| p.name()).collect()
    }

    /// Install every plugin's contribution onto a router for one
    /// surface. This replaces the per-plugin registration loops that
    /// `main` used to carry, three lines each.
    pub fn install(&self, mut router: McpRouter, surface: Surface) -> McpRouter {
        for plugin in &self.plugins {
            for tool in plugin.tools(surface) {
                router = router.tool(tool);
            }
            for resource in plugin.resources() {
                router = router.resource(resource);
            }
            for prompt in plugin.prompts() {
                router = router.prompt(prompt);
            }
        }
        router
    }

    /// Every plugin's own HTTP routes, merged.
    pub fn routes(&self) -> Router {
        let mut router = Router::new();
        for plugin in &self.plugins {
            if let Some(routes) = plugin.routes() {
                router = router.merge(routes);
            }
        }
        router
    }

    pub async fn board_sections(&self) -> Vec<Section> {
        let mut sections = Vec::new();
        for plugin in &self.plugins {
            if let Some(section) = plugin.board_section().await {
                sections.push(section);
            }
        }
        sections
    }

    /// Every plugin's stats, keyed by plugin name.
    pub async fn health(&self) -> Value {
        let mut out = serde_json::Map::new();
        for plugin in &self.plugins {
            let stats = plugin.health().await;
            if stats.as_object().is_some_and(|o| !o.is_empty()) {
                out.insert(plugin.name().to_string(), stats);
            }
        }
        Value::Object(out)
    }

    pub async fn prune(&self, cutoff: i64) -> Value {
        let mut out = serde_json::Map::new();
        for plugin in &self.plugins {
            let report = plugin.prune(cutoff).await;
            if report.as_object().is_some_and(|o| !o.is_empty()) {
                out.insert(plugin.name().to_string(), report);
            }
        }
        Value::Object(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingExecutor(Mutex<Vec<(String, i64)>>);

    impl TurnExecutor for RecordingExecutor {
        fn submit(&self, agent_id: String, seq: i64) {
            self.0
                .lock()
                .expect("recording executor")
                .push((agent_id, seq));
        }

        fn kill(&self, _agent_id: &str, _seq: i64) -> bool {
            false
        }

        fn name(&self) -> &'static str {
            "recording"
        }
    }

    struct Fake(&'static str, &'static [&'static str]);

    impl Plugin for Fake {
        fn name(&self) -> &'static str {
            self.0
        }
        fn tables(&self) -> &'static [&'static str] {
            self.1
        }
        fn setup<'a>(&'a mut self, _ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
            Box::pin(async { Ok(()) })
        }
    }

    async fn ctx() -> PluginContext {
        let pool = SqlitePool::connect("sqlite::memory:").await.expect("pool");
        let ledger = Ledger::setup(pool.clone()).await.expect("ledger");
        let (tx, _rx) = tower_mcp::context::notification_channel(8);
        let notify = Notifier(tx);
        let exec = crate::exec::HandExecutor::start(ledger.clone(), notify.clone(), 1);
        PluginContext {
            pool,
            ledger,
            exec,
            notify,
            db_path: String::new(),
            loopback_mcp_config: String::new(),
            operator_mcp_config: String::new(),
            plugin_config: toml::Value::Table(toml::map::Map::new()),
            limits: Default::default(),
            runtime: Default::default(),
        }
    }

    #[tokio::test]
    async fn two_plugins_claiming_one_table_is_refused() {
        let ctx = ctx().await;
        let err = PluginHost::setup(
            vec![
                Box::new(Fake("alpha", &["shared", "alpha_only"])),
                Box::new(Fake("beta", &["beta_only", "shared"])),
            ],
            &ctx,
        )
        .await
        .map(|_| ())
        .expect_err("collision must be refused");
        let message = err.to_string();
        assert!(message.contains("alpha"), "{message}");
        assert!(message.contains("beta"), "{message}");
        assert!(message.contains("shared"), "{message}");
    }

    #[tokio::test]
    async fn distinct_tables_are_accepted() {
        let ctx = ctx().await;
        let host = PluginHost::setup(
            vec![
                Box::new(Fake("alpha", &["alpha_only"])),
                Box::new(Fake("beta", &["beta_only"])),
            ],
            &ctx,
        )
        .await
        .expect("no collision");
        assert_eq!(host.names(), vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn migrations_run_once_and_are_recorded() {
        let ctx = ctx().await;
        const M: &[Migration] = &[Migration::new(
            "0001_probe",
            "CREATE TABLE probe (id TEXT PRIMARY KEY)",
        )];
        assert_eq!(
            apply_migrations(&ctx.pool, "probe", M)
                .await
                .expect("first"),
            1
        );
        // Re-running must not re-execute: the CREATE would fail if it did.
        assert_eq!(
            apply_migrations(&ctx.pool, "probe", M)
                .await
                .expect("second"),
            0
        );
    }

    /// A plugin gets its own section and no one else's.
    #[tokio::test]
    async fn agent_config_reaches_the_plugin_that_owns_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static SEEN: AtomicUsize = AtomicUsize::new(0);

        struct Claimer;
        impl Plugin for Claimer {
            fn name(&self) -> &'static str {
                "claimer"
            }
            fn setup<'a>(&'a mut self, _c: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
                Box::pin(async { Ok(()) })
            }
            fn agent_config<'a>(
                &'a self,
                _agent_id: &'a str,
                section: &'a toml::Value,
            ) -> BoxFut<'a, Result<(), FlatError>> {
                Box::pin(async move {
                    assert_eq!(section.get("k").and_then(|v| v.as_str()), Some("v"));
                    SEEN.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            }
        }

        let ctx = ctx().await;
        let host = PluginHost::setup(
            vec![Box::new(Claimer), Box::new(Fake("bystander", &[]))],
            &ctx,
        )
        .await
        .expect("setup");

        let mut sections = std::collections::BTreeMap::new();
        sections.insert(
            "claimer".to_string(),
            toml::from_str::<toml::Value>("k = \"v\"").expect("value"),
        );
        host.apply_agent_config("agent-1", &sections)
            .await
            .expect("apply");
        assert_eq!(
            SEEN.load(Ordering::SeqCst),
            1,
            "the owner sees it exactly once"
        );
    }

    /// Naming a plugin that is not loaded is a typo, and a typo that is
    /// ignored is a schedule that never fires with nothing to explain
    /// why.
    #[tokio::test]
    async fn agent_config_for_an_unknown_plugin_is_refused() {
        let ctx = ctx().await;
        let host = PluginHost::setup(vec![Box::new(Fake("real", &[]))], &ctx)
            .await
            .expect("setup");
        let mut sections = std::collections::BTreeMap::new();
        sections.insert("shcedule".to_string(), toml::Value::Boolean(true));
        let err = host
            .apply_agent_config("agent-1", &sections)
            .await
            .expect_err("a typo must not pass silently");
        assert!(format!("{err}").contains("shcedule"), "names the offender");
    }

    #[tokio::test]
    async fn warn_only_spend_limit_emits_a_warning_and_allows_the_turn() {
        use crate::agent::AgentDef;
        use crate::limits::Limits;
        use tower_mcp::ServerNotification;

        let pool = SqlitePool::connect("sqlite::memory:").await.expect("pool");
        let ledger = Ledger::setup(pool.clone()).await.expect("ledger");
        let agent_id = ledger
            .create_agent(&AgentDef::new("target", "system"), None)
            .await
            .expect("agent");
        sqlx::query(
            "INSERT INTO turns
                 (agent_id, seq, prompt, state, cost_micro_usd, at_unix)
             VALUES ('previous', 1, 'spent', 'ok', 2000000, ?1)",
        )
        .bind(crate::time::now_unix())
        .execute(&pool)
        .await
        .expect("record spend");
        let (tx, mut rx) = tower_mcp::context::notification_channel(8);
        let notify = Notifier(tx);
        let exec = RecordingExecutor::default();
        let limits = Limits {
            daily_warn_usd: Some(1.0),
            daily_stop_usd: None,
            ..Default::default()
        };

        let outcome = submit(
            &ledger, &exec, &notify, &limits, &agent_id, "continue", "test",
        )
        .await;
        assert!(
            matches!(outcome, Submission::Submitted { .. }),
            "{outcome:?}"
        );
        assert_eq!(exec.0.lock().expect("recorded").len(), 1);

        let notification = rx.recv().await.expect("budget warning");
        let ServerNotification::LogMessage(message) = notification else {
            panic!("expected a log notification");
        };
        assert_eq!(message.data["state"], "budget_warning");
        assert!(
            message.data["detail"]
                .as_str()
                .is_some_and(|detail| !detail.contains("stop at")),
            "warn-only detail must not invent a stop threshold: {}",
            message.data
        );
    }
}
