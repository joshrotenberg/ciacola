//! Translating a [`TurnIntent`] into a `claude-wrapper` [`QueryCommand`].
//!
//! Kept apart from process execution so the translation is testable
//! without spawning anything: every test in this module inspects
//! [`ClaudeCommand::args`] or [`QueryCommand::to_command_string`], the
//! same rendering the wrapper itself offers a caller who wants a
//! preview rather than a run.

use ciacola_agent::{AgentError, Effort as ContractEffort, Isolation, TurnIntent};
use claude_wrapper::{
    Effort as WrapperEffort, HermeticScope, McpConfigBuilder, QueryCommand, TempMcpConfig,
};

/// Build the full query command for one turn, materializing an MCP
/// config file when the intent scopes one.
///
/// `mcp_guard` receives the temp file so the caller can keep it alive
/// for the lifetime of the spawned process: the CLI reads the file by
/// path, and dropping it before the run resolves would delete the
/// config out from under the child.
pub(crate) fn build(
    intent: &TurnIntent,
    mcp_guard: &mut Option<TempMcpConfig>,
) -> Result<QueryCommand, AgentError> {
    let mut command = query_for_intent(intent);

    if let Some(model) = &intent.model {
        command = command.model(model.clone());
    }
    if let Some(effort) = intent.effort {
        command = command.effort(map_effort(effort));
    }
    match &intent.allowed_tools {
        // No policy requested: preserve the provider's defaults.
        None => {}
        // These are two different Claude controls. `--tools ""`
        // removes every built-in tool from the model's available set;
        // `--allowed-tools ""` makes the permission grant empty as
        // well, including for dynamically loaded tools such as MCP.
        // The wrapper omits either flag for an actually empty iterator,
        // so a one-element empty string is intentional here.
        Some(tools) if tools.is_empty() => {
            command = command.tools([""]).allowed_tools([""]);
        }
        Some(tools) => {
            command = command.allowed_tools(tools.clone());
        }
    }
    if let Some(max_turns) = intent.max_provider_turns {
        command = command.max_turns(max_turns);
    }
    if let Some(ceiling) = &intent.turn_ceiling {
        if ceiling.limit == 0 {
            return Err(AgentError::Unsupported {
                provider: ciacola_agent::ProviderKey::claude(),
                constraint: ciacola_agent::Constraint::TurnCeiling,
                detail: "Claude's per-turn micro-USD ceiling must be positive".into(),
            });
        }
        command = command.max_budget_usd(ceiling.limit as f64 / 1_000_000.0);
    }
    if let Some(scope) = &intent.mcp {
        // Endpoints (and their per-agent secret headers) become a JSON
        // file on disk, the shape the CLI's own --mcp-config already
        // expects; only the *path* reaches argv. `McpEndpoint::Debug`
        // redacts header values, but that only protects against this
        // crate's own logging -- the header values still have to reach
        // the file the CLI reads, which is the whole point of sending
        // them.
        let mut builder = McpConfigBuilder::new();
        for endpoint in &scope.endpoints {
            builder = builder.http_server_with_headers(
                endpoint.name.clone(),
                endpoint.url.clone(),
                endpoint.headers.clone(),
            );
        }
        let temp = builder.build_temp().map_err(|e| AgentError::Io {
            detail: format!("failed to write MCP config for claude: {e}"),
        })?;
        command = command.mcp_config(temp.path());
        if scope.strict {
            command = command.strict_mcp_config();
        }
        *mcp_guard = Some(temp);
    }
    match intent.isolation {
        Isolation::Full => command = command.hermetic_scoped(HermeticScope::Full),
        Isolation::Project => command = command.hermetic_scoped(HermeticScope::Project),
        Isolation::Inherit => {}
    }

    Ok(command)
}

/// The one session-state choice, reproducing `ciacola-core::agent`'s
/// `query_for_session` against [`ResumeId`](ciacola_agent::ResumeId)
/// instead of the old `(Option<&str>, bool)` pair.
///
/// - [`ResumeId::ProviderAssigned`](ciacola_agent::ResumeId::ProviderAssigned):
///   the conversation is open at the backend. `--resume` continues it;
///   instructions are never resent, because the session already
///   carries them.
/// - [`ResumeId::ClientAssigned`](ciacola_agent::ResumeId::ClientAssigned):
///   we named it but the backend has not seen it yet. `--session-id`
///   opens it under that name, and instructions are sent because this
///   turn is the one that opens the conversation.
/// - No resume at all: agents created before ids were assigned. No
///   session flag, so the provider mints one; instructions are sent
///   because this is also an opening turn.
pub(crate) fn query_for_intent(intent: &TurnIntent) -> QueryCommand {
    let mut command = QueryCommand::new(intent.prompt.clone());
    match &intent.resume {
        Some(resume) if resume.is_open() => {
            command = command.resume(resume.value());
        }
        Some(resume) => {
            command = command.session_id(resume.value());
            if let Some(instructions) = &intent.instructions {
                command = command.system_prompt(instructions.clone());
            }
        }
        None => {
            if let Some(instructions) = &intent.instructions {
                command = command.system_prompt(instructions.clone());
            }
        }
    }
    command
}

fn map_effort(effort: ContractEffort) -> WrapperEffort {
    match effort {
        ContractEffort::Low => WrapperEffort::Low,
        ContractEffort::Medium => WrapperEffort::Medium,
        ContractEffort::High => WrapperEffort::High,
        ContractEffort::Xhigh => WrapperEffort::Xhigh,
        ContractEffort::Max => WrapperEffort::Max,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciacola_agent::{McpEndpoint, McpScope, ResumeId};
    use claude_wrapper::{Claude, ClaudeCommand};
    use std::collections::BTreeMap;

    fn intent(prompt: &str) -> TurnIntent {
        TurnIntent::new(prompt)
    }

    fn with_ceiling(mut intent: TurnIntent, limit: u64) -> TurnIntent {
        intent.turn_ceiling = Some(ciacola_agent::TurnCeiling {
            capability: crate::turn_ceiling_capability(),
            limit,
        });
        intent
    }

    #[test]
    fn a_fresh_turn_with_no_resume_sends_instructions_and_no_session_flag() {
        let mut i = intent("hello");
        i.instructions = Some("you are an agent".into());
        let args = query_for_intent(&i).args();

        assert!(!args.contains(&"--resume".to_string()));
        assert!(!args.contains(&"--session-id".to_string()));
        let pos = args.iter().position(|a| a == "--system-prompt").unwrap();
        assert_eq!(args[pos + 1], "you are an agent");
    }

    #[test]
    fn a_fresh_turn_with_no_instructions_sends_no_system_prompt_flag() {
        let i = intent("hello");
        let args = query_for_intent(&i).args();
        assert!(!args.contains(&"--system-prompt".to_string()));
    }

    #[test]
    fn a_client_assigned_resume_opens_the_session_and_sends_instructions() {
        let mut i = intent("hello again");
        i.instructions = Some("you are an agent".into());
        i.resume = Some(ResumeId::ClientAssigned("agent-1".into()));
        let args = query_for_intent(&i).args();

        let pos = args.iter().position(|a| a == "--session-id").unwrap();
        assert_eq!(args[pos + 1], "agent-1");
        assert!(!args.contains(&"--resume".to_string()));
        assert!(args.contains(&"--system-prompt".to_string()));
    }

    /// The whole point of the resume/instructions split: a conversation
    /// that already exists at the backend must never be sent the
    /// instructions again, and must be continued with `--resume`, not
    /// renamed with `--session-id`.
    #[test]
    fn a_provider_assigned_resume_continues_the_session_and_never_resends_instructions() {
        let mut i = intent("continue");
        i.instructions = Some("you are an agent".into());
        i.resume = Some(ResumeId::ProviderAssigned("sess-1".into()));
        let args = query_for_intent(&i).args();

        let pos = args.iter().position(|a| a == "--resume").unwrap();
        assert_eq!(args[pos + 1], "sess-1");
        assert!(!args.contains(&"--session-id".to_string()));
        assert!(
            !args.contains(&"--system-prompt".to_string()),
            "a resumed session already carries its instructions: {args:?}"
        );
    }

    #[test]
    fn model_effort_tools_and_internal_turn_ceiling_render_as_flags() {
        let mut i = intent("go");
        i.model = Some("opus".into());
        i.effort = Some(ContractEffort::High);
        i.allowed_tools = Some(vec!["Read".into(), "Edit".into()]);
        i.max_provider_turns = Some(12);

        let mut guard = None;
        let args = build(&i, &mut guard).expect("builds").args();

        let pos = args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(args[pos + 1], "opus");
        let pos = args.iter().position(|a| a == "--effort").unwrap();
        assert_eq!(args[pos + 1], "high");
        let pos = args.iter().position(|a| a == "--allowed-tools").unwrap();
        assert_eq!(args[pos + 1], "Read,Edit");
        let pos = args.iter().position(|a| a == "--max-turns").unwrap();
        assert_eq!(args[pos + 1], "12");
    }

    #[test]
    fn a_micro_usd_ceiling_is_identical_on_open_and_resume() {
        let open = with_ceiling(intent("open"), 12_345);
        let mut resume = with_ceiling(intent("resume"), 12_345);
        resume.resume = Some(ResumeId::ProviderAssigned("session-1".into()));

        for turn in [open, resume] {
            let mut guard = None;
            let args = build(&turn, &mut guard).expect("builds").args();
            let position = args
                .iter()
                .position(|arg| arg == "--max-budget-usd")
                .expect("native budget flag");
            assert_eq!(args[position + 1], "0.012345", "{args:?}");
        }
    }

    #[test]
    fn a_zero_micro_usd_ceiling_is_rejected_before_launch() {
        let turn = with_ceiling(intent("go"), 0);
        let mut guard = None;
        let error = build(&turn, &mut guard).expect_err("zero cannot authorize work");
        assert!(matches!(
            error,
            AgentError::Unsupported {
                constraint: ciacola_agent::Constraint::TurnCeiling,
                ..
            }
        ));
    }

    /// The legacy path used an empty vector for both "inherit" and
    /// "none", so a supposedly toolless agent received no CLI flags and
    /// therefore inherited Claude's tools. The contract's outer Option
    /// keeps the two states distinct and this adapter must render each
    /// one differently.
    #[test]
    fn an_explicitly_toolless_turn_disables_availability_and_permission() {
        let mut i = intent("reason only");
        i.allowed_tools = Some(Vec::new());

        let mut guard = None;
        let args = build(&i, &mut guard).expect("builds").args();

        let tools = args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(args[tools + 1], "");
        let allowed = args.iter().position(|a| a == "--allowed-tools").unwrap();
        assert_eq!(args[allowed + 1], "");
    }

    #[test]
    fn an_inherited_tool_policy_emits_no_tool_flags() {
        let i = intent("use provider defaults");
        assert!(i.allowed_tools.is_none());

        let mut guard = None;
        let args = build(&i, &mut guard).expect("builds").args();
        assert!(!args.contains(&"--tools".to_string()));
        assert!(!args.contains(&"--allowed-tools".to_string()));
    }

    #[test]
    fn full_isolation_seals_the_run_and_project_isolation_keeps_the_users_settings() {
        let mut i = intent("go");
        i.isolation = Isolation::Full;
        let mut guard = None;
        let args = build(&i, &mut guard).expect("builds").args();
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--setting-sources" && w[1].is_empty()),
            "{args:?}"
        );
        assert!(args.contains(&"--strict-mcp-config".to_string()));

        let mut i = intent("go");
        i.isolation = Isolation::Project;
        let mut guard = None;
        let args = build(&i, &mut guard).expect("builds").args();
        assert!(args.windows(2).any(|w| w == ["--setting-sources", "user"]));

        let mut i = intent("go");
        i.isolation = Isolation::Inherit;
        let mut guard = None;
        let args = build(&i, &mut guard).expect("builds").args();
        assert!(!args.contains(&"--setting-sources".to_string()));
    }

    /// The MCP endpoint list becomes a file on disk, and only its path
    /// reaches argv: the secret header value must never appear there.
    #[test]
    fn mcp_endpoints_materialize_a_file_and_never_put_the_secret_in_argv() {
        let mut headers = BTreeMap::new();
        headers.insert("x-ciacola-agent".to_string(), "top-secret".to_string());
        let mut i = intent("go");
        i.mcp = Some(McpScope {
            endpoints: vec![McpEndpoint {
                name: "ciacola".into(),
                url: "http://127.0.0.1:4823/mcp".into(),
                headers,
            }],
            strict: true,
        });

        let mut guard = None;
        let args = build(&i, &mut guard).expect("builds").args();

        assert!(args.contains(&"--strict-mcp-config".to_string()));
        let pos = args.iter().position(|a| a == "--mcp-config").unwrap();
        let path = &args[pos + 1];
        assert!(
            !path.contains("top-secret"),
            "argv must not carry the secret: {args:?}"
        );

        let written = std::fs::read_to_string(path).expect("temp mcp config written");
        assert!(
            written.contains("top-secret"),
            "the CLI still has to receive the header value from the file: {written}"
        );
        assert!(
            guard.is_some(),
            "the temp file must be handed back so the caller keeps it alive for the run"
        );
    }

    /// A non-strict scope must not set `--strict-mcp-config`, matching
    /// the intent's own `strict` flag rather than always sealing.
    #[test]
    fn a_non_strict_mcp_scope_does_not_force_strict_mcp_config() {
        let mut i = intent("go");
        i.mcp = Some(McpScope {
            endpoints: vec![McpEndpoint {
                name: "ciacola".into(),
                url: "http://127.0.0.1:4823/mcp".into(),
                headers: BTreeMap::new(),
            }],
            strict: false,
        });
        let mut guard = None;
        let args = build(&i, &mut guard).expect("builds").args();
        assert!(!args.contains(&"--strict-mcp-config".to_string()));
    }

    /// `to_command_string` is the wrapper's own no-execution preview;
    /// exercising it here pins that our translation renders a command
    /// that could actually be run, without running it.
    #[test]
    fn the_full_command_renders_as_a_preview_without_executing_anything() {
        let claude = Claude::builder()
            .binary("/usr/local/bin/claude")
            .build()
            .unwrap();
        let mut i = intent("explain quicksort");
        i.model = Some("sonnet".into());
        i.resume = Some(ResumeId::ProviderAssigned("sess-9".into()));

        let mut guard = None;
        let preview = build(&i, &mut guard)
            .expect("builds")
            .to_command_string(&claude);

        assert!(preview.starts_with("/usr/local/bin/claude"));
        assert!(preview.contains("--resume"));
        assert!(preview.contains("sess-9"));
        assert!(preview.contains("--model"));
        assert!(preview.contains("sonnet"));
    }
}
