//! Git state, when an agent happens to be working in a repository.
//!
//! The third plugin shape, after SQL-backed (`items`) and key-value
//! (`refs`): **stateless**. No tables, no migrations, no store. Every
//! answer is read live from the filesystem, because a cached branch
//! name is worse than no branch name.
//!
//! This is a raw surface on purpose. It reports what git says about an
//! agent's working directory and stops there; deciding whether a dirty
//! tree means "in progress" or "abandoned" is the reader's job, agent
//! or person.
//!
//! The conditional is the whole design: agents with no `working_dir`,
//! or one that is not a repository, contribute nothing and cost
//! nothing. A repo-manager pointed at a checkout lights up; a
//! summarizer spoke stays blank.

use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tower_mcp::{
    CallToolResult, ReadResourceResult, Resource, ResourceBuilder, ResourceContent, Tool,
    ToolBuilder,
};

use ciacola_core::agent::FlatError;
use ciacola_core::ledger::Ledger;
use ciacola_core::plugin::{BoxFut, Plugin, PluginContext, Section, Surface};

/// What git says about one working directory.
#[derive(Debug, Clone)]
pub struct RepoState {
    pub agent_id: String,
    pub name: String,
    pub dir: String,
    pub branch: String,
    pub head: String,
    /// Files with uncommitted changes.
    pub dirty_files: usize,
    pub insertions: u64,
    pub deletions: u64,
    /// Relative to the upstream branch, when there is one.
    pub ahead: u64,
    pub behind: u64,
    pub upstream: Option<String>,
}

async fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `None` when the directory is missing or is not a repository, which
/// is the common case and not an error.
async fn read_repo(agent_id: &str, name: &str, dir: &Path) -> Option<RepoState> {
    if git(dir, &["rev-parse", "--is-inside-work-tree"]).await? != "true" {
        return None;
    }

    let branch = git(dir, &["branch", "--show-current"])
        .await
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "(detached)".into());
    let head = git(dir, &["rev-parse", "--short", "HEAD"])
        .await
        .unwrap_or_default();

    let dirty_files = git(dir, &["status", "--porcelain"])
        .await
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or_default();

    // Working-tree changes against HEAD, staged and unstaged together.
    let (insertions, deletions) = git(dir, &["diff", "HEAD", "--numstat"])
        .await
        .map(|s| {
            s.lines().fold((0u64, 0u64), |(add, del), line| {
                let mut cols = line.split('\t');
                let a: u64 = cols.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let d: u64 = cols.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                (add + a, del + d)
            })
        })
        .unwrap_or_default();

    let upstream = git(dir, &["rev-parse", "--abbrev-ref", "@{upstream}"]).await;
    let (ahead, behind) = match &upstream {
        Some(_) => git(
            dir,
            &["rev-list", "--left-right", "--count", "@{upstream}...HEAD"],
        )
        .await
        .map(|s| {
            let mut parts = s.split_whitespace();
            let behind = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let ahead = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            (ahead, behind)
        })
        .unwrap_or((0, 0)),
        None => (0, 0),
    };

    Some(RepoState {
        agent_id: agent_id.to_string(),
        name: name.to_string(),
        dir: dir.display().to_string(),
        branch,
        head,
        dirty_files,
        insertions,
        deletions,
        ahead,
        behind,
        upstream,
    })
}

fn repo_json(r: &RepoState) -> serde_json::Value {
    json!({
        "agent_id": r.agent_id,
        "name": r.name,
        "dir": r.dir,
        "branch": r.branch,
        "head": r.head,
        "dirty_files": r.dirty_files,
        "insertions": r.insertions,
        "deletions": r.deletions,
        "upstream": r.upstream,
        "ahead": r.ahead,
        "behind": r.behind,
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RepoArgs {
    /// Whose working directory to look at. Omit for every agent that
    /// has one.
    agent_id: Option<String>,
}

#[derive(Default)]
pub struct GitPlugin {
    ledger: Option<Ledger>,
}

impl GitPlugin {
    async fn states(&self, only: Option<&str>) -> Vec<RepoState> {
        let Some(ledger) = &self.ledger else {
            return Vec::new();
        };
        let agents = ledger.list_agents().await.unwrap_or_default();
        let mut out = Vec::new();
        for agent in agents {
            if only.is_some_and(|id| id != agent.agent_id) {
                continue;
            }
            let Some(dir) = &agent.def.working_dir else {
                continue;
            };
            if let Some(state) = read_repo(&agent.agent_id, &agent.name, dir).await {
                out.push(state);
            }
        }
        out
    }
}

impl Plugin for GitPlugin {
    fn name(&self) -> &'static str {
        "git"
    }

    // Stateless: no tables(), no migrations(), no Store. Everything is
    // read from the filesystem when asked.

    fn setup<'a>(&'a mut self, ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async move {
            self.ledger = Some(ctx.ledger.clone());
            Ok(())
        })
    }

    fn tools(&self, _surface: Surface) -> Vec<Tool> {
        let ledger = self.ledger.clone();
        vec![
            ToolBuilder::new("repo_state")
                .description(
                    "Git state of an agent's working directory: branch, \
                     head, uncommitted files, diff size, and distance \
                     from upstream. Empty for agents not working in a \
                     repository.",
                )
                .read_only()
                .handler(move |args: RepoArgs| {
                    let plugin = GitPlugin {
                        ledger: ledger.clone(),
                    };
                    async move {
                        let states = plugin.states(args.agent_id.as_deref()).await;
                        Ok(CallToolResult::json(json!({
                            "repos": states.iter().map(repo_json).collect::<Vec<_>>()
                        })))
                    }
                })
                .build(),
        ]
    }

    fn resources(&self) -> Vec<Resource> {
        let ledger = self.ledger.clone();
        vec![
            ResourceBuilder::new("ciacola://repos")
                .name("repos")
                .description("Git state of every agent working in a repository.")
                .mime_type("application/json")
                .handler(move || {
                    let plugin = GitPlugin {
                        ledger: ledger.clone(),
                    };
                    async move {
                        let states = plugin.states(None).await;
                        Ok(ReadResourceResult {
                            contents: vec![ResourceContent {
                                uri: "ciacola://repos".to_string(),
                                mime_type: Some("application/json".to_string()),
                                text: Some(
                                    json!(states.iter().map(repo_json).collect::<Vec<_>>())
                                        .to_string(),
                                ),
                                blob: None,
                                meta: None,
                            }],
                            ..Default::default()
                        })
                    }
                })
                .build(),
        ]
    }

    fn board_section(&self) -> BoxFut<'_, Option<Section>> {
        Box::pin(async move {
            let states = self.states(None).await;
            if states.is_empty() {
                return None;
            }
            let mut html = String::from(
                "<table><tr><th>agent</th><th>branch</th><th>head</th>\
                 <th class=\"num\">changes</th><th class=\"num\">dirty</th>\
                 <th>upstream</th></tr>",
            );
            for r in &states {
                html.push_str(&format!(
                    "<tr><td><a href=\"/board/agent/{id}\">{name}</a></td>\
                     <td class=\"mono\">{branch}</td><td class=\"dim mono\">{head}</td>\
                     <td class=\"num\"><span style=\"color:#3fb950\">+{ins}</span> \
                     <span style=\"color:#f85149\">-{del}</span></td>\
                     <td class=\"num\">{dirty}</td><td class=\"dim mono\">{upstream}</td></tr>",
                    id = ciacola_core::render::esc(&r.agent_id),
                    name = ciacola_core::render::esc(&r.name),
                    branch = ciacola_core::render::esc(&r.branch),
                    head = ciacola_core::render::esc(&r.head),
                    ins = r.insertions,
                    del = r.deletions,
                    dirty = r.dirty_files,
                    upstream = ciacola_core::render::esc(&match &r.upstream {
                        Some(u) if r.ahead > 0 || r.behind > 0 =>
                            format!("{u} (+{}/-{})", r.ahead, r.behind),
                        Some(u) => u.clone(),
                        None => "-".into(),
                    }),
                ));
            }
            html.push_str("</table>");
            Some(Section {
                title: "repositories".into(),
                html,
            })
        })
    }

    fn health(&self) -> BoxFut<'_, serde_json::Value> {
        Box::pin(async move {
            let states = self.states(None).await;
            json!({
                "repos": states.len(),
                "dirty": states.iter().filter(|r| r.dirty_files > 0).count(),
            })
        })
    }
}
