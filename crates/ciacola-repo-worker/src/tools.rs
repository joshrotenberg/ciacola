//! The MCP tool surface: start_issue, worktrees, open_pr, and
//! finish_issue, wired per requesting surface.

use serde_json::json;
use tower_mcp::extract::{Context, Json, State};
use tower_mcp::{CallToolResult, Tool, ToolBuilder};

use ciacola_core::agent::FlatError;
use ciacola_core::plugin::{PluginContext, Surface};

use std::path::{Path, PathBuf};

use ciacola_core::roles::Roles;

use crate::assignment::{AssignmentState, CleanupReason, PrState};
use crate::config::{BranchPolicies, DEFAULT_BRANCH_TEMPLATE};
use crate::db::AssignmentDb;
use crate::git::{gh, git_output, github_repo};
use crate::journey::{cleanup_plan, publish_assignment, validate_cleanup_resources};
use crate::repos::Repos;
use crate::{
    FinishArgs, OpenPrArgs, ROLE, RepoWorkerPlugin, StartIssueArgs,
    validate_start_issue_role_arguments,
};
pub(crate) fn tools(plugin: &RepoWorkerPlugin, surface: Surface) -> Vec<Tool> {
    let (Some(repos), Some(ctx)) = (plugin.repos.clone(), plugin.ctx.clone()) else {
        return Vec::new();
    };
    // Higher-level plugin wiring consumes the same configured catalog as
    // roles, spawn_role, completion, and persistent role agents.
    let roles = ctx.roles.clone();
    let branch_policies = plugin.branch_policies.clone();

    let start = {
        let (repos, ctx, roles, branch_policies) = (
            repos.clone(),
            ctx.clone(),
            roles.clone(),
            branch_policies.clone(),
        );
        ToolBuilder::new("start_issue")
            .description(
                "Begin work on a GitHub issue: ensure the system's own \
                 clone, cut a fresh worktree and branch, and spawn an \
                 implementer pointed at it. Returns the agent to send to.",
            )
            .non_destructive()
            .extractor_handler(
                (
                    repos.clone(),
                    ctx.clone(),
                    roles.clone(),
                    branch_policies.clone(),
                ),
                move |State((repos, ctx, roles, branch_policies)): State<(
                    Repos,
                    PluginContext,
                    Roles,
                    BranchPolicies,
                )>,
                      mcp: Context,
                      Json(args): Json<StartIssueArgs>| async move {
                    if !repos.allows(&args.repo) {
                        return Ok(CallToolResult::error(format!(
                            "repository '{}' is not in the configured list",
                            args.repo
                        )));
                    }
                    let Some(role) = roles.get(ROLE).cloned() else {
                        return Ok(CallToolResult::error("role missing".to_string()));
                    };
                    // Authority is settled before the default-branch GitHub
                    // query, clone, worktree, agent, or assignment can
                    // mutate anything. `spawn_role` calls this same typed
                    // convergence point.
                    let authorization = match ciacola_core::preflight_role_spawn(
                        &ctx.ledger,
                        &role,
                        &mcp,
                        surface,
                        ctx.limits.max_spawn_depth,
                    )
                    .await
                    {
                        Ok(authorization) => authorization,
                        Err(refusal) => {
                            return Ok(CallToolResult::error(refusal.to_string()));
                        }
                    };
                    if let Err(error) = validate_start_issue_role_arguments(&role) {
                        return Ok(CallToolResult::error(error));
                    }
                    let assignments = AssignmentDb::new(ctx.pool.clone());
                    let (mut assignment, claimed) = match assignments
                        .reserve(
                            &args.repo,
                            args.issue,
                            args.base.as_deref(),
                            &repos,
                            branch_policies.for_repo(&args.repo),
                            authorization.spawned_by.as_deref(),
                        )
                        .await
                    {
                        Ok(result) => result,
                        Err(e) => return Ok(CallToolResult::error(e.to_string())),
                    };
                    if !claimed {
                        if assignment.state == AssignmentState::Active {
                            let _lifecycle = repos.lifecycle.lock().await;
                            assignment = match assignments
                                .get_by_id(&assignment.assignment_id)
                                .await
                            {
                                Ok(Some(current)) => current,
                                Ok(None) => {
                                    return Ok(CallToolResult::error(
                                        "active assignment disappeared".to_string(),
                                    ));
                                }
                                Err(e) => {
                                    return Ok(CallToolResult::error(format!(
                                        "cannot re-read active assignment: {e}"
                                    )));
                                }
                            };
                            if assignment.state != AssignmentState::Active {
                                return Ok(CallToolResult::error(
                                    assignment.conflict(args.base.as_deref()),
                                ));
                            }
                            if args.base.is_some()
                                && args.base.as_deref() != assignment.base.as_deref()
                            {
                                return Ok(CallToolResult::error(
                                    assignment.conflict(args.base.as_deref()),
                                ));
                            }
                            let agent_id = match assignment.agent_id.as_deref() {
                                Some(agent_id) => agent_id,
                                None => {
                                    let error = "active assignment has no agent id";
                                    let _ = assignments
                                        .stale(
                                            &assignment.assignment_id,
                                            "active_replay",
                                            error,
                                        )
                                        .await;
                                    return Ok(CallToolResult::error(error.to_string()));
                                }
                            };
                            match ctx.ledger.get_agent(agent_id).await {
                                Ok(Some(agent)) if !agent.retired => {}
                                Ok(Some(_)) => {
                                    let error = format!(
                                        "active assignment agent '{agent_id}' is retired"
                                    );
                                    let _ = assignments
                                        .stale(
                                            &assignment.assignment_id,
                                            "active_replay",
                                            &error,
                                        )
                                        .await;
                                    return Ok(CallToolResult::error(error));
                                }
                                Ok(None) => {
                                    let error = format!(
                                        "active assignment agent '{agent_id}' is missing"
                                    );
                                    let _ = assignments
                                        .stale(
                                            &assignment.assignment_id,
                                            "active_replay",
                                            &error,
                                        )
                                        .await;
                                    return Ok(CallToolResult::error(error));
                                }
                                Err(e) => {
                                    return Ok(CallToolResult::error(format!(
                                        "cannot validate active assignment agent: {e}"
                                    )));
                                }
                            }
                            if let Err(e) = repos
                                .validate_worktree_at(
                                    Path::new(&assignment.worktree),
                                    &assignment.branch,
                                    Path::new(&assignment.bare_path),
                                )
                                .await
                            {
                                let error = e.to_string();
                                let _ = assignments
                                    .stale(
                                        &assignment.assignment_id,
                                        "active_replay",
                                        &error,
                                    )
                                    .await;
                                return Ok(CallToolResult::error(format!(
                                    "active assignment is stale: {error}"
                                )));
                            }
                            return Ok(CallToolResult::json(assignment.response(false)));
                        }
                        return Ok(CallToolResult::error(
                            assignment.conflict(args.base.as_deref()),
                        ));
                    }

                    let base = match &args.base {
                        Some(base) => base.clone(),
                        None => {
                            let repository = github_repo(&args.repo);
                            match gh(
                                &repos.gh_binary,
                                None,
                                &[
                                    "repo",
                                    "view",
                                    &repository,
                                    "--json",
                                    "defaultBranchRef",
                                    "--jq",
                                    ".defaultBranchRef.name",
                                ],
                            )
                            .await
                            {
                                Ok(base) if !base.is_empty() => base,
                                Ok(_) => {
                                    let error = "default branch lookup returned an empty name";
                                    let persistence = assignments
                                        .record_pre_resource_failure(
                                            &assignment.assignment_id,
                                            "default_branch",
                                            error,
                                        )
                                        .await;
                                    return Ok(CallToolResult::error(match persistence {
                                        Ok(()) => error.to_string(),
                                        Err(e) => format!("{error}; {e}"),
                                    }));
                                }
                                Err(e) => {
                                    let error = e.to_string();
                                    let persistence = assignments
                                        .record_pre_resource_failure(
                                            &assignment.assignment_id,
                                            "default_branch",
                                            &error,
                                        )
                                        .await;
                                    return Ok(CallToolResult::error(match persistence {
                                        Ok(()) => {
                                            format!("default branch lookup failed: {error}")
                                        }
                                        Err(e) => format!(
                                            "default branch lookup failed: {error}; {e}"
                                        ),
                                    }));
                                }
                            }
                        }
                    };
                    if let Err(e) = assignments.set_base(&assignment.assignment_id, &base).await
                    {
                        let error = e.to_string();
                        let _ = assignments
                            .stale(&assignment.assignment_id, "persist_base", &error)
                            .await;
                        return Ok(CallToolResult::error(error));
                    }
                    assignment.base = Some(base.clone());
                    if let Err(e) = assignments
                        .set_phase(&assignment.assignment_id, "worktree")
                        .await
                    {
                        let error = e.to_string();
                        let recorded = assignments
                            .stale(&assignment.assignment_id, "persist_worktree_phase", &error)
                            .await;
                        let suffix = recorded
                            .err()
                            .map(|record| {
                                format!("; stale-state persistence also failed: {record}")
                            })
                            .unwrap_or_default();
                        return Ok(CallToolResult::error(format!("{error}{suffix}")));
                    }
                    let (worktree, branch) = match repos
                        .add_worktree(
                            &args.repo,
                            &assignment.slug,
                            &base,
                            &assignment.branch,
                        )
                        .await
                    {
                        Ok(pair) => pair,
                        Err(e) => {
                            let error = e.to_string();
                            let recorded = assignments
                                .stale(&assignment.assignment_id, "worktree", &error)
                                .await;
                            let suffix = recorded
                                .err()
                                .map(|record| format!("; stale-state persistence also failed: {record}"))
                                .unwrap_or_default();
                            return Ok(CallToolResult::error(format!("{error}{suffix}")));
                        }
                    };
                    if worktree.as_path() != Path::new(&assignment.worktree)
                        || branch != assignment.branch
                    {
                        let error = "repository provisioner returned resources different from the durable claim";
                        let _ = assignments
                            .stale(&assignment.assignment_id, "worktree_identity", error)
                            .await;
                        return Ok(CallToolResult::error(error.to_string()));
                    }
                    let base_head = match git_output(
                        &worktree,
                        &["rev-parse", "--verify", "HEAD^{commit}"],
                    )
                    .await
                    {
                        Ok(head) => head,
                        Err(e) => {
                            let error = format!("cannot capture assignment base commit: {e}");
                            let _ = assignments
                                .stale(&assignment.assignment_id, "base_head", &error)
                                .await;
                            return Ok(CallToolResult::error(error));
                        }
                    };
                    if let Err(e) = assignments
                        .set_base_head(&assignment.assignment_id, &base_head)
                        .await
                    {
                        let error = e.to_string();
                        let _ = assignments
                            .stale(&assignment.assignment_id, "persist_base_head", &error)
                            .await;
                        return Ok(CallToolResult::error(error));
                    }
                    let args_map = std::collections::HashMap::from([
                        ("repo".to_string(), args.repo.clone()),
                        ("issue".to_string(), args.issue.to_string()),
                        ("worktree".to_string(), worktree.display().to_string()),
                    ]);
                    let mut def = roles.to_def(&role, &args_map);
                    def.name = format!("impl-{}", assignment.slug);

                    let activation: Result<(), FlatError> = async {
                        let mut tx = ctx.pool.begin_with("BEGIN IMMEDIATE").await?;
                        let agent_id = ctx
                            .ledger
                            .create_agent_in(
                                &mut tx,
                                &def,
                                authorization.spawned_by.as_deref(),
                            )
                            .await?;
                        let related = serde_json::to_string(&[&agent_id])?;
                        let done = sqlx::query(
                            "UPDATE repo_worker_assignments
                             SET state = 'active', phase = 'ready', agent_id = ?2,
                                 related_agent_ids = ?3, base_head = ?4, updated_unix = ?5
                             WHERE assignment_id = ?1 AND state = 'preparing'",
                        )
                        .bind(&assignment.assignment_id)
                        .bind(&agent_id)
                        .bind(related)
                        .bind(&base_head)
                        .bind(ciacola_core::now_unix())
                        .execute(&mut *tx)
                        .await?;
                        if done.rows_affected() != 1 {
                            return Err("assignment activation lost its preparing claim".into());
                        }
                        tx.commit().await?;
                        Ok(())
                    }
                    .await;
                    if let Err(e) = activation {
                        let error = e.to_string();
                        let recorded = assignments
                            .stale(&assignment.assignment_id, "agent_activation", &error)
                            .await;
                        let suffix = recorded
                            .err()
                            .map(|record| {
                                format!("; stale-state persistence also failed: {record}")
                            })
                            .unwrap_or_default();
                        return Ok(CallToolResult::error(format!("{error}{suffix}")));
                    }
                    assignment = match assignments.get_by_id(&assignment.assignment_id).await {
                        Ok(Some(active)) if active.state == AssignmentState::Active => active,
                        Ok(Some(other)) => {
                            return Ok(CallToolResult::error(format!(
                                "assignment committed in unexpected state '{}'",
                                other.state.as_str()
                            )));
                        }
                        Ok(None) => {
                            return Ok(CallToolResult::error(
                                "activated assignment is not discoverable".to_string(),
                            ));
                        }
                        Err(e) => return Ok(CallToolResult::error(e.to_string())),
                    };
                    Ok(CallToolResult::json(assignment.response(true)))
                },
            )
            .build()
    };

    let list = {
        let repos = repos.clone();
        let pool = ctx.pool.clone();
        let branch_policies = branch_policies.clone();
        ToolBuilder::new("worktrees")
            .description("Durable repository assignments and worktrees the system holds.")
            .read_only()
            .no_params_handler(move || {
                let repos = repos.clone();
                let pool = pool.clone();
                let branch_policies = branch_policies.clone();
                async move {
                    let worktrees = match repos.worktrees() {
                        Ok(worktrees) => worktrees,
                        Err(e) => return Ok(CallToolResult::error(e.to_string())),
                    };
                    let assignments = match AssignmentDb::new(pool).list().await {
                        Ok(assignments) => assignments,
                        Err(e) => return Ok(CallToolResult::error(e.to_string())),
                    };
                    let owned: std::collections::HashSet<PathBuf> = assignments
                        .iter()
                        .filter(|a| a.state != AssignmentState::Completed)
                        .map(|a| PathBuf::from(&a.worktree))
                        .collect();
                    Ok(CallToolResult::json(json!({
                        "root": repos.root.display().to_string(),
                        "default_branch_policy": DEFAULT_BRANCH_TEMPLATE,
                        "branch_policies": branch_policies.configured_state(),
                        "worktrees": worktrees
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>(),
                        "orphans": worktrees
                            .iter()
                            .filter(|p| !owned.contains(*p))
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>(),
                        "assignments": assignments.iter().map(|a| json!({
                            "assignment_id": a.assignment_id,
                            "repo": a.repo,
                            "issue": a.issue,
                            "state": a.state.as_str(),
                            "phase": a.phase,
                            "agent_id": a.agent_id,
                            "spawned_by": a.spawned_by,
                            "base": a.base,
                            "base_head": a.base_head,
                            "branch": a.branch,
                            "branch_policy": a.branch_policy,
                            "expected_head": a.expected_head,
                            "pushed_head": a.pushed_head,
                            "publication_state": a.publication_state.as_str(),
                            "pr": a.pr,
                            "pr_url": a.pr_url,
                            "pr_state": a.pr_state.map(PrState::as_str),
                            "pr_draft": a.pr_draft,
                            "pr_head": a.pr_head,
                            "pr_base": a.pr_base,
                            "pr_checked_unix": a.pr_checked_unix,
                            "cleanup_state": a.cleanup_state.as_str(),
                            "cleanup_head": a.cleanup_head,
                            "cleanup_reason": a.cleanup_reason.map(CleanupReason::as_str),
                            "worktree": a.worktree,
                            "bare_path": a.bare_path,
                            "last_error": a.last_error,
                            "created_unix": a.created_unix,
                            "updated_unix": a.updated_unix,
                            "terminal_unix": a.terminal_unix,
                        })).collect::<Vec<_>>(),
                    })))
                }
            })
            .build()
    };

    let mut tools = vec![start, list];

    // Writing to the outside world is the operator's surface only.
    // An agent asks for a pull request by finishing its work and
    // saying so; a person, or a supervising manager on the stdio
    // side, decides whether one gets opened.
    if surface == Surface::Operator {
        let ctx_pr = ctx.clone();
        let repos_pr = repos.clone();
        tools.push(
            ToolBuilder::new("open_pr")
                .description(
                    "Publish one exact, durably pinned commit and open or \
                     reconcile its pull request. Existing open, closed, or \
                     merged PRs are returned rather than duplicated.",
                )
                .destructive()
                .handler(move |args: OpenPrArgs| {
                    let ctx = ctx_pr.clone();
                    let repos = repos_pr.clone();
                    async move {
                        let _lifecycle = repos.lifecycle.lock().await;
                        match publish_assignment(&ctx, &repos, &args).await {
                            Ok(value) => Ok(CallToolResult::json(value)),
                            Err(error) => Ok(CallToolResult::error(error.to_string())),
                        }
                    }
                })
                .build(),
        );

        let ctx_fin = ctx.clone();
        let repos_fin = repos.clone();
        tools.push(
            ToolBuilder::new("finish_issue")
                .description(
                    "Retire the agent, then retain its worktree or remove \
                     only work proven merged/unchanged or confirmed by an \
                     exact discard_head.",
                )
                .destructive()
                .handler(move |args: FinishArgs| {
                    let (ctx, repos) = (ctx_fin.clone(), repos_fin.clone());
                    async move {
                        let _lifecycle = repos.lifecycle.lock().await;
                        let assignments = AssignmentDb::new(ctx.pool.clone());
                        let found = match (&args.agent_id, &args.assignment_id) {
                            (Some(agent_id), None) => assignments.get_by_agent(agent_id).await,
                            (None, Some(assignment_id)) => {
                                assignments.get_by_id(assignment_id).await
                            }
                            (Some(_), Some(_)) => {
                                return Ok(CallToolResult::error(
                                    "pass agent_id or assignment_id, not both".to_string(),
                                ));
                            }
                            (None, None) => {
                                return Ok(CallToolResult::error(
                                    "agent_id or assignment_id is required".to_string(),
                                ));
                            }
                        };
                        let a = match found {
                            Ok(Some(a)) => a,
                            Ok(None) => {
                                return Ok(CallToolResult::error("no assignment".to_string()));
                            }
                            Err(e) => return Ok(CallToolResult::error(e.to_string())),
                        };
                        let keep = args.keep.unwrap_or(false);
                        if keep && args.discard_head.is_some() {
                            return Ok(CallToolResult::error(
                                "keep=true cannot be combined with discard_head".to_string(),
                            ));
                        }
                        match a.state {
                            AssignmentState::Retained if keep => {
                                return Ok(CallToolResult::json(json!({
                                    "assignment_id": a.assignment_id,
                                    "state": "retained",
                                    "worktree_removed": false,
                                    "agent_retired": true,
                                    "pr": a.pr,
                                    "cleanup_state": a.cleanup_state.as_str(),
                                    "cleanup_head": a.cleanup_head,
                                    "cleanup_reason": a.cleanup_reason.map(CleanupReason::as_str),
                                })));
                            }
                            AssignmentState::Completed if !keep => {
                                return Ok(CallToolResult::json(json!({
                                    "assignment_id": a.assignment_id,
                                    "state": "completed",
                                    "worktree_removed": true,
                                    "branch_removed": true,
                                    "agent_retired": true,
                                    "pr": a.pr,
                                    "cleanup_state": a.cleanup_state.as_str(),
                                    "cleanup_head": a.cleanup_head,
                                    "cleanup_reason": a.cleanup_reason.map(CleanupReason::as_str),
                                })));
                            }
                            AssignmentState::Active
                            | AssignmentState::Finishing
                            | AssignmentState::Retained
                            | AssignmentState::Stale
                                if !keep || a.state != AssignmentState::Retained => {}
                            _ => return Ok(CallToolResult::error(a.conflict(None))),
                        }
                        if !keep {
                            match assignments.conflicting_resources(&a).await {
                                Ok(conflicts) if conflicts.is_empty() => {}
                                Ok(conflicts) => {
                                    return Ok(CallToolResult::error(format!(
                                        "assignment '{}' shares its worktree or branch with assignments {}; refusing ambiguous cleanup",
                                        a.assignment_id,
                                        conflicts.join(", ")
                                    )));
                                }
                                Err(e) => return Ok(CallToolResult::error(e.to_string())),
                            }
                        }
                        if a.related_agent_ids.len() > 1 {
                            return Ok(CallToolResult::error(format!(
                                "assignment '{}' has ambiguous legacy owners {}; inspect and retire every agent before manual cleanup",
                                a.assignment_id,
                                a.related_agent_ids.join(", ")
                            )));
                        }
                        let plan = if keep {
                            None
                        } else {
                            match cleanup_plan(
                                &assignments,
                                &repos,
                                &a,
                                args.discard_head.as_deref(),
                            )
                            .await
                            {
                                Ok(plan) => Some(plan),
                                Err(error) => {
                                    return Ok(CallToolResult::error(error.to_string()));
                                }
                            }
                        };
                        let prior_state = if a.state == AssignmentState::Retained
                            || a.phase == "finishing_remove_retained"
                        {
                            AssignmentState::Retained
                        } else if a.state == AssignmentState::Stale {
                            AssignmentState::Stale
                        } else {
                            AssignmentState::Active
                        };
                        match assignments
                            .begin_finish(
                                &a,
                                keep,
                                plan.as_ref().and_then(|plan| plan.head.as_deref()),
                                plan.as_ref().map(|plan| plan.reason),
                            )
                            .await
                        {
                            Ok(true) => {}
                            Ok(false) => {
                                let current = assignments
                                    .get_by_id(&a.assignment_id)
                                    .await
                                    .ok()
                                    .flatten()
                                    .unwrap_or(a);
                                return Ok(CallToolResult::error(current.conflict(None)));
                            }
                            Err(e) => return Ok(CallToolResult::error(e.to_string())),
                        }
                        // Retire before touching the worktree, and
                        // `retire_agent` is the proof: it flips the
                        // row atomically with the check that no turn
                        // is `queued` or `running`, so `true` here
                        // means idle, not merely "we asked". Removing
                        // the directory first, as this used to, could
                        // delete a mid-turn agent's working tree out
                        // from under it and only afterwards discover
                        // retirement had refused.
                        // A prior cleanup attempt may have retired
                        // successfully and then failed in git or the
                        // store. Treat that as proof too, so the
                        // retained assignment is actually retryable.
                        let Some(agent_id) = a.agent_id.as_deref() else {
                            let resumable_remove = a.state == AssignmentState::Stale
                                || (a.state == AssignmentState::Finishing
                                    && matches!(
                                        a.phase.as_str(),
                                        "finishing_remove"
                                            | "finishing_remove_retained"
                                            | "finishing_remove_stale"
                                    ));
                            if resumable_remove && !keep {
                                let plan = plan.as_ref().expect("remove has cleanup plan");
                                let removed = match repos
                                    .remove_worktree_at(
                                        &a.branch,
                                        Path::new(&a.worktree),
                                        Path::new(&a.bare_path),
                                        plan.head.as_deref(),
                                    )
                                    .await
                                {
                                    Ok(()) => true,
                                    Err(e) => {
                                        let error = e.to_string();
                                        let _ = assignments
                                            .stale(&a.assignment_id, "cleanup", &error)
                                            .await;
                                        return Ok(CallToolResult::error(format!(
                                            "stale assignment cleanup failed: {error}"
                                        )));
                                    }
                                };
                                if let Err(e) = assignments
                                    .terminal(
                                        &a.assignment_id,
                                        AssignmentState::Completed,
                                        "cleanup_complete",
                                        None,
                                    )
                                    .await
                                {
                                    return Ok(CallToolResult::error(e.to_string()));
                                }
                                return Ok(CallToolResult::json(json!({
                                    "assignment_id": a.assignment_id,
                                    "state": "completed",
                                    "worktree_removed": removed,
                                    "branch_removed": true,
                                    "agent_retired": false,
                                    "pr": a.pr,
                                    "cleanup_state": "completed",
                                    "cleanup_head": plan.head,
                                    "cleanup_reason": plan.reason.as_str(),
                                })));
                            }
                            return Ok(CallToolResult::error(
                                "assignment has no agent; clean it by assignment_id with keep=false"
                                    .to_string(),
                            ));
                        };
                        let mut retired = match ctx.ledger.get_agent(agent_id).await {
                            Ok(Some(agent)) => agent.retired,
                            Ok(None) if a.state == AssignmentState::Stale && !keep => true,
                            Ok(None) => {
                                let error = format!("agent '{agent_id}' is missing");
                                let _ = assignments
                                    .stale(&a.assignment_id, "finish_agent_missing", &error)
                                    .await;
                                return Ok(CallToolResult::error(error));
                            }
                            Err(e) => {
                                let error = format!("cannot read agent for finish: {e}");
                                let _ = assignments
                                    .stale(&a.assignment_id, "finish_agent_read", &error)
                                    .await;
                                return Ok(CallToolResult::error(error));
                            }
                        };
                        if !retired {
                            retired = match ctx.ledger.retire_agent(agent_id).await {
                                Ok(retired) => retired,
                                Err(e) => {
                                    let error = format!("cannot retire agent: {e}");
                                    let _ = assignments
                                        .stale(
                                            &a.assignment_id,
                                            "finish_agent_retire",
                                            &error,
                                        )
                                        .await;
                                    return Ok(CallToolResult::error(error));
                                }
                            };
                        }
                        if !retired {
                            retired = match ctx.ledger.get_agent(agent_id).await {
                                Ok(Some(agent)) => agent.retired,
                                Ok(None) => false,
                                Err(e) => {
                                    let error = format!("cannot verify agent retirement: {e}");
                                    let _ = assignments
                                        .stale(
                                            &a.assignment_id,
                                            "finish_agent_verify",
                                            &error,
                                        )
                                        .await;
                                    return Ok(CallToolResult::error(error));
                                }
                            };
                        }
                        if !retired {
                            if let Err(e) = assignments.restore_after_busy(&a).await {
                                return Ok(CallToolResult::error(format!(
                                    "agent is busy and restoring assignment state failed: {e}"
                                )));
                            }
                            return Ok(CallToolResult::error(
                                "agent still has a queued or running turn; \
                                 finish once it goes idle"
                                    .to_string(),
                            ));
                        }
                        if let Some(plan) = plan.as_ref()
                            && let Err(error) =
                                validate_cleanup_resources(&repos, &a, plan).await
                        {
                            let message = error.to_string();
                            let _ = assignments
                                .stale(&a.assignment_id, "cleanup_recheck", &message)
                                .await;
                            return Ok(CallToolResult::error(format!(
                                "agent retired, but cleanup authorization is stale: {message}"
                            )));
                        }
                        let removed = if keep {
                            false
                        } else {
                            let plan = plan.as_ref().expect("remove has cleanup plan");
                            if let Err(e) = repos
                                .remove_worktree_at(
                                    &a.branch,
                                    Path::new(&a.worktree),
                                    Path::new(&a.bare_path),
                                    plan.head.as_deref(),
                                )
                                .await
                            {
                                let error = e.to_string();
                                let _ = assignments
                                    .stale(&a.assignment_id, "cleanup", &error)
                                    .await;
                                return Ok(CallToolResult::error(format!(
                                    "agent retired, but cleanup failed; assignment is stale: {error}"
                                )));
                            }
                            true
                        };
                        let state = if keep {
                            AssignmentState::Retained
                        } else {
                            AssignmentState::Completed
                        };
                        if let Err(e) = assignments
                            .terminal(
                                &a.assignment_id,
                                state,
                                if keep { "retained" } else { "cleanup_complete" },
                                None,
                            )
                            .await
                        {
                            let error = e.to_string();
                            let _ = assignments
                                .stale(
                                    &a.assignment_id,
                                    if keep {
                                        "finish_terminal_keep"
                                    } else if prior_state == AssignmentState::Retained {
                                        "finish_terminal_remove_retained"
                                    } else {
                                        "finish_terminal_remove"
                                    },
                                    &error,
                                )
                                .await;
                            return Ok(CallToolResult::error(format!(
                                "agent retired and cleanup applied, but recording '{}' failed: {e}",
                                state.as_str()
                            )));
                        }
                        Ok(CallToolResult::json(json!({
                            "assignment_id": a.assignment_id,
                            "state": state.as_str(),
                            "worktree_removed": removed,
                            "branch_removed": !keep,
                            "agent_retired": retired,
                            "pr": a.pr,
                            "cleanup_state": if keep { "retained" } else { "completed" },
                            "cleanup_head": plan.as_ref().and_then(|plan| plan.head.as_deref()),
                            "cleanup_reason": plan.as_ref().map(|plan| plan.reason.as_str()),
                        })))
                    }
                })
                .build(),
        );
    }
    tools
}
