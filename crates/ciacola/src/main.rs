//! ciacola: a laptop-local agent server.
//!
//! Everything that is not core is now a plugin, including the parts we
//! lean on hardest: kanban, memory, findings, schedules, roles. They go
//! through the same trait a third party would, which is the only way to
//! keep that trait honest.
//!
//! Compare this `main` with flat12's. There, every plugin cost three
//! registration loops per surface plus its own constructor threaded
//! into the board; adding one touched five places. Here it is a line in
//! a `Vec`, and the board, the health report, and the retention pass
//! pick it up because the plugin contributes to each.
//!
//! What stayed in core is the answer to "where is the line": the
//! provider seam, the ledger, an executor, notifications, recovery, the
//! six verbs, and the board shell. That is exactly [`PluginContext`].
//!
//! ```bash
//! mcp-repl -- cargo run -p ciacola
//! ```

use std::sync::Arc;
use std::time::Duration;

use apalis_sqlite::SqlitePool;
use ciacola_core::exec::TurnExecutor;
use ciacola_core::health::{
    Health, operator_tools as health_operator_tools, resources as health_resources,
    tools as health_tools,
};
use ciacola_core::plugin::{Plugin, PluginContext, PluginHost, Surface};
use ciacola_core::roles::RolesPlugin;
use ciacola_core::{HandExecutor, Ledger, Notifier, board, recover, server};
use ciacola_findings::FindingsPlugin;
use ciacola_git::GitPlugin;
use ciacola_kanban::{KanbanPlugin, agents_resource};
use ciacola_memory::MemoryPlugin;
use ciacola_refs::RefsPlugin;
use ciacola_repo_worker::RepoWorkerPlugin;
use ciacola_schedule::SchedulePlugin;
use ciacola_schedule::Schedules;
use ciacola_tuning::TuningPlugin;
use ciacola_webhook::WebhookPlugin;

mod config;
use tower_mcp::context::notification_channel;
use tower_mcp::transport::{GenericStdioTransport, HttpTransport};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Seeded now so the split into real crates inherits instrumentation
    // rather than needing it retrofitted. Off unless RUST_LOG asks, and
    // to stderr because stdout is the MCP transport.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    #[tracing::instrument]
    fn probe_span(n: i32) {}
    probe_span(1);
    let path = std::env::var("CIACOLA_DB").unwrap_or_else(|_| {
        std::env::temp_dir()
            .join("ciacola.db")
            .display()
            .to_string()
    });
    let concurrency: usize = std::env::var("CIACOLA_CONCURRENCY")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(4);
    let port: u16 = std::env::var("CIACOLA_HTTP")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4823);
    let config_path =
        std::env::var("CIACOLA_CONFIG").unwrap_or_else(|_| "spike/flat12.toml".to_string());

    let pool = SqlitePool::connect(&format!("sqlite://{path}?mode=rwc")).await?;
    let declared_early = config::load(&config_path)?;
    let ledger = Ledger::setup(pool.clone())
        .await?
        .with_runtime(declared_early.runtime.clone())?;

    let (tx, rx) = notification_channel(64);
    let notify = Notifier(tx);
    let exec = HandExecutor::start(ledger.clone(), notify.clone(), concurrency);
    let drain_exec = exec.clone();

    if std::env::var("CIACOLA_NO_RECOVER").is_err() {
        let report = recover::recover(&ledger, exec.as_ref()).await?;
        eprintln!(
            "[ciacola] recovery: {} resubmitted, {} orphaned, {} orphan processes killed, {} unverified",
            report.resubmitted, report.orphaned, report.orphans_killed, report.orphans_unverified
        );
    }

    let mcp_config_path = std::env::temp_dir().join("ciacola-mcp.json");
    std::fs::write(
        &mcp_config_path,
        format!(
            "{{\"mcpServers\": {{\"ciacola\": {{\"type\": \"http\", \"url\": \"http://127.0.0.1:{port}/mcp\"}}}}}}"
        ),
    )?;

    let declared = declared_early;
    let ctx = PluginContext {
        pool: pool.clone(),
        ledger: ledger.clone(),
        exec: exec.clone(),
        notify: notify.clone(),
        db_path: path.clone(),
        loopback_mcp_config: mcp_config_path.display().to_string(),
        plugin_config: declared.plugins.clone(),
        limits: declared.limits.clone(),
        runtime: declared.runtime.clone(),
    };
    declared.runtime.check_claude_home();
    eprintln!("[ciacola] limits: {}", declared.limits.summary());

    // The whole registration surface. Order is contribution order:
    // board sections and health keys appear in this sequence.
    //
    // Roles are collected before setup on purpose: `roles()` needs no
    // state, so the shipped ones can be merged with the operator's and
    // handed to RolesPlugin in the same list. An earlier version built
    // a second host for that, which re-ran the unclaimed-config-section
    // check against a one-plugin list and warned about every real
    // section.
    let mut plugins: Vec<Box<dyn Plugin>> = vec![
        Box::new(KanbanPlugin::default()),
        Box::new(FindingsPlugin::default()),
        Box::new(SchedulePlugin::default()),
        Box::new(MemoryPlugin::default()),
        Box::new(RefsPlugin::default()),
        Box::new(GitPlugin::default()),
        Box::new(WebhookPlugin::default()),
        Box::new(RepoWorkerPlugin::default()),
        // Stateless analysis over core's tables: what each model
        // actually costs and achieves.
        Box::new(TuningPlugin::default()),
    ];
    let shipped: Vec<_> = plugins.iter().flat_map(|p| p.roles()).collect();
    let merged_roles: Vec<_> = shipped
        .into_iter()
        .filter(|s| !declared.roles.iter().any(|c| c.name == s.name))
        .chain(declared.roles.iter().cloned())
        .collect();
    eprintln!(
        "[ciacola] roles: {}",
        merged_roles
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    plugins.push(Box::new(RolesPlugin::new(merged_roles)));

    let host = Arc::new(PluginHost::setup(plugins, &ctx).await?);
    eprintln!("[ciacola] plugins: {}", host.names().join(", "));

    // Known boundary crossing: agent config declares schedules, so
    // core's config pass needs a handle to a plugin's table. Setup is
    // idempotent DDL, so a second handle is harmless, but a real design
    // would give plugins a config hook instead.
    for line in config::apply(
        &declared,
        &ledger,
        &Schedules::setup(pool.clone()).await?,
        &mcp_config_path.display().to_string(),
    )
    .await?
    {
        eprintln!("[ciacola] config: {line}");
    }

    let health = Health::new(pool, path).with_host(host.clone());

    // Core verbs, then every plugin's contribution for this surface.
    let mut stdio_router = server::router_with_limits(
        ledger.clone(),
        exec.clone(),
        notify.clone(),
        true,
        declared.limits.clone(),
    )
    .resource(agents_resource(ledger.clone()));
    stdio_router = host.install(stdio_router, Surface::Operator);
    for tool in health_tools(health.clone()) {
        stdio_router = stdio_router.tool(tool);
    }
    for tool in health_operator_tools(health.clone()) {
        stdio_router = stdio_router.tool(tool);
    }
    for resource in health_resources(health.clone()) {
        stdio_router = stdio_router.resource(resource);
    }

    let mut agent_router =
        server::router_with_limits(ledger.clone(), exec, notify, false, declared.limits.clone())
            .resource(agents_resource(ledger.clone()));
    agent_router = host.install(agent_router, Surface::Agent);
    for tool in health_tools(health.clone()) {
        agent_router = agent_router.tool(tool);
    }
    for resource in health_resources(health) {
        agent_router = agent_router.resource(resource);
    }

    let http = HttpTransport::new(agent_router)
        .into_router_at("/mcp")
        .merge(board::router_with_limits(ledger, host, declared.limits));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, http).await {
            eprintln!("[ciacola] http: {e}");
        }
    });
    eprintln!("[ciacola] board at http://127.0.0.1:{port}/board, agents' tools at /mcp");

    // Ctrl-C drains rather than kills. Without this, a signal ends a
    // twenty minute agent run mid-flight: recovery tidies up on the
    // next boot, but it tidies up by killing the provider and marking
    // the turn failed, so the work and the money are both gone.
    let mut transport = GenericStdioTransport::with_notifications(stdio_router, rx);
    tokio::select! {
        result = transport.run() => { result?; }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\n[ciacola] draining, in-flight turns finish (Ctrl-C again to abandon them)");
            tokio::select! {
                left = drain_exec.drain(Duration::from_secs(600)) => {
                    if left > 0 {
                        eprintln!("[ciacola] gave up with {left} turn(s) still running; recovery will adjudicate them");
                    } else {
                        eprintln!("[ciacola] drained clean");
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("[ciacola] abandoning in-flight turns");
                }
            }
            // Exit explicitly rather than returning: the stdio
            // transport reads stdin on a blocking thread that never
            // finishes, and tokio waits for blocking tasks at
            // shutdown, so a plain return hangs forever. The work we
            // cared about is already drained.
            std::process::exit(0);
        }
    }
    Ok(())
}
