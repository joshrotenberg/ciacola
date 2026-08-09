//! What a provider cannot do, declared up front and checked before the
//! turn runs.
//!
//! # Why declaring beats discovering
//!
//! Parity of API shape between two backends is achievable; parity of
//! capability often is not. The temptation is to accept every field and
//! quietly ignore the ones a given backend cannot honour, which is fine
//! right up until the ignored field was the one holding an agent inside
//! its box.
//!
//! # Where the line between fail and warn falls
//!
//! At boundaries, and nowhere else.
//!
//! **Fail** ([`Severity::Fail`]) when dropping the constraint would
//! widen what the agent can reach or see, or exceed spend the caller
//! authorized: isolation from ambient
//! configuration, credential isolation, the filesystem/network sandbox,
//! the scoped MCP endpoint list, and the allowed-tool grant. Each of
//! those is the whole of some boundary. An agent handed the operator
//! MCP mount because `strict` was ignored is one guessable path away
//! from `kill` and `open_pr`; an agent that inherited the operator's
//! ambient rules because isolation was ignored behaves in ways nobody
//! configured; an agent that wrote outside its working directory
//! because a claimed sandbox was actually a permission prompt has done
//! the thing the sandbox existed to prevent.
//!
//! **Warn** ([`Severity::Warn`]) when dropping the constraint only
//! degrades a run: an effort level the backend does not have or a
//! ceiling on provider-internal turns it does not count. A per-turn
//! provider-work ceiling is different: it is explicit authorization to
//! spend no further than that boundary, so dropping it fails even
//! though [`Constraint::security`] correctly says it is not security.
//!
//! The authority rule is [`Constraint::security`], and
//! [`Constraint::severity`] names the additional spend boundary.

use serde::{Deserialize, Serialize};

use crate::intent::TurnIntent;
use crate::provider::ProviderKey;

/// The stable name and semantics of the units a provider enforces.
///
/// This is deliberately an opaque persisted string rather than an enum. A
/// provider upgrade can introduce a genuinely different meter without making
/// old ledger rows unreadable; exact equality is the restart fence that keeps
/// a queued turn from silently changing units before it runs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MeterId(String);

impl MeterId {
    /// A stable meter identifier owned by an adapter.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The persisted identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where a provider observes an enforced ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnforcementGranularity {
    /// The provider will not knowingly cross the configured value.
    Exact,
    /// The provider checks after complete model responses. Responses already
    /// in flight can cross the configured value before work stops; providers
    /// with concurrent root/subagent responses can therefore overshoot by
    /// more than one response.
    ProviderResponseBoundary,
}

/// How cached input contributes to a provider-native meter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTreatment {
    /// The meter is not token-based, so cache accounting does not apply.
    NotApplicable,
    /// Cached input contributes to the meter.
    Included,
    /// Cached input does not contribute to the meter.
    Excluded,
    /// Provider-reported opaque units take precedence; when they are absent,
    /// the provider's token fallback excludes cached input.
    ProviderDefinedWithExcludedFallback,
}

/// One enforceable provider-native ceiling, including its unit semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CeilingCapability {
    /// Stable identifier for the provider-native unit.
    pub meter: MeterId,
    /// When enforcement observes the counter.
    pub granularity: EnforcementGranularity,
    /// Whether cached input contributes to the counter.
    pub cache_treatment: CacheTreatment,
}

/// One thing a [`TurnIntent`] can ask for that a provider might not
/// have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Constraint {
    /// Sealing the turn off from ambient provider configuration.
    Isolation,
    /// Keeping the provider's configuration and login in a directory of
    /// our choosing, rather than the operator's own.
    CredentialIsolation,
    /// Confining filesystem writes and network reach the way
    /// [`Sandbox`](crate::intent::Sandbox) asks.
    Sandbox,
    /// Restricting the turn to a named set of MCP endpoints.
    ScopedMcp,
    /// Restricting the turn to that set and no other.
    StrictMcp,
    /// Restricting the agent to a named set of tools.
    AllowedTools,
    /// Naming the conversation before it exists, so a crash mid-turn
    /// leaves something resumable.
    ClientAssignedResume,
    /// A ceiling on provider-internal turns.
    MaxProviderTurns,
    /// An enforceable ceiling on provider work for this turn.
    TurnCeiling,
    /// An effort level.
    Effort,
}

impl Constraint {
    /// Whether dropping this constraint would widen what the agent can
    /// reach, see, or spend credentials on.
    ///
    /// Security constraints fail the turn. Non-security constraints usually
    /// warn; [`Constraint::TurnCeiling`] is the explicit spend-boundary
    /// exception in [`Self::severity`]. Kept as code so that a new variant
    /// cannot be added without answering the authority question, which a doc
    /// comment would have let slide.
    pub fn security(&self) -> bool {
        match self {
            Constraint::Isolation
            | Constraint::CredentialIsolation
            | Constraint::Sandbox
            | Constraint::ScopedMcp
            | Constraint::StrictMcp
            | Constraint::AllowedTools => true,
            // Naming the conversation up front is durability, not
            // authority: a provider without it simply learns the id
            // later, which is where ciacola started.
            Constraint::ClientAssignedResume
            | Constraint::MaxProviderTurns
            | Constraint::TurnCeiling
            | Constraint::Effort => false,
        }
    }

    /// How to react to this constraint going unhonoured.
    pub fn severity(&self) -> Severity {
        match self {
            // This is a spend boundary rather than an authority boundary, but
            // silently dropping it would still run work the caller explicitly
            // refused to authorize.
            Constraint::TurnCeiling => Severity::Fail,
            constraint if constraint.security() => Severity::Fail,
            _ => Severity::Warn,
        }
    }
}

/// What to do about a constraint the provider cannot honour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Refuse the turn. Running it would silently grant more authority or
    /// provider spend than was authorized.
    Fail,
    /// Say so on the way past. The run is worse than asked for, not
    /// wider.
    Warn,
}

/// One constraint this provider cannot honour, and what that means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    /// Which constraint went unhonoured.
    pub constraint: Constraint,
    /// Fail or warn.
    pub severity: Severity,
    /// A sentence for whoever has to fix it.
    pub detail: String,
}

/// The result of checking an intent against a provider's capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Validation {
    /// Everything the provider cannot honour, in declaration order.
    pub unsupported: Vec<Unsupported>,
}

impl Validation {
    /// The first constraint that must stop the turn, if any. Callers
    /// turn this into
    /// [`AgentError::Unsupported`](crate::AgentError::Unsupported).
    pub fn blocking(&self) -> Option<&Unsupported> {
        self.unsupported
            .iter()
            .find(|u| u.severity == Severity::Fail)
    }

    /// Everything worth saying out loud but not worth refusing over.
    pub fn warnings(&self) -> impl Iterator<Item = &Unsupported> {
        self.unsupported
            .iter()
            .filter(|u| u.severity == Severity::Warn)
    }
}

/// What one provider can honour.
///
/// Declared by the adapter, so the answer lives beside the code that
/// would have to implement it. Every field is "can this backend do the
/// thing at all", never "should it this time".
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Capabilities {
    /// Which provider this describes.
    pub provider: ProviderKey,
    /// Accepts a conversation id we chose, before the conversation
    /// exists.
    pub client_assigned_resume: bool,
    /// Can be sealed off from ambient configuration.
    pub isolation: bool,
    /// Keeps its configuration and login where we tell it to.
    pub credential_isolation: bool,
    /// Can confine filesystem writes and network reach the way
    /// [`Sandbox`](crate::intent::Sandbox) asks. `false` for any
    /// adapter whose containment is a permission prompt rather than an
    /// OS-level boundary -- Claude's `claude` CLI included.
    pub sandbox: bool,
    /// Takes a set of MCP endpoints.
    pub scoped_mcp: bool,
    /// Takes that set as exclusive.
    pub strict_mcp: bool,
    /// Enforces a tool grant.
    pub allowed_tools: bool,
    /// Takes a ceiling on provider-internal turns.
    pub max_provider_turns: bool,
    /// The provider-native per-turn ceiling this exact adapter/runtime can
    /// enforce. `None` means it must not accept a [`TurnCeiling`].
    ///
    /// [`TurnCeiling`]: crate::intent::TurnCeiling
    pub turn_ceiling: Option<CeilingCapability>,
    /// Takes an effort level.
    pub effort: bool,
    /// Reports money. `false` is the reason [`Cost::NotPriced`] exists:
    /// it means "never here" rather than "not this time".
    ///
    /// [`Cost::NotPriced`]: crate::Cost::NotPriced
    pub reports_cost: bool,
    /// Reports token counts.
    pub reports_token_usage: bool,
    /// Counts its own internal turns.
    pub reports_provider_turns: bool,
}

impl Capabilities {
    /// A provider that can do nothing yet. Adapters start here and set
    /// what they have, so a capability added to this struct later
    /// defaults to absent rather than silently claimed.
    pub fn none(provider: ProviderKey) -> Self {
        Self {
            provider,
            client_assigned_resume: false,
            isolation: false,
            credential_isolation: false,
            sandbox: false,
            scoped_mcp: false,
            strict_mcp: false,
            allowed_tools: false,
            max_provider_turns: false,
            turn_ceiling: None,
            effort: false,
            reports_cost: false,
            reports_token_usage: false,
            reports_provider_turns: false,
        }
    }

    /// Everything in `intent` that this provider cannot honour.
    ///
    /// Only constraints the intent actually *asks for* are reported. A
    /// turn that inherits ambient configuration is not asking to be
    /// isolated, so a provider without isolation is fine for it, and
    /// reporting otherwise would train operators to ignore the list.
    pub fn validate(&self, intent: &TurnIntent) -> Validation {
        let mut unsupported = Vec::new();
        let mut miss = |constraint: Constraint, detail: String| {
            unsupported.push(Unsupported {
                constraint,
                severity: constraint.severity(),
                detail,
            });
        };
        let who = self.provider.as_str().to_string();

        if intent.isolation.is_sealed() && !self.isolation {
            miss(
                Constraint::Isolation,
                format!(
                    "provider '{who}' cannot seal a turn off from ambient configuration, \
                     and this agent asked to be sealed"
                ),
            );
        }
        if (intent.config_home.is_some() || intent.token_env.is_some())
            && !self.credential_isolation
        {
            miss(
                Constraint::CredentialIsolation,
                format!(
                    "provider '{who}' cannot keep its configuration and login in a \
                     directory of our choosing, so this agent would authenticate as \
                     the operator"
                ),
            );
        }
        if intent.sandbox.is_constrained() && !self.sandbox {
            miss(
                Constraint::Sandbox,
                format!(
                    "provider '{who}' cannot confine filesystem writes or network reach, \
                     and this agent asked to be sandboxed"
                ),
            );
        }
        if let Some(mcp) = &intent.mcp {
            if !self.scoped_mcp {
                miss(
                    Constraint::ScopedMcp,
                    format!("provider '{who}' takes no MCP endpoint configuration"),
                );
            } else if mcp.strict && !self.strict_mcp {
                miss(
                    Constraint::StrictMcp,
                    format!(
                        "provider '{who}' cannot treat an endpoint list as exclusive, so \
                         this agent could reach servers it was not granted"
                    ),
                );
            }
        }
        if intent.allowed_tools.is_some() && !self.allowed_tools {
            miss(
                Constraint::AllowedTools,
                format!(
                    "provider '{who}' does not enforce a tool grant, so this agent \
                     would hold more than it was given"
                ),
            );
        }
        if intent
            .resume
            .as_ref()
            .is_some_and(|r| !r.is_open() && !self.client_assigned_resume)
        {
            miss(
                Constraint::ClientAssignedResume,
                format!(
                    "provider '{who}' names its own conversations, so the id assigned \
                     before this turn will be replaced by the one it reports"
                ),
            );
        }
        if intent.max_provider_turns.is_some() && !self.max_provider_turns {
            miss(
                Constraint::MaxProviderTurns,
                format!("provider '{who}' takes no ceiling on internal turns"),
            );
        }
        if let Some(requested) = &intent.turn_ceiling {
            match &self.turn_ceiling {
                None => miss(
                    Constraint::TurnCeiling,
                    format!("provider '{who}' cannot enforce a per-turn ceiling with this runtime"),
                ),
                Some(available) if available != &requested.capability => miss(
                    Constraint::TurnCeiling,
                    format!(
                        "provider '{who}' now advertises ceiling capability '{}' but this turn was admitted with capability '{}'; meter granularity or cache semantics drifted, so execution is refused before launch",
                        available.meter.as_str(),
                        requested.capability.meter.as_str()
                    ),
                ),
                Some(_) => {}
            }
        }
        if intent.effort.is_some() && !self.effort {
            miss(
                Constraint::Effort,
                format!("provider '{who}' has no effort setting; its default is used"),
            );
        }

        Validation { unsupported }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::{Effort, Isolation, McpEndpoint, McpScope, ResumeId, Sandbox};

    fn poor() -> Capabilities {
        Capabilities::none(ProviderKey::new("poor"))
    }

    /// The rule, pinned: dropping a security constraint stops the turn;
    /// dropping a comfort one does not.
    #[test]
    fn security_constraints_fail_and_the_rest_warn() {
        for c in [
            Constraint::Isolation,
            Constraint::CredentialIsolation,
            Constraint::Sandbox,
            Constraint::ScopedMcp,
            Constraint::StrictMcp,
            Constraint::AllowedTools,
        ] {
            assert_eq!(c.severity(), Severity::Fail, "{c:?} is a boundary");
        }
        for c in [
            Constraint::ClientAssignedResume,
            Constraint::MaxProviderTurns,
            Constraint::Effort,
        ] {
            assert_eq!(c.severity(), Severity::Warn, "{c:?} is not a boundary");
        }
    }

    /// A provider-work ceiling is a blocking spend boundary without
    /// being mislabeled as an authority/security boundary.
    #[test]
    fn a_turn_ceiling_fails_but_is_not_security() {
        assert!(!Constraint::TurnCeiling.security());
        assert_eq!(Constraint::TurnCeiling.severity(), Severity::Fail);
    }

    /// A turn asking to be sealed off must not run wide open just
    /// because the backend cannot seal it.
    #[test]
    fn an_unsupported_isolation_request_blocks_the_turn() {
        let mut intent = TurnIntent::new("go");
        intent.isolation = Isolation::Full;
        let v = poor().validate(&intent);
        let blocking = v.blocking().expect("isolation must block");
        assert_eq!(blocking.constraint, Constraint::Isolation);
    }

    /// Asking to inherit is asking for nothing, so a provider without
    /// isolation is fine for it. Reporting otherwise trains operators
    /// to ignore the list.
    #[test]
    fn inheriting_ambient_config_is_not_an_unsupported_request() {
        let intent = TurnIntent::new("go");
        assert!(poor().validate(&intent).unsupported.is_empty());
    }

    #[test]
    fn a_ceiling_requires_the_exact_admitted_capability() {
        let weighted = CeilingCapability {
            meter: MeterId::new("codex.weighted.v1"),
            granularity: EnforcementGranularity::ProviderResponseBoundary,
            cache_treatment: CacheTreatment::Excluded,
        };
        let provider_units = CeilingCapability {
            meter: MeterId::new("codex.provider-units.v1"),
            granularity: EnforcementGranularity::ProviderResponseBoundary,
            cache_treatment: CacheTreatment::ProviderDefinedWithExcludedFallback,
        };
        let mut intent = TurnIntent::new("go");
        intent.turn_ceiling = Some(crate::TurnCeiling {
            capability: weighted.clone(),
            limit: 10_000,
        });

        assert_eq!(
            poor().validate(&intent).blocking().map(|u| u.constraint),
            Some(Constraint::TurnCeiling)
        );

        let mut exact = poor();
        exact.turn_ceiling = Some(weighted);
        assert!(exact.validate(&intent).blocking().is_none());

        let mut drifted = poor();
        drifted.turn_ceiling = Some(provider_units);
        assert_eq!(
            drifted.validate(&intent).blocking().map(|u| u.constraint),
            Some(Constraint::TurnCeiling)
        );
    }

    /// Inheriting the provider's tool policy and explicitly granting no
    /// tools are different requests. The latter is a security boundary
    /// even though its inner list is empty.
    #[test]
    fn an_explicitly_toolless_turn_requires_tool_policy_support() {
        let mut intent = TurnIntent::new("go");
        intent.allowed_tools = Some(Vec::new());
        let v = poor().validate(&intent);
        assert_eq!(
            v.blocking().map(|u| u.constraint),
            Some(Constraint::AllowedTools)
        );

        let inherited = TurnIntent::new("go");
        assert!(poor().validate(&inherited).unsupported.is_empty());
    }

    /// The concrete case this constraint exists for: a provider whose
    /// containment is a permission prompt, not an OS-level sandbox --
    /// which today describes every adapter this crate ships with --
    /// must refuse a turn that asked to be sandboxed rather than run it
    /// wide open under the name of a security feature it does not have.
    #[test]
    fn an_unsupported_sandbox_request_blocks_the_turn() {
        let mut intent = TurnIntent::new("go");
        intent.sandbox = Sandbox::WorkspaceWriteNoNetwork;
        let v = poor().validate(&intent);
        let blocking = v.blocking().expect("sandbox must block");
        assert_eq!(blocking.constraint, Constraint::Sandbox);
    }

    /// An unconstrained turn asks nothing of the provider's sandboxing,
    /// so the absence of that capability is not a reason to refuse it.
    #[test]
    fn an_unconstrained_turn_does_not_require_sandbox_support() {
        let intent = TurnIntent::new("go");
        assert!(poor().validate(&intent).unsupported.is_empty());

        let mut caps = poor();
        caps.sandbox = true;
        assert!(caps.validate(&intent).unsupported.is_empty());
    }

    /// An endpoint list that is not exclusive is not the endpoint list
    /// that was granted.
    #[test]
    fn a_non_exclusive_endpoint_list_blocks_the_turn() {
        let mut caps = poor();
        caps.scoped_mcp = true;
        let mut intent = TurnIntent::new("go");
        intent.mcp = Some(McpScope {
            endpoints: vec![McpEndpoint {
                name: "ciacola".into(),
                url: "http://127.0.0.1:4823/mcp".into(),
                headers: Default::default(),
            }],
            strict: true,
        });
        let v = caps.validate(&intent);
        assert_eq!(
            v.blocking().map(|u| u.constraint),
            Some(Constraint::StrictMcp)
        );
    }

    /// Effort and internal-turn hints degrade the run without widening an
    /// authority or spend boundary, so they are said out loud and the turn
    /// proceeds.
    #[test]
    fn missing_effort_and_internal_turn_hints_only_warn() {
        let mut intent = TurnIntent::new("go");
        intent.effort = Some(Effort::High);
        intent.max_provider_turns = Some(40);
        let v = poor().validate(&intent);
        assert!(v.blocking().is_none(), "neither is a boundary");
        assert_eq!(v.warnings().count(), 2);
    }

    /// A backend that names its own conversations is where ciacola
    /// started; it loses crash durability, not containment.
    #[test]
    fn a_provider_that_names_its_own_conversations_only_warns() {
        let mut intent = TurnIntent::new("go");
        intent.resume = Some(ResumeId::ClientAssigned("chosen".into()));
        let v = poor().validate(&intent);
        assert!(v.blocking().is_none());
        assert_eq!(
            v.warnings().next().map(|u| u.constraint),
            Some(Constraint::ClientAssignedResume)
        );
    }

    /// Resuming a conversation the backend already knows asks nothing
    /// of it.
    #[test]
    fn resuming_an_open_conversation_asks_for_nothing() {
        let mut intent = TurnIntent::new("go");
        intent.resume = Some(ResumeId::ProviderAssigned("theirs".into()));
        assert!(poor().validate(&intent).unsupported.is_empty());
    }
}
