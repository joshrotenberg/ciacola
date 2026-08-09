//! Direct provider-child environments, assembled by explicit inclusion.
//!
//! A provider CLI is a child of a long-lived server process. Letting that
//! child inherit the server's complete environment also hands it unrelated
//! credentials, client bearers, and server configuration. This module holds
//! the provider-neutral half of the fix: a small documented baseline plus
//! exact variable names an operator deliberately opted into. Even sensitive
//! names may be opted into: the safety property is that nothing is inherited
//! accidentally. Each adapter removes its own auth, routing, and config
//! selectors from this neutral snapshot before applying the intended values.
//!
//! The resulting snapshot is intentionally not serializable and its
//! [`Debug`](std::fmt::Debug) implementation renders names only. It belongs on
//! a provider instance created at startup, never in an agent definition, turn
//! intent, ledger row, or command line. An adapter applies it only after its
//! wrapper has cleared the inherited child environment, then adds the one
//! provider credential and any child-only MCP material that turn needs.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;

/// Environment names every provider child may receive when they are present.
///
/// This is deliberately narrower than a login shell. `PATH`, home/user
/// identity, temporary-directory selection, and locale are the portable
/// substrate provider CLIs and ordinary host tools need. Git consequently
/// retains its normal `HOME`-based config behavior. SSH agents, Git overrides,
/// proxies, GitHub tokens, and similar workflow-specific variables are not
/// baseline values; they require exact-name passthrough configuration.
///
/// The Windows entries are harmlessly absent on Unix and preserve the minimum
/// process-launch and home-directory conventions when Ciacola is built there.
pub const PROVIDER_CHILD_BASELINE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_COLLATE",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_PAPER",
    "LC_NAME",
    "LC_ADDRESS",
    "LC_TELEPHONE",
    "LC_MEASUREMENT",
    "LC_IDENTIFICATION",
    "SystemRoot",
    "WINDIR",
    "ComSpec",
    "PATHEXT",
    "USERPROFILE",
    "USERNAME",
];

/// Why an explicit provider-child environment could not be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderChildEnvironmentError {
    /// A configured entry is not a portable exact environment-variable name.
    InvalidName {
        /// The rejected name. Environment values are never carried here.
        name: String,
    },
    /// A selected value cannot be represented by the wrapper APIs, which take
    /// UTF-8 strings.
    NonUnicodeValue {
        /// The selected variable's name. Its value remains undisclosed.
        name: String,
    },
}

impl fmt::Display for ProviderChildEnvironmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name } => write!(
                formatter,
                "provider_env_passthrough entry '{name}' is not a portable environment-variable name"
            ),
            Self::NonUnicodeValue { name } => write!(
                formatter,
                "selected provider-child environment variable '{name}' is not valid UTF-8"
            ),
        }
    }
}

impl std::error::Error for ProviderChildEnvironmentError {}

/// A startup snapshot of the environment explicitly granted to provider CLIs.
///
/// Values may include opt-in workflow credentials such as a proxy URL or
/// `GH_TOKEN`, so debug output shows keys only. Clone this into provider
/// instances; do not persist or serialize it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProviderChildEnvironment {
    values: BTreeMap<String, String>,
}

impl ProviderChildEnvironment {
    /// Construct a snapshot from already-selected UTF-8 values.
    ///
    /// This is primarily useful to embedders that obtain their host policy
    /// from a source other than the process environment, and to deterministic
    /// adapter tests. Names receive the same portable exact-name validation as
    /// configured passthrough. Later duplicate entries replace earlier ones.
    pub fn from_values(
        values: impl IntoIterator<Item = (impl Into<String>, impl Into<String>)>,
    ) -> Result<Self, ProviderChildEnvironmentError> {
        let mut selected = BTreeMap::new();
        for (name, value) in values {
            let name = name.into();
            validate_name(&name)?;
            selected.insert(name, value.into());
        }
        Ok(Self { values: selected })
    }

    /// Validate exact-name passthrough configuration without reading the
    /// process environment.
    ///
    /// Duplicate names are accepted and collapse naturally in the eventual
    /// snapshot. Invalid names fail as a complete configuration error rather
    /// than being silently ignored. Sensitive names are valid: naming one here
    /// is the explicit opt-in, while adapters remain responsible for removing
    /// their own credential and routing selectors before launch.
    pub fn validate_passthrough(
        passthrough: &[String],
    ) -> Result<(), ProviderChildEnvironmentError> {
        for name in passthrough {
            validate_name(name)?;
        }
        Ok(())
    }

    /// Capture the fixed baseline and configured exact-name passthrough from
    /// the current process environment.
    ///
    /// Names that are not present are omitted. A selected non-UTF-8 value is
    /// an error because both pinned wrapper builder APIs accept UTF-8 strings;
    /// silently substituting or widening the environment would be misleading.
    pub fn capture(passthrough: &[String]) -> Result<Self, ProviderChildEnvironmentError> {
        Self::capture_with(passthrough, |name| std::env::var_os(name))
    }

    /// Iterate over granted names and values in deterministic name order.
    ///
    /// Calling this is an explicit secret-bearing boundary: adapters use it to
    /// populate a wrapper only after clearing inherited environment state.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Read one granted value by exact name.
    ///
    /// Prefer [`iter`](Self::iter) when applying the complete snapshot.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    /// Iterate over granted names without exposing their values.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.values.keys().map(String::as_str)
    }

    /// Clone this snapshot without names or namespaces an adapter owns.
    ///
    /// Matching is ASCII case-insensitive so the same policy remains safe on
    /// Windows, where environment names are case-insensitive. Provider
    /// adapters use this to remove their own authentication, routing, and
    /// config selectors even when an operator explicitly allowlisted one,
    /// then apply the intended credential and config home last.
    #[must_use]
    pub fn excluding(&self, exact: &[&str], prefixes: &[&str]) -> Self {
        let values = self
            .values
            .iter()
            .filter(|(name, _)| {
                !exact
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate))
                    && !prefixes.iter().any(|prefix| {
                        name.get(..prefix.len())
                            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
                    })
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        Self { values }
    }

    /// Number of granted variables in this snapshot.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether this snapshot grants no variables.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn capture_with(
        passthrough: &[String],
        mut lookup: impl FnMut(&str) -> Option<OsString>,
    ) -> Result<Self, ProviderChildEnvironmentError> {
        Self::validate_passthrough(passthrough)?;

        let names: BTreeSet<&str> = PROVIDER_CHILD_BASELINE_ENV
            .iter()
            .copied()
            .chain(passthrough.iter().map(String::as_str))
            .collect();
        let mut values = BTreeMap::new();
        for name in names {
            let Some(value) = lookup(name) else {
                continue;
            };
            let value = value.into_string().map_err(|_| {
                ProviderChildEnvironmentError::NonUnicodeValue {
                    name: name.to_string(),
                }
            })?;
            values.insert(name.to_string(), value);
        }
        Ok(Self { values })
    }
}

impl fmt::Debug for ProviderChildEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderChildEnvironment")
            .field("keys", &self.values.keys().collect::<Vec<_>>())
            .finish()
    }
}

fn validate_name(name: &str) -> Result<(), ProviderChildEnvironmentError> {
    let mut bytes = name.bytes();
    let valid_first = bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic());
    if !valid_first || !bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric()) {
        return Err(ProviderChildEnvironmentError::InvalidName {
            name: name.to_string(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn capture_includes_only_baseline_and_exact_opt_ins() {
        let passthrough = names(&["SSH_AUTH_SOCK", "HTTPS_PROXY", "GH_TOKEN"]);
        let ambient = BTreeMap::from([
            ("PATH", "/usr/bin"),
            ("HOME", "/home/operator"),
            ("LANG", "en_US.UTF-8"),
            ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
            ("HTTPS_PROXY", "https://proxy.example"),
            ("GH_TOKEN", "github-secret"),
            ("UNRELATED_SECRET", "must-not-pass"),
            ("MCP_CLIENT_SENTINEL", "must-not-pass"),
        ]);

        let captured = ProviderChildEnvironment::capture_with(&passthrough, |name| {
            ambient.get(name).map(OsString::from)
        })
        .expect("valid policy");

        assert_eq!(captured.get("PATH"), Some("/usr/bin"));
        assert_eq!(captured.get("HOME"), Some("/home/operator"));
        assert_eq!(captured.get("LANG"), Some("en_US.UTF-8"));
        assert_eq!(captured.get("SSH_AUTH_SOCK"), Some("/tmp/agent.sock"));
        assert_eq!(captured.get("HTTPS_PROXY"), Some("https://proxy.example"));
        assert_eq!(captured.get("GH_TOKEN"), Some("github-secret"));
        assert_eq!(captured.get("UNRELATED_SECRET"), None);
        assert_eq!(captured.get("MCP_CLIENT_SENTINEL"), None);
    }

    #[test]
    fn absent_opt_ins_are_omitted_and_empty_values_are_preserved() {
        let passthrough = names(&["SSH_AUTH_SOCK", "HTTPS_PROXY"]);
        let captured = ProviderChildEnvironment::capture_with(&passthrough, |name| {
            (name == "HTTPS_PROXY").then(OsString::new)
        })
        .expect("valid policy");

        assert_eq!(captured.get("SSH_AUTH_SOCK"), None);
        assert_eq!(captured.get("HTTPS_PROXY"), Some(""));
    }

    #[test]
    fn duplicate_and_baseline_names_collapse_in_the_snapshot() {
        let passthrough = names(&["PATH", "GH_TOKEN", "GH_TOKEN"]);
        let mut lookups = Vec::new();
        let captured = ProviderChildEnvironment::capture_with(&passthrough, |name| {
            if matches!(name, "PATH" | "GH_TOKEN") {
                lookups.push(name.to_string());
                Some(OsString::from(format!("value-for-{name}")))
            } else {
                None
            }
        })
        .expect("valid policy");

        assert_eq!(captured.len(), 2);
        assert_eq!(lookups, ["GH_TOKEN", "PATH"]);
    }

    #[test]
    fn explicit_values_use_the_same_name_policy_and_last_value_wins() {
        let environment = ProviderChildEnvironment::from_values([
            ("PATH", "/first"),
            ("PATH", "/second"),
            ("CIACOLA_SENTINEL", "deliberate"),
        ])
        .expect("valid explicit snapshot");
        assert_eq!(environment.get("PATH"), Some("/second"));
        assert_eq!(environment.get("CIACOLA_SENTINEL"), Some("deliberate"));

        assert!(ProviderChildEnvironment::from_values([("HAS-DASH", "no")]).is_err());
    }

    #[test]
    fn sensitive_and_workflow_values_are_valid_only_as_exact_opt_ins() {
        ProviderChildEnvironment::validate_passthrough(&names(&[
            "SSH_AUTH_SOCK",
            "GIT_ASKPASS",
            "HTTPS_PROXY",
            "http_proxy",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "MCP_BEARER",
            "CIACOLA_SENTINEL",
            "CLAUDE_CODE_USE_BEDROCK",
            "ANTHROPIC_API_KEY",
            "CODEX_API_KEY",
        ]))
        .expect("exactly named values are deliberate opt-ins");
    }

    #[test]
    fn adapters_can_remove_their_own_selectors_after_capture() {
        let passthrough = names(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "ANTHROPIC_API_KEY",
            "CODEX_API_KEY",
            "MCP_BEARER",
            "CIACOLA_SENTINEL",
        ]);
        let captured = ProviderChildEnvironment::capture_with(&passthrough, |name| {
            Some(OsString::from(format!("value-for-{name}")))
        })
        .expect("valid exact policy");

        let claude = captured.excluding(
            &["CLAUDECODE"],
            &["CLAUDE_", "ANTHROPIC_", "AWS_", "GOOGLE_"],
        );
        assert_eq!(claude.get("CLAUDE_CODE_USE_BEDROCK"), None);
        assert_eq!(claude.get("ANTHROPIC_API_KEY"), None);
        assert!(claude.get("CODEX_API_KEY").is_some());
        assert!(claude.get("MCP_BEARER").is_some());
        assert!(claude.get("CIACOLA_SENTINEL").is_some());

        let codex = captured.excluding(&[], &["CODEX_", "OPENAI_"]);
        assert_eq!(codex.get("CODEX_API_KEY"), None);
        assert!(codex.get("ANTHROPIC_API_KEY").is_some());
    }

    #[test]
    fn malformed_names_fail_before_environment_lookup() {
        for name in ["", "9TOKEN", "HAS-DASH", "HAS.DOT", "A=B", "SNOWMAN_☃"] {
            let error = ProviderChildEnvironment::validate_passthrough(&names(&[name]))
                .expect_err("not a portable exact environment name");
            assert!(matches!(
                error,
                ProviderChildEnvironmentError::InvalidName { .. }
            ));
        }
    }

    #[test]
    fn debug_renders_names_and_never_values() {
        let captured = ProviderChildEnvironment::capture_with(&names(&["GH_TOKEN"]), |name| {
            (name == "GH_TOKEN").then(|| OsString::from("never-print-this-secret"))
        })
        .expect("valid policy");

        let rendered = format!("{captured:?}");
        assert!(rendered.contains("GH_TOKEN"), "{rendered}");
        assert!(!rendered.contains("never-print-this-secret"), "{rendered}");
    }

    #[cfg(unix)]
    #[test]
    fn selected_non_unicode_values_fail_without_rendering_the_value() {
        use std::os::unix::ffi::OsStringExt;

        let error = ProviderChildEnvironment::capture_with(&names(&["GH_TOKEN"]), |name| {
            (name == "GH_TOKEN").then(|| OsString::from_vec(vec![0xff, 0xfe]))
        })
        .expect_err("wrapper APIs cannot carry non-UTF-8 values");

        assert_eq!(
            error,
            ProviderChildEnvironmentError::NonUnicodeValue {
                name: "GH_TOKEN".into()
            }
        );
        assert!(!error.to_string().contains('�'));
    }
}
