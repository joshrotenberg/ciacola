//! The fail-closed delegated-assignment preflight and its typed
//! requests and refusals. Runtime delegated authority stays disabled
//! until #81 picks an isolation backend.

use std::collections::HashSet;
use std::fmt;

use ciacola_core::agent::FlatError;
use ciacola_core::delegation::{DelegatableAction, DelegationPolicy};
use ciacola_core::ledger::Ledger;

use crate::RepoWorkerPlugin;
use crate::assignment::Assignment;
use crate::db::AssignmentDb;

/// A future isolated broker's narrow publication request.
///
/// Unlike the compatibility operator tool, this shape has no agent selector,
/// no optional head, and no non-draft mode. Repository, issue, branch, and
/// owner are resolved from the durable assignment during preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedOpenPrRequest {
    pub assignment_id: String,
    pub expected_head: String,
    pub title: String,
    pub body: String,
}

/// The only cleanup choices available to a future delegated supervisor.
///
/// There is deliberately no discard variant. Removing work still requires the
/// repo-worker's existing proof that it is merged or unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegatedFinishDisposition {
    Retain,
    RemoveIfMergedOrUnchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegatedFinishIssueRequest {
    pub assignment_id: String,
    pub disposition: DelegatedFinishDisposition,
}

/// Closed request vocabulary for the future isolated broker.
///
/// This is domain input, not authority. The process-isolation backend must
/// separately derive and attest the manager principal before invoking the
/// read-only eligibility preflight below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegatedAssignmentRequest {
    OpenPr(DelegatedOpenPrRequest),
    FinishIssue(DelegatedFinishIssueRequest),
}

impl DelegatedAssignmentRequest {
    pub const fn action(&self) -> DelegatableAction {
        match self {
            Self::OpenPr(_) => DelegatableAction::RepoWorkerOpenPr,
            Self::FinishIssue(_) => DelegatableAction::RepoWorkerFinishIssue,
        }
    }

    pub fn assignment_id(&self) -> &str {
        match self {
            Self::OpenPr(request) => &request.assignment_id,
            Self::FinishIssue(request) => &request.assignment_id,
        }
    }
}

/// Durable facts established for an attested manager before a delegated
/// repo-worker action may enter the existing publication or cleanup fences.
///
/// This proof does not itself authorize an external effect. It intentionally
/// carries no bearer, backend identity, grant epoch, or caller-supplied
/// ancestry claim. It is not cloneable or serializable and must be recomputed
/// at the future action convergence point rather than persisted or replayed
/// after assignment, manager, or grant state changes.
#[derive(Debug, PartialEq, Eq)]
pub struct DelegatedAssignmentEligibility {
    manager_agent_id: String,
    assignment_id: String,
    owner_agent_id: String,
    action: DelegatableAction,
    creator_hops: usize,
    owner_hops: usize,
}

impl DelegatedAssignmentEligibility {
    pub fn manager_agent_id(&self) -> &str {
        &self.manager_agent_id
    }

    pub fn assignment_id(&self) -> &str {
        &self.assignment_id
    }

    pub fn owner_agent_id(&self) -> &str {
        &self.owner_agent_id
    }

    pub const fn action(&self) -> DelegatableAction {
        self.action
    }

    /// Distance from the manager to the agent that reserved the assignment.
    /// Zero means the manager called `start_issue` directly.
    pub const fn creator_hops(&self) -> usize {
        self.creator_hops
    }

    /// Distance from the manager to the assignment's current implementer.
    pub const fn owner_hops(&self) -> usize {
        self.owner_hops
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegatedLineageRefusal {
    MissingAgent {
        agent_id: String,
    },
    OutsideManager {
        agent_id: String,
        manager_agent_id: String,
    },
    Cycle {
        agent_id: String,
    },
    TooDeep,
}

impl fmt::Display for DelegatedLineageRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingAgent { agent_id } => {
                write!(f, "lineage agent '{agent_id}' is missing")
            }
            Self::OutsideManager {
                agent_id,
                manager_agent_id,
            } => write!(
                f,
                "agent '{agent_id}' is not descended from manager '{manager_agent_id}'"
            ),
            Self::Cycle { agent_id } => {
                write!(f, "lineage contains a cycle at agent '{agent_id}'")
            }
            Self::TooDeep => f.write_str("lineage exceeds the 64-agent safety bound"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegatedLineageSubject {
    AssignmentCreator,
    AssignmentOwner,
}

impl fmt::Display for DelegatedLineageSubject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AssignmentCreator => f.write_str("assignment creator"),
            Self::AssignmentOwner => f.write_str("assignment owner"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegatedAssignmentRefusal {
    PluginUnavailable,
    ActionNotGranted {
        action: DelegatableAction,
    },
    ManagerNotFound {
        agent_id: String,
    },
    ManagerRetired {
        agent_id: String,
    },
    AssignmentNotFound {
        assignment_id: String,
    },
    AssignmentCreatorMissing {
        assignment_id: String,
    },
    AssignmentOwnerMissing {
        assignment_id: String,
    },
    AmbiguousOwners {
        assignment_id: String,
        owners: Vec<String>,
    },
    Lineage {
        subject: DelegatedLineageSubject,
        reason: DelegatedLineageRefusal,
    },
    Ledger {
        reason: String,
    },
    Assignments {
        reason: String,
    },
}

impl fmt::Display for DelegatedAssignmentRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PluginUnavailable => f.write_str("repo-worker is not set up"),
            Self::ActionNotGranted { action } => {
                write!(f, "delegation policy does not grant '{action}'")
            }
            Self::ManagerNotFound { agent_id } => {
                write!(f, "manager agent '{agent_id}' is missing")
            }
            Self::ManagerRetired { agent_id } => {
                write!(f, "manager agent '{agent_id}' is retired")
            }
            Self::AssignmentNotFound { assignment_id } => {
                write!(f, "assignment '{assignment_id}' is missing")
            }
            Self::AssignmentCreatorMissing { assignment_id } => write!(
                f,
                "assignment '{assignment_id}' has no durable creator lineage"
            ),
            Self::AssignmentOwnerMissing { assignment_id } => {
                write!(f, "assignment '{assignment_id}' has no current owner")
            }
            Self::AmbiguousOwners {
                assignment_id,
                owners,
            } => write!(
                f,
                "assignment '{assignment_id}' has ambiguous owners [{}]",
                owners.join(", ")
            ),
            Self::Lineage { subject, reason } => {
                write!(f, "{subject} lineage refused: {reason}")
            }
            Self::Ledger { reason } => write!(f, "cannot read agent lineage: {reason}"),
            Self::Assignments { reason } => {
                write!(f, "cannot read durable assignment: {reason}")
            }
        }
    }
}

impl std::error::Error for DelegatedAssignmentRefusal {}
pub(crate) async fn delegated_lineage_hops(
    ledger: &Ledger,
    agent_id: &str,
    manager_agent_id: &str,
    allow_manager_itself: bool,
) -> Result<usize, DelegatedLineageCheckError> {
    let mut current = agent_id.to_string();
    let mut visited = HashSet::new();
    let mut manager_hops = None;

    for hops in 0..=64 {
        if !visited.insert(current.clone()) {
            return Err(DelegatedLineageCheckError::Refusal(
                DelegatedLineageRefusal::Cycle { agent_id: current },
            ));
        }
        let row = ledger
            .get_agent(&current)
            .await
            .map_err(|error| DelegatedLineageCheckError::Ledger(error.to_string()))?
            .ok_or_else(|| {
                DelegatedLineageCheckError::Refusal(DelegatedLineageRefusal::MissingAgent {
                    agent_id: current.clone(),
                })
            })?;

        if current == manager_agent_id && (hops > 0 || allow_manager_itself) {
            manager_hops = Some(hops);
        }

        let Some(parent) = row.spawned_by else {
            return manager_hops.ok_or_else(|| {
                DelegatedLineageCheckError::Refusal(DelegatedLineageRefusal::OutsideManager {
                    agent_id: agent_id.to_string(),
                    manager_agent_id: manager_agent_id.to_string(),
                })
            });
        };
        current = parent;
    }

    Err(DelegatedLineageCheckError::Refusal(
        DelegatedLineageRefusal::TooDeep,
    ))
}

pub(crate) enum DelegatedLineageCheckError {
    Refusal(DelegatedLineageRefusal),
    Ledger(String),
}
impl RepoWorkerPlugin {
    pub(crate) fn assignment_db(&self) -> Option<AssignmentDb> {
        Some(AssignmentDb::new(self.ctx.as_ref()?.pool.clone()))
    }

    pub(crate) async fn assignments(&self) -> Result<Vec<Assignment>, FlatError> {
        match self.assignment_db() {
            Some(db) => db.list().await,
            None => Ok(Vec::new()),
        }
    }

    /// Verify the durable assignment and lineage facts required by the future
    /// isolated broker.
    ///
    /// `manager_agent_id` is not treated as provenance. The caller must already
    /// hold a backend-attested principal bound to that manager and policy. This
    /// method is deliberately read-only and cannot publish, clean up, or make a
    /// backend available; it only prevents the future broker from trusting
    /// request prose or incomplete assignment metadata.
    pub async fn preflight_delegated_assignment(
        &self,
        manager_agent_id: &str,
        policy: &DelegationPolicy,
        request: &DelegatedAssignmentRequest,
    ) -> Result<DelegatedAssignmentEligibility, DelegatedAssignmentRefusal> {
        let Some(ctx) = self.ctx.as_ref() else {
            return Err(DelegatedAssignmentRefusal::PluginUnavailable);
        };
        let action = request.action();
        if !policy.contains(action) {
            return Err(DelegatedAssignmentRefusal::ActionNotGranted { action });
        }

        let manager = ctx
            .ledger
            .get_agent(manager_agent_id)
            .await
            .map_err(|error| DelegatedAssignmentRefusal::Ledger {
                reason: error.to_string(),
            })?
            .ok_or_else(|| DelegatedAssignmentRefusal::ManagerNotFound {
                agent_id: manager_agent_id.to_string(),
            })?;
        if manager.retired {
            return Err(DelegatedAssignmentRefusal::ManagerRetired {
                agent_id: manager_agent_id.to_string(),
            });
        }

        let assignment_id = request.assignment_id();
        let assignment = AssignmentDb::new(ctx.pool.clone())
            .get_by_id(assignment_id)
            .await
            .map_err(|error| DelegatedAssignmentRefusal::Assignments {
                reason: error.to_string(),
            })?
            .ok_or_else(|| DelegatedAssignmentRefusal::AssignmentNotFound {
                assignment_id: assignment_id.to_string(),
            })?;

        let creator = assignment.spawned_by.as_deref().ok_or_else(|| {
            DelegatedAssignmentRefusal::AssignmentCreatorMissing {
                assignment_id: assignment_id.to_string(),
            }
        })?;
        let creator_hops = delegated_lineage_hops(&ctx.ledger, creator, manager_agent_id, true)
            .await
            .map_err(|refusal| match refusal {
                DelegatedLineageCheckError::Refusal(reason) => {
                    DelegatedAssignmentRefusal::Lineage {
                        subject: DelegatedLineageSubject::AssignmentCreator,
                        reason,
                    }
                }
                DelegatedLineageCheckError::Ledger(reason) => {
                    DelegatedAssignmentRefusal::Ledger { reason }
                }
            })?;

        let owner = assignment.agent_id.as_deref().ok_or_else(|| {
            DelegatedAssignmentRefusal::AssignmentOwnerMissing {
                assignment_id: assignment_id.to_string(),
            }
        })?;
        let mut related = assignment.related_agent_ids.clone();
        related.sort();
        related.dedup();
        if related.len() != 1 || related.first().is_none_or(|known| known != owner) {
            if !related.iter().any(|known| known == owner) {
                related.push(owner.to_string());
                related.sort();
            }
            return Err(DelegatedAssignmentRefusal::AmbiguousOwners {
                assignment_id: assignment_id.to_string(),
                owners: related,
            });
        }

        let owner_hops = delegated_lineage_hops(&ctx.ledger, owner, manager_agent_id, false)
            .await
            .map_err(|refusal| match refusal {
                DelegatedLineageCheckError::Refusal(reason) => {
                    DelegatedAssignmentRefusal::Lineage {
                        subject: DelegatedLineageSubject::AssignmentOwner,
                        reason,
                    }
                }
                DelegatedLineageCheckError::Ledger(reason) => {
                    DelegatedAssignmentRefusal::Ledger { reason }
                }
            })?;

        Ok(DelegatedAssignmentEligibility {
            manager_agent_id: manager_agent_id.to_string(),
            assignment_id: assignment_id.to_string(),
            owner_agent_id: owner.to_string(),
            action,
            creator_hops,
            owner_hops,
        })
    }
}
