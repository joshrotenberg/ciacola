//! What ciacola asks a backend to do, and what it gets back.
//!
//! This crate is the provider boundary and nothing else. It depends on
//! no wrapper and on no part of ciacola, which is the property that
//! makes it a boundary rather than a layer: a second adapter can be
//! written against it without reading `ciacola-core`, and `ciacola-core`
//! can be read without knowing which CLI is behind the seam.
//!
//! # The shape
//!
//! - [`TurnIntent`] says what a turn is *for*: instructions, a prompt,
//!   a model, an effort level, a working directory, the tools it may
//!   use, a filesystem/network sandbox, a ceiling on provider-internal
//!   turns, the MCP endpoints it may reach, how sealed it is from
//!   ambient configuration, and which conversation to continue. All
//!   intent, no flags.
//! - [`Provider`] runs one, [`ProviderRegistry`] finds one by name, and
//!   [`Capabilities`] says up front what it cannot do.
//! - [`TurnOutcome`] is what a turn that *ended* looks like, including
//!   the ones that ended badly.
//! - [`AgentError`] is what a turn that never ran to a usable result
//!   looks like.
//!
//! # Four decisions worth the words
//!
//! **A run that hit a ceiling comes back as data, not as `Err`.** The
//! provider ran, at length, and stopped at a cap we set. That cost real
//! money and it may have opened the conversation. Stringifying it into
//! an error throws both away, and a five minute run then lands in the
//! ledger as costing nothing: invisible to the spend limit and to
//! anything reading the board. So a cap is an [`TurnOutcome`] carrying
//! usage, a resume id, and a [`TurnFailure`]. `Err` covers the rest --
//! except that a handful of its variants can *also* follow real spend;
//! see the next point and [`error`]'s own docs.
//!
//! **Not every `Err` is free, either.** [`AgentError::Protocol`],
//! [`AgentError::Timeout`], and [`AgentError::Cancelled`] can all be
//! raised after the provider process launched and did paid work: a
//! cancelled twenty-minute run has spent real money and may already
//! hold a session id. Whatever an adapter still knows in that case goes
//! on [`error::PartialTelemetry`] rather than being thrown away for the
//! sake of a blanket "Err means nothing happened" claim that does not
//! hold for those post-launch failures.
//!
//! **Cost and token usage are not shared `Option` fields.** Claude
//! reports money; codex reports tokens and deliberately refuses to
//! synthesize a price. A field that is permanently `None` on one side
//! reads as "not this time" when it means "never here", and that exact
//! confusion is how two upstream issues came to be scoped wrong
//! (joshrotenberg/codex-wrapper#111). [`Cost`] and [`Usage`] therefore
//! each have three states, the same shape for the same reason: a
//! provider that never reports says so once and for good
//! ([`Cost::NotPriced`], [`Usage::NotTracked`]); one that usually
//! reports and did not this time says so per-outcome
//! ([`Cost::Unreported`], [`Usage::Unreported`]).
//!
//! **An unsupported security constraint is never silently dropped.** A
//! provider that cannot seal itself off from ambient configuration,
//! cannot keep its credentials in a directory of our choosing, or
//! cannot confine filesystem writes and network reach the way a
//! [`Sandbox`] asks, must say so and fail. A provider that cannot count
//! its own internal turns may warn and carry on. A provider-work ceiling is
//! the other blocking boundary: it is not security, but silently dropping it
//! would exceed the spend authorization for the turn. The difference is
//! executable in [`Constraint::severity`]; [`Constraint::security`] remains
//! the narrower authority classification.
//!
//! # Why the futures are boxed
//!
//! [`Provider`] is used behind `dyn`, because the whole point of
//! [`ProviderRegistry`] is resolving an adapter by a string that came
//! out of the ledger. `async fn` in traits is not `dyn`-safe, and this
//! workspace does not take an `async-trait` dependency, so the futures
//! are spelled out as [`BoxFut`]. That is the same shape
//! `ciacola_core::plugin` already uses.

#![warn(missing_docs)]

use std::future::Future;
use std::pin::Pin;

pub mod capability;
pub mod environment;
pub mod error;
pub mod events;
pub mod intent;
pub mod outcome;
pub mod provider;

pub use capability::{
    CacheTreatment, Capabilities, CeilingCapability, Constraint, EnforcementGranularity, MeterId,
    Severity, Unsupported, Validation,
};
pub use environment::{
    PROVIDER_CHILD_BASELINE_ENV, ProviderChildEnvironment, ProviderChildEnvironmentError,
};
pub use error::{AgentError, PartialTelemetry};
pub use events::{NoEvents, TurnEvents};
pub use intent::{
    Effort, Isolation, McpEndpoint, McpScope, ResumeId, Sandbox, TurnCeiling, TurnIntent,
};
pub use outcome::{Cost, FailureKind, TokenUsage, TurnFailure, TurnOutcome, Usage};
pub use provider::{DuplicateProvider, Provider, ProviderKey, ProviderRegistry};

/// Boxed future, so [`Provider`] stays object-safe without an
/// `async-trait` dependency. Adapters write `Box::pin(async move { .. })`.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
