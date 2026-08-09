//! Config-born agents: the persistent ones, declared in TOML.
//!
//! "Persistent versus ephemeral" is not a type in this system; an agent
//! is an agent. The difference is authorship. Ephemeral agents are
//! spawned by a person or another agent mid-flight and retired when
//! their job is done. Persistent agents are declared here, upserted at
//! every boot by name: the definition follows the file, the identity
//! and the conversation persist across restarts and redefinitions.
//!
//! A declared agent can also carry a schedule (the wake) and ask for
//! the loopback (`loopback = true`), which grants it this server's own
//! spawn/send/wait/get/list/retire as tools. `{agent_id}` in the
//! system prompt is substituted at upsert time, so an orchestrator
//! knows its own id and can tag the helpers it spawns.

use std::collections::{BTreeMap, HashMap};
use std::io::ErrorKind;

use serde::Deserialize;

use ciacola_core::agent::{AgentDef, FlatError};
use ciacola_core::ledger::Ledger;
use ciacola_core::roles::{Role, Roles};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub agents: Vec<ConfigAgent>,
    /// Reusable agent definitions, exposed as MCP prompts and to
    /// `spawn_role`. A persistent agent may instantiate one by name.
    #[serde(default)]
    pub roles: Vec<Role>,
    /// `[plugins.<name>]` sections, handed to plugins verbatim so a
    /// plugin's settings never appear in `main`.
    #[serde(default = "empty_table")]
    pub plugins: toml::Value,
    /// Circuit breakers. Absent means no spend limit and the default
    /// spawn depth.
    #[serde(default)]
    pub limits: ciacola_core::limits::Limits,
    /// Defaults every agent inherits: isolation from the operator's
    /// ambient provider config, and where session data lands.
    #[serde(default)]
    pub runtime: ciacola_core::roles::Runtime,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            agents: Vec::new(),
            roles: Vec::new(),
            plugins: empty_table(),
            limits: Default::default(),
            runtime: Default::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigAgent {
    pub name: String,
    /// Backend key. Omit for the server-wide default.
    pub provider: Option<String>,
    /// Instantiate this role instead of spelling out a definition. The
    /// fields below still override what the role provides.
    pub role: Option<String>,
    /// Values for the role's declared `{{arguments}}`. Unknown and
    /// missing names are rejected at boot, before the agent is upserted.
    #[serde(default)]
    pub arguments: HashMap<String, String>,
    #[serde(default)]
    pub system_prompt: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub hermetic: Option<String>,
    pub sandbox: Option<String>,
    pub working_dir: Option<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Use the selected provider's native policy instead of Claude tool names.
    #[serde(default)]
    pub inherit_provider_tools: bool,
    pub max_turns: Option<u32>,
    /// Start a fresh provider session after this many turns.
    pub rotate_after_turns: Option<u32>,
    /// Hand this agent the server's own tools over loopback HTTP.
    #[serde(default)]
    pub loopback: bool,
    /// Config belonging to plugins, keyed by plugin name. Core does
    /// not read these; each plugin is offered its own and parses it
    /// itself. See `Plugin::agent_config`.
    #[serde(default)]
    pub plugins: BTreeMap<String, toml::Value>,
}

fn empty_table() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

pub fn load(path: &str) -> Result<Config, FlatError> {
    parse(&std::fs::read_to_string(path)?)
}

pub const DEFAULT_PATH: &str = "ciacola.toml";

/// Load an explicitly selected config strictly. With no override, use
/// `ciacola.toml` when present and otherwise start from the documented
/// empty configuration. A typo in `CIACOLA_CONFIG` must still be loud.
pub fn load_startup(explicit_path: Option<&str>) -> Result<Config, FlatError> {
    load_startup_at(explicit_path, DEFAULT_PATH)
}

fn load_startup_at(explicit_path: Option<&str>, default_path: &str) -> Result<Config, FlatError> {
    match explicit_path {
        Some(path) => load(path),
        None => match std::fs::read_to_string(default_path) {
            Ok(text) => parse(&text),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(error.into()),
        },
    }
}

fn parse(text: &str) -> Result<Config, FlatError> {
    let config: Config = toml::from_str(text)?;
    validate(&config)?;
    Ok(config)
}

fn validate(config: &Config) -> Result<(), FlatError> {
    config.limits.validate()?;
    validate_sandbox("runtime", config.runtime.sandbox.as_deref())?;
    ciacola_agent::ProviderChildEnvironment::validate_passthrough(
        &config.runtime.provider_env_passthrough,
    )?;
    if config.runtime.token_env.is_some() {
        return Err(
            "runtime.token_env is retired because startup environment secrets may remain visible to same-user process inspection; pass the token through CIACOLA_CLAUDE_TOKEN_FD or authenticate claude_home separately"
                .into(),
        );
    }
    if config.runtime.codex_token_env.is_some() {
        return Err(
            "runtime.codex_token_env is retired because startup environment secrets may remain visible to same-user process inspection; pass the token through CIACOLA_CODEX_TOKEN_FD or authenticate codex_home separately"
                .into(),
        );
    }
    for role in &config.roles {
        if role.inherit_provider_tools && !role.allowed_tools.is_empty() {
            return Err(format!(
                "role '{}': inherit_provider_tools and allowed_tools are mutually exclusive",
                role.name
            )
            .into());
        }
        validate_role_surface(role)?;
        validate_sandbox(&format!("role '{}'", role.name), role.sandbox.as_deref())?;
    }
    for agent in &config.agents {
        if agent.inherit_provider_tools && !agent.allowed_tools.is_empty() {
            return Err(format!(
                "agent '{}': inherit_provider_tools and allowed_tools are mutually exclusive",
                agent.name
            )
            .into());
        }
        validate_sandbox(&format!("agent '{}'", agent.name), agent.sandbox.as_deref())?;
    }
    Ok(())
}

fn validate_role_surface(role: &Role) -> Result<(), FlatError> {
    let Some(surface) = role.surface.as_deref() else {
        return Ok(());
    };
    if !role.loopback {
        return Err(format!("role '{}': surface requires loopback = true", role.name).into());
    }
    match surface {
        "agent" => Ok(()),
        "operator" => Err(format!(
            "role '{}': provider-backed operator roles are disabled; use stdio or authenticated human HTTP",
            role.name
        )
        .into()),
        other => Err(format!(
            "role '{}': unknown loopback surface '{other}'; expected agent",
            role.name
        )
        .into()),
    }
}

fn validate_sandbox(owner: &str, sandbox: Option<&str>) -> Result<(), FlatError> {
    if let Some(sandbox) = sandbox
        && ciacola_agent::Sandbox::parse(sandbox).is_none()
    {
        return Err(format!(
            "{owner}: unknown sandbox '{sandbox}'; expected read-only, workspace-write, workspace-write-no-network, or none"
        )
        .into());
    }
    Ok(())
}

fn role_definition(
    config: &Config,
    declared: &ConfigAgent,
    roles: &Roles,
    loopback_mcp_config: &str,
) -> Result<AgentDef, FlatError> {
    let mut def = match &declared.role {
        Some(role_name) => {
            let role = roles
                .get(role_name)
                .ok_or_else(|| format!("agent '{}': no role '{role_name}'", declared.name))?;
            if role.surface.as_deref() == Some("operator") {
                return Err(format!(
                    "agent '{}': role '{}' requests the human-only operator surface",
                    declared.name, role.name
                )
                .into());
            }
            let missing: Vec<&str> = role
                .arguments
                .iter()
                .filter(|name| !declared.arguments.contains_key(*name))
                .map(String::as_str)
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "agent '{}': role '{}' needs arguments {missing:?}",
                    declared.name, role.name
                )
                .into());
            }
            let unknown: Vec<&str> = declared
                .arguments
                .keys()
                .filter(|name| !role.arguments.contains(name))
                .map(String::as_str)
                .collect();
            if !unknown.is_empty() {
                return Err(format!(
                    "agent '{}': role '{}' has no arguments {unknown:?}",
                    declared.name, role.name
                )
                .into());
            }
            let mut def = roles.to_def(role, &declared.arguments);
            def.name = declared.name.clone();
            def
        }
        None => {
            if !declared.arguments.is_empty() {
                return Err(format!("agent '{}': arguments require a role", declared.name).into());
            }
            AgentDef::new(&declared.name, &declared.system_prompt)
        }
    };
    if let Some(provider) = &declared.provider {
        def = def.provider(provider.as_str());
    }
    if let Some(model) = &declared.model {
        def = def.model(model);
    }
    if let Some(effort) = &declared.effort {
        def = def.effort(effort);
    }
    if let Some(dir) = &declared.working_dir {
        def = def.working_dir(dir);
    }
    if declared.inherit_provider_tools {
        def = def.inherit_provider_tools();
    } else if !declared.allowed_tools.is_empty() {
        def = def.allowed_tools(declared.allowed_tools.clone());
    }
    if let Some(sandbox) = &declared.sandbox {
        def = def.sandbox(sandbox);
    }
    if let Some(max_turns) = declared.max_turns {
        def = def.max_turns(max_turns);
    }
    if let Some(rotate) = declared.rotate_after_turns {
        def = def.rotate_after_turns(rotate);
    }
    if declared.loopback {
        def = def.mcp_config(loopback_mcp_config);
    }
    if let Some(scope) = declared
        .hermetic
        .as_ref()
        .or(config.runtime.hermetic.as_ref())
    {
        def = def.hermetic(scope);
    }
    let provider = declared
        .provider
        .as_deref()
        .or_else(|| {
            declared
                .role
                .as_ref()
                .and_then(|name| roles.get(name).and_then(|role| role.provider.as_deref()))
        })
        .or(config.runtime.default_provider.as_deref())
        .unwrap_or(ciacola_agent::ProviderKey::CLAUDE);
    let home = if provider == "codex" {
        &config.runtime.codex_home
    } else {
        &config.runtime.claude_home
    };
    if let Some(home) = home {
        def = def.config_home(home);
    }
    Ok(def)
}

/// Upsert every declared agent and its schedule. Boot-idempotent:
/// running twice changes nothing; editing the file and rebooting
/// updates definitions in place without touching conversations.
pub async fn apply(
    config: &Config,
    ledger: &Ledger,
    host: &ciacola_core::plugin::PluginHost,
    roles: &Roles,
    loopback_mcp_config: &str,
) -> Result<Vec<String>, FlatError> {
    let house_rules = config.runtime.resolved_house_rules()?;
    let mut report = Vec::new();
    for declared in &config.agents {
        // A persistent agent is either spelled out or an instance of a
        // role; the same definitions serve both, which is the whole
        // point of roles.
        let mut def = role_definition(config, declared, roles, loopback_mcp_config)?;
        if let Some(rules) = &house_rules {
            def = def.house_rules(rules.as_str());
        }

        let (agent_id, verb) = match ledger.find_active_by_name(&declared.name).await? {
            Some(existing) => (existing.agent_id, "updated"),
            None => (ledger.create_agent(&def, None).await?, "created"),
        };
        def.system_prompt = def.system_prompt.replace("{agent_id}", &agent_id);
        ledger.update_agent_def(&agent_id, &def).await?;

        // Whatever the declaration says that is not core's business
        // goes to whichever plugin owns it. This pass used to hold a
        // Schedules handle to do one plugin's work, which core could
        // never have offered to a plugin it had not been compiled
        // against.
        host.apply_agent_config(&agent_id, &declared.plugins)
            .await?;

        let claimed = if declared.plugins.is_empty() {
            String::new()
        } else {
            format!(
                " [{}]",
                declared
                    .plugins
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        report.push(format!("{verb} {} ({agent_id}){claimed}", declared.name));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing_path(label: &str) -> String {
        std::env::temp_dir()
            .join(format!(
                "ciacola-{label}-{}-{}.toml",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ))
            .display()
            .to_string()
    }

    #[test]
    fn missing_default_config_is_an_empty_server() {
        let path = missing_path("default");
        let config = load_startup_at(None, &path).expect("an absent default is optional");
        assert!(config.agents.is_empty());
        assert!(config.roles.is_empty());
    }

    #[test]
    fn missing_explicit_config_is_an_error() {
        let path = missing_path("explicit");
        let error = load_startup_at(Some(&path), "ignored")
            .expect_err("an explicit typo must not silently become an empty server");
        assert!(error.to_string().contains("No such file"), "{error}");
    }

    #[test]
    fn codex_runtime_defaults_and_native_policy_parse_as_a_product_surface() {
        let config = parse(
            r#"
                [runtime]
                default_provider = "codex"
                sandbox = "workspace-write-no-network"
                codex_home = "~/.local/share/ciacola/codex"
                provider_env_passthrough = ["SSH_AUTH_SOCK", "HTTPS_PROXY", "CIACOLA_SENTINEL"]

                [[roles]]
                name = "implementer"
                description = "writes code"
                inherit_provider_tools = true
                model = "gpt-5.6-sol"
                system_prompt = "Implement it"

                [[agents]]
                name = "worker"
                role = "implementer"
            "#,
        )
        .expect("codex config");
        let roles = Roles::with_runtime(config.roles.clone(), "agent.json", config.runtime.clone());
        let def =
            role_definition(&config, &config.agents[0], &roles, "agent.json").expect("definition");
        assert!(def.inherit_provider_tools);
        assert_eq!(def.sandbox.as_deref(), Some("workspace-write-no-network"));
        assert_eq!(
            def.config_home.as_deref(),
            Some("~/.local/share/ciacola/codex")
        );
        assert!(def.token_env.is_none());
        assert_eq!(
            config.runtime.provider_env_passthrough,
            ["SSH_AUTH_SOCK", "HTTPS_PROXY", "CIACOLA_SENTINEL"]
        );
    }

    #[test]
    fn legacy_token_environment_settings_fail_with_descriptor_migration() {
        let claude = parse(
            r#"
                [runtime]
                token_env = "CIACOLA_CLAUDE_TOKEN"
            "#,
        )
        .expect_err("startup environment secrets are retired");
        assert!(claude.to_string().contains("runtime.token_env"), "{claude}");
        assert!(
            claude.to_string().contains("CIACOLA_CLAUDE_TOKEN_FD"),
            "{claude}"
        );
        assert!(claude.to_string().contains("claude_home"), "{claude}");

        let codex = parse(
            r#"
                [runtime]
                codex_token_env = "CIACOLA_CODEX_TOKEN"
            "#,
        )
        .expect_err("startup environment secrets are retired");
        assert!(
            codex.to_string().contains("runtime.codex_token_env"),
            "{codex}"
        );
        assert!(
            codex.to_string().contains("CIACOLA_CODEX_TOKEN_FD"),
            "{codex}"
        );
        assert!(codex.to_string().contains("codex_home"), "{codex}");
    }

    #[test]
    fn provider_environment_passthrough_is_exact_and_legacy_configs_default_empty() {
        let legacy = parse("").expect("a pre-field config remains valid");
        assert!(legacy.runtime.provider_env_passthrough.is_empty());

        let sensitive = parse(
            r#"
                [runtime]
                provider_env_passthrough = [
                    "MCP_BEARER",
                    "CIACOLA_SENTINEL",
                    "ANTHROPIC_API_KEY",
                    "CODEX_API_KEY",
                ]
            "#,
        )
        .expect("sensitive values may be deliberately allowlisted by exact name");
        assert_eq!(sensitive.runtime.provider_env_passthrough.len(), 4);

        let malformed = parse(
            r#"
                [runtime]
                provider_env_passthrough = ["HAS-DASH"]
            "#,
        )
        .expect_err("non-portable names must fail at config validation");
        assert!(malformed.to_string().contains("HAS-DASH"), "{malformed}");
        assert!(malformed.to_string().contains("portable"), "{malformed}");
    }

    #[test]
    fn conflicting_tool_policy_and_unknown_sandbox_fail_at_parse_time() {
        let conflict = parse(
            r#"
                [[roles]]
                name = "broken"
                description = "broken"
                allowed_tools = ["Read"]
                inherit_provider_tools = true
                system_prompt = "broken"
            "#,
        )
        .expect_err("conflicting authority must fail");
        assert!(conflict.to_string().contains("mutually exclusive"));

        let sandbox = parse(
            r#"
                [runtime]
                sandbox = "workspcae-write"
            "#,
        )
        .expect_err("a typo must not remove containment");
        assert!(sandbox.to_string().contains("unknown sandbox"));

        let operator = parse(
            r#"
                [[roles]]
                name = "manager"
                description = "manager"
                loopback = true
                surface = "operator"
                system_prompt = "manage"
            "#,
        )
        .expect_err("a provider role cannot receive the human root surface");
        assert!(
            operator.to_string().contains("authenticated human HTTP"),
            "{operator}"
        );
    }

    #[test]
    fn provider_token_limits_parse_with_clear_total_token_units() {
        let config = parse(
            r#"
                [limits]
                daily_stop_usd = 50.0

                [limits.providers.codex]
                daily_warn_tokens = 2000000
                daily_stop_tokens = 4000000
            "#,
        )
        .expect("provider limits");
        let codex = config.limits.providers.get("codex").expect("codex");
        assert_eq!(codex.daily_warn_tokens, Some(2_000_000));
        assert_eq!(codex.daily_stop_tokens, Some(4_000_000));
    }

    #[test]
    fn inverted_or_zero_limits_fail_at_parse_time() {
        let inverted = parse(
            r#"
                [limits.providers.codex]
                daily_warn_tokens = 20
                daily_stop_tokens = 10
            "#,
        )
        .expect_err("inverted token thresholds");
        assert!(inverted.to_string().contains("must be <="), "{inverted}");

        let zero = parse(
            r#"
                [limits]
                daily_stop_usd = 0.0
            "#,
        )
        .expect_err("zero means omit the breaker, not a valid value");
        assert!(zero.to_string().contains("positive"), "{zero}");
    }

    #[test]
    fn persistent_role_arguments_render_on_the_agent_surface() {
        let config: Config = toml::from_str(
            r#"
                [[agents]]
                name = "repo-manager"
                role = "manager"
                provider = "codex"
                arguments = { checkout = "/tmp/repo" }
            "#,
        )
        .expect("config");
        let role: Role = toml::from_str(
            r#"
                name = "manager"
                description = "manager"
                working_dir = "{{checkout}}"
                loopback = true
                surface = "agent"
                arguments = ["checkout"]
                system_prompt = "Manage {{checkout}}"
            "#,
        )
        .expect("shipped role");
        let roles = Roles::with_runtime(vec![role], "agent.json", Default::default());

        let def =
            role_definition(&config, &config.agents[0], &roles, "agent.json").expect("definition");
        assert_eq!(def.name, "repo-manager");
        assert_eq!(def.provider.as_str(), "codex");
        assert_eq!(def.system_prompt, "Manage /tmp/repo");
        assert_eq!(
            def.working_dir.as_deref(),
            Some(std::path::Path::new("/tmp/repo"))
        );
        assert_eq!(def.mcp_config.as_deref(), Some("agent.json"));
    }

    #[test]
    fn persistent_shipped_operator_role_is_refused_before_agent_creation() {
        let config: Config = toml::from_str(
            r#"
                [[agents]]
                name = "repo-manager"
                role = "manager"
            "#,
        )
        .expect("config");
        let role: Role = toml::from_str(
            r#"
                name = "manager"
                description = "manager"
                loopback = true
                surface = "operator"
                system_prompt = "Manage"
            "#,
        )
        .expect("shipped role");
        let roles = Roles::new(vec![role], "agent.json");

        let error = role_definition(&config, &config.agents[0], &roles, "agent.json")
            .expect_err("provider-backed operator authority must not enter the ledger");
        assert!(error.to_string().contains("human-only"), "{error}");
    }

    #[test]
    fn persistent_role_missing_an_argument_fails_at_boot() {
        let config: Config = toml::from_str(
            r#"
                [[agents]]
                name = "repo-manager"
                role = "manager"
            "#,
        )
        .expect("config");
        let role: Role = toml::from_str(
            r#"
                name = "manager"
                description = "manager"
                arguments = ["checkout"]
                system_prompt = "Manage {{checkout}}"
            "#,
        )
        .expect("shipped role");
        let roles = Roles::new(vec![role], "agent.json");

        let error = role_definition(&config, &config.agents[0], &roles, "agent.json")
            .expect_err("an unresolved persistent role must not be created");
        assert!(error.to_string().contains("checkout"), "{error}");
    }
}
