//! Who is calling, when the call came over the loopback.
//!
//! Every agent's MCP config carries a per-agent secret in a header. A
//! layer in front of the HTTP mounts maps that header to an agent id
//! with [`crate::ledger::Ledger::agent_id_by_token`] and attaches the
//! result to the request; the transport bridges it into
//! [`tower_mcp::context::RequestContext`], where a tool reads it with
//! `ctx.extension::<AgentIdentity>()`.
//!
//! What its presence means, precisely:
//!
//! - **Present**: at the protocol boundary, the caller holds the named
//!   agent's scoped bearer. `spawned_by` is derived from this and any claimed
//!   value is ignored, which closes the honour-system hole: lineage drives
//!   the cost rollup and depth cap, and both were previously built on whatever
//!   the caller said. This is identity inside the shared OS-user trust
//!   boundary, not proof that another same-user process could not copy the
//!   bearer.
//! - **Absent on stdio**: the operator's own terminal. There is no
//!   HTTP request, so there is nothing to authenticate, and claimed
//!   attribution is accepted; that a person at the terminal is trusted
//!   is the definition of operator.
//! - **Absent over HTTP**: an anonymous local caller (mcp-repl during
//!   debugging, curl). Tolerated, but it can claim nothing: a spawn
//!   without identity on the agent surface gets no parentage at all.

/// The authenticated agent id behind a loopback request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity(pub String);

/// The header an agent's MCP config carries its token in.
pub const TOKEN_HEADER: &str = "x-ciacola-agent";

/// What an authenticated parent may hand to a child.
///
/// Kept here beside caller identity because the ceiling is an authority
/// rule, not a property of any one creation tool. Raw `spawn`,
/// `spawn_role`, and bundled capabilities such as repo-worker all use the
/// same calculation so adding a new creation path cannot quietly skip it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildToolGrant {
    pub granted: Vec<String>,
    pub denied: Vec<String>,
}

/// Whether one Claude Code tool grant contains another.
///
/// Most grants are exact names. Two useful aggregate forms appear in
/// shipped roles: `Bash(git:*)` contains narrower git commands, and an
/// MCP server name such as `mcp__ciacola` contains its individual tools.
/// Keep the matcher conservative: an unfamiliar pattern must be requested
/// explicitly rather than being guessed broader.
fn tool_covers(parent: &str, requested: &str) -> bool {
    if parent == requested {
        return true;
    }
    if let Some(prefix) = parent.strip_suffix(":*)") {
        return requested
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(' ') || rest.starts_with(':'));
    }
    if !parent.contains('(')
        && requested
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('('))
    {
        return true;
    }
    parent.starts_with("mcp__")
        && requested
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with("__"))
}

/// Intersect a requested child tool set with the authenticated parent's.
/// An absent caller is the operator or an anonymous local request, whose
/// treatment is decided by the surface before this function is called.
pub async fn grant_child_tools(
    ledger: &crate::ledger::Ledger,
    caller: Option<&str>,
    requested: Vec<String>,
) -> Result<ChildToolGrant, crate::agent::FlatError> {
    let Some(parent_id) = caller else {
        return Ok(ChildToolGrant {
            granted: requested,
            denied: Vec::new(),
        });
    };
    let parent = ledger
        .get_agent(parent_id)
        .await?
        .ok_or_else(|| format!("caller '{parent_id}' not found"))?;
    let (granted, denied) = requested.into_iter().partition(|tool| {
        parent
            .def
            .allowed_tools
            .iter()
            .any(|held| tool_covers(held, tool))
    });
    Ok(ChildToolGrant { granted, denied })
}

#[cfg(test)]
mod tests {
    use super::tool_covers;

    #[test]
    fn aggregate_grants_cover_only_their_narrower_tools() {
        assert!(tool_covers("Bash(git:*)", "Bash(git status:*)"));
        assert!(tool_covers("Bash", "Bash(git status:*)"));
        assert!(!tool_covers("Bash(git:*)", "Bash(github:*)"));
        assert!(!tool_covers("Bash(git status:*)", "Bash(git:*)"));
        assert!(tool_covers("mcp__ciacola", "mcp__ciacola__track"));
        assert!(!tool_covers("mcp__ciacola__track", "mcp__ciacola__items"));
    }
}
