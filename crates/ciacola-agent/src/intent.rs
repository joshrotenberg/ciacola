//! A turn described as intent rather than as provider flags.
//!
//! Everything here is what a *person* would say about a turn: who the
//! agent is, what it is being asked, how hard to think, what it may
//! touch, and which conversation it continues. Nothing here is a CLI
//! flag, and nothing here names a vendor. Translating intent into flags
//! is the adapter's whole job, and keeping the translation on one side
//! of this line is what lets a second backend arrive without another
//! round of conditionals in the core.

use std::path::PathBuf;

/// Which conversation to continue, and who chose its name.
///
/// The two variants are not cosmetic. ciacola assigns an id *before*
/// the first turn runs, precisely so a crash mid-turn leaves something
/// resumable behind; before that, the id was recorded only at turn end
/// and a crash meant "send again" started the conversation over. So the
/// contract has to carry both cases, and an adapter has to be able to
/// tell them apart, because they render differently: naming a session
/// that does not exist yet is a different request from resuming one
/// that does. Asking for the wrong one is the dogfood bug where a
/// capped first turn made the second turn try to create an id the
/// provider had already created.
///
/// The string itself is deliberately opaque. It is a Claude session id
/// today, a codex thread id next, and a REST conversation id after
/// that; nothing above this line should parse it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeId {
    /// We chose it, and the backend has not seen it yet. The turn that
    /// uses it opens the conversation and must carry the instructions.
    ClientAssigned(String),
    /// The backend told us this id, either because it minted one or
    /// because a previous turn opened the one we assigned. The
    /// conversation exists and already carries the instructions.
    ProviderAssigned(String),
}

impl ResumeId {
    /// The id itself, whoever chose it.
    pub fn value(&self) -> &str {
        match self {
            ResumeId::ClientAssigned(id) | ResumeId::ProviderAssigned(id) => id,
        }
    }

    /// True when the conversation exists at the backend, so a turn
    /// should resume rather than open it.
    pub fn is_open(&self) -> bool {
        matches!(self, ResumeId::ProviderAssigned(_))
    }
}

/// How hard to think. Paired with the model because that is how it is
/// actually chosen: a manager reasoning about supervision wants more
/// than a spoke summarizing a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    /// The least the model will do.
    Low,
    /// The usual middle.
    Medium,
    /// More deliberation, more tokens.
    High,
    /// Past `High`, where the provider offers it.
    Xhigh,
    /// Everything the provider will give.
    Max,
}

impl Effort {
    /// Parse a configured string. `None` for anything unrecognised, so
    /// the caller decides whether a typo in an optional hint is worth
    /// failing a turn over. It is not: ciacola warns and takes the
    /// provider default.
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            "xhigh" => Some(Effort::Xhigh),
            "max" => Some(Effort::Max),
            _ => None,
        }
    }
}

/// How sealed a turn is from the operator's ambient provider
/// configuration.
///
/// **This is a security constraint, not a preference.** An interactive
/// session inherits a `CLAUDE.md`, skills, and settings that the person
/// can see and reason about; a scheduled agent inherits them invisibly,
/// so its behaviour depends on files nobody remembered were there. A
/// provider that cannot honour the requested scope must fail rather
/// than run wide open; see [`Constraint::security`](crate::Constraint::security).
///
/// Isolation also has to be paired with putting back what it removes.
/// Sealing off the ambient configuration silently removed the
/// operator's own standing rules once, and the first real pull request
/// carried a trailer those rules forbid. That is why
/// [`TurnIntent::instructions`] is composed by the caller rather than
/// left to whatever the environment happens to contain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Isolation {
    /// Inherit whatever the environment offers. The interactive
    /// default, and the wrong default for unattended work.
    #[default]
    Inherit,
    /// Drop project and local settings; keep the user's global ones.
    Project,
    /// Drop user, project, and local settings alike.
    Full,
}

impl Isolation {
    /// Parse a configured scope. `None` for anything unrecognised.
    ///
    /// `true`/`false` are accepted because the setting began life as a
    /// boolean and both spellings are in live configuration files.
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "full" | "true" => Some(Isolation::Full),
            "project" => Some(Isolation::Project),
            "none" | "false" => Some(Isolation::Inherit),
            _ => None,
        }
    }

    /// True when the turn is asking to be sealed off from something.
    /// [`Isolation::Inherit`] asks for nothing, so it can never be
    /// dropped by a provider that does not support isolation.
    pub fn is_sealed(&self) -> bool {
        !matches!(self, Isolation::Inherit)
    }
}

/// The MCP endpoints a turn may reach.
///
/// This is the recursion mechanism: point it at a server (this one,
/// say) and the agent can drive agents with the same verbs a person
/// uses. `strict` means *only* these, which is what makes the endpoint
/// an authority boundary rather than a suggestion: an agent handed the
/// agent mount must not be able to add the operator one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpScope {
    /// Path to a config file describing the servers.
    pub config_path: String,
    /// Refuse anything not in that file. Dropping this quietly would
    /// widen an agent's reach, so it is a security constraint.
    pub strict: bool,
}

/// One turn, as intent.
///
/// Built by the caller from an agent definition and the thing being
/// said; consumed by exactly one [`Provider`](crate::Provider). Every
/// field is optional-by-meaning: `None` means "the provider's default",
/// never "zero".
#[derive(Debug, Clone, Default)]
pub struct TurnIntent {
    /// System or developer instructions: who the agent is, what good
    /// looks like, what it may not do.
    ///
    /// Sent only when the turn opens a conversation, because a resumed
    /// conversation already carries them. The caller composes this
    /// (rather than the adapter) so that every creation path gets the
    /// same layers: a guard on one path is not a guard.
    pub instructions: Option<String>,
    /// The thing being said this turn.
    pub prompt: String,
    /// Provider model. `None` is the provider's default.
    pub model: Option<String>,
    /// How hard to think. `None` is the provider's default.
    pub effort: Option<Effort>,
    /// Where the agent works. `None` means it does not touch a
    /// filesystem.
    pub working_dir: Option<PathBuf>,
    /// Tools the agent may use. Empty means none beyond conversation.
    ///
    /// A grant, not a hint. A provider that cannot enforce it must fail
    /// rather than hand the agent everything: a toolless spoke does not
    /// refuse, it fabricates, and the inverse is worse.
    pub allowed_tools: Vec<String>,
    /// Ceiling on provider-internal turns (tool calls and the like) for
    /// this one reply. `None` is the provider's default.
    pub max_provider_turns: Option<u32>,
    /// Scoped MCP endpoints. `None` means the agent gets whatever the
    /// provider would give it, which for an isolated turn is nothing.
    pub mcp: Option<McpScope>,
    /// How sealed this turn is from ambient configuration.
    pub isolation: Isolation,
    /// Where the provider keeps its configuration, and its login.
    ///
    /// Pointing this at the server's own directory keeps transcripts
    /// with the run that produced them rather than mixed into the
    /// operator's history. **It also isolates credentials**, which is
    /// why it is paired with [`token_env`](Self::token_env): a fresh
    /// directory authenticates as nobody, and every turn then fails
    /// with "not logged in".
    pub config_home: Option<String>,
    /// The *name* of an environment variable holding a long-lived
    /// token, read from the server's own environment.
    ///
    /// The name, never the value, so a credential never lands in a
    /// config file, the ledger, argv, the board, or a log line. Nothing
    /// in this crate ever stores the value it resolves to.
    pub token_env: Option<String>,
    /// Which conversation to continue. `None` starts a fresh one and
    /// lets the provider name it.
    pub resume: Option<ResumeId>,
}

impl TurnIntent {
    /// A turn that says `prompt` and nothing else. Fill in the rest by
    /// assignment; there are enough fields that a builder per field
    /// would be noise.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The distinction the whole resume story rests on: an id we chose
    /// is not yet a conversation, and an adapter must be able to see
    /// the difference without parsing the string.
    #[test]
    fn a_client_assigned_id_is_not_an_open_conversation() {
        assert!(!ResumeId::ClientAssigned("a".into()).is_open());
        assert!(ResumeId::ProviderAssigned("a".into()).is_open());
        assert_eq!(ResumeId::ClientAssigned("a".into()).value(), "a");
    }

    /// Both spellings of the isolation switch are in live config files,
    /// because it began life as a boolean.
    #[test]
    fn isolation_parses_the_spellings_that_exist_in_config() {
        assert_eq!(Isolation::parse("full"), Some(Isolation::Full));
        assert_eq!(Isolation::parse("true"), Some(Isolation::Full));
        assert_eq!(Isolation::parse("Project"), Some(Isolation::Project));
        assert_eq!(Isolation::parse("none"), Some(Isolation::Inherit));
        assert_eq!(Isolation::parse("false"), Some(Isolation::Inherit));
        assert_eq!(Isolation::parse("hermetic"), None);
    }

    /// Asking to inherit is asking for nothing, so it can never be the
    /// constraint a provider silently drops.
    #[test]
    fn inheriting_is_not_a_constraint() {
        assert!(!Isolation::Inherit.is_sealed());
        assert!(Isolation::Project.is_sealed());
        assert!(Isolation::Full.is_sealed());
    }

    #[test]
    fn effort_is_case_insensitive_and_rejects_typos() {
        assert_eq!(Effort::parse("XHIGH"), Some(Effort::Xhigh));
        assert_eq!(Effort::parse("hihg"), None);
    }
}
