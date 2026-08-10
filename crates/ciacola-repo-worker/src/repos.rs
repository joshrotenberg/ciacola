//! Bare clones, worktrees, branches, and pushes: the on-disk git
//! lifecycle behind every assignment.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ciacola_core::agent::FlatError;
use git_spawn::{CloneCommand, GitCommand, WorktreeCommand};

use crate::assignment::Assignment;
use crate::git::{
    bare_repo, branch_ref_exists, check_branch_name, current_branch, delete_ref_at, git_output,
    git_predicate, github_origin_matches, is_ancestor, is_bare_repository, ls_remote_heads,
    repo_storage_key, rev_list_count, rev_parse_verify, stable_publication_url,
};

#[derive(Debug, Clone)]
pub(crate) struct WorktreeSnapshot {
    pub(crate) head: String,
    pub(crate) base_head: String,
    pub(crate) push_url: String,
    pub(crate) commits_ahead: u64,
    pub(crate) has_material_delta: bool,
}
#[derive(Clone)]
pub(crate) struct Repos {
    pub(crate) root: PathBuf,
    pub(crate) allowed: Arc<Vec<String>>,
    pub(crate) gh_binary: PathBuf,
    /// Held across every mutation of a bare repository: clone, fetch,
    /// worktree add/remove, and local branch cleanup. Assignment ownership
    /// is durable in SQLite; this lock prevents unrelated assignments from
    /// making git contend with itself inside one server process.
    pub(crate) cloning: Arc<tokio::sync::Mutex<()>>,
    pub(crate) lifecycle: Arc<tokio::sync::Mutex<()>>,
}

impl Repos {
    pub(crate) fn bare(&self, repo: &str) -> PathBuf {
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

    pub(crate) fn allows(&self, repo: &str) -> bool {
        self.allowed.iter().any(|r| r == repo)
    }

    /// Clone once into the plugin's own root, then refresh and reuse.
    #[cfg(test)]
    pub(crate) async fn ensure_clone_from(
        &self,
        repo: &str,
        url: &str,
    ) -> Result<PathBuf, FlatError> {
        let _guard = self.cloning.lock().await;
        self.ensure_clone_from_locked(repo, url).await
    }

    pub(crate) async fn ensure_clone_from_locked(
        &self,
        repo: &str,
        url: &str,
    ) -> Result<PathBuf, FlatError> {
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
        let is_bare = is_bare_repository(&bare).await.map_err(|e| -> FlatError {
            format!(
                "existing clone path '{}' is not a usable bare repository: {e}",
                bare.display()
            )
            .into()
        })?;
        if !is_bare {
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

    pub(crate) async fn validate_worktree_at(
        &self,
        path: &Path,
        branch: &str,
        bare: &Path,
    ) -> Result<(), FlatError> {
        if !path.is_dir() {
            return Err(format!("worktree '{}' is not a directory", path.display()).into());
        }
        let actual_branch = current_branch(path).await.map_err(|e| -> FlatError {
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

    pub(crate) async fn inspect_assignment_worktree(
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
        if !check_branch_name(bare, base).await? {
            return Err(format!("assignment base '{base}' is not a valid branch name").into());
        }
        if !check_branch_name(bare, &assignment.branch).await? {
            return Err(format!(
                "assignment branch '{}' is not a valid branch name",
                assignment.branch
            )
            .into());
        }
        let head = rev_parse_verify(worktree, "HEAD^{commit}").await?;
        let branch_head = rev_parse_verify(
            worktree,
            &format!("refs/heads/{}^{{commit}}", assignment.branch),
        )
        .await?;
        if branch_head != head {
            return Err(format!(
                "assigned branch '{}' points at {branch_head}, but worktree HEAD is {head}",
                assignment.branch
            )
            .into());
        }
        let canonical_base = rev_parse_verify(worktree, &format!("{base_head}^{{commit}}")).await?;
        if canonical_base != base_head {
            return Err(format!(
                "durable base head '{base_head}' is not a full canonical commit OID"
            )
            .into());
        }
        if !is_ancestor(worktree, base_head, &head).await? {
            return Err(format!(
                "assigned branch head {head} is not descended from durable base {base_head}"
            )
            .into());
        }
        let commits_ahead = rev_list_count(worktree, &format!("{base_head}..{head}")).await?;
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

    pub(crate) async fn remote_branch_head(
        &self,
        assignment: &Assignment,
        push_url: &str,
    ) -> Result<Option<String>, FlatError> {
        let mut entries = ls_remote_heads(
            Path::new(&assignment.worktree),
            push_url,
            &format!("refs/heads/{}", assignment.branch),
        )
        .await?;
        if entries.len() > 1 {
            return Err("remote branch query returned more than one exact ref".into());
        }
        Ok(entries.pop().map(|entry| entry.sha))
    }

    pub(crate) async fn local_branch_head(
        &self,
        assignment: &Assignment,
    ) -> Result<Option<String>, FlatError> {
        let bare = Path::new(&assignment.bare_path);
        if !bare.exists() {
            return Ok(None);
        }
        let reference = format!("refs/heads/{}", assignment.branch);
        if !branch_ref_exists(bare, &reference).await? {
            return Ok(None);
        }
        Ok(Some(
            rev_parse_verify(bare, &format!("{reference}^{{commit}}")).await?,
        ))
    }

    pub(crate) async fn push_exact(
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
    pub(crate) async fn add_worktree(
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

    pub(crate) async fn add_worktree_from(
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

    pub(crate) async fn remove_worktree_at(
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
        let reference = format!("refs/heads/{branch}");
        let exists = branch_ref_exists(bare, &reference)
            .await
            .map_err(|_| -> FlatError {
                format!("cannot inspect branch '{branch}' in '{}'", bare.display()).into()
            })?;
        if exists {
            let expected_head = expected_head.ok_or_else(|| -> FlatError {
                format!("refusing to delete local branch '{branch}' without an expected commit")
                    .into()
            })?;
            delete_ref_at(bare, &reference, expected_head)
                .await
                .map_err(|e| -> FlatError { format!("branch delete {branch}: {e}").into() })?;
            if branch_ref_exists(bare, &reference).await? {
                return Err(format!(
                    "local branch '{branch}' moved before compare-and-swap deletion"
                )
                .into());
            }
        }
        Ok(())
    }

    pub(crate) fn worktrees(&self) -> Result<Vec<PathBuf>, FlatError> {
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
