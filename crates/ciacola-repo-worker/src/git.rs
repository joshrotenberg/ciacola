//! Process helpers: the gh CLI and the raw git calls git-spawn
//! does not cover yet. Per-command migration onto git-spawn's typed
//! builders happens here, one command family at a time.

use std::path::Path;

use ciacola_core::agent::FlatError;
use git_spawn::Repository;

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

pub(crate) async fn validate_branch_name(branch: &str) -> Result<(), FlatError> {
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
