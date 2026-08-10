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

use std::path::PathBuf;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::Tool;

use ciacola_core::agent::FlatError;
use ciacola_core::plugin::{BoxFut, Migration, Plugin, PluginContext, Section, Surface};
use ciacola_core::roles::Role;

mod assignment;
mod board;
mod config;
mod db;
mod delegation;
mod git;
mod journey;
mod migrations;
mod repos;
mod tools;
use config::{BranchPolicies, RepoWorkerConfig, expand};
use db::AssignmentDb;
use migrations::{ASSIGNMENTS_TABLE, MIGRATIONS};
use repos::Repos;

const ROLE: &str = "issue-implementer";
const START_ISSUE_ROLE_ARGUMENTS: [&str; 3] = ["repo", "issue", "worktree"];
/// The other half of the loop: whoever dispatches work is also who
/// notices what the implementer prompt got wrong, and that only turns
/// into a better prompt if it is somebody's stated job.
const MANAGER: &str = "repo-manager";

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

#[derive(Default)]
pub struct RepoWorkerPlugin {
    repos: Option<Repos>,
    ctx: Option<PluginContext>,
    branch_policies: BranchPolicies,
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
                system_prompt: include_str!("prompts/repo-manager.md").into(),
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
                system_prompt: include_str!("prompts/issue-implementer.md").into(),
            },
        ]
    }

    fn tools(&self, surface: Surface) -> Vec<Tool> {
        tools::tools(self, surface)
    }

    fn board_section(&self) -> BoxFut<'_, Option<Section>> {
        board::board_section(self)
    }

    fn health(&self) -> BoxFut<'_, serde_json::Value> {
        board::health(self)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use git_spawn::{CloneCommand, GitCommand, WorktreeCommand};

    use super::*;
    use crate::assignment::{
        Assignment, AssignmentState, CleanupReason, CleanupState, LegacyAssignment, PrState,
        PublicationState, sqlite_u64,
    };
    use crate::config::DEFAULT_BRANCH_TEMPLATE;
    use crate::delegation::{
        DelegatedAssignmentRefusal, DelegatedAssignmentRequest, DelegatedFinishDisposition,
        DelegatedFinishIssueRequest, DelegatedLineageRefusal, DelegatedLineageSubject,
        DelegatedOpenPrRequest,
    };
    use crate::git::git_output;
    use crate::git::{bare_repo, git_predicate, github_origin_matches, repo_storage_key};
    use crate::journey::GhPr;
    use crate::journey::conventional_title;
    use ciacola_core::delegation::{DelegatableAction, DelegationPolicy};
    use ciacola_core::ledger::Ledger;
    use ciacola_core::roles::Roles;
    use serde_json::json;
    use std::path::Path;
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
            loopback_mcp_config: "agent.json".into(),
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

        let manager_cycle = delegation_fixture(false).await;
        sqlx::query("UPDATE agents SET spawned_by = ?2 WHERE agent_id = ?1")
            .bind(&manager_cycle.manager)
            .bind(&manager_cycle.owner)
            .execute(&manager_cycle.plugin.ctx.as_ref().expect("context").pool)
            .await
            .expect("cycle through manager");
        let refusal = manager_cycle
            .plugin
            .preflight_delegated_assignment(
                &manager_cycle.manager,
                &policy,
                &delegated_open(&manager_cycle.assignment_id),
            )
            .await
            .expect_err("manager cycle");
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
        assert_eq!(board.title, "repository journeys");
        assert!(board.html.contains("1 current assignment(s)"));
        assert!(board.html.contains("Current repository journeys"));
        assert!(
            board
                .html
                .contains("https://github.com/local/repo/issues/76")
        );
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
        let board = plugin.board_section().await.expect("board section");
        assert!(
            board
                .html
                .contains("No open or retained repository journeys."),
            "{}",
            board.html
        );
        assert!(
            board.html.contains("1 completed journey(s)"),
            "{}",
            board.html
        );
        assert!(
            board
                .html
                .contains("Recently completed repository journeys"),
            "{}",
            board.html
        );
        assert!(board.html.contains("discarded"), "{}", board.html);
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
            loopback_mcp_config: "agent.json".into(),
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
            loopback_mcp_config: "agent.json".into(),
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
