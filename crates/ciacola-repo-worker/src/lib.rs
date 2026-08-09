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
//! root = "~/.local/share/ciacola/repos"   # clones and worktrees
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
use tower_mcp::extract::{Context, Json, State};
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

/// Is this a conventional-commit title: `type(scope)!: subject`?
///
/// Enforced mechanically in `open_pr` rather than only asked for in the
/// prompt, because the title is the one piece of an agent's writing
/// that lands on GitHub verbatim, and a guard that is only a request
/// is not a guard. Scope and `!` are optional; the type is the closed
/// set below; the subject must be non-empty.
fn conventional_title(title: &str) -> bool {
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

    #[cfg(test)]
    async fn ensure_clone(&self, repo: &str) -> Result<PathBuf, FlatError> {
        self.ensure_clone_from(repo, &format!("https://github.com/{repo}.git"))
            .await
    }

    /// Clone once into the plugin's own root, then refresh and reuse.
    async fn ensure_clone_from(&self, repo: &str, url: &str) -> Result<PathBuf, FlatError> {
        let bare = self.bare(repo);
        let _guard = self.cloning.lock().await;
        if !bare.exists() {
            std::fs::create_dir_all(&self.root)?;
            eprintln!("[repo-worker] cloning {repo} (once)");
            CloneCommand::new(url)
                .bare()
                .directory(&bare)
                .execute()
                .await
                .map_err(|e| -> FlatError { format!("clone {repo}: {e}").into() })?;
        }

        // The refspec is not optional, even immediately after cloning.
        // `git clone --bare` writes no `remote.origin.fetch` and creates
        // `refs/heads/main`, while add_worktree deliberately starts from
        // `refs/remotes/origin/main`. Returning before this fetch made
        // the first start_issue fail and the identical retry succeed.
        //
        // Mapping to remote-tracking refs also keeps refreshes away from
        // local agent branches and branches checked out by worktrees.
        // `+refs/heads/*:refs/heads/*` would instead prune unpublished
        // agent branches, collide with the local namespace, and refuse
        // to update a branch held by a live worktree.
        let mut fetch = bare_repo(&bare).fetch();
        fetch
            .remote("origin")
            .refspec("+refs/heads/*:refs/remotes/origin/*");
        fetch
            .execute()
            .await
            .map_err(|e| -> FlatError { format!("fetch {repo}: {e}").into() })?;
        Ok(bare)
    }

    /// A directory and a branch for one unit of work.
    async fn add_worktree(
        &self,
        repo: &str,
        slug: &str,
        base: &str,
    ) -> Result<(PathBuf, String), FlatError> {
        self.add_worktree_from(repo, slug, base, &format!("https://github.com/{repo}.git"))
            .await
    }

    async fn add_worktree_from(
        &self,
        repo: &str,
        slug: &str,
        base: &str,
        url: &str,
    ) -> Result<(PathBuf, String), FlatError> {
        let bare = self.ensure_clone_from(repo, url).await?;
        let path = self.root.join(format!("wt-{slug}"));
        let branch = format!("agent/{slug}");
        if path.exists() {
            return Ok((path, branch));
        }
        // `origin/main` rather than `main`: the refresh writes
        // remote-tracking refs, so this is the one that moves. A local
        // `main` in this clone would be a stale copy at best, and
        // nothing here creates one.
        let mut add = WorktreeCommand::add(&path);
        add.new_branch(&branch).commit_ish(format!("origin/{base}"));
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
        // Cleanup is retried after partial failures, so an already
        // absent worktree is success. The branch deletion below is
        // deliberately idempotent as well.
        if path.exists() {
            let mut remove = WorktreeCommand::remove(&path);
            remove.force();
            bare_repo(&bare)
                .worktree(remove)
                .execute()
                .await
                .map_err(|e| -> FlatError { format!("worktree remove: {e}").into() })?;
        }
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
                    .unwrap_or("~/.local/share/ciacola/repos"),
            );
            self.repos = Some(Repos {
                root,
                allowed: Arc::new(config.repos),
                cloning: Arc::new(tokio::sync::Mutex::new(())),
            });
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
                          prepares reviewed work for a human to publish."
                    .into(),
                provider: None,
                model: None,
                effort: Some("high".into()),
                // Not hermetic: it edits this repository while curating the
                // implementer prompt. Publication remains a human operator
                // action rather than ambient provider authority.
                hermetic: Some("none".into()),
                working_dir: Some("{{checkout}}".into()),
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
                    "Bash(git log:*)".into(),
                    "Bash(git show:*)".into(),
                    "Bash(cargo:*)".into(),
                    "Bash(just:*)".into(),
                    // The implementer role below holds this too, for
                    // repositories whose gate is a Makefile rather than a
                    // justfile. A manager that cannot hold what it
                    // dispatches cannot spawn the implementer shipped by
                    // this same plugin: start_issue and spawn_role both
                    // run every requested tool through the same authority
                    // check, so a bundled parent missing one of its own
                    // child's tools is refused before any work happens.
                    "Bash(make:*)".into(),
                    "Bash(gh issue view:*)".into(),
                    // The ordinary ciacola surface is enough to dispatch and
                    // inspect work. open_pr is deliberately absent: only a
                    // human stdio/root-bearer session may publish.
                    "mcp__ciacola".into(),
                ],
                inherit_provider_tools: false,
                sandbox: None,
                max_turns: None,
                rotate_after_turns: None,
                loopback: true,
                surface: None,
                arguments: vec!["checkout".into()],
                system_prompt: "\
You dispatch issues to implementers and you own the prompt they run on.
The second half is the part that is easy to skip, and it is why this
role exists rather than a person just calling start_issue.

Working in {{checkout}}, which is ciacola's own repository.

Dispatching:
- start_issue, then send, then wait. Pass timeout_secs; the default is
  120 and real work runs longer, and a turn cut short loses its session.
- Read the diff yourself. The reply is the worker's account of what it did,
  which is not the same thing. When it is ready, report the exact assignment
  and evidence to the human operator; do not claim to open or merge a pull
  request yourself.
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
                description: "Implements one GitHub issue in its own worktree and prepares the \
                          committed result for human review."
                    .into(),
                provider: None,
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
                inherit_provider_tools: false,
                sandbox: None,
                max_turns: Some(60),
                rotate_after_turns: None,
                loopback: true,
                surface: None,
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
   title on one line, in conventional-commit form like the commit \
   (open_pr refuses any other shape), and a pull request body whose last line is \
   Closes #{{issue}} and nothing else. Those two go to open_pr exactly \
   as given, so write them to be used rather than edited.

You cannot push, open pull requests, or comment. Those are the server's \
to do, on purpose."
                    .into(),
            },
        ]
    }

    fn tools(&self, surface: Surface) -> Vec<Tool> {
        let operator_surface = surface == Surface::Operator;
        let (Some(repos), Some(ctx)) = (self.repos.clone(), self.ctx.clone()) else {
            return Vec::new();
        };
        // Higher-level plugin wiring consumes the same configured catalog as
        // roles, spawn_role, completion, and persistent role agents.
        let roles = ctx.roles.clone();

        let start = {
            let (repos, ctx, roles) = (repos.clone(), ctx.clone(), roles.clone());
            ToolBuilder::new("start_issue")
                .description(
                    "Begin work on a GitHub issue: ensure the system's own \
                     clone, cut a fresh worktree and branch, and spawn an \
                     implementer pointed at it. Returns the agent to send to.",
                )
                .non_destructive()
                .extractor_handler(
                    (repos.clone(), ctx.clone(), roles.clone()),
                    move |State((repos, ctx, roles)): State<(Repos, PluginContext, Roles)>,
                          mcp: Context,
                          Json(args): Json<StartIssueArgs>| async move {
                        // Same policy as spawn: an authenticated caller
                        // is the parent whatever it claims; only the
                        // operator's terminal is taken at its word.
                        let caller = mcp
                            .extension::<ciacola_core::AgentIdentity>()
                            .map(|i| i.0.clone());
                        let spawned_by = match (&caller, operator_surface) {
                            (Some(id), _) => Some(id.clone()),
                            (None, true) => args.spawned_by.clone(),
                            (None, false) => None,
                        };
                        if !repos.allows(&args.repo) {
                            return Ok(CallToolResult::error(format!(
                                "repository '{}' is not in the configured list",
                                args.repo
                            )));
                        }
                        let Some(role) = roles.get(ROLE).cloned() else {
                            return Ok(CallToolResult::error("role missing".to_string()));
                        };
                        if role.surface.as_deref() == Some("operator") {
                            return Ok(CallToolResult::error(format!(
                                "role '{}' carries the operator surface, which provider-backed agents cannot hold; use stdio or authenticated human HTTP",
                                role.name
                            )));
                        }
                        if role.inherit_provider_tools && (caller.is_some() || !operator_surface) {
                            return Ok(CallToolResult::error(format!(
                                "role '{}' inherits its provider's native tool policy, which an agent cannot bound; ask the operator to start the issue",
                                role.name
                            )));
                        }
                        let grant = match ciacola_core::grant_child_tools(
                            &ctx.ledger,
                            caller.as_deref(),
                            role.allowed_tools.clone(),
                        )
                        .await
                        {
                            Ok(grant) => grant,
                            Err(e) => return Ok(CallToolResult::error(e.to_string())),
                        };
                        if !grant.denied.is_empty() {
                            return Ok(CallToolResult::error(format!(
                                "issue-implementer needs tools its parent does not hold: {}",
                                grant.denied.join(", ")
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
                        let args_map = std::collections::HashMap::from([
                            ("repo".to_string(), args.repo.clone()),
                            ("issue".to_string(), args.issue.to_string()),
                            ("worktree".to_string(), worktree.display().to_string()),
                        ]);
                        let mut def = roles.to_def(&role, &args_map);
                        def.name = format!("impl-{slug}");

                        match ctx.ledger.create_agent(&def, spawned_by.as_deref()).await {
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
                    },
                )
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
                            // The title gate is the preflight: keep it
                            // ahead of assignment lookup, gh, git, and
                            // especially push so every rejection is
                            // side-effect free.
                            if !conventional_title(&args.title) {
                                return Ok(CallToolResult::error(format!(
                                    "title '{}' is not conventional-commit form. \
                                     Use type(scope): subject, e.g. 'fix: ...' or \
                                     'feat(board): ...'; types are build, chore, ci, \
                                     docs, feat, fix, perf, refactor, revert, style, test.",
                                    args.title
                                )));
                            }
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
                            let retired = match ctx.ledger.get_agent(&args.agent_id).await {
                                Ok(Some(agent)) if agent.retired => true,
                                Ok(Some(_)) => ctx
                                    .ledger
                                    .retire_agent(&args.agent_id)
                                    .await
                                    .unwrap_or(false),
                                _ => false,
                            };
                            if !retired {
                                return Ok(CallToolResult::error(
                                    "agent still has a queued or running turn; \
                                     finish once it goes idle"
                                        .to_string(),
                                ));
                            }
                            let keep = args.keep.unwrap_or(false);
                            let removed = if keep {
                                false
                            } else {
                                if let Err(e) = repos.remove_worktree(&a.repo, &a.slug).await {
                                    return Ok(CallToolResult::error(format!(
                                        "agent retired, but cleanup failed; assignment kept for retry: {e}"
                                    )));
                                }
                                true
                            };
                            // A retired agent is done, not "in progress":
                            // drop the assignment so it stops appearing
                            // in the board's "issues in progress" section
                            // and in `health`'s count. Nothing else in
                            // this plugin reads a finished assignment, so
                            // there is no history to preserve by keeping
                            // it.
                            if let Err(e) = store.delete(&key).await {
                                return Ok(CallToolResult::error(format!(
                                    "agent retired and worktree cleanup completed, but assignment cleanup failed: {e}"
                                )));
                            }
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
    use tower_mcp::context::{Extensions, RequestContext};
    use tower_mcp::protocol::RequestId;

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

    fn bundled_roles() -> Roles {
        Roles::new(RepoWorkerPlugin::default().roles(), "agent.json")
            .with_operator_mcp_config("operator.json")
    }

    fn native_implementer_role() -> Role {
        Role {
            name: ROLE.into(),
            description: "Codex implementation role".into(),
            provider: Some("codex".into()),
            model: None,
            effort: Some("high".into()),
            hermetic: Some("none".into()),
            working_dir: Some("{{worktree}}".into()),
            allowed_tools: Vec::new(),
            inherit_provider_tools: true,
            sandbox: Some("workspace-write".into()),
            max_turns: Some(60),
            rotate_after_turns: None,
            loopback: false,
            surface: None,
            arguments: vec!["repo".into(), "issue".into(), "worktree".into()],
            system_prompt: "Implement {{repo}}#{{issue}} in {{worktree}}".into(),
        }
    }

    fn roles_with_implementer(role: Role) -> Roles {
        Roles::new(vec![role], "agent.json").with_operator_mcp_config("operator.json")
    }

    fn native_implementer_roles() -> Roles {
        roles_with_implementer(native_implementer_role())
    }

    fn context(pool: sqlx::SqlitePool, ledger: Ledger) -> PluginContext {
        let (tx, _rx) = tower_mcp::context::notification_channel(8);
        let notify = ciacola_core::Notifier(tx);
        let exec = ciacola_core::HandExecutor::start(ledger.clone(), notify.clone(), 1);
        PluginContext {
            pool,
            ledger,
            exec,
            notify,
            db_path: String::new(),
            loopback_mcp_config: "agent.json".into(),
            operator_mcp_config: "operator.json".into(),
            plugin_config: toml::Value::Table(Default::default()),
            limits: Default::default(),
            runtime: Default::default(),
            roles: bundled_roles(),
        }
    }

    fn plugin(ctx: PluginContext, repos: Repos) -> RepoWorkerPlugin {
        RepoWorkerPlugin {
            repos: Some(repos),
            ctx: Some(ctx),
        }
    }

    fn operator_tool(plugin: &RepoWorkerPlugin, name: &str) -> Tool {
        plugin
            .tools(Surface::Operator)
            .into_iter()
            .find(|tool| tool.definition().name == name)
            .unwrap_or_else(|| panic!("operator tool {name}"))
    }

    async fn local_repos(label: &str) -> (PathBuf, Repos) {
        let tmp = std::env::temp_dir().join(format!("ciacola-{label}-{}", ulid::Ulid::new()));
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
        CloneCommand::new(format!("file://{}", origin.display()))
            .bare()
            .directory(repos.bare("local/repo"))
            .execute()
            .await
            .expect("clone");
        (tmp, repos)
    }

    async fn memory_plugin(repos: Repos) -> (RepoWorkerPlugin, Ledger) {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool.clone()).await.expect("ledger");
        // The real plugin host applies core's shared Store migration.
        // These focused plugin tests build the context directly, so the
        // fixture must supply the same table explicitly.
        sqlx::query(
            "CREATE TABLE plugin_kv (
                 plugin TEXT NOT NULL,
                 key TEXT NOT NULL,
                 value TEXT NOT NULL,
                 updated_unix INTEGER NOT NULL,
                 PRIMARY KEY (plugin, key))",
        )
        .execute(&pool)
        .await
        .expect("plugin store");
        (plugin(context(pool, ledger.clone()), repos), ledger)
    }

    #[tokio::test]
    async fn invalid_pr_title_is_refused_before_assignment_or_git_work() {
        let root = std::env::temp_dir().join(format!("ciacola-title-{}", ulid::Ulid::new()));
        let repos = Repos {
            root: root.clone(),
            allowed: Arc::new(Vec::new()),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
        };
        let (plugin, _ledger) = memory_plugin(repos).await;

        let out = operator_tool(&plugin, "open_pr")
            .call(json!({
                "agent_id": "missing",
                "title": "not conventional",
                "body": "unused"
            }))
            .await;
        let rendered = serde_json::to_string(&out).expect("render");

        assert!(rendered.contains("not conventional-commit form"));
        assert!(!rendered.contains("no assignment"));
        assert!(!root.exists(), "preflight must not create repository state");
    }

    #[tokio::test]
    async fn finish_refuses_a_running_agent_before_removing_its_worktree() {
        let (tmp, repos) = local_repos("finish-running").await;
        let slug = "local-repo-42";
        let (worktree, branch) = repos
            .add_worktree("local/repo", slug, "main")
            .await
            .expect("worktree");
        let (plugin, ledger) = memory_plugin(repos).await;
        let agent_id = ledger
            .create_agent(&ciacola_core::AgentDef::new("worker", "s"), None)
            .await
            .expect("agent");
        let seq = ledger.enqueue_turn(&agent_id, "work").await.expect("turn");
        assert!(ledger.claim_turn(&agent_id, seq).await.expect("claim"));
        let store = plugin.store().expect("store");
        store
            .put(
                &format!("agent/{agent_id}"),
                &Assignment {
                    repo: "local/repo".into(),
                    issue: 42,
                    slug: slug.into(),
                    branch,
                    worktree: worktree.display().to_string(),
                    pr: None,
                },
            )
            .await
            .expect("assignment");

        let out = operator_tool(&plugin, "finish_issue")
            .call(json!({"agent_id": agent_id}))
            .await;
        let rendered = serde_json::to_string(&out).expect("render");

        assert!(rendered.contains("queued or running"), "got: {rendered}");
        assert!(worktree.exists(), "finish removed a live worktree");
        assert!(
            store
                .get::<Assignment>(&format!("agent/{agent_id}"))
                .await
                .expect("read assignment")
                .is_some(),
            "finish lost the live assignment"
        );
        assert!(!ledger.get_agent(&agent_id).await.unwrap().unwrap().retired);
        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test]
    async fn finished_assignment_leaves_the_in_progress_view() {
        let root = std::env::temp_dir().join(format!("ciacola-finish-keep-{}", ulid::Ulid::new()));
        let worktree = root.join("wt-local-repo-42");
        std::fs::create_dir_all(&worktree).expect("worktree");
        let repos = Repos {
            root: root.clone(),
            allowed: Arc::new(Vec::new()),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
        };
        let (plugin, ledger) = memory_plugin(repos).await;
        let agent_id = ledger
            .create_agent(&ciacola_core::AgentDef::new("worker", "s"), None)
            .await
            .expect("agent");
        plugin
            .store()
            .unwrap()
            .put(
                &format!("agent/{agent_id}"),
                &Assignment {
                    repo: "local/repo".into(),
                    issue: 42,
                    slug: "local-repo-42".into(),
                    branch: "agent/local-repo-42".into(),
                    worktree: worktree.display().to_string(),
                    pr: Some(44),
                },
            )
            .await
            .expect("assignment");

        let out = operator_tool(&plugin, "finish_issue")
            .call(json!({"agent_id": agent_id, "keep": true}))
            .await;
        let rendered = serde_json::to_string(&out).expect("render");

        assert!(rendered.contains("agent_retired"), "got: {rendered}");
        assert!(ledger.get_agent(&agent_id).await.unwrap().unwrap().retired);
        assert!(plugin.assignments().await.is_empty());
        assert!(worktree.exists(), "keep=true must retain the worktree");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn cleanup_failure_keeps_a_retryable_assignment() {
        let root = std::env::temp_dir().join(format!("ciacola-finish-retry-{}", ulid::Ulid::new()));
        let worktree = root.join("wt-local-repo-42");
        std::fs::create_dir_all(&worktree).expect("worktree");
        let repos = Repos {
            root: root.clone(),
            allowed: Arc::new(Vec::new()),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
        };
        let (plugin, ledger) = memory_plugin(repos).await;
        let agent_id = ledger
            .create_agent(&ciacola_core::AgentDef::new("worker", "s"), None)
            .await
            .expect("agent");
        let store = plugin.store().unwrap();
        store
            .put(
                &format!("agent/{agent_id}"),
                &Assignment {
                    repo: "local/repo".into(),
                    issue: 42,
                    slug: "local-repo-42".into(),
                    branch: "agent/local-repo-42".into(),
                    worktree: worktree.display().to_string(),
                    pr: None,
                },
            )
            .await
            .expect("assignment");

        let finish = operator_tool(&plugin, "finish_issue");
        let failed = finish.call(json!({"agent_id": agent_id})).await;
        let failed = serde_json::to_string(&failed).expect("render");
        assert!(
            failed.contains("assignment kept for retry"),
            "got: {failed}"
        );
        assert!(ledger.get_agent(&agent_id).await.unwrap().unwrap().retired);
        assert!(
            store
                .get::<Assignment>(&format!("agent/{agent_id}"))
                .await
                .unwrap()
                .is_some()
        );

        // A later cleanup recognizes the already-retired agent instead
        // of getting stuck forever at the retirement gate.
        let retried = finish
            .call(json!({"agent_id": agent_id, "keep": true}))
            .await;
        let retried = serde_json::to_string(&retried).expect("render");
        assert!(retried.contains("agent_retired"), "got: {retried}");
        assert!(plugin.assignments().await.is_empty());
        std::fs::remove_dir_all(root).ok();
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
                .args(["rev-parse", "origin/main"])
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

    /// The initial clone and worktree add are one start_issue call. A
    /// test that primes the bare clone first misses the exact boundary
    /// where a bare clone has `main` but no `origin/main` yet.
    #[tokio::test]
    async fn first_add_worktree_populates_its_remote_tracking_base() {
        let tmp = std::env::temp_dir().join(format!("ciacola-first-clone-{}", ulid::Ulid::new()));
        let origin = tmp.join("origin");
        std::fs::create_dir_all(&origin).expect("mkdir");
        git(&origin, &["init", "-q", "-b", "main"]).await;
        git(&origin, &["config", "user.email", "t@example.com"]).await;
        git(&origin, &["config", "user.name", "t"]).await;
        std::fs::write(origin.join("a"), "first clone").expect("write");
        git(&origin, &["add", "."]).await;
        git(&origin, &["commit", "-qm", "initial"]).await;
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
        let repos = Repos {
            root: tmp.join("root"),
            allowed: Arc::new(vec!["local/repo".into()]),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
        };
        let url = format!("file://{}", origin.display());

        let (worktree, _) = repos
            .add_worktree_from("local/repo", "local-repo-1", "main", &url)
            .await
            .expect("the first add must not require a retry");
        let got = String::from_utf8_lossy(
            &tokio::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&worktree)
                .output()
                .await
                .expect("rev-parse")
                .stdout,
        )
        .trim()
        .to_string();

        std::fs::remove_dir_all(&tmp).ok();
        assert_eq!(got, want, "the first worktree did not start at origin/main");
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

    /// The refresh must work while a worktree is checked out.
    ///
    /// `+refs/heads/*:refs/heads/*` does not: git refuses to fetch into
    /// a branch some worktree has checked out, so the whole refresh
    /// aborts and every later start_issue fails. Mapping to
    /// remote-tracking refs never writes a local branch, so there is
    /// nothing to refuse.
    #[tokio::test]
    async fn refreshing_works_while_a_worktree_is_checked_out() {
        let tmp = std::env::temp_dir().join(format!("ciacola-live-{}", ulid::Ulid::new()));
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
        repos
            .add_worktree("local/repo", "local-repo-1", "main")
            .await
            .expect("worktree");

        // The branch that worktree holds is now pushed, so a refspec
        // writing local heads would try to update it and be refused.
        git(&origin, &["config", "receive.denyCurrentBranch", "ignore"]).await;

        let refreshed = repos.ensure_clone("local/repo").await;
        std::fs::remove_dir_all(&tmp).ok();
        refreshed.expect("refresh must not be blocked by a live worktree");
    }

    /// The title gate is a pure function; the closed set and both
    /// optional parts are worth pinning.
    #[test]
    fn conventional_titles_pass_and_the_rest_do_not() {
        for good in [
            "fix: accept bare repositories",
            "feat(board): live updates",
            "feat!: drop the queue",
            "refactor(core)!: rename the ledger",
            "test: cover the skip path",
        ] {
            assert!(conventional_title(good), "should pass: {good}");
        }
        for bad in [
            "Fix: capitalized type",
            "fixed the bug",
            "fix:",
            "fix: ",
            "wip: not a type",
            "feat(: broken scope",
            "feat(scope: unclosed",
            ": no type at all",
        ] {
            assert!(!conventional_title(bad), "should fail: {bad}");
        }
    }

    /// `start_issue` creates an agent through plugin wiring, so it must
    /// meet the same ceiling as raw spawn and spawn_role. The refusal is
    /// deliberately checked before gh or git runs.
    #[tokio::test]
    async fn start_issue_cannot_out_reach_its_authenticated_parent() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool.clone()).await.expect("ledger");
        let parent = ledger
            .create_agent(
                &ciacola_core::AgentDef::new("parent", "s").allowed_tools(["Read"]),
                None,
            )
            .await
            .expect("parent");
        let (tx, _rx) = tower_mcp::context::notification_channel(8);
        let notify = ciacola_core::Notifier(tx);
        let exec = ciacola_core::HandExecutor::start(ledger.clone(), notify.clone(), 1);
        let root = std::env::temp_dir().join(format!("ciacola-authority-{}", ulid::Ulid::new()));
        let plugin_config: toml::Value = toml::from_str(&format!(
            "[repo-worker]\nroot = {:?}\nrepos = [\"local/repo\"]",
            root.display().to_string()
        ))
        .expect("config");
        let ctx = PluginContext {
            pool,
            ledger: ledger.clone(),
            exec,
            notify,
            db_path: String::new(),
            loopback_mcp_config: "agent.json".into(),
            operator_mcp_config: "operator.json".into(),
            plugin_config,
            limits: Default::default(),
            runtime: Default::default(),
            roles: bundled_roles(),
        };
        let mut plugin = RepoWorkerPlugin::default();
        plugin.setup(&ctx).await.expect("setup");
        let start = plugin
            .tools(Surface::Agent)
            .into_iter()
            .find(|tool| tool.definition().name == "start_issue")
            .expect("start_issue");
        let mut extensions = Extensions::new();
        extensions.insert(ciacola_core::AgentIdentity(parent));
        let request =
            RequestContext::new(RequestId::Number(1)).with_extensions(Arc::new(extensions));

        let out = start
            .call_with_context(
                request,
                serde_json::json!({"repo": "local/repo", "issue": 1}),
            )
            .await;
        let rendered = serde_json::to_string(&out).expect("render");
        assert!(
            rendered.contains("needs tools its parent does not hold"),
            "must refuse: {rendered}"
        );
        assert!(!root.exists(), "authority refusal must happen before clone");
        assert_eq!(ledger.list_agents().await.expect("list").len(), 1);
    }

    /// Issue #70: repo-worker used to rebuild its own shipped role catalog in
    /// setup, so the public roles tool showed an operator's configured Codex
    /// override while start_issue silently created the Claude-shaped role.
    #[tokio::test]
    async fn start_issue_uses_the_server_merged_role_catalog() {
        let (tmp, repos) = local_repos("merged-role").await;
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool.clone()).await.expect("ledger");
        let mut ctx = context(pool, ledger.clone());
        ctx.roles = native_implementer_roles();
        ctx.plugin_config = toml::from_str(&format!(
            "[repo-worker]\nroot = {:?}\nrepos = [\"local/repo\"]",
            repos.root.display().to_string()
        ))
        .expect("config");

        let mut plugin = RepoWorkerPlugin::default();
        plugin.setup(&ctx).await.expect("setup");
        let start = operator_tool(&plugin, "start_issue");
        let out = start
            .call(json!({"repo": "local/repo", "issue": 70, "base": "main"}))
            .await;
        let rendered = serde_json::to_string(&out).expect("render");
        assert!(rendered.contains("\"agent_id\""), "got: {rendered}");

        let agents = ledger.list_agents().await.expect("agents");
        assert_eq!(agents.len(), 1);
        let def = &agents[0].def;
        assert_eq!(def.provider.as_str(), "codex");
        assert_eq!(def.model, None, "the shipped Claude model must not leak");
        assert!(def.allowed_tools.is_empty());
        assert!(def.inherit_provider_tools);
        assert_eq!(def.sandbox.as_deref(), Some("workspace-write"));

        std::fs::remove_dir_all(&tmp).ok();
    }

    /// A configured native-tool role is an operator-only provisioning
    /// decision. An authenticated agent cannot use start_issue to mint a
    /// child whose provider-native authority has no named ceiling.
    #[tokio::test]
    async fn start_issue_refuses_native_tool_inheritance_from_an_agent() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool.clone()).await.expect("ledger");
        let parent = ledger
            .create_agent(&ciacola_core::AgentDef::new("parent", "s"), None)
            .await
            .expect("parent");
        let root =
            std::env::temp_dir().join(format!("ciacola-native-authority-{}", ulid::Ulid::new()));
        let mut ctx = context(pool, ledger.clone());
        ctx.roles = native_implementer_roles();
        ctx.plugin_config = toml::from_str(&format!(
            "[repo-worker]\nroot = {:?}\nrepos = [\"local/repo\"]",
            root.display().to_string()
        ))
        .expect("config");

        let mut plugin = RepoWorkerPlugin::default();
        plugin.setup(&ctx).await.expect("setup");
        let start = plugin
            .tools(Surface::Agent)
            .into_iter()
            .find(|tool| tool.definition().name == "start_issue")
            .expect("start_issue");
        let mut extensions = Extensions::new();
        extensions.insert(ciacola_core::AgentIdentity(parent));
        let request =
            RequestContext::new(RequestId::Number(70)).with_extensions(Arc::new(extensions));
        let out = start
            .call_with_context(
                request,
                json!({"repo": "local/repo", "issue": 70, "base": "main"}),
            )
            .await;
        let rendered = serde_json::to_string(&out).expect("render");
        assert!(
            rendered.contains("inherits its provider's native tool policy"),
            "must refuse: {rendered}"
        );
        assert!(
            !root.exists(),
            "refusal must happen before clone or worktree"
        );
        assert_eq!(ledger.list_agents().await.expect("list").len(), 1);
    }

    /// A configured implementer cannot receive the human-only HTTP operator
    /// surface, even when a human calls start_issue.
    #[tokio::test]
    async fn start_issue_refuses_an_operator_role_before_side_effects() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool.clone()).await.expect("ledger");
        let root =
            std::env::temp_dir().join(format!("ciacola-operator-role-{}", ulid::Ulid::new()));
        let mut operator_role = native_implementer_role();
        operator_role.inherit_provider_tools = false;
        operator_role.surface = Some("operator".into());
        operator_role.loopback = true;
        let mut ctx = context(pool, ledger.clone());
        ctx.roles = roles_with_implementer(operator_role);
        ctx.plugin_config = toml::from_str(&format!(
            "[repo-worker]\nroot = {:?}\nrepos = [\"local/repo\"]",
            root.display().to_string()
        ))
        .expect("config");

        let mut plugin = RepoWorkerPlugin::default();
        plugin.setup(&ctx).await.expect("setup");
        let start = plugin
            .tools(Surface::Operator)
            .into_iter()
            .find(|tool| tool.definition().name == "start_issue")
            .expect("start_issue");
        let request = RequestContext::new(RequestId::Number(71));
        let out = start
            .call_with_context(
                request,
                json!({"repo": "local/repo", "issue": 70, "base": "main"}),
            )
            .await;
        let rendered = serde_json::to_string(&out).expect("render");
        assert!(
            rendered.contains("provider-backed agents cannot hold"),
            "must refuse: {rendered}"
        );
        assert!(
            !root.exists(),
            "refusal must happen before clone or worktree"
        );
        assert!(ledger.list_agents().await.expect("list").is_empty());
    }

    /// Issue #60: the first live manager-driven run could not use
    /// `start_issue` or `spawn_role`, because the bundled `repo-manager`
    /// role did not hold `Bash(make:*)`, which the bundled
    /// `issue-implementer` role requests. The capability ceiling worked
    /// as designed and refused the manager anyway.
    ///
    /// This builds the manager's authenticated agent from the *shipped*
    /// `Role` (via `setup` and `to_def`, the same path a real boot
    /// takes), not a hand-typed tool list, so a future edit that lets
    /// the two roles drift apart again fails here rather than only in
    /// prompt-derived documentation. An intentionally underprivileged
    /// authenticated parent is still refused by
    /// `start_issue_cannot_out_reach_its_authenticated_parent` above;
    /// this test is its positive counterpart, and covers both creation
    /// paths that share `grant_child_tools`.
    #[tokio::test]
    async fn bundled_manager_can_delegate_to_bundled_implementer() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool.clone()).await.expect("ledger");
        let (tx, _rx) = tower_mcp::context::notification_channel(8);
        let notify = ciacola_core::Notifier(tx);
        let exec = ciacola_core::HandExecutor::start(ledger.clone(), notify.clone(), 1);

        let root =
            std::env::temp_dir().join(format!("ciacola-manager-delegation-{}", ulid::Ulid::new()));
        let origin = root.join("origin");
        std::fs::create_dir_all(&origin).expect("mkdir");
        git(&origin, &["init", "-q", "-b", "main"]).await;
        git(&origin, &["config", "user.email", "t@example.com"]).await;
        git(&origin, &["config", "user.name", "t"]).await;
        std::fs::write(origin.join("a"), "one").expect("write");
        git(&origin, &["add", "."]).await;
        git(&origin, &["commit", "-qm", "one"]).await;

        let repos_root = root.join("repos");
        std::fs::create_dir_all(&repos_root).expect("mkdir");
        CloneCommand::new(format!("file://{}", origin.display()))
            .bare()
            .directory(repos_root.join("local__repo.git"))
            .execute()
            .await
            .expect("bare clone");

        let plugin_config: toml::Value = toml::from_str(&format!(
            "[repo-worker]\nroot = {:?}\nrepos = [\"local/repo\"]",
            repos_root.display().to_string()
        ))
        .expect("config");
        let ctx = PluginContext {
            pool,
            ledger: ledger.clone(),
            exec,
            notify,
            db_path: String::new(),
            loopback_mcp_config: "agent.json".into(),
            operator_mcp_config: "operator.json".into(),
            plugin_config,
            limits: Default::default(),
            runtime: Default::default(),
            roles: bundled_roles(),
        };
        let mut plugin = RepoWorkerPlugin::default();
        plugin.setup(&ctx).await.expect("setup");

        // The manager's authority comes from the shipped role, not a
        // stand-in typed into this test.
        let roles = ctx.roles.clone();
        let manager_role = roles.get(MANAGER).cloned().expect("manager role bundled");
        assert_eq!(manager_role.surface, None, "manager uses the agent mount");
        assert!(
            !manager_role
                .allowed_tools
                .iter()
                .any(|tool| matches!(tool.as_str(), "Bash(gh:*)" | "Bash(git:*)")),
            "provider manager must not inherit broad publication commands"
        );
        let manager_args =
            std::collections::HashMap::from([("checkout".to_string(), root.display().to_string())]);
        let manager_def = roles.to_def(&manager_role, &manager_args);
        let manager_id = ledger
            .create_agent(&manager_def, None)
            .await
            .expect("manager agent");

        let start = plugin
            .tools(Surface::Agent)
            .into_iter()
            .find(|tool| tool.definition().name == "start_issue")
            .expect("start_issue");
        let mut extensions = Extensions::new();
        extensions.insert(ciacola_core::AgentIdentity(manager_id.clone()));
        let request =
            RequestContext::new(RequestId::Number(1)).with_extensions(Arc::new(extensions));
        let out = start
            .call_with_context(
                request,
                serde_json::json!({"repo": "local/repo", "issue": 60, "base": "main"}),
            )
            .await;
        let rendered = serde_json::to_string(&out).expect("render");
        assert!(
            !rendered.contains("needs tools its parent does not hold"),
            "the shipped manager must be able to start_issue the shipped implementer: {rendered}"
        );
        assert!(rendered.contains("\"agent_id\""), "got: {rendered}");

        // spawn_role is the other creation path sharing grant_child_tools;
        // the manager must be able to reach the implementer through it
        // too, e.g. when start_issue's wiring is not what is wanted.
        let spawn_roles = Roles::with_runtime(
            roles.all().to_vec(),
            ctx.loopback_mcp_config.clone(),
            ctx.runtime.clone(),
        );
        let spawn_role_tool =
            ciacola_core::roles::tools_with_depth(spawn_roles, ledger.clone(), 3, false)
                .into_iter()
                .find(|t| t.definition().name == "spawn_role")
                .expect("spawn_role");
        let mut extensions = Extensions::new();
        extensions.insert(ciacola_core::AgentIdentity(manager_id.clone()));
        let request =
            RequestContext::new(RequestId::Number(2)).with_extensions(Arc::new(extensions));
        let out = spawn_role_tool
            .call_with_context(
                request,
                serde_json::json!({
                    "role": ROLE,
                    "arguments": {
                        "repo": "local/repo",
                        "issue": "60",
                        "worktree": root.join("wt-manual").display().to_string(),
                    }
                }),
            )
            .await;
        let rendered = serde_json::to_string(&out).expect("render");
        assert!(
            !rendered.contains("needs tools its parent does not hold"),
            "the shipped manager must be able to spawn_role the shipped implementer: {rendered}"
        );

        // The ceiling still refuses a genuinely underprivileged parent
        // through this second path, matching start_issue's refusal.
        let underprivileged = ledger
            .create_agent(
                &ciacola_core::AgentDef::new("under", "s").allowed_tools(["Read"]),
                None,
            )
            .await
            .expect("underprivileged agent");
        let mut extensions = Extensions::new();
        extensions.insert(ciacola_core::AgentIdentity(underprivileged));
        let request =
            RequestContext::new(RequestId::Number(3)).with_extensions(Arc::new(extensions));
        let out = spawn_role_tool
            .call_with_context(
                request,
                serde_json::json!({
                    "role": ROLE,
                    "arguments": {
                        "repo": "local/repo",
                        "issue": "60",
                        "worktree": root.join("wt-manual-2").display().to_string(),
                    }
                }),
            )
            .await;
        let rendered = serde_json::to_string(&out).expect("render");
        assert!(
            rendered.contains("needs tools its parent does not hold"),
            "an underprivileged authenticated parent must still be refused: {rendered}"
        );

        std::fs::remove_dir_all(&root).ok();
    }
}
