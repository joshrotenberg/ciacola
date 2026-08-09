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
//! branch_templates = { "joshrotenberg/tower-mcp" = "fix/{slug}" }
//! ```
//!
//! External writes are gated: `open_pr` is the only tool that can
//! affect the outside world. It durably pins an exact commit, publishes
//! that object rather than a mutable branch tip, and reconciles existing
//! pull-request state before creating anything. A resent or redelivered
//! turn must neither publish a different commit nor open a second PR.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{
    collections::{BTreeMap, HashSet},
    fmt,
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Row, SqlitePool};
use tower_mcp::extract::{Context, Json, State};
use tower_mcp::{CallToolResult, Tool, ToolBuilder};

use git_spawn::{CloneCommand, GitCommand, Repository, WorktreeCommand};

use ciacola_core::agent::FlatError;
use ciacola_core::delegation::{DelegatableAction, DelegationPolicy};
use ciacola_core::ledger::Ledger;
use ciacola_core::plugin::{BoxFut, Migration, Plugin, PluginContext, Section, Surface};
use ciacola_core::roles::{Role, Roles};

const ROLE: &str = "issue-implementer";
const START_ISSUE_ROLE_ARGUMENTS: [&str; 3] = ["repo", "issue", "worktree"];
/// The other half of the loop: whoever dispatches work is also who
/// notices what the implementer prompt got wrong, and that only turns
/// into a better prompt if it is somebody's stated job.
const MANAGER: &str = "repo-manager";
const DEFAULT_BRANCH_TEMPLATE: &str = "agent/{slug}";

#[derive(Clone, PartialEq, Eq)]
struct BranchTemplate(String);

impl fmt::Debug for BranchTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BranchTemplate").field(&self.0).finish()
    }
}

impl BranchTemplate {
    fn parse(value: String) -> Result<Self, String> {
        if value.matches("{slug}").count() != 1 {
            return Err(format!(
                "branch template '{value}' must contain exactly one '{{slug}}' placeholder"
            ));
        }
        let remainder = value.replace("{slug}", "");
        if remainder.contains(['{', '}']) {
            return Err(format!(
                "branch template '{value}' contains an unsupported placeholder; only '{{slug}}' is allowed"
            ));
        }
        Ok(Self(value))
    }

    fn render(&self, slug: &str) -> String {
        self.0.replace("{slug}", slug)
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
struct BranchPolicies {
    default: BranchTemplate,
    configured: BTreeMap<String, BranchTemplate>,
}

impl Default for BranchPolicies {
    fn default() -> Self {
        Self {
            default: BranchTemplate(DEFAULT_BRANCH_TEMPLATE.to_string()),
            configured: BTreeMap::new(),
        }
    }
}

impl BranchPolicies {
    fn new(allowed: &[String], configured: BTreeMap<String, String>) -> Result<Self, String> {
        let mut parsed = BTreeMap::new();
        for (repo, template) in configured {
            if !allowed.iter().any(|allowed| allowed == &repo) {
                return Err(format!(
                    "branch template repository '{repo}' is not present in plugins.repo-worker.repos"
                ));
            }
            parsed.insert(repo, BranchTemplate::parse(template)?);
        }
        Ok(Self {
            default: BranchTemplate(DEFAULT_BRANCH_TEMPLATE.to_string()),
            configured: parsed,
        })
    }

    fn for_repo(&self, repo: &str) -> &BranchTemplate {
        self.configured.get(repo).unwrap_or(&self.default)
    }

    fn configured_state(&self) -> BTreeMap<&str, &str> {
        self.configured
            .iter()
            .map(|(repo, template)| (repo.as_str(), template.as_str()))
            .collect()
    }
}

/// `start_issue` owns this role's argument map. Configured overrides may
/// change the prompt and provisioning, but cannot require values the tool has
/// no typed way to supply or drop the values that define its contract.
fn validate_start_issue_role_arguments(role: &Role) -> Result<(), String> {
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for argument in &role.arguments {
        *counts.entry(argument.as_str()).or_default() += 1;
    }

    let missing: Vec<&str> = START_ISSUE_ROLE_ARGUMENTS
        .iter()
        .copied()
        .filter(|argument| !counts.contains_key(argument))
        .collect();
    let unsupported: Vec<&str> = counts
        .keys()
        .copied()
        .filter(|argument| !START_ISSUE_ROLE_ARGUMENTS.contains(argument))
        .collect();
    let duplicate: Vec<&str> = counts
        .iter()
        .filter_map(|(argument, count)| (*count > 1).then_some(*argument))
        .collect();

    if missing.is_empty() && unsupported.is_empty() && duplicate.is_empty() {
        return Ok(());
    }

    let mut differences = Vec::new();
    if !missing.is_empty() {
        differences.push(format!("missing {missing:?}"));
    }
    if !unsupported.is_empty() {
        differences.push(format!("unsupported {unsupported:?}"));
    }
    if !duplicate.is_empty() {
        differences.push(format!("duplicate {duplicate:?}"));
    }
    Err(format!(
        "role '{}' is incompatible with start_issue: expected exactly arguments {:?}; {}. \
         start_issue supplies only repo, issue, and worktree",
        role.name,
        START_ISSUE_ROLE_ARGUMENTS,
        differences.join("; ")
    ))
}

const ASSIGNMENTS_TABLE: &str = "repo_worker_assignments";
const MIGRATIONS: &[Migration] = &[
    Migration::new(
        "0001_assignments",
        "CREATE TABLE IF NOT EXISTS repo_worker_assignments (
         assignment_id TEXT PRIMARY KEY,
         repo TEXT NOT NULL COLLATE NOCASE,
         issue_number INTEGER NOT NULL,
         state TEXT NOT NULL CHECK (
             state IN ('preparing', 'active', 'finishing', 'retained', 'completed', 'stale')),
         phase TEXT NOT NULL,
         base TEXT,
         slug TEXT NOT NULL,
         branch TEXT NOT NULL,
         worktree TEXT NOT NULL,
         bare_path TEXT NOT NULL,
         agent_id TEXT UNIQUE,
         related_agent_ids TEXT NOT NULL DEFAULT '[]',
         spawned_by TEXT,
         pr INTEGER,
         last_error TEXT,
         created_unix INTEGER NOT NULL,
         updated_unix INTEGER NOT NULL,
         terminal_unix INTEGER,
         UNIQUE(repo, issue_number));
     CREATE UNIQUE INDEX IF NOT EXISTS repo_worker_owned_worktree
         ON repo_worker_assignments(worktree)
         WHERE state IN ('preparing', 'active', 'finishing', 'retained');
     CREATE UNIQUE INDEX IF NOT EXISTS repo_worker_owned_branch
         ON repo_worker_assignments(repo, branch)
         WHERE state IN ('preparing', 'active', 'finishing', 'retained');",
    ),
    Migration::add_column(
        "0002_base_head",
        "ALTER TABLE repo_worker_assignments ADD COLUMN base_head TEXT",
    ),
    Migration::add_column(
        "0003_expected_head",
        "ALTER TABLE repo_worker_assignments ADD COLUMN expected_head TEXT",
    ),
    Migration::add_column(
        "0004_publication_state",
        "ALTER TABLE repo_worker_assignments ADD COLUMN publication_state TEXT NOT NULL
             DEFAULT 'unpublished' CHECK (
                 publication_state IN ('unpublished', 'publishing', 'published', 'failed'))",
    ),
    Migration::add_column(
        "0005_pr_url",
        "ALTER TABLE repo_worker_assignments ADD COLUMN pr_url TEXT",
    ),
    Migration::add_column(
        "0006_pr_state",
        "ALTER TABLE repo_worker_assignments ADD COLUMN pr_state TEXT CHECK (
             pr_state IS NULL OR pr_state IN ('open', 'closed', 'merged'))",
    ),
    Migration::add_column(
        "0007_pr_draft",
        "ALTER TABLE repo_worker_assignments ADD COLUMN pr_draft INTEGER CHECK (
             pr_draft IS NULL OR pr_draft IN (0, 1))",
    ),
    Migration::add_column(
        "0008_pr_head",
        "ALTER TABLE repo_worker_assignments ADD COLUMN pr_head TEXT",
    ),
    Migration::add_column(
        "0009_pr_base",
        "ALTER TABLE repo_worker_assignments ADD COLUMN pr_base TEXT",
    ),
    Migration::add_column(
        "0010_pr_checked_unix",
        "ALTER TABLE repo_worker_assignments ADD COLUMN pr_checked_unix INTEGER",
    ),
    Migration::add_column(
        "0011_cleanup_state",
        "ALTER TABLE repo_worker_assignments ADD COLUMN cleanup_state TEXT NOT NULL
             DEFAULT 'none' CHECK (
                 cleanup_state IN ('none', 'retaining', 'retained', 'removing', 'completed', 'failed'))",
    ),
    Migration::add_column(
        "0012_cleanup_head",
        "ALTER TABLE repo_worker_assignments ADD COLUMN cleanup_head TEXT",
    ),
    Migration::add_column(
        "0013_cleanup_reason",
        "ALTER TABLE repo_worker_assignments ADD COLUMN cleanup_reason TEXT CHECK (
             cleanup_reason IS NULL OR cleanup_reason IN ('absent', 'no_changes', 'merged', 'discarded'))",
    ),
    Migration::add_column(
        "0014_pushed_head",
        "ALTER TABLE repo_worker_assignments ADD COLUMN pushed_head TEXT",
    ),
    Migration::new(
        "0015_journey_backfill",
        "UPDATE repo_worker_assignments
            SET publication_state = 'published'
          WHERE pr IS NOT NULL;
         UPDATE repo_worker_assignments
            SET cleanup_state = CASE state
                WHEN 'finishing' THEN CASE
                    WHEN phase = 'finishing_keep' THEN 'retaining'
                    ELSE 'removing'
                END
                WHEN 'retained' THEN 'retained'
                WHEN 'completed' THEN 'completed'
                WHEN 'stale' THEN 'failed'
                ELSE 'none'
            END",
    ),
    Migration::add_column(
        "0016_branch_policy",
        "ALTER TABLE repo_worker_assignments ADD COLUMN branch_policy TEXT NOT NULL
             DEFAULT 'agent/{slug}'",
    ),
];

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoWorkerConfig {
    /// Where clones and worktrees live. `~` is expanded.
    root: Option<String>,
    /// Repositories that may be worked on, `owner/name`. An empty list
    /// means none: this plugin does not get to pick.
    #[serde(default)]
    repos: Vec<String>,
    /// Per-repository branch templates. The only placeholder is `{slug}`,
    /// which is required exactly once so every assignment remains unique.
    #[serde(default)]
    branch_templates: BTreeMap<String, String>,
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

fn repo_storage_key(repo: &str) -> String {
    // Stable FNV-1a keeps the directory compact and collision-resistant while
    // the readable prefix remains useful to an operator looking at the root.
    // The hash is part of the on-disk contract; do not replace it with
    // `DefaultHasher`, whose output is not stable across implementations.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in repo.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let readable: String = repo
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    format!("{readable}-{hash:016x}")
}

fn github_origin_matches(repo: &str, origin: &str) -> bool {
    let repo = repo.to_ascii_lowercase();
    let origin = origin.to_ascii_lowercase();
    // A transport URL normally has one optional `.git` suffix. The configured
    // GitHub repository name is data, though, and may itself end in `.git`.
    let origin = origin.strip_suffix(".git").unwrap_or(&origin);
    origin == format!("https://github.com/{repo}")
        || origin == format!("git@github.com:{repo}")
        || origin == format!("ssh://git@github.com/{repo}")
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

async fn gh(binary: &Path, dir: Option<&Path>, args: &[&str]) -> Result<String, FlatError> {
    let mut command = tokio::process::Command::new(binary);
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

fn github_repo(repo: &str) -> String {
    format!("github.com/{repo}")
}

async fn git_output(dir: &Path, args: &[&str]) -> Result<String, FlatError> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .kill_on_drop(true)
        .output()
        .await?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn git_predicate(dir: &Path, args: &[&str]) -> Result<bool, FlatError> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .kill_on_drop(true)
        .output()
        .await?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into()),
    }
}

async fn validate_branch_name(branch: &str) -> Result<(), FlatError> {
    let output = tokio::process::Command::new("git")
        .args(["check-ref-format", "--branch", branch])
        .kill_on_drop(true)
        .output()
        .await?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "branch template rendered invalid Git branch '{branch}': {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )
    .into())
}

async fn worktree_is_clean(dir: &Path) -> Result<bool, FlatError> {
    let out = tokio::process::Command::new("git")
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ])
        .current_dir(dir)
        .kill_on_drop(true)
        .output()
        .await?;
    if !out.status.success() {
        return Err(format!(
            "git status: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(out.stdout.is_empty())
}

async fn stable_publication_url(dir: &Path, url: &str) -> Result<(), FlatError> {
    let probe = format!(
        "ciacola-publication-{}",
        ulid::Ulid::new().to_string().to_ascii_lowercase()
    );
    let config = format!("remote.{probe}.url={url}");
    // `remote get-url <name>` ignores remotes supplied only through `-c`, but
    // `remote -v` includes and fully expands them in both fetch and push
    // contexts. This second resolution catches chained insteadOf /
    // pushInsteadOf rules before the snapshotted URL reaches `git push`.
    let remotes = git_output(dir, &["-c", &config, "remote", "-v"]).await?;
    let prefix = format!("{probe}\t");
    let mut fetch = Vec::new();
    let mut push = Vec::new();
    for line in remotes.lines() {
        let Some(value) = line.strip_prefix(&prefix) else {
            continue;
        };
        if let Some(value) = value.strip_suffix(" (fetch)") {
            fetch.push(value);
        } else if let Some(value) = value.strip_suffix(" (push)") {
            push.push(value);
        }
    }
    if fetch.as_slice() != [url] || push.as_slice() != [url] {
        return Err(format!(
            "publication URL '{url}' is rewritten to fetch '{}' / push '{}'; refusing an unstable remote target",
            fetch.join(", "),
            push.join(", "),
        )
        .into());
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct WorktreeSnapshot {
    head: String,
    base_head: String,
    push_url: String,
    commits_ahead: u64,
    has_material_delta: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPr {
    number: u64,
    url: String,
    state: String,
    is_draft: bool,
    head_ref_name: String,
    head_ref_oid: String,
    base_ref_name: String,
    #[serde(default)]
    is_cross_repository: bool,
    #[serde(default)]
    merged_at: Option<String>,
}

impl GhPr {
    fn parsed_state(&self) -> Result<PrState, FlatError> {
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

const GH_PR_FIELDS: &str =
    "number,url,state,isDraft,headRefName,headRefOid,baseRefName,isCrossRepository,mergedAt";

#[derive(Clone)]
struct Repos {
    root: PathBuf,
    allowed: Arc<Vec<String>>,
    gh_binary: PathBuf,
    /// Held across every mutation of a bare repository: clone, fetch,
    /// worktree add/remove, and local branch cleanup. Assignment ownership
    /// is durable in SQLite; this lock prevents unrelated assignments from
    /// making git contend with itself inside one server process.
    cloning: Arc<tokio::sync::Mutex<()>>,
    lifecycle: Arc<tokio::sync::Mutex<()>>,
}

impl Repos {
    fn bare(&self, repo: &str) -> PathBuf {
        let preferred = self.root.join(format!("{}.git", repo_storage_key(repo)));
        if preferred.exists() {
            return preferred;
        }
        // Pre-#73 used an ambiguous `owner__repo.git` encoding. Reuse it
        // only when its recorded origin proves it belongs to this exact
        // GitHub repository; otherwise leave it untouched and create the
        // collision-safe path. This is deliberately conservative because a
        // false adoption would point work at another repository.
        let legacy = self.root.join(format!("{}.git", repo.replace('/', "__")));
        let expected = format!("https://github.com/{repo}.git");
        if legacy.exists()
            && std::fs::read_to_string(legacy.join("config"))
                .ok()
                .and_then(|config| {
                    config.lines().find_map(|line| {
                        line.trim()
                            .strip_prefix("url =")
                            .map(str::trim)
                            .map(str::to_string)
                    })
                })
                .as_deref()
                == Some(expected.as_str())
        {
            return legacy;
        }
        preferred
    }

    fn allows(&self, repo: &str) -> bool {
        self.allowed.iter().any(|r| r == repo)
    }

    /// Clone once into the plugin's own root, then refresh and reuse.
    #[cfg(test)]
    async fn ensure_clone_from(&self, repo: &str, url: &str) -> Result<PathBuf, FlatError> {
        let _guard = self.cloning.lock().await;
        self.ensure_clone_from_locked(repo, url).await
    }

    async fn ensure_clone_from_locked(&self, repo: &str, url: &str) -> Result<PathBuf, FlatError> {
        let bare = self.bare(repo);
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
        let is_bare = git_output(&bare, &["rev-parse", "--is-bare-repository"])
            .await
            .map_err(|e| -> FlatError {
                format!(
                    "existing clone path '{}' is not a usable bare repository: {e}",
                    bare.display()
                )
                .into()
            })?;
        if is_bare != "true" {
            return Err(format!("existing clone path '{}' is not bare", bare.display()).into());
        }
        let actual_origin = git_output(&bare, &["remote", "get-url", "origin"])
            .await
            .map_err(|e| -> FlatError {
                format!(
                    "cannot validate origin of bare repository '{}': {e}",
                    bare.display()
                )
                .into()
            })?;
        #[cfg(test)]
        let origin_matches = actual_origin == url
            || git_output(&bare, &["config", "--get", "remote.origin.url"])
                .await
                .is_ok_and(|configured| configured == url);
        #[cfg(not(test))]
        let origin_matches = actual_origin == url;
        if !origin_matches {
            return Err(format!(
                "bare repository '{}' has origin '{}', expected '{}'",
                bare.display(),
                actual_origin,
                url
            )
            .into());
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

    async fn validate_worktree_at(
        &self,
        path: &Path,
        branch: &str,
        bare: &Path,
    ) -> Result<(), FlatError> {
        if !path.is_dir() {
            return Err(format!("worktree '{}' is not a directory", path.display()).into());
        }
        let actual_branch = git_output(path, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .await
            .map_err(|e| -> FlatError {
                format!(
                    "cannot validate existing worktree '{}': {e}",
                    path.display()
                )
                .into()
            })?;
        if actual_branch != branch {
            return Err(format!(
                "existing worktree '{}' is on branch '{}', expected '{}'",
                path.display(),
                actual_branch,
                branch
            )
            .into());
        }
        let common = git_output(
            path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?;
        let expected = bare.canonicalize().map_err(|e| -> FlatError {
            format!("cannot validate bare repository '{}': {e}", bare.display()).into()
        })?;
        let actual = PathBuf::from(common)
            .canonicalize()
            .map_err(|e| -> FlatError {
                format!("cannot validate git common directory: {e}").into()
            })?;
        if actual != expected {
            return Err(format!(
                "existing worktree '{}' belongs to '{}', expected '{}'",
                path.display(),
                actual.display(),
                expected.display()
            )
            .into());
        }
        Ok(())
    }

    async fn inspect_assignment_worktree(
        &self,
        assignment: &Assignment,
    ) -> Result<WorktreeSnapshot, FlatError> {
        let worktree = Path::new(&assignment.worktree);
        let bare = Path::new(&assignment.bare_path);
        self.validate_worktree_at(worktree, &assignment.branch, bare)
            .await?;
        let Some(base) = assignment.base.as_deref() else {
            return Err("assignment has no durable base branch".into());
        };
        let Some(base_head) = assignment.base_head.as_deref() else {
            return Err(
                "assignment predates durable base-head tracking; retain it and inspect manually"
                    .into(),
            );
        };
        let origin = git_output(worktree, &["remote", "get-url", "origin"]).await?;
        #[cfg(test)]
        let origin_matches = github_origin_matches(&assignment.repo, &origin)
            || git_output(worktree, &["config", "--get", "remote.origin.url"])
                .await
                .is_ok_and(|configured| github_origin_matches(&assignment.repo, &configured));
        #[cfg(not(test))]
        let origin_matches = github_origin_matches(&assignment.repo, &origin);
        if !origin_matches {
            return Err(format!(
                "assigned worktree origin is '{origin}', expected GitHub repository '{}'",
                assignment.repo
            )
            .into());
        }
        let resolved_push_origins = git_output(
            worktree,
            &["remote", "get-url", "--push", "--all", "origin"],
        )
        .await?;
        #[cfg(test)]
        let configured_push_origins =
            match git_output(worktree, &["config", "--get-all", "remote.origin.pushurl"]).await {
                Ok(configured) if !configured.is_empty() => configured,
                _ => git_output(worktree, &["config", "--get-all", "remote.origin.url"]).await?,
            };
        let resolved_push_origins: Vec<&str> = resolved_push_origins
            .lines()
            .filter(|url| !url.trim().is_empty())
            .collect();
        #[cfg(not(test))]
        let identity_push_origins = resolved_push_origins.clone();
        #[cfg(test)]
        let identity_push_origins: Vec<&str> = {
            let configured: Vec<&str> = configured_push_origins
                .lines()
                .filter(|url| !url.trim().is_empty())
                .collect();
            if configured.len() == 1 && github_origin_matches(&assignment.repo, configured[0]) {
                configured
            } else {
                resolved_push_origins.clone()
            }
        };
        if resolved_push_origins.len() != 1
            || identity_push_origins.len() != 1
            || !github_origin_matches(&assignment.repo, identity_push_origins[0])
        {
            return Err(format!(
                "assigned worktree push URLs are '{}', expected only GitHub repository '{}'",
                identity_push_origins.join(", "),
                assignment.repo,
            )
            .into());
        }
        let push_url = resolved_push_origins[0].to_string();
        stable_publication_url(worktree, &push_url).await?;
        if !git_predicate(bare, &["check-ref-format", "--branch", base]).await? {
            return Err(format!("assignment base '{base}' is not a valid branch name").into());
        }
        if !git_predicate(bare, &["check-ref-format", "--branch", &assignment.branch]).await? {
            return Err(format!(
                "assignment branch '{}' is not a valid branch name",
                assignment.branch
            )
            .into());
        }
        let head = git_output(worktree, &["rev-parse", "--verify", "HEAD^{commit}"]).await?;
        let branch_head = git_output(
            worktree,
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{}^{{commit}}", assignment.branch),
            ],
        )
        .await?;
        if branch_head != head {
            return Err(format!(
                "assigned branch '{}' points at {branch_head}, but worktree HEAD is {head}",
                assignment.branch
            )
            .into());
        }
        let canonical_base = git_output(
            worktree,
            &["rev-parse", "--verify", &format!("{base_head}^{{commit}}")],
        )
        .await?;
        if canonical_base != base_head {
            return Err(format!(
                "durable base head '{base_head}' is not a full canonical commit OID"
            )
            .into());
        }
        if !git_predicate(worktree, &["merge-base", "--is-ancestor", base_head, &head]).await? {
            return Err(format!(
                "assigned branch head {head} is not descended from durable base {base_head}"
            )
            .into());
        }
        let commits_ahead = git_output(
            worktree,
            &["rev-list", "--count", &format!("{base_head}..{head}")],
        )
        .await?
        .parse::<u64>()?;
        let has_material_delta =
            !git_predicate(worktree, &["diff", "--quiet", base_head, &head, "--"]).await?;
        Ok(WorktreeSnapshot {
            head,
            base_head: base_head.to_string(),
            push_url,
            commits_ahead,
            has_material_delta,
        })
    }

    async fn remote_branch_head(
        &self,
        assignment: &Assignment,
        push_url: &str,
    ) -> Result<Option<String>, FlatError> {
        let output = git_output(
            Path::new(&assignment.worktree),
            &[
                "ls-remote",
                "--heads",
                push_url,
                &format!("refs/heads/{}", assignment.branch),
            ],
        )
        .await?;
        if output.is_empty() {
            return Ok(None);
        }
        let mut lines = output.lines();
        let head = lines
            .next()
            .and_then(|line| line.split_whitespace().next())
            .ok_or("remote branch query returned malformed output")?
            .to_string();
        if lines.next().is_some() {
            return Err("remote branch query returned more than one exact ref".into());
        }
        Ok(Some(head))
    }

    async fn local_branch_head(
        &self,
        assignment: &Assignment,
    ) -> Result<Option<String>, FlatError> {
        let bare = Path::new(&assignment.bare_path);
        if !bare.exists() {
            return Ok(None);
        }
        let reference = format!("refs/heads/{}", assignment.branch);
        if !git_predicate(bare, &["show-ref", "--verify", "--quiet", &reference]).await? {
            return Ok(None);
        }
        Ok(Some(
            git_output(
                bare,
                &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
            )
            .await?,
        ))
    }

    async fn push_exact(
        &self,
        assignment: &Assignment,
        expected_head: &str,
        expected_remote: Option<&str>,
        push_url: &str,
    ) -> Result<(), FlatError> {
        let reference = format!("refs/heads/{}", assignment.branch);
        let lease = format!(
            "--force-with-lease={reference}:{}",
            expected_remote.unwrap_or_default()
        );
        let refspec = format!("{expected_head}:{reference}");
        git_output(
            Path::new(&assignment.worktree),
            &[
                "push",
                "--no-follow-tags",
                "--no-verify",
                "--recurse-submodules=no",
                &lease,
                push_url,
                &refspec,
            ],
        )
        .await?;
        match self.remote_branch_head(assignment, push_url).await? {
            Some(remote) if remote == expected_head => Ok(()),
            Some(remote) => Err(format!(
                "remote branch moved to {remote} while publishing expected head {expected_head}"
            )
            .into()),
            None => Err("push returned success but the remote branch is absent".into()),
        }
    }

    /// A directory and a branch for one unit of work.
    async fn add_worktree(
        &self,
        repo: &str,
        slug: &str,
        base: &str,
        branch: &str,
    ) -> Result<(PathBuf, String), FlatError> {
        self.add_worktree_from(
            repo,
            slug,
            base,
            branch,
            &format!("https://github.com/{repo}.git"),
        )
        .await
    }

    async fn add_worktree_from(
        &self,
        repo: &str,
        slug: &str,
        base: &str,
        branch: &str,
        url: &str,
    ) -> Result<(PathBuf, String), FlatError> {
        let _guard = self.cloning.lock().await;
        let bare = self.ensure_clone_from_locked(repo, url).await?;
        let path = self.root.join(format!("wt-{slug}"));
        if path.exists() {
            self.validate_worktree_at(&path, branch, &bare).await?;
            return Err(format!(
                "worktree '{}' already exists without an active durable assignment",
                path.display()
            )
            .into());
        }
        // `origin/main` rather than `main`: the refresh writes
        // remote-tracking refs, so this is the one that moves. A local
        // `main` in this clone would be a stale copy at best, and
        // nothing here creates one.
        let mut add = WorktreeCommand::add(&path);
        add.new_branch(branch).commit_ish(format!("origin/{base}"));
        bare_repo(&bare)
            .worktree(add)
            .execute()
            .await
            .map_err(|e| -> FlatError { format!("worktree add: {e}").into() })?;
        Ok((path, branch.to_string()))
    }

    async fn remove_worktree_at(
        &self,
        branch: &str,
        path: &Path,
        bare: &Path,
        expected_head: Option<&str>,
    ) -> Result<(), FlatError> {
        let _guard = self.cloning.lock().await;
        if !path.exists() && !bare.exists() {
            return Ok(());
        }
        if !bare.exists() {
            return Err(format!(
                "cannot clean worktree '{}' because bare repository '{}' is missing",
                path.display(),
                bare.display()
            )
            .into());
        }
        // Cleanup is retried after partial failures, so an already
        // absent worktree is success. The branch deletion below is
        // deliberately idempotent as well.
        if path.exists() {
            let remove = WorktreeCommand::remove(path);
            bare_repo(bare)
                .worktree(remove)
                .execute()
                .await
                .map_err(|e| -> FlatError { format!("worktree remove: {e}").into() })?;
        }
        let exists = tokio::process::Command::new("git")
            .args([
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .current_dir(bare)
            .kill_on_drop(true)
            .status()
            .await?;
        if exists.success() {
            let expected_head = expected_head.ok_or_else(|| -> FlatError {
                format!("refusing to delete local branch '{branch}' without an expected commit")
                    .into()
            })?;
            git_output(
                bare,
                &[
                    "update-ref",
                    "--no-deref",
                    "-d",
                    &format!("refs/heads/{branch}"),
                    expected_head,
                ],
            )
            .await
            .map_err(|e| -> FlatError { format!("branch delete {branch}: {e}").into() })?;
            if git_predicate(
                bare,
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ],
            )
            .await?
            {
                return Err(format!(
                    "local branch '{branch}' moved before compare-and-swap deletion"
                )
                .into());
            }
        } else if exists.code() != Some(1) {
            return Err(format!("cannot inspect branch '{branch}' in '{}'", bare.display()).into());
        }
        Ok(())
    }

    fn worktrees(&self) -> Result<Vec<PathBuf>, FlatError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut worktrees = Vec::new();
        for entry in std::fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("wt-"))
            {
                worktrees.push(path);
            }
        }
        Ok(worktrees)
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
    /// Deprecated compatibility field. Parentage is derived from the
    /// authenticated request context and this value is ignored.
    #[serde(rename = "spawned_by")]
    _spawned_by: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct OpenPrArgs {
    /// The agent whose worktree holds the work.
    agent_id: String,
    /// Full commit OID reviewed for publication. The current assigned branch
    /// must still point at this exact commit before Ciacola will push it. For
    /// compatibility, omitting it on the first call pins the current clean
    /// HEAD; later omissions reuse that durable pin and refuse a moved branch.
    expected_head: Option<String>,
    /// Conventional-commit title used only when a PR must be created.
    title: String,
    /// Body used only when a PR must be created.
    body: String,
    /// Open as a draft. Default true, because a machine-authored pull
    /// request should wait for a person by default.
    draft: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FinishArgs {
    /// The agent to wind up.
    agent_id: Option<String>,
    /// A stale assignment may have failed before an agent existed. Operators
    /// can identify that durable claim directly for explicit cleanup.
    assignment_id: Option<String>,
    /// Keep the worktree for inspection instead of removing it.
    keep: Option<bool>,
    /// Explicitly authorize discarding unpublished or unmerged committed
    /// work at this exact full commit OID. Dirty work is never discarded.
    discard_head: Option<String>,
}

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

#[derive(Default)]
pub struct RepoWorkerPlugin {
    repos: Option<Repos>,
    ctx: Option<PluginContext>,
    branch_policies: BranchPolicies,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AssignmentState {
    Preparing,
    Active,
    Finishing,
    Retained,
    Completed,
    Stale,
}

impl AssignmentState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Active => "active",
            Self::Finishing => "finishing",
            Self::Retained => "retained",
            Self::Completed => "completed",
            Self::Stale => "stale",
        }
    }

    fn parse(value: &str) -> Result<Self, FlatError> {
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
enum PublicationState {
    Unpublished,
    Publishing,
    Published,
    Failed,
}

impl PublicationState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unpublished => "unpublished",
            Self::Publishing => "publishing",
            Self::Published => "published",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self, FlatError> {
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
enum PrState {
    Open,
    Closed,
    Merged,
}

impl PrState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
        }
    }

    fn parse(value: &str) -> Result<Self, FlatError> {
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
enum CleanupState {
    None,
    Retaining,
    Retained,
    Removing,
    Completed,
    Failed,
}

impl CleanupState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Retaining => "retaining",
            Self::Retained => "retained",
            Self::Removing => "removing",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self, FlatError> {
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
enum CleanupReason {
    Absent,
    NoChanges,
    Merged,
    Discarded,
}

impl CleanupReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::NoChanges => "no_changes",
            Self::Merged => "merged",
            Self::Discarded => "discarded",
        }
    }

    fn parse(value: &str) -> Result<Self, FlatError> {
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
struct Assignment {
    assignment_id: String,
    repo: String,
    issue: u64,
    state: AssignmentState,
    phase: String,
    base: Option<String>,
    base_head: Option<String>,
    slug: String,
    branch: String,
    branch_policy: String,
    worktree: String,
    bare_path: String,
    agent_id: Option<String>,
    related_agent_ids: Vec<String>,
    spawned_by: Option<String>,
    expected_head: Option<String>,
    pushed_head: Option<String>,
    publication_state: PublicationState,
    pr: Option<u64>,
    pr_url: Option<String>,
    pr_state: Option<PrState>,
    pr_draft: Option<bool>,
    pr_head: Option<String>,
    pr_base: Option<String>,
    pr_checked_unix: Option<i64>,
    cleanup_state: CleanupState,
    cleanup_head: Option<String>,
    cleanup_reason: Option<CleanupReason>,
    last_error: Option<String>,
    created_unix: i64,
    updated_unix: i64,
    terminal_unix: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyAssignment {
    repo: String,
    issue: u64,
    slug: String,
    branch: String,
    worktree: String,
    pr: Option<u64>,
}

impl Assignment {
    fn from_row(row: sqlx::sqlite::SqliteRow) -> Result<Self, FlatError> {
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

    fn response(&self, created: bool) -> serde_json::Value {
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

    fn conflict(&self, requested_base: Option<&str>) -> String {
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

fn assignment_slug(repo: &str, issue: u64, assignment_id: &str) -> String {
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

fn sqlite_u64(value: u64, label: &str) -> Result<i64, FlatError> {
    i64::try_from(value)
        .map_err(|_| format!("{label} {value} exceeds SQLite's integer range").into())
}

#[derive(Clone)]
struct AssignmentDb {
    pool: SqlitePool,
}

impl AssignmentDb {
    fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn get(&self, repo: &str, issue: u64) -> Result<Option<Assignment>, FlatError> {
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

    async fn get_by_agent(&self, agent_id: &str) -> Result<Option<Assignment>, FlatError> {
        let row = sqlx::query("SELECT * FROM repo_worker_assignments WHERE agent_id = ?1")
            .bind(agent_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(Assignment::from_row).transpose()
    }

    async fn get_by_id(&self, assignment_id: &str) -> Result<Option<Assignment>, FlatError> {
        let row = sqlx::query("SELECT * FROM repo_worker_assignments WHERE assignment_id = ?1")
            .bind(assignment_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(Assignment::from_row).transpose()
    }

    async fn list(&self) -> Result<Vec<Assignment>, FlatError> {
        let rows = sqlx::query(
            "SELECT * FROM repo_worker_assignments ORDER BY updated_unix DESC, assignment_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Assignment::from_row).collect()
    }

    async fn conflicting_resources(
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

    async fn reserve(
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

    async fn set_base(&self, assignment_id: &str, base: &str) -> Result<(), FlatError> {
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
    async fn abandon_reservation(&self, assignment_id: &str) -> Result<bool, FlatError> {
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

    async fn record_pre_resource_failure(
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

    async fn set_phase(&self, assignment_id: &str, phase: &str) -> Result<(), FlatError> {
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

    async fn set_base_head(&self, assignment_id: &str, base_head: &str) -> Result<(), FlatError> {
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

    async fn stale(&self, assignment_id: &str, phase: &str, error: &str) -> Result<(), FlatError> {
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

    async fn terminal(
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

    async fn begin_finish(
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

    async fn restore_after_busy(&self, assignment: &Assignment) -> Result<(), FlatError> {
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

    async fn begin_publication(
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

    async fn record_branch_pushed(
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

    async fn publication_failed(&self, assignment_id: &str, error: &str) -> Result<(), FlatError> {
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

    async fn record_pr_observation(
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

    async fn import_legacy(&self, ledger: &Ledger, repos: &Repos) -> Result<(), FlatError> {
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

    async fn reconcile_on_start(&self, ledger: &Ledger, repos: &Repos) -> Result<(), FlatError> {
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

async fn record_validated_pr(
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

async fn discover_pr(repos: &Repos, assignment: &Assignment) -> Result<Option<GhPr>, FlatError> {
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

fn validate_pr_identity(assignment: &Assignment, pr: &GhPr) -> Result<(), FlatError> {
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

fn pr_response(
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
struct CleanupPlan {
    head: Option<String>,
    reason: CleanupReason,
}

async fn validate_cleanup_resources(
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

async fn cleanup_plan(
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

async fn canonical_approved_head(
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

async fn publish_assignment(
    ctx: &PluginContext,
    repos: &Repos,
    args: &OpenPrArgs,
) -> Result<serde_json::Value, FlatError> {
    let assignments = AssignmentDb::new(ctx.pool.clone());
    let Some(mut assignment) = assignments.get_by_agent(&args.agent_id).await? else {
        if !conventional_title(&args.title) {
            return Err(format!(
                "title '{}' is not conventional-commit form. Use type(scope): subject, e.g. 'fix: ...' or 'feat(board): ...'; types are build, chore, ci, docs, feat, fix, perf, refactor, revert, style, test.",
                args.title
            )
            .into());
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
        return Err(format!(
            "title '{}' is not conventional-commit form. Use type(scope): subject, e.g. 'fix: ...' or 'feat(board): ...'; types are build, chore, ci, docs, feat, fix, perf, refactor, revert, style, test.",
            args.title
        )
        .into());
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

async fn delegated_lineage_hops(
    ledger: &Ledger,
    agent_id: &str,
    manager_agent_id: &str,
    allow_manager_itself: bool,
) -> Result<usize, DelegatedLineageCheckError> {
    let mut current = agent_id.to_string();
    let mut visited = HashSet::new();

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

        if current == manager_agent_id {
            if hops > 0 || allow_manager_itself {
                return Ok(hops);
            }
            return Err(DelegatedLineageCheckError::Refusal(
                DelegatedLineageRefusal::OutsideManager {
                    agent_id: agent_id.to_string(),
                    manager_agent_id: manager_agent_id.to_string(),
                },
            ));
        }

        let Some(parent) = row.spawned_by else {
            return Err(DelegatedLineageCheckError::Refusal(
                DelegatedLineageRefusal::OutsideManager {
                    agent_id: agent_id.to_string(),
                    manager_agent_id: manager_agent_id.to_string(),
                },
            ));
        };
        current = parent;
    }

    Err(DelegatedLineageCheckError::Refusal(
        DelegatedLineageRefusal::TooDeep,
    ))
}

enum DelegatedLineageCheckError {
    Refusal(DelegatedLineageRefusal),
    Ledger(String),
}

impl RepoWorkerPlugin {
    fn assignment_db(&self) -> Option<AssignmentDb> {
        Some(AssignmentDb::new(self.ctx.as_ref()?.pool.clone()))
    }

    async fn assignments(&self) -> Result<Vec<Assignment>, FlatError> {
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

impl Plugin for RepoWorkerPlugin {
    fn name(&self) -> &'static str {
        "repo-worker"
    }

    fn tables(&self) -> &'static [&'static str] {
        &[ASSIGNMENTS_TABLE]
    }

    fn migrations(&self) -> &'static [Migration] {
        MIGRATIONS
    }

    fn setup<'a>(&'a mut self, ctx: &'a PluginContext) -> BoxFut<'a, Result<(), FlatError>> {
        Box::pin(async move {
            #[cfg(test)]
            ciacola_core::plugin::apply_migrations(&ctx.pool, self.name(), MIGRATIONS).await?;
            let config: RepoWorkerConfig = match ctx.config_for(self.name()) {
                Some(value) => value.clone().try_into()?,
                None => RepoWorkerConfig::default(),
            };
            let branch_policies =
                BranchPolicies::new(&config.repos, config.branch_templates.clone())?;
            let root = expand(
                config
                    .root
                    .as_deref()
                    .unwrap_or("~/.local/share/ciacola/repos"),
            );
            let root = if root.is_absolute() {
                root
            } else {
                std::env::current_dir()?.join(root)
            };
            self.repos = Some(Repos {
                root,
                allowed: Arc::new(config.repos),
                gh_binary: PathBuf::from("gh"),
                cloning: Arc::new(tokio::sync::Mutex::new(())),
                lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            });
            self.branch_policies = branch_policies;
            self.ctx = Some(ctx.clone());
            let assignments = AssignmentDb::new(ctx.pool.clone());
            assignments
                .import_legacy(&ctx.ledger, self.repos.as_ref().expect("repos"))
                .await?;
            assignments
                .reconcile_on_start(&ctx.ledger, self.repos.as_ref().expect("repos"))
                .await?;
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
- Call start_issue first. When it returns created=true, send the implementation
  prompt, then wait. When it returns created=false, the durable assignment was
  already active: inspect and reuse that agent, and do not blindly send the
  implementation prompt a second time.
- Pass timeout_secs when waiting; the default is 120 and real work runs longer,
  and a turn cut short loses its session.
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
   exact command you verified with and its output; the full commit OID from \
   `git rev-parse HEAD`; then a pull request \
   title on one line, in conventional-commit form like the commit \
   (open_pr refuses any other shape and publishes only the reviewed, durably \
   pinned OID), \
   and a pull request body whose last line is \
   Closes #{{issue}} and nothing else. Those two go to open_pr exactly \
   as given, so write them to be used rather than edited.

You cannot push, open pull requests, or comment. Those are the server's \
to do, on purpose."
                    .into(),
            },
        ]
    }

    fn tools(&self, surface: Surface) -> Vec<Tool> {
        let (Some(repos), Some(ctx)) = (self.repos.clone(), self.ctx.clone()) else {
            return Vec::new();
        };
        // Higher-level plugin wiring consumes the same configured catalog as
        // roles, spawn_role, completion, and persistent role agents.
        let roles = ctx.roles.clone();
        let branch_policies = self.branch_policies.clone();

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

    fn board_section(&self) -> BoxFut<'_, Option<Section>> {
        Box::pin(async move {
            let all = match self.assignments().await {
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
            let ledger: Option<Ledger> = self.ctx.as_ref().map(|c| c.ledger.clone());
            let mut html = String::from(
                "<table><tr><th>assignment</th><th>publication</th><th>cleanup</th>\
                 <th>issue</th><th>agent</th><th>branch</th><th>policy</th><th>pr</th>\
                 <th>worktree</th><th>detail</th></tr>",
            );
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
                html.push_str(&format!(
                    "<tr><td>{state}</td><td>{publication}</td><td>{cleanup}</td>\
                     <td>{repo}#{issue}</td><td>{agent}</td>\
                     <td class=\"mono\">{branch}</td><td class=\"mono\">{policy}</td><td>{pr}</td>\
                     <td class=\"dim mono\">{wt}</td><td>{detail}</td></tr>",
                    state = ciacola_core::render::esc(a.state.as_str()),
                    publication = ciacola_core::render::esc(&match a.pr_state {
                        Some(pr_state) =>
                            format!("{} / {}", a.publication_state.as_str(), pr_state.as_str()),
                        None => a.publication_state.as_str().to_string(),
                    }),
                    cleanup = ciacola_core::render::esc(a.cleanup_state.as_str()),
                    repo = ciacola_core::render::esc(&a.repo),
                    issue = a.issue,
                    agent = agent,
                    branch = ciacola_core::render::esc(&a.branch),
                    policy = ciacola_core::render::esc(&a.branch_policy),
                    pr = match (a.pr, a.pr_url.as_deref()) {
                        (Some(number), Some(url)) => format!(
                            "<a href=\"{}\">#{number}</a>",
                            ciacola_core::render::esc(url)
                        ),
                        (Some(number), None) => format!("#{number}"),
                        (None, _) => "-".into(),
                    },
                    wt = ciacola_core::render::esc(&a.worktree),
                    detail =
                        ciacola_core::render::esc(a.last_error.as_deref().unwrap_or_else(|| {
                            if matches!(
                                a.state,
                                AssignmentState::Active | AssignmentState::Retained
                            ) && a.base_head.is_none()
                            {
                                "missing durable base-head provenance; retain and inspect"
                            } else {
                                &a.phase
                            }
                        }),),
                ));
            }
            html.push_str("</table>");
            Some(Section {
                title: "repository work".into(),
                html,
            })
        })
    }

    fn health(&self) -> BoxFut<'_, serde_json::Value> {
        Box::pin(async move {
            let all = match self.assignments().await {
                Ok(all) => all,
                Err(e) => return json!({"status": "degraded", "assignment_error": e.to_string()}),
            };
            let worktrees = match self.repos.as_ref().map(Repos::worktrees) {
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
                .filter(|a| {
                    a.state == AssignmentState::Completed && Path::new(&a.worktree).exists()
                })
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
            if let Some(ctx) = &self.ctx {
                for assignment in all.iter().filter(|a| {
                    matches!(a.state, AssignmentState::Active | AssignmentState::Retained)
                }) {
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

    async fn configure_local_transport(bare: &Path, origin: &Path, repo: &str) {
        let expected = format!("https://github.com/{repo}.git");
        let local = format!("file://{}", origin.display());
        git(bare, &["config", "remote.origin.url", &expected]).await;
        git(
            bare,
            &["config", &format!("url.{local}.insteadOf"), &expected],
        )
        .await;
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
            branch_policies: BranchPolicies::default(),
        }
    }

    fn configured_branch_plugin(
        ctx: PluginContext,
        repos: Repos,
        template: &str,
    ) -> RepoWorkerPlugin {
        let allowed = vec!["local/repo".to_string()];
        let configured = BTreeMap::from([("local/repo".to_string(), template.to_string())]);
        RepoWorkerPlugin {
            repos: Some(repos),
            ctx: Some(ctx),
            branch_policies: BranchPolicies::new(&allowed, configured).expect("branch policy"),
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
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        };
        std::fs::create_dir_all(&repos.root).expect("mkdir root");
        CloneCommand::new(format!("file://{}", origin.display()))
            .bare()
            .directory(repos.bare("local/repo"))
            .execute()
            .await
            .expect("clone");
        configure_local_transport(&repos.bare("local/repo"), &origin, "local/repo").await;
        (tmp, repos)
    }

    #[cfg(unix)]
    struct FakeGh {
        binary: PathBuf,
        list: PathBuf,
        view: PathBuf,
        created_list: PathBuf,
        created_view: PathBuf,
        fail_list: PathBuf,
        log: PathBuf,
    }

    #[cfg(unix)]
    impl FakeGh {
        fn new(root: &Path) -> Self {
            use std::os::unix::fs::PermissionsExt;

            let dir = root.join("fake-gh");
            std::fs::create_dir_all(&dir).expect("fake gh dir");
            let fake = Self {
                binary: dir.join("gh"),
                list: dir.join("list.json"),
                view: dir.join("view.json"),
                created_list: dir.join("created-list.json"),
                created_view: dir.join("created-view.json"),
                fail_list: dir.join("fail-list"),
                log: dir.join("argv.log"),
            };
            std::fs::write(&fake.list, "[]").expect("empty PR list");
            std::fs::write(&fake.created_list, "[]").expect("empty created list");
            std::fs::write(&fake.created_view, "{}").expect("empty created view");
            let script = format!(
                "#!/bin/sh\n\
                 printf '%s\\n' \"$*\" >> '{}'\n\
                 if [ \"$1 $2\" = 'pr list' ]; then\n\
                   if [ -f '{}' ]; then echo 'injected list failure' >&2; exit 1; fi\n\
                   cat '{}'; exit 0\n\
                 fi\n\
                 if [ \"$1 $2\" = 'pr view' ]; then\n\
                   if [ -f '{}' ]; then cat '{}'; exit 0; fi\n\
                   echo 'missing fake PR' >&2; exit 1\n\
                 fi\n\
                 if [ \"$1 $2\" = 'pr create' ]; then\n\
                   cp '{}' '{}'; cp '{}' '{}';\n\
                   printf 'https://github.com/local/repo/pull/41\\n'; exit 0\n\
                 fi\n\
                 if [ \"$1 $2\" = 'repo view' ]; then printf 'main\\n'; exit 0; fi\n\
                 echo \"unsupported fake gh invocation: $*\" >&2; exit 2\n",
                fake.log.display(),
                fake.fail_list.display(),
                fake.list.display(),
                fake.view.display(),
                fake.view.display(),
                fake.created_list.display(),
                fake.list.display(),
                fake.created_view.display(),
                fake.view.display(),
            );
            std::fs::write(&fake.binary, script).expect("fake gh script");
            let mut permissions = std::fs::metadata(&fake.binary)
                .expect("fake gh metadata")
                .permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&fake.binary, permissions).expect("fake gh executable");
            fake
        }

        fn pr(
            &self,
            number: u64,
            state: &str,
            draft: bool,
            branch: &str,
            head: &str,
            base: &str,
        ) -> serde_json::Value {
            json!({
                "number": number,
                "url": format!("https://github.com/local/repo/pull/{number}"),
                "state": state,
                "isDraft": draft,
                "headRefName": branch,
                "headRefOid": head,
                "baseRefName": base,
                "isCrossRepository": false,
                "mergedAt": if state.eq_ignore_ascii_case("merged") {
                    Some("2026-08-09T00:00:00Z")
                } else {
                    None
                },
            })
        }

        fn set_existing(&self, pr: Option<&serde_json::Value>) {
            match pr {
                Some(pr) => {
                    std::fs::write(&self.list, json!([pr]).to_string()).expect("fake PR list");
                    std::fs::write(&self.view, pr.to_string()).expect("fake PR view");
                }
                None => {
                    std::fs::write(&self.list, "[]").expect("empty PR list");
                    std::fs::remove_file(&self.view).ok();
                }
            }
        }

        fn set_created(&self, pr: &serde_json::Value) {
            std::fs::write(&self.created_list, json!([pr]).to_string()).expect("created PR list");
            std::fs::write(&self.created_view, pr.to_string()).expect("created PR view");
        }

        fn set_created_list_raw(&self, contents: &str) {
            std::fs::write(&self.created_list, contents).expect("raw created PR list");
        }

        fn fail_list(&self) {
            std::fs::write(&self.fail_list, "fail").expect("fail list marker");
        }

        fn log(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    async fn commit_change(worktree: &Path, name: &str, contents: &str) -> String {
        git(worktree, &["config", "user.email", "t@example.com"]).await;
        git(worktree, &["config", "user.name", "t"]).await;
        std::fs::write(worktree.join(name), contents).expect("write change");
        git(worktree, &["add", name]).await;
        git(worktree, &["commit", "-qm", "fix: test change"]).await;
        git_output(worktree, &["rev-parse", "--verify", "HEAD^{commit}"])
            .await
            .expect("commit head")
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
        ciacola_core::plugin::apply_migrations(&pool, "repo-worker", MIGRATIONS)
            .await
            .expect("repo-worker migrations");
        (plugin(context(pool, ledger.clone()), repos), ledger)
    }

    async fn memory_plugin_with_branch_template(
        repos: Repos,
        template: &str,
    ) -> (RepoWorkerPlugin, Ledger) {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool.clone()).await.expect("ledger");
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
        ciacola_core::plugin::apply_migrations(&pool, "repo-worker", MIGRATIONS)
            .await
            .expect("repo-worker migrations");
        (
            configured_branch_plugin(context(pool, ledger.clone()), repos, template),
            ledger,
        )
    }

    struct AssignmentFixture<'a> {
        agent_id: &'a str,
        repo: &'a str,
        issue: u64,
        slug: &'a str,
        branch: &'a str,
        worktree: &'a Path,
        pr: Option<u64>,
    }

    async fn record_assignment(
        plugin: &RepoWorkerPlugin,
        fixture: AssignmentFixture<'_>,
    ) -> Assignment {
        let assignment_id = ulid::Ulid::new().to_string();
        let now = ciacola_core::now_unix();
        let bare = plugin
            .repos
            .as_ref()
            .expect("repos")
            .bare(fixture.repo)
            .display()
            .to_string();
        let base_head = git_output(
            fixture.worktree,
            &["rev-parse", "--verify", "HEAD^{commit}"],
        )
        .await
        .ok();
        sqlx::query(
            "INSERT INTO repo_worker_assignments
                 (assignment_id, repo, issue_number, state, phase, base, base_head, slug, branch,
                  worktree, bare_path, agent_id, related_agent_ids, pr, created_unix, updated_unix)
             VALUES (?1, ?2, ?3, 'active', 'ready', 'main', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
        )
        .bind(&assignment_id)
        .bind(fixture.repo)
        .bind(sqlite_u64(fixture.issue, "issue").expect("issue range"))
        .bind(base_head)
        .bind(fixture.slug)
        .bind(fixture.branch)
        .bind(fixture.worktree.display().to_string())
        .bind(bare)
        .bind(fixture.agent_id)
        .bind(serde_json::to_string(&[fixture.agent_id]).expect("agent ids"))
        .bind(fixture.pr.map(|pr| sqlite_u64(pr, "pr").expect("pr range")))
        .bind(now)
        .execute(&plugin.ctx.as_ref().expect("context").pool)
        .await
        .expect("assignment");
        plugin
            .assignment_db()
            .expect("assignment db")
            .get_by_id(&assignment_id)
            .await
            .expect("read assignment")
            .expect("assignment row")
    }

    struct DelegationFixture {
        plugin: RepoWorkerPlugin,
        ledger: Ledger,
        manager: String,
        creator: String,
        owner: String,
        assignment_id: String,
    }

    async fn delegation_fixture(indirect: bool) -> DelegationFixture {
        let root =
            std::env::temp_dir().join(format!("ciacola-delegated-preflight-{}", ulid::Ulid::new()));
        let repos = Repos {
            root: root.clone(),
            allowed: Arc::new(vec!["local/repo".into()]),
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        };
        let (plugin, ledger) = memory_plugin(repos).await;
        let manager = ledger
            .create_agent(&ciacola_core::AgentDef::new("manager", "s"), None)
            .await
            .expect("manager");
        let creator = if indirect {
            ledger
                .create_agent(
                    &ciacola_core::AgentDef::new("dispatcher", "s"),
                    Some(&manager),
                )
                .await
                .expect("dispatcher")
        } else {
            manager.clone()
        };
        let owner = ledger
            .create_agent(
                &ciacola_core::AgentDef::new("implementer", "s"),
                Some(&creator),
            )
            .await
            .expect("owner");
        let assignment_id = ulid::Ulid::new().to_string();
        let now = ciacola_core::now_unix();
        sqlx::query(
            "INSERT INTO repo_worker_assignments
                 (assignment_id, repo, issue_number, state, phase, base, slug, branch,
                  worktree, bare_path, agent_id, related_agent_ids, spawned_by,
                  created_unix, updated_unix)
             VALUES (?1, 'local/repo', 81, 'active', 'ready', 'main', 'delegated',
                     'agent/delegated', ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        )
        .bind(&assignment_id)
        .bind(root.join("wt-delegated").display().to_string())
        .bind(root.join("repo.git").display().to_string())
        .bind(&owner)
        .bind(serde_json::to_string(&[&owner]).expect("owners"))
        .bind(&creator)
        .bind(now)
        .execute(&plugin.ctx.as_ref().expect("context").pool)
        .await
        .expect("assignment");
        DelegationFixture {
            plugin,
            ledger,
            manager,
            creator,
            owner,
            assignment_id,
        }
    }

    fn delegated_open(assignment_id: &str) -> DelegatedAssignmentRequest {
        DelegatedAssignmentRequest::OpenPr(DelegatedOpenPrRequest {
            assignment_id: assignment_id.to_string(),
            expected_head: "0123456789abcdef0123456789abcdef01234567".to_string(),
            title: "fix: delegated test".to_string(),
            body: "Closes #81.".to_string(),
        })
    }

    fn delegated_finish(assignment_id: &str) -> DelegatedAssignmentRequest {
        DelegatedAssignmentRequest::FinishIssue(DelegatedFinishIssueRequest {
            assignment_id: assignment_id.to_string(),
            disposition: DelegatedFinishDisposition::RemoveIfMergedOrUnchanged,
        })
    }

    fn delegated_policy(actions: impl IntoIterator<Item = DelegatableAction>) -> DelegationPolicy {
        DelegationPolicy::new(actions).expect("delegation policy")
    }

    #[tokio::test]
    async fn delegated_preflight_proves_direct_and_indirect_durable_lineage() {
        let direct = delegation_fixture(false).await;
        let policy = delegated_policy([
            DelegatableAction::RepoWorkerOpenPr,
            DelegatableAction::RepoWorkerFinishIssue,
        ]);
        let direct_proof = direct
            .plugin
            .preflight_delegated_assignment(
                &direct.manager,
                &policy,
                &delegated_open(&direct.assignment_id),
            )
            .await
            .expect("direct assignment");
        assert_eq!(direct_proof.manager_agent_id(), direct.manager);
        assert_eq!(direct_proof.assignment_id(), direct.assignment_id);
        assert_eq!(direct_proof.owner_agent_id(), direct.owner);
        assert_eq!(direct_proof.action(), DelegatableAction::RepoWorkerOpenPr);
        assert_eq!(direct_proof.creator_hops(), 0);
        assert_eq!(direct_proof.owner_hops(), 1);

        let indirect = delegation_fixture(true).await;
        let indirect_proof = indirect
            .plugin
            .preflight_delegated_assignment(
                &indirect.manager,
                &policy,
                &delegated_finish(&indirect.assignment_id),
            )
            .await
            .expect("indirect assignment");
        assert_eq!(
            indirect_proof.action(),
            DelegatableAction::RepoWorkerFinishIssue
        );
        assert_eq!(indirect_proof.creator_hops(), 1);
        assert_eq!(indirect_proof.owner_hops(), 2);
    }

    #[tokio::test]
    async fn delegated_preflight_fails_closed_on_policy_manager_and_assignment_gaps() {
        let fixture = delegation_fixture(false).await;
        let open_only = delegated_policy([DelegatableAction::RepoWorkerOpenPr]);
        assert_eq!(
            fixture
                .plugin
                .preflight_delegated_assignment(
                    &fixture.manager,
                    &open_only,
                    &delegated_finish(&fixture.assignment_id),
                )
                .await,
            Err(DelegatedAssignmentRefusal::ActionNotGranted {
                action: DelegatableAction::RepoWorkerFinishIssue,
            })
        );
        assert_eq!(
            fixture
                .plugin
                .preflight_delegated_assignment(
                    "missing-manager",
                    &open_only,
                    &delegated_open(&fixture.assignment_id),
                )
                .await,
            Err(DelegatedAssignmentRefusal::ManagerNotFound {
                agent_id: "missing-manager".to_string(),
            })
        );
        assert_eq!(
            fixture
                .plugin
                .preflight_delegated_assignment(
                    &fixture.manager,
                    &open_only,
                    &delegated_open("missing-assignment"),
                )
                .await,
            Err(DelegatedAssignmentRefusal::AssignmentNotFound {
                assignment_id: "missing-assignment".to_string(),
            })
        );

        let ownerless = delegation_fixture(false).await;
        sqlx::query("UPDATE repo_worker_assignments SET agent_id = NULL WHERE assignment_id = ?1")
            .bind(&ownerless.assignment_id)
            .execute(&ownerless.plugin.ctx.as_ref().expect("context").pool)
            .await
            .expect("remove owner");
        assert_eq!(
            ownerless
                .plugin
                .preflight_delegated_assignment(
                    &ownerless.manager,
                    &open_only,
                    &delegated_open(&ownerless.assignment_id),
                )
                .await,
            Err(DelegatedAssignmentRefusal::AssignmentOwnerMissing {
                assignment_id: ownerless.assignment_id,
            })
        );

        assert!(
            fixture
                .ledger
                .retire_agent(&fixture.manager)
                .await
                .expect("retire manager")
        );
        assert_eq!(
            fixture
                .plugin
                .preflight_delegated_assignment(
                    &fixture.manager,
                    &open_only,
                    &delegated_open(&fixture.assignment_id),
                )
                .await,
            Err(DelegatedAssignmentRefusal::ManagerRetired {
                agent_id: fixture.manager,
            })
        );
    }

    #[tokio::test]
    async fn delegated_preflight_refuses_cross_manager_missing_cyclic_and_ambiguous_lineage() {
        let policy = delegated_policy([DelegatableAction::RepoWorkerOpenPr]);

        let cross = delegation_fixture(true).await;
        let other_manager = cross
            .ledger
            .create_agent(&ciacola_core::AgentDef::new("other", "s"), None)
            .await
            .expect("other manager");
        let refusal = cross
            .plugin
            .preflight_delegated_assignment(
                &other_manager,
                &policy,
                &delegated_open(&cross.assignment_id),
            )
            .await
            .expect_err("cross-manager assignment");
        assert!(
            matches!(
                refusal,
                DelegatedAssignmentRefusal::Lineage {
                    subject: DelegatedLineageSubject::AssignmentCreator,
                    reason: DelegatedLineageRefusal::OutsideManager { .. }
                }
            ),
            "got: {refusal}"
        );

        let missing = delegation_fixture(true).await;
        sqlx::query("UPDATE agents SET spawned_by = 'missing-link' WHERE agent_id = ?1")
            .bind(&missing.owner)
            .execute(&missing.plugin.ctx.as_ref().expect("context").pool)
            .await
            .expect("break owner lineage");
        let refusal = missing
            .plugin
            .preflight_delegated_assignment(
                &missing.manager,
                &policy,
                &delegated_open(&missing.assignment_id),
            )
            .await
            .expect_err("missing link");
        assert!(
            matches!(
                refusal,
                DelegatedAssignmentRefusal::Lineage {
                    subject: DelegatedLineageSubject::AssignmentOwner,
                    reason: DelegatedLineageRefusal::MissingAgent { .. }
                }
            ),
            "got: {refusal}"
        );

        let cycle = delegation_fixture(true).await;
        sqlx::query("UPDATE agents SET spawned_by = ?2 WHERE agent_id = ?1")
            .bind(&cycle.creator)
            .bind(&cycle.owner)
            .execute(&cycle.plugin.ctx.as_ref().expect("context").pool)
            .await
            .expect("cycle lineage");
        let refusal = cycle
            .plugin
            .preflight_delegated_assignment(
                &cycle.manager,
                &policy,
                &delegated_open(&cycle.assignment_id),
            )
            .await
            .expect_err("cycle");
        assert!(
            matches!(
                refusal,
                DelegatedAssignmentRefusal::Lineage {
                    subject: DelegatedLineageSubject::AssignmentCreator,
                    reason: DelegatedLineageRefusal::Cycle { .. }
                }
            ),
            "got: {refusal}"
        );

        let legacy = delegation_fixture(false).await;
        sqlx::query(
            "UPDATE repo_worker_assignments SET spawned_by = NULL WHERE assignment_id = ?1",
        )
        .bind(&legacy.assignment_id)
        .execute(&legacy.plugin.ctx.as_ref().expect("context").pool)
        .await
        .expect("legacy creator");
        assert_eq!(
            legacy
                .plugin
                .preflight_delegated_assignment(
                    &legacy.manager,
                    &policy,
                    &delegated_open(&legacy.assignment_id),
                )
                .await,
            Err(DelegatedAssignmentRefusal::AssignmentCreatorMissing {
                assignment_id: legacy.assignment_id,
            })
        );

        let ambiguous = delegation_fixture(false).await;
        sqlx::query(
            "UPDATE repo_worker_assignments SET related_agent_ids = ?2 WHERE assignment_id = ?1",
        )
        .bind(&ambiguous.assignment_id)
        .bind(serde_json::to_string(&[&ambiguous.owner, &ambiguous.manager]).expect("owners"))
        .execute(&ambiguous.plugin.ctx.as_ref().expect("context").pool)
        .await
        .expect("ambiguous owners");
        let refusal = ambiguous
            .plugin
            .preflight_delegated_assignment(
                &ambiguous.manager,
                &policy,
                &delegated_open(&ambiguous.assignment_id),
            )
            .await
            .expect_err("ambiguous owners");
        assert!(
            matches!(refusal, DelegatedAssignmentRefusal::AmbiguousOwners { .. }),
            "got: {refusal}"
        );
    }

    #[test]
    fn branch_policy_is_explicit_unique_and_repository_scoped() {
        let config: RepoWorkerConfig = toml::from_str(
            r#"
repos = ["local/repo"]
branch_templates = { "local/repo" = "fix/{slug}" }
"#,
        )
        .expect("documented branch policy config");
        let allowed = config.repos;
        let policies =
            BranchPolicies::new(&allowed, config.branch_templates).expect("configured policy");
        assert_eq!(policies.for_repo("local/repo").as_str(), "fix/{slug}");
        assert_eq!(
            BranchPolicies::new(&allowed, BTreeMap::new())
                .expect("default policy")
                .for_repo("local/repo")
                .as_str(),
            DEFAULT_BRANCH_TEMPLATE
        );

        for template in ["fix/static", "fix/{slug}/{slug}", "fix/{issue}/{slug}"] {
            assert!(
                BranchPolicies::new(
                    &allowed,
                    BTreeMap::from([("local/repo".to_string(), template.to_string())]),
                )
                .is_err(),
                "unsafe template must be rejected: {template}"
            );
        }
        assert!(
            BranchPolicies::new(
                &allowed,
                BTreeMap::from([("other/repo".to_string(), "fix/{slug}".to_string())]),
            )
            .expect_err("unknown repository")
            .contains("not present")
        );
    }

    #[tokio::test]
    async fn invalid_rendered_branch_is_refused_before_durable_or_git_mutation() {
        let (tmp, repos) = local_repos("invalid-branch-policy").await;
        let (plugin, ledger) =
            memory_plugin_with_branch_template(repos.clone(), "fix/{slug}.lock").await;

        let result = operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 77, "base": "main"}))
            .await;
        let rendered = serde_json::to_string(&result).expect("render refusal");

        assert!(
            rendered.contains("rendered invalid Git branch"),
            "got: {rendered}"
        );
        assert!(plugin.assignments().await.expect("assignments").is_empty());
        assert!(ledger.list_agents().await.expect("agents").is_empty());
        assert!(repos.worktrees().expect("worktrees").is_empty());
        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test]
    async fn sequential_start_replays_one_durable_assignment() {
        let (tmp, repos) = local_repos("sequential-start").await;
        let (plugin, ledger) = memory_plugin(repos).await;
        let args = json!({"repo": "local/repo", "issue": 73, "base": "main"});

        let first = operator_tool(&plugin, "start_issue")
            .call(args.clone())
            .await;
        let replay = operator_tool(&plugin, "start_issue").call(args).await;
        let first = serde_json::to_string(&first).expect("render first start");
        let replay = serde_json::to_string(&replay).expect("render replay");

        assert!(first.contains("\"created\":true"), "got: {first}");
        assert!(replay.contains("\"created\":false"), "got: {replay}");
        let assignments = plugin.assignments().await.expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].state, AssignmentState::Active);
        assert_eq!(ledger.list_agents().await.expect("agents").len(), 1);
        assert_eq!(
            plugin
                .repos
                .as_ref()
                .expect("repos")
                .worktrees()
                .expect("worktrees")
                .len(),
            1
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test]
    async fn concurrent_start_has_one_assignment_agent_and_worktree() {
        let (tmp, repos) = local_repos("concurrent-start").await;
        let (plugin, ledger) = memory_plugin(repos).await;
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let start = |tool: Tool, barrier: Arc<tokio::sync::Barrier>| async move {
            barrier.wait().await;
            tool.call(json!({"repo": "local/repo", "issue": 73, "base": "main"}))
                .await
        };
        let (left, right) = tokio::join!(
            start(operator_tool(&plugin, "start_issue"), barrier.clone()),
            start(operator_tool(&plugin, "start_issue"), barrier),
        );

        let left = serde_json::to_string(&left).expect("render left");
        let right = serde_json::to_string(&right).expect("render right");
        assert_eq!(
            usize::from(left.contains("\"created\":true"))
                + usize::from(right.contains("\"created\":true")),
            1,
            "one call must activate the claim: left={left}; right={right}"
        );
        assert!(
            left.contains("\"created\":false")
                || right.contains("\"created\":false")
                || left.contains("preparing")
                || right.contains("preparing"),
            "the loser must replay or observe the in-flight claim: left={left}; right={right}"
        );
        assert_eq!(plugin.assignments().await.expect("assignments").len(), 1);
        assert_eq!(ledger.list_agents().await.expect("agents").len(), 1);
        assert_eq!(
            plugin
                .repos
                .as_ref()
                .expect("repos")
                .worktrees()
                .expect("worktrees")
                .len(),
            1
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test]
    async fn file_backed_restart_replays_the_active_assignment() {
        let (tmp, repos) = local_repos("restart-replay").await;
        let db_path = tmp.join("ciacola.db");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("file pool");
        let ledger = Ledger::setup(pool.clone()).await.expect("ledger");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS plugin_kv (
                 plugin TEXT NOT NULL,
                 key TEXT NOT NULL,
                 value TEXT NOT NULL,
                 updated_unix INTEGER NOT NULL,
                 PRIMARY KEY (plugin, key))",
        )
        .execute(&pool)
        .await
        .expect("plugin store");
        let mut ctx = context(pool.clone(), ledger.clone());
        ctx.plugin_config = toml::from_str(&format!(
            "[repo-worker]\nroot = {:?}\nrepos = [\"local/repo\"]",
            repos.root.display().to_string()
        ))
        .expect("config");
        let args = json!({"repo": "local/repo", "issue": 73, "base": "main"});

        let mut first = RepoWorkerPlugin::default();
        first.setup(&ctx).await.expect("first setup");
        let created = operator_tool(&first, "start_issue")
            .call(args.clone())
            .await;
        let created = serde_json::to_string(&created).expect("render create");
        assert!(created.contains("\"created\":true"), "got: {created}");
        let original = first.assignments().await.expect("assignments")[0].clone();
        drop(first);
        drop(ctx);
        drop(ledger);
        pool.close().await;

        let fresh_pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&db_path)
                    .create_if_missing(false),
            )
            .await
            .expect("reopened file pool");
        let fresh_ledger = Ledger::setup(fresh_pool.clone())
            .await
            .expect("reopened ledger");
        let mut fresh_ctx = context(fresh_pool.clone(), fresh_ledger.clone());
        fresh_ctx.plugin_config = toml::from_str(&format!(
            "[repo-worker]\nroot = {:?}\nrepos = [\"local/repo\"]",
            repos.root.display().to_string()
        ))
        .expect("restart config");
        let mut restarted = RepoWorkerPlugin::default();
        restarted.setup(&fresh_ctx).await.expect("restart setup");
        let replay = operator_tool(&restarted, "start_issue").call(args).await;
        let replay = serde_json::to_string(&replay).expect("render replay");
        assert!(replay.contains("\"created\":false"), "got: {replay}");
        let after = restarted.assignments().await.expect("assignments");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].assignment_id, original.assignment_id);
        assert_eq!(after[0].agent_id, original.agent_id);
        assert_eq!(fresh_ledger.list_agents().await.expect("agents").len(), 1);
        drop(restarted);
        drop(fresh_ctx);
        drop(fresh_ledger);
        fresh_pool.close().await;
        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test]
    async fn agent_insert_failure_leaves_no_agent_and_a_stale_claim() {
        let (tmp, repos) = local_repos("agent-insert-failure").await;
        let (plugin, ledger) = memory_plugin(repos).await;
        let pool = &plugin.ctx.as_ref().expect("context").pool;
        sqlx::query(
            "CREATE TRIGGER inject_agent_insert_failure
             BEFORE INSERT ON agents BEGIN
                 SELECT RAISE(FAIL, 'injected agent insert failure');
             END",
        )
        .execute(pool)
        .await
        .expect("trigger");

        let out = operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 73, "base": "main"}))
            .await;
        let rendered = serde_json::to_string(&out).expect("render");
        assert!(
            rendered.contains("injected agent insert failure"),
            "got: {rendered}"
        );
        assert!(ledger.list_agents().await.expect("agents").is_empty());
        let assignments = plugin.assignments().await.expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].state, AssignmentState::Stale);
        assert_eq!(assignments[0].phase, "agent_activation");
        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test]
    async fn activation_failure_rolls_back_agent_and_can_clean_the_worktree() {
        let (tmp, repos) = local_repos("activation-failure").await;
        let (plugin, ledger) = memory_plugin(repos).await;
        let pool = &plugin.ctx.as_ref().expect("context").pool;
        sqlx::query(
            "CREATE TRIGGER inject_activation_failure
             BEFORE UPDATE OF state ON repo_worker_assignments
             WHEN NEW.state = 'active' BEGIN
                 SELECT RAISE(FAIL, 'injected activation failure');
             END",
        )
        .execute(pool)
        .await
        .expect("trigger");

        let out = operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 73, "base": "main"}))
            .await;
        let rendered = serde_json::to_string(&out).expect("render");
        assert!(
            rendered.contains("injected activation failure"),
            "got: {rendered}"
        );
        assert!(ledger.list_agents().await.expect("agents").is_empty());
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        assert_eq!(assignment.state, AssignmentState::Stale);
        assert_eq!(assignment.phase, "agent_activation");
        assert!(Path::new(&assignment.worktree).exists());

        let cleaned = operator_tool(&plugin, "finish_issue")
            .call(json!({"assignment_id": assignment.assignment_id}))
            .await;
        let cleaned = serde_json::to_string(&cleaned).expect("render cleanup");
        assert!(
            cleaned.contains("\"state\":\"completed\""),
            "got: {cleaned}"
        );
        assert!(!Path::new(&assignment.worktree).exists());
        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test]
    async fn provisioning_failure_never_creates_an_agent_and_is_cleanable() {
        let (tmp, repos) = local_repos("provisioning-failure").await;
        git(
            &repos.bare("local/repo"),
            &[
                "config",
                "remote.origin.url",
                "https://github.com/wrong/repo.git",
            ],
        )
        .await;
        let (plugin, ledger) = memory_plugin(repos).await;

        let out = operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 73, "base": "main"}))
            .await;
        let rendered = serde_json::to_string(&out).expect("render");
        assert!(rendered.contains("expected"), "got: {rendered}");
        assert!(!rendered.contains("\"created\":true"), "got: {rendered}");
        assert!(ledger.list_agents().await.expect("agents").is_empty());
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        assert_eq!(assignment.state, AssignmentState::Stale);
        assert_eq!(assignment.phase, "worktree");
        assert!(!Path::new(&assignment.worktree).exists());

        let cleaned = operator_tool(&plugin, "finish_issue")
            .call(json!({"assignment_id": assignment.assignment_id}))
            .await;
        let cleaned = serde_json::to_string(&cleaned).expect("render cleanup");
        assert!(
            cleaned.contains("\"state\":\"completed\""),
            "got: {cleaned}"
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test]
    async fn legacy_slug_collision_imports_stale_and_refuses_ambiguous_cleanup() {
        let root = std::env::temp_dir().join(format!("ciacola-legacy-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&root).expect("root");
        let repos = Repos {
            root: root.clone(),
            allowed: Arc::new(Vec::new()),
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        };
        let (plugin, ledger) = memory_plugin(repos).await;
        let first_agent = ledger
            .create_agent(&ciacola_core::AgentDef::new("first", "s"), None)
            .await
            .expect("first agent");
        let second_agent = ledger
            .create_agent(&ciacola_core::AgentDef::new("second", "s"), None)
            .await
            .expect("second agent");
        let slug = "acme-a-b-1";
        let branch = format!("agent/{slug}");
        let shared_worktree = root.join(format!("wt-{slug}"));
        let durable = record_assignment(
            &plugin,
            AssignmentFixture {
                agent_id: &first_agent,
                repo: "acme/a-b",
                issue: 1,
                slug,
                branch: &branch,
                worktree: &shared_worktree,
                pr: None,
            },
        )
        .await;
        sqlx::query(
            "UPDATE repo_worker_assignments
             SET state = 'stale', phase = 'seeded_legacy_stale'
             WHERE assignment_id = ?1",
        )
        .bind(&durable.assignment_id)
        .execute(&plugin.ctx.as_ref().expect("context").pool)
        .await
        .expect("seed stale");

        for (agent_id, repo) in [(&first_agent, "acme/a-b"), (&second_agent, "acme-a/b")] {
            let value = json!({
                "repo": repo,
                "issue": 1,
                "slug": slug,
                "branch": branch,
                "worktree": shared_worktree.display().to_string(),
                "pr": null,
            });
            sqlx::query(
                "INSERT INTO plugin_kv (plugin, key, value, updated_unix)
                 VALUES ('repo-worker', ?1, ?2, ?3)",
            )
            .bind(format!("agent/{agent_id}"))
            .bind(value.to_string())
            .bind(ciacola_core::now_unix())
            .execute(&plugin.ctx.as_ref().expect("context").pool)
            .await
            .expect("legacy row");
        }

        let assignments = plugin.assignment_db().expect("assignment db");
        assignments
            .import_legacy(&ledger, plugin.repos.as_ref().expect("repos"))
            .await
            .expect("legacy import");
        let imported = assignments.list().await.expect("assignments");
        assert_eq!(imported.len(), 2);
        assert!(
            imported
                .iter()
                .all(|assignment| assignment.state == AssignmentState::Stale)
        );
        assert!(imported.iter().all(|assignment| {
            assignment.related_agent_ids.contains(&first_agent)
                && assignment.related_agent_ids.contains(&second_agent)
        }));
        let second = imported
            .iter()
            .find(|assignment| assignment.repo == "acme-a/b")
            .expect("second assignment");
        let refused = operator_tool(&plugin, "finish_issue")
            .call(json!({"assignment_id": second.assignment_id}))
            .await;
        let refused = serde_json::to_string(&refused).expect("render refusal");
        assert!(refused.contains(&durable.assignment_id), "got: {refused}");
        assert!(refused.contains(&first_agent), "got: {refused}");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn health_degrades_for_durable_and_physical_drift() {
        let root = std::env::temp_dir().join(format!("ciacola-health-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&root).expect("root");
        let repos = Repos {
            root: root.clone(),
            allowed: Arc::new(Vec::new()),
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        };
        let (plugin, ledger) = memory_plugin(repos.clone()).await;
        let agent_id = ledger
            .create_agent(&ciacola_core::AgentDef::new("drifted", "s"), None)
            .await
            .expect("agent");
        assert!(ledger.retire_agent(&agent_id).await.expect("retire"));
        record_assignment(
            &plugin,
            AssignmentFixture {
                agent_id: &agent_id,
                repo: "local/repo",
                issue: 1,
                slug: "missing-active",
                branch: "agent/missing-active",
                worktree: &root.join("wt-missing-active"),
                pr: None,
            },
        )
        .await;
        let policies = BranchPolicies::default();
        let (completed, _) = plugin
            .assignment_db()
            .expect("assignment db")
            .reserve(
                "local/repo",
                2,
                Some("main"),
                &repos,
                policies.for_repo("local/repo"),
                None,
            )
            .await
            .expect("completed reservation");
        std::fs::create_dir_all(&completed.worktree).expect("completed worktree");
        sqlx::query(
            "UPDATE repo_worker_assignments
             SET state = 'completed', phase = 'cleanup_complete'
             WHERE assignment_id = ?1",
        )
        .bind(&completed.assignment_id)
        .execute(&plugin.ctx.as_ref().expect("context").pool)
        .await
        .expect("complete assignment");

        let health = plugin.health().await;
        assert_eq!(health["status"], "degraded");
        assert_eq!(health["missing_active_worktrees"], 1);
        assert_eq!(health["agent_state_drift"], 1);
        assert_eq!(health["completed_with_worktree"], 1);
        assert_eq!(health["orphans"], 1);
        assert_eq!(health["missing_journey_provenance"], 1);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn pre_journey_assignments_upgrade_without_inventing_commit_provenance() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        ciacola_core::plugin::apply_migrations(&pool, "repo-worker", &MIGRATIONS[..1])
            .await
            .expect("legacy assignment migration");
        for (id, repo, state, phase, pr) in [
            (
                "retained",
                "local/retained",
                "retained",
                "retained",
                Some(4_i64),
            ),
            (
                "completed",
                "local/completed",
                "completed",
                "cleanup_complete",
                None,
            ),
        ] {
            sqlx::query(
                "INSERT INTO repo_worker_assignments
                     (assignment_id, repo, issue_number, state, phase, base, slug,
                      branch, worktree, bare_path, pr, created_unix, updated_unix)
                 VALUES (?1, ?2, 1, ?3, ?4, 'main', ?1,
                         'agent/' || ?1, '/tmp/wt-' || ?1, '/tmp/bare-' || ?1,
                         ?5, 1, 1)",
            )
            .bind(id)
            .bind(repo)
            .bind(state)
            .bind(phase)
            .bind(pr)
            .execute(&pool)
            .await
            .expect("legacy row");
        }

        ciacola_core::plugin::apply_migrations(&pool, "repo-worker", MIGRATIONS)
            .await
            .expect("journey migration");
        let rows = AssignmentDb::new(pool).list().await.expect("upgraded rows");
        let retained = rows
            .iter()
            .find(|row| row.assignment_id == "retained")
            .expect("retained row");
        assert_eq!(retained.publication_state, PublicationState::Published);
        assert_eq!(retained.cleanup_state, CleanupState::Retained);
        assert!(retained.base_head.is_none());
        assert!(retained.expected_head.is_none());
        assert!(retained.pr_state.is_none());
        assert_eq!(retained.branch_policy, DEFAULT_BRANCH_TEMPLATE);
        let completed = rows
            .iter()
            .find(|row| row.assignment_id == "completed")
            .expect("completed row");
        assert_eq!(completed.publication_state, PublicationState::Unpublished);
        assert_eq!(completed.cleanup_state, CleanupState::Completed);
        assert_eq!(completed.branch_policy, DEFAULT_BRANCH_TEMPLATE);
    }

    #[tokio::test]
    async fn partially_applied_journey_columns_resume_safely() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        ciacola_core::plugin::apply_migrations(&pool, "repo-worker", &MIGRATIONS[..1])
            .await
            .expect("assignment table");
        // Simulate a process dying after SQLite committed ALTER TABLE but
        // before the migration marker was inserted.
        sqlx::query("ALTER TABLE repo_worker_assignments ADD COLUMN base_head TEXT")
            .execute(&pool)
            .await
            .expect("partial base-head migration");
        sqlx::query("ALTER TABLE repo_worker_assignments ADD COLUMN expected_head TEXT")
            .execute(&pool)
            .await
            .expect("partial expected-head migration");

        ciacola_core::plugin::apply_migrations(&pool, "repo-worker", MIGRATIONS)
            .await
            .expect("resumed journey migrations");
        let columns: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM pragma_table_info('repo_worker_assignments') ORDER BY cid",
        )
        .fetch_all(&pool)
        .await
        .expect("columns");
        let columns: std::collections::HashSet<String> =
            columns.into_iter().map(|(name,)| name).collect();
        for expected in [
            "base_head",
            "expected_head",
            "publication_state",
            "pr_url",
            "pr_state",
            "pr_draft",
            "pr_head",
            "pr_base",
            "pr_checked_unix",
            "cleanup_state",
            "cleanup_head",
            "cleanup_reason",
            "pushed_head",
        ] {
            assert!(columns.contains(expected), "missing column {expected}");
        }
    }

    #[tokio::test]
    async fn direct_legacy_store_import_preserves_known_pr_publication() {
        let root = std::env::temp_dir().join(format!("ciacola-legacy-pr-{}", ulid::Ulid::new()));
        let repos = Repos {
            root: root.clone(),
            allowed: Arc::new(Vec::new()),
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        };
        let (plugin, ledger) = memory_plugin(repos.clone()).await;
        let value = serde_json::to_string(&LegacyAssignment {
            repo: "local/legacy".into(),
            issue: 76,
            slug: "local-legacy-76".into(),
            branch: "agent/local-legacy-76".into(),
            worktree: root.join("wt-local-legacy-76").display().to_string(),
            pr: Some(76),
        })
        .expect("legacy assignment");
        sqlx::query(
            "INSERT INTO plugin_kv (plugin, key, value, updated_unix)
             VALUES ('repo-worker', 'agent/missing-legacy-agent', ?1, 1)",
        )
        .bind(value)
        .execute(&plugin.ctx.as_ref().expect("context").pool)
        .await
        .expect("legacy Store row");
        let assignments = plugin.assignment_db().expect("assignment db");
        assignments
            .import_legacy(&ledger, &repos)
            .await
            .expect("legacy import");
        let imported = assignments.list().await.expect("assignments");
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].pr, Some(76));
        assert_eq!(imported[0].publication_state, PublicationState::Published);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn restart_preserves_publication_and_cleanup_recovery_fences() {
        let (tmp, repos) = local_repos("journey-restart").await;
        let (plugin, ledger) = memory_plugin(repos.clone()).await;
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 84, "base": "main"}))
            .await;
        let active = plugin.assignments().await.expect("assignments")[0].clone();
        let approved = active.base_head.clone().expect("base head");
        let assignments = plugin.assignment_db().expect("assignment db");
        assignments
            .begin_publication(&active.assignment_id, &approved)
            .await
            .expect("begin publication");
        let policies = BranchPolicies::default();
        let (finishing, _) = assignments
            .reserve(
                "local/repo",
                85,
                Some("main"),
                &repos,
                policies.for_repo("local/repo"),
                None,
            )
            .await
            .expect("cleanup reservation");
        sqlx::query(
            "UPDATE repo_worker_assignments
             SET state = 'finishing', phase = 'removing_branch',
                 cleanup_state = 'removing', cleanup_head = 'deadbeef',
                 cleanup_reason = 'discarded'
             WHERE assignment_id = ?1",
        )
        .bind(&finishing.assignment_id)
        .execute(&plugin.ctx.as_ref().expect("context").pool)
        .await
        .expect("seed interrupted cleanup");

        assignments
            .reconcile_on_start(&ledger, &repos)
            .await
            .expect("reconcile");
        let active = assignments
            .get_by_id(&active.assignment_id)
            .await
            .expect("active query")
            .expect("active row");
        assert_eq!(active.state, AssignmentState::Active);
        assert_eq!(active.publication_state, PublicationState::Failed);
        assert_eq!(active.expected_head.as_deref(), Some(approved.as_str()));
        let finishing = assignments
            .get_by_id(&finishing.assignment_id)
            .await
            .expect("cleanup query")
            .expect("cleanup row");
        assert_eq!(finishing.state, AssignmentState::Stale);
        assert_eq!(finishing.cleanup_state, CleanupState::Failed);
        assert_eq!(finishing.cleanup_head.as_deref(), Some("deadbeef"));
        assert_eq!(finishing.cleanup_reason, Some(CleanupReason::Discarded));
        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test]
    async fn invalid_pr_title_is_refused_before_assignment_or_git_work() {
        let root = std::env::temp_dir().join(format!("ciacola-title-{}", ulid::Ulid::new()));
        let repos = Repos {
            root: root.clone(),
            allowed: Arc::new(Vec::new()),
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn publication_pushes_the_approved_oid_and_records_one_pr_journey() {
        let (tmp, repos) = local_repos("publish-approved").await;
        let (mut plugin, _ledger) = memory_plugin_with_branch_template(repos, "fix/{slug}").await;
        let fake = FakeGh::new(&tmp);
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        git(&tmp.join("origin"), &["branch", "develop"]).await;

        let started = operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 76, "base": "develop"}))
            .await;
        let started = serde_json::to_string(&started).expect("render start");
        assert!(started.contains("\"created\":true"), "got: {started}");
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        assert!(assignment.branch.starts_with("fix/"));
        assert_eq!(assignment.branch_policy, "fix/{slug}");
        assert_eq!(
            git_output(
                Path::new(&assignment.worktree),
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
            )
            .await
            .expect("worktree branch"),
            assignment.branch
        );
        let state = operator_tool(&plugin, "worktrees").call(json!({})).await;
        let state = serde_json::to_string(&state).expect("render worktree state");
        assert!(state.contains("\"branch_policy\":\"fix/{slug}\""));
        assert!(state.contains("\"local/repo\":\"fix/{slug}\""));
        let board = plugin.board_section().await.expect("board section");
        assert!(board.html.contains("fix/{slug}"));
        let worktree = Path::new(&assignment.worktree);
        let head = commit_change(worktree, "change", "published").await;
        git(
            worktree,
            &["tag", "-a", "secret-tag", "-m", "not approved", &head],
        )
        .await;
        git(worktree, &["config", "push.followTags", "true"]).await;
        git(worktree, &["config", "push.recurseSubmodules", "on-demand"]).await;
        let pre_push = Path::new(&assignment.bare_path).join("hooks/pre-push");
        std::fs::write(&pre_push, "#!/bin/sh\nexit 1\n").expect("write rejecting pre-push hook");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pre_push, std::fs::Permissions::from_mode(0o700))
            .expect("make pre-push executable");
        let pr = fake.pr(41, "OPEN", true, &assignment.branch, &head, "develop");
        fake.set_created(&pr);

        let args = json!({
            "agent_id": assignment.agent_id,
            "title": "fix: publish exact head",
            "body": "Closes #76"
        });
        let opened = operator_tool(&plugin, "open_pr").call(args.clone()).await;
        let opened = serde_json::to_string(&opened).expect("render open");
        assert!(opened.contains("\"created\":true"), "got: {opened}");
        assert!(opened.contains("\"pr_state\":\"open\""), "got: {opened}");

        let remote = git_output(
            &tmp.join("origin"),
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{}^{{commit}}", assignment.branch),
            ],
        )
        .await
        .expect("remote branch");
        assert_eq!(remote, head);
        assert!(
            !git_predicate(
                &tmp.join("origin"),
                &["show-ref", "--verify", "--quiet", "refs/tags/secret-tag"],
            )
            .await
            .expect("inspect remote tag"),
            "publication must not follow reachable tags from ambient Git config"
        );
        let durable = plugin.assignments().await.expect("assignments")[0].clone();
        assert_eq!(durable.expected_head.as_deref(), Some(head.as_str()));
        assert_eq!(durable.pushed_head.as_deref(), Some(head.as_str()));
        assert_eq!(durable.pr, Some(41));
        assert_eq!(durable.pr_state, Some(PrState::Open));
        assert_eq!(durable.pr_head.as_deref(), Some(head.as_str()));
        assert_eq!(durable.pr_base.as_deref(), Some("develop"));
        assert!(durable.pr_checked_unix.is_some());
        let log = fake.log();
        assert!(log.contains("pr create"), "gh log: {log}");
        assert!(
            log.contains("--repo github.com/local/repo"),
            "gh host must be explicit: {log}"
        );
        assert!(log.contains("--base develop"), "gh log: {log}");

        std::fs::write(Path::new(&assignment.worktree).join("later-dirty"), "dirty")
            .expect("dirty post-publication worktree");
        let replay = operator_tool(&plugin, "open_pr").call(args).await;
        let replay = serde_json::to_string(&replay).expect("render replay");
        assert!(replay.contains("\"created\":false"), "got: {replay}");
        assert_eq!(fake.log().matches("pr create").count(), 1);
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cleanup_deletes_the_exact_persisted_configured_branch() {
        let (tmp, repos) = local_repos("configured-branch-cleanup").await;
        let (mut plugin, _ledger) = memory_plugin_with_branch_template(repos, "fix/{slug}").await;
        let fake = FakeGh::new(&tmp);
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary;
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 77, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        assert!(assignment.branch.starts_with("fix/"));
        let head = commit_change(Path::new(&assignment.worktree), "change", "discarded").await;

        let finished = operator_tool(&plugin, "finish_issue")
            .call(json!({
                "agent_id": assignment.agent_id,
                "discard_head": head,
            }))
            .await;
        let finished = serde_json::to_string(&finished).expect("render finish");
        assert!(
            finished.contains("\"state\":\"completed\""),
            "got: {finished}"
        );
        assert!(!Path::new(&assignment.worktree).exists());
        assert!(
            !git_predicate(
                Path::new(&assignment.bare_path),
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", assignment.branch),
                ],
            )
            .await
            .expect("inspect configured branch"),
            "cleanup must delete the exact persisted configured branch"
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_open_pr_update_retries_from_the_previous_remote_fence() {
        let (tmp, repos) = local_repos("pr-update-retry").await;
        let (mut plugin, _ledger) = memory_plugin(repos).await;
        let fake = FakeGh::new(&tmp);
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 92, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let worktree = Path::new(&assignment.worktree);
        let first = commit_change(worktree, "first", "one").await;
        let first_pr = fake.pr(47, "OPEN", false, &assignment.branch, &first, "main");
        fake.set_created(&first_pr);
        let args = |head: &str| {
            json!({
                "agent_id": assignment.agent_id,
                "expected_head": head,
                "title": "fix: retry exact PR updates",
                "body": "Closes #92"
            })
        };
        let opened = operator_tool(&plugin, "open_pr").call(args(&first)).await;
        assert!(
            serde_json::to_string(&opened)
                .expect("render first publication")
                .contains("\"pr_state\":\"open\"")
        );

        let second = commit_change(worktree, "second", "two").await;
        let lock = tmp
            .join("origin/.git/refs/heads")
            .join(format!("{}.lock", assignment.branch));
        std::fs::create_dir_all(lock.parent().expect("remote lock parent"))
            .expect("remote lock parent");
        std::fs::write(&lock, "locked").expect("remote ref lock");
        let failed = operator_tool(&plugin, "open_pr").call(args(&second)).await;
        let failed = serde_json::to_string(&failed).expect("render failed update");
        assert!(failed.contains("cannot lock ref"), "got: {failed}");
        let durable = plugin.assignments().await.expect("assignments")[0].clone();
        assert_eq!(durable.expected_head.as_deref(), Some(second.as_str()));
        assert_eq!(durable.pushed_head.as_deref(), Some(first.as_str()));
        assert_eq!(durable.pr_head.as_deref(), Some(first.as_str()));
        assert_eq!(durable.publication_state, PublicationState::Failed);

        std::fs::remove_file(&lock).expect("remove remote ref lock");
        let advanced = operator_tool(&plugin, "open_pr").call(args(&second)).await;
        let advanced = serde_json::to_string(&advanced).expect("render advanced update");
        assert!(advanced.contains("still reports head"), "got: {advanced}");
        let durable = plugin.assignments().await.expect("assignments")[0].clone();
        assert_eq!(durable.pushed_head.as_deref(), Some(second.as_str()));
        assert_eq!(durable.publication_state, PublicationState::Failed);

        let second_pr = fake.pr(47, "OPEN", false, &assignment.branch, &second, "main");
        fake.set_existing(Some(&second_pr));
        let reconciled = operator_tool(&plugin, "open_pr").call(args(&second)).await;
        let reconciled = serde_json::to_string(&reconciled).expect("render reconciled update");
        assert!(
            reconciled.contains("\"created\":false"),
            "got: {reconciled}"
        );
        let durable = plugin.assignments().await.expect("assignments")[0].clone();
        assert_eq!(durable.publication_state, PublicationState::Published);
        assert_eq!(durable.expected_head.as_deref(), Some(second.as_str()));
        assert_eq!(durable.pushed_head.as_deref(), Some(second.as_str()));
        assert_eq!(durable.pr_head.as_deref(), Some(second.as_str()));
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn publication_preflight_refuses_zero_dirty_moved_and_wrong_branch_work() {
        let (tmp, repos) = local_repos("publish-preflight").await;
        let (mut plugin, _ledger) = memory_plugin(repos).await;
        let fake = FakeGh::new(&tmp);
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 77, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let worktree = Path::new(&assignment.worktree);
        let open = |head: &str| {
            json!({
                "agent_id": assignment.agent_id,
                "expected_head": head,
                "title": "fix: guarded publication",
                "body": "Closes #77"
            })
        };

        let zero = operator_tool(&plugin, "open_pr")
            .call(open(assignment.base_head.as_deref().expect("base head")))
            .await;
        assert!(
            serde_json::to_string(&zero)
                .expect("render zero")
                .contains("no committed material delta")
        );

        let first = commit_change(worktree, "first", "one").await;
        std::fs::write(worktree.join("untracked"), "dirty").expect("dirty file");
        let dirty = operator_tool(&plugin, "open_pr").call(open(&first)).await;
        assert!(
            serde_json::to_string(&dirty)
                .expect("render dirty")
                .contains("worktree is dirty")
        );
        std::fs::remove_file(worktree.join("untracked")).expect("clean fixture");

        let second = commit_change(worktree, "second", "two").await;
        let moved = operator_tool(&plugin, "open_pr").call(open(&first)).await;
        assert!(
            serde_json::to_string(&moved)
                .expect("render moved")
                .contains("assigned branch moved")
        );
        git(worktree, &["switch", "-qc", "wrong-branch"]).await;
        let wrong = operator_tool(&plugin, "open_pr").call(open(&second)).await;
        assert!(
            serde_json::to_string(&wrong)
                .expect("render wrong")
                .contains("expected")
        );
        assert!(!fake.log().contains("pr create"));
        assert!(
            !git_predicate(
                &tmp.join("origin"),
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", assignment.branch),
                ],
            )
            .await
            .expect("remote ref predicate")
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn github_lookup_failure_cannot_fall_through_to_push_or_create() {
        let (tmp, repos) = local_repos("gh-query-failure").await;
        let (mut plugin, _ledger) = memory_plugin(repos).await;
        let fake = FakeGh::new(&tmp);
        fake.fail_list();
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 81, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let head = commit_change(Path::new(&assignment.worktree), "change", "query").await;
        let out = operator_tool(&plugin, "open_pr")
            .call(json!({
                "agent_id": assignment.agent_id,
                "expected_head": head,
                "title": "fix: fail closed on lookup",
                "body": "Closes #81"
            }))
            .await;
        let out = serde_json::to_string(&out).expect("render failure");
        assert!(out.contains("injected list failure"), "got: {out}");
        assert!(!fake.log().contains("pr create"));
        assert!(
            !git_predicate(
                &tmp.join("origin"),
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", assignment.branch),
                ],
            )
            .await
            .expect("remote ref predicate")
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_push_reconciliation_failure_settles_publication_as_failed() {
        let (tmp, repos) = local_repos("post-push-reconciliation").await;
        let (mut plugin, _ledger) = memory_plugin(repos).await;
        let fake = FakeGh::new(&tmp);
        fake.set_created_list_raw("not json");
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 84, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let head = commit_change(Path::new(&assignment.worktree), "change", "reconcile").await;

        let out = operator_tool(&plugin, "open_pr")
            .call(json!({
                "agent_id": assignment.agent_id,
                "expected_head": head,
                "title": "fix: settle reconciliation failures",
                "body": "Closes #84"
            }))
            .await;
        let out = serde_json::to_string(&out).expect("render reconciliation failure");
        assert!(out.contains("cannot parse pull request list"), "got: {out}");
        let durable = plugin.assignments().await.expect("assignments")[0].clone();
        assert_eq!(durable.publication_state, PublicationState::Failed);
        assert_eq!(durable.expected_head.as_deref(), Some(head.as_str()));
        assert!(
            durable
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("cannot parse pull request list"))
        );
        assert_eq!(plugin.health().await["status"], "degraded");
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn publication_refuses_an_unrecognized_remote_branch_head() {
        let (tmp, repos) = local_repos("remote-head-drift").await;
        let (mut plugin, _ledger) = memory_plugin(repos).await;
        let fake = FakeGh::new(&tmp);
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 82, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let head = commit_change(Path::new(&assignment.worktree), "change", "local").await;
        git(&tmp.join("origin"), &["branch", &assignment.branch, "main"]).await;

        let out = operator_tool(&plugin, "open_pr")
            .call(json!({
                "agent_id": assignment.agent_id,
                "expected_head": head,
                "title": "fix: refuse remote drift",
                "body": "Closes #82"
            }))
            .await;
        let out = serde_json::to_string(&out).expect("render drift");
        assert!(out.contains("remote branch moved"), "got: {out}");
        let remote = git_output(
            &tmp.join("origin"),
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{}^{{commit}}", assignment.branch),
            ],
        )
        .await
        .expect("remote head");
        assert_eq!(remote, assignment.base_head.expect("base head"));
        assert!(!fake.log().contains("pr create"));
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn publication_rejects_multiple_push_destinations() {
        let (tmp, repos) = local_repos("multiple-pushurls").await;
        let (mut plugin, _ledger) = memory_plugin(repos).await;
        let fake = FakeGh::new(&tmp);
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 86, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let worktree = Path::new(&assignment.worktree);
        let head = commit_change(worktree, "change", "push urls").await;
        let unintended = tmp.join("unintended.git");
        std::fs::create_dir_all(&unintended).expect("unintended dir");
        git(&unintended, &["init", "--bare", "-q"]).await;
        git(
            worktree,
            &[
                "config",
                "--add",
                "remote.origin.pushurl",
                "https://github.com/local/repo.git",
            ],
        )
        .await;
        git(
            worktree,
            &[
                "config",
                "--add",
                "remote.origin.pushurl",
                &format!("file://{}", unintended.display()),
            ],
        )
        .await;

        let refused = operator_tool(&plugin, "open_pr")
            .call(json!({
                "agent_id": assignment.agent_id,
                "expected_head": head,
                "title": "fix: reject extra push destinations",
                "body": "Closes #86"
            }))
            .await;
        let refused = serde_json::to_string(&refused).expect("render push URL refusal");
        assert!(
            refused.contains("expected only GitHub repository"),
            "got: {refused}"
        );
        assert!(!fake.log().contains("pr create"));
        assert!(
            !git_predicate(
                &unintended,
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", assignment.branch),
                ],
            )
            .await
            .expect("unintended ref predicate")
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn publication_rejects_a_chained_push_url_rewrite() {
        let (tmp, repos) = local_repos("chained-push-rewrite").await;
        let (mut plugin, _ledger) = memory_plugin(repos).await;
        let fake = FakeGh::new(&tmp);
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 96, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let worktree = Path::new(&assignment.worktree);
        let head = commit_change(worktree, "change", "rewrite").await;
        let origin_url = format!("file://{}", tmp.join("origin").display());
        git(
            worktree,
            &[
                "config",
                "--unset-all",
                &format!("url.{origin_url}.insteadOf"),
            ],
        )
        .await;
        let unintended = tmp.join("rewrite-target.git");
        std::fs::create_dir_all(&unintended).expect("unintended dir");
        git(&unintended, &["init", "--bare", "-q"]).await;
        git(worktree, &["config", "remote.origin.url", "alias://repo"]).await;
        git(
            worktree,
            &[
                "config",
                "url.https://github.com/local/repo.git.insteadOf",
                "alias://repo",
            ],
        )
        .await;
        git(
            worktree,
            &[
                "config",
                &format!("url.file://{}.pushInsteadOf", unintended.display()),
                "https://github.com/local/repo.git",
            ],
        )
        .await;

        let refused = operator_tool(&plugin, "open_pr")
            .call(json!({
                "agent_id": assignment.agent_id,
                "expected_head": head,
                "title": "fix: reject chained URL rewrites",
                "body": "Closes #96"
            }))
            .await;
        let refused = serde_json::to_string(&refused).expect("render rewrite refusal");
        assert!(refused.contains("unstable remote target"), "got: {refused}");
        assert!(!fake.log().contains("pr create"));
        assert!(
            !git_predicate(
                &unintended,
                &[
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{}", assignment.branch),
                ],
            )
            .await
            .expect("unintended ref predicate")
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exact_push_lease_closes_remote_check_race() {
        let (tmp, repos) = local_repos("push-lease-race").await;
        let (plugin, _ledger) = memory_plugin(repos.clone()).await;
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 87, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let head = commit_change(Path::new(&assignment.worktree), "change", "lease").await;
        let snapshot = repos
            .inspect_assignment_worktree(&assignment)
            .await
            .expect("snapshot");

        // Simulate another actor creating the branch after our absent-ref
        // observation but before push. The explicit empty lease must refuse
        // even though an ordinary non-force push would fast-forward it.
        git(&tmp.join("origin"), &["branch", &assignment.branch, "main"]).await;
        let error = repos
            .push_exact(&assignment, &head, None, &snapshot.push_url)
            .await
            .expect_err("stale absent-ref lease must refuse");
        assert!(error.to_string().contains("stale info"), "got: {error}");
        let remote = git_output(
            &tmp.join("origin"),
            &[
                "rev-parse",
                "--verify",
                &format!("refs/heads/{}^{{commit}}", assignment.branch),
            ],
        )
        .await
        .expect("remote head");
        assert_eq!(remote, assignment.base_head.expect("base head"));
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pull_request_base_drift_is_observed_but_never_accepted() {
        let (tmp, repos) = local_repos("pr-base-drift").await;
        let (mut plugin, _ledger) = memory_plugin(repos).await;
        let fake = FakeGh::new(&tmp);
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 83, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let head = commit_change(Path::new(&assignment.worktree), "change", "base drift").await;
        let drifted = fake.pr(44, "OPEN", false, &assignment.branch, &head, "develop");
        fake.set_existing(Some(&drifted));

        let out = operator_tool(&plugin, "open_pr")
            .call(json!({
                "agent_id": assignment.agent_id,
                "expected_head": head,
                "title": "fix: reject base drift",
                "body": "Closes #83"
            }))
            .await;
        let out = serde_json::to_string(&out).expect("render drift");
        assert!(out.contains("targets base 'develop'"), "got: {out}");
        let durable = plugin.assignments().await.expect("assignments")[0].clone();
        assert_eq!(durable.pr, Some(44));
        assert_eq!(durable.pr_state, Some(PrState::Open));
        assert_eq!(durable.pr_base.as_deref(), Some("develop"));
        assert_eq!(durable.publication_state, PublicationState::Failed);
        assert!(
            durable
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("targets base 'develop'"))
        );
        assert_eq!(plugin.health().await["status"], "degraded");
        assert!(!fake.log().contains("pr create"));

        // An exact discard is a local destruction authorization, so cleanup
        // must remain possible even when the mismatched PR can no longer be
        // queried or validated.
        std::fs::remove_file(&fake.view).ok();
        let finished = operator_tool(&plugin, "finish_issue")
            .call(json!({
                "agent_id": assignment.agent_id,
                "discard_head": head,
            }))
            .await;
        let finished = serde_json::to_string(&finished).expect("render discard");
        assert!(
            finished.contains("\"state\":\"completed\""),
            "got: {finished}"
        );
        assert!(
            finished.contains("\"cleanup_reason\":\"discarded\""),
            "got: {finished}"
        );
        assert_eq!(plugin.health().await["status"], "ok");
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pull_request_head_drift_is_durable_and_degraded() {
        let (tmp, repos) = local_repos("pr-head-drift").await;
        let (mut plugin, _ledger) = memory_plugin(repos).await;
        let fake = FakeGh::new(&tmp);
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 85, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let reviewed = commit_change(Path::new(&assignment.worktree), "change", "reviewed").await;
        let original = fake.pr(45, "OPEN", false, &assignment.branch, &reviewed, "main");
        fake.set_existing(Some(&original));
        let opened = operator_tool(&plugin, "open_pr")
            .call(json!({
                "agent_id": assignment.agent_id,
                "expected_head": reviewed,
                "title": "fix: publish reviewed head",
                "body": "Closes #85"
            }))
            .await;
        assert!(
            serde_json::to_string(&opened)
                .expect("render open")
                .contains("\"pr_state\":\"open\"")
        );

        let drifted_head = assignment.base_head.as_deref().expect("base head");
        let drifted = fake.pr(45, "OPEN", false, &assignment.branch, drifted_head, "main");
        fake.set_existing(Some(&drifted));
        let refused = operator_tool(&plugin, "open_pr")
            .call(json!({
                "agent_id": assignment.agent_id,
                "expected_head": reviewed,
                "title": "fix: publish reviewed head",
                "body": "Closes #85"
            }))
            .await;
        let refused = serde_json::to_string(&refused).expect("render head drift");
        assert!(
            refused.contains("drifted from durable expected head"),
            "got: {refused}"
        );
        let durable = plugin.assignments().await.expect("assignments")[0].clone();
        assert_eq!(durable.expected_head.as_deref(), Some(reviewed.as_str()));
        assert_eq!(durable.pr_head.as_deref(), Some(drifted_head));
        assert_eq!(durable.publication_state, PublicationState::Failed);
        assert_eq!(plugin.health().await["status"], "degraded");
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_pr_observation_is_one_atomic_database_write() {
        let (tmp, repos) = local_repos("pr-observation-atomic").await;
        let (mut plugin, _ledger) = memory_plugin(repos).await;
        let fake = FakeGh::new(&tmp);
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 91, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let head = commit_change(Path::new(&assignment.worktree), "change", "atomic").await;
        let drifted = fake.pr(46, "OPEN", false, &assignment.branch, &head, "develop");
        fake.set_existing(Some(&drifted));
        sqlx::query(
            "CREATE TRIGGER fail_pr_observation
             BEFORE UPDATE OF pr ON repo_worker_assignments
             BEGIN SELECT RAISE(ABORT, 'injected observation failure'); END",
        )
        .execute(&plugin.ctx.as_ref().expect("context").pool)
        .await
        .expect("observation failure trigger");

        let refused = operator_tool(&plugin, "open_pr")
            .call(json!({
                "agent_id": assignment.agent_id,
                "expected_head": head,
                "title": "fix: keep observation atomic",
                "body": "Closes #91"
            }))
            .await;
        let refused = serde_json::to_string(&refused).expect("render trigger failure");
        assert!(
            refused.contains("injected observation failure"),
            "got: {refused}"
        );
        let durable = plugin.assignments().await.expect("assignments")[0].clone();
        assert_eq!(durable.pr, None);
        assert_eq!(durable.pr_base, None);
        assert_eq!(durable.pr_head, None);
        assert_eq!(durable.publication_state, PublicationState::Unpublished);
        assert_eq!(durable.last_error, None);
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn closed_and_merged_prs_are_distinct_and_merged_cleanup_is_safe() {
        let (tmp, repos) = local_repos("pr-states").await;
        let (mut plugin, ledger) = memory_plugin(repos).await;
        let fake = FakeGh::new(&tmp);
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 78, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let head = commit_change(Path::new(&assignment.worktree), "change", "states").await;
        let closed = fake.pr(42, "CLOSED", false, &assignment.branch, &head, "main");
        fake.set_existing(Some(&closed));
        let args = json!({
            "agent_id": assignment.agent_id,
            "expected_head": head,
            "title": "fix: reconcile state",
            "body": "Closes #78"
        });
        let observed = operator_tool(&plugin, "open_pr").call(args.clone()).await;
        let observed = serde_json::to_string(&observed).expect("render closed");
        assert!(
            observed.contains("\"pr_state\":\"closed\""),
            "got: {observed}"
        );

        let merged = fake.pr(42, "MERGED", false, &assignment.branch, &head, "main");
        fake.set_existing(Some(&merged));
        let observed = operator_tool(&plugin, "open_pr").call(args).await;
        let observed = serde_json::to_string(&observed).expect("render merged");
        assert!(
            observed.contains("\"pr_state\":\"merged\""),
            "got: {observed}"
        );

        let finished = operator_tool(&plugin, "finish_issue")
            .call(json!({"agent_id": assignment.agent_id}))
            .await;
        let finished = serde_json::to_string(&finished).expect("render finish");
        assert!(
            finished.contains("\"state\":\"completed\""),
            "got: {finished}"
        );
        assert!(
            finished.contains("\"cleanup_reason\":\"merged\""),
            "got: {finished}"
        );
        assert!(!Path::new(&assignment.worktree).exists());
        assert!(
            ledger
                .get_agent(assignment.agent_id.as_deref().expect("agent"))
                .await
                .expect("agent query")
                .expect("agent row")
                .retired
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn retained_work_can_be_published_without_unretiring_its_agent() {
        let (tmp, repos) = local_repos("retained-publish").await;
        let (mut plugin, ledger) = memory_plugin(repos).await;
        let fake = FakeGh::new(&tmp);
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 79, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let head = commit_change(Path::new(&assignment.worktree), "change", "retained").await;
        let retained = operator_tool(&plugin, "finish_issue")
            .call(json!({"agent_id": assignment.agent_id, "keep": true}))
            .await;
        assert!(
            serde_json::to_string(&retained)
                .expect("render retain")
                .contains("\"state\":\"retained\"")
        );
        let pr = fake.pr(43, "OPEN", true, &assignment.branch, &head, "main");
        fake.set_created(&pr);
        let opened = operator_tool(&plugin, "open_pr")
            .call(json!({
                "agent_id": assignment.agent_id,
                "expected_head": head,
                "title": "fix: publish retained work",
                "body": "Closes #79"
            }))
            .await;
        let opened = serde_json::to_string(&opened).expect("render open");
        assert!(opened.contains("\"created\":true"), "got: {opened}");
        assert!(
            ledger
                .get_agent(assignment.agent_id.as_deref().expect("agent"))
                .await
                .expect("agent query")
                .expect("agent row")
                .retired,
            "publication must not reactivate retained work"
        );
        assert_eq!(
            plugin.assignments().await.expect("assignments")[0].state,
            AssignmentState::Retained
        );
        let merged = fake.pr(43, "MERGED", false, &assignment.branch, &head, "main");
        fake.set_existing(Some(&merged));
        let cleaned = operator_tool(&plugin, "finish_issue")
            .call(json!({"assignment_id": assignment.assignment_id}))
            .await;
        let cleaned = serde_json::to_string(&cleaned).expect("render retained cleanup");
        assert!(
            cleaned.contains("\"state\":\"completed\""),
            "got: {cleaned}"
        );
        assert!(
            cleaned.contains("\"cleanup_reason\":\"merged\""),
            "got: {cleaned}"
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exact_discard_survives_branch_delete_failure_and_retries() {
        let (tmp, repos) = local_repos("discard-retry").await;
        let (mut plugin, ledger) = memory_plugin(repos).await;
        let fake = FakeGh::new(&tmp);
        plugin.repos.as_mut().expect("repos").gh_binary = fake.binary.clone();
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 80, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let head = commit_change(Path::new(&assignment.worktree), "change", "discard").await;

        let refused = operator_tool(&plugin, "finish_issue")
            .call(json!({"agent_id": assignment.agent_id}))
            .await;
        let refused = serde_json::to_string(&refused).expect("render refusal");
        assert!(refused.contains(&head), "got: {refused}");
        assert!(refused.contains("discard_head"), "got: {refused}");
        std::fs::write(Path::new(&assignment.worktree).join("dirty"), "dirty")
            .expect("dirty fixture");
        let dirty = operator_tool(&plugin, "finish_issue")
            .call(json!({"agent_id": assignment.agent_id, "discard_head": head}))
            .await;
        assert!(
            serde_json::to_string(&dirty)
                .expect("render dirty")
                .contains("worktree is dirty")
        );
        std::fs::remove_file(Path::new(&assignment.worktree).join("dirty")).expect("clean fixture");

        let lock = Path::new(&assignment.bare_path)
            .join("refs/heads")
            .join(format!("{}.lock", assignment.branch));
        std::fs::create_dir_all(lock.parent().expect("lock parent")).expect("lock parent");
        std::fs::write(&lock, "locked").expect("lock ref");
        let failed = operator_tool(&plugin, "finish_issue")
            .call(json!({"agent_id": assignment.agent_id, "discard_head": head}))
            .await;
        let failed = serde_json::to_string(&failed).expect("render failed cleanup");
        assert!(failed.contains("cleanup failed"), "got: {failed}");
        assert!(!Path::new(&assignment.worktree).exists());
        let durable = plugin.assignments().await.expect("assignments")[0].clone();
        assert_eq!(durable.state, AssignmentState::Stale);
        assert_eq!(durable.cleanup_state, CleanupState::Failed);
        assert_eq!(durable.cleanup_head.as_deref(), Some(head.as_str()));
        assert_eq!(durable.cleanup_reason, Some(CleanupReason::Discarded));
        assert!(
            ledger
                .get_agent(assignment.agent_id.as_deref().expect("agent"))
                .await
                .expect("agent query")
                .expect("agent row")
                .retired
        );

        std::fs::remove_file(lock).expect("unlock ref");
        let retried = operator_tool(&plugin, "finish_issue")
            .call(json!({"assignment_id": assignment.assignment_id}))
            .await;
        let retried = serde_json::to_string(&retried).expect("render retry");
        assert!(
            retried.contains("\"state\":\"completed\""),
            "got: {retried}"
        );
        assert!(
            retried.contains("\"cleanup_reason\":\"discarded\""),
            "got: {retried}"
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_cleanup_can_be_reauthorized_at_a_moved_branch_head() {
        let (tmp, repos) = local_repos("discard-reauthorize").await;
        let (plugin, _ledger) = memory_plugin(repos).await;
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 89, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let head = commit_change(Path::new(&assignment.worktree), "change", "discard").await;
        let base = assignment.base_head.as_deref().expect("base head");
        let lock = Path::new(&assignment.bare_path)
            .join("refs/heads")
            .join(format!("{}.lock", assignment.branch));
        std::fs::create_dir_all(lock.parent().expect("lock parent")).expect("lock parent");
        std::fs::write(&lock, "locked").expect("lock ref");
        let failed = operator_tool(&plugin, "finish_issue")
            .call(json!({"agent_id": assignment.agent_id, "discard_head": head}))
            .await;
        assert!(
            serde_json::to_string(&failed)
                .expect("render failed cleanup")
                .contains("cleanup failed")
        );
        std::fs::remove_file(lock).expect("unlock ref");
        git_output(
            Path::new(&assignment.bare_path),
            &[
                "update-ref",
                &format!("refs/heads/{}", assignment.branch),
                base,
                &head,
            ],
        )
        .await
        .expect("move branch after failed authorization");

        let retried = operator_tool(&plugin, "finish_issue")
            .call(json!({
                "assignment_id": assignment.assignment_id,
                "discard_head": base,
            }))
            .await;
        let retried = serde_json::to_string(&retried).expect("render reauthorized cleanup");
        assert!(
            retried.contains("\"state\":\"completed\""),
            "got: {retried}"
        );
        assert!(
            retried.contains("\"cleanup_head\":") && retried.contains(base),
            "got: {retried}"
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_process_cleanup_reauthorization_replaces_the_durable_fence() {
        let (tmp, repos) = local_repos("discard-reauthorize-finishing").await;
        let (plugin, _ledger) = memory_plugin(repos).await;
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 90, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        let first = commit_change(Path::new(&assignment.worktree), "first", "one").await;
        let assignments = plugin.assignment_db().expect("assignment db");
        assert!(
            assignments
                .begin_finish(
                    &assignment,
                    false,
                    Some(&first),
                    Some(CleanupReason::Discarded),
                )
                .await
                .expect("persist first cleanup intent")
        );
        // This models an external branch update after intent was stored and
        // the operator request was cancelled, but before retirement/removal.
        let second = commit_change(Path::new(&assignment.worktree), "second", "two").await;

        let finished = operator_tool(&plugin, "finish_issue")
            .call(json!({
                "assignment_id": assignment.assignment_id,
                "discard_head": second,
            }))
            .await;
        let finished = serde_json::to_string(&finished).expect("render reauthorized finish");
        assert!(
            finished.contains("\"state\":\"completed\""),
            "got: {finished}"
        );
        let durable = plugin.assignments().await.expect("assignments")[0].clone();
        assert_eq!(durable.cleanup_head.as_deref(), Some(second.as_str()));
        assert_eq!(durable.cleanup_reason, Some(CleanupReason::Discarded));
        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test]
    async fn branch_cleanup_deletes_the_assignment_symref_not_its_target() {
        let (tmp, repos) = local_repos("cleanup-symref").await;
        let (worktree, branch) = repos
            .add_worktree(
                "local/repo",
                "cleanup-symref",
                "main",
                "agent/cleanup-symref",
            )
            .await
            .expect("worktree");
        let bare = repos.bare("local/repo");
        bare_repo(&bare)
            .worktree(WorktreeCommand::remove(&worktree))
            .execute()
            .await
            .expect("remove fixture worktree");
        let main = git_output(
            &bare,
            &["rev-parse", "--verify", "refs/remotes/origin/main^{commit}"],
        )
        .await
        .expect("main commit");
        git_output(&bare, &["update-ref", "refs/heads/main", &main])
            .await
            .expect("local main ref");
        git_output(
            &bare,
            &[
                "symbolic-ref",
                &format!("refs/heads/{branch}"),
                "refs/heads/main",
            ],
        )
        .await
        .expect("replace assignment ref with symref");

        repos
            .remove_worktree_at(&branch, &worktree, &bare, Some(&main))
            .await
            .expect("CAS-delete assignment symref");
        assert_eq!(
            git_output(
                &bare,
                &["rev-parse", "--verify", "refs/heads/main^{commit}"]
            )
            .await
            .expect("main survives"),
            main
        );
        assert!(
            !git_predicate(
                &bare,
                &["symbolic-ref", "--quiet", &format!("refs/heads/{branch}")],
            )
            .await
            .expect("assignment symref predicate")
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test]
    async fn finish_refuses_a_running_agent_before_removing_its_worktree() {
        let (tmp, repos) = local_repos("finish-running").await;
        let slug = "local-repo-42";
        let (worktree, branch) = repos
            .add_worktree("local/repo", slug, "main", &format!("agent/{slug}"))
            .await
            .expect("worktree");
        let (plugin, ledger) = memory_plugin(repos).await;
        let agent_id = ledger
            .create_agent(&ciacola_core::AgentDef::new("worker", "s"), None)
            .await
            .expect("agent");
        let seq = ledger.enqueue_turn(&agent_id, "work").await.expect("turn");
        assert!(ledger.claim_turn(&agent_id, seq).await.expect("claim"));
        record_assignment(
            &plugin,
            AssignmentFixture {
                agent_id: &agent_id,
                repo: "local/repo",
                issue: 42,
                slug,
                branch: &branch,
                worktree: &worktree,
                pr: None,
            },
        )
        .await;

        let out = operator_tool(&plugin, "finish_issue")
            .call(json!({"agent_id": agent_id}))
            .await;
        let rendered = serde_json::to_string(&out).expect("render");

        assert!(rendered.contains("queued or running"), "got: {rendered}");
        assert!(worktree.exists(), "finish removed a live worktree");
        assert!(
            plugin
                .assignment_db()
                .expect("assignment db")
                .get_by_agent(&agent_id)
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
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        };
        let (plugin, ledger) = memory_plugin(repos).await;
        let agent_id = ledger
            .create_agent(&ciacola_core::AgentDef::new("worker", "s"), None)
            .await
            .expect("agent");
        record_assignment(
            &plugin,
            AssignmentFixture {
                agent_id: &agent_id,
                repo: "local/repo",
                issue: 42,
                slug: "local-repo-42",
                branch: "agent/local-repo-42",
                worktree: &worktree,
                pr: Some(44),
            },
        )
        .await;

        let out = operator_tool(&plugin, "finish_issue")
            .call(json!({"agent_id": agent_id, "keep": true}))
            .await;
        let rendered = serde_json::to_string(&out).expect("render");

        assert!(rendered.contains("agent_retired"), "got: {rendered}");
        assert!(ledger.get_agent(&agent_id).await.unwrap().unwrap().retired);
        let assignments = plugin.assignments().await.expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].state, AssignmentState::Retained);
        assert!(worktree.exists(), "keep=true must retain the worktree");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn interrupted_retention_is_retryable_after_agent_query_failure() {
        let (tmp, repos) = local_repos("retain-retry").await;
        let (plugin, ledger) = memory_plugin(repos).await;
        operator_tool(&plugin, "start_issue")
            .call(json!({"repo": "local/repo", "issue": 88, "base": "main"}))
            .await;
        let assignment = plugin.assignments().await.expect("assignments")[0].clone();
        sqlx::query(
            "UPDATE repo_worker_assignments
             SET state = 'stale', phase = 'finish_agent_read',
                 cleanup_state = 'failed', last_error = 'transient ledger read failure'
             WHERE assignment_id = ?1",
        )
        .bind(&assignment.assignment_id)
        .execute(&plugin.ctx.as_ref().expect("context").pool)
        .await
        .expect("interrupted retention fixture");
        plugin
            .assignment_db()
            .expect("assignment db")
            .record_pr_observation(
                &assignment.assignment_id,
                &GhPr {
                    number: 99,
                    url: "https://github.com/local/repo/pull/99".into(),
                    state: "OPEN".into(),
                    is_draft: true,
                    head_ref_name: assignment.branch.clone(),
                    head_ref_oid: "unexpected".into(),
                    base_ref_name: "wrong-base".into(),
                    is_cross_repository: false,
                    merged_at: None,
                },
                Some("observed PR identity drift"),
            )
            .await
            .expect("record PR observation without erasing cleanup recovery");
        let interrupted = plugin.assignments().await.expect("assignments")[0].clone();
        assert_eq!(interrupted.phase, "finish_agent_read");
        assert_eq!(
            interrupted.last_error.as_deref(),
            Some("transient ledger read failure")
        );

        let retained = operator_tool(&plugin, "finish_issue")
            .call(json!({"assignment_id": assignment.assignment_id, "keep": true}))
            .await;
        let retained = serde_json::to_string(&retained).expect("render retained retry");
        assert!(
            retained.contains("\"state\":\"retained\""),
            "got: {retained}"
        );
        assert!(
            ledger
                .get_agent(assignment.agent_id.as_deref().expect("agent"))
                .await
                .expect("agent query")
                .expect("agent")
                .retired
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    #[tokio::test]
    async fn cleanup_refuses_an_invalid_worktree_before_retiring_the_agent() {
        let root = std::env::temp_dir().join(format!("ciacola-finish-retry-{}", ulid::Ulid::new()));
        let worktree = root.join("wt-local-repo-42");
        std::fs::create_dir_all(&worktree).expect("worktree");
        let repos = Repos {
            root: root.clone(),
            allowed: Arc::new(Vec::new()),
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        };
        let (plugin, ledger) = memory_plugin(repos).await;
        let agent_id = ledger
            .create_agent(&ciacola_core::AgentDef::new("worker", "s"), None)
            .await
            .expect("agent");
        record_assignment(
            &plugin,
            AssignmentFixture {
                agent_id: &agent_id,
                repo: "local/repo",
                issue: 42,
                slug: "local-repo-42",
                branch: "agent/local-repo-42",
                worktree: &worktree,
                pr: None,
            },
        )
        .await;

        let finish = operator_tool(&plugin, "finish_issue");
        let failed = finish.call(json!({"agent_id": agent_id})).await;
        let failed = serde_json::to_string(&failed).expect("render");
        assert!(failed.contains("not a git repository"), "got: {failed}");
        assert!(!ledger.get_agent(&agent_id).await.unwrap().unwrap().retired);
        let assignments = plugin.assignments().await.expect("assignments");
        assert_eq!(assignments.len(), 1);
        assert_eq!(assignments[0].state, AssignmentState::Active);
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
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
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

        let url = format!("file://{}", origin.display());
        repos
            .ensure_clone_from("local/repo", &url)
            .await
            .expect("refresh");

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
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        };
        let url = format!("file://{}", origin.display());

        let (worktree, _) = repos
            .add_worktree_from(
                "local/repo",
                "local-repo-1",
                "main",
                "agent/local-repo-1",
                &url,
            )
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
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        };
        std::fs::create_dir_all(&repos.root).expect("mkdir root");
        let bare = repos.bare("local/repo");
        CloneCommand::new(format!("file://{}", origin.display()))
            .bare()
            .directory(&bare)
            .execute()
            .await
            .expect("clone");
        let url = format!("file://{}", origin.display());

        // One unit of work in flight, exactly as a batch has.
        let (_wt, branch) = repos
            .add_worktree_from(
                "local/repo",
                "local-repo-1",
                "main",
                "agent/local-repo-1",
                &url,
            )
            .await
            .expect("worktree");

        // A second `start_issue` refreshes the clone while the first is
        // still working. That is where the branch used to disappear.
        repos
            .ensure_clone_from("local/repo", &url)
            .await
            .expect("refresh");

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
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        };
        std::fs::create_dir_all(&repos.root).expect("mkdir root");
        let bare = repos.bare("local/repo");
        CloneCommand::new(format!("file://{}", origin.display()))
            .bare()
            .directory(&bare)
            .execute()
            .await
            .expect("clone");
        let url = format!("file://{}", origin.display());
        repos
            .add_worktree_from(
                "local/repo",
                "local-repo-1",
                "main",
                "agent/local-repo-1",
                &url,
            )
            .await
            .expect("worktree");

        // The branch that worktree holds is now pushed, so a refspec
        // writing local heads would try to update it and be refused.
        git(&origin, &["config", "receive.denyCurrentBranch", "ignore"]).await;

        let refreshed = repos.ensure_clone_from("local/repo", &url).await;
        std::fs::remove_dir_all(&tmp).ok();
        refreshed.expect("refresh must not be blocked by a live worktree");
    }

    /// Repository names may legitimately end in `.git`; only the transport
    /// URL suffix is optional syntax.
    #[test]
    fn github_repository_names_are_not_transport_suffixes() {
        assert!(github_origin_matches(
            "owner/foo.git",
            "https://github.com/owner/foo.git.git"
        ));
        assert!(!github_origin_matches(
            "owner/foo.git",
            "https://github.com/owner/foo.git"
        ));
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

    #[test]
    fn start_issue_role_argument_contract_is_exact_and_deterministic() {
        let mut role = native_implementer_role();
        role.arguments = vec!["worktree".into(), "repo".into(), "issue".into()];
        validate_start_issue_role_arguments(&role).expect("order does not matter");

        role.arguments = vec!["repo".into(), "worktree".into()];
        assert_eq!(
            validate_start_issue_role_arguments(&role).expect_err("missing issue"),
            "role 'issue-implementer' is incompatible with start_issue: expected exactly \
             arguments [\"repo\", \"issue\", \"worktree\"]; missing [\"issue\"]. \
             start_issue supplies only repo, issue, and worktree"
        );

        role.arguments = vec![
            "repo".into(),
            "issue".into(),
            "worktree".into(),
            "release".into(),
            "repo".into(),
        ];
        assert_eq!(
            validate_start_issue_role_arguments(&role).expect_err("extra and duplicate"),
            "role 'issue-implementer' is incompatible with start_issue: expected exactly \
             arguments [\"repo\", \"issue\", \"worktree\"]; unsupported [\"release\"]; \
             duplicate [\"repo\"]. start_issue supplies only repo, issue, and worktree"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn incompatible_implementer_refuses_before_assignment_github_or_git() {
        let tmp = std::env::temp_dir().join(format!(
            "ciacola-role-argument-preflight-{}",
            ulid::Ulid::new()
        ));
        let fake = FakeGh::new(&tmp);
        let repos = Repos {
            root: tmp.join("repos"),
            allowed: Arc::new(vec!["local/repo".into()]),
            gh_binary: fake.binary.clone(),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        };
        let (mut plugin, ledger) = memory_plugin(repos.clone()).await;
        let mut role = native_implementer_role();
        role.arguments.push("release".into());
        plugin.ctx.as_mut().expect("context").roles = roles_with_implementer(role);

        let start = operator_tool(&plugin, "start_issue");
        let out = start.call(json!({"repo": "local/repo", "issue": 85})).await;
        let rendered = serde_json::to_string(&out).expect("render");

        assert!(
            rendered.contains("unsupported [\\\"release\\\"]"),
            "must explain the incompatible role: {rendered}"
        );
        assert!(fake.log().is_empty(), "GitHub must not run: {}", fake.log());
        assert!(
            !repos.root.exists(),
            "clone, refs, and worktree paths must remain absent"
        );
        assert!(ledger.list_agents().await.expect("agents").is_empty());
        let assignments = AssignmentDb::new(plugin.ctx.as_ref().expect("context").pool.clone())
            .list()
            .await
            .expect("assignments");
        assert!(assignments.is_empty(), "no durable claim may be created");

        std::fs::remove_dir_all(&tmp).ok();
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

    /// The shared role preflight must derive the real parent before applying
    /// depth. Claiming a shallower parent cannot make either creation path
    /// pass, and start_issue must refuse before repository or assignment
    /// state exists.
    #[tokio::test]
    async fn start_issue_and_spawn_role_share_depth_and_trusted_parentage() {
        let root_path = std::env::temp_dir().join(format!("ciacola-depth-{}", ulid::Ulid::new()));
        let repos = Repos {
            root: root_path.clone(),
            allowed: Arc::new(vec!["local/repo".into()]),
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        };
        let (mut plugin, ledger) = memory_plugin(repos).await;
        let ctx = plugin.ctx.as_mut().expect("context");
        ctx.limits.max_spawn_depth = 1;
        let roles = ctx.roles.clone();
        let pool = ctx.pool.clone();
        let implementer = roles.get(ROLE).expect("implementer");

        let root = ledger
            .create_agent(&ciacola_core::AgentDef::new("root", "s"), None)
            .await
            .expect("root");
        let parent = ledger
            .create_agent(
                &ciacola_core::AgentDef::new("manager", "s")
                    .allowed_tools(implementer.allowed_tools.clone()),
                Some(&root),
            )
            .await
            .expect("parent");

        let mut extensions = Extensions::new();
        extensions.insert(ciacola_core::AgentIdentity(parent.clone()));
        let request = RequestContext::new(RequestId::Number(72))
            .with_extensions(Arc::new(extensions.clone()));
        let start = plugin
            .tools(Surface::Agent)
            .into_iter()
            .find(|tool| tool.definition().name == "start_issue")
            .expect("start_issue");
        let start_out = start
            .call_with_context(
                request,
                json!({
                    "repo": "local/repo",
                    "issue": 72,
                    "base": "main",
                    "spawned_by": root,
                }),
            )
            .await;
        let start_rendered = serde_json::to_string(&start_out).expect("render start_issue");

        let spawn_role = ciacola_core::roles::tools_with_depth(roles, ledger.clone(), 1, false)
            .into_iter()
            .find(|tool| tool.definition().name == "spawn_role")
            .expect("spawn_role");
        let request =
            RequestContext::new(RequestId::Number(73)).with_extensions(Arc::new(extensions));
        let spawn_out = spawn_role
            .call_with_context(
                request,
                json!({
                    "role": ROLE,
                    "arguments": {
                        "repo": "local/repo",
                        "issue": "72",
                        "worktree": root_path.join("never-created").display().to_string(),
                    },
                    "spawned_by": root,
                }),
            )
            .await;
        let spawn_rendered = serde_json::to_string(&spawn_out).expect("render spawn_role");

        assert_eq!(start_rendered, spawn_rendered, "refusal paths drifted");
        assert!(
            start_rendered.contains("depth 2, past the limit of 1"),
            "must use authenticated parent depth: {start_rendered}"
        );
        assert!(!root_path.exists(), "preflight must run before clone");
        assert_eq!(ledger.list_agents().await.expect("agents").len(), 2);
        assert!(
            AssignmentDb::new(pool)
                .list()
                .await
                .expect("assignments")
                .is_empty(),
            "refusal must not record an assignment"
        );
    }

    /// Omitting the identity header cannot turn an at-limit agent into an
    /// anonymous root and bypass role authorization.
    #[tokio::test]
    async fn anonymous_agent_surface_cannot_start_an_issue() {
        let root = std::env::temp_dir().join(format!("ciacola-anonymous-{}", ulid::Ulid::new()));
        let repos = Repos {
            root: root.clone(),
            allowed: Arc::new(vec!["local/repo".into()]),
            gh_binary: PathBuf::from("gh"),
            cloning: Arc::new(tokio::sync::Mutex::new(())),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
        };
        let (plugin, ledger) = memory_plugin(repos).await;
        let start = plugin
            .tools(Surface::Agent)
            .into_iter()
            .find(|tool| tool.definition().name == "start_issue")
            .expect("start_issue");
        let out = start
            .call_with_context(
                RequestContext::new(RequestId::Number(74)),
                json!({
                    "repo": "local/repo",
                    "issue": 72,
                    "base": "main",
                    "spawned_by": "forged",
                }),
            )
            .await;
        let rendered = serde_json::to_string(&out).expect("render");

        assert!(
            rendered.contains("requires an authenticated agent"),
            "must refuse: {rendered}"
        );
        assert!(!root.exists(), "refusal must happen before clone");
        assert!(ledger.list_agents().await.expect("agents").is_empty());
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
            .call(json!({
                "repo": "local/repo",
                "issue": 70,
                "base": "main",
                "spawned_by": "forged-parent",
            }))
            .await;
        let rendered = serde_json::to_string(&out).expect("render");
        assert!(rendered.contains("\"agent_id\""), "got: {rendered}");

        let agents = ledger.list_agents().await.expect("agents");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].spawned_by, None, "caller prose is not lineage");
        let def = &agents[0].def;
        assert_eq!(def.provider.as_str(), "codex");
        assert_eq!(def.model, None, "the shipped Claude model must not leak");
        assert!(def.allowed_tools.is_empty());
        assert!(def.inherit_provider_tools);
        assert_eq!(def.sandbox.as_deref(), Some("workspace-write"));
        assert_eq!(def.catalog_role(), Some(ROLE));

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
        let bare = repos_root.join(format!("{}.git", repo_storage_key("local/repo")));
        CloneCommand::new(format!("file://{}", origin.display()))
            .bare()
            .directory(&bare)
            .execute()
            .await
            .expect("bare clone");
        configure_local_transport(&bare, &origin, "local/repo").await;

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
