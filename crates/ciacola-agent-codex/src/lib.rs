//! Codex adapter over the exact `codex-wrapper` revision this workspace pins.
//!
//! Codex names its own threads, emits token usage without money, and controls
//! authority through sandbox and exec policy rather than Claude tool names.
//! This adapter keeps those differences visible instead of manufacturing
//! parity: thread ids are persisted from the live JSONL stream, monetary cost
//! is always [`ciacola_agent::Cost::NotPriced`], and a Claude-style tool grant
//! is refused before launch.

#![warn(missing_docs)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ciacola_agent::{
    AgentError, BoxFut, Capabilities, Provider, ProviderKey, ResumeId, TurnEvents, TurnIntent,
    TurnOutcome,
};
use codex_wrapper::{CliVersionStatus, Codex, JsonLineEvent};

mod command;
mod outcome;

/// The Codex backend.
#[derive(Debug, Clone, Default)]
pub struct CodexProvider {
    binary: Option<PathBuf>,
    binary_args: Vec<String>,
    timeout: Option<Duration>,
}

impl CodexProvider {
    /// Use the Codex binary discovered on `PATH`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Report whether the installed CLI is inside the wrapper's tested range.
    ///
    /// This spawns only `codex --version`; it makes no API request and spends
    /// no tokens. A newer CLI is reported by the wrapper rather than refused.
    pub async fn cli_version_status(&self) -> Result<CliVersionStatus, AgentError> {
        let codex = self.client(None, std::iter::empty())?;
        codex
            .cli_version_status()
            .await
            .map_err(|error| outcome::classify_failure(error, Duration::ZERO, &[]))
    }

    fn client(
        &self,
        intent: Option<&TurnIntent>,
        env: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Codex, AgentError> {
        let mut builder = Codex::builder();
        if let Some(binary) = &self.binary {
            builder = builder.binary(binary);
        }
        for arg in &self.binary_args {
            builder = builder.arg(arg);
        }
        if let Some(timeout) = self.timeout {
            builder = builder.timeout(timeout);
        }
        if let Some(intent) = intent {
            if let Some(dir) = &intent.working_dir {
                builder = builder.working_dir(dir);
            }
            if let Some(home) = &intent.config_home {
                std::fs::create_dir_all(home).map_err(|error| AgentError::Io {
                    detail: format!("failed to create codex home '{home}': {error}"),
                })?;
                builder = builder.env("CODEX_HOME", home);
            }
            if intent.config_home.is_some() || intent.token_env.is_some() {
                for variable in codex_wrapper::auth::AUTH_ENV_VARS {
                    builder = builder.env(variable, "");
                }
            }
            if let Some(variable) = &intent.token_env {
                match std::env::var(variable) {
                    Ok(token) if !token.is_empty() => {
                        builder = builder.env("CODEX_API_KEY", token);
                    }
                    _ => tracing::warn!(
                        variable,
                        "token_env is set but the variable is empty or unset"
                    ),
                }
            }
        }
        builder = builder.envs(env);
        builder
            .build()
            .map_err(|error| outcome::classify_failure(error, Duration::ZERO, &[]))
    }
}

impl Provider for CodexProvider {
    fn key(&self) -> ProviderKey {
        ProviderKey::codex()
    }

    fn capabilities(&self) -> Capabilities {
        let mut capabilities = Capabilities::none(self.key());
        capabilities.client_assigned_resume = false;
        capabilities.isolation = true;
        capabilities.credential_isolation = true;
        capabilities.sandbox = true;
        capabilities.scoped_mcp = true;
        capabilities.strict_mcp = true;
        capabilities.allowed_tools = false;
        capabilities.max_provider_turns = false;
        capabilities.effort = true;
        capabilities.reports_cost = false;
        capabilities.reports_token_usage = true;
        capabilities.reports_provider_turns = false;
        capabilities
    }

    fn run<'a>(
        &'a self,
        intent: &'a TurnIntent,
        events: &'a dyn TurnEvents,
    ) -> BoxFut<'a, Result<TurnOutcome, AgentError>> {
        Box::pin(async move { self.run_turn(intent, events).await })
    }

    fn owns_process(&self, ps_line: &str) -> bool {
        ps_line.split_whitespace().any(|token| {
            matches!(
                Path::new(token).file_name().and_then(|name| name.to_str()),
                Some("codex" | "codex.exe" | "codex.js")
            )
        })
    }
}

impl CodexProvider {
    async fn run_turn(
        &self,
        intent: &TurnIntent,
        sink: &dyn TurnEvents,
    ) -> Result<TurnOutcome, AgentError> {
        let prepared = command::build(intent)?;
        let codex = self.client(Some(intent), prepared.env)?;
        let collected = Arc::new(Mutex::new(Vec::<JsonLineEvent>::new()));
        let callback_events = Arc::clone(&collected);
        let (resume_tx, resume_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut seen_thread = None;
        let callback = move |event: JsonLineEvent| {
            if let Some(thread_id) = event.thread_id()
                && seen_thread.as_deref() != Some(thread_id)
            {
                seen_thread = Some(thread_id.to_string());
                let _ = resume_tx.send(ResumeId::ProviderAssigned(thread_id.to_string()));
            }
            callback_events
                .lock()
                .expect("codex event collection lock")
                .push(event);
        };

        let started = Instant::now();
        let result = match prepared.command {
            command::PreparedCommand::Exec(command) => {
                drive_stream(command.stream(&codex, callback), resume_rx, sink).await
            }
            command::PreparedCommand::Resume(command) => {
                drive_stream(command.stream(&codex, callback), resume_rx, sink).await
            }
        };
        let elapsed = started.elapsed();
        let events = collected
            .lock()
            .expect("codex event collection lock")
            .clone();

        match result {
            Ok(()) => Ok(outcome::from_events(events, elapsed)),
            Err(_) if outcome::has_terminal_failure(&events) => {
                Ok(outcome::from_events(events, elapsed))
            }
            Err(error) => Err(outcome::classify_failure(error, elapsed, &events)),
        }
    }
}

async fn drive_stream<F>(
    stream: F,
    mut resume_rx: tokio::sync::mpsc::UnboundedReceiver<ResumeId>,
    sink: &dyn TurnEvents,
) -> codex_wrapper::Result<()>
where
    F: Future<Output = codex_wrapper::Result<()>>,
{
    tokio::pin!(stream);
    loop {
        tokio::select! {
            result = &mut stream => {
                while let Ok(resume) = resume_rx.try_recv() {
                    sink.resume_id(&resume).await;
                }
                return result;
            }
            resume = resume_rx.recv() => {
                if let Some(resume) = resume {
                    sink.resume_id(&resume).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciacola_agent::{
        Constraint, Cost, Isolation, McpEndpoint, McpScope, NoEvents, Sandbox, TokenUsage, Usage,
    };

    fn fixture(name: &str, args: impl IntoIterator<Item = String>) -> CodexProvider {
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join(name);
        CodexProvider {
            binary: Some(PathBuf::from("/bin/bash")),
            binary_args: std::iter::once(script.display().to_string())
                .chain(args)
                .collect(),
            timeout: Some(Duration::from_secs(3)),
        }
    }

    fn intent(prompt: &str) -> TurnIntent {
        let mut intent = TurnIntent::new(prompt);
        intent.allowed_tools = None;
        intent
    }

    fn temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ciacola-codex-{label}-{}-{}",
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
        marker: Option<PathBuf>,
    }

    impl TurnEvents for RecordingEvents {
        fn resume_id<'a>(&'a self, id: &'a ResumeId) -> BoxFut<'a, ()> {
            Box::pin(async move {
                self.ids
                    .lock()
                    .expect("recording event lock")
                    .push(id.value().to_string());
                if let Some(marker) = &self.marker {
                    std::fs::write(marker, "persisted").expect("write persistence marker");
                }
            })
        }
    }

    #[test]
    fn the_provider_declares_real_capabilities_and_the_tool_policy_gap() {
        let capabilities = CodexProvider::new().capabilities();
        assert!(capabilities.sandbox);
        assert!(capabilities.scoped_mcp);
        assert!(capabilities.strict_mcp);
        assert!(capabilities.reports_token_usage);
        assert!(!capabilities.reports_cost);
        assert!(!capabilities.allowed_tools);
        assert!(!capabilities.client_assigned_resume);

        let mut intent = TurnIntent::new("go");
        intent.allowed_tools = Some(vec!["Read".into()]);
        let validation = capabilities.validate(&intent);
        assert_eq!(
            validation
                .blocking()
                .map(|unsupported| unsupported.constraint),
            Some(Constraint::AllowedTools)
        );
    }

    #[test]
    fn every_supported_security_constraint_validates() {
        let mut intent = TurnIntent::new("go");
        intent.allowed_tools = None;
        intent.isolation = Isolation::Full;
        intent.config_home = Some("/tmp/codex-home".into());
        intent.token_env = Some("CIACOLA_CODEX_TOKEN".into());
        intent.sandbox = Sandbox::WorkspaceWriteNoNetwork;
        intent.mcp = Some(McpScope {
            endpoints: vec![McpEndpoint {
                name: "ciacola".into(),
                url: "http://127.0.0.1:4823/mcp".into(),
                headers: Default::default(),
            }],
            strict: true,
        });
        let validation = CodexProvider::new().capabilities().validate(&intent);
        assert!(validation.blocking().is_none(), "{validation:?}");
    }

    #[test]
    fn owns_only_codex_process_tokens() {
        let provider = CodexProvider::new();
        assert!(provider.owns_process("42 codex exec --json prompt"));
        assert!(provider.owns_process("42 node /opt/codex.js exec prompt"));
        assert!(!provider.owns_process("42 codex-wrapper-test prompt"));
        assert!(!provider.owns_process("42 vim /tmp/codex/notes"));
    }

    #[tokio::test]
    async fn an_explicit_missing_binary_is_a_launch_failure() {
        let provider = CodexProvider {
            binary: Some(PathBuf::from("/definitely/not/a/codex-binary")),
            ..Default::default()
        };
        let mut intent = TurnIntent::new("go");
        intent.allowed_tools = None;
        let error = provider
            .run(&intent, &NoEvents)
            .await
            .expect_err("missing binary");
        assert!(matches!(error, AgentError::Launch { .. }));
    }

    #[tokio::test]
    async fn a_fake_opening_turn_streams_reply_thread_and_usage() {
        let provider = fixture("fake-codex-success.sh", []);
        let events = RecordingEvents::default();
        let outcome = provider
            .run(&intent("open"), &events)
            .await
            .expect("fake run");

        assert_eq!(outcome.reply, "fake reply");
        assert_eq!(outcome.cost, Cost::NotPriced);
        assert_eq!(
            outcome.usage,
            Usage::Reported(TokenUsage {
                input: 21,
                output: 5,
                cached_input: 13,
            })
        );
        assert_eq!(
            events.ids.lock().expect("ids").as_slice(),
            ["thread-success"]
        );
    }

    #[tokio::test]
    async fn thread_started_is_persisted_before_the_child_can_finish() {
        let marker = temp_path("early-marker");
        let provider = fixture("fake-codex-early.sh", [marker.display().to_string()]);
        let events = RecordingEvents {
            ids: Mutex::new(Vec::new()),
            marker: Some(marker.clone()),
        };

        let outcome = provider
            .run(&intent("open"), &events)
            .await
            .expect("the sink releases the blocked child");

        assert_eq!(outcome.reply, "persisted");
        assert_eq!(events.ids.lock().expect("ids").as_slice(), ["thread-early"]);
        let _ = std::fs::remove_file(marker);
    }

    #[tokio::test]
    async fn a_resume_command_reaches_the_wrapper_with_the_stored_thread() {
        let capture = temp_path("resume-args");
        let provider = fixture("fake-codex-capture.sh", [capture.display().to_string()]);
        let mut resumed = intent("continue now");
        resumed.resume = Some(ResumeId::ProviderAssigned("thread-existing".into()));

        let outcome = provider
            .run(&resumed, &NoEvents)
            .await
            .expect("fake resume");
        assert_eq!(outcome.reply, "continued");

        let args = std::fs::read_to_string(&capture).expect("captured argv");
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(&args[..2], ["exec", "resume"]);
        assert!(args.contains(&"thread-existing"), "{args:?}");
        assert!(args.contains(&"continue now"), "{args:?}");
        let _ = std::fs::remove_file(capture);
    }

    #[tokio::test]
    async fn a_terminal_failed_event_is_an_outcome_even_on_nonzero_exit() {
        let provider = fixture("fake-codex-failed.sh", []);
        let outcome = provider
            .run(&intent("fail"), &NoEvents)
            .await
            .expect("provider-reported failure is data");

        assert!(!outcome.succeeded());
        assert_eq!(outcome.failure_message(), Some("fake provider failure"));
        assert_eq!(
            outcome.resume,
            Some(ResumeId::ProviderAssigned("thread-failed".into()))
        );
        assert_eq!(
            outcome.usage,
            Usage::Reported(TokenUsage {
                input: 9,
                output: 2,
                cached_input: 0,
            })
        );
    }

    #[tokio::test]
    async fn a_real_fake_config_rejection_is_pre_turn() {
        let provider = fixture("fake-codex-config.sh", []);
        let error = provider
            .run(&intent("do not leak this prompt"), &NoEvents)
            .await
            .expect_err("config must be rejected");
        assert!(matches!(error, AgentError::Launch { .. }));
        assert!(!error.to_string().contains("do not leak this prompt"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_turn_kills_the_fake_codex_process_group() {
        let pid_file = temp_path("blocking-pid");
        let provider = fixture("fake-codex-blocks.sh", [pid_file.display().to_string()]);
        let run = tokio::spawn(async move { provider.run(&intent("block"), &NoEvents).await });

        let pid = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&pid_file) {
                    break pid.trim().to_string();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake process started");

        run.abort();
        let _ = run.await;
        let gone = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let alive = std::process::Command::new("kill")
                    .args(["-0", &pid])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success());
                if !alive {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();

        assert!(
            gone,
            "fake codex process {pid} survived future cancellation"
        );
        let _ = std::fs::remove_file(pid_file);
    }
}
