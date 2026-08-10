//! The durable assignment ledger: every SQL statement the plugin
//! runs against its own tables.

use std::path::{Path, PathBuf};

use sqlx::{Row, SqlitePool};

use ciacola_core::agent::FlatError;
use ciacola_core::ledger::Ledger;

use crate::assignment::{
    Assignment, AssignmentState, CleanupReason, CleanupState, LegacyAssignment, PublicationState,
    assignment_slug, sqlite_u64,
};
use crate::config::BranchTemplate;
use crate::git::{git_output, github_origin_matches, validate_branch_name};
use crate::journey::GhPr;
use crate::repos::Repos;

#[derive(Clone)]
pub(crate) struct AssignmentDb {
    pub(crate) pool: SqlitePool,
}

impl AssignmentDb {
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub(crate) async fn get(
        &self,
        repo: &str,
        issue: u64,
    ) -> Result<Option<Assignment>, FlatError> {
        let issue = sqlite_u64(issue, "issue")?;
        let row = sqlx::query(
            "SELECT * FROM repo_worker_assignments WHERE repo = ?1 AND issue_number = ?2",
        )
        .bind(repo)
        .bind(issue)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Assignment::from_row).transpose()
    }

    pub(crate) async fn get_by_agent(
        &self,
        agent_id: &str,
    ) -> Result<Option<Assignment>, FlatError> {
        let row = sqlx::query("SELECT * FROM repo_worker_assignments WHERE agent_id = ?1")
            .bind(agent_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(Assignment::from_row).transpose()
    }

    pub(crate) async fn get_by_id(
        &self,
        assignment_id: &str,
    ) -> Result<Option<Assignment>, FlatError> {
        let row = sqlx::query("SELECT * FROM repo_worker_assignments WHERE assignment_id = ?1")
            .bind(assignment_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(Assignment::from_row).transpose()
    }

    pub(crate) async fn list(&self) -> Result<Vec<Assignment>, FlatError> {
        let rows = sqlx::query(
            "SELECT * FROM repo_worker_assignments ORDER BY updated_unix DESC, assignment_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Assignment::from_row).collect()
    }

    pub(crate) async fn conflicting_resources(
        &self,
        assignment: &Assignment,
    ) -> Result<Vec<String>, FlatError> {
        let rows: Vec<(String, Option<String>, String)> = sqlx::query_as(
            "SELECT assignment_id, agent_id, related_agent_ids
             FROM repo_worker_assignments
             WHERE assignment_id <> ?1
               AND (worktree = ?2 OR (repo = ?3 AND branch = ?4))
             ORDER BY assignment_id",
        )
        .bind(&assignment.assignment_id)
        .bind(&assignment.worktree)
        .bind(&assignment.repo)
        .bind(&assignment.branch)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(id, agent_id, related)| {
                let mut agents: Vec<String> = serde_json::from_str(&related)?;
                if let Some(agent_id) = agent_id {
                    if !agents.contains(&agent_id) {
                        agents.push(agent_id);
                    }
                }
                Ok(if agents.is_empty() {
                    id
                } else {
                    format!("{id} (agents: {})", agents.join(", "))
                })
            })
            .collect()
    }

    pub(crate) async fn reserve(
        &self,
        repo: &str,
        issue: u64,
        requested_base: Option<&str>,
        repos: &Repos,
        branch_template: &BranchTemplate,
        spawned_by: Option<&str>,
    ) -> Result<(Assignment, bool), FlatError> {
        let issue_sql = sqlite_u64(issue, "issue")?;
        if let Some(existing) = self.get(repo, issue).await? {
            return Ok((existing, false));
        }
        let assignment_id = ulid::Ulid::new().to_string();
        let slug = assignment_slug(repo, issue, &assignment_id);
        let branch = branch_template.render(&slug);
        validate_branch_name(&branch).await?;
        let worktree = repos.root.join(format!("wt-{slug}")).display().to_string();
        let bare_path = repos.bare(repo).display().to_string();
        let now = ciacola_core::now_unix();
        let done = sqlx::query(
            "INSERT INTO repo_worker_assignments
                 (assignment_id, repo, issue_number, state, phase, base, slug, branch,
                  branch_policy, worktree, bare_path, spawned_by, created_unix, updated_unix)
             VALUES (?1, ?2, ?3, 'preparing', 'reserved', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)
             ON CONFLICT DO NOTHING",
        )
        .bind(&assignment_id)
        .bind(repo)
        .bind(issue_sql)
        .bind(requested_base)
        .bind(&slug)
        .bind(&branch)
        .bind(branch_template.as_str())
        .bind(&worktree)
        .bind(&bare_path)
        .bind(spawned_by)
        .bind(now)
        .execute(&self.pool)
        .await?;
        if done.rows_affected() == 1 {
            return Ok((
                self.get_by_id(&assignment_id)
                    .await?
                    .ok_or("reserved assignment disappeared")?,
                true,
            ));
        }
        if let Some(existing) = self.get(repo, issue).await? {
            return Ok((existing, false));
        }
        let collision = sqlx::query(
            "SELECT assignment_id, repo, issue_number FROM repo_worker_assignments
             WHERE worktree = ?1 OR (repo = ?2 AND branch = ?3)",
        )
        .bind(&worktree)
        .bind(repo)
        .bind(&branch)
        .fetch_optional(&self.pool)
        .await?;
        match collision {
            Some(row) => Err(format!(
                "assignment path/branch collides with '{}' for {}#{}",
                row.try_get::<String, _>("assignment_id")?,
                row.try_get::<String, _>("repo")?,
                row.try_get::<i64, _>("issue_number")?
            )
            .into()),
            None => Err("assignment reservation was refused by its database constraints".into()),
        }
    }

    pub(crate) async fn set_base(&self, assignment_id: &str, base: &str) -> Result<(), FlatError> {
        let done = sqlx::query(
            "UPDATE repo_worker_assignments
             SET base = ?2, phase = 'base_resolved', updated_unix = ?3
             WHERE assignment_id = ?1 AND state = 'preparing'",
        )
        .bind(assignment_id)
        .bind(base)
        .bind(ciacola_core::now_unix())
        .execute(&self.pool)
        .await?;
        if done.rows_affected() != 1 {
            return Err("assignment stopped preparing before base selection".into());
        }
        Ok(())
    }

    /// Release a reservation only while it is still provably pre-resource.
    /// Once any durable phase advances, callers must retain the row as stale
    /// because git or agent side effects may already exist.
    pub(crate) async fn abandon_reservation(&self, assignment_id: &str) -> Result<bool, FlatError> {
        let done = sqlx::query(
            "DELETE FROM repo_worker_assignments
             WHERE assignment_id = ?1 AND state = 'preparing'
               AND phase = 'reserved' AND agent_id IS NULL",
        )
        .bind(assignment_id)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    pub(crate) async fn record_pre_resource_failure(
        &self,
        assignment_id: &str,
        phase: &str,
        error: &str,
    ) -> Result<(), FlatError> {
        match self.abandon_reservation(assignment_id).await {
            Ok(true) => Ok(()),
            Ok(false) => self.stale(assignment_id, phase, error).await,
            Err(delete_error) => match self.stale(assignment_id, phase, error).await {
                Ok(()) => Err(format!(
                    "could not release pre-resource reservation ({delete_error}); retained it as stale"
                )
                .into()),
                Err(stale_error) => Err(format!(
                    "could not release pre-resource reservation ({delete_error}) or mark it stale ({stale_error})"
                )
                .into()),
            },
        }
    }

    pub(crate) async fn set_phase(
        &self,
        assignment_id: &str,
        phase: &str,
    ) -> Result<(), FlatError> {
        let done = sqlx::query(
            "UPDATE repo_worker_assignments SET phase = ?2, updated_unix = ?3
             WHERE assignment_id = ?1 AND state = 'preparing'",
        )
        .bind(assignment_id)
        .bind(phase)
        .bind(ciacola_core::now_unix())
        .execute(&self.pool)
        .await?;
        if done.rows_affected() != 1 {
            return Err("assignment stopped preparing during provisioning".into());
        }
        Ok(())
    }

    pub(crate) async fn set_base_head(
        &self,
        assignment_id: &str,
        base_head: &str,
    ) -> Result<(), FlatError> {
        let done = sqlx::query(
            "UPDATE repo_worker_assignments
             SET base_head = ?2, phase = 'worktree_ready', updated_unix = ?3
             WHERE assignment_id = ?1 AND state = 'preparing'",
        )
        .bind(assignment_id)
        .bind(base_head)
        .bind(ciacola_core::now_unix())
        .execute(&self.pool)
        .await?;
        if done.rows_affected() != 1 {
            return Err("assignment stopped preparing before its base head was recorded".into());
        }
        Ok(())
    }

    pub(crate) async fn stale(
        &self,
        assignment_id: &str,
        phase: &str,
        error: &str,
    ) -> Result<(), FlatError> {
        let now = ciacola_core::now_unix();
        let done = sqlx::query(
            "UPDATE repo_worker_assignments
             SET state = 'stale', phase = ?2, last_error = ?3,
                 cleanup_state = CASE WHEN state = 'finishing'
                     OR cleanup_state IN ('retaining', 'removing')
                     THEN 'failed' ELSE cleanup_state END,
                 updated_unix = ?4, terminal_unix = ?4
             WHERE assignment_id = ?1 AND state IN ('preparing', 'active', 'finishing', 'retained')",
        )
        .bind(assignment_id)
        .bind(phase)
        .bind(error)
        .bind(now)
        .execute(&self.pool)
        .await?;
        if done.rows_affected() != 1 {
            return Err("assignment could not be marked stale".into());
        }
        Ok(())
    }

    pub(crate) async fn terminal(
        &self,
        assignment_id: &str,
        state: AssignmentState,
        phase: &str,
        error: Option<&str>,
    ) -> Result<(), FlatError> {
        let now = ciacola_core::now_unix();
        let done = sqlx::query(
            "UPDATE repo_worker_assignments
             SET state = ?2, phase = ?3, last_error = ?4,
                 cleanup_state = CASE ?2
                     WHEN 'retained' THEN 'retained'
                     WHEN 'completed' THEN 'completed'
                     ELSE cleanup_state END,
                 updated_unix = ?5, terminal_unix = ?5
             WHERE assignment_id = ?1 AND state = 'finishing'",
        )
        .bind(assignment_id)
        .bind(state.as_str())
        .bind(phase)
        .bind(error)
        .bind(now)
        .execute(&self.pool)
        .await?;
        if done.rows_affected() != 1 {
            return Err("assignment state transition lost its durable row".into());
        }
        Ok(())
    }

    pub(crate) async fn begin_finish(
        &self,
        assignment: &Assignment,
        keep: bool,
        cleanup_head: Option<&str>,
        cleanup_reason: Option<CleanupReason>,
    ) -> Result<bool, FlatError> {
        let phase = if keep {
            "finishing_keep"
        } else if assignment.state == AssignmentState::Retained
            || (assignment.state == AssignmentState::Stale
                && matches!(
                    assignment.phase.as_str(),
                    "finishing_remove_retained" | "finish_terminal_remove_retained"
                ))
        {
            "finishing_remove_retained"
        } else if assignment.state == AssignmentState::Stale {
            "finishing_remove_stale"
        } else {
            "finishing_remove"
        };
        if assignment.state == AssignmentState::Finishing {
            let same_mode = if keep {
                assignment.phase == "finishing_keep"
            } else {
                matches!(
                    assignment.phase.as_str(),
                    "finishing_remove" | "finishing_remove_retained" | "finishing_remove_stale"
                )
            };
            if !same_mode {
                return Ok(false);
            }
            if keep {
                return Ok(true);
            }
            let done = sqlx::query(
                "UPDATE repo_worker_assignments
                 SET cleanup_state = 'removing', cleanup_head = ?2,
                     cleanup_reason = ?3, last_error = NULL, updated_unix = ?4
                 WHERE assignment_id = ?1 AND state = 'finishing' AND phase = ?5",
            )
            .bind(&assignment.assignment_id)
            .bind(cleanup_head)
            .bind(cleanup_reason.map(CleanupReason::as_str))
            .bind(ciacola_core::now_unix())
            .bind(&assignment.phase)
            .execute(&self.pool)
            .await?;
            return Ok(done.rows_affected() == 1);
        }
        let allowed = if keep {
            assignment.state == AssignmentState::Active
                || (assignment.state == AssignmentState::Stale
                    && (matches!(
                        assignment.phase.as_str(),
                        "finishing_keep" | "finish_terminal_keep"
                    ) || (assignment.cleanup_state == CleanupState::Failed
                        && assignment.cleanup_reason.is_none()
                        && assignment.phase.starts_with("finish_agent_"))))
        } else {
            matches!(
                assignment.state,
                AssignmentState::Active | AssignmentState::Retained | AssignmentState::Stale
            )
        };
        if !allowed {
            return Ok(false);
        }
        let done = sqlx::query(
            "UPDATE repo_worker_assignments
             SET state = 'finishing', phase = ?3, last_error = NULL,
                 cleanup_state = ?4, cleanup_head = ?5, cleanup_reason = ?6,
                 updated_unix = ?7
             WHERE assignment_id = ?1 AND state = ?2",
        )
        .bind(&assignment.assignment_id)
        .bind(assignment.state.as_str())
        .bind(phase)
        .bind(if keep { "retaining" } else { "removing" })
        .bind(cleanup_head)
        .bind(cleanup_reason.map(CleanupReason::as_str))
        .bind(ciacola_core::now_unix())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    pub(crate) async fn restore_after_busy(
        &self,
        assignment: &Assignment,
    ) -> Result<(), FlatError> {
        let done = sqlx::query(
            "UPDATE repo_worker_assignments
             SET state = ?2, phase = ?3, last_error = ?4,
                 cleanup_state = ?5, cleanup_head = ?6, cleanup_reason = ?7,
                 updated_unix = ?8
             WHERE assignment_id = ?1 AND state = 'finishing'",
        )
        .bind(&assignment.assignment_id)
        .bind(assignment.state.as_str())
        .bind(&assignment.phase)
        .bind(&assignment.last_error)
        .bind(assignment.cleanup_state.as_str())
        .bind(&assignment.cleanup_head)
        .bind(assignment.cleanup_reason.map(CleanupReason::as_str))
        .bind(ciacola_core::now_unix())
        .execute(&self.pool)
        .await?;
        if done.rows_affected() != 1 {
            return Err("finishing assignment changed before its busy rollback".into());
        }
        Ok(())
    }

    pub(crate) async fn begin_publication(
        &self,
        assignment_id: &str,
        expected_head: &str,
    ) -> Result<(), FlatError> {
        let done = sqlx::query(
            "UPDATE repo_worker_assignments
             SET expected_head = ?2, publication_state = 'publishing',
                 phase = 'publishing', last_error = NULL, updated_unix = ?3
             WHERE assignment_id = ?1 AND state IN ('active', 'retained')",
        )
        .bind(assignment_id)
        .bind(expected_head)
        .bind(ciacola_core::now_unix())
        .execute(&self.pool)
        .await?;
        if done.rows_affected() != 1 {
            return Err("assignment is not publishable while recording its expected head".into());
        }
        Ok(())
    }

    pub(crate) async fn record_branch_pushed(
        &self,
        assignment_id: &str,
        expected_head: &str,
    ) -> Result<(), FlatError> {
        let done = sqlx::query(
            "UPDATE repo_worker_assignments
             SET phase = 'branch_pushed', pushed_head = ?2, updated_unix = ?3
             WHERE assignment_id = ?1 AND state IN ('active', 'retained')
               AND publication_state = 'publishing' AND expected_head = ?2",
        )
        .bind(assignment_id)
        .bind(expected_head)
        .bind(ciacola_core::now_unix())
        .execute(&self.pool)
        .await?;
        if done.rows_affected() != 1 {
            return Err("assignment changed after its branch was pushed".into());
        }
        Ok(())
    }

    pub(crate) async fn publication_failed(
        &self,
        assignment_id: &str,
        error: &str,
    ) -> Result<(), FlatError> {
        let done = sqlx::query(
            "UPDATE repo_worker_assignments
             SET publication_state = 'failed', phase = 'publication_failed',
                 last_error = ?2, updated_unix = ?3
             WHERE assignment_id = ?1",
        )
        .bind(assignment_id)
        .bind(error)
        .bind(ciacola_core::now_unix())
        .execute(&self.pool)
        .await?;
        if done.rows_affected() != 1 {
            return Err("assignment changed while recording publication failure".into());
        }
        Ok(())
    }

    pub(crate) async fn record_pr_observation(
        &self,
        assignment_id: &str,
        pr: &GhPr,
        validation_error: Option<&str>,
    ) -> Result<(), FlatError> {
        let number = sqlite_u64(pr.number, "pull request")?;
        let state = pr.parsed_state()?;
        let now = ciacola_core::now_unix();
        let done = sqlx::query(
            "UPDATE repo_worker_assignments
             SET pr = ?2, pr_url = ?3, pr_state = ?4, pr_draft = ?5,
                 pr_head = ?6, pr_base = ?7, pr_checked_unix = ?8,
                 publication_state = CASE WHEN ?9 IS NULL
                     THEN 'published' ELSE 'failed' END,
                 phase = CASE
                     WHEN ?9 IS NOT NULL AND state IN ('active', 'retained')
                         THEN 'publication_failed'
                     WHEN state IN ('active', 'retained') THEN 'pr_' || ?4
                     ELSE phase END,
                 last_error = CASE
                     WHEN ?9 IS NOT NULL AND state IN ('active', 'retained') THEN ?9
                     WHEN state IN ('active', 'retained') THEN NULL
                     ELSE last_error END,
                 updated_unix = ?8
             WHERE assignment_id = ?1",
        )
        .bind(assignment_id)
        .bind(number)
        .bind(&pr.url)
        .bind(state.as_str())
        .bind(i64::from(pr.is_draft))
        .bind(&pr.head_ref_oid)
        .bind(&pr.base_ref_name)
        .bind(now)
        .bind(validation_error)
        .execute(&self.pool)
        .await?;
        if done.rows_affected() != 1 {
            return Err("assignment disappeared while recording pull request state".into());
        }
        Ok(())
    }

    pub(crate) async fn import_legacy(
        &self,
        ledger: &Ledger,
        repos: &Repos,
    ) -> Result<(), FlatError> {
        let has_store: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'plugin_kv'",
        )
        .fetch_one(&self.pool)
        .await?;
        if has_store.0 == 0 {
            return Ok(());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT key, value FROM plugin_kv
             WHERE plugin = 'repo-worker' AND key LIKE 'agent/%' ORDER BY key",
        )
        .fetch_all(&self.pool)
        .await?;
        type LegacyGroups =
            std::collections::BTreeMap<(String, u64), Vec<(String, String, LegacyAssignment)>>;
        let mut groups = LegacyGroups::new();
        for (key, value) in rows {
            let legacy: LegacyAssignment =
                serde_json::from_str(&value).map_err(|e| -> FlatError {
                    format!("cannot import legacy repo-worker assignment '{key}': {e}").into()
                })?;
            let agent_id = key
                .strip_prefix("agent/")
                .ok_or("legacy assignment has an invalid key")?
                .to_string();
            groups
                .entry((legacy.repo.clone(), legacy.issue))
                .or_default()
                .push((key, agent_id, legacy));
        }

        type LegacyResourceOwners =
            std::collections::BTreeMap<String, std::collections::BTreeSet<String>>;
        type LegacyBranchOwners =
            std::collections::BTreeMap<(String, String), std::collections::BTreeSet<String>>;
        let mut worktree_owners = LegacyResourceOwners::new();
        let mut branch_owners = LegacyBranchOwners::new();
        for group in groups.values() {
            for (_, agent_id, legacy) in group {
                worktree_owners
                    .entry(legacy.worktree.clone())
                    .or_default()
                    .insert(agent_id.clone());
                branch_owners
                    .entry((legacy.repo.to_ascii_lowercase(), legacy.branch.clone()))
                    .or_default()
                    .insert(agent_id.clone());
            }
        }
        type DurableResourceOwners = std::collections::BTreeMap<String, Vec<Assignment>>;
        type DurableBranchOwners = std::collections::BTreeMap<(String, String), Vec<Assignment>>;
        let mut durable_worktree_owners = DurableResourceOwners::new();
        let mut durable_branch_owners = DurableBranchOwners::new();
        for assignment in self.list().await? {
            durable_worktree_owners
                .entry(assignment.worktree.clone())
                .or_default()
                .push(assignment.clone());
            durable_branch_owners
                .entry((
                    assignment.repo.to_ascii_lowercase(),
                    assignment.branch.clone(),
                ))
                .or_default()
                .push(assignment);
        }

        for ((repo, issue), group) in groups {
            let own_agent_ids: std::collections::BTreeSet<String> =
                group.iter().map(|(_, id, _)| id.clone()).collect();
            let mut resource_agent_ids = own_agent_ids.clone();
            let mut durable_conflicts = std::collections::BTreeMap::new();
            for (_, _, legacy) in &group {
                if let Some(ids) = worktree_owners.get(&legacy.worktree) {
                    resource_agent_ids.extend(ids.iter().cloned());
                }
                if let Some(ids) =
                    branch_owners.get(&(legacy.repo.to_ascii_lowercase(), legacy.branch.clone()))
                {
                    resource_agent_ids.extend(ids.iter().cloned());
                }
                for peer in durable_worktree_owners
                    .get(&legacy.worktree)
                    .into_iter()
                    .flatten()
                    .chain(
                        durable_branch_owners
                            .get(&(legacy.repo.to_ascii_lowercase(), legacy.branch.clone()))
                            .into_iter()
                            .flatten(),
                    )
                    .filter(|peer| peer.issue != issue || !peer.repo.eq_ignore_ascii_case(&repo))
                {
                    if let Some(agent_id) = &peer.agent_id {
                        resource_agent_ids.insert(agent_id.clone());
                    }
                    resource_agent_ids.extend(peer.related_agent_ids.iter().cloned());
                    durable_conflicts
                        .entry(peer.assignment_id.clone())
                        .or_insert_with(|| peer.clone());
                }
            }
            let resource_collision = resource_agent_ids
                .iter()
                .any(|id| !own_agent_ids.contains(id))
                || !durable_conflicts.is_empty();
            let related_agent_ids: Vec<String> = resource_agent_ids.into_iter().collect();
            if resource_collision {
                let peer_ids = durable_conflicts.keys().cloned().collect::<Vec<_>>();
                for peer in durable_conflicts.values() {
                    let mut related = peer.related_agent_ids.clone();
                    if let Some(agent_id) = &peer.agent_id {
                        if !related.contains(agent_id) {
                            related.push(agent_id.clone());
                        }
                    }
                    for id in &related_agent_ids {
                        if !related.contains(id) {
                            related.push(id.clone());
                        }
                    }
                    let error = format!(
                        "legacy resource collision with assignments {}; agents: {}",
                        peer_ids.join(", "),
                        related.join(", ")
                    );
                    sqlx::query(
                        "UPDATE repo_worker_assignments
                         SET state = 'stale', phase = 'legacy_resource_conflict',
                             related_agent_ids = ?2, last_error = ?3,
                             updated_unix = ?4, terminal_unix = ?4
                         WHERE assignment_id = ?1
                           AND state IN ('preparing', 'active', 'finishing', 'retained', 'stale')",
                    )
                    .bind(&peer.assignment_id)
                    .bind(serde_json::to_string(&related)?)
                    .bind(error)
                    .bind(ciacola_core::now_unix())
                    .execute(&self.pool)
                    .await?;
                }
            }
            if let Some(existing) = self.get(&repo, issue).await? {
                let same_resources = group.iter().all(|(_, id, legacy)| {
                    existing.related_agent_ids.iter().any(|known| known == id)
                        && legacy.worktree == existing.worktree
                        && legacy.branch == existing.branch
                });
                if !same_resources || resource_collision {
                    let mut related = existing.related_agent_ids.clone();
                    for id in &related_agent_ids {
                        if !related.contains(id) {
                            related.push(id.clone());
                        }
                    }
                    let error = format!(
                        "legacy Store rows conflict with durable assignment; agents: {}",
                        related.join(", ")
                    );
                    sqlx::query(
                        "UPDATE repo_worker_assignments
                         SET state = 'stale', phase = 'legacy_conflict_after_migration',
                             related_agent_ids = ?2, last_error = ?3,
                             updated_unix = ?4, terminal_unix = ?4
                         WHERE assignment_id = ?1",
                    )
                    .bind(&existing.assignment_id)
                    .bind(serde_json::to_string(&related)?)
                    .bind(error)
                    .bind(ciacola_core::now_unix())
                    .execute(&self.pool)
                    .await?;
                }
                continue;
            }
            let (_, first_agent, first) = &group[0];
            let agent_ids = related_agent_ids;
            let bare_path = match git_output(
                Path::new(&first.worktree),
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await
            {
                Ok(path) => PathBuf::from(path),
                Err(_) => repos.bare(&repo),
            };
            let mut state = AssignmentState::Stale;
            let mut phase = if resource_collision {
                "legacy_resource_conflict".to_string()
            } else {
                "legacy_conflict".to_string()
            };
            let mut error = Some(if resource_collision {
                format!(
                    "legacy worktree or branch is claimed across assignments {}; agents: {}",
                    durable_conflicts
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                    agent_ids.join(", "),
                )
            } else {
                format!(
                    "legacy assignment has {} candidate agents: {}",
                    agent_ids.len(),
                    agent_ids.join(", ")
                )
            });
            let mut agent_id = None;
            let mut spawned_by = None;
            if group.len() == 1 && !resource_collision {
                agent_id = Some(first_agent.clone());
                match ledger.get_agent(first_agent).await? {
                    Some(agent) if !agent.retired => {
                        spawned_by = agent.spawned_by;
                        let validation = match git_output(
                            &bare_path,
                            &["remote", "get-url", "origin"],
                        )
                        .await
                        {
                            Ok(origin) if github_origin_matches(&repo, &origin) => {
                                repos
                                    .validate_worktree_at(
                                        Path::new(&first.worktree),
                                        &first.branch,
                                        &bare_path,
                                    )
                                    .await
                            }
                            Ok(origin) => Err(format!(
                                "legacy bare repository origin '{origin}' does not match '{repo}'"
                            )
                            .into()),
                            Err(e) => Err(e),
                        };
                        match validation {
                            Ok(()) => {
                                state = AssignmentState::Active;
                                phase = "legacy_imported".into();
                                error = None;
                            }
                            Err(e) => {
                                phase = "legacy_worktree_invalid".into();
                                error = Some(e.to_string());
                            }
                        }
                    }
                    Some(agent) => {
                        spawned_by = agent.spawned_by;
                        phase = "legacy_agent_retired".into();
                        error = Some(format!("legacy agent '{first_agent}' is retired"));
                    }
                    None => {
                        phase = "legacy_agent_missing".into();
                        error = Some(format!("legacy agent '{first_agent}' is missing"));
                    }
                }
            }
            let assignment_id = ulid::Ulid::new().to_string();
            let now = ciacola_core::now_unix();
            let issue_sql = sqlite_u64(issue, "issue")?;
            let pr_sql = first
                .pr
                .map(|pr| sqlite_u64(pr, "pull request"))
                .transpose()?;
            let terminal = (state == AssignmentState::Stale).then_some(now);
            let related = serde_json::to_string(&agent_ids)?;
            sqlx::query(
                "INSERT INTO repo_worker_assignments
                     (assignment_id, repo, issue_number, state, phase, base, slug, branch,
                      worktree, bare_path, agent_id, related_agent_ids, spawned_by, pr,
                      publication_state, last_error, created_unix, updated_unix, terminal_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                         ?13, ?14, ?15, ?16, ?16, ?17)",
            )
            .bind(&assignment_id)
            .bind(&repo)
            .bind(issue_sql)
            .bind(state.as_str())
            .bind(&phase)
            .bind(&first.slug)
            .bind(&first.branch)
            .bind(&first.worktree)
            .bind(bare_path.display().to_string())
            .bind(&agent_id)
            .bind(&related)
            .bind(&spawned_by)
            .bind(pr_sql)
            .bind(if first.pr.is_some() {
                PublicationState::Published.as_str()
            } else {
                PublicationState::Unpublished.as_str()
            })
            .bind(&error)
            .bind(now)
            .bind(terminal)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    pub(crate) async fn reconcile_on_start(
        &self,
        ledger: &Ledger,
        repos: &Repos,
    ) -> Result<(), FlatError> {
        let now = ciacola_core::now_unix();
        sqlx::query(
            "UPDATE repo_worker_assignments
             SET state = 'stale', phase = 'restart_during_preparation',
                 last_error = 'server restarted before provisioning completed',
                 updated_unix = ?1, terminal_unix = ?1
             WHERE state = 'preparing'",
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "UPDATE repo_worker_assignments
             SET state = 'stale',
                 cleanup_state = 'failed',
                 last_error = 'server restarted before finish completed; inspect resources before cleanup',
                 updated_unix = ?1, terminal_unix = ?1
             WHERE state = 'finishing'",
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "UPDATE repo_worker_assignments
             SET publication_state = 'failed', phase = 'publication_interrupted',
                 last_error = 'server restarted before publication completed; reconcile before retry',
                 updated_unix = ?1
             WHERE publication_state = 'publishing'",
        )
        .bind(now)
        .execute(&self.pool)
        .await?;

        for assignment in self.list().await? {
            let problem = match assignment.state {
                AssignmentState::Active => match assignment.agent_id.as_deref() {
                    None => Some("active assignment has no agent id".to_string()),
                    Some(agent_id) => match ledger.get_agent(agent_id).await? {
                        None => Some(format!("active agent '{agent_id}' is missing")),
                        Some(agent) if agent.retired => {
                            Some(format!("active agent '{agent_id}' is retired"))
                        }
                        Some(_) => repos
                            .validate_worktree_at(
                                Path::new(&assignment.worktree),
                                &assignment.branch,
                                Path::new(&assignment.bare_path),
                            )
                            .await
                            .err()
                            .map(|e| e.to_string()),
                    },
                },
                AssignmentState::Retained => match assignment.agent_id.as_deref() {
                    None => Some("retained assignment has no agent id".to_string()),
                    Some(agent_id) => match ledger.get_agent(agent_id).await? {
                        Some(agent) if agent.retired => repos
                            .validate_worktree_at(
                                Path::new(&assignment.worktree),
                                &assignment.branch,
                                Path::new(&assignment.bare_path),
                            )
                            .await
                            .err()
                            .map(|e| e.to_string()),
                        Some(_) => Some(format!("retained agent '{agent_id}' is not retired")),
                        None => Some(format!("retained agent '{agent_id}' is missing")),
                    },
                },
                AssignmentState::Completed if Path::new(&assignment.worktree).exists() => {
                    Some(format!(
                        "completed assignment still has worktree '{}'",
                        assignment.worktree
                    ))
                }
                AssignmentState::Preparing
                | AssignmentState::Finishing
                | AssignmentState::Completed
                | AssignmentState::Stale => None,
            };
            if let Some(problem) = problem {
                let now = ciacola_core::now_unix();
                let done = sqlx::query(
                    "UPDATE repo_worker_assignments
                     SET state = 'stale', phase = 'restart_reconciliation', last_error = ?3,
                         updated_unix = ?4, terminal_unix = ?4
                     WHERE assignment_id = ?1 AND state = ?2",
                )
                .bind(&assignment.assignment_id)
                .bind(assignment.state.as_str())
                .bind(&problem)
                .bind(now)
                .execute(&self.pool)
                .await?;
                if done.rows_affected() != 1 {
                    return Err("assignment changed during startup reconciliation".into());
                }
            }
        }
        Ok(())
    }
}
