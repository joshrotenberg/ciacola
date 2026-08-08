//! A sink for the one thing that must not wait for the turn to finish.
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
//! # Honest limitation
//!
//! Whether "the instant it appears" is early depends on the backend. An
//! adapter over a CLI that only prints its result event at the end
//! cannot call this any sooner than the end, and the Claude adapter is
//! in that position today. The seam is here so that a streaming backend
//! is a change of one adapter rather than a change of the contract.

use crate::BoxFut;
use crate::intent::ResumeId;

/// Told about a turn's conversation id the moment the backend reveals
/// one.
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
}

/// The sink for callers with nothing to persist: tests, and any path
/// that records the id from the terminal outcome instead.
pub struct NoEvents;

impl TurnEvents for NoEvents {
    fn resume_id<'a>(&'a self, _id: &'a ResumeId) -> BoxFut<'a, ()> {
        Box::pin(async {})
    }
}
