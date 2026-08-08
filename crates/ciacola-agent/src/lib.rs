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
//!   use, a ceiling on provider-internal turns, the MCP endpoints it may
//!   reach, how sealed it is from ambient configuration, and which
//!   conversation to continue. All intent, no flags.
//! - [`Provider`] runs one, [`ProviderRegistry`] finds one by name, and
//!   [`Capabilities`] says up front what it cannot do.
//! - [`TurnOutcome`] is what a turn that *ended* looks like, including
//!   the ones that ended badly.
//! - [`AgentError`] is what a turn that never happened looks like.
//!
//! # Three decisions worth the words
//!
//! **A run that hit a ceiling comes back as data, not as `Err`.** The
//! provider ran, at length, and stopped at a cap we set. That cost real
//! money and it may have opened the conversation. Stringifying it into
//! an error throws both away, and a five minute run then lands in the
//! ledger as costing nothing: invisible to the spend limit and to
//! anything reading the board. So a cap is an [`TurnOutcome`] carrying
//! usage, a resume id, and a [`TurnFailure`]; `Err` is reserved for
//! "the turn did not happen". See [`FailureKind`].
//!
//! **Cost is not a shared `Option<u64>`.** Claude reports money; codex
//! reports tokens and deliberately refuses to synthesize a price. A
//! field that is permanently `None` on one side reads as "not this
//! time" when it means "never here", and that exact confusion is how
//! two upstream issues came to be scoped wrong
//! (joshrotenberg/codex-wrapper#111). [`Cost`] therefore has three
//! states, and [`TokenUsage`] is the portable measure that both sides
//! actually report.
//!
//! **An unsupported constraint is never silently dropped.** A provider
//! that cannot seal itself off from ambient configuration, or cannot
//! keep its credentials in a directory of our choosing, must say so and
//! fail. A provider that cannot count its own internal turns may warn
//! and carry on. The difference is [`Severity`], and the line is drawn
//! at security: see [`Constraint::security`].
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
pub mod error;
pub mod events;
pub mod intent;
pub mod outcome;
pub mod provider;

pub use capability::{Capabilities, Constraint, Severity, Unsupported, Validation};
pub use error::AgentError;
pub use events::{NoEvents, TurnEvents};
pub use intent::{Effort, Isolation, McpScope, ResumeId, TurnIntent};
pub use outcome::{Cost, FailureKind, TokenUsage, TurnFailure, TurnOutcome};
pub use provider::{Provider, ProviderKey, ProviderRegistry};

/// Boxed future, so [`Provider`] stays object-safe without an
/// `async-trait` dependency. Adapters write `Box::pin(async move { .. })`.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
