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

use serde::Deserialize;

use ciacola_core::agent::{AgentDef, FlatError};
use ciacola_core::ledger::Ledger;
use ciacola_core::roles::{Role, Roles};
use ciacola_schedule::Schedules;

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigAgent {
    pub name: String,
    /// Instantiate this role instead of spelling out a definition. The
    /// fields below still override what the role provides.
    pub role: Option<String>,
    #[serde(default)]
    pub system_prompt: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub hermetic: Option<String>,
    pub working_dir: Option<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    pub max_turns: Option<u32>,
    /// Start a fresh provider session after this many turns.
    pub rotate_after_turns: Option<u32>,
    /// Hand this agent the server's own tools over loopback HTTP.
    #[serde(default)]
    pub loopback: bool,
    pub schedule: Option<ConfigSchedule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSchedule {
    pub every_secs: i64,
    pub text: String,
}

fn empty_table() -> toml::Value {
    toml::Value::Table(toml::map::Map::new())
}

pub fn load(path: &str) -> Result<Config, FlatError> {
    Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
}

/// Upsert every declared agent and its schedule. Boot-idempotent:
/// running twice changes nothing; editing the file and rebooting
/// updates definitions in place without touching conversations.
pub async fn apply(
    config: &Config,
    ledger: &Ledger,
    schedules: &Schedules,
    loopback_mcp_config: &str,
) -> Result<Vec<String>, FlatError> {
    let house_rules = config.runtime.resolved_house_rules()?;
    let roles = Roles::with_runtime(
        config.roles.clone(),
        loopback_mcp_config,
        config.runtime.clone(),
    );
    let mut report = Vec::new();
    for declared in &config.agents {
        // A persistent agent is either spelled out or an instance of a
        // role; the same definitions serve both, which is the whole
        // point of roles.
        let mut def = match &declared.role {
            Some(role) => {
                let role = roles
                    .get(role)
                    .ok_or_else(|| format!("agent '{}': no role '{role}'", declared.name))?;
                let mut def = roles.to_def(role, &std::collections::HashMap::new());
                def.name = declared.name.clone();
                def
            }
            None => AgentDef::new(&declared.name, &declared.system_prompt),
        };
        if let Some(model) = &declared.model {
            def = def.model(model);
        }
        if let Some(effort) = &declared.effort {
            def = def.effort(effort);
        }
        if let Some(dir) = &declared.working_dir {
            def = def.working_dir(dir);
        }
        if !declared.allowed_tools.is_empty() {
            def = def.allowed_tools(declared.allowed_tools.clone());
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
        if let Some(home) = &config.runtime.claude_home {
            def = def.claude_home(home);
        }
        if let Some(rules) = &house_rules {
            def = def.house_rules(rules.as_str());
        }

        let (agent_id, verb) = match ledger.find_active_by_name(&declared.name).await? {
            Some(existing) => (existing.agent_id, "updated"),
            None => (ledger.create_agent(&def, None).await?, "created"),
        };
        def.system_prompt = def.system_prompt.replace("{agent_id}", &agent_id);
        ledger.update_agent_def(&agent_id, &def).await?;

        // One schedule per config agent: replace whatever is there so
        // the file is the truth for the wake as well as the def.
        for schedule in schedules.list().await? {
            if schedule.agent_id == agent_id {
                schedules.delete(&schedule.schedule_id).await?;
            }
        }
        if let Some(wake) = &declared.schedule {
            schedules
                .create(&agent_id, &wake.text, wake.every_secs)
                .await?;
        }

        report.push(format!(
            "{verb} {} ({agent_id}){}",
            declared.name,
            declared
                .schedule
                .as_ref()
                .map(|w| format!(", wakes every {}s", w.every_secs))
                .unwrap_or_default()
        ));
    }
    Ok(report)
}
