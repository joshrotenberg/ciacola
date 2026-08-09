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
use std::path::Path;
use std::time::Instant;

use ciacola_agent::{
    AgentError, BoxFut, CacheTreatment, Capabilities, CeilingCapability, EnforcementGranularity,
    MeterId, PartialTelemetry, Provider, ProviderKey, ResumeId, TokenUsage, TurnEvents, TurnIntent,
    TurnOutcome,
};
use claude_wrapper::{
    Claude, OutputFormat, QueryResult,
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
#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeProvider;

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
            if let Some(blocking) = self.capabilities().validate(intent).blocking() {
                return Err(AgentError::Unsupported {
                    provider: self.key(),
                    constraint: blocking.constraint,
                    detail: blocking.detail.clone(),
                });
            }
            run_turn(intent, events).await
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

async fn run_turn(intent: &TurnIntent, events: &dyn TurnEvents) -> Result<TurnOutcome, AgentError> {
    let provider = ProviderKey::claude();

    let mut builder = Claude::builder();
    if let Some(dir) = &intent.working_dir {
        builder = builder.working_dir(dir);
    }
    if let Some(home) = &intent.config_home {
        // The CLI reads its config and writes its sessions here.
        std::fs::create_dir_all(home).map_err(|e| AgentError::Io {
            detail: format!("failed to create claude config home '{home}': {e}"),
        })?;
        builder = builder.env("CLAUDE_CONFIG_DIR", home);
    }
    if let Some(var) = &intent.token_env {
        match std::env::var(var) {
            Ok(token) if !token.is_empty() => {
                builder = builder.env("CLAUDE_CODE_OAUTH_TOKEN", token);
            }
            _ => {
                tracing::warn!(var, "token_env is set but the variable is empty or unset");
            }
        }
    }
    let claude = builder.build().map_err(|e| AgentError::NotFound {
        provider: provider.clone(),
        detail: e.to_string(),
    })?;

    run_turn_with_client(intent, events, &claude).await
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
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn fixture(name: &str, args: impl IntoIterator<Item = String>) -> Claude {
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
        builder.build().expect("fake claude client")
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
            .with(Arc::new(ClaudeProvider))
            .expect("unique key");
        assert!(registry.get(&ProviderKey::claude()).is_ok());
    }

    /// The capability this whole adapter exists to declare honestly:
    /// the CLI's permission prompts are not an OS-level sandbox.
    #[test]
    fn sandbox_is_always_declared_unsupported() {
        assert!(!ClaudeProvider.capabilities().sandbox);
    }

    #[test]
    fn native_budget_capability_is_micro_usd_at_a_response_boundary() {
        let capability = ClaudeProvider
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
        let validation = ClaudeProvider.capabilities().validate(&intent);
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
        intent.token_env = Some("CLAUDE_TOKEN".into());
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

        let validation = ClaudeProvider.capabilities().validate(&intent);
        assert!(
            validation.unsupported.is_empty(),
            "{:?}",
            validation.unsupported
        );
    }

    #[test]
    fn owns_process_matches_the_claude_binary_and_nothing_else() {
        let provider = ClaudeProvider;
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
