//! Provider-neutral policy for a future isolated delegation boundary.
//!
//! This module deliberately does not create a transport, principal, or
//! authorization decision. It describes the small policy an independently
//! attested backend may eventually recognize. Until that backend and its
//! ledger-derived preflight exist, [`DelegationBackendStatus::Unavailable`] is
//! the only representable backend state.
//!
//! Three constraints are structural rather than conventions:
//!
//! - the action vocabulary is closed and contains no general operator action;
//! - a policy applies only to repo-worker assignments descended from the
//!   future broker-derived principal; and
//! - delegation is never inherited by a child agent.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Deserializer, de};

/// An operator-side effect that the isolated delegation channel may expose.
///
/// These are intentionally qualified by plugin. Adding a variant is a
/// security-policy change: unknown names, wildcards, and ordinary operator
/// tools are rejected during deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
pub enum DelegatableAction {
    #[serde(rename = "repo-worker/open_pr")]
    RepoWorkerOpenPr,
    #[serde(rename = "repo-worker/finish_issue")]
    RepoWorkerFinishIssue,
}

impl DelegatableAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepoWorkerOpenPr => "repo-worker/open_pr",
            Self::RepoWorkerFinishIssue => "repo-worker/finish_issue",
        }
    }
}

impl fmt::Display for DelegatableAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The parameter boundary applied in addition to the action allowlist.
///
/// There is deliberately no `AnyAssignment` variant. The assignment store
/// must establish ancestry from trusted state; a request parameter cannot
/// assert that it is a descendant.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationScope {
    #[default]
    DescendantAssignments,
}

impl DelegationScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DescendantAssignments => "descendant_assignments",
        }
    }
}

/// Delegated authority is usable only on the isolated broker channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationSurface {
    IsolatedBroker,
}

/// Delegated authority never follows the principal into another Ciacola agent
/// or role. A shell subprocess inside one attested supervisor sandbox is part
/// of that same launch principal; it is not a separately spawned agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegationInheritance {
    Never,
}

/// An immutable, normalized delegation policy.
///
/// Construction and deserialization reject empty and duplicate action lists.
/// Internally the exact allowlist is sorted, which gives an order-independent
/// semantic value. It is deliberately not an attestation encoding: the real
/// backend contract must bind policy together with the manager, grant version,
/// launch epoch, revocation state, and backend instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationPolicy {
    actions: BTreeSet<DelegatableAction>,
    scope: DelegationScope,
}

impl DelegationPolicy {
    /// Build the supported scope with a non-empty exact action allowlist.
    pub fn new(
        actions: impl IntoIterator<Item = DelegatableAction>,
    ) -> Result<Self, DelegationPolicyError> {
        Self::scoped(actions, DelegationScope::DescendantAssignments)
    }

    /// Build a policy with an explicit parameter scope.
    pub fn scoped(
        actions: impl IntoIterator<Item = DelegatableAction>,
        scope: DelegationScope,
    ) -> Result<Self, DelegationPolicyError> {
        let mut normalized = BTreeSet::new();
        for action in actions {
            if !normalized.insert(action) {
                return Err(DelegationPolicyError::DuplicateAction(action));
            }
        }
        if normalized.is_empty() {
            return Err(DelegationPolicyError::EmptyActions);
        }
        Ok(Self {
            actions: normalized,
            scope,
        })
    }

    /// The exact action allowlist. Iteration order is normalized.
    pub fn actions(&self) -> &BTreeSet<DelegatableAction> {
        &self.actions
    }

    pub const fn scope(&self) -> DelegationScope {
        self.scope
    }

    pub const fn surface(&self) -> DelegationSurface {
        DelegationSurface::IsolatedBroker
    }

    pub const fn inheritance(&self) -> DelegationInheritance {
        DelegationInheritance::Never
    }

    pub fn contains(&self, action: DelegatableAction) -> bool {
        self.actions.contains(&action)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegationPolicyWire {
    actions: Vec<DelegatableAction>,
    #[serde(default)]
    scope: DelegationScope,
}

impl<'de> Deserialize<'de> for DelegationPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = DelegationPolicyWire::deserialize(deserializer)?;
        Self::scoped(wire.actions, wire.scope).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegationPolicyError {
    EmptyActions,
    DuplicateAction(DelegatableAction),
}

impl fmt::Display for DelegationPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyActions => f.write_str("delegation actions must not be empty"),
            Self::DuplicateAction(action) => {
                write!(f, "delegation action '{action}' appears more than once")
            }
        }
    }
}

impl std::error::Error for DelegationPolicyError {}

/// Backend availability reserved for the future provenance boundary.
///
/// There is intentionally no public way to represent an attested backend yet.
/// Adding that state must arrive with the real isolation adapter, an opaque
/// backend-derived principal, and a ledger-derived descendant preflight. A
/// transport or config value cannot manufacture availability in this PR.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DelegationBackendStatus {
    /// No backend has established an isolated, non-replayable principal. This
    /// is the only sound and representable state today.
    #[default]
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_only() -> DelegationPolicy {
        DelegationPolicy::new([DelegatableAction::RepoWorkerOpenPr]).expect("valid policy")
    }

    #[test]
    fn policy_contains_only_its_exact_allowlist() {
        let policy = open_only();
        assert!(policy.contains(DelegatableAction::RepoWorkerOpenPr));
        assert!(
            !policy.contains(DelegatableAction::RepoWorkerFinishIssue),
            "one typed action must not imply another"
        );
        assert_eq!(
            policy.actions(),
            &BTreeSet::from([DelegatableAction::RepoWorkerOpenPr])
        );
    }

    #[test]
    fn policies_are_nonempty_normalized_and_immutable() {
        assert_eq!(
            DelegationPolicy::new([]),
            Err(DelegationPolicyError::EmptyActions)
        );
        assert_eq!(
            DelegationPolicy::new([
                DelegatableAction::RepoWorkerOpenPr,
                DelegatableAction::RepoWorkerOpenPr,
            ]),
            Err(DelegationPolicyError::DuplicateAction(
                DelegatableAction::RepoWorkerOpenPr
            ))
        );

        let input = r#"{
            "actions": ["repo-worker/finish_issue", "repo-worker/open_pr"],
            "scope": "descendant_assignments"
        }"#;
        let policy: DelegationPolicy = serde_json::from_str(input).expect("valid policy");
        let reversed = DelegationPolicy::new([
            DelegatableAction::RepoWorkerOpenPr,
            DelegatableAction::RepoWorkerFinishIssue,
        ])
        .expect("valid policy");
        assert_eq!(
            policy, reversed,
            "input order must not change policy semantics"
        );
        assert_eq!(
            policy.actions().iter().copied().collect::<Vec<_>>(),
            vec![
                DelegatableAction::RepoWorkerOpenPr,
                DelegatableAction::RepoWorkerFinishIssue,
            ],
            "the normalized set has deterministic iteration without defining a wire encoding"
        );
        assert_eq!(
            serde_json::from_str::<DelegationPolicy>(
                r#"{"actions":["repo-worker/open_pr","repo-worker/open_pr"]}"#
            )
            .expect_err("duplicates must not be silently coalesced")
            .to_string(),
            "delegation action 'repo-worker/open_pr' appears more than once"
        );
    }

    #[test]
    fn unknown_and_operator_actions_cannot_widen_the_policy() {
        for action in ["*", "kill", "operator/kill", "repo-worker/start_issue"] {
            let encoded = format!(r#"{{"actions":["{action}"]}}"#);
            assert!(
                serde_json::from_str::<DelegationPolicy>(&encoded).is_err(),
                "unexpected delegated action {action}"
            );
        }

        let policy = open_only();
        assert_eq!(policy.surface(), DelegationSurface::IsolatedBroker);
        assert_eq!(policy.inheritance(), DelegationInheritance::Never);
    }

    #[test]
    fn scope_cannot_be_widened_or_claimed_by_input() {
        let policy = open_only();
        assert_eq!(
            policy.scope(),
            DelegationScope::DescendantAssignments,
            "the only representable scope is derived descendant assignments"
        );
        assert!(
            serde_json::from_str::<DelegationPolicy>(
                r#"{"actions":["repo-worker/open_pr"],"scope":"all_assignments"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn unavailable_is_the_only_constructible_backend_state() {
        assert_eq!(
            DelegationBackendStatus::default(),
            DelegationBackendStatus::Unavailable
        );
    }

    #[test]
    fn malformed_policy_documents_fail_closed() {
        for encoded in [
            r#"{"actions":[]}"#,
            r#"{"actions":["repo-worker/open_pr"],"scope":"all_assignments"}"#,
            r#"{"actions":["repo-worker/open_pr"],"inherit":true}"#,
        ] {
            assert!(serde_json::from_str::<DelegationPolicy>(encoded).is_err());
        }
    }
}
