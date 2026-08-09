//! Claude adapter: [`ciacola_agent::Provider`] implemented over the
//! exact `claude-wrapper` revision this workspace pins.
//!
//! This crate reproduces the live behavior in
//! `ciacola-core::agent::run_exchange`, `query_for_session`, and
//! `capped` -- session resume, hermetic scoping, credential isolation,
//! MCP endpoint materialization, and "a capped run is data, not an
//! error" -- against the provider-neutral contract in `ciacola-agent`,
//! without `ciacola-core` ever depending on `claude-wrapper` directly.
//! The translation itself lives in the crate-private `command` module
//! (intent to `QueryCommand`) and `outcome` module (wrapper
//! result/error to [`TurnOutcome`]/[`AgentError`]); both are
//! unit-tested without spawning a process.
//!
//! # What Claude cannot honour
//!
//! **`sandbox: false`, always.** The `claude` CLI's permission prompts
//! are a confirmation gate a human or a hook answers, not an OS-level
//! boundary the way a container or a sandboxed exec profile is.
//! Claiming otherwise would let a turn that asked to be sandboxed run
//! wide open under the name of a security feature this adapter does
//! not have; see `ciacola_agent::capability` for why that distinction
//! is drawn at the authority boundary. Per-turn provider spend is a separate,
//! non-security boundary that this adapter enforces with
//! `--max-budget-usd`.
//!
//! # Credential-isolation boundary
//!
//! A [`ClaudeProvider`] owns one startup snapshot of the environment
//! deliberately granted to provider children. Every opening and resume clears
//! the inherited daemon environment, restores that snapshot after removing
//! Claude's authentication, routing, cloud, and config selectors, and finally
//! applies the intended config home and optional in-memory OAuth credential.
//! This is what `credential_isolation = true` means here: deterministic
//! credential selection and direct-child environment minimization. It is not
//! protection from another process running under the same OS user, from files
//! that user can read, or from a deliberately allowlisted value. The pinned
//! Claude CLI has no separate shell-tool environment exclusion contract;
//! subprocesses naturally begin with the already-minimized child environment,
//! but that inheritance is defense in depth rather than a stronger isolation
//! claim.
//!
//! # Cancellation and drop safety
//!
//! [`ClaudeProvider::run`] consumes the wrapper's live JSONL stream.
//! Complete assistant messages carry per-provider-turn usage, which is
//! deduplicated, accumulated, and handed synchronously to
//! [`TurnEvents::usage_snapshot`] before the stream callback returns.
//! The stream also reveals `session_id` earlier than the old buffered
//! path did, but [`TurnEvents::resume_id`] is intentionally async while
//! the wrapper callback is synchronous. This adapter therefore persists
//! that id as soon as the stream returns, as before. Ciacola's Claude
//! agents are client-assigned an id before launch, so an operator kill
//! does not lose their resumability; a legacy host that starts Claude
//! without an assigned id retains the contract's documented limitation
//! on force-dropped synchronous streams.
//! Every wrapper spawn sets `kill_on_drop(true)` and places the child in
//! its own process group, whose `Drop` guard SIGKILLs the whole group.
//! Dropping (or aborting) the future this method returns therefore
//! kills the `claude` process and everything it spawned for tool use,
//! with no cooperation required from this crate. That is also today's
//! live behavior: `ciacola-core::agent::run_exchange` has never carried
//! its own cancellation path either, and relies on the same wrapper
//! guarantee transitively. Nothing here constructs
//! [`AgentError::Cancelled`], because nothing reaches this adapter to
//! say a turn was cancelled rather than simply dropped; that variant is
//! for a backend with its own cooperative cancel signal, which
//! `claude -p` does not offer.

#![warn(missing_docs)]

use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use ciacola_agent::{
    AgentError, BoxFut, CacheTreatment, Capabilities, CeilingCapability, EnforcementGranularity,
    MeterId, PartialTelemetry, Provider, ProviderChildEnvironment, ProviderKey, ResumeId,
    TokenUsage, TurnEvents, TurnIntent, TurnOutcome,
};
use claude_wrapper::{
    Claude, ClaudeBuilder, OutputFormat, QueryResult,
    streaming::{StreamEvent, stream_query},
};

mod command;
mod outcome;

/// Claude's native `--max-budget-usd` meter, represented without floating
/// point in Ciacola configuration and persistence.
pub const MAX_BUDGET_MICRO_USD_METER: &str = "claude.max_budget.micro_usd.v1";

/// The stable ceiling capability of the pinned Claude CLI contract.
pub fn turn_ceiling_capability() -> CeilingCapability {
    CeilingCapability {
        meter: MeterId::new(MAX_BUDGET_MICRO_USD_METER),
        granularity: EnforcementGranularity::ProviderResponseBoundary,
        cache_treatment: CacheTreatment::NotApplicable,
    }
}

/// The Claude backend, over `claude-wrapper`.
///
/// Environment values and the optional credential are runtime-only: neither
/// is serialized into a turn intent or ledger row. Debug output exposes only
/// the granted environment names and whether a credential is present.
#[derive(Clone, Default)]
pub struct ClaudeProvider {
    child_environment: ProviderChildEnvironment,
    credential: Option<Arc<str>>,
}

impl ClaudeProvider {
    /// Construct a provider from the daemon's startup environment snapshot and
    /// an optional credential consumed from `CIACOLA_CLAUDE_TOKEN_FD`.
    ///
    /// `credential` is moved into redacted process memory and is applied only
    /// as Claude's canonical `CLAUDE_CODE_OAUTH_TOKEN` child variable. Use
    /// `None` with a separately authenticated provider home.
    #[must_use]
    pub fn new(child_environment: ProviderChildEnvironment, credential: Option<String>) -> Self {
        Self {
            child_environment,
            credential: credential.map(Arc::from),
        }
    }

    fn legacy_token_error(intent: &TurnIntent) -> Option<AgentError> {
        intent.token_env.as_ref().map(|_| AgentError::Launch {
            provider: ProviderKey::claude(),
            detail: "legacy AgentDef.token_env blocks launch; reapply a config-managed definition or retire and recreate this agent, then use CIACOLA_CLAUDE_TOKEN_FD at daemon startup or a separately authenticated claude_home"
                .into(),
        })
    }

    fn client_for_intent(
        &self,
        intent: &TurnIntent,
        mut builder: ClaudeBuilder,
    ) -> Result<Claude, AgentError> {
        if let Some(error) = Self::legacy_token_error(intent) {
            return Err(error);
        }

        // Claude owns these namespaces for authentication, routing, cloud
        // selection, and config. An operator may name them in the neutral
        // passthrough policy, but they can never override this adapter's
        // credential choice. Broad cloud prefixes are intentional: a newly
        // introduced AWS/Vertex selector must fail closed without waiting for
        // Ciacola to learn its exact spelling.
        let environment = self.child_environment.excluding(
            &["CLAUDECODE", "CLOUD_ML_REGION"],
            &[
                "CLAUDE_",
                "ANTHROPIC_",
                "AWS_",
                "GOOGLE_",
                "GCLOUD_",
                "CLOUDSDK_",
                "VERTEX_",
            ],
        );
        builder = builder.clear_env().envs(environment.iter());

        if let Some(dir) = &intent.working_dir {
            builder = builder.working_dir(dir);
        }
        if let Some(home) = &intent.config_home {
            // The CLI reads its config and writes its sessions here. Apply it
            // after the neutral snapshot so a passthrough entry cannot win.
            std::fs::create_dir_all(home).map_err(|e| AgentError::Io {
                detail: format!("failed to create claude config home '{home}': {e}"),
            })?;
            builder = builder.env("CLAUDE_CONFIG_DIR", home);
        }
        if let Some(credential) = &self.credential {
            // Canonical intended auth is last for the same reason.
            builder = builder.env("CLAUDE_CODE_OAUTH_TOKEN", credential.as_ref());
        }

        builder.build().map_err(|e| AgentError::NotFound {
            provider: ProviderKey::claude(),
            detail: e.to_string(),
        })
    }

    async fn run_turn_with_builder(
        &self,
        intent: &TurnIntent,
        events: &dyn TurnEvents,
        builder: ClaudeBuilder,
    ) -> Result<TurnOutcome, AgentError> {
        let claude = self.client_for_intent(intent, builder)?;
        run_turn_with_client(intent, events, &claude).await
    }
}

impl fmt::Debug for ClaudeProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaudeProvider")
            .field("child_environment", &self.child_environment)
            .field("has_credential", &self.credential.is_some())
            .finish()
    }
}

impl Provider for ClaudeProvider {
    fn key(&self) -> ProviderKey {
        ProviderKey::claude()
    }

    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::none(self.key());
        caps.client_assigned_resume = true;
        caps.isolation = true;
        caps.credential_isolation = true;
        // The CLI's permission prompts are a confirmation gate, not an
        // OS-level boundary. Left at `Capabilities::none`'s default of
        // `false`, spelled out here so the choice reads as deliberate.
        caps.sandbox = false;
        caps.scoped_mcp = true;
        caps.strict_mcp = true;
        caps.allowed_tools = true;
        caps.max_provider_turns = true;
        caps.turn_ceiling = Some(turn_ceiling_capability());
        caps.effort = true;
        caps.reports_cost = true;
        caps.reports_token_usage = true;
        caps.reports_provider_turns = true;
        caps
    }

    fn run<'a>(
        &'a self,
        intent: &'a TurnIntent,
        events: &'a dyn TurnEvents,
    ) -> BoxFut<'a, Result<TurnOutcome, AgentError>> {
        Box::pin(async move {
            if let Some(error) = Self::legacy_token_error(intent) {
                return Err(error);
            }
            if let Some(blocking) = self.capabilities().validate(intent).blocking() {
                return Err(AgentError::Unsupported {
                    provider: self.key(),
                    constraint: blocking.constraint,
                    detail: blocking.detail.clone(),
                });
            }
            self.run_turn_with_builder(intent, events, Claude::builder())
                .await
        })
    }

    fn owns_process(&self, ps_line: &str) -> bool {
        // Match on the basename of any whitespace-separated token
        // rather than a bare substring search: a literal "claude"
        // check would also match `claude-code`, a project directory
        // named `.../claude/...`, or an unrelated shell history entry
        // that happens to mention the word. This matches only a token
        // that names the `claude` binary itself, wherever it lives on
        // PATH.
        ps_line
            .split_whitespace()
            .any(|token| Path::new(token).file_name().and_then(|f| f.to_str()) == Some("claude"))
    }
}

#[derive(Default)]
struct StreamObservations {
    terminal: Option<QueryResult>,
    terminal_parse_error: Option<String>,
    cumulative_usage: Option<TokenUsage>,
    seen_message_ids: HashSet<String>,
    resume: Option<ResumeId>,
}

impl StreamObservations {
    /// Record one wrapper event and return a new cumulative usage value
    /// when the event reported one.
    fn observe(&mut self, event: &StreamEvent) -> Option<TokenUsage> {
        if let Some(session_id) = event.session_id() {
            self.resume = Some(ResumeId::ProviderAssigned(session_id.to_string()));
        }

        if let Some((message_id, usage)) = outcome::assistant_usage(event) {
            let is_new = message_id
                .map(|id| self.seen_message_ids.insert(id))
                .unwrap_or(true);
            if is_new {
                let cumulative =
                    outcome::add_usage(self.cumulative_usage.unwrap_or_default(), usage);
                self.cumulative_usage = Some(cumulative);
                return Some(cumulative);
            }
        }

        if event.is_result() {
            match serde_json::from_value::<QueryResult>(event.data.clone()) {
                Ok(result) => {
                    let reported = outcome::reported_usage(&result);
                    self.terminal = Some(result);
                    if let Some(usage) = reported {
                        // Terminal usage is authoritative, not another
                        // delta to add to assistant-message totals.
                        self.cumulative_usage = Some(usage);
                        return Some(usage);
                    }
                }
                Err(error) => {
                    self.terminal_parse_error = Some(error.to_string());
                }
            }
        }
        None
    }
}

async fn run_turn_with_client(
    intent: &TurnIntent,
    events: &dyn TurnEvents,
    claude: &Claude,
) -> Result<TurnOutcome, AgentError> {
    let provider = ProviderKey::claude();

    // The temp MCP config file must outlive `stream_query`: the CLI
    // reads it by path, and dropping it early would delete the file
    // out from under the child.
    let mut mcp_guard = None;
    let query = command::build(intent, &mut mcp_guard)?.output_format(OutputFormat::StreamJson);
    let mut observations = StreamObservations::default();

    let started = Instant::now();
    let result = stream_query(claude, &query, |event| {
        let snapshot = observations.observe(&event);
        if let Some(usage) = snapshot {
            // Synchronous handoff is the cancellation boundary: core
            // accepts this into a writer that outlives a dropped
            // provider future.
            events.usage_snapshot(usage);
        }
    })
    .await;
    let elapsed = started.elapsed();
    drop(mcp_guard);

    let terminal = observations.terminal.take();
    let terminal_parse_error = observations.terminal_parse_error.take();
    let cumulative_usage = observations.cumulative_usage;
    let observed_resume = observations.resume.take();
    let fallback_resume = observed_resume.clone().or_else(|| intent.resume.clone());

    if let Some(query_result) = terminal {
        let outcome = outcome::from_stream_result(
            query_result,
            cumulative_usage,
            elapsed,
            fallback_resume.as_ref(),
        );
        if let Some(resume) = &outcome.resume {
            events.resume_id(resume).await;
        }
        return Ok(outcome);
    }

    if let Some(resume) = &observed_resume {
        events.resume_id(resume).await;
    }
    let partial = PartialTelemetry {
        resume: observed_resume,
        cost: None,
        usage: cumulative_usage,
        elapsed: Some(elapsed),
    };
    if let Some(detail) = terminal_parse_error {
        return Err(AgentError::Protocol {
            provider,
            detail: format!("failed to parse claude result event: {detail}"),
            partial: partial.into(),
        });
    }
    match result {
        Ok(_) => Err(AgentError::Protocol {
            provider,
            detail: "claude stream ended without a result event".to_string(),
            partial: partial.into(),
        }),
        Err(error) => Err(outcome::classify_failure_with_partial(
            error, elapsed, provider, partial,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciacola_agent::{Cost, FailureKind, Isolation, ProviderRegistry, Sandbox, Usage};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const ENV_WORKER_ROOT: &str = "CIACOLA_CLAUDE_ENV_WORKER_ROOT";

    fn fixture_builder(name: &str, args: impl IntoIterator<Item = String>) -> ClaudeBuilder {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join(name);
        let mut builder = Claude::builder()
            .binary("/bin/bash")
            .arg(script.display().to_string())
            .timeout(Duration::from_secs(3));
        for arg in args {
            builder = builder.arg(arg);
        }
        builder
    }

    fn fixture(name: &str, args: impl IntoIterator<Item = String>) -> Claude {
        fixture_builder(name, args)
            .build()
            .expect("fake claude client")
    }

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ciacola-claude-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }

    fn captured_environment(path: &Path) -> BTreeMap<String, String> {
        std::fs::read_to_string(path)
            .expect("captured child environment")
            .lines()
            .map(|line| {
                let (name, value) = line
                    .split_once('=')
                    .expect("env output always contains an equals sign");
                (name.to_string(), value.to_string())
            })
            .collect()
    }

    fn environment_keys(environment: &BTreeMap<String, String>) -> Vec<&str> {
        environment.keys().map(String::as_str).collect()
    }

    #[derive(Default)]
    struct RecordingEvents {
        ids: Mutex<Vec<String>>,
        usage: Mutex<Vec<TokenUsage>>,
        marker: Option<PathBuf>,
    }

    impl TurnEvents for RecordingEvents {
        fn resume_id<'a>(&'a self, id: &'a ResumeId) -> BoxFut<'a, ()> {
            Box::pin(async move {
                self.ids
                    .lock()
                    .expect("resume recorder lock")
                    .push(id.value().to_string());
            })
        }

        fn usage_snapshot(&self, usage: TokenUsage) {
            self.usage.lock().expect("usage recorder lock").push(usage);
            if let Some(marker) = &self.marker {
                std::fs::write(marker, b"accepted").expect("write usage marker");
            }
        }
    }

    fn intent(prompt: &str) -> TurnIntent {
        let mut intent = TurnIntent::new(prompt);
        intent.allowed_tools = None;
        intent
    }

    #[test]
    fn the_provider_registers_under_the_claude_key() {
        let registry = ProviderRegistry::new()
            .with(Arc::new(ClaudeProvider::default()))
            .expect("unique key");
        assert!(registry.get(&ProviderKey::claude()).is_ok());
    }

    #[test]
    fn provider_debug_redacts_the_in_memory_credential() {
        let provider = ClaudeProvider::new(
            ProviderChildEnvironment::default(),
            Some("never-render-this-claude-token".into()),
        );

        let rendered = format!("{provider:?}");
        assert!(rendered.contains("has_credential: true"), "{rendered}");
        assert!(!rendered.contains("never-render-this-claude-token"));
    }

    #[tokio::test]
    async fn a_legacy_token_source_is_refused_before_the_fake_child_launches() {
        let capture = temp_path("legacy-token-refusal");
        std::fs::create_dir_all(&capture).expect("capture directory");
        let mut turn = intent("must not launch");
        turn.token_env = Some("CIACOLA_OLD_CLAUDE_TOKEN".into());

        let error = ClaudeProvider::default()
            .run_turn_with_builder(
                &turn,
                &RecordingEvents::default(),
                fixture_builder(
                    "fake-claude-capture-env.sh",
                    [capture.display().to_string(), "legacy".into()],
                ),
            )
            .await
            .expect_err("legacy startup environment credentials must fail closed");

        match error {
            AgentError::Launch { detail, .. } => {
                assert!(detail.contains("AgentDef.token_env"), "{detail}");
                assert!(detail.contains("CIACOLA_CLAUDE_TOKEN_FD"), "{detail}");
                assert!(detail.contains("claude_home"), "{detail}");
                assert!(!detail.contains("CIACOLA_OLD_CLAUDE_TOKEN"), "{detail}");
            }
            other => panic!("expected pre-launch migration error, got {other:?}"),
        }
        assert!(
            !capture.join("legacy.marker").exists(),
            "the fake child must never have run"
        );
        std::fs::remove_dir_all(capture).expect("remove capture directory");
    }

    /// Runs the environment assertions in a nested test process so ambient
    /// variables can be supplied through `Command::env` without mutating the
    /// multithreaded test runner's process-global environment.
    #[test]
    fn opening_and_resume_environment_contract_runs_in_an_isolated_process() {
        let root = temp_path("child-environment");
        std::fs::create_dir_all(&root).expect("worker directory");
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "tests::provider_child_environment_worker",
                "--nocapture",
            ])
            .env_clear()
            .env(ENV_WORKER_ROOT, &root)
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.join("baseline-home"))
            .env("LANG", "C")
            .env("ANTHROPIC_API_KEY", "ambient-anthropic-key")
            .env("ANTHROPIC_AUTH_TOKEN", "allowlisted-own-auth")
            .env("CLAUDE_CODE_OAUTH_TOKEN", "ambient-wrong-oauth")
            .env("CLAUDE_CODE_USE_BEDROCK", "1")
            .env("CLAUDE_CODE_USE_VERTEX", "1")
            .env("CLAUDE_CONFIG_DIR", root.join("ambient-claude-home"))
            .env("CLAUDECODE", "ambient-nested-session")
            .env("AWS_ACCESS_KEY_ID", "ambient-aws-key")
            .env(
                "GOOGLE_APPLICATION_CREDENTIALS",
                root.join("ambient-google-credentials"),
            )
            .env("CLOUD_ML_REGION", "ambient-cloud-region")
            .env("CIACOLA_CLAUDE_SOURCE_TOKEN", "ambient-source-token")
            .env("CODEX_API_KEY", "ambient-opposite-provider-token")
            .env("MCP_BEARER", "ambient-client-bearer")
            .env("CIACOLA_TEST_SENTINEL", "ambient-ciacola-secret")
            .env("UNRELATED_SECRET_SENTINEL", "ambient-unrelated-secret")
            .env("EXPLICIT_WORKFLOW", "explicit-workflow-value")
            .output()
            .expect("spawn isolated environment worker");

        if !output.status.success() {
            panic!(
                "environment worker failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert!(
            root.join("worker-complete").exists(),
            "exact worker test did not run; stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        std::fs::remove_dir_all(root).expect("remove worker directory");
    }

    #[tokio::test]
    async fn provider_child_environment_worker() {
        let Some(root) = std::env::var_os(ENV_WORKER_ROOT).map(PathBuf::from) else {
            // The outer test launches this exact test again with controlled
            // ambient values. Its ordinary test-suite invocation is a no-op.
            return;
        };

        let open_home = root.join("open-home");
        let open_snapshot = ProviderChildEnvironment::capture(&[]).expect("opening snapshot");
        let open_provider =
            ClaudeProvider::new(open_snapshot, Some("intended-open-oauth-token".into()));
        let mut open = intent("open safely");
        open.config_home = Some(open_home.display().to_string());
        open.resume = Some(ResumeId::ClientAssigned("client-open".into()));
        open_provider
            .run_turn_with_builder(
                &open,
                &RecordingEvents::default(),
                fixture_builder(
                    "fake-claude-capture-env.sh",
                    [root.display().to_string(), "open".into()],
                ),
            )
            .await
            .expect("opening turn");

        let open_env = captured_environment(&root.join("open.env"));
        assert!(
            open_env.contains_key("PATH"),
            "exported keys: {:?}",
            environment_keys(&open_env)
        );
        assert_eq!(
            open_env.get("HOME").map(String::as_str),
            Some(root.join("baseline-home").to_string_lossy().as_ref())
        );
        assert_eq!(open_env.get("LANG").map(String::as_str), Some("C"));
        assert_eq!(
            open_env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some(open_home.to_string_lossy().as_ref())
        );
        assert_eq!(
            open_env.get("CLAUDE_CODE_OAUTH_TOKEN").map(String::as_str),
            Some("intended-open-oauth-token")
        );
        for absent in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDECODE",
            "AWS_ACCESS_KEY_ID",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "CLOUD_ML_REGION",
            "CIACOLA_CLAUDE_SOURCE_TOKEN",
            "CODEX_API_KEY",
            "MCP_BEARER",
            "CIACOLA_TEST_SENTINEL",
            "UNRELATED_SECRET_SENTINEL",
            "EXPLICIT_WORKFLOW",
            ENV_WORKER_ROOT,
        ] {
            assert!(
                !open_env.contains_key(absent),
                "unexpected child key {absent}; exported keys: {:?}",
                environment_keys(&open_env)
            );
        }
        assert!(root.join("open.marker").exists());

        let allowed = [
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CONFIG_DIR",
            "AWS_ACCESS_KEY_ID",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "CLOUD_ML_REGION",
            "CIACOLA_CLAUDE_SOURCE_TOKEN",
            "CODEX_API_KEY",
            "MCP_BEARER",
            "CIACOLA_TEST_SENTINEL",
            "UNRELATED_SECRET_SENTINEL",
            "EXPLICIT_WORKFLOW",
        ]
        .map(str::to_string);
        let resume_snapshot = ProviderChildEnvironment::capture(&allowed).expect("resume snapshot");
        let debug_snapshot = format!("{resume_snapshot:?}");
        assert!(debug_snapshot.contains("CODEX_API_KEY"));
        assert!(!debug_snapshot.contains("ambient-opposite-provider-token"));

        let resume_home = root.join("resume-home");
        let resume_provider =
            ClaudeProvider::new(resume_snapshot, Some("intended-resume-oauth-token".into()));
        let mut resume = intent("continue safely");
        resume.config_home = Some(resume_home.display().to_string());
        resume.resume = Some(ResumeId::ProviderAssigned("sess-existing".into()));
        resume_provider
            .run_turn_with_builder(
                &resume,
                &RecordingEvents::default(),
                fixture_builder(
                    "fake-claude-capture-env.sh",
                    [root.display().to_string(), "resume".into()],
                ),
            )
            .await
            .expect("resume turn");

        let resume_env = captured_environment(&root.join("resume.env"));
        assert_eq!(
            resume_env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some(resume_home.to_string_lossy().as_ref())
        );
        assert_eq!(
            resume_env
                .get("CLAUDE_CODE_OAUTH_TOKEN")
                .map(String::as_str),
            Some("intended-resume-oauth-token")
        );
        for stripped in [
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
            "AWS_ACCESS_KEY_ID",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "CLOUD_ML_REGION",
        ] {
            assert!(
                !resume_env.contains_key(stripped),
                "Claude-owned selector {stripped} survived; exported keys: {:?}",
                environment_keys(&resume_env)
            );
        }
        for (name, value) in [
            ("CIACOLA_CLAUDE_SOURCE_TOKEN", "ambient-source-token"),
            ("CODEX_API_KEY", "ambient-opposite-provider-token"),
            ("MCP_BEARER", "ambient-client-bearer"),
            ("CIACOLA_TEST_SENTINEL", "ambient-ciacola-secret"),
            ("UNRELATED_SECRET_SENTINEL", "ambient-unrelated-secret"),
            ("EXPLICIT_WORKFLOW", "explicit-workflow-value"),
        ] {
            assert_eq!(resume_env.get(name).map(String::as_str), Some(value));
        }
        let resume_args =
            std::fs::read_to_string(root.join("resume.args")).expect("captured resume arguments");
        assert!(
            resume_args
                .lines()
                .collect::<Vec<_>>()
                .windows(2)
                .any(|pair| pair == ["--resume", "sess-existing"])
        );
        assert!(root.join("resume.marker").exists());
        std::fs::write(root.join("worker-complete"), b"ok").expect("worker completion marker");
    }

    /// The capability this whole adapter exists to declare honestly:
    /// the CLI's permission prompts are not an OS-level sandbox.
    #[test]
    fn sandbox_is_always_declared_unsupported() {
        assert!(!ClaudeProvider::default().capabilities().sandbox);
    }

    #[test]
    fn native_budget_capability_is_micro_usd_at_a_response_boundary() {
        let capability = ClaudeProvider::default()
            .capabilities()
            .turn_ceiling
            .expect("Claude enforces max-budget-usd");
        assert_eq!(capability.meter.as_str(), MAX_BUDGET_MICRO_USD_METER);
        assert_eq!(
            capability.granularity,
            EnforcementGranularity::ProviderResponseBoundary
        );
        assert_eq!(capability.cache_treatment, CacheTreatment::NotApplicable);
    }

    /// The consequence of that declaration: a turn that asks to be
    /// sandboxed must be refused before it runs, not silently widened.
    #[test]
    fn a_sandboxed_turn_is_blocked_by_capability_validation() {
        let mut intent = TurnIntent::new("go");
        intent.sandbox = Sandbox::WorkspaceWriteNoNetwork;
        let validation = ClaudeProvider::default().capabilities().validate(&intent);
        let blocking = validation.blocking().expect("sandbox must block");
        assert_eq!(blocking.constraint, ciacola_agent::Constraint::Sandbox);
    }

    /// Isolation, credential isolation, scoped/strict MCP, allowed tools,
    /// client-assigned resume, internal-turn hints, and spend ceilings are all things
    /// this adapter actually implements, so an intent that asks for
    /// them must not be blocked.
    #[test]
    fn every_other_supported_constraint_this_adapter_implements_passes_validation() {
        let mut intent = TurnIntent::new("go");
        intent.isolation = Isolation::Full;
        intent.config_home = Some("/tmp/claude-home".into());
        intent.allowed_tools = Some(vec!["Read".into()]);
        intent.resume = Some(ciacola_agent::ResumeId::ClientAssigned("agent-1".into()));
        intent.max_provider_turns = Some(20);
        intent.turn_ceiling = Some(ciacola_agent::TurnCeiling {
            capability: turn_ceiling_capability(),
            limit: 25_000,
        });
        intent.mcp = Some(ciacola_agent::McpScope {
            endpoints: vec![ciacola_agent::McpEndpoint {
                name: "ciacola".into(),
                url: "http://127.0.0.1:4823/mcp".into(),
                headers: Default::default(),
            }],
            strict: true,
        });

        let validation = ClaudeProvider::default().capabilities().validate(&intent);
        assert!(
            validation.unsupported.is_empty(),
            "{:?}",
            validation.unsupported
        );
    }

    #[test]
    fn owns_process_matches_the_claude_binary_and_nothing_else() {
        let provider = ClaudeProvider::default();
        assert!(provider.owns_process("54322 claude -p do the thing"));
        assert!(provider.owns_process("54322 /usr/local/bin/claude --resume sess-1 -- go"));
        assert!(
            !provider.owns_process("54321 fake-backend --resume sess-1 do the thing"),
            "another backend's process is not this backend's to kill"
        );
        assert!(
            !provider.owns_process("54323 claude-code --resume sess-1"),
            "a similarly named but different binary must not match"
        );
        assert!(!provider.owns_process("54324 -zsh"));
        assert!(
            !provider.owns_process("54325 vim /home/josh/notes/claude/todo.md"),
            "a path that merely mentions claude must not match"
        );
    }

    #[tokio::test]
    async fn stream_success_keeps_terminal_parity_and_emits_cumulative_usage() {
        let claude = fixture("fake-claude-stream-success.sh", []);
        let events = RecordingEvents::default();

        let outcome = run_turn_with_client(&intent("do it"), &events, &claude)
            .await
            .expect("streamed turn");

        assert_eq!(outcome.reply, "streamed reply");
        assert_eq!(
            outcome.resume,
            Some(ResumeId::ProviderAssigned("sess-stream".into()))
        );
        assert_eq!(outcome.cost, Cost::Reported { micro_usd: 20_000 });
        assert_eq!(
            outcome.usage,
            Usage::Reported(TokenUsage {
                input: 29,
                output: 6,
                cached_input: 7,
            })
        );
        assert_eq!(outcome.provider_turns, Some(2));
        assert!(outcome.succeeded());
        assert_eq!(
            events.usage.lock().expect("usage recorder lock").as_slice(),
            [
                TokenUsage {
                    input: 17,
                    output: 2,
                    cached_input: 4,
                },
                TokenUsage {
                    input: 22,
                    output: 5,
                    cached_input: 4,
                },
                TokenUsage {
                    input: 29,
                    output: 6,
                    cached_input: 7,
                },
            ],
            "the duplicate msg-1 is ignored and terminal usage replaces rather than adds"
        );
        assert_eq!(
            events.ids.lock().expect("resume recorder lock").as_slice(),
            ["sess-stream"]
        );
    }

    #[tokio::test]
    async fn usage_is_handed_off_before_a_killed_stream_future_can_discard_it() {
        let marker = temp_path("usage-marker");
        let claude = fixture("fake-claude-usage-early.sh", [marker.display().to_string()]);
        let events = Arc::new(RecordingEvents {
            marker: Some(marker.clone()),
            ..Default::default()
        });
        let run_events = Arc::clone(&events);
        let run = tokio::spawn(async move {
            run_turn_with_client(&intent("work until killed"), run_events.as_ref(), &claude).await
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if marker.exists() && events.usage.lock().expect("usage recorder lock").len() == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("usage snapshots arrived before termination");

        assert!(!run.is_finished(), "fixture must still be running at kill");
        assert_eq!(
            events.usage.lock().expect("usage recorder lock").as_slice(),
            [
                TokenUsage {
                    input: 4,
                    output: 1,
                    cached_input: 0,
                },
                TokenUsage {
                    input: 9,
                    output: 3,
                    cached_input: 2,
                },
            ],
            "empty/null usage is ignored; complete messages without ids are distinct"
        );

        run.abort();
        let _ = run.await;
        let _ = std::fs::remove_file(marker);
    }

    #[tokio::test]
    async fn streamed_turn_cap_stays_a_limit_with_spend_usage_and_resume() {
        let claude = fixture("fake-claude-cap.sh", []);
        let events = RecordingEvents::default();
        let mut turn = intent("run to the ceiling");
        turn.resume = Some(ResumeId::ClientAssigned("assigned-cap".into()));

        let outcome = run_turn_with_client(&turn, &events, &claude)
            .await
            .expect("a provider cap is terminal data");

        assert_eq!(
            outcome.failure.as_ref().map(|f| f.kind),
            Some(FailureKind::Limit)
        );
        assert_eq!(
            outcome.failure_message(),
            Some("reached maximum number of turns (60)")
        );
        assert_eq!(
            outcome.cost,
            Cost::Reported {
                micro_usd: 1_250_000
            }
        );
        assert_eq!(
            outcome.usage,
            Usage::Reported(TokenUsage {
                input: 8,
                output: 2,
                cached_input: 0,
            })
        );
        assert_eq!(outcome.provider_turns, Some(60));
        assert_eq!(
            outcome.resume,
            Some(ResumeId::ProviderAssigned("assigned-cap".into())),
            "a terminal cap proves the client-assigned conversation opened"
        );
    }

    #[tokio::test]
    async fn streamed_auth_failure_is_classified_without_leaking_the_prompt() {
        let claude = fixture("fake-claude-auth.sh", []);
        let error = run_turn_with_client(
            &intent("MY-SECRET-PROMPT must not reach the error"),
            &RecordingEvents::default(),
            &claude,
        )
        .await
        .expect_err("auth failure");

        match error {
            AgentError::Other {
                detail, partial, ..
            } => {
                assert!(detail.contains("auth error"), "{detail}");
                assert!(detail.contains("Not authenticated"), "{detail}");
                assert!(!detail.contains("MY-SECRET-PROMPT"), "{detail}");
                assert!(partial.usage.is_none());
                assert!(partial.elapsed.is_some());
            }
            other => panic!("expected classified post-launch auth error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn streamed_resume_keeps_the_existing_cli_and_outcome_contract() {
        let capture = temp_path("resume-args");
        let claude = fixture(
            "fake-claude-capture-resume.sh",
            [capture.display().to_string()],
        );
        let events = RecordingEvents::default();
        let mut turn = intent("continue now");
        turn.resume = Some(ResumeId::ProviderAssigned("sess-existing".into()));

        let outcome = run_turn_with_client(&turn, &events, &claude)
            .await
            .expect("resumed stream");
        let args = std::fs::read_to_string(&capture).expect("captured args");
        let args: Vec<&str> = args.lines().collect();
        let _ = std::fs::remove_file(capture);

        assert!(
            args.windows(2)
                .any(|pair| pair == ["--resume", "sess-existing"]),
            "{args:?}"
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--output-format", "stream-json"]),
            "{args:?}"
        );
        assert_eq!(outcome.reply, "resumed reply");
        assert_eq!(
            outcome.resume,
            Some(ResumeId::ProviderAssigned("sess-existing".into()))
        );
        assert_eq!(
            events.ids.lock().expect("resume recorder lock").as_slice(),
            ["sess-existing"]
        );
    }
}
