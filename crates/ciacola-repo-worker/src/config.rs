//! Plugin configuration and branch-name policy.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;

pub(crate) const DEFAULT_BRANCH_TEMPLATE: &str = "agent/{slug}";
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BranchTemplate(pub(crate) String);

impl fmt::Debug for BranchTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BranchTemplate").field(&self.0).finish()
    }
}

impl BranchTemplate {
    pub(crate) fn parse(value: String) -> Result<Self, String> {
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

    pub(crate) fn render(&self, slug: &str) -> String {
        self.0.replace("{slug}", slug)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BranchPolicies {
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
    pub(crate) fn new(
        allowed: &[String],
        configured: BTreeMap<String, String>,
    ) -> Result<Self, String> {
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

    pub(crate) fn for_repo(&self, repo: &str) -> &BranchTemplate {
        self.configured.get(repo).unwrap_or(&self.default)
    }

    pub(crate) fn configured_state(&self) -> BTreeMap<&str, &str> {
        self.configured
            .iter()
            .map(|(repo, template)| (repo.as_str(), template.as_str()))
            .collect()
    }
}
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RepoWorkerConfig {
    /// Where clones and worktrees live. `~` is expanded.
    pub(crate) root: Option<String>,
    /// Repositories that may be worked on, `owner/name`. An empty list
    /// means none: this plugin does not get to pick.
    #[serde(default)]
    pub(crate) repos: Vec<String>,
    /// Per-repository branch templates. The only placeholder is `{slug}`,
    /// which is required exactly once so every assignment remains unique.
    #[serde(default)]
    pub(crate) branch_templates: BTreeMap<String, String>,
}

pub(crate) fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var("HOME") {
            Ok(home) => PathBuf::from(home).join(rest),
            Err(_) => PathBuf::from(path),
        },
        None => PathBuf::from(path),
    }
}
