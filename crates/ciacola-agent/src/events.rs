//! Facts a backend can reveal before the turn finishes.
//!
//! A turn can run for twenty minutes. If the conversation id only
//! reaches durable storage when the turn ends, a crash at minute
//! nineteen loses it, and "send again" starts the conversation over
//! rather than resuming it. That is the exact failure ciacola already
//! paid for once, and the reason it now assigns ids up front.
//!
//! Up-front assignment does not cover every backend. One that names its
//! own conversations tells us the id partway through, and that moment
//! is the one worth persisting at. So the contract carries a sink the
//! adapter calls the instant an id appears, rather than only handing
//! ids back in the terminal [`TurnOutcome`](crate::TurnOutcome).
//!
//! Usage has the same problem when a process is stopped mid-turn. Some
//! streaming backends report token counts after each provider-internal
//! turn, before the final result event. Throwing those observations away
//! on cancellation would make an interrupted turn look wholly
//! unmeasured even though the backend had already told us otherwise.
//! [`TurnEvents::usage_snapshot`] is the deliberately small seam for
//! retaining them.
//!
//! # Honest limitation
//!
//! Whether either event arrives early depends on the backend. An adapter
//! over a CLI that reports usage only in its terminal event cannot
//! manufacture a partial snapshot. The seam keeps that limitation local
//! to the adapter rather than turning an unavailable measurement into a
//! zero.

use crate::BoxFut;
use crate::intent::ResumeId;
use crate::outcome::TokenUsage;

/// Told about facts a backend reveals before the terminal outcome.
///
/// Async because persisting is: the implementation ciacola cares about
/// writes to sqlite. Boxed rather than `async fn` for the same reason
/// [`Provider`](crate::Provider) is: this is used behind `dyn`.
pub trait TurnEvents: Send + Sync {
    /// The backend named this conversation.
    ///
    /// May be called with an id we already hold, when the backend
    /// simply confirms the one we assigned. Implementations must be
    /// idempotent and must not fail the turn: an id that cannot be
    /// persisted is worth a log line, never an abandoned run that has
    /// already been paid for.
    fn resume_id<'a>(&'a self, id: &'a ResumeId) -> BoxFut<'a, ()>;

    /// Best token counts the backend has reported for this ciacola turn
    /// so far.
    ///
    /// This is a cumulative snapshot, never a delta. A backend that first
    /// observes 10 input tokens and later observes 15 calls this with 10
    /// and then 15; a sink replaces the earlier value rather than adding
    /// the two. Backends call it only when at least one usage bucket was
    /// actually reported, so an all-zero snapshot is a measured zero and
    /// silence remains unreported.
    ///
    /// Synchronous on purpose. CLI wrappers expose synchronous stream
    /// callbacks, and accepting the value before that callback returns is
    /// what lets a caller hand it to persistence before cancellation drops
    /// the provider future. Implementations must therefore return quickly,
    /// normally by enqueueing the owned value for an async writer, and must
    /// never fail a turn because telemetry could not be stored.
    ///
    /// The default is a no-op so adding honest partial accounting does not
    /// break existing provider hosts that only care about terminal
    /// outcomes.
    fn usage_snapshot(&self, _usage: TokenUsage) {}
}

/// The sink for callers with nothing to persist: tests, and any path
/// that records the id from the terminal outcome instead.
pub struct NoEvents;

impl TurnEvents for NoEvents {
    fn resume_id<'a>(&'a self, _id: &'a ResumeId) -> BoxFut<'a, ()> {
        Box::pin(async {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// An implementation written before usage snapshots existed keeps
    /// compiling and inherits the no-op. This default is the contract's
    /// source-compatibility promise to out-of-tree provider hosts.
    struct ResumeOnly;

    impl TurnEvents for ResumeOnly {
        fn resume_id<'a>(&'a self, _id: &'a ResumeId) -> BoxFut<'a, ()> {
            Box::pin(async {})
        }
    }

    #[test]
    fn usage_snapshots_are_source_compatible_for_existing_sinks() {
        ResumeOnly.usage_snapshot(TokenUsage {
            input: 1,
            output: 2,
            cached_input: 0,
        });
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<TokenUsage>>);

    impl TurnEvents for Recorder {
        fn resume_id<'a>(&'a self, _id: &'a ResumeId) -> BoxFut<'a, ()> {
            Box::pin(async {})
        }

        fn usage_snapshot(&self, usage: TokenUsage) {
            self.0.lock().expect("usage recorder lock").push(usage);
        }
    }

    #[test]
    fn a_sink_receives_cumulative_snapshots_without_contract_side_arithmetic() {
        let recorder = Recorder::default();
        let first = TokenUsage {
            input: 10,
            output: 2,
            cached_input: 4,
        };
        let later = TokenUsage {
            input: 15,
            output: 5,
            cached_input: 6,
        };

        recorder.usage_snapshot(first);
        recorder.usage_snapshot(later);

        assert_eq!(
            recorder.0.lock().expect("usage recorder lock").as_slice(),
            [first, later],
            "the provider supplies cumulative values; the contract neither adds nor rewrites them"
        );
    }
}
