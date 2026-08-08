//! Typed provider failures.
//!
//! Most of what is here means **the turn did not happen**: no process,
//! no spend, no conversation. A turn that ran and went wrong is an
//! [`TurnOutcome`](crate::TurnOutcome) with a
//! [`TurnFailure`](crate::TurnFailure) on it, because it cost money and
//! may have opened a session, and `Err` would throw both away.
//!
//! Boxed strings were the previous answer, and they made every failure
//! look the same to the code above: a missing binary, a cancelled run,
//! and an unsupported security constraint all arrived as text. The
//! first is an operator's install problem, the second is normal, and
//! the third must never be retried as-is.
//!
//! # The exception, named rather than hidden
//!
//! [`Protocol`](AgentError::Protocol), [`Timeout`](AgentError::Timeout),
//! [`Cancelled`](AgentError::Cancelled), and an unclassified
//! [`Other`](AgentError::Other) can all be raised *after*
//! the provider process launched and did paid work: a CLI that drifted
//! past the wrapper's tested range still burned tokens producing the
//! output that failed to parse; a run that hit its deadline was working
//! for the whole twenty minutes we waited; a cancelled run stopped
//! mid-turn, not before it. Claiming "no spend, no session" for these
//! variants would be exactly the mistake this module exists to avoid
//! elsewhere. Where an adapter can say what it knows about that spend,
//! it goes on [`PartialTelemetry`] rather than being thrown away for
//! the sake of a claim that does not hold for them.

use std::fmt;
use std::time::Duration;

use crate::capability::Constraint;
use crate::intent::ResumeId;
use crate::outcome::{Cost, TokenUsage};
use crate::provider::ProviderKey;

/// Whatever an adapter can still say about a turn that stopped after
/// the provider had already started, when it can say anything at all.
///
/// Every field is `None` for "the adapter does not know", never a zero
/// or an empty value standing in for it: the whole reason this exists
/// is that a cancelled or timed-out run may have spent real money and
/// opened a real conversation, and inventing a default would recreate
/// the conflation [`Cost`] exists to avoid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialTelemetry {
    /// The conversation the backend had already opened, if the adapter
    /// knows it. Present even here so a cancelled turn can still be
    /// resumed rather than restarted.
    pub resume: Option<ResumeId>,
    /// Spend known before the turn stopped, if the adapter has it.
    pub cost: Option<Cost>,
    /// Tokens known before the turn stopped, if the adapter has them.
    pub usage: Option<TokenUsage>,
    /// Measured wall-clock time before the turn stopped. The caller can
    /// always supply this even when the provider reports no usage.
    pub elapsed: Option<Duration>,
}

impl PartialTelemetry {
    /// Nothing known. The honest default for an adapter that has no
    /// way to learn anything about a turn once it has decided to
    /// report `Err` for it.
    pub fn none() -> Self {
        Self::default()
    }

    /// True when every field is absent, i.e. this carries no more
    /// information than admitting the gap.
    pub fn is_empty(&self) -> bool {
        self.resume.is_none()
            && self.cost.is_none()
            && self.usage.is_none()
            && self.elapsed.is_none()
    }
}

/// A turn that did not run to a usable result.
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
    ///
    /// The tokens that produced the unparseable output were still
    /// spent; whatever the adapter knows of that goes on `partial`.
    Protocol {
        /// Which provider.
        provider: ProviderKey,
        /// What did not parse.
        detail: String,
        /// What is known about spend and the conversation, if anything.
        ///
        /// Boxed to keep [`AgentError`] small: it is the `Err` half of
        /// every turn's `Result`, and several variants carrying this
        /// inline pushed it past the size where clippy starts
        /// objecting to the cost on the success path.
        partial: Box<PartialTelemetry>,
    },
    /// The provider was still working when its deadline passed.
    ///
    /// The elapsed time is the minimum it actually ran for, not an
    /// estimate; `partial` carries whatever the adapter had already
    /// learned before giving up on it.
    Timeout {
        /// Which provider.
        provider: ProviderKey,
        /// How long it had.
        elapsed: Duration,
        /// What is known about spend and the conversation, if anything.
        ///
        /// Boxed to keep [`AgentError`] small: it is the `Err` half of
        /// every turn's `Result`, and several variants carrying this
        /// inline pushed it past the size where clippy starts
        /// objecting to the cost on the success path.
        partial: Box<PartialTelemetry>,
    },
    /// We stopped it. Normal, and not an incident: `kill` and a
    /// draining shutdown both land here.
    ///
    /// Normal does not mean free: a twenty minute run that gets
    /// cancelled has spent real money and may already hold a session
    /// id, and `partial` is where that survives instead of being
    /// thrown away with the rest of the turn.
    Cancelled {
        /// Which provider.
        provider: ProviderKey,
        /// What is known about spend and the conversation, if anything.
        ///
        /// Boxed to keep [`AgentError`] small: it is the `Err` half of
        /// every turn's `Result`, and several variants carrying this
        /// inline pushed it past the size where clippy starts
        /// objecting to the cost on the success path.
        partial: Box<PartialTelemetry>,
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
        /// What is known if this unclassified failure followed launch.
        partial: Box<PartialTelemetry>,
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
            | AgentError::Cancelled { provider, .. }
            | AgentError::Unsupported { provider, .. }
            | AgentError::Other { provider, .. } => Some(provider),
            AgentError::UnknownProvider { .. } | AgentError::Io { .. } => None,
        }
    }

    /// What is known about spend and the conversation, for the
    /// variants that can follow real work: [`Protocol`](Self::Protocol),
    /// [`Timeout`](Self::Timeout), [`Cancelled`](Self::Cancelled), and
    /// [`Other`](Self::Other).
    /// Every other variant returns `None` because none of them can
    /// happen after the provider has spent anything, which is the
    /// actual distinction this method draws: not "does this variant
    /// carry the field" but "can this variant ever have something to
    /// report".
    pub fn partial(&self) -> Option<&PartialTelemetry> {
        match self {
            AgentError::Protocol { partial, .. }
            | AgentError::Timeout { partial, .. }
            | AgentError::Cancelled { partial, .. }
            | AgentError::Other { partial, .. } => Some(partial.as_ref()),
            _ => None,
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
            AgentError::Protocol {
                provider, detail, ..
            } => {
                write!(
                    f,
                    "provider '{provider}' said something unreadable: {detail}"
                )
            }
            AgentError::Timeout {
                provider, elapsed, ..
            } => {
                write!(f, "provider '{provider}' timed out after {elapsed:?}")
            }
            AgentError::Cancelled { provider, .. } => {
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
            AgentError::Other {
                provider, detail, ..
            } => {
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
            partial: PartialTelemetry::none().into(),
        };
        assert!(e.is_cancelled());
        assert!(
            !AgentError::Io {
                detail: "boom".into()
            }
            .is_cancelled()
        );
    }

    /// The point of this module's honesty carve-out: a cancelled run
    /// that had already spent money and opened a conversation must not
    /// throw either away just because it is reported as `Err`.
    #[test]
    fn a_cancelled_turn_can_still_carry_its_spend_and_its_session() {
        let e = AgentError::Cancelled {
            provider: ProviderKey::claude(),
            partial: PartialTelemetry {
                resume: Some(ResumeId::ProviderAssigned("sess-mid-run".into())),
                cost: Some(Cost::Reported { micro_usd: 900_000 }),
                usage: Some(TokenUsage {
                    input: 4_000,
                    output: 300,
                    cached_input: 0,
                }),
                elapsed: Some(Duration::from_secs(1_200)),
            }
            .into(),
        };
        let partial = e.partial().expect("cancelled turns can carry partials");
        assert!(!partial.is_empty());
        assert_eq!(
            partial.resume.as_ref().map(ResumeId::value),
            Some("sess-mid-run")
        );
        assert_eq!(partial.cost, Some(Cost::Reported { micro_usd: 900_000 }));
        assert_eq!(partial.elapsed, Some(Duration::from_secs(1_200)));
    }

    /// A launch failure happens before the provider ever starts, so it
    /// has nothing to report and `partial()` must say so rather than
    /// fabricate an empty-but-present value.
    #[test]
    fn a_pre_launch_failure_has_no_partial_telemetry_at_all() {
        let e = AgentError::Launch {
            provider: ProviderKey::claude(),
            detail: "binary not found".into(),
        };
        assert!(e.partial().is_none());
    }

    /// The fallback variant must not reintroduce telemetry loss merely
    /// because an adapter could not classify a post-launch failure.
    #[test]
    fn an_unclassified_post_launch_failure_keeps_elapsed_time() {
        let e = AgentError::Other {
            provider: ProviderKey::claude(),
            detail: "unexpected wrapper failure".into(),
            partial: PartialTelemetry {
                elapsed: Some(Duration::from_secs(30)),
                ..PartialTelemetry::none()
            }
            .into(),
        };
        let partial = e.partial().expect("post-launch telemetry");
        assert_eq!(partial.elapsed, Some(Duration::from_secs(30)));
        assert!(!partial.is_empty());
    }
}
