//! The durable assignment record and its journey-state enums.

use ciacola_core::agent::FlatError;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AssignmentState {
    Preparing,
    Active,
    Finishing,
    Retained,
    Completed,
    Stale,
}

impl AssignmentState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Active => "active",
            Self::Finishing => "finishing",
            Self::Retained => "retained",
            Self::Completed => "completed",
            Self::Stale => "stale",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, FlatError> {
        match value {
            "preparing" => Ok(Self::Preparing),
            "active" => Ok(Self::Active),
            "finishing" => Ok(Self::Finishing),
            "retained" => Ok(Self::Retained),
            "completed" => Ok(Self::Completed),
            "stale" => Ok(Self::Stale),
            _ => Err(format!("invalid repo-worker assignment state '{value}'").into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PublicationState {
    Unpublished,
    Publishing,
    Published,
    Failed,
}

impl PublicationState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unpublished => "unpublished",
            Self::Publishing => "publishing",
            Self::Published => "published",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, FlatError> {
        match value {
            "unpublished" => Ok(Self::Unpublished),
            "publishing" => Ok(Self::Publishing),
            "published" => Ok(Self::Published),
            "failed" => Ok(Self::Failed),
            _ => Err(format!("invalid repo-worker publication state '{value}'").into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PrState {
    Open,
    Closed,
    Merged,
}

impl PrState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, FlatError> {
        match value {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            "merged" => Ok(Self::Merged),
            _ => Err(format!("invalid repo-worker pull-request state '{value}'").into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CleanupState {
    None,
    Retaining,
    Retained,
    Removing,
    Completed,
    Failed,
}

impl CleanupState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Retaining => "retaining",
            Self::Retained => "retained",
            Self::Removing => "removing",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, FlatError> {
        match value {
            "none" => Ok(Self::None),
            "retaining" => Ok(Self::Retaining),
            "retained" => Ok(Self::Retained),
            "removing" => Ok(Self::Removing),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            _ => Err(format!("invalid repo-worker cleanup state '{value}'").into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CleanupReason {
    Absent,
    NoChanges,
    Merged,
    Discarded,
}

impl CleanupReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::NoChanges => "no_changes",
            Self::Merged => "merged",
            Self::Discarded => "discarded",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, FlatError> {
        match value {
            "absent" => Ok(Self::Absent),
            "no_changes" => Ok(Self::NoChanges),
            "merged" => Ok(Self::Merged),
            "discarded" => Ok(Self::Discarded),
            _ => Err(format!("invalid repo-worker cleanup reason '{value}'").into()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Assignment {
    pub(crate) assignment_id: String,
    pub(crate) repo: String,
    pub(crate) issue: u64,
    pub(crate) state: AssignmentState,
    pub(crate) phase: String,
    pub(crate) base: Option<String>,
    pub(crate) base_head: Option<String>,
    pub(crate) slug: String,
    pub(crate) branch: String,
    pub(crate) branch_policy: String,
    pub(crate) worktree: String,
    pub(crate) bare_path: String,
    pub(crate) agent_id: Option<String>,
    pub(crate) related_agent_ids: Vec<String>,
    pub(crate) spawned_by: Option<String>,
    pub(crate) expected_head: Option<String>,
    pub(crate) pushed_head: Option<String>,
    pub(crate) publication_state: PublicationState,
    pub(crate) pr: Option<u64>,
    pub(crate) pr_url: Option<String>,
    pub(crate) pr_state: Option<PrState>,
    pub(crate) pr_draft: Option<bool>,
    pub(crate) pr_head: Option<String>,
    pub(crate) pr_base: Option<String>,
    pub(crate) pr_checked_unix: Option<i64>,
    pub(crate) cleanup_state: CleanupState,
    pub(crate) cleanup_head: Option<String>,
    pub(crate) cleanup_reason: Option<CleanupReason>,
    pub(crate) last_error: Option<String>,
    pub(crate) created_unix: i64,
    pub(crate) updated_unix: i64,
    pub(crate) terminal_unix: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LegacyAssignment {
    pub(crate) repo: String,
    pub(crate) issue: u64,
    pub(crate) slug: String,
    pub(crate) branch: String,
    pub(crate) worktree: String,
    pub(crate) pr: Option<u64>,
}

impl Assignment {
    pub(crate) fn from_row(row: sqlx::sqlite::SqliteRow) -> Result<Self, FlatError> {
        let issue: i64 = row.try_get("issue_number")?;
        let pr: Option<i64> = row.try_get("pr")?;
        let related: String = row.try_get("related_agent_ids")?;
        Ok(Self {
            assignment_id: row.try_get("assignment_id")?,
            repo: row.try_get("repo")?,
            issue: u64::try_from(issue).map_err(|_| "negative issue number in assignment")?,
            state: AssignmentState::parse(row.try_get("state")?)?,
            phase: row.try_get("phase")?,
            base: row.try_get("base")?,
            base_head: row.try_get("base_head")?,
            slug: row.try_get("slug")?,
            branch: row.try_get("branch")?,
            branch_policy: row.try_get("branch_policy")?,
            worktree: row.try_get("worktree")?,
            bare_path: row.try_get("bare_path")?,
            agent_id: row.try_get("agent_id")?,
            related_agent_ids: serde_json::from_str(&related)?,
            spawned_by: row.try_get("spawned_by")?,
            expected_head: row.try_get("expected_head")?,
            pushed_head: row.try_get("pushed_head")?,
            publication_state: PublicationState::parse(row.try_get("publication_state")?)?,
            pr: pr.map(u64::try_from).transpose()?,
            pr_url: row.try_get("pr_url")?,
            pr_state: row
                .try_get::<Option<&str>, _>("pr_state")?
                .map(PrState::parse)
                .transpose()?,
            pr_draft: row
                .try_get::<Option<i64>, _>("pr_draft")?
                .map(|value| value != 0),
            pr_head: row.try_get("pr_head")?,
            pr_base: row.try_get("pr_base")?,
            pr_checked_unix: row.try_get("pr_checked_unix")?,
            cleanup_state: CleanupState::parse(row.try_get("cleanup_state")?)?,
            cleanup_head: row.try_get("cleanup_head")?,
            cleanup_reason: row
                .try_get::<Option<&str>, _>("cleanup_reason")?
                .map(CleanupReason::parse)
                .transpose()?,
            last_error: row.try_get("last_error")?,
            created_unix: row.try_get("created_unix")?,
            updated_unix: row.try_get("updated_unix")?,
            terminal_unix: row.try_get("terminal_unix")?,
        })
    }

    pub(crate) fn response(&self, created: bool) -> serde_json::Value {
        json!({
            "assignment_id": self.assignment_id,
            "agent_id": self.agent_id,
            "repo": self.repo,
            "issue": self.issue,
            "state": self.state.as_str(),
            "created": created,
            "base": self.base,
            "base_head": self.base_head,
            "branch": self.branch,
            "branch_policy": self.branch_policy,
            "worktree": self.worktree,
            "expected_head": self.expected_head,
            "pushed_head": self.pushed_head,
            "publication_state": self.publication_state.as_str(),
            "pr": self.pr,
            "url": self.pr_url,
            "pr_state": self.pr_state.map(PrState::as_str),
            "pr_draft": self.pr_draft,
            "pr_head": self.pr_head,
            "pr_base": self.pr_base,
            "pr_checked_unix": self.pr_checked_unix,
            "cleanup_state": self.cleanup_state.as_str(),
            "cleanup_head": self.cleanup_head,
            "cleanup_reason": self.cleanup_reason.map(CleanupReason::as_str),
        })
    }

    pub(crate) fn conflict(&self, requested_base: Option<&str>) -> String {
        if self.state == AssignmentState::Active
            && requested_base.is_some()
            && requested_base != self.base.as_deref()
        {
            return format!(
                "assignment '{}' already uses base '{}', not requested base '{}'",
                self.assignment_id,
                self.base.as_deref().unwrap_or("<unknown>"),
                requested_base.unwrap_or_default()
            );
        }
        format!(
            "assignment '{}' for {}#{} is {}; phase '{}'; {}",
            self.assignment_id,
            self.repo,
            self.issue,
            self.state.as_str(),
            self.phase,
            self.last_error.as_deref().unwrap_or("no additional detail")
        )
    }
}

pub(crate) fn assignment_slug(repo: &str, issue: u64, assignment_id: &str) -> String {
    let readable: String = repo
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{readable}-{issue}-{}", assignment_id.to_ascii_lowercase())
}

pub(crate) fn sqlite_u64(value: u64, label: &str) -> Result<i64, FlatError> {
    i64::try_from(value)
        .map_err(|_| format!("{label} {value} exceeds SQLite's integer range").into())
}
