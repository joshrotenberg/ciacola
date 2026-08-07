//! Live values for argument completion.
//!
//! There are two ways to make driving this server pleasant from a
//! terminal, and only one of them is worth doing.
//!
//! The first is to write a ciacola REPL. Reading `mcp-repl`, that means
//! reimplementing about nineteen thousand lines of general MCP client:
//! schema-driven completion, an editor, job control, variables,
//! aliases, elicitation, sampling, wire tracing. All of it already
//! exists, none of it is about agents, and a fork of it would rot.
//!
//! The second is to make the *server* worth completing against, which
//! is the whole premise of a REPL whose command set is the server's
//! surface. A generic client already knows `send` takes an `agent_id`,
//! because the schema says so. What it cannot know is which agent ids
//! exist right now. That is this file: the server answers
//! `completion/complete`, and a generic REPL becomes a ciacola REPL
//! without either side knowing about the other.
//!
//! The same argument holds for the board, the notifications, and the
//! plugin facility. Be a good MCP citizen and the clients are
//! interchangeable; write a bespoke client and you own it forever.
//!
//! Enum-valued arguments are handled by the schema instead: `lane`,
//! `kind`, and `effort` are real enums, so a client completes them
//! with no round trip. Only open sets belong here.

use tower_mcp::protocol::{CompleteParams, CompletionReference};
use tower_mcp::{CompleteResult, McpRouter};

use crate::ledger::Ledger;
use crate::roles::Roles;

/// How many suggestions to return. The protocol caps a response at 100
/// and a terminal cannot use that many anyway.
const MAX: usize = 50;

/// Filter by what the user typed, matching case-insensitively anywhere
/// in the haystack rather than only at the start: a ULID has no
/// meaningful prefix to type.
fn matching<I: IntoIterator<Item = (String, String)>>(pairs: I, typed: &str) -> Vec<String> {
    let typed = typed.to_ascii_lowercase();
    pairs
        .into_iter()
        .filter(|(value, haystack)| {
            typed.is_empty()
                || value.to_ascii_lowercase().contains(&typed)
                || haystack.to_ascii_lowercase().contains(&typed)
        })
        .map(|(value, _)| value)
        .take(MAX)
        .collect()
}

/// Answer `completion/complete` from the ledger and the role catalog.
///
/// Attached by the binary because only it has both. Prompt references
/// are role prompts, whose arguments are free text we cannot guess, so
/// they complete to nothing rather than to something wrong.
pub fn attach(router: McpRouter, ledger: Ledger, roles: Roles) -> McpRouter {
    router.completion_handler(move |params: CompleteParams| {
        let (ledger, roles) = (ledger.clone(), roles.clone());
        async move {
            let name = params.argument.name.as_str();
            let value = params.argument.value.as_str();

            // A resource reference is completing a URI, not an
            // argument; the ones here are fixed and already listed.
            if matches!(params.reference, CompletionReference::Resource { .. }) {
                return Ok(CompleteResult::new(Vec::new()));
            }

            let suggestions = match name {
                // The completion has to be the id, because that is what
                // the tools take, but nobody types a ULID. So the name
                // is matched against as well: "alpha" finds alpha's id.
                "agent_id" | "spawned_by" => {
                    let agents = ledger.list_agents().await.unwrap_or_default();
                    matching(agents.into_iter().map(|a| (a.agent_id, a.name)), value)
                }
                "role" => matching(
                    roles
                        .all()
                        .iter()
                        .map(|r| (r.name.clone(), r.description.clone())),
                    value,
                ),
                _ => Vec::new(),
            };
            Ok(CompleteResult::new(suggestions))
        }
    })
}
