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

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use ciacola_core::health::{
    Health, operator_tools as health_operator_tools, resources as health_resources,
    tools as health_tools,
};
use ciacola_core::plugin::{Plugin, PluginContext, PluginHost, Surface};
use ciacola_core::roles::RolesPlugin;
use ciacola_core::{
    DispatchReadiness, HandExecutor, Ledger, Notifier, PollingExecutor, TurnExecutor, recover,
    server,
};
use ciacola_findings::FindingsPlugin;
use ciacola_git::GitPlugin;
use ciacola_kanban::{KanbanPlugin, agents_resource};
use ciacola_memory::MemoryPlugin;
use ciacola_refs::RefsPlugin;
use ciacola_repo_worker::RepoWorkerPlugin;
use ciacola_schedule::SchedulePlugin;
use ciacola_tuning::TuningPlugin;
use ciacola_webhook::WebhookPlugin;
use sqlx::SqlitePool;

mod config;
mod operator_auth;
use tower_mcp::context::notification_channel;
use tower_mcp::transport::{GenericStdioTransport, HttpTransport};

#[derive(Debug, PartialEq, Eq)]
struct DatabasePath {
    path: PathBuf,
    temporary_fallback: bool,
}

fn non_empty_path(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

fn resolve_database_path(
    explicit: Option<OsString>,
    xdg_data_home: Option<OsString>,
    home: Option<OsString>,
    temp_dir: &Path,
) -> DatabasePath {
    if let Some(path) = non_empty_path(explicit) {
        return DatabasePath {
            path,
            temporary_fallback: false,
        };
    }

    if let Some(path) = non_empty_path(xdg_data_home) {
        return DatabasePath {
            path: path.join("ciacola").join("ciacola.db"),
            temporary_fallback: false,
        };
    }

    if let Some(path) = non_empty_path(home) {
        return DatabasePath {
            path: path
                .join(".local")
                .join("share")
                .join("ciacola")
                .join("ciacola.db"),
            temporary_fallback: false,
        };
    }

    DatabasePath {
        path: temp_dir.join("ciacola.db"),
        temporary_fallback: true,
    }
}

fn write_loopback_configs(directory: &Path, port: u16) -> std::io::Result<PathBuf> {
    let config = format!(
        "{{\"mcpServers\": {{\"ciacola\": {{\"type\": \"http\", \"url\": \"http://127.0.0.1:{port}/mcp\"}}}}}}"
    );
    let agent = directory.join("ciacola-mcp.json");
    publish_loopback_config(&agent, &config)?;

    // Definitions persisted before authenticated operator HTTP may still
    // name this path. Keep the file for an upgrade window, but point it at
    // the ordinary mount. A legacy supervisor loses authority safely instead
    // of failing its paid turn on a missing file or retaining anonymous root.
    publish_loopback_config(&directory.join("ciacola-mcp-operator.json"), &config)?;
    Ok(agent)
}

/// Publish a shared, non-secret endpoint description without following a
/// stale symlink at the predictable compatibility path. `NamedTempFile`
/// creates a randomized 0600 file in the same directory; `persist` atomically
/// replaces the directory entry, so a symlink is replaced rather than opened.
fn publish_loopback_config(path: &Path, contents: &str) -> std::io::Result<()> {
    let directory = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "loopback config path has no parent directory",
        )
    })?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".ciacola-mcp-")
        .tempfile_in(directory)?;
    temporary.write_all(contents.as_bytes())?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
}

fn provider_protection_summary(
    configured: Option<u64>,
    capability: Option<&ciacola_agent::CeilingCapability>,
) -> String {
    let capability_detail = |capability: &ciacola_agent::CeilingCapability| {
        let cache = match capability.cache_treatment {
            ciacola_agent::CacheTreatment::NotApplicable => "cache not applicable",
            ciacola_agent::CacheTreatment::Included => "cached input included",
            ciacola_agent::CacheTreatment::Excluded => "cached input excluded",
            ciacola_agent::CacheTreatment::ProviderDefinedWithExcludedFallback => {
                "provider-defined units; fallback excludes cached input"
            }
        };
        let boundary = match capability.granularity {
            ciacola_agent::EnforcementGranularity::Exact => "exact enforcement",
            ciacola_agent::EnforcementGranularity::ProviderResponseBoundary => {
                "response-boundary enforcement; in-flight work can overshoot"
            }
        };
        format!("meter {}; {cache}; {boundary}", capability.meter.as_str())
    };
    let amount = |value: u64, capability: &ciacola_agent::CeilingCapability| {
        if capability.meter.as_str().contains("micro_usd") {
            format!("${:.4} ({value} micro-USD)", value as f64 / 1e6)
        } else {
            value.to_string()
        }
    };

    match (configured, capability) {
        (Some(value), Some(capability)) => format!(
            "ENFORCED at {}; {}",
            amount(value, capability),
            capability_detail(capability)
        ),
        (Some(value), None) => format!(
            "UNSUPPORTED: configured at {value} provider-native units; automatic work will be refused"
        ),
        (None, Some(capability)) => format!(
            "UNBOUNDED: no ceiling configured; runtime supports {}",
            capability_detail(capability)
        ),
        (None, None) => {
            "UNBOUNDED: no ceiling configured; runtime declares no enforceable ceiling".into()
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Consume the human HTTP credential before Tokio creates worker threads
    // and before any provider child can inherit the server environment.
    let operator_token = operator_auth::take_from_environment()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(operator_token))
}

async fn run(
    operator_token: Option<operator_auth::HumanOperatorToken>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Seeded now so the split into real crates inherits instrumentation
    // rather than needing it retrofitted. Off unless RUST_LOG asks, and
    // to stderr because stdout is the MCP transport.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_writer(std::io::stderr)
        .init();

    let database = resolve_database_path(
        std::env::var_os("CIACOLA_DB"),
        std::env::var_os("XDG_DATA_HOME"),
        std::env::var_os("HOME"),
        &std::env::temp_dir(),
    );
    if let Some(parent) = database
        .path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    eprintln!("[ciacola] ledger: {}", database.path.display());
    if database.temporary_fallback {
        eprintln!(
            "[ciacola] WARNING: no user data directory found; using a temporary ledger. \
             Set CIACOLA_DB for durable operation."
        );
    }
    let concurrency: usize = std::env::var("CIACOLA_CONCURRENCY")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(4);
    let port: u16 = std::env::var("CIACOLA_HTTP")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4823);
    let config_path = std::env::var("CIACOLA_CONFIG").ok();
    eprintln!(
        "[ciacola] operator HTTP: {}",
        if operator_token.is_some() {
            "human bearer configured"
        } else {
            "human bearer disabled; stdio operator access remains available"
        }
    );

    let pool =
        SqlitePool::connect(&format!("sqlite://{}?mode=rwc", database.path.display())).await?;
    let declared_early = config::load_startup(config_path.as_deref())?;
    eprintln!(
        "[ciacola] config: {}",
        config_path.as_deref().unwrap_or(config::DEFAULT_PATH)
    );
    // Runtime assembly. This is the only place in the workspace that
    // names a backend: core resolves an agent's `provider` key through
    // this registry and never learns what is behind it. A second
    // adapter is one more line here plus one more dependency, which is
    // the whole point of the seam.
    let mut codex_provider = ciacola_agent_codex::CodexProvider::new();
    match codex_provider.detect_cli_capabilities().await {
        Ok((version, status)) => eprintln!("[ciacola] codex CLI {version}: {status:?}"),
        Err(error) => eprintln!(
            "[ciacola] warning: Codex CLI probe failed: {error}; provider remains registered, \
             but configured per-turn protection is unsupported"
        ),
    }
    let providers = ciacola_agent::ProviderRegistry::new()
        .with(std::sync::Arc::new(ciacola_agent_claude::ClaudeProvider))
        .and_then(|providers| providers.with(std::sync::Arc::new(codex_provider)))
        .map_err(|e| -> ciacola_core::FlatError { e.to_string().into() })?;
    declared_early.limits.validate_providers(&providers)?;
    providers
        .get(&declared_early.runtime.default_provider_key())
        .map_err(|error| -> ciacola_core::FlatError { error.to_string().into() })?;
    eprintln!("[ciacola] providers: {}", providers.keys().join(", "));
    for key in providers.keys() {
        let provider = providers
            .get(&ciacola_agent::ProviderKey::new(key.clone()))
            .map_err(|error| -> ciacola_core::FlatError { error.to_string().into() })?;
        let capabilities = provider.capabilities();
        let configured = declared_early.limits.provider(&key).per_turn_ceiling;
        eprintln!(
            "[ciacola] provider {key} per-turn protection: {}",
            provider_protection_summary(configured, capabilities.turn_ceiling.as_ref())
        );
    }

    let ledger = Ledger::setup(pool.clone())
        .await?
        .with_runtime(declared_early.runtime.clone())?
        .with_providers(providers);

    // Publish the internal endpoint before constructing roles that carry it.
    // This describes where the service will be; the closed dispatch boundary
    // below is what prevents a provider from mistaking publication for HTTP
    // readiness.
    let mcp_config_path = write_loopback_configs(&std::env::temp_dir(), port)?;

    let (tx, rx) = notification_channel(64);
    let notify = Notifier(tx);
    let dispatch = DispatchReadiness::closed();
    // Two executors, one trait, and nothing above this line can tell
    // them apart. The polling one rereads the ledger, so a turn queued
    // before a crash is picked up on the next tick without help; the
    // channel one is a little quicker off the mark. Both are constructed
    // now for plugin dependency injection, but the shared boundary keeps
    // them from claiming anything until the complete loopback router is
    // listening and recovery has reconciled the durable ledger.
    let exec: std::sync::Arc<dyn TurnExecutor> =
        if std::env::var("CIACOLA_EXECUTOR").as_deref() == Ok("channel") {
            HandExecutor::start_gated(
                ledger.clone(),
                notify.clone(),
                concurrency,
                dispatch.clone(),
            )
        } else {
            PollingExecutor::start_gated(
                ledger.clone(),
                notify.clone(),
                concurrency,
                std::time::Duration::from_secs(2),
                dispatch.clone(),
            )
        };
    let drain_exec = exec.clone();

    let declared = declared_early;
    declared.runtime.check_provider_homes();
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
    let configured_roles = ciacola_core::roles::Roles::with_runtime(
        merged_roles.clone(),
        mcp_config_path.display().to_string(),
        declared.runtime.clone(),
    );
    plugins.push(Box::new(RolesPlugin::new()));

    let ctx = PluginContext {
        pool: pool.clone(),
        ledger: ledger.clone(),
        exec: exec.clone(),
        notify: notify.clone(),
        db_path: database.path.display().to_string(),
        loopback_mcp_config: mcp_config_path.display().to_string(),
        // Provider-backed operator roles are disabled. Supplying the ordinary
        // path here keeps any compatibility caller on the weaker mount.
        operator_mcp_config: mcp_config_path.display().to_string(),
        plugin_config: declared.plugins.clone(),
        limits: declared.limits.clone(),
        runtime: declared.runtime.clone(),
        roles: configured_roles.clone(),
    };

    let host = Arc::new(PluginHost::setup(plugins, &ctx).await?);
    eprintln!("[ciacola] plugins: {}", host.names().join(", "));

    // The config pass knows about agents and nothing else. Anything a
    // declaration says that belongs to a plugin is handed to that
    // plugin by name, so this no longer needs a handle per plugin it
    // might encounter.
    for line in config::apply(
        &declared,
        &ledger,
        host.as_ref(),
        &configured_roles,
        &mcp_config_path.display().to_string(),
    )
    .await?
    {
        eprintln!("[ciacola] config: {line}");
    }

    let health = Health::new(ledger.clone(), database.path.display().to_string())
        .with_limits(declared.limits.clone())
        .with_host(host.clone());

    // Core verbs, then every plugin's contribution for this surface.
    // Live values for completion/complete, so a generic REPL completes
    // agent ids and role names from this server without knowing what a
    // ciacola is. Enum arguments need no help; they are in the schema.
    let completing = configured_roles;

    let mut stdio_router = server::router_interactive_with_limits(
        ledger.clone(),
        exec.clone(),
        notify.clone(),
        true,
        declared.limits.clone(),
    )
    .resource(agents_resource(ledger.clone()));
    stdio_router = host.install(stdio_router, Surface::Operator);
    stdio_router = ciacola_core::complete::attach(stdio_router, ledger.clone(), completing.clone());
    for tool in health_tools(health.clone()) {
        stdio_router = stdio_router.tool(tool);
    }
    for tool in health_operator_tools(health.clone()) {
        stdio_router = stdio_router.tool(tool);
    }
    for resource in health_resources(health.clone()) {
        stdio_router = stdio_router.resource(resource);
    }

    let mut agent_router = server::router_with_limits(
        ledger.clone(),
        exec.clone(),
        notify.clone(),
        false,
        declared.limits.clone(),
    )
    .resource(agents_resource(ledger.clone()));
    agent_router = host.install(agent_router, Surface::Agent);
    agent_router = ciacola_core::complete::attach(agent_router, ledger.clone(), completing.clone());
    for tool in health_tools(health.clone()) {
        agent_router = agent_router.tool(tool);
    }
    for resource in health_resources(health.clone()) {
        agent_router = agent_router.resource(resource);
    }

    // The operator surface again, this time over human-authenticated HTTP.
    // Same tools as stdio, but a transport consumes its router, so the stdio
    // one cannot be reused here.
    let mut operator_router = server::router_interactive_with_limits(
        ledger.clone(),
        exec.clone(),
        notify,
        true,
        declared.limits.clone(),
    )
    .resource(agents_resource(ledger.clone()));
    operator_router = host.install(operator_router, Surface::Operator);
    operator_router = ciacola_core::complete::attach(operator_router, ledger.clone(), completing);
    for tool in health_tools(health.clone()) {
        operator_router = operator_router.tool(tool);
    }
    for tool in health_operator_tools(health.clone()) {
        operator_router = operator_router.tool(tool);
    }
    for resource in health_resources(health) {
        operator_router = operator_router.resource(resource);
    }

    // Shared with the drain below, so the HTTP side and the turn
    // executor wind down on the same signal: a request in flight when
    // the server stops finishes rather than getting cut, and an agent
    // mid-call against the loopback `/mcp` sees a response rather than
    // a connection error.
    let shutdown = CancellationToken::new();

    // Authentication is scoped to each MCP mount, before tower-mcp can
    // initialize a session or invoke a handler. The agent surface requires a
    // token belonging to an active agent; the operator surface requires the
    // human root bearer. Agent identity headers are explicitly refused there.
    // This deliberately covers tower-mcp's built-in `/mcp/health` too: there
    // was no public liveness contract to preserve on the loopback listener.
    let agent_http = HttpTransport::new(agent_router)
        .bridge_extension::<ciacola_core::AgentIdentity>()
        .into_router_at("/mcp")
        .layer(axum::middleware::from_fn_with_state(
            ledger.clone(),
            operator_auth::require_agent,
        ));
    let operator_http = HttpTransport::new(operator_router)
        .into_router_at("/mcp-operator")
        .layer(axum::middleware::from_fn_with_state(
            operator_auth::OperatorHttpAuth::new(operator_token),
            operator_auth::require_operator,
        ));
    let http = agent_http
        .merge(operator_http)
        // The board is optional by construction: nothing in core knows
        // about it, so leaving it out is leaving out a merge.
        .merge(ciacola_board::router_with_limits(
            ledger.clone(),
            host,
            declared.limits,
            shutdown.clone(),
        ));
    let listener = bind_loopback_before_dispatch(port, &dispatch).await?;
    let http_shutdown = shutdown.clone();
    let http_handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, http)
            .with_graceful_shutdown(async move { http_shutdown.cancelled().await })
            .await
        {
            eprintln!("[ciacola] http: {e}");
        }
    });
    eprintln!(
        "[ciacola] board at http://127.0.0.1:{port}/board, agents' tools at /mcp, \
         authenticated human operators at /mcp-operator"
    );

    // The bound listener can now accept and queue loopback connections, but
    // dispatch remains closed while recovery adjudicates pre-crash `running`
    // rows and resubmits `queued` rows. Opening first would let the polling
    // executor claim a queued turn while recovery is scanning `running`, and
    // recovery could then mistake this process's live work for an orphan.
    if std::env::var("CIACOLA_NO_RECOVER").is_err() {
        let report = recover::recover(&ledger, exec.as_ref()).await?;
        eprintln!(
            "[ciacola] recovery: {} resubmitted, {} orphaned, {} orphan processes killed, {} unverified",
            report.resubmitted, report.orphaned, report.orphans_killed, report.orphans_unverified
        );
    }
    dispatch.open();
    eprintln!("[ciacola] dispatch: ready ({})", exec.name());

    // Ctrl-C drains rather than kills. Without this, a signal ends a
    // twenty minute agent run mid-flight: recovery tidies up on the
    // next boot, but it tidies up by killing the provider and marking
    // the turn failed, so the work and the money are both gone.
    let mut transport = GenericStdioTransport::with_notifications(stdio_router, rx);
    tokio::select! {
        result = transport.run() => { result?; }
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\n[ciacola] draining, in-flight turns finish (Ctrl-C again to abandon them)");
            // Same signal, so the HTTP side and the executor wind down
            // together: this stops the axum server from accepting new
            // connections and lets `/board/events` end its stream, so
            // graceful shutdown does not itself hang on a long-lived
            // response.
            shutdown.cancel();
            tokio::select! {
                left = async {
                    let (left, _) = tokio::join!(
                        drain_exec.drain(Duration::from_secs(600)),
                        http_handle,
                    );
                    left
                } => {
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

/// Bind the complete loopback surface while paid work is still unable to
/// claim. Keeping the assertion beside the bind makes a future startup
/// reorder fail closed instead of silently restoring the publication gap.
async fn bind_loopback_before_dispatch(
    port: u16,
    dispatch: &DispatchReadiness,
) -> std::io::Result<tokio::net::TcpListener> {
    if dispatch.is_open() {
        return Err(std::io::Error::other(
            "dispatch readiness opened before the loopback listener was bound",
        ));
    }
    tokio::net::TcpListener::bind(("127.0.0.1", port)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_database_path_has_highest_precedence() {
        let database = resolve_database_path(
            Some("/explicit/ciacola.db".into()),
            Some("/xdg".into()),
            Some("/home/example".into()),
            Path::new("/tmp"),
        );

        assert_eq!(database.path, Path::new("/explicit/ciacola.db"));
        assert!(!database.temporary_fallback);
    }

    #[test]
    fn xdg_data_home_precedes_home() {
        let database = resolve_database_path(
            None,
            Some("/xdg".into()),
            Some("/home/example".into()),
            Path::new("/tmp"),
        );

        assert_eq!(database.path, Path::new("/xdg/ciacola/ciacola.db"));
        assert!(!database.temporary_fallback);
    }

    #[test]
    fn home_provides_a_durable_default() {
        let database =
            resolve_database_path(None, None, Some("/home/example".into()), Path::new("/tmp"));

        assert_eq!(
            database.path,
            Path::new("/home/example/.local/share/ciacola/ciacola.db")
        );
        assert!(!database.temporary_fallback);
    }

    #[test]
    fn missing_user_directories_use_a_marked_temporary_fallback() {
        let database = resolve_database_path(None, None, None, Path::new("/tmp"));

        assert_eq!(database.path, Path::new("/tmp/ciacola.db"));
        assert!(database.temporary_fallback);
    }

    #[tokio::test]
    async fn an_occupied_loopback_port_leaves_dispatch_closed() {
        let occupied = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("occupy a loopback port");
        let port = occupied.local_addr().expect("occupied address").port();
        let dispatch = DispatchReadiness::closed();

        let error = bind_loopback_before_dispatch(port, &dispatch)
            .await
            .expect_err("a second listener must not bind the occupied port");

        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        assert!(!dispatch.is_open());
    }

    #[test]
    fn empty_environment_values_are_ignored() {
        let database = resolve_database_path(
            Some(OsString::new()),
            Some(OsString::new()),
            Some("/home/example".into()),
            Path::new("/tmp"),
        );

        assert_eq!(
            database.path,
            Path::new("/home/example/.local/share/ciacola/ciacola.db")
        );
    }

    fn protection_capability(
        meter: &str,
        cache_treatment: ciacola_agent::CacheTreatment,
    ) -> ciacola_agent::CeilingCapability {
        ciacola_agent::CeilingCapability {
            meter: ciacola_agent::MeterId::new(meter),
            granularity: ciacola_agent::EnforcementGranularity::ProviderResponseBoundary,
            cache_treatment,
        }
    }

    #[test]
    fn startup_summary_distinguishes_unbounded_unsupported_and_enforced() {
        let codex = protection_capability(
            ciacola_agent_codex::WEIGHTED_ROLLOUT_METER,
            ciacola_agent::CacheTreatment::Excluded,
        );
        let unbounded = provider_protection_summary(None, Some(&codex));
        assert!(unbounded.contains("UNBOUNDED"), "{unbounded}");
        assert!(
            unbounded.contains(ciacola_agent_codex::WEIGHTED_ROLLOUT_METER),
            "{unbounded}"
        );
        assert!(unbounded.contains("cached input excluded"), "{unbounded}");
        assert!(
            unbounded.contains("in-flight work can overshoot"),
            "{unbounded}"
        );

        let unsupported = provider_protection_summary(Some(250_000), None);
        assert!(unsupported.contains("UNSUPPORTED"), "{unsupported}");
        assert!(
            unsupported.contains("automatic work will be refused"),
            "{unsupported}"
        );

        let enforced = provider_protection_summary(Some(250_000), Some(&codex));
        assert!(enforced.contains("ENFORCED"), "{enforced}");
        assert!(enforced.contains("250000"), "{enforced}");
    }

    #[test]
    fn startup_summary_renders_claude_ceiling_as_micro_usd() {
        let claude = protection_capability(
            ciacola_agent_claude::MAX_BUDGET_MICRO_USD_METER,
            ciacola_agent::CacheTreatment::NotApplicable,
        );
        let summary = provider_protection_summary(Some(2_000_000), Some(&claude));
        assert!(summary.contains("$2.0000"), "{summary}");
        assert!(summary.contains("2000000 micro-USD"), "{summary}");
        assert!(summary.contains("cache not applicable"), "{summary}");
    }

    #[test]
    fn legacy_operator_config_is_downgraded_to_the_agent_mount() {
        let directory = std::env::temp_dir().join(format!(
            "ciacola-loopback-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir(&directory).expect("test directory");

        #[cfg(unix)]
        let victim = {
            use std::os::unix::fs::symlink;

            let victim = directory.join("must-not-be-overwritten");
            std::fs::write(&victim, "operator data").expect("victim");
            symlink(&victim, directory.join("ciacola-mcp.json")).expect("agent symlink");
            symlink(&victim, directory.join("ciacola-mcp-operator.json")).expect("legacy symlink");
            victim
        };

        let agent = write_loopback_configs(&directory, 9345).expect("write configs");
        let legacy = directory.join("ciacola-mcp-operator.json");
        let agent_text = std::fs::read_to_string(&agent).expect("agent config");
        let legacy_text = std::fs::read_to_string(&legacy).expect("legacy config");

        assert_eq!(agent_text, legacy_text);
        assert!(legacy_text.contains("http://127.0.0.1:9345/mcp"));
        assert!(!legacy_text.contains("/mcp-operator"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                std::fs::read_to_string(&victim).expect("victim survives"),
                "operator data"
            );
            assert!(
                !std::fs::symlink_metadata(&agent)
                    .expect("agent metadata")
                    .file_type()
                    .is_symlink()
            );
            assert!(
                !std::fs::symlink_metadata(&legacy)
                    .expect("legacy metadata")
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                std::fs::metadata(&agent)
                    .expect("agent permissions")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            std::fs::remove_file(victim).expect("remove victim");
        }

        std::fs::remove_file(agent).expect("remove agent config");
        std::fs::remove_file(legacy).expect("remove legacy config");
        std::fs::remove_dir(directory).expect("remove test directory");
    }
}
