//! What a turn that ended looks like, including the ones that ended
//! badly.
//!
//! The distinction this module exists to make: **ending badly is not
//! the same as never happening.** A run that worked for five minutes
//! and stopped at a ceiling we set spent real money and may have opened
//! the conversation, so it comes back here as an [`TurnOutcome`] with a
//! [`TurnFailure`] on it. `Err(`[`AgentError`](crate::AgentError)`)` is
//! reserved for turns that did not happen: no process, no spend, no
//! session.
//!
//! That was learned the expensive way. The wrapper keeps cost and
//! session id off the terminal result event for a capped run, and
//! stringifying the error threw both away, so a long run landed in the
//! ledger as costing nothing: invisible to the spend limit, invisible
//! on the board, and unresumable.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::intent::ResumeId;

/// Tokens, which are the portable measure.
///
/// Cost is not: codex reports usage and no price at all, and
/// deliberately refuses to synthesize one. A ledger that records only
/// dollars goes blank the moment a second provider lands, so tokens sit
/// beside cost rather than under it.
///
/// `cached_input` is a subset of `input`, reported when the provider
/// distinguishes it. Zero means "not reported", which is the honest
/// reading: nothing here is ever invented to fill a gap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Tokens sent to the model.
    pub input: u64,
    /// Tokens the model produced.
    pub output: u64,
    /// The part of `input` served from a cache.
    pub cached_input: u64,
}

impl TokenUsage {
    /// Usage with nothing reported. Distinct in meaning from a run that
    /// genuinely used nothing, but not distinguishable here: no
    /// provider reports the difference, so pretending otherwise would
    /// be a field that is always the same value.
    pub const NONE: TokenUsage = TokenUsage {
        input: 0,
        output: 0,
        cached_input: 0,
    };
}

/// What a turn cost, in three states rather than two.
///
/// A plain `Option<u64>` cannot say the thing that matters. Claude
/// reports money; codex reports tokens and no price. An always-`None`
/// field reads as "not this time" when it means "never here", and that
/// exact confusion is how two upstream issues came to be scoped wrong
/// (joshrotenberg/codex-wrapper#111). ciacola has the same bug in the
/// other direction today: `cost_micro_usd = 0` in the ledger means both
/// "free" and "unreported", and nothing can tell which.
///
/// So a provider that never prices its work says [`Cost::NotPriced`]
/// once, in its [`Capabilities`](crate::Capabilities), and every one of
/// its outcomes says it again. A provider that *can* price and did not
/// this time says [`Cost::Unreported`], which is a gap worth a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cost {
    /// The provider priced this run.
    Reported {
        /// Millionths of a US dollar. Integer because money in a
        /// database should not be a float.
        micro_usd: u64,
    },
    /// This provider prices its work, but this run came back without a
    /// price. A gap, and worth saying so out loud.
    Unreported,
    /// This provider never reports money. Not a gap; there is nothing
    /// to report and nothing to warn about.
    NotPriced,
}

impl Cost {
    /// The price when there is one. `None` covers both
    /// [`Cost::Unreported`] and [`Cost::NotPriced`], and callers that
    /// care about the difference must match rather than call this.
    pub fn micro_usd(&self) -> Option<u64> {
        match self {
            Cost::Reported { micro_usd } => Some(*micro_usd),
            Cost::Unreported | Cost::NotPriced => None,
        }
    }

    /// The price, flattened to zero when absent, for the ledger columns
    /// that predate this type.
    ///
    /// Every call is a place where "free" and "unreported" become
    /// indistinguishable, which is the conflation issue 53 names. Kept
    /// as one obvious function so those places can be found.
    pub fn micro_usd_or_zero(&self) -> u64 {
        self.micro_usd().unwrap_or(0)
    }

    /// True when the provider could have priced this run and did not.
    /// [`Cost::NotPriced`] is not missing data, so it is not this.
    pub fn is_missing(&self) -> bool {
        matches!(self, Cost::Unreported)
    }
}

/// Why a turn that ran did not succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// The run hit a ceiling we set: provider-internal turns, a budget,
    /// a wall clock. The provider worked, at length, and stopped.
    ///
    /// Kept apart from [`FailureKind::Reported`] because the two want
    /// different handling: a cap carries a cost and a resume id but no
    /// usage breakdown, and it is not an incident.
    Limit,
    /// The provider ran and reported the result itself as an error.
    Reported,
}

/// A turn that ran and did not succeed, with everything it still knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFailure {
    /// Which sort of failure.
    pub kind: FailureKind,
    /// What to show an operator. Never a credential, never argv.
    pub message: String,
}

impl TurnFailure {
    /// A ceiling we set.
    pub fn limit(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Limit,
            message: message.into(),
        }
    }

    /// The provider called its own result an error.
    pub fn reported(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Reported,
            message: message.into(),
        }
    }
}

/// A turn that ended, however it ended.
///
/// Terminal by construction: reaching this means the provider ran. A
/// turn that could not be run at all is
/// `Err(`[`AgentError`](crate::AgentError)`)` instead, and the two must
/// not be conflated, because only one of them costs money.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnOutcome {
    /// What the agent said. Empty for a run that ended at a cap before
    /// producing a reply.
    pub reply: String,
    /// The conversation this turn leaves behind, resumable by anyone
    /// holding it from any process at any later time.
    ///
    /// Present even on failure where the provider gave one, or where
    /// the failure itself proves the assigned id was opened: that is
    /// the difference between an agent that can be sent to again and
    /// one that starts over.
    pub resume: Option<ResumeId>,
    /// What it cost, in the three states [`Cost`] distinguishes.
    pub cost: Cost,
    /// Tokens in, out, and cached.
    pub usage: TokenUsage,
    /// Provider-internal turns spent producing the reply, where the
    /// provider counts them. `None` means it does not, which is not the
    /// same as zero.
    pub provider_turns: Option<u32>,
    /// Wall clock for the attempt. Measurable whatever went wrong, and
    /// it used to be discarded: a five minute failure read as 0ms, so
    /// the runs most worth investigating looked like the cheapest ones.
    pub elapsed: Duration,
    /// Anything else the provider said that is worth keeping and has no
    /// portable home. Free-form on purpose: a shared field that only
    /// one provider ever fills is the mistake this crate is trying not
    /// to repeat.
    pub metadata: BTreeMap<String, String>,
    /// `Some` when the turn ran and did not succeed.
    pub failure: Option<TurnFailure>,
}

impl TurnOutcome {
    /// A turn that succeeded, with everything else left at its
    /// "nothing reported" value.
    pub fn ok(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
            resume: None,
            cost: Cost::Unreported,
            usage: TokenUsage::NONE,
            provider_turns: None,
            elapsed: Duration::ZERO,
            metadata: BTreeMap::new(),
            failure: None,
        }
    }

    /// True when the provider produced a usable reply.
    pub fn succeeded(&self) -> bool {
        self.failure.is_none()
    }

    /// The failure message, if this turn had one.
    pub fn failure_message(&self) -> Option<&str> {
        self.failure.as_ref().map(|f| f.message.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reason [`Cost`] is not an `Option`: "this provider
    /// never reports money" and "this run came back without a price"
    /// must not read the same.
    #[test]
    fn unpriced_and_unreported_are_different_facts() {
        assert_eq!(Cost::NotPriced.micro_usd(), None);
        assert_eq!(Cost::Unreported.micro_usd(), None);
        assert_ne!(Cost::NotPriced, Cost::Unreported);
        assert!(
            Cost::Unreported.is_missing(),
            "a provider that prices and did not is a gap"
        );
        assert!(
            !Cost::NotPriced.is_missing(),
            "a provider that never prices has nothing missing"
        );
    }

    /// The flattening the old ledger columns force. Named so the places
    /// that lose the distinction are greppable.
    #[test]
    fn flattening_to_zero_is_explicit() {
        assert_eq!(Cost::Reported { micro_usd: 12 }.micro_usd_or_zero(), 12);
        assert_eq!(Cost::NotPriced.micro_usd_or_zero(), 0);
        assert_eq!(Cost::Unreported.micro_usd_or_zero(), 0);
    }

    /// A cap is data. It keeps its spend and its session, and it still
    /// reads as failed.
    #[test]
    fn a_capped_outcome_keeps_its_spend_and_its_session() {
        let outcome = TurnOutcome {
            failure: Some(TurnFailure::limit("reached maximum number of turns (60)")),
            cost: Cost::Reported {
                micro_usd: 1_250_000,
            },
            resume: Some(ResumeId::ProviderAssigned("sess-1".into())),
            elapsed: Duration::from_millis(323_000),
            ..TurnOutcome::ok("")
        };
        assert!(!outcome.succeeded());
        assert_eq!(outcome.cost.micro_usd(), Some(1_250_000));
        assert_eq!(outcome.resume.as_ref().map(ResumeId::value), Some("sess-1"));
        assert_eq!(
            outcome.failure.as_ref().map(|f| f.kind),
            Some(FailureKind::Limit)
        );
    }
}
