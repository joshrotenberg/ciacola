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
//! is drawn at the security line and nowhere else.
//!
//! # Cancellation and drop safety
//!
//! [`ClaudeProvider::run`] holds no buffering of its own between the
//! caller and the spawned process: it awaits
//! `QueryCommand::execute_json` directly, and every spawn in
//! `claude-wrapper` sets `kill_on_drop(true)` and places the child in
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

use std::path::Path;
use std::time::Instant;

use ciacola_agent::{
    AgentError, BoxFut, Capabilities, Provider, ProviderKey, TurnEvents, TurnIntent, TurnOutcome,
};
use claude_wrapper::Claude;

mod command;
mod outcome;

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
        Box::pin(async move { run_turn(intent, events).await })
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

    // The temp MCP config file must outlive `execute_json`: the CLI
    // reads it by path, and dropping it early would delete the file
    // out from under the child.
    let mut mcp_guard = None;
    let query = command::build(intent, &mut mcp_guard)?;

    let started = Instant::now();
    let result = query.execute_json(&claude).await;
    let elapsed = started.elapsed();
    drop(mcp_guard);

    match result {
        Ok(query_result) => {
            let outcome = outcome::from_query_result(query_result, elapsed);
            if let Some(resume) = &outcome.resume {
                events.resume_id(resume).await;
            }
            Ok(outcome)
        }
        Err(e) => match outcome::capped(&e, elapsed, intent.resume.as_ref()) {
            Some(capped) => {
                if let Some(resume) = &capped.resume {
                    events.resume_id(resume).await;
                }
                Ok(capped)
            }
            None => Err(outcome::classify_failure(e, elapsed, provider)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciacola_agent::{Isolation, ProviderRegistry, Sandbox};
    use std::sync::Arc;

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

    /// Isolation, credential isolation, scoped/strict MCP, allowed
    /// tools, client-assigned resume, and turn ceilings are all things
    /// this adapter actually implements, so an intent that asks for
    /// them must not be blocked.
    #[test]
    fn every_other_security_constraint_this_adapter_implements_passes_validation() {
        let mut intent = TurnIntent::new("go");
        intent.isolation = Isolation::Full;
        intent.config_home = Some("/tmp/claude-home".into());
        intent.token_env = Some("CLAUDE_TOKEN".into());
        intent.allowed_tools = Some(vec!["Read".into()]);
        intent.resume = Some(ciacola_agent::ResumeId::ClientAssigned("agent-1".into()));
        intent.max_provider_turns = Some(20);
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
}
