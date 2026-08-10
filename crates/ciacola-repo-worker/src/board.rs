//! The board section and health report: what an operator sees of
//! this plugin without opening MCP.

use serde_json::json;

use ciacola_core::plugin::{BoxFut, Section};

use std::path::{Path, PathBuf};

use ciacola_core::ledger::Ledger;

use crate::RepoWorkerPlugin;
use crate::assignment::{AssignmentState, CleanupReason, CleanupState, PrState, PublicationState};
use crate::repos::Repos;

pub(crate) fn board_section(plugin: &RepoWorkerPlugin) -> BoxFut<'_, Option<Section>> {
    Box::pin(async move {
        let all = match plugin.assignments().await {
            Ok(all) => all,
            Err(e) => {
                return Some(Section {
                    title: "repository work (degraded)".into(),
                    html: format!(
                        "<p class=\"error\">assignment query failed: {}</p>",
                        ciacola_core::render::esc(&e.to_string())
                    ),
                });
            }
        };
        if all.is_empty() {
            return None;
        }
        let ledger: Option<Ledger> = plugin.ctx.as_ref().map(|c| c.ledger.clone());
        let mut current_rows = String::new();
        let mut completed_rows = String::new();
        let mut current_count = 0usize;
        let mut completed_count = 0usize;
        for a in &all {
            let agent = match (&ledger, a.agent_id.as_deref()) {
                (Some(l), Some(agent_id)) => {
                    let name = l
                        .get_agent(agent_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|x| x.name)
                        .unwrap_or_else(|| agent_id.to_string());
                    format!(
                        "<a href=\"/board/agent/{}\">{}</a>",
                        ciacola_core::render::esc(agent_id),
                        ciacola_core::render::esc(&name)
                    )
                }
                (_, Some(agent_id)) => ciacola_core::render::esc(agent_id),
                _ => "-".into(),
            };
            let publication = match a.pr_state {
                Some(pr_state) => {
                    format!("{} / {}", a.publication_state.as_str(), pr_state.as_str())
                }
                None => a.publication_state.as_str().to_string(),
            };
            let pr = match (a.pr, a.pr_url.as_deref()) {
                (Some(number), Some(url)) => format!(
                    "<a href=\"{}\">PR #{number}</a>",
                    ciacola_core::render::esc(url)
                ),
                (Some(number), None) => format!("PR #{number}"),
                (None, _) => "no PR".into(),
            };
            let detail = a.last_error.as_deref().unwrap_or_else(|| {
                if matches!(a.state, AssignmentState::Active | AssignmentState::Retained)
                    && a.base_head.is_none()
                {
                    "missing durable base-head provenance; retain and inspect"
                } else {
                    &a.phase
                }
            });
            let issue_url = format!("https://github.com/{}/issues/{}", a.repo, a.issue);
            let row = format!(
                "<tr><td data-label=\"issue\"><a href=\"{issue_url}\">{repo}#{issue}</a><br>\
                 <span class=\"dim mono\">{assignment}</span></td>\
                 <td data-label=\"assignment\">{state}<br><span class=\"dim\">{detail}</span></td>\
                 <td data-label=\"work\">{agent}<br><span class=\"dim mono\">{branch}</span><br>\
                 <span class=\"dim\">policy {policy}</span></td>\
                 <td data-label=\"publication\">{publication}<br><span class=\"dim\">{pr}</span></td>\
                 <td data-label=\"cleanup\">{cleanup}<br><span class=\"dim\">{cleanup_detail}</span></td></tr>",
                issue_url = ciacola_core::render::esc(&issue_url),
                repo = ciacola_core::render::esc(&a.repo),
                issue = a.issue,
                assignment = ciacola_core::render::esc(
                    &a.assignment_id[a.assignment_id.len().saturating_sub(8)..]
                ),
                state = ciacola_core::render::chip(a.state.as_str()),
                detail = ciacola_core::render::esc(detail),
                agent = agent,
                branch = ciacola_core::render::esc(&a.branch),
                policy = ciacola_core::render::esc(&a.branch_policy),
                publication = ciacola_core::render::esc(&publication),
                pr = pr,
                cleanup = ciacola_core::render::chip(a.cleanup_state.as_str()),
                cleanup_detail = ciacola_core::render::esc(
                    a.cleanup_reason
                        .map(CleanupReason::as_str)
                        .unwrap_or("not settled")
                ),
            );
            if a.state == AssignmentState::Completed {
                completed_count += 1;
                if completed_count <= 5 {
                    completed_rows.push_str(&row);
                }
            } else {
                current_count += 1;
                current_rows.push_str(&row);
            }
        }
        let headings = "<tr><th scope=\"col\">issue</th><th scope=\"col\">assignment</th>\
                        <th scope=\"col\">work</th><th scope=\"col\">publication</th>\
                        <th scope=\"col\">cleanup</th></tr>";
        let mut html = if current_rows.is_empty() {
            "<p class=\"empty\">No open or retained repository journeys.</p>".into()
        } else {
            format!(
                "<p class=\"dim\">{current_count} current assignment(s)</p>\
                 <table class=\"responsive-table\"><caption class=\"sr-only\">Current repository journeys</caption>\
                 {headings}{current_rows}</table>"
            )
        };
        if completed_count > 0 {
            let completed_summary = if completed_count > 5 {
                format!("{completed_count} completed journeys; latest 5 shown")
            } else {
                format!("{completed_count} completed journey(s)")
            };
            html.push_str(&format!(
                "<details><summary>{completed_summary}</summary>\
                 <table class=\"responsive-table\"><caption class=\"sr-only\">Recently completed repository journeys</caption>\
                 {headings}{completed_rows}</table></details>"
            ));
        }
        Some(Section {
            title: "repository journeys".into(),
            html,
        })
    })
}

pub(crate) fn health(plugin: &RepoWorkerPlugin) -> BoxFut<'_, serde_json::Value> {
    Box::pin(async move {
        let all = match plugin.assignments().await {
            Ok(all) => all,
            Err(e) => return json!({"status": "degraded", "assignment_error": e.to_string()}),
        };
        let worktrees = match plugin.repos.as_ref().map(Repos::worktrees) {
            Some(Ok(worktrees)) => worktrees,
            Some(Err(e)) => {
                return json!({
                    "status": "degraded",
                    "assignments": all.len(),
                    "worktree_error": e.to_string(),
                });
            }
            None => Vec::new(),
        };
        let owned: std::collections::HashSet<PathBuf> = all
            .iter()
            .filter(|a| a.state != AssignmentState::Completed)
            .map(|a| PathBuf::from(&a.worktree))
            .collect();
        let orphan_count = worktrees
            .iter()
            .filter(|path| !owned.contains(*path))
            .count();
        let stale_count = all
            .iter()
            .filter(|a| a.state == AssignmentState::Stale)
            .count();
        let publication_failed = all
            .iter()
            .filter(|a| a.publication_state == PublicationState::Failed)
            .count();
        let unresolved_publication_failed = all
            .iter()
            .filter(|a| {
                a.publication_state == PublicationState::Failed
                    && a.state != AssignmentState::Completed
            })
            .count();
        let cleanup_failed = all
            .iter()
            .filter(|a| a.cleanup_state == CleanupState::Failed)
            .count();
        let missing_owned = all
            .iter()
            .filter(|a| {
                matches!(a.state, AssignmentState::Active | AssignmentState::Retained)
                    && !Path::new(&a.worktree).exists()
            })
            .count();
        let missing_active = all
            .iter()
            .filter(|a| a.state == AssignmentState::Active && !Path::new(&a.worktree).exists())
            .count();
        let completed_with_worktree = all
            .iter()
            .filter(|a| a.state == AssignmentState::Completed && Path::new(&a.worktree).exists())
            .count();
        let missing_journey_provenance = all
            .iter()
            .filter(|a| {
                matches!(a.state, AssignmentState::Active | AssignmentState::Retained)
                    && (a.base.is_none() || a.base_head.is_none())
            })
            .count();
        let mut agent_state_drift = 0;
        let mut agent_query_error = None;
        if let Some(ctx) = &plugin.ctx {
            for assignment in all
                .iter()
                .filter(|a| matches!(a.state, AssignmentState::Active | AssignmentState::Retained))
            {
                let expected_retired = assignment.state == AssignmentState::Retained;
                let Some(agent_id) = assignment.agent_id.as_deref() else {
                    agent_state_drift += 1;
                    continue;
                };
                match ctx.ledger.get_agent(agent_id).await {
                    Ok(Some(agent)) if agent.retired == expected_retired => {}
                    Ok(_) => agent_state_drift += 1,
                    Err(e) => {
                        agent_query_error = Some(e.to_string());
                        break;
                    }
                }
            }
        }
        let degraded = stale_count > 0
            || unresolved_publication_failed > 0
            || cleanup_failed > 0
            || orphan_count > 0
            || missing_owned > 0
            || completed_with_worktree > 0
            || missing_journey_provenance > 0
            || agent_state_drift > 0
            || agent_query_error.is_some();
        json!({
            "status": if degraded { "degraded" } else { "ok" },
            "assignments": all.len(),
            "active": all.iter().filter(|a| a.state == AssignmentState::Active).count(),
            "preparing": all.iter().filter(|a| a.state == AssignmentState::Preparing).count(),
            "finishing": all.iter().filter(|a| a.state == AssignmentState::Finishing).count(),
            "retained": all.iter().filter(|a| a.state == AssignmentState::Retained).count(),
            "completed": all.iter().filter(|a| a.state == AssignmentState::Completed).count(),
            "stale": stale_count,
            "with_pr": all.iter().filter(|a| a.pr.is_some()).count(),
            "publication": {
                "unpublished": all.iter().filter(|a| a.publication_state == PublicationState::Unpublished).count(),
                "publishing": all.iter().filter(|a| a.publication_state == PublicationState::Publishing).count(),
                "published": all.iter().filter(|a| a.publication_state == PublicationState::Published).count(),
                "failed": publication_failed,
                "unresolved_failed": unresolved_publication_failed,
            },
            "pr": {
                "open": all.iter().filter(|a| a.pr_state == Some(PrState::Open)).count(),
                "closed": all.iter().filter(|a| a.pr_state == Some(PrState::Closed)).count(),
                "merged": all.iter().filter(|a| a.pr_state == Some(PrState::Merged)).count(),
            },
            "cleanup": {
                "none": all.iter().filter(|a| a.cleanup_state == CleanupState::None).count(),
                "retaining": all.iter().filter(|a| a.cleanup_state == CleanupState::Retaining).count(),
                "retained": all.iter().filter(|a| a.cleanup_state == CleanupState::Retained).count(),
                "removing": all.iter().filter(|a| a.cleanup_state == CleanupState::Removing).count(),
                "completed": all.iter().filter(|a| a.cleanup_state == CleanupState::Completed).count(),
                "failed": cleanup_failed,
            },
            "worktrees": worktrees.len(),
            "orphans": orphan_count,
            "missing_active_worktrees": missing_active,
            "missing_owned_worktrees": missing_owned,
            "completed_with_worktree": completed_with_worktree,
            "missing_journey_provenance": missing_journey_provenance,
            "agent_state_drift": agent_state_drift,
            "agent_query_error": agent_query_error,
        })
    })
}
