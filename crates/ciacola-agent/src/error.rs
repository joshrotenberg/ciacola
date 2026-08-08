//! Typed provider failures.
//!
//! Everything here means **the turn did not happen**: no process, no
//! spend, no conversation. A turn that ran and went wrong is an
//! [`TurnOutcome`](crate::TurnOutcome) with a
//! [`TurnFailure`](crate::TurnFailure) on it, because it cost money and
//! may have opened a session, and `Err` would throw both away.
//!
//! Boxed strings were the previous answer, and they made every failure
//! look the same to the code above: a missing binary, a cancelled run,
//! and an unsupported security constraint all arrived as text. The
//! first is an operator's install problem, the second is normal, and
//! the third must never be retried as-is.

use std::fmt;
use std::time::Duration;

use crate::capability::Constraint;
use crate::provider::ProviderKey;

/// A turn that did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentError {
    /// No adapter is registered under this key. Almost always a typo in
    /// a definition, or a binary built without the adapter linked in.
    UnknownProvider {
        /// The key that was asked for.
        requested: String,
        /// What is registered, so the message can name the alternatives
        /// rather than leaving the reader to guess.
        known: Vec<String>,
    },
    /// The provider's binary or endpoint could not be found.
    NotFound {
        /// Which provider.
        provider: ProviderKey,
        /// What was looked for.
        detail: String,
    },
    /// The provider could not be started.
    Launch {
        /// Which provider.
        provider: ProviderKey,
        /// Why not.
        detail: String,
    },
    /// The provider ran but its output could not be understood. Usually
    /// a CLI that has drifted past the wrapper's tested range.
    Protocol {
        /// Which provider.
        provider: ProviderKey,
        /// What did not parse.
        detail: String,
    },
    /// The provider was still working when its deadline passed.
    Timeout {
        /// Which provider.
        provider: ProviderKey,
        /// How long it had.
        elapsed: Duration,
    },
    /// We stopped it. Normal, and not an incident: `kill` and a
    /// draining shutdown both land here.
    Cancelled {
        /// Which provider.
        provider: ProviderKey,
    },
    /// The provider cannot honour a constraint that must not be
    /// silently dropped. See [`Constraint::security`].
    Unsupported {
        /// Which provider.
        provider: ProviderKey,
        /// Which constraint.
        constraint: Constraint,
        /// A sentence for whoever has to fix it.
        detail: String,
    },
    /// Our own side failed before the provider was reached: creating
    /// the configuration directory, reading a config file.
    Io {
        /// What we were doing.
        detail: String,
    },
    /// The adapter could not classify this one.
    ///
    /// Deliberately present. Forcing every wrapper error into one of
    /// the buckets above would put failures in the wrong one, which is
    /// worse than admitting the gap.
    Other {
        /// Which provider.
        provider: ProviderKey,
        /// Whatever it said.
        detail: String,
    },
}

impl AgentError {
    /// True when we stopped the turn ourselves, so nothing above needs
    /// to treat it as a fault.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, AgentError::Cancelled { .. })
    }

    /// The provider this came from, where there is one.
    /// [`AgentError::UnknownProvider`] and [`AgentError::Io`] have
    /// none, which is the point of them.
    pub fn provider(&self) -> Option<&ProviderKey> {
        match self {
            AgentError::NotFound { provider, .. }
            | AgentError::Launch { provider, .. }
            | AgentError::Protocol { provider, .. }
            | AgentError::Timeout { provider, .. }
            | AgentError::Cancelled { provider }
            | AgentError::Unsupported { provider, .. }
            | AgentError::Other { provider, .. } => Some(provider),
            AgentError::UnknownProvider { .. } | AgentError::Io { .. } => None,
        }
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::UnknownProvider { requested, known } => {
                write!(f, "no provider adapter registered as '{requested}'")?;
                if known.is_empty() {
                    write!(f, "; none are registered")
                } else {
                    write!(f, "; registered: {}", known.join(", "))
                }
            }
            AgentError::NotFound { provider, detail } => {
                write!(f, "provider '{provider}' not found: {detail}")
            }
            AgentError::Launch { provider, detail } => {
                write!(f, "provider '{provider}' could not start: {detail}")
            }
            AgentError::Protocol { provider, detail } => {
                write!(
                    f,
                    "provider '{provider}' said something unreadable: {detail}"
                )
            }
            AgentError::Timeout { provider, elapsed } => {
                write!(f, "provider '{provider}' timed out after {elapsed:?}")
            }
            AgentError::Cancelled { provider } => {
                write!(f, "provider '{provider}' run was cancelled")
            }
            AgentError::Unsupported {
                provider,
                constraint,
                detail,
            } => write!(
                f,
                "provider '{provider}' cannot honour {constraint:?}: {detail}"
            ),
            AgentError::Io { detail } => write!(f, "{detail}"),
            AgentError::Other { provider, detail } => {
                write!(f, "provider '{provider}': {detail}")
            }
        }
    }
}

impl std::error::Error for AgentError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing adapter is nearly always a typo, so the message names
    /// what is actually registered rather than leaving it to be guessed.
    #[test]
    fn an_unknown_provider_names_the_alternatives() {
        let e = AgentError::UnknownProvider {
            requested: "claud".into(),
            known: vec!["claude".into()],
        };
        let text = e.to_string();
        assert!(text.contains("claud"), "{text}");
        assert!(text.contains("registered: claude"), "{text}");
    }

    /// Stopping a turn on purpose is not a fault, and the code above
    /// has to be able to tell without reading a string.
    #[test]
    fn cancellation_is_distinguishable_without_parsing_text() {
        let e = AgentError::Cancelled {
            provider: ProviderKey::claude(),
        };
        assert!(e.is_cancelled());
        assert!(
            !AgentError::Io {
                detail: "boom".into()
            }
            .is_cancelled()
        );
    }
}
