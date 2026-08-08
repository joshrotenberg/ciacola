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
//! - **Present**: the caller is the named agent, because it proved it
//!   with a secret only its own config file holds. `spawned_by` is
//!   derived from this and any claimed value is ignored, which is what
//!   closes the honour-system hole: lineage drives the cost rollup and
//!   the depth cap, and both were previously built on whatever the
//!   caller said.
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
