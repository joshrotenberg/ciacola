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
use tower_mcp::{
    CallToolResult, GetPromptResult, Prompt, PromptArgument, PromptBuilder, Tool, ToolBuilder,
};

use crate::agent::{AgentDef, FlatError};
use crate::ledger::Ledger;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub name: String,
    /// What this role is for. Shown in prompts/list, so write it for
    /// whoever is choosing: "read-only analyst for one GitHub issue".
    pub description: String,
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
    pub max_turns: Option<u32>,
    /// Start a fresh provider session after this many turns. The
    /// agent's durable state lives in the kanban, memory, and
    /// findings, so rotation costs a preamble, not knowledge.
    pub rotate_after_turns: Option<u32>,
    /// Hand this role the server's own tools over loopback.
    #[serde(default)]
    pub loopback: bool,
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
    /// Applied to every agent that does not set its own.
    pub hermetic: Option<String>,
    /// Provider config and session directory for every agent. Must be
    /// logged in separately; see `AgentDef::claude_home`.
    pub claude_home: Option<String>,
    /// Standing rules prepended to every system prompt.
    pub house_rules: Option<String>,
    /// A file to read them from instead, typically the operator's own
    /// `CLAUDE.md`. Appended after any inline rules.
    pub house_rules_file: Option<String>,
    /// Environment variable holding a long-lived provider token, for
    /// use with an isolated `claude_home`. Mint one with
    /// `claude setup-token`. The name, never the value.
    pub token_env: Option<String>,
}

impl Runtime {
    /// Complain at boot about an isolated config directory that has no
    /// login, rather than letting the first turn fail on it. The check
    /// is a heuristic (the CLI may keep credentials in a keychain), so
    /// it warns and continues.
    pub fn check_claude_home(&self) {
        let Some(home) = &self.claude_home else {
            return;
        };
        let dir = std::path::Path::new(home);
        if self.token_env.is_some() {
            return;
        }
        if dir.exists() && !dir.join(".credentials.json").exists() {
            eprintln!(
                "[runtime] warning: claude_home {home} has no .credentials.json. \
                 CLAUDE_CONFIG_DIR isolates the login as well as the session data, \
                 so agents may fail with \"Not logged in\". Either mint a token \
                 with `claude setup-token` and point [runtime] token_env at the \
                 variable holding it, or unset claude_home."
            );
        }
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

#[derive(Clone)]
pub struct Roles {
    roles: Arc<Vec<Role>>,
    loopback_mcp_config: Arc<String>,
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
        Self {
            roles: Arc::new(roles),
            loopback_mcp_config: Arc::new(loopback_mcp_config.into()),
            runtime: Arc::new(runtime),
            house_rules: Arc::new(house_rules),
        }
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
        let mut def = AgentDef::new(&role.name, self.render(role, args));
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
        if let Some(max_turns) = role.max_turns {
            def = def.max_turns(max_turns);
        }
        if let Some(rotate) = role.rotate_after_turns {
            def = def.rotate_after_turns(rotate);
        }
        if role.loopback {
            def = def.mcp_config(self.loopback_mcp_config.as_str());
        }
        if let Some(scope) = role.hermetic.as_ref().or(self.runtime.hermetic.as_ref()) {
            def = def.hermetic(scope);
        }
        if let Some(home) = &self.runtime.claude_home {
            def = def.claude_home(home);
        }
        if let Some(rules) = self.house_rules.as_ref() {
            def = def.house_rules(rules.as_str());
        }
        def
    }
}

fn role_json(role: &Role) -> serde_json::Value {
    json!({
        "name": role.name,
        "description": role.description,
        "model": role.model,
        "effort": role.effort,
        "allowed_tools": role.allowed_tools,
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
    /// Your own agent_id, if you are an agent spawning a helper.
    spawned_by: Option<String>,
}

/// `spawn_role` and `roles`. Both surfaces: an orchestrator provisions
/// from the catalog rather than inventing a system prompt.
pub fn tools(roles: Roles, ledger: Ledger) -> Vec<Tool> {
    tools_with_depth(roles, ledger, crate::limits::DEFAULT_MAX_SPAWN_DEPTH)
}

pub fn tools_with_depth(roles: Roles, ledger: Ledger, max_depth: i64) -> Vec<Tool> {
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
            .handler(move |args: SpawnRoleArgs| {
                let roles = roles.clone();
                let ledger = ledger.clone();
                async move {
                    let Some(role) = roles.get(&args.role) else {
                        return Ok(CallToolResult::error(format!(
                            "no role '{}'; see the roles tool",
                            args.role
                        )));
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
                    if let Some(reason) =
                        crate::server::depth_refusal(&ledger, args.spawned_by.as_deref(), max_depth)
                            .await
                    {
                        return Ok(CallToolResult::error(reason));
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
                    match ledger.create_agent(&def, args.spawned_by.as_deref()).await {
                        Ok(agent_id) => {
                            // The role's prompt may mention its own id;
                            // fill it in now that one exists.
                            def.system_prompt =
                                def.system_prompt.replace("{{agent_id}}", &agent_id);
                            let _ = ledger.update_agent_def(&agent_id, &def).await;
                            Ok(CallToolResult::json(json!({
                                "agent_id": agent_id,
                                "role": role.name,
                            })))
                        }
                        Err(e) => Ok(CallToolResult::error(e.to_string())),
                    }
                }
            })
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
                    Ok(CallToolResult::json(json!({
                        "roles": roles.all().iter().map(role_json).collect::<Vec<_>>()
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

use crate::plugin::{BoxFut, Plugin, PluginContext, Surface};

/// Roles as a plugin. Stateless: the config file is the truth, so
/// `setup` only builds the loopback path into the catalog. Contributes
/// prompts, which no other plugin does yet.
pub struct RolesPlugin {
    declared: Vec<Role>,
    roles: Option<Roles>,
    ledger: Option<Ledger>,
    max_depth: i64,
}

impl RolesPlugin {
    pub fn new(declared: Vec<Role>) -> Self {
        Self {
            declared,
            roles: None,
            ledger: None,
            max_depth: crate::limits::DEFAULT_MAX_SPAWN_DEPTH,
        }
    }
}

impl Plugin for RolesPlugin {
    fn name(&self) -> &'static str {
        "roles"
    }

    fn setup<'a>(&'a mut self, ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async move {
            self.roles = Some(Roles::new(
                self.declared.clone(),
                ctx.loopback_mcp_config.clone(),
            ));
            self.ledger = Some(ctx.ledger.clone());
            self.max_depth = ctx.limits.max_spawn_depth;
            Ok(())
        })
    }

    fn tools(&self, _surface: Surface) -> Vec<Tool> {
        match (&self.roles, &self.ledger) {
            (Some(roles), Some(ledger)) => {
                tools_with_depth(roles.clone(), ledger.clone(), self.max_depth)
            }
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
        Box::pin(async move { json!({ "roles": self.declared.len() }) })
    }
}
