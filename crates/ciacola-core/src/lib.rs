//! An agent is a durable conversation.
//!
//! Everything here follows from that one sentence. The provider keeps
//! the conversation and we keep its id, so an agent exists while
//! nothing is running, a *turn* is one process execution against it,
//! and recovery is resume rather than retry. A queue's durability buys
//! re-execution, which is exactly what paid agent work must never do.
//!
//! # What is core
//!
//! Whatever nothing works without: the provider seam ([`agent`]), the
//! ledger of agents and turns ([`ledger`]), an executor ([`exec`]),
//! notifications ([`notify`]), startup recovery ([`recover`]), the six
//! verbs ([`server`]), and the board shell ([`board`]).
//!
//! [`plugin::PluginContext`] is the precise statement of that line. If
//! a plugin needs something not on that struct, either it belongs here
//! or the plugin is reaching.
//!
//! # What is not
//!
//! Everything else, including the parts this system leans on hardest:
//! the kanban, memory, findings, schedules, references, git state,
//! webhooks, model statistics, and the repository worker. They are all
//! plugins going through the same [`plugin::Plugin`] trait a third
//! party would, which is the only thing that keeps that trait honest. A
//! built-in with a privileged path leaves the plugin API a second-class
//! citizen that rots.
//!
//! The facility is also not a lock-in, which is what lets it stay
//! small. Agents are handed an MCP config, so any other MCP server can
//! be added to it and the agent cannot tell the difference. A plugin
//! earns its keep only when it needs something core owns: the ledger,
//! the board, the health and retention passes, or the agent lifecycle.
//!
//! # Two rules learned expensively
//!
//! **A guard on one path is not a guard.** The spend limit was added to
//! one submission path and the primary path walked past it, spending
//! four times the configured stop. Runtime defaults were applied on two
//! of three creation paths and the third produced agents with no
//! isolation. Both are now enforced where every path converges:
//! [`plugin::submit`] and [`ledger::Ledger::create_agent`].
//!
//! **Isolation has to be paired with putting back what it removes.**
//! Hermetic agents inherit no ambient configuration, which is the
//! point, and which silently removed the operator's own standing rules
//! until house rules became an explicit layer of the system prompt.

pub mod agent;
pub mod exec;
pub mod health;
pub mod ledger;
pub mod limits;
pub mod notify;
pub mod plugin;
pub mod polling;
pub mod recover;
pub mod registry;
pub mod render;
pub mod roles;
pub mod server;
pub mod store;
pub mod time;

pub use agent::{Agent, AgentDef, Exchange, FlatError, Turn, prompt, run_exchange};
pub use exec::{HandExecutor, TurnExecutor, run_turn};
pub use ledger::{AgentRow, Ledger, TurnRow};
pub use limits::Limits;
pub use notify::Notifier;
pub use plugin::{
    BoxFut, Migration, Plugin, PluginContext, PluginHost, Section, Submission, Surface,
    apply_migrations, submit,
};
pub use polling::PollingExecutor;
pub use roles::{Role, Roles, Runtime};
pub use store::Store;
pub use time::now_unix;
