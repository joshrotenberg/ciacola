//! Provider-neutral intent translated into Codex exec builders.

use std::collections::BTreeMap;

use ciacola_agent::{
    AgentError, Effort, Isolation, McpScope, ProviderKey, ResumeId, Sandbox, TurnIntent,
};
#[cfg(test)]
use codex_wrapper::CodexCommand;
use codex_wrapper::{
    ApprovalPolicy, ExecCommand, ExecResumeCommand, McpConfigBuilder, McpServerConfig, SandboxMode,
};

/// An opening turn or a resumed turn, with the child-only environment
/// required by its scoped MCP headers.
#[derive(Debug)]
pub(crate) struct PreparedTurn {
    pub(crate) command: PreparedCommand,
    pub(crate) env: BTreeMap<String, String>,
}

#[derive(Debug)]
pub(crate) enum PreparedCommand {
    Exec(ExecCommand),
    Resume(ExecResumeCommand),
}

impl PreparedCommand {
    #[cfg(test)]
    pub(crate) fn args(&self) -> Vec<String> {
        match self {
            Self::Exec(command) => command.args(),
            Self::Resume(command) => command.args(),
        }
    }
}

/// Build one command without spawning it.
pub(crate) fn build(intent: &TurnIntent) -> Result<PreparedTurn, AgentError> {
    let provider = ProviderKey::codex();
    if intent.allowed_tools.is_some() {
        return Err(AgentError::Unsupported {
            provider,
            constraint: ciacola_agent::Constraint::AllowedTools,
            detail: "Codex sandbox and exec-policy controls do not enforce Claude-style tool names"
                .into(),
        });
    }

    let mut config = Vec::new();
    if let Some(instructions) = &intent.instructions {
        config.push(format!(
            "developer_instructions={}",
            toml::Value::String(instructions.clone())
        ));
    }
    if let Some(effort) = intent.effort {
        config.push(format!(
            "model_reasoning_effort=\"{}\"",
            effort_name(effort)
        ));
    }

    match intent.sandbox {
        Sandbox::Unconstrained | Sandbox::ReadOnly => {}
        Sandbox::WorkspaceWrite => {
            config.push("sandbox_workspace_write.network_access=true".into());
        }
        Sandbox::WorkspaceWriteNoNetwork => {
            config.push("sandbox_workspace_write.network_access=false".into());
        }
    }

    // MCP header values have to exist in the Codex process environment so
    // `env_http_headers` can read them. For strict internal MCP turns, the
    // user config is also ignored below; in that sealed shape these excludes
    // keep the values (and ambient Ciacola/client bearer names) out of shell
    // commands the model launches. They are defense in depth rather than an
    // isolation claim for arbitrary user-configured Codex turns.
    config.push("shell_environment_policy.ignore_default_excludes=false".into());
    config.push("shell_environment_policy.exclude=[\"CIACOLA_*\",\"MCP_BEARER\"]".into());

    let mut env = BTreeMap::new();
    if let Some(scope) = &intent.mcp {
        let mcp = mcp_config(scope, &mut env);
        config.extend(mcp.config_overrides());
    }

    let sealed_from_user = matches!(intent.isolation, Isolation::Full)
        || intent.mcp.as_ref().is_some_and(|scope| scope.strict);
    let sealed_from_project = matches!(intent.isolation, Isolation::Full | Isolation::Project);

    let command = match intent.resume.as_ref().filter(|resume| resume.is_open()) {
        Some(ResumeId::ProviderAssigned(thread_id)) => {
            let mut command = ExecResumeCommand::new()
                .session_id(thread_id)
                .prompt(&intent.prompt)
                .approval_policy(ApprovalPolicy::Never)
                .strict_config();
            if let Some(model) = &intent.model {
                command = command.model(model);
            }
            if sealed_from_user {
                command = command.ignore_user_config();
            }
            if sealed_from_project {
                command = command.ignore_rules();
            }
            command = apply_resume_sandbox(command, intent.sandbox);
            for value in config {
                command = command.config(value);
            }
            PreparedCommand::Resume(command)
        }
        _ => {
            let mut command = ExecCommand::new(&intent.prompt)
                .approval_policy(ApprovalPolicy::Never)
                .strict_config();
            if let Some(model) = &intent.model {
                command = command.model(model);
            }
            if sealed_from_user {
                command = command.ignore_user_config();
            }
            if sealed_from_project {
                command = command.ignore_rules();
            }
            command = apply_exec_sandbox(command, intent.sandbox);
            for value in config {
                command = command.config(value);
            }
            PreparedCommand::Exec(command)
        }
    };

    Ok(PreparedTurn { command, env })
}

fn effort_name(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Xhigh => "xhigh",
        Effort::Max => "max",
    }
}

fn apply_exec_sandbox(command: ExecCommand, sandbox: Sandbox) -> ExecCommand {
    match sandbox {
        Sandbox::Unconstrained => command,
        Sandbox::ReadOnly => command.sandbox(SandboxMode::ReadOnly),
        Sandbox::WorkspaceWrite | Sandbox::WorkspaceWriteNoNetwork => {
            command.sandbox(SandboxMode::WorkspaceWrite)
        }
    }
}

fn apply_resume_sandbox(command: ExecResumeCommand, sandbox: Sandbox) -> ExecResumeCommand {
    match sandbox {
        Sandbox::Unconstrained => command,
        Sandbox::ReadOnly => command.config("sandbox_mode=\"read-only\""),
        Sandbox::WorkspaceWrite | Sandbox::WorkspaceWriteNoNetwork => {
            command.config("sandbox_mode=\"workspace-write\"")
        }
    }
}

fn mcp_config(scope: &McpScope, env: &mut BTreeMap<String, String>) -> McpConfigBuilder {
    let mut mcp = McpConfigBuilder::new();
    for (server_index, endpoint) in scope.endpoints.iter().enumerate() {
        let mut server = McpServerConfig::http(&endpoint.url);
        for (header_index, (header, value)) in endpoint.headers.iter().enumerate() {
            let variable = format!("CIACOLA_MCP_{server_index}_HEADER_{header_index}");
            env.insert(variable.clone(), value.clone());
            server = server.env_http_header(header, variable);
        }
        if scope.strict {
            server = server.required();
        }
        mcp = mcp.server(&endpoint.name, server);
    }
    mcp
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciacola_agent::{McpEndpoint, Sandbox};

    fn config_values(args: &[String]) -> Vec<&str> {
        args.windows(2)
            .filter(|pair| pair[0] == "-c")
            .map(|pair| pair[1].as_str())
            .collect()
    }

    #[test]
    fn an_opening_turn_carries_instructions_model_effort_and_unattended_policy() {
        let mut intent = TurnIntent::new("implement it");
        intent.instructions = Some("Be exact.\nDo the work.".into());
        intent.model = Some("gpt-5.6-sol".into());
        intent.effort = Some(Effort::High);
        intent.allowed_tools = None;
        intent.isolation = Isolation::Full;
        intent.sandbox = Sandbox::WorkspaceWriteNoNetwork;
        intent.resume = Some(ResumeId::ClientAssigned("ignored-client-id".into()));

        let prepared = build(&intent).expect("supported");
        let args = prepared.command.args();
        assert_eq!(args.first().map(String::as_str), Some("exec"));
        assert!(!args.contains(&"resume".to_string()));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "gpt-5.6-sol"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sandbox", "workspace-write"])
        );
        assert!(args.contains(&"--ignore-user-config".to_string()));
        assert!(args.contains(&"--ignore-rules".to_string()));

        let config = config_values(&args);
        assert!(
            config
                .iter()
                .any(|value| value.starts_with("developer_instructions="))
        );
        assert!(config.contains(&"model_reasoning_effort=\"high\""));
        assert!(config.contains(&"approval_policy=\"never\""));
        assert!(config.contains(&"sandbox_workspace_write.network_access=false"));
        assert!(config.contains(&"shell_environment_policy.ignore_default_excludes=false"));
        assert!(
            config.contains(&"shell_environment_policy.exclude=[\"CIACOLA_*\",\"MCP_BEARER\"]")
        );
    }

    #[test]
    fn a_resumed_turn_uses_the_thread_and_reapplies_runtime_policy() {
        let mut intent = TurnIntent::new("continue");
        intent.allowed_tools = None;
        intent.model = Some("gpt-5.6-sol".into());
        intent.sandbox = Sandbox::ReadOnly;
        intent.resume = Some(ResumeId::ProviderAssigned("thread-123".into()));

        let args = build(&intent).expect("supported").command.args();
        assert_eq!(&args[..2], ["exec", "resume"]);
        assert!(args.contains(&"thread-123".to_string()));
        assert!(args.contains(&"continue".to_string()));
        let config = config_values(&args);
        assert!(config.contains(&"approval_policy=\"never\""));
        assert!(config.contains(&"sandbox_mode=\"read-only\""));
        assert!(
            config.contains(&"shell_environment_policy.exclude=[\"CIACOLA_*\",\"MCP_BEARER\"]")
        );
        assert!(
            !config
                .iter()
                .any(|value| value.starts_with("developer_instructions="))
        );
    }

    #[test]
    fn mcp_identity_is_env_backed_required_and_absent_from_argv() {
        let mut intent = TurnIntent::new("use the server");
        intent.allowed_tools = None;
        intent.mcp = Some(McpScope {
            endpoints: vec![McpEndpoint {
                name: "ciacola".into(),
                url: "http://127.0.0.1:4823/mcp".into(),
                headers: BTreeMap::from([("x-ciacola-agent".into(), "top-secret".into())]),
            }],
            strict: true,
        });

        let prepared = build(&intent).expect("supported");
        let args = prepared.command.args();
        let rendered = args.join(" ");
        assert!(
            !rendered.contains("top-secret"),
            "secret reached argv: {rendered}"
        );
        assert!(rendered.contains("env_http_headers"), "{rendered}");
        assert!(rendered.contains("x-ciacola-agent"), "{rendered}");
        assert!(rendered.contains("CIACOLA_MCP_0_HEADER_0"), "{rendered}");
        assert!(
            rendered.contains("mcp_servers.ciacola.required=true"),
            "{rendered}"
        );
        assert!(args.contains(&"--ignore-user-config".to_string()));
        assert_eq!(
            prepared
                .env
                .get("CIACOLA_MCP_0_HEADER_0")
                .map(String::as_str),
            Some("top-secret")
        );
    }

    #[test]
    fn a_claude_tool_grant_is_refused_instead_of_claimed() {
        let mut intent = TurnIntent::new("go");
        intent.allowed_tools = Some(vec!["Read".into()]);
        let error = build(&intent).expect_err("cannot enforce this vocabulary");
        assert!(matches!(
            error,
            AgentError::Unsupported {
                constraint: ciacola_agent::Constraint::AllowedTools,
                ..
            }
        ));
    }
}
