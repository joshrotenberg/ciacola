//! Roles: agent definitions as config, exposed as MCP prompts.
//!
//! Until now a persistent agent was declared in TOML and an ephemeral
//! one had its system prompt written from scratch by whoever spawned
//! it. That asymmetry is the thing flat10's memory lessons were
//! compensating for: "for PR-summary tasks use haiku plus Bash plus
//! track" is provisioning knowledge that should be config the system
//! enforces, not advice a manager might recall.
//!
//! A role is one definition serving both: a system prompt template
//! with `{{placeholders}}`, a model, a tool bundle, budgets, and a
//! rotation policy. Persistent agents are roles instantiated at boot;
//! ephemeral agents are the same roles instantiated on demand by
//! `spawn_role`. Stage 5's rule, formalized: knowledge lives in the
//! role, the task is a thin template.
//!
//! Roles are exposed three ways, and the split is the same one this
//! workspace settled on for tools versus resources:
//!
//! - **MCP prompts** (`prompts/list`, `prompts/get`): inspect and
//!   render a role. No side effects; a REPL user sees exactly what an
//!   agent would get.
//! - **`spawn_role` tool**: act on one.
//! - **`roles` tool**: the machine-readable catalog, for agents
//!   choosing provisioning.

use std::collections::HashMap;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_mcp::context::RequestContext;
use tower_mcp::extract::{Context, Json, State};
use tower_mcp::{
    CallToolResult, GetPromptResult, Prompt, PromptArgument, PromptBuilder, Tool, ToolBuilder,
};

use crate::agent::{AgentDef, FlatError};
use crate::identity::{AgentIdentity, grant_child_tools_from_parent};
use crate::ledger::Ledger;
use crate::plugin::Surface;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub name: String,
    /// What this role is for. Shown in prompts/list, so write it for
    /// whoever is choosing: "read-only analyst for one GitHub issue".
    pub description: String,
    /// Backend key. Omit for the server-wide default.
    #[serde(default)]
    pub provider: Option<String>,
    pub model: Option<String>,
    /// low, medium, high, xhigh, max.
    pub effort: Option<String>,
    /// full, project, or none. Falls back to `[runtime] hermetic`.
    pub hermetic: Option<String>,
    /// Where an instance works, `{{placeholders}}` and all. A role
    /// pointed at a repository is what makes the git surface light up
    /// for its agents, and templating it is what lets one role cover
    /// every checkout instead of one role per repo.
    pub working_dir: Option<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Use the backend's native tool policy instead of `allowed_tools`.
    #[serde(default)]
    pub inherit_provider_tools: bool,
    /// read-only, workspace-write, workspace-write-no-network, or none.
    pub sandbox: Option<String>,
    pub max_turns: Option<u32>,
    /// Start a fresh provider session after this many turns. The
    /// agent's durable state lives in the kanban, memory, and
    /// findings, so rotation costs a preamble, not knowledge.
    pub rotate_after_turns: Option<u32>,
    /// Hand this role the server's own tools over loopback.
    #[serde(default)]
    pub loopback: bool,
    /// Which loopback surface, when `loopback` is set. `agent` is the only
    /// provider-backed surface currently supported. `operator` is retained
    /// in the config shape for migration clarity, but provisioning it is
    /// refused: the HTTP operator mount requires a human root bearer, and a
    /// bearer placed in one provider process can be copied by another process
    /// under the same OS user.
    #[serde(default)]
    pub surface: Option<String>,
    /// Placeholder names substituted into `system_prompt` as
    /// `{{name}}`. `agent_id` is always available.
    #[serde(default)]
    pub arguments: Vec<String>,
    pub system_prompt: String,
}

/// Server-wide defaults a role inherits unless it says otherwise.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runtime {
    /// Backend used by definitions that do not select one. Omit for Claude.
    pub default_provider: Option<String>,
    /// Applied to every agent that does not set its own.
    pub hermetic: Option<String>,
    /// Applied to every agent that does not set its own.
    pub sandbox: Option<String>,
    /// Provider config and session directory for every agent. Must be
    /// logged in separately.
    pub claude_home: Option<String>,
    /// Dedicated Codex config, auth, and session directory.
    pub codex_home: Option<String>,
    /// Standing rules prepended to every system prompt.
    pub house_rules: Option<String>,
    /// A file to read them from instead, typically the operator's own
    /// `CLAUDE.md`. Appended after any inline rules.
    pub house_rules_file: Option<String>,
    /// Additional ambient environment variables copied by exact name into
    /// the provider-child snapshot at startup.
    ///
    /// The provider-neutral baseline already covers path, home/user identity,
    /// temp directories, and locale. SSH agents, Git overrides, proxies,
    /// GitHub tokens, client bearers, Ciacola variables, and unrelated secrets
    /// are absent unless named here. Each adapter still removes its own auth,
    /// routing, and config selectors before applying the intended credential.
    #[serde(default)]
    pub provider_env_passthrough: Vec<String>,
    /// Deprecated startup-environment credential source retained only so old
    /// config receives a precise migration error instead of an unknown field.
    pub token_env: Option<String>,
    /// Deprecated Codex startup-environment credential source retained only
    /// for migration diagnostics.
    pub codex_token_env: Option<String>,
}

impl Runtime {
    /// Complain at boot about an isolated config directory that has no
    /// login, rather than letting the first turn fail on it. The check
    /// is a heuristic (the CLI may keep credentials in a keychain), so
    /// it warns and continues.
    pub fn check_provider_homes(
        &self,
        claude_descriptor_credential: bool,
        codex_descriptor_credential: bool,
    ) {
        if let Some(home) = &self.claude_home {
            let expanded = expand_home_path(home);
            let dir = std::path::Path::new(&expanded);
            if !claude_descriptor_credential && !dir.join(".credentials.json").exists() {
                eprintln!(
                    "[runtime] warning: claude_home {home} has no .credentials.json. \
                     CLAUDE_CONFIG_DIR isolates the login as well as the session data, \
                     so agents need a separately authenticated home or, on Unix, a \
                     CIACOLA_CLAUDE_TOKEN_FD startup credential."
                );
            }
        }

        if let Some(home) = &self.codex_home {
            let expanded = expand_home_path(home);
            let dir = std::path::Path::new(&expanded);
            if !codex_descriptor_credential && !dir.join("auth.json").exists() {
                eprintln!(
                    "[runtime] warning: codex_home {home} has no auth.json. CODEX_HOME \
                     isolates login state, so agents need a separately authenticated \
                     home or, on Unix, a CIACOLA_CODEX_TOKEN_FD startup credential."
                );
            }
        }
    }

    /// The configured default, preserving Claude when omitted.
    pub fn default_provider_key(&self) -> ciacola_agent::ProviderKey {
        self.default_provider
            .as_deref()
            .map(ciacola_agent::ProviderKey::new)
            .unwrap_or_else(ciacola_agent::ProviderKey::claude)
    }

    /// Inline rules plus the file, resolved once at boot so a missing
    /// or unreadable file is loud rather than silently empty.
    pub fn resolved_house_rules(&self) -> Result<Option<String>, FlatError> {
        let mut parts = Vec::new();
        if let Some(inline) = self.house_rules.as_ref().filter(|r| !r.trim().is_empty()) {
            parts.push(inline.trim().to_string());
        }
        if let Some(path) = &self.house_rules_file {
            let expanded = match path.strip_prefix("~/") {
                Some(rest) => match std::env::var("HOME") {
                    Ok(home) => format!("{home}/{rest}"),
                    Err(_) => path.clone(),
                },
                None => path.clone(),
            };
            let text = std::fs::read_to_string(&expanded)
                .map_err(|e| -> FlatError { format!("house_rules_file {expanded}: {e}").into() })?;
            if !text.trim().is_empty() {
                parts.push(text.trim().to_string());
            }
        }
        Ok((!parts.is_empty()).then(|| parts.join("\n\n")))
    }
}

fn expand_home_path(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME")
            .map(|home| format!("{home}/{rest}"))
            .unwrap_or_else(|_| path.to_string()),
        None => path.to_string(),
    }
}

#[derive(Clone)]
pub struct Roles {
    roles: Arc<Vec<Role>>,
    loopback_mcp_config: Arc<String>,
    /// The same server, mounted where the destructive tools live. Empty
    /// unless the binary supplies it, in which case a role asking for
    /// the operator surface falls back to the agent one rather than
    /// getting no tools at all.
    operator_mcp_config: Arc<String>,
    runtime: Arc<Runtime>,
    /// Resolved once, so every agent gets the same text and a broken
    /// path fails at boot rather than per spawn.
    house_rules: Arc<Option<String>>,
}

impl Roles {
    pub fn new(roles: Vec<Role>, loopback_mcp_config: impl Into<String>) -> Self {
        Self::with_runtime(roles, loopback_mcp_config, Runtime::default())
    }

    pub fn with_runtime(
        roles: Vec<Role>,
        loopback_mcp_config: impl Into<String>,
        runtime: Runtime,
    ) -> Self {
        let house_rules = runtime.resolved_house_rules().unwrap_or_else(|e| {
            eprintln!("[roles] {e}");
            None
        });
        let loopback = loopback_mcp_config.into();
        Self {
            roles: Arc::new(roles),
            // Defaults to the agent surface, so a role asking for the
            // operator one before the binary supplies its path gets the
            // safe surface rather than none.
            operator_mcp_config: Arc::new(loopback.clone()),
            loopback_mcp_config: Arc::new(loopback),
            runtime: Arc::new(runtime),
            house_rules: Arc::new(house_rules),
        }
    }

    /// Point roles asking for the operator surface at its own mount.
    ///
    /// The binary calls this once it knows the port. Until it does, such
    /// a role gets the agent surface, which is the safe direction.
    #[must_use]
    pub fn with_operator_mcp_config(mut self, path: impl Into<String>) -> Self {
        self.operator_mcp_config = Arc::new(path.into());
        self
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub fn all(&self) -> &[Role] {
        &self.roles
    }

    pub fn get(&self, name: &str) -> Option<&Role> {
        self.roles.iter().find(|r| r.name == name)
    }

    /// Substitute `{{placeholders}}`. Unknown placeholders are left
    /// alone rather than erroring: a role whose prompt mentions
    /// `{{agent_id}}` is filled in later by the caller that knows it.
    pub fn render(&self, role: &Role, args: &HashMap<String, String>) -> String {
        let mut out = role.system_prompt.clone();
        for (key, value) in args {
            out = out.replace(&format!("{{{{{key}}}}}"), value);
        }
        out
    }

    /// A role plus arguments becomes a definition ready to create.
    pub fn to_def(&self, role: &Role, args: &HashMap<String, String>) -> AgentDef {
        let mut def =
            AgentDef::new(&role.name, self.render(role, args)).with_catalog_role(role.name.clone());
        if let Some(provider) = &role.provider {
            def = def.provider(provider.as_str());
        }
        if let Some(model) = &role.model {
            def = def.model(model);
        }
        if let Some(effort) = &role.effort {
            def = def.effort(effort);
        }
        if let Some(dir) = &role.working_dir {
            // Templated like the system prompt, so one role serves many
            // repositories: working_dir = "/Code/{{repo}}" with repo as
            // an argument beats a role per checkout.
            let mut dir = dir.clone();
            for (key, value) in args {
                dir = dir.replace(&format!("{{{{{key}}}}}"), value);
            }
            def = def.working_dir(dir);
        }
        if !role.allowed_tools.is_empty() {
            def = def.allowed_tools(role.allowed_tools.clone());
        }
        if role.inherit_provider_tools {
            def = def.inherit_provider_tools();
        }
        if let Some(sandbox) = role.sandbox.as_ref().or(self.runtime.sandbox.as_ref()) {
            def = def.sandbox(sandbox);
        }
        if let Some(max_turns) = role.max_turns {
            def = def.max_turns(max_turns);
        }
        if let Some(rotate) = role.rotate_after_turns {
            def = def.rotate_after_turns(rotate);
        }
        if role.loopback {
            let config = match role.surface.as_deref() {
                Some("operator") => self.operator_mcp_config.as_str(),
                Some("agent") | None => self.loopback_mcp_config.as_str(),
                Some(other) => {
                    // A typo grants less rather than more, which is the
                    // right failure, but silently is not: say so.
                    eprintln!("[roles] unknown surface '{other}', using the agent surface");
                    self.loopback_mcp_config.as_str()
                }
            };
            def = def.mcp_config(config);
        }
        if let Some(scope) = role.hermetic.as_ref().or(self.runtime.hermetic.as_ref()) {
            def = def.hermetic(scope);
        }
        let provider = role
            .provider
            .as_deref()
            .or(self.runtime.default_provider.as_deref())
            .unwrap_or(ciacola_agent::ProviderKey::CLAUDE);
        let home = if provider == "codex" {
            &self.runtime.codex_home
        } else {
            &self.runtime.claude_home
        };
        if let Some(home) = home {
            def = def.config_home(home);
        }
        if let Some(rules) = self.house_rules.as_ref() {
            def = def.house_rules(rules.as_str());
        }
        def
    }
}

fn role_json(role: &Role, default_provider: &str) -> serde_json::Value {
    json!({
        "name": role.name,
        "description": role.description,
        "provider": role.provider.as_deref().unwrap_or(default_provider),
        "model": role.model,
        "effort": role.effort,
        "allowed_tools": role.allowed_tools,
        "inherit_provider_tools": role.inherit_provider_tools,
        "sandbox": role.sandbox,
        "max_turns": role.max_turns,
        "rotate_after_turns": role.rotate_after_turns,
        "arguments": role.arguments,
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SpawnRoleArgs {
    /// Role name, from the roles tool or prompts/list.
    role: String,
    /// Name for this instance, e.g. "spoke-issue-1204". Defaults to
    /// the role name.
    name: Option<String>,
    /// Values for the role's declared arguments.
    #[serde(default)]
    arguments: HashMap<String, String>,
    /// Override the role's model for this instance. Check model_stats
    /// first: the role's default is a starting guess, not a verdict.
    model: Option<String>,
    /// Override the role's effort for this instance.
    effort: Option<String>,
    /// Deprecated compatibility field. Parentage is derived from the
    /// authenticated request context and this value is ignored.
    #[serde(rename = "spawned_by")]
    _spawned_by: Option<String>,
}

/// The authority core derived for a role creation request.
///
/// Creation paths persist this value verbatim. It never comes from caller
/// prose: an authenticated request is always attributed to that agent, an
/// anonymous agent-surface request is refused, and an interactive operator
/// creates a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSpawnAuthorization {
    pub spawned_by: Option<String>,
}

/// Stable policy categories returned by every role-backed creation path.
///
/// The variant is suitable for programmatic comparison while
/// [`std::fmt::Display`]
/// supplies the same actionable text to `spawn_role`, `start_issue`, and any
/// future role consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleSpawnRefusal {
    UnauthenticatedAgentSurface,
    OperatorSurface { role: String },
    ProviderNativeTools { role: String },
    ChildTools { role: String, denied: Vec<String> },
    Depth { depth: i64, max_depth: i64 },
    CallerNotFound { agent_id: String },
    Ledger { reason: String },
}

impl std::fmt::Display for RoleSpawnRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnauthenticatedAgentSurface => write!(
                f,
                "role spawning on the agent HTTP surface requires an authenticated agent"
            ),
            Self::OperatorSurface { role } => write!(
                f,
                "role '{role}' carries the operator surface, which provider-backed agents cannot hold; use stdio or authenticated human HTTP"
            ),
            Self::ProviderNativeTools { role } => write!(
                f,
                "role '{role}' inherits its provider's native tool policy, which an agent cannot bound; ask the operator to create it"
            ),
            Self::ChildTools { role, denied } => write!(
                f,
                "role '{role}' needs tools its parent does not hold: {}",
                denied.join(", ")
            ),
            Self::Depth { depth, max_depth } => write!(
                f,
                "refused: spawning here would be depth {depth}, past the limit of {max_depth}. Do the work yourself or ask the operator to raise max_spawn_depth."
            ),
            Self::CallerNotFound { agent_id } => write!(f, "caller '{agent_id}' not found"),
            Self::Ledger { reason } => {
                write!(
                    f,
                    "could not authorize role spawn against the ledger: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for RoleSpawnRefusal {}

/// Authorize one role-backed agent creation before any side effect.
///
/// The request context is deliberately accepted instead of a caller id so
/// plugins cannot accidentally trust an argument supplied by an agent. This
/// is the convergence point for role surface, provider policy, named tool
/// grants, lineage, and spawn depth.
pub async fn preflight_role_spawn(
    ledger: &Ledger,
    role: &Role,
    request: &RequestContext,
    surface: Surface,
    max_depth: i64,
) -> Result<RoleSpawnAuthorization, RoleSpawnRefusal> {
    let caller = request.extension::<AgentIdentity>().map(|i| i.0.clone());
    let spawned_by = caller.clone();

    // An agent can deliberately omit its identity header when making an
    // arbitrary HTTP request. Treating that request as a new root would let
    // it bypass both its named-tool ceiling and max_spawn_depth. Humans have
    // stdio and the separately authenticated operator HTTP mount.
    if caller.is_none() && surface == Surface::Agent {
        return Err(RoleSpawnRefusal::UnauthenticatedAgentSurface);
    }

    if role.surface.as_deref() == Some("operator") {
        return Err(RoleSpawnRefusal::OperatorSurface {
            role: role.name.clone(),
        });
    }
    if role.inherit_provider_tools && (caller.is_some() || surface != Surface::Operator) {
        return Err(RoleSpawnRefusal::ProviderNativeTools {
            role: role.name.clone(),
        });
    }

    let parent = match caller.as_deref() {
        Some(agent_id) => match ledger.get_agent(agent_id).await {
            Ok(Some(parent)) => Some(parent),
            Ok(None) => {
                return Err(RoleSpawnRefusal::CallerNotFound {
                    agent_id: agent_id.to_string(),
                });
            }
            Err(error) => {
                return Err(RoleSpawnRefusal::Ledger {
                    reason: error.to_string(),
                });
            }
        },
        None => None,
    };

    let grant = grant_child_tools_from_parent(parent.as_ref(), role.allowed_tools.clone());
    if !grant.denied.is_empty() {
        return Err(RoleSpawnRefusal::ChildTools {
            role: role.name.clone(),
            denied: grant.denied,
        });
    }

    if max_depth > 0
        && let Some(parent) = spawned_by.as_deref()
    {
        let depth = ledger
            .spawn_depth(parent)
            .await
            .map_err(|error| RoleSpawnRefusal::Ledger {
                reason: error.to_string(),
            })?
            + 1;
        if depth > max_depth {
            return Err(RoleSpawnRefusal::Depth { depth, max_depth });
        }
    }

    Ok(RoleSpawnAuthorization { spawned_by })
}

/// `spawn_role` and `roles`. Both surfaces: an orchestrator provisions
/// from the catalog rather than inventing a system prompt.
pub fn tools(roles: Roles, ledger: Ledger) -> Vec<Tool> {
    tools_with_depth(roles, ledger, crate::limits::DEFAULT_MAX_SPAWN_DEPTH, true)
}

pub fn tools_with_depth(
    roles: Roles,
    ledger: Ledger,
    max_depth: i64,
    operator_surface: bool,
) -> Vec<Tool> {
    let surface = if operator_surface {
        Surface::Operator
    } else {
        Surface::Agent
    };
    let spawn_role = {
        let roles = roles.clone();
        ToolBuilder::new("spawn_role")
            .description(
                "Create an agent from a configured role, filling in its \
                 arguments. Prefer this over spawn: roles carry the \
                 provisioning (model, tools, budgets) that a task shape \
                 is known to need.",
            )
            .non_destructive()
            .extractor_handler(
                (roles.clone(), ledger.clone()),
                move |State((roles, ledger)): State<(Roles, Ledger)>,
                      ctx: Context,
                      Json(args): Json<SpawnRoleArgs>| async move {
                    let Some(role) = roles.get(&args.role) else {
                        return Ok(CallToolResult::error(format!(
                            "no role '{}'; see the roles tool",
                            args.role
                        )));
                    };
                    let authorization = match preflight_role_spawn(
                        &ledger,
                        role,
                        &ctx,
                        surface,
                        max_depth,
                    )
                    .await
                    {
                        Ok(authorization) => authorization,
                        Err(refusal) => {
                            return Ok(CallToolResult::error(refusal.to_string()));
                        }
                    };
                    let missing: Vec<&String> = role
                        .arguments
                        .iter()
                        .filter(|a| !args.arguments.contains_key(*a))
                        .collect();
                    if !missing.is_empty() {
                        return Ok(CallToolResult::error(format!(
                            "role '{}' needs arguments {missing:?}",
                            role.name
                        )));
                    }
                    let mut def = roles.to_def(role, &args.arguments);
                    if let Some(name) = args.name {
                        def.name = name;
                    }
                    if let Some(model) = args.model {
                        def.model = Some(model);
                    }
                    if let Some(effort) = args.effort {
                        def.effort = Some(effort);
                    }
                    match ledger
                        .create_agent(&def, authorization.spawned_by.as_deref())
                        .await
                    {
                        Ok(agent_id) => {
                            // The role's prompt may mention its own id;
                            // fill it in now that one exists.
                            def.system_prompt =
                                def.system_prompt.replace("{{agent_id}}", &agent_id);
                            if let Err(e) = ledger.update_agent_def(&agent_id, &def).await {
                                return Ok(CallToolResult::error(format!(
                                    "created agent '{agent_id}' but could not finish its definition: {e}"
                                )));
                            }
                            Ok(CallToolResult::json(json!({
                                "agent_id": agent_id,
                                "role": role.name,
                            })))
                        }
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                },
            )
            .build()
    };

    let list = {
        let roles = roles.clone();
        ToolBuilder::new("roles")
            .description("Configured roles: what each is for and what it needs.")
            .read_only()
            .no_params_handler(move || {
                let roles = roles.clone();
                async move {
                    let default_provider = roles.runtime.default_provider_key();
                    Ok(CallToolResult::json(json!({
                        "roles": roles
                            .all()
                            .iter()
                            .map(|role| role_json(role, default_provider.as_str()))
                            .collect::<Vec<_>>()
                    })))
                }
            })
            .build()
    };

    vec![spawn_role, list]
}

/// One MCP prompt per role: the inspectable, renderable half. A client
/// lists them like slash commands and can see exactly what an agent in
/// that role would be told.
pub fn prompts(roles: Roles) -> Vec<Prompt> {
    roles
        .all()
        .iter()
        .map(|role| {
            let mut builder = PromptBuilder::new(format!("role/{}", role.name))
                .description(role.description.clone());
            for argument in &role.arguments {
                builder = builder.argument(PromptArgument {
                    name: argument.clone(),
                    description: Some(format!("Value for {{{{{argument}}}}}")),
                    required: true,
                });
            }
            let roles = roles.clone();
            let name = role.name.clone();
            builder
                .handler(move |args: HashMap<String, String>| {
                    let roles = roles.clone();
                    let name = name.clone();
                    async move {
                        let rendered = roles
                            .get(&name)
                            .map(|role| roles.render(role, &args))
                            .unwrap_or_else(|| format!("no role '{name}'"));
                        Ok(GetPromptResult::builder()
                            .description(format!("system prompt for role {name}"))
                            .user(rendered)
                            .build())
                    }
                })
                .build()
        })
        .collect()
}

// --- plugin ---

use crate::plugin::{BoxFut, Plugin, PluginContext};

/// Roles as a plugin. Stateless: the config file is the truth, so
/// `setup` only builds the loopback path into the catalog. Contributes
/// prompts, which no other plugin does yet.
pub struct RolesPlugin {
    roles: Option<Roles>,
    ledger: Option<Ledger>,
    max_depth: i64,
}

impl RolesPlugin {
    pub fn new() -> Self {
        Self {
            roles: None,
            ledger: None,
            max_depth: crate::limits::DEFAULT_MAX_SPAWN_DEPTH,
        }
    }
}

impl Default for RolesPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for RolesPlugin {
    fn name(&self) -> &'static str {
        "roles"
    }

    fn setup<'a>(&'a mut self, ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async move {
            self.roles = Some(ctx.roles.clone());
            self.ledger = Some(ctx.ledger.clone());
            self.max_depth = ctx.limits.max_spawn_depth;
            Ok(())
        })
    }

    fn tools(&self, surface: Surface) -> Vec<Tool> {
        match (&self.roles, &self.ledger) {
            (Some(roles), Some(ledger)) => tools_with_depth(
                roles.clone(),
                ledger.clone(),
                self.max_depth,
                surface == Surface::Operator,
            ),
            _ => Vec::new(),
        }
    }

    fn prompts(&self) -> Vec<Prompt> {
        self.roles
            .as_ref()
            .map(|r| prompts(r.clone()))
            .unwrap_or_default()
    }

    fn health(&self) -> BoxFut<'_, serde_json::Value> {
        Box::pin(async move {
            json!({ "roles": self.roles.as_ref().map_or(0, |roles| roles.all().len()) })
        })
    }
}

#[cfg(test)]
mod surface_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tower_mcp::context::{Extensions, RequestContext};
    use tower_mcp::protocol::RequestId;

    fn role(surface: Option<&str>) -> Role {
        Role {
            name: "r".into(),
            description: "d".into(),
            provider: None,
            model: None,
            effort: None,
            hermetic: None,
            working_dir: None,
            allowed_tools: Vec::new(),
            inherit_provider_tools: false,
            sandbox: None,
            max_turns: None,
            rotate_after_turns: None,
            loopback: true,
            surface: surface.map(Into::into),
            arguments: Vec::new(),
            system_prompt: "s".into(),
        }
    }

    async fn ledger() -> Ledger {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        Ledger::setup(pool).await.expect("ledger")
    }

    fn context(agent_id: Option<&str>) -> RequestContext {
        let mut extensions = Extensions::new();
        if let Some(agent_id) = agent_id {
            extensions.insert(AgentIdentity(agent_id.to_string()));
        }
        RequestContext::new(RequestId::Number(1)).with_extensions(Arc::new(extensions))
    }

    /// Which mount a role is pointed at is the whole of its authority,
    /// so the routing is worth pinning tightly.
    #[test]
    fn operator_surface_gets_the_operator_config() {
        let roles = Roles::new(vec![role(Some("operator"))], "agent.json")
            .with_operator_mcp_config("operator.json");
        let def = roles.to_def(roles.get("r").unwrap(), &HashMap::new());
        assert_eq!(def.catalog_role(), Some("r"));
        assert_eq!(def.mcp_config.as_deref(), Some("operator.json"));
    }

    #[test]
    fn default_surface_gets_the_agent_config() {
        let roles =
            Roles::new(vec![role(None)], "agent.json").with_operator_mcp_config("operator.json");
        let def = roles.to_def(roles.get("r").unwrap(), &HashMap::new());
        assert_eq!(def.mcp_config.as_deref(), Some("agent.json"));

        let roles = Roles::new(vec![role(Some("agent"))], "agent.json")
            .with_operator_mcp_config("operator.json");
        let def = roles.to_def(roles.get("r").unwrap(), &HashMap::new());
        assert_eq!(def.mcp_config.as_deref(), Some("agent.json"));
    }

    #[test]
    fn a_role_selects_its_provider_without_changing_the_default() {
        let mut selected = role(None);
        selected.provider = Some("codex".into());
        let roles = Roles::new(vec![selected], "agent.json");
        let def = roles.to_def(roles.get("r").unwrap(), &HashMap::new());
        assert_eq!(def.provider.as_str(), "codex");

        let defaults = Roles::new(vec![role(None)], "agent.json");
        let def = defaults.to_def(defaults.get("r").unwrap(), &HashMap::new());
        assert_eq!(def.provider, ciacola_agent::ProviderKey::claude());
    }

    #[test]
    fn role_catalog_reports_the_runtime_default_provider() {
        let value = role_json(&role(None), "codex");
        assert_eq!(value["provider"], "codex");
    }

    /// Before the binary supplies the operator path, asking for it gets
    /// the agent surface rather than nothing: fail toward less.
    #[test]
    fn operator_surface_without_a_supplied_path_falls_back() {
        let roles = Roles::new(vec![role(Some("operator"))], "agent.json");
        let def = roles.to_def(roles.get("r").unwrap(), &HashMap::new());
        assert_eq!(def.mcp_config.as_deref(), Some("agent.json"));
    }

    /// A typo grants less, never more, and warns on stderr.
    #[test]
    fn unknown_surface_falls_back_to_the_agent_config() {
        let roles = Roles::new(vec![role(Some("opreator"))], "agent.json")
            .with_operator_mcp_config("operator.json");
        let def = roles.to_def(roles.get("r").unwrap(), &HashMap::new());
        assert_eq!(def.mcp_config.as_deref(), Some("agent.json"));
    }

    #[tokio::test]
    async fn preflight_derives_parentage_from_the_trusted_context_and_surface() {
        let ledger = ledger().await;
        let parent = ledger
            .create_agent(&AgentDef::new("parent", "s").allowed_tools(["Read"]), None)
            .await
            .expect("parent");
        let requested = role(None);

        let authenticated = preflight_role_spawn(
            &ledger,
            &requested,
            &context(Some(&parent)),
            Surface::Agent,
            3,
        )
        .await
        .expect("authenticated parent");
        assert_eq!(authenticated.spawned_by.as_deref(), Some(parent.as_str()));

        let anonymous =
            preflight_role_spawn(&ledger, &requested, &context(None), Surface::Agent, 3).await;
        assert_eq!(
            anonymous,
            Err(RoleSpawnRefusal::UnauthenticatedAgentSurface)
        );

        let operator =
            preflight_role_spawn(&ledger, &requested, &context(None), Surface::Operator, 3)
                .await
                .expect("interactive operator");
        assert_eq!(operator.spawned_by, None);
    }

    #[tokio::test]
    async fn operator_claimed_parent_is_ignored_by_spawn_role() {
        let ledger = ledger().await;
        let claimed = ledger
            .create_agent(&AgentDef::new("claimed parent", "s"), None)
            .await
            .expect("claimed parent");
        let roles = Roles::new(vec![role(None)], "agent.json");
        let spawn = tools_with_depth(roles, ledger.clone(), 3, true)
            .into_iter()
            .find(|tool| tool.definition().name == "spawn_role")
            .expect("spawn_role");

        let out = spawn
            .call(json!({
                "role": "r",
                "name": "named-instance",
                "arguments": {},
                "spawned_by": claimed,
            }))
            .await;
        let rendered = serde_json::to_string(&out).expect("render");
        assert!(rendered.contains("\"agent_id\""), "got: {rendered}");

        let created = ledger
            .list_agents()
            .await
            .expect("agents")
            .into_iter()
            .find(|agent| agent.name == "named-instance")
            .expect("created role agent");
        assert_eq!(created.spawned_by, None);
        assert_eq!(created.def.catalog_role(), Some("r"));
    }

    #[tokio::test]
    async fn preflight_refuses_an_operator_surface_role_for_every_caller() {
        let ledger = ledger().await;
        let requested = role(Some("operator"));
        let caller = ledger
            .create_agent(&AgentDef::new("caller", "s"), None)
            .await
            .expect("caller");

        assert_eq!(
            preflight_role_spawn(
                &ledger,
                &requested,
                &context(Some(&caller)),
                Surface::Agent,
                3,
            )
            .await,
            Err(RoleSpawnRefusal::OperatorSurface { role: "r".into() })
        );
        assert_eq!(
            preflight_role_spawn(&ledger, &requested, &context(None), Surface::Operator, 3,).await,
            Err(RoleSpawnRefusal::OperatorSurface { role: "r".into() })
        );
    }

    #[tokio::test]
    async fn only_an_operator_may_choose_provider_native_inheritance() {
        let ledger = ledger().await;
        let mut requested = role(None);
        requested.inherit_provider_tools = true;

        assert!(
            preflight_role_spawn(&ledger, &requested, &context(None), Surface::Operator, 3,)
                .await
                .is_ok()
        );
        assert_eq!(
            preflight_role_spawn(&ledger, &requested, &context(None), Surface::Agent, 3).await,
            Err(RoleSpawnRefusal::UnauthenticatedAgentSurface)
        );

        let parent = ledger
            .create_agent(&AgentDef::new("parent", "s"), None)
            .await
            .expect("parent");
        assert_eq!(
            preflight_role_spawn(
                &ledger,
                &requested,
                &context(Some(&parent)),
                Surface::Agent,
                3,
            )
            .await,
            Err(RoleSpawnRefusal::ProviderNativeTools { role: "r".into() })
        );
    }

    #[tokio::test]
    async fn preflight_enforces_named_tool_grants() {
        let ledger = ledger().await;
        let parent = ledger
            .create_agent(&AgentDef::new("parent", "s").allowed_tools(["Read"]), None)
            .await
            .expect("parent");
        let mut requested = role(None);
        requested.allowed_tools = vec!["Read".into(), "Edit".into()];

        assert_eq!(
            preflight_role_spawn(
                &ledger,
                &requested,
                &context(Some(&parent)),
                Surface::Agent,
                3,
            )
            .await,
            Err(RoleSpawnRefusal::ChildTools {
                role: "r".into(),
                denied: vec!["Edit".into()],
            })
        );

        requested.allowed_tools.pop();
        assert!(
            preflight_role_spawn(
                &ledger,
                &requested,
                &context(Some(&parent)),
                Surface::Agent,
                3,
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn preflight_allows_the_depth_boundary_and_zero_is_unlimited() {
        let ledger = ledger().await;
        let root = ledger
            .create_agent(&AgentDef::new("root", "s"), None)
            .await
            .expect("root");
        let at_one = ledger
            .create_agent(&AgentDef::new("one", "s"), Some(&root))
            .await
            .expect("one");
        let at_two = ledger
            .create_agent(&AgentDef::new("two", "s"), Some(&at_one))
            .await
            .expect("two");
        let requested = role(None);

        assert!(
            preflight_role_spawn(
                &ledger,
                &requested,
                &context(Some(&at_one)),
                Surface::Agent,
                2,
            )
            .await
            .is_ok(),
            "a child at exactly the limit is allowed"
        );
        assert_eq!(
            preflight_role_spawn(
                &ledger,
                &requested,
                &context(Some(&at_two)),
                Surface::Agent,
                2,
            )
            .await,
            Err(RoleSpawnRefusal::Depth {
                depth: 3,
                max_depth: 2,
            })
        );
        assert!(
            preflight_role_spawn(
                &ledger,
                &requested,
                &context(Some(&at_two)),
                Surface::Agent,
                0,
            )
            .await
            .is_ok(),
            "zero disables the ceiling"
        );
    }

    #[tokio::test]
    async fn preflight_types_missing_callers_and_ledger_failures() {
        let ledger = ledger().await;
        let requested = role(None);
        assert_eq!(
            preflight_role_spawn(
                &ledger,
                &requested,
                &context(Some("missing")),
                Surface::Agent,
                3,
            )
            .await,
            Err(RoleSpawnRefusal::CallerNotFound {
                agent_id: "missing".into(),
            })
        );

        ledger.pool().close().await;
        assert!(matches!(
            preflight_role_spawn(
                &ledger,
                &requested,
                &context(Some("claimed")),
                Surface::Agent,
                3,
            )
            .await,
            Err(RoleSpawnRefusal::Ledger { .. })
        ));
    }
}
