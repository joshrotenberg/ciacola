//! Process helpers: the gh CLI and the raw git calls git-spawn
//! does not cover yet. Per-command migration onto git-spawn's typed
//! builders happens here, one command family at a time.

use std::path::Path;

use ciacola_core::agent::FlatError;
use git_spawn::parse::LsRemoteEntry;
use git_spawn::{
    CheckRefFormatCommand, GitCommand, LsRemoteCommand, MergeBaseCommand, Repository,
    RevListCommand, RevParseCommand, ShowRefCommand, SymbolicRefCommand, UpdateRefCommand,
};

/// git-spawn, dogfooded, in place of hand-rolled `Command` calls.
///
/// One gap found immediately by using it: `Repository::open` requires a
/// `.git` entry, so it rejects a bare repository, which is exactly what
/// this plugin keeps. `new_unchecked` is the way through and is
/// documented for a different purpose (about to init or clone). Filed
/// upstream as joshrotenberg/git-spawn#157.
pub(crate) fn bare_repo(path: &Path) -> Repository {
    Repository::new_unchecked(path)
}

pub(crate) fn repo_storage_key(repo: &str) -> String {
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

pub(crate) fn github_origin_matches(repo: &str, origin: &str) -> bool {
    let repo = repo.to_ascii_lowercase();
    let origin = origin.to_ascii_lowercase();
    // A transport URL normally has one optional `.git` suffix. The configured
    // GitHub repository name is data, though, and may itself end in `.git`.
    let origin = origin.strip_suffix(".git").unwrap_or(&origin);
    origin == format!("https://github.com/{repo}")
        || origin == format!("git@github.com:{repo}")
        || origin == format!("ssh://git@github.com/{repo}")
}
pub(crate) async fn gh(
    binary: &Path,
    dir: Option<&Path>,
    args: &[&str],
) -> Result<String, FlatError> {
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

pub(crate) fn github_repo(repo: &str) -> String {
    format!("github.com/{repo}")
}

pub(crate) async fn git_output(dir: &Path, args: &[&str]) -> Result<String, FlatError> {
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

pub(crate) async fn git_predicate(dir: &Path, args: &[&str]) -> Result<bool, FlatError> {
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

/// Flatten a typed git-spawn failure into the `git <args>: <stderr>` shape
/// the raw helpers produced, so wrapping messages and the error text tests
/// assert on are unchanged by the builder migration.
fn flat_git_error(error: git_spawn::Error) -> FlatError {
    match error {
        git_spawn::Error::CommandFailed {
            command, stderr, ..
        } => format!("{command}: {}", stderr.trim()).into(),
        other => Box::new(other),
    }
}

/// `git rev-parse --verify <rev>`: resolve a revision or fail loudly.
pub(crate) async fn rev_parse_verify(dir: &Path, rev: &str) -> Result<String, FlatError> {
    let mut command = RevParseCommand::new();
    command.verify().arg_str(rev).current_dir(dir);
    command.execute().await.map_err(flat_git_error)
}

/// `git rev-parse --is-bare-repository`.
pub(crate) async fn is_bare_repository(dir: &Path) -> Result<bool, FlatError> {
    let mut command = RevParseCommand::new();
    command.is_bare_repository().current_dir(dir);
    Ok(command.execute().await.map_err(flat_git_error)? == "true")
}

/// `git symbolic-ref --quiet --short HEAD`: the checked-out branch name.
pub(crate) async fn current_branch(dir: &Path) -> Result<String, FlatError> {
    let mut command = SymbolicRefCommand::read("HEAD");
    command.quiet().short().current_dir(dir);
    command.execute().await.map_err(flat_git_error)
}

/// `git ls-remote --heads <url> <reference>`, parsed into typed entries.
pub(crate) async fn ls_remote_heads(
    dir: &Path,
    url: &str,
    reference: &str,
) -> Result<Vec<LsRemoteEntry>, FlatError> {
    let mut command = LsRemoteCommand::remote(url);
    command.heads().pattern(reference).current_dir(dir);
    let output = command.execute().await.map_err(flat_git_error)?;
    Ok(command.parse_entries(&output))
}

/// `git show-ref --verify --quiet <reference>` as a predicate: exit 0 is
/// true, exit 1 is false, anything else is an error.
pub(crate) async fn branch_ref_exists(dir: &Path, reference: &str) -> Result<bool, FlatError> {
    let mut command = ShowRefCommand::new();
    command.verify().quiet().pattern(reference).current_dir(dir);
    let output = command.execute_raw_unchecked().await?;
    match output.exit_code {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(format!(
            "git show-ref --verify --quiet {reference}: {}",
            output.stderr.trim()
        )
        .into()),
    }
}

/// `git update-ref --no-deref -d <reference> <expected>`: the
/// compare-and-swap branch deletion.
pub(crate) async fn delete_ref_at(
    dir: &Path,
    reference: &str,
    expected: &str,
) -> Result<(), FlatError> {
    let mut command = UpdateRefCommand::new();
    command
        .no_deref()
        .delete()
        .ref_name(reference)
        .old_value(expected)
        .current_dir(dir);
    command.execute().await.map_err(flat_git_error)?;
    Ok(())
}

/// `git check-ref-format --branch <branch>` as a predicate, preserving the
/// raw helper's exit-code handling: 0 is valid, 1 is invalid, and the 128
/// git actually emits for malformed names in `--branch` mode stays an error.
pub(crate) async fn check_branch_name(dir: &Path, branch: &str) -> Result<bool, FlatError> {
    let mut command = CheckRefFormatCommand::branch(branch);
    command.current_dir(dir);
    let output = command.execute_raw_unchecked().await?;
    match output.exit_code {
        0 => Ok(true),
        1 => Ok(false),
        _ => Err(format!(
            "git check-ref-format --branch {branch}: {}",
            output.stderr.trim()
        )
        .into()),
    }
}

/// `git merge-base --is-ancestor <ancestor> <descendant>`.
pub(crate) async fn is_ancestor(
    dir: &Path,
    ancestor: &str,
    descendant: &str,
) -> Result<bool, FlatError> {
    let mut command = MergeBaseCommand::new();
    command
        .is_ancestor()
        .commit(ancestor)
        .commit(descendant)
        .current_dir(dir);
    command.execute_is_ancestor().await.map_err(flat_git_error)
}

/// `git rev-list --count <range>`.
pub(crate) async fn rev_list_count(dir: &Path, range: &str) -> Result<u64, FlatError> {
    let mut command = RevListCommand::new();
    command.count().range(range).current_dir(dir);
    Ok(command.execute().await.map_err(flat_git_error)?.parse()?)
}

pub(crate) async fn validate_branch_name(branch: &str) -> Result<(), FlatError> {
    let output = CheckRefFormatCommand::branch(branch)
        .execute_raw_unchecked()
        .await?;
    if output.success {
        return Ok(());
    }
    Err(format!(
        "branch template rendered invalid Git branch '{branch}': {}",
        output.stderr.trim()
    )
    .into())
}

/// Still raw: `StatusCommand` covers `--porcelain=v1 -z
/// --untracked-files=all` but has no typed `--ignore-submodules=none`,
/// which this check needs so repository config can never hide submodule
/// changes. Migrates when the upstream gap closes.
pub(crate) async fn worktree_is_clean(dir: &Path) -> Result<bool, FlatError> {
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

pub(crate) async fn stable_publication_url(dir: &Path, url: &str) -> Result<(), FlatError> {
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
