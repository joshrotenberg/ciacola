//! Publication and cleanup: pull requests, exact-head pushes, and
//! the guarded teardown of finished assignments.

use std::path::Path;

use ciacola_core::agent::FlatError;
use ciacola_core::plugin::PluginContext;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::OpenPrArgs;
use crate::assignment::{
    Assignment, AssignmentState, CleanupReason, CleanupState, PrState, PublicationState,
};
use crate::db::AssignmentDb;
use crate::git::{gh, git_output, git_predicate, github_repo, worktree_is_clean};
use crate::repos::{Repos, WorktreeSnapshot};

/// Is this a conventional-commit title: `type(scope)!: subject`?
///
/// Enforced mechanically in `open_pr` rather than only asked for in the
/// prompt, because the title is the one piece of an agent's writing
/// that lands on GitHub verbatim, and a guard that is only a request
/// is not a guard. Scope and `!` are optional; the type is the closed
/// set below; the subject must be non-empty.
/// The refusal text both publication paths emit for a bad title.
fn not_conventional_title(title: &str) -> String {
    format!(
        "title '{title}' is not conventional-commit form. Use type(scope): subject, e.g. \
         'fix: ...' or 'feat(board): ...'; types are build, chore, ci, docs, feat, fix, \
         perf, refactor, revert, style, test."
    )
}

pub(crate) fn conventional_title(title: &str) -> bool {
    const TYPES: &[&str] = &[
        "build", "chore", "ci", "docs", "feat", "fix", "perf", "refactor", "revert", "style",
        "test",
    ];
    let Some((prefix, subject)) = title.split_once(':') else {
        return false;
    };
    if subject.trim().is_empty() {
        return false;
    }
    let prefix = prefix.strip_suffix('!').unwrap_or(prefix);
    let ty = match prefix.split_once('(') {
        Some((ty, scope)) => {
            if !scope.ends_with(')') || scope.len() < 2 {
                return false;
            }
            ty
        }
        None => prefix,
    };
    TYPES.contains(&ty)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GhPr {
    pub(crate) number: u64,
    pub(crate) url: String,
    pub(crate) state: String,
    pub(crate) is_draft: bool,
    pub(crate) head_ref_name: String,
    pub(crate) head_ref_oid: String,
    pub(crate) base_ref_name: String,
    #[serde(default)]
    pub(crate) is_cross_repository: bool,
    #[serde(default)]
    pub(crate) merged_at: Option<String>,
}

impl GhPr {
    pub(crate) fn parsed_state(&self) -> Result<PrState, FlatError> {
        if self.merged_at.is_some() || self.state.eq_ignore_ascii_case("merged") {
            return Ok(PrState::Merged);
        }
        if self.state.eq_ignore_ascii_case("open") {
            Ok(PrState::Open)
        } else if self.state.eq_ignore_ascii_case("closed") {
            Ok(PrState::Closed)
        } else {
            Err(format!(
                "pull request #{} has unknown state '{}'",
                self.number, self.state
            )
            .into())
        }
    }
}

pub(crate) const GH_PR_FIELDS: &str =
    "number,url,state,isDraft,headRefName,headRefOid,baseRefName,isCrossRepository,mergedAt";
pub(crate) async fn record_validated_pr(
    assignments: &AssignmentDb,
    assignment: &Assignment,
    pr: &GhPr,
) -> Result<(), FlatError> {
    let identity = validate_pr_identity(assignment, pr);
    let expected_mismatch = assignment
        .expected_head
        .as_deref()
        .filter(|expected| *expected != pr.head_ref_oid);
    let retry_baseline = identity.is_ok()
        && expected_mismatch.is_some()
        && matches!(
            assignment.publication_state,
            PublicationState::Publishing | PublicationState::Failed
        )
        && assignment.pr_head.as_deref() == Some(pr.head_ref_oid.as_str());
    let validation = identity.and_then(|()| {
        if let Some(expected) = expected_mismatch
            && !retry_baseline
        {
            return Err(format!(
                "pull request #{} head {} drifted from durable expected head {expected}",
                pr.number, pr.head_ref_oid
            )
            .into());
        }
        Ok(())
    });
    let message = validation
        .as_ref()
        .err()
        .map(ToString::to_string)
        .or_else(|| {
            retry_baseline.then(|| {
                format!(
                    "publication update to expected head {} is pending; pull request #{} remains at {}",
                    assignment.expected_head.as_deref().unwrap_or("<unknown>"),
                    pr.number,
                    pr.head_ref_oid
                )
            })
        });
    assignments
        .record_pr_observation(&assignment.assignment_id, pr, message.as_deref())
        .await?;
    validation
}

pub(crate) async fn discover_pr(
    repos: &Repos,
    assignment: &Assignment,
) -> Result<Option<GhPr>, FlatError> {
    let repository = github_repo(&assignment.repo);
    if let Some(number) = assignment.pr {
        let number = number.to_string();
        let output = gh(
            &repos.gh_binary,
            None,
            &[
                "pr",
                "view",
                &number,
                "--repo",
                &repository,
                "--json",
                GH_PR_FIELDS,
            ],
        )
        .await?;
        return Ok(Some(serde_json::from_str(&output).map_err(
            |e| -> FlatError { format!("cannot parse pull request #{number}: {e}").into() },
        )?));
    }

    let output = gh(
        &repos.gh_binary,
        None,
        &[
            "pr",
            "list",
            "--repo",
            &repository,
            "--head",
            &assignment.branch,
            "--state",
            "all",
            "--limit",
            "100",
            "--json",
            GH_PR_FIELDS,
        ],
    )
    .await?;
    let candidates: Vec<GhPr> = serde_json::from_str(&output)
        .map_err(|e| -> FlatError { format!("cannot parse pull request list: {e}").into() })?;
    if candidates.is_empty() {
        return Ok(None);
    }
    let mut same_repo: Vec<GhPr> = candidates
        .into_iter()
        .filter(|pr| !pr.is_cross_repository && pr.head_ref_name == assignment.branch)
        .collect();
    if same_repo.is_empty() {
        return Err(format!(
            "pull requests exist for branch '{}', but none belongs to the assigned repository head",
            assignment.branch
        )
        .into());
    }
    let open_count = same_repo
        .iter()
        .filter(|pr| pr.parsed_state().is_ok_and(|state| state == PrState::Open))
        .count();
    if open_count > 1 {
        return Err(format!(
            "more than one open pull request exists for assigned branch '{}'",
            assignment.branch
        )
        .into());
    }
    same_repo.sort_by_key(|pr| pr.number);
    Ok(same_repo
        .iter()
        .find(|pr| pr.parsed_state().is_ok_and(|state| state == PrState::Open))
        .cloned()
        .or_else(|| same_repo.pop()))
}

pub(crate) fn validate_pr_identity(assignment: &Assignment, pr: &GhPr) -> Result<(), FlatError> {
    let base = assignment
        .base
        .as_deref()
        .ok_or("assignment has no durable base branch")?;
    if pr.is_cross_repository {
        return Err(format!(
            "pull request #{} uses a cross-repository head; expected '{}'",
            pr.number, assignment.branch
        )
        .into());
    }
    if pr.head_ref_name != assignment.branch {
        return Err(format!(
            "pull request #{} uses head '{}', expected '{}'",
            pr.number, pr.head_ref_name, assignment.branch
        )
        .into());
    }
    if pr.base_ref_name != base {
        return Err(format!(
            "pull request #{} targets base '{}', expected '{}'",
            pr.number, pr.base_ref_name, base
        )
        .into());
    }
    Ok(())
}

pub(crate) fn pr_response(
    assignment: &Assignment,
    pr: &GhPr,
    created: bool,
) -> Result<serde_json::Value, FlatError> {
    Ok(json!({
        "assignment_id": assignment.assignment_id,
        "pr": pr.number,
        "url": pr.url,
        "created": created,
        "pr_state": pr.parsed_state()?.as_str(),
        "draft": pr.is_draft,
        "base": assignment.base,
        "pr_base": pr.base_ref_name,
        "expected_head": assignment.expected_head,
        "pushed_head": assignment.pushed_head,
        "pr_head": pr.head_ref_oid,
    }))
}

#[derive(Debug, Clone)]
pub(crate) struct CleanupPlan {
    pub(crate) head: Option<String>,
    pub(crate) reason: CleanupReason,
}

pub(crate) async fn validate_cleanup_resources(
    repos: &Repos,
    assignment: &Assignment,
    plan: &CleanupPlan,
) -> Result<(), FlatError> {
    let worktree = Path::new(&assignment.worktree);
    if worktree.exists() {
        repos
            .validate_worktree_at(
                worktree,
                &assignment.branch,
                Path::new(&assignment.bare_path),
            )
            .await?;
        if !worktree_is_clean(worktree).await? {
            return Err(
                "assigned worktree is dirty; retain it or commit/clean it before cleanup".into(),
            );
        }
        let current = git_output(worktree, &["rev-parse", "--verify", "HEAD^{commit}"]).await?;
        if plan.head.as_deref() != Some(current.as_str()) {
            return Err(format!(
                "assigned worktree moved to {current} after cleanup was authorized at {}",
                plan.head.as_deref().unwrap_or("<no branch>")
            )
            .into());
        }
    }
    if let Some(current) = repos.local_branch_head(assignment).await?
        && plan.head.as_deref() != Some(current.as_str())
    {
        return Err(format!(
            "assigned branch moved to {current} after cleanup was authorized at {}",
            plan.head.as_deref().unwrap_or("<no branch>")
        )
        .into());
    }
    Ok(())
}

pub(crate) async fn cleanup_plan(
    assignments: &AssignmentDb,
    repos: &Repos,
    assignment: &Assignment,
    discard_head: Option<&str>,
) -> Result<CleanupPlan, FlatError> {
    if matches!(
        assignment.cleanup_state,
        CleanupState::Removing | CleanupState::Failed
    ) && let Some(reason) = assignment.cleanup_reason
        && discard_head.is_none_or(|discard| assignment.cleanup_head.as_deref() == Some(discard))
    {
        let plan = CleanupPlan {
            head: assignment.cleanup_head.clone(),
            reason,
        };
        validate_cleanup_resources(repos, assignment, &plan).await?;
        return Ok(plan);
    }

    let worktree = Path::new(&assignment.worktree);
    let branch_head = repos.local_branch_head(assignment).await?;
    if !worktree.exists() && branch_head.is_none() {
        return Ok(CleanupPlan {
            head: None,
            reason: CleanupReason::Absent,
        });
    }

    let head = if worktree.exists() {
        repos
            .validate_worktree_at(
                worktree,
                &assignment.branch,
                Path::new(&assignment.bare_path),
            )
            .await?;
        if !worktree_is_clean(worktree).await? {
            return Err(
                "assigned worktree is dirty; retain it or commit/clean it before cleanup".into(),
            );
        }
        let head = git_output(worktree, &["rev-parse", "--verify", "HEAD^{commit}"]).await?;
        if let Some(branch_head) = branch_head.as_deref()
            && branch_head != head
        {
            return Err(
                format!("assigned branch is {branch_head}, but worktree HEAD is {head}").into(),
            );
        }
        head
    } else {
        branch_head.ok_or("assignment branch disappeared during cleanup inspection")?
    };

    let no_changes = assignment.base_head.as_deref() == Some(head.as_str());
    let reason = if no_changes {
        CleanupReason::NoChanges
    } else if let Some(discard) = discard_head {
        let canonical = if worktree.exists() {
            git_output(
                worktree,
                &["rev-parse", "--verify", &format!("{discard}^{{commit}}")],
            )
            .await?
        } else {
            git_output(
                Path::new(&assignment.bare_path),
                &["rev-parse", "--verify", &format!("{discard}^{{commit}}")],
            )
            .await?
        };
        if canonical != discard || canonical != head {
            return Err(format!(
                "discard_head must be the full current assigned commit OID '{head}'"
            )
            .into());
        }
        CleanupReason::Discarded
    } else {
        let pr = discover_pr(repos, assignment).await?;
        if let Some(pr) = pr.as_ref() {
            record_validated_pr(assignments, assignment, pr).await?;
        }
        let merged = pr.as_ref().is_some_and(|pr| {
            pr.parsed_state().ok() == Some(PrState::Merged)
                && pr.head_ref_oid == head
                && assignment.expected_head.as_deref() == Some(head.as_str())
                && assignment.base.as_deref() == Some(pr.base_ref_name.as_str())
        });
        if merged {
            CleanupReason::Merged
        } else {
            let pr_state = pr
                .as_ref()
                .and_then(|pr| pr.parsed_state().ok())
                .map(PrState::as_str)
                .unwrap_or("unpublished");
            return Err(format!(
                "cleanup would discard {pr_state} work at {head}; review it and retry with discard_head='{head}', or keep=true"
            )
            .into());
        }
    };
    Ok(CleanupPlan {
        head: Some(head),
        reason,
    })
}

pub(crate) async fn canonical_approved_head(
    assignment: &Assignment,
    snapshot: &WorktreeSnapshot,
    requested: Option<&str>,
) -> Result<String, FlatError> {
    let worktree = Path::new(&assignment.worktree);
    match requested {
        Some(requested) => {
            let canonical = git_output(
                worktree,
                &["rev-parse", "--verify", &format!("{requested}^{{commit}}")],
            )
            .await?;
            if canonical != requested {
                return Err(format!(
                    "expected_head must be the full canonical commit OID '{canonical}'"
                )
                .into());
            }
            if canonical != snapshot.head {
                return Err(format!(
                    "assigned branch moved: expected_head is {canonical}, current head is {}",
                    snapshot.head
                )
                .into());
            }
            Ok(canonical)
        }
        None => match assignment.expected_head.as_deref() {
            Some(expected) if expected == snapshot.head => Ok(expected.to_string()),
            Some(expected) => Err(format!(
                "assigned branch moved from durable expected head {expected} to {}; review it and retry with expected_head='{}'",
                snapshot.head, snapshot.head
            )
            .into()),
            None => Ok(snapshot.head.clone()),
        },
    }
}

pub(crate) async fn publish_assignment(
    ctx: &PluginContext,
    repos: &Repos,
    args: &OpenPrArgs,
) -> Result<serde_json::Value, FlatError> {
    let assignments = AssignmentDb::new(ctx.pool.clone());
    let Some(mut assignment) = assignments.get_by_agent(&args.agent_id).await? else {
        if !conventional_title(&args.title) {
            return Err(not_conventional_title(&args.title).into());
        }
        return Err(format!("no assignment for '{}'", args.agent_id).into());
    };

    let existing = discover_pr(repos, &assignment).await?;
    if let Some(pr) = existing.as_ref() {
        record_validated_pr(&assignments, &assignment, pr).await?;
        let pr_state = pr.parsed_state()?;
        if pr_state != PrState::Open
            || !matches!(
                assignment.state,
                AssignmentState::Active | AssignmentState::Retained
            )
        {
            if let Some(expected) = args.expected_head.as_deref()
                && expected != pr.head_ref_oid
            {
                return Err(format!(
                    "pull request #{} records head {}, not supplied expected head {expected}",
                    pr.number, pr.head_ref_oid
                )
                .into());
            }
            if let Some(expected) = args.expected_head.as_deref()
                && matches!(
                    assignment.state,
                    AssignmentState::Active | AssignmentState::Retained
                )
            {
                assignments
                    .begin_publication(&assignment.assignment_id, expected)
                    .await?;
                assignments
                    .record_pr_observation(&assignment.assignment_id, pr, None)
                    .await?;
                assignment.expected_head = Some(expected.to_string());
            }
            return pr_response(&assignment, pr, false);
        }
        let requested_changes_head = args
            .expected_head
            .as_deref()
            .is_some_and(|expected| expected != pr.head_ref_oid);
        if !requested_changes_head {
            if assignment.expected_head.is_none()
                && let Some(expected) = args.expected_head.as_deref()
            {
                assignments
                    .begin_publication(&assignment.assignment_id, expected)
                    .await?;
                assignments
                    .record_pr_observation(&assignment.assignment_id, pr, None)
                    .await?;
                assignment.expected_head = Some(expected.to_string());
            }
            return pr_response(&assignment, pr, false);
        }
    }

    if !matches!(
        assignment.state,
        AssignmentState::Active | AssignmentState::Retained
    ) {
        return Err(assignment.conflict(None).into());
    }
    if existing.is_none() && !conventional_title(&args.title) {
        return Err(not_conventional_title(&args.title).into());
    }
    let snapshot = repos.inspect_assignment_worktree(&assignment).await?;
    if !worktree_is_clean(Path::new(&assignment.worktree)).await? {
        return Err("assigned worktree is dirty; commit or retain it before publication".into());
    }
    if snapshot.commits_ahead == 0 || !snapshot.has_material_delta {
        return Err(format!(
            "assigned branch has no committed material delta from base {}",
            snapshot.base_head
        )
        .into());
    }
    let approved =
        canonical_approved_head(&assignment, &snapshot, args.expected_head.as_deref()).await?;

    let remote_before = repos
        .remote_branch_head(&assignment, &snapshot.push_url)
        .await?;
    if let Some(remote) = remote_before.as_deref()
        && remote != approved
    {
        let expected_previous = existing
            .as_ref()
            .map(|pr| pr.head_ref_oid.as_str())
            .or(assignment.pr_head.as_deref())
            .or(assignment.pushed_head.as_deref())
            .or(assignment.expected_head.as_deref());
        if expected_previous != Some(remote)
            || !git_predicate(
                Path::new(&assignment.worktree),
                &["merge-base", "--is-ancestor", remote, &approved],
            )
            .await?
        {
            return Err(format!(
                "remote branch moved to {remote}; refusing to overwrite it with approved head {approved}"
            )
            .into());
        }
    }

    assignments
        .begin_publication(&assignment.assignment_id, &approved)
        .await?;
    assignment.expected_head = Some(approved.clone());
    assignment.publication_state = PublicationState::Publishing;
    let result: Result<serde_json::Value, FlatError> = async {
        if remote_before.as_deref() != Some(approved.as_str()) {
            repos
                .push_exact(
                    &assignment,
                    &approved,
                    remote_before.as_deref(),
                    &snapshot.push_url,
                )
                .await
                .map_err(|error| -> FlatError { format!("push: {error}").into() })?;
        }
        assignments
            .record_branch_pushed(&assignment.assignment_id, &approved)
            .await?;
        assignment.pushed_head = Some(approved.clone());

        if let Some(pr) = existing {
            let refreshed = discover_pr(
                repos,
                &Assignment {
                    pr: Some(pr.number),
                    ..assignment.clone()
                },
            )
            .await?;
            let refreshed = refreshed.ok_or("published pull request disappeared during refresh")?;
            record_validated_pr(&assignments, &assignment, &refreshed).await?;
            if refreshed.head_ref_oid != approved {
                return Err(format!(
                    "pull request #{} still reports head {}, expected published head {approved}",
                    refreshed.number, refreshed.head_ref_oid
                )
                .into());
            }
            return pr_response(&assignment, &refreshed, false);
        }

        let base = assignment
            .base
            .as_deref()
            .ok_or("assignment has no durable base branch")?;
        let draft = args.draft.unwrap_or(true);
        let repository = github_repo(&assignment.repo);
        let mut command = vec![
            "pr",
            "create",
            "--repo",
            &repository,
            "--head",
            &assignment.branch,
            "--base",
            base,
            "--title",
            &args.title,
            "--body",
            &args.body,
        ];
        if draft {
            command.push("--draft");
        }
        let created = gh(
            &repos.gh_binary,
            Some(Path::new(&assignment.worktree)),
            &command,
        )
        .await;
        let reconciled = discover_pr(repos, &assignment).await?;
        match reconciled {
            Some(pr) => {
                record_validated_pr(&assignments, &assignment, &pr).await?;
                if pr.head_ref_oid != approved {
                    return Err(format!(
                        "created pull request #{} reports head {}, expected {approved}",
                        pr.number, pr.head_ref_oid
                    )
                    .into());
                }
                pr_response(&assignment, &pr, created.is_ok())
            }
            None => Err(created
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "gh reported success but no pull request is discoverable".into())
                .into()),
        }
    }
    .await;
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            let message = error.to_string();
            if let Err(persistence) = assignments
                .publication_failed(&assignment.assignment_id, &message)
                .await
            {
                return Err(format!(
                    "{message}; recording the publication failure also failed: {persistence}"
                )
                .into());
            }
            Err(error)
        }
    }
}
