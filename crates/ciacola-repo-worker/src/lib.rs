//! A bundled capability: tools, the role that knows how to use them,
//! and the isolation both depend on.
//!
//! This is the argument for plugins over config alone. Everything here
//! *could* be config: a role with a working directory, some allowed
//! tools. What config cannot express is the wiring, because starting
//! work on an issue means ensuring a clone exists, cutting a worktree,
//! and only then spawning an agent pointed at it. `start_issue` does
//! all three, which is possible precisely because the plugin knows its
//! own role and its own tools.
//!
//! **The clone is the system's, not the operator's.** Stage 7 proved
//! the mechanics (one bare clone per repository, a worktree per unit of
//! work) and flat never inherited them, so `working_dir` pointed at
//! whatever checkout a person also had open. That is not acceptable for
//! unattended work: two agents would collide in one working tree, and
//! the first surprise would be a person finding their own branch
//! switched under them. Everything here lives under the plugin's own
//! root, and the operator's checkouts are never touched.
//!
//! ```toml
//! [plugins.repo-worker]
//! root = "~/.local/share/flat/repos"      # clones and worktrees
//! repos = ["joshrotenberg/tower-mcp"]     # what may be worked on
//! ```
//!
//! External writes are gated: `open_pr` is the only tool that can
//! affect the outside world, and it is idempotent by construction (it
//! looks for a pull request from the branch before creating one) because
//! a resent or redelivered turn must not open a second one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tower_mcp::{CallToolResult, Tool, ToolBuilder};

use git_spawn::{CloneCommand, GitCommand, Repository, WorktreeCommand};

use ciacola_core::agent::FlatError;
use ciacola_core::ledger::Ledger;
use ciacola_core::plugin::{BoxFut, Plugin, PluginContext, Section, Surface};
use ciacola_core::roles::{Role, Roles};

const ROLE: &str = "issue-implementer";
/// The other half of the loop: whoever dispatches work is also who
/// notices what the implementer prompt got wrong, and that only turns
/// into a better prompt if it is somebody's stated job.
const MANAGER: &str = "repo-manager";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoWorkerConfig {
    /// Where clones and worktrees live. `~` is expanded.
    root: Option<String>,
    /// Repositories that may be worked on, `owner/name`. An empty list
    /// means none: this plugin does not get to pick.
    #[serde(default)]
    repos: Vec<String>,
}

fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => PathBuf::from(home).join(rest),
            Err(_) => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    }
}

/// git-spawn, dogfooded, in place of hand-rolled `Command` calls.
///
/// One gap found immediately by using it: `Repository::open` requires a
/// `.git` entry, so it rejects a bare repository, which is exactly what
/// this plugin keeps. `new_unchecked` is the way through and is
/// documented for a different purpose (about to init or clone). Filed
/// upstream as joshrotenberg/git-spawn#157.
fn bare_repo(path: &Path) -> Repository {
    Repository::new_unchecked(path)
}

async fn gh(dir: Option<&Path>, args: &[&str]) -> Result<String, FlatError> {
    let mut command = tokio::process::Command::new("gh");
    command.args(args).kill_on_drop(true);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let out = command.output().await?;
    if !out.status.success() {
        return Err(format!(
            "gh {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[derive(Clone)]
struct Repos {
    root: PathBuf,
    allowed: Arc<Vec<String>>,
    /// Held across the clone, never merely around the check. Stage 7
    /// shipped the released-too-early version of this lock and a second
    /// run raced ahead to a bare repository that did not exist yet.
    cloning: Arc<tokio::sync::Mutex<()>>,
}

impl Repos {
    fn bare(&self, repo: &str) -> PathBuf {
        self.root.join(format!("{}.git", repo.replace('/', "__")))
    }

    fn allows(&self, repo: &str) -> bool {
        self.allowed.iter().any(|r| r == repo)
    }

    /// Clone once into the plugin's own root, then reuse.
    async fn ensure_clone(&self, repo: &str) -> Result<PathBuf, FlatError> {
        let bare = self.bare(repo);
        let _guard = self.cloning.lock().await;
        if bare.exists() {
            // The refspec is not optional, and its absence is silent.
            // `git clone --bare` writes no `remote.origin.fetch`, so a
            // plain `git fetch origin` against a bare repository updates
            // FETCH_HEAD and leaves `refs/heads/*` exactly where they
            // were. The clone's `main` then never moves after the day it
            // was made, and every worktree is cut from that frozen
            // commit while looking perfectly healthy.
            //
            // This shipped. Four agents worked issues against a `main`
            // three merges behind, and it surfaced only because one of
            // them reasoned about a file that had changed since.
            // No `--prune`. It looks harmless next to a refspec and is
            // the opposite: `refs/heads/*` here contains this plugin's
            // own `agent/*` branches, which by definition do not exist
            // on origin until `open_pr` pushes them, so pruning deletes
            // them. Git does that even while a worktree has one checked
            // out, and the worktree's HEAD then dangles, so the agent's
            // next commit has no parent at all.
            //
            // It was present before the refspec was and did nothing,
            // because a bare clone configures no refspec for prune to
            // work against. Supplying one armed it. Two batches ran
            // before the orphan commits were noticed.
            let mut fetch = bare_repo(&bare).fetch();
            fetch.remote("origin").refspec("+refs/heads/*:refs/heads/*");
            // Nor is the result optional. A discarded error here reads
            // exactly like a refresh that worked, which is the other
            // half of why the above went unnoticed.
            fetch
                .execute()
                .await
                .map_err(|e| -> FlatError { format!("fetch {repo}: {e}").into() })?;
            return Ok(bare);
        }
        std::fs::create_dir_all(&self.root)?;
        eprintln!("[repo-worker] cloning {repo} (once)");
        CloneCommand::new(format!("https://github.com/{repo}.git"))
            .bare()
            .directory(&bare)
            .execute()
            .await
            .map_err(|e| -> FlatError { format!("clone {repo}: {e}").into() })?;
        Ok(bare)
    }

    /// A directory and a branch for one unit of work.
    async fn add_worktree(
        &self,
        repo: &str,
        slug: &str,
        base: &str,
    ) -> Result<(PathBuf, String), FlatError> {
        let bare = self.ensure_clone(repo).await?;
        let path = self.root.join(format!("wt-{slug}"));
        let branch = format!("agent/{slug}");
        if path.exists() {
            return Ok((path, branch));
        }
        // A bare clone's local heads are the remote's, so `origin/main`
        // does not resolve here the way it would in a working clone.
        // Base off the branch directly.
        let mut add = WorktreeCommand::add(&path);
        add.new_branch(&branch).commit_ish(base);
        bare_repo(&bare)
            .worktree(add)
            .execute()
            .await
            .map_err(|e| -> FlatError { format!("worktree add: {e}").into() })?;
        Ok((path, branch))
    }

    async fn remove_worktree(&self, repo: &str, slug: &str) -> Result<(), FlatError> {
        let bare = self.bare(repo);
        let path = self.root.join(format!("wt-{slug}"));
        let mut remove = WorktreeCommand::remove(&path);
        remove.force();
        bare_repo(&bare)
            .worktree(remove)
            .execute()
            .await
            .map_err(|e| -> FlatError { format!("worktree remove: {e}").into() })?;
        let _ = bare_repo(&bare)
            .branch()
            .delete(format!("agent/{slug}"))
            .force_delete()
            .execute()
            .await;
        Ok(())
    }

    fn worktrees(&self) -> Vec<PathBuf> {
        std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("wt-"))
            })
            .collect()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartIssueArgs {
    /// `owner/name`, and it must be in the configured list.
    repo: String,
    /// Issue number to work.
    issue: u64,
    /// Branch to cut from. Defaults to the repository's default branch.
    base: Option<String>,
    /// Your agent_id, if an agent is starting this.
    spawned_by: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct OpenPrArgs {
    /// The agent whose worktree holds the work.
    agent_id: String,
    title: String,
    body: String,
    /// Open as a draft. Default true, because a machine-authored pull
    /// request should wait for a person by default.
    draft: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FinishArgs {
    /// The agent to wind up.
    agent_id: String,
    /// Keep the worktree for inspection instead of removing it.
    keep: Option<bool>,
}

#[derive(Default)]
pub struct RepoWorkerPlugin {
    repos: Option<Repos>,
    ctx: Option<PluginContext>,
    roles: Option<Roles>,
}

/// Per-agent state, keyed by agent id in the plugin's key-value slice.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
struct Assignment {
    repo: String,
    issue: u64,
    slug: String,
    branch: String,
    worktree: String,
    pr: Option<u64>,
}

impl RepoWorkerPlugin {
    fn store(&self) -> Option<ciacola_core::store::Store> {
        Some(ciacola_core::store::Store::new(
            self.ctx.as_ref()?.pool.clone(),
            "repo-worker",
        ))
    }

    async fn assignments(&self) -> Vec<(String, Assignment)> {
        match self.store() {
            Some(store) => store
                .list::<Assignment>(Some("agent/"))
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k.trim_start_matches("agent/").to_string(), v))
                .collect(),
            None => Vec::new(),
        }
    }
}

impl Plugin for RepoWorkerPlugin {
    fn name(&self) -> &'static str {
        "repo-worker"
    }

    fn setup<'a>(&'a mut self, ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async move {
            let config: RepoWorkerConfig = match ctx.config_for(self.name()) {
                Some(value) => value.clone().try_into()?,
                None => RepoWorkerConfig::default(),
            };
            let root = expand(
                config
                    .root
                    .as_deref()
                    .unwrap_or("~/.local/share/flat/repos"),
            );
            self.repos = Some(Repos {
                root,
                allowed: Arc::new(config.repos),
                cloning: Arc::new(tokio::sync::Mutex::new(())),
            });
            // The server's runtime, not Default: an empty one meant this
            // plugin's role silently opted out of every server-wide
            // setting, which is how the first real run ended up with
            // role-level hermetic but ambient credentials.
            self.roles = Some(Roles::with_runtime(
                self.roles(),
                ctx.loopback_mcp_config.clone(),
                ctx.runtime.clone(),
            ));
            self.ctx = Some(ctx.clone());
            Ok(())
        })
    }

    /// The role ships with the tools, so the prompt can assume exactly
    /// the capabilities it was given. `{{worktree}}` is filled at spawn
    /// by `start_issue`, which is the wiring config alone cannot do.
    fn roles(&self) -> Vec<Role> {
        vec![
            Role {
                name: MANAGER.into(),
                description: "Dispatches issues to implementers, checks what comes back, and \
                          curates the implementer prompt from what it sees."
                    .into(),
                model: None,
                effort: Some("high".into()),
                // Not hermetic: it edits this repository, and it is played
                // by an interactive session today, which already has the
                // operator's config and should keep it.
                hermetic: Some("none".into()),
                working_dir: Some("{{checkout}}".into()),
                allowed_tools: vec![
                    "Read".into(),
                    "Glob".into(),
                    "Grep".into(),
                    "Edit".into(),
                    "Write".into(),
                    "Bash(git:*)".into(),
                    "Bash(cargo:*)".into(),
                    "Bash(just:*)".into(),
                    "Bash(gh:*)".into(),
                ],
                max_turns: None,
                rotate_after_turns: None,
                loopback: true,
                arguments: vec!["checkout".into()],
                system_prompt: "\
You dispatch issues to implementers and you own the prompt they run on.
The second half is the part that is easy to skip, and it is why this
role exists rather than a person just calling start_issue.

Working in {{checkout}}, which is ciacola's own repository.

Dispatching:
- start_issue, then send, then wait. Pass timeout_secs; the default is
  120 and real work runs longer, and a turn cut short loses its session.
- Read the diff yourself before open_pr. The reply is the worker's
  account of what it did, which is not the same thing.
- Verify its verification. It reports running the gate; run the gate.
  The gap between what a role grants and what its agents actually use
  is only visible if someone looks.

Curating the implementer prompt, which lives in
crates/ciacola-repo-worker/src/lib.rs and needs a rebuild to take
effect:

- Change it only from a run you watched. Not from imagining how an
  agent might go wrong: that produces long prompts full of rules
  nobody needed, and every added rule dilutes the ones that matter.
- When a worker did something right that the prompt never asked for,
  make it required. Good behaviour that depends on the model's mood is
  not a feature.
- When you had to fix its reply by hand before you could use it, the
  prompt should have produced the usable form. Hand-editing twice is a
  prompt bug.
- When you add an instruction, grant the tool it needs in the same
  commit. An instruction the allowlist does not permit fails later and
  less legibly than one that is simply absent.
- When a worker used the wrong command for a repository, the fix is
  usually to tell it to read that repository's own rules, not to
  hardcode the right command here.
- Say in the commit message which run taught you the change. A prompt
  whose history reads as evidence can be argued with; one that reads as
  taste cannot."
                    .into(),
            },
            Role {
                name: ROLE.into(),
                description: "Implements one GitHub issue in its own worktree, then opens a draft \
                          pull request for review."
                    .into(),
                model: Some("sonnet".into()),
                effort: Some("high".into()),
                hermetic: Some("full".into()),
                working_dir: Some("{{worktree}}".into()),
                allowed_tools: vec![
                    "Read".into(),
                    "Glob".into(),
                    "Grep".into(),
                    "Edit".into(),
                    "Write".into(),
                    "Bash(git add:*)".into(),
                    "Bash(git commit:*)".into(),
                    "Bash(git status:*)".into(),
                    "Bash(git diff:*)".into(),
                    "Bash(cargo build:*)".into(),
                    "Bash(cargo test:*)".into(),
                    "Bash(cargo fmt:*)".into(),
                    "Bash(cargo clippy:*)".into(),
                    "Bash(cargo doc:*)".into(),
                    // Step 6 tells it to prefer the repository's own gate.
                    // An instruction without the matching grant is the same
                    // defect as a grant without the instruction, and fails
                    // later and less legibly.
                    "Bash(just:*)".into(),
                    "Bash(make:*)".into(),
                    "Bash(gh issue view:*)".into(),
                    "mcp__ciacola__track".into(),
                    "mcp__ciacola__items".into(),
                ],
                max_turns: Some(60),
                rotate_after_turns: None,
                loopback: true,
                arguments: vec!["repo".into(), "issue".into(), "worktree".into()],
                system_prompt: "\
You are implementing issue #{{issue}} of {{repo}}, working in {{worktree}}, \
which is a git worktree created for you on its own branch. Nobody else is \
in it.

Numbered steps, in order:
1. Read the issue: gh issue view {{issue}} --repo {{repo}}
2. Read the repository's own rules before its code: CONTRIBUTING.md, \
   CLAUDE.md, AGENTS.md, README.md, whichever exist. They outrank what \
   you would infer from the source, and they are where a project says \
   how it wants to be verified.
3. Read the code the issue concerns before changing anything. If the issue \
   turns out to be already fixed, or the fix is not what the issue asks \
   for, say so and stop; do not invent work.
4. Make the smallest change that resolves it. Match the surrounding code. \
   Where the issue proposes more than one approach, take the one it \
   prefers; where it proposes none and there is a real choice, say what \
   you chose against and why. If the repository keeps a record the issue \
   appears on, a known-issues list or a changelog, update it in the same \
   change: a fix that leaves its own bug documented as open is not \
   finished.
5. Cover it with a test, in whatever style the repository already uses. A \
   fix with no test is not finished. If it genuinely cannot be tested, \
   say why rather than skipping quietly.
6. Verify. If the repository has its own gate, run that: `just` when \
   there is a justfile, `make` when there is a Makefile, whatever \
   CONTRIBUTING names. It is the set CI runs, and it usually checks more \
   than the obvious three. Failing that: cargo fmt, cargo clippy, cargo \
   test. Do not proceed past a failure; fix it or report that you cannot.
7. Commit with a conventional-commit message. House rules, which apply \
   because a hermetic agent inherits none of the operator's ambient \
   config: no em dashes anywhere; no Co-Authored-By or any other author \
   trailer; no AI attribution or generated-with footer; state what \
   changed and why without editorializing. Do not push; the server \
   handles that.
8. Reply with, in this order: what you changed and why; the files; the \
   exact command you verified with and its output; then a pull request \
   title on one line, and a pull request body whose last line is \
   Closes #{{issue}} and nothing else. Those two go to open_pr exactly \
   as given, so write them to be used rather than edited.

You cannot push, open pull requests, or comment. Those are the server's \
to do, on purpose."
                    .into(),
            },
        ]
    }

    fn tools(&self, surface: Surface) -> Vec<Tool> {
        let (Some(repos), Some(ctx), Some(roles)) =
            (self.repos.clone(), self.ctx.clone(), self.roles.clone())
        else {
            return Vec::new();
        };

        let start = {
            let (repos, ctx, roles) = (repos.clone(), ctx.clone(), roles.clone());
            ToolBuilder::new("start_issue")
                .description(
                    "Begin work on a GitHub issue: ensure the system's own \
                     clone, cut a fresh worktree and branch, and spawn an \
                     implementer pointed at it. Returns the agent to send to.",
                )
                .non_destructive()
                .handler(move |args: StartIssueArgs| {
                    let (repos, ctx, roles) = (repos.clone(), ctx.clone(), roles.clone());
                    async move {
                        if !repos.allows(&args.repo) {
                            return Ok(CallToolResult::error(format!(
                                "repository '{}' is not in the configured list",
                                args.repo
                            )));
                        }
                        let slug = format!("{}-{}", args.repo.replace('/', "-"), args.issue);
                        let base = match &args.base {
                            Some(base) => base.clone(),
                            None => gh(
                                None,
                                &[
                                    "repo",
                                    "view",
                                    &args.repo,
                                    "--json",
                                    "defaultBranchRef",
                                    "--jq",
                                    ".defaultBranchRef.name",
                                ],
                            )
                            .await
                            .unwrap_or_else(|_| "main".into()),
                        };
                        let (worktree, branch) =
                            match repos.add_worktree(&args.repo, &slug, &base).await {
                                Ok(pair) => pair,
                                Err(e) => return Ok(CallToolResult::error(e.to_string())),
                            };

                        let Some(role) = roles.get(ROLE) else {
                            return Ok(CallToolResult::error("role missing".to_string()));
                        };
                        let args_map = std::collections::HashMap::from([
                            ("repo".to_string(), args.repo.clone()),
                            ("issue".to_string(), args.issue.to_string()),
                            ("worktree".to_string(), worktree.display().to_string()),
                        ]);
                        let mut def = roles.to_def(role, &args_map);
                        def.name = format!("impl-{slug}");

                        match ctx
                            .ledger
                            .create_agent(&def, args.spawned_by.as_deref())
                            .await
                        {
                            Ok(agent_id) => {
                                let assignment = Assignment {
                                    repo: args.repo.clone(),
                                    issue: args.issue,
                                    slug,
                                    branch: branch.clone(),
                                    worktree: worktree.display().to_string(),
                                    pr: None,
                                };
                                if let Some(store) =
                                    ciacola_core::store::Store::new(ctx.pool.clone(), "repo-worker")
                                        .put(&format!("agent/{agent_id}"), &assignment)
                                        .await
                                        .err()
                                {
                                    eprintln!("[repo-worker] record: {store}");
                                }
                                Ok(CallToolResult::json(json!({
                                    "agent_id": agent_id,
                                    "repo": args.repo,
                                    "issue": args.issue,
                                    "branch": branch,
                                    "worktree": worktree.display().to_string(),
                                })))
                            }
                            Err(e) => Ok(CallToolResult::error(e.to_string())),
                        }
                    }
                })
                .build()
        };

        let list = {
            let repos = repos.clone();
            ToolBuilder::new("worktrees")
                .description("Worktrees the system currently holds.")
                .read_only()
                .no_params_handler(move || {
                    let repos = repos.clone();
                    async move {
                        Ok(CallToolResult::json(json!({
                            "root": repos.root.display().to_string(),
                            "worktrees": repos
                                .worktrees()
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>()
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
            tools.push(
                ToolBuilder::new("open_pr")
                    .description(
                        "Push the agent's branch and open a draft pull \
                         request. Idempotent: if one already exists from \
                         this branch it is returned rather than duplicated.",
                    )
                    .destructive()
                    .handler(move |args: OpenPrArgs| {
                        let ctx = ctx_pr.clone();
                        async move {
                            let store =
                                ciacola_core::store::Store::new(ctx.pool.clone(), "repo-worker");
                            let key = format!("agent/{}", args.agent_id);
                            let Ok(Some(mut a)) = store.get::<Assignment>(&key).await else {
                                return Ok(CallToolResult::error(format!(
                                    "no assignment for '{}'",
                                    args.agent_id
                                )));
                            };
                            let worktree = PathBuf::from(&a.worktree);

                            // Idempotency, the whole point of this being
                            // a mechanical step: ask GitHub before
                            // creating, so a resent turn cannot open a
                            // second pull request.
                            if let Ok(existing) = gh(
                                Some(&worktree),
                                &[
                                    "pr",
                                    "list",
                                    "--repo",
                                    &a.repo,
                                    "--head",
                                    &a.branch,
                                    "--state",
                                    "all",
                                    "--json",
                                    "number",
                                    "--jq",
                                    ".[0].number",
                                ],
                            )
                            .await
                            {
                                if let Ok(number) = existing.trim().parse::<u64>() {
                                    a.pr = Some(number);
                                    let _ = store.put(&key, &a).await;
                                    return Ok(CallToolResult::json(json!({
                                        "pr": number,
                                        "created": false,
                                        "note": "already open from this branch",
                                    })));
                                }
                            }

                            let pushed = match Repository::open(&worktree) {
                                Ok(repo) => repo
                                    .push()
                                    .remote("origin")
                                    .refspec(&a.branch)
                                    .set_upstream()
                                    .execute()
                                    .await
                                    .map_err(|e| e.to_string()),
                                Err(e) => Err(e.to_string()),
                            };
                            if let Err(e) = pushed {
                                return Ok(CallToolResult::error(format!("push: {e}")));
                            }
                            let draft = args.draft.unwrap_or(true);
                            let mut cmd = vec![
                                "pr",
                                "create",
                                "--repo",
                                &a.repo,
                                "--head",
                                &a.branch,
                                "--title",
                                &args.title,
                                "--body",
                                &args.body,
                            ];
                            if draft {
                                cmd.push("--draft");
                            }
                            match gh(Some(&worktree), &cmd).await {
                                Ok(url) => {
                                    let number = url
                                        .rsplit('/')
                                        .next()
                                        .and_then(|n| n.trim().parse::<u64>().ok());
                                    a.pr = number;
                                    let _ = store.put(&key, &a).await;
                                    Ok(CallToolResult::json(json!({
                                        "pr": number,
                                        "url": url,
                                        "created": true,
                                        "draft": draft,
                                    })))
                                }
                                Err(e) => Ok(CallToolResult::error(e.to_string())),
                            }
                        }
                    })
                    .build(),
            );

            let ctx_fin = ctx.clone();
            let repos_fin = repos.clone();
            tools.push(
                ToolBuilder::new("finish_issue")
                    .description("Retire the agent and remove its worktree.")
                    .destructive()
                    .handler(move |args: FinishArgs| {
                        let (ctx, repos) = (ctx_fin.clone(), repos_fin.clone());
                        async move {
                            let store =
                                ciacola_core::store::Store::new(ctx.pool.clone(), "repo-worker");
                            let key = format!("agent/{}", args.agent_id);
                            let Ok(Some(a)) = store.get::<Assignment>(&key).await else {
                                return Ok(CallToolResult::error("no assignment".to_string()));
                            };
                            let removed = if args.keep.unwrap_or(false) {
                                false
                            } else {
                                repos.remove_worktree(&a.repo, &a.slug).await.is_ok()
                            };
                            let retired = ctx
                                .ledger
                                .retire_agent(&args.agent_id)
                                .await
                                .unwrap_or(false);
                            Ok(CallToolResult::json(json!({
                                "worktree_removed": removed,
                                "agent_retired": retired,
                                "pr": a.pr,
                            })))
                        }
                    })
                    .build(),
            );
        }
        tools
    }

    fn board_section(&self) -> BoxFut<'_, Option<Section>> {
        Box::pin(async move {
            let all = self.assignments().await;
            if all.is_empty() {
                return None;
            }
            let ledger: Option<Ledger> = self.ctx.as_ref().map(|c| c.ledger.clone());
            let mut html = String::from(
                "<table><tr><th>issue</th><th>agent</th><th>branch</th><th>pr</th>\
                 <th>worktree</th></tr>",
            );
            for (agent_id, a) in &all {
                let name = match &ledger {
                    Some(l) => l
                        .get_agent(agent_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|x| x.name)
                        .unwrap_or_else(|| agent_id.clone()),
                    None => agent_id.clone(),
                };
                html.push_str(&format!(
                    "<tr><td>{repo}#{issue}</td>\
                     <td><a href=\"/board/agent/{id}\">{name}</a></td>\
                     <td class=\"mono\">{branch}</td><td>{pr}</td>\
                     <td class=\"dim mono\">{wt}</td></tr>",
                    repo = ciacola_core::render::esc(&a.repo),
                    issue = a.issue,
                    id = ciacola_core::render::esc(agent_id),
                    name = ciacola_core::render::esc(&name),
                    branch = ciacola_core::render::esc(&a.branch),
                    pr = a.pr.map(|n| format!("#{n}")).unwrap_or_else(|| "-".into()),
                    wt = ciacola_core::render::esc(&a.worktree),
                ));
            }
            html.push_str("</table>");
            Some(Section {
                title: "issues in progress".into(),
                html,
            })
        })
    }

    fn health(&self) -> BoxFut<'_, serde_json::Value> {
        Box::pin(async move {
            let all = self.assignments().await;
            json!({
                "assignments": all.len(),
                "with_pr": all.iter().filter(|(_, a)| a.pr.is_some()).count(),
                "worktrees": self.repos.as_ref().map(|r| r.worktrees().len()).unwrap_or(0),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn git(dir: &Path, args: &[&str]) {
        let out = tokio::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .await
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A bare clone made by `git clone --bare` has no
    /// `remote.origin.fetch`, so `git fetch origin` moves FETCH_HEAD and
    /// nothing else. Without an explicit refspec the clone's `main` is
    /// frozen at the moment it was made, and every worktree cut from it
    /// is silently working against stale code.
    ///
    /// Uses a `file://` origin so it needs no network, per CONTRIBUTING.
    #[tokio::test]
    async fn ensure_clone_advances_main_after_the_origin_moves() {
        let tmp = std::env::temp_dir().join(format!("ciacola-fetch-{}", ulid::Ulid::new()));
        let origin = tmp.join("origin");
        std::fs::create_dir_all(&origin).expect("mkdir");

        git(&origin, &["init", "-q", "-b", "main"]).await;
        git(&origin, &["config", "user.email", "t@example.com"]).await;
        git(&origin, &["config", "user.name", "t"]).await;
        std::fs::write(origin.join("a"), "one").expect("write");
        git(&origin, &["add", "."]).await;
        git(&origin, &["commit", "-qm", "one"]).await;

        let repos = Repos {
            root: tmp.join("root"),
            allowed: Arc::new(vec!["local/repo".into()]),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
        };
        std::fs::create_dir_all(&repos.root).expect("mkdir root");
        let bare = repos.bare("local/repo");
        CloneCommand::new(format!("file://{}", origin.display()))
            .bare()
            .directory(&bare)
            .execute()
            .await
            .expect("clone");

        // The origin moves after the clone exists, which is the whole
        // scenario: a long-lived clone and a repository that keeps
        // merging.
        std::fs::write(origin.join("a"), "two").expect("write");
        git(&origin, &["add", "."]).await;
        git(&origin, &["commit", "-qm", "two"]).await;
        let want = String::from_utf8_lossy(
            &tokio::process::Command::new("git")
                .args(["rev-parse", "main"])
                .current_dir(&origin)
                .output()
                .await
                .expect("rev-parse")
                .stdout,
        )
        .trim()
        .to_string();

        repos.ensure_clone("local/repo").await.expect("refresh");

        let got = String::from_utf8_lossy(
            &tokio::process::Command::new("git")
                .args(["rev-parse", "main"])
                .current_dir(&bare)
                .output()
                .await
                .expect("rev-parse")
                .stdout,
        )
        .trim()
        .to_string();

        std::fs::remove_dir_all(&tmp).ok();
        assert_eq!(got, want, "the clone's main did not follow origin's");
    }

    /// The refresh must not delete the branches this plugin creates.
    ///
    /// `agent/*` exists only locally until `open_pr` pushes it, so any
    /// prune against `refs/heads/*` removes it, and git will do that
    /// even while a worktree has it checked out. The worktree's HEAD
    /// then dangles and the agent's next commit is an orphan with no
    /// ancestry, which cannot be merged and does not look wrong until
    /// someone reads the history.
    #[tokio::test]
    async fn refreshing_does_not_delete_an_agent_branch() {
        let tmp = std::env::temp_dir().join(format!("ciacola-prune-{}", ulid::Ulid::new()));
        let origin = tmp.join("origin");
        std::fs::create_dir_all(&origin).expect("mkdir");
        git(&origin, &["init", "-q", "-b", "main"]).await;
        git(&origin, &["config", "user.email", "t@example.com"]).await;
        git(&origin, &["config", "user.name", "t"]).await;
        std::fs::write(origin.join("a"), "one").expect("write");
        git(&origin, &["add", "."]).await;
        git(&origin, &["commit", "-qm", "one"]).await;

        let repos = Repos {
            root: tmp.join("root"),
            allowed: Arc::new(vec!["local/repo".into()]),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
        };
        std::fs::create_dir_all(&repos.root).expect("mkdir root");
        let bare = repos.bare("local/repo");
        CloneCommand::new(format!("file://{}", origin.display()))
            .bare()
            .directory(&bare)
            .execute()
            .await
            .expect("clone");

        // One unit of work in flight, exactly as a batch has.
        let (_wt, branch) = repos
            .add_worktree("local/repo", "local-repo-1", "main")
            .await
            .expect("worktree");

        // A second `start_issue` refreshes the clone while the first is
        // still working. That is where the branch used to disappear.
        repos.ensure_clone("local/repo").await.expect("refresh");

        let refs = tokio::process::Command::new("git")
            .args(["for-each-ref", "--format=%(refname:short)"])
            .current_dir(&bare)
            .output()
            .await
            .expect("for-each-ref");
        let refs = String::from_utf8_lossy(&refs.stdout).to_string();

        std::fs::remove_dir_all(&tmp).ok();
        assert!(
            refs.lines().any(|r| r == branch),
            "refresh deleted the in-flight branch {branch}; refs were:\n{refs}"
        );
    }
}
