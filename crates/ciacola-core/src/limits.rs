//! Admission circuit breakers.
//!
//! Money and tokens are deliberately separate measurements. Some
//! providers price their own work, some report portable token usage,
//! and Ciacola does not maintain a price table to pretend those are the
//! same thing. A configured stop applies to new submissions only;
//! already-running work always settles into the ledger.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::agent::FlatError;

pub const DEFAULT_MAX_SPAWN_DEPTH: i64 = 3;
pub const ROLLING_WINDOW_SECS: i64 = 86_400;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderLimits {
    /// Warn when total input + output reaches this value in 24 hours.
    pub daily_warn_tokens: Option<u64>,
    /// Refuse new submissions at or above this value in 24 hours.
    pub daily_stop_tokens: Option<u64>,
    /// Provider-native ceiling applied independently to every turn.
    ///
    /// The unit and enforcement boundary come from the selected
    /// provider's declared capability. Unlike the rolling token stop,
    /// this is passed to the provider and enforced while that turn runs.
    pub per_turn_ceiling: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Notify once the rolling day reaches this reported spend.
    pub daily_warn_usd: Option<f64>,
    /// Refuse new submissions at or above this reported spend.
    pub daily_stop_usd: Option<f64>,
    /// Per-provider portable token breakers.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderLimits>,
    /// How deep a spawned_by chain may go. `0` disables the check.
    #[serde(default = "default_depth")]
    pub max_spawn_depth: i64,
}

fn default_depth() -> i64 {
    DEFAULT_MAX_SPAWN_DEPTH
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            daily_warn_usd: None,
            daily_stop_usd: None,
            providers: BTreeMap::new(),
            max_spawn_depth: DEFAULT_MAX_SPAWN_DEPTH,
        }
    }
}

impl Limits {
    pub fn stop_micro_usd(&self) -> Option<i64> {
        self.daily_stop_usd.map(usd_to_micro_saturated)
    }

    pub fn warn_micro_usd(&self) -> Option<i64> {
        self.daily_warn_usd.map(usd_to_micro_saturated)
    }

    pub fn provider(&self, provider: &str) -> ProviderLimits {
        self.providers.get(provider).cloned().unwrap_or_default()
    }

    /// Validate values without needing a runtime provider registry.
    pub fn validate(&self) -> Result<(), FlatError> {
        validate_usd("daily_warn_usd", self.daily_warn_usd)?;
        validate_usd("daily_stop_usd", self.daily_stop_usd)?;
        if let (Some(warn), Some(stop)) = (self.warn_micro_usd(), self.stop_micro_usd())
            && warn > stop
        {
            return Err("limits: daily_warn_usd must be <= daily_stop_usd".into());
        }
        if self.max_spawn_depth < 0 {
            return Err("limits: max_spawn_depth must be >= 0".into());
        }
        for (provider, limits) in &self.providers {
            validate_tokens(provider, "daily_warn_tokens", limits.daily_warn_tokens)?;
            validate_tokens(provider, "daily_stop_tokens", limits.daily_stop_tokens)?;
            validate_ceiling(provider, limits.per_turn_ceiling)?;
            if let (Some(warn), Some(stop)) = (limits.daily_warn_tokens, limits.daily_stop_tokens)
                && warn > stop
            {
                return Err(format!(
                    "limits.providers.{provider}: daily_warn_tokens must be <= daily_stop_tokens"
                )
                .into());
            }
        }
        Ok(())
    }

    /// Validate provider keys and measurement capabilities after the
    /// binary assembles its adapter registry, before any executor starts.
    pub fn validate_providers(
        &self,
        providers: &ciacola_agent::ProviderRegistry,
    ) -> Result<(), FlatError> {
        self.validate()?;
        for (key, limits) in &self.providers {
            let provider = providers
                .get(&ciacola_agent::ProviderKey::new(key.clone()))
                .map_err(|error| -> FlatError { error.to_string().into() })?;
            if (limits.daily_warn_tokens.is_some() || limits.daily_stop_tokens.is_some())
                && !provider.capabilities().reports_token_usage
            {
                return Err(format!(
                    "limits.providers.{key}: token limits require a provider that reports token usage"
                )
                .into());
            }
        }
        Ok(())
    }

    /// Describes itself for the board and the startup banner.
    pub fn summary(&self) -> String {
        let money = match (self.daily_warn_usd, self.daily_stop_usd) {
            (Some(w), Some(s)) => format!("warn ${w:.2}/day, stop ${s:.2}/day"),
            (Some(w), None) => format!("warn ${w:.2}/day"),
            (None, Some(s)) => format!("stop ${s:.2}/day"),
            (None, None) => "no spend limit".into(),
        };
        let tokens = if self.providers.is_empty() {
            "no provider token limits".to_string()
        } else {
            self.providers
                .iter()
                .map(|(provider, limit)| {
                    let rolling = match (limit.daily_warn_tokens, limit.daily_stop_tokens) {
                        (Some(warn), Some(stop)) => {
                            format!("{provider} warn {warn} tokens/day, stop {stop}")
                        }
                        (Some(warn), None) => format!("{provider} warn {warn} tokens/day"),
                        (None, Some(stop)) => format!("{provider} stop {stop} tokens/day"),
                        (None, None) => format!("{provider} token limits disabled"),
                    };
                    match limit.per_turn_ceiling {
                        Some(ceiling) => {
                            format!("{rolling}, per-turn ceiling {ceiling} provider units")
                        }
                        None => format!("{rolling}, per-turn unbounded"),
                    }
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        let depth = match self.max_spawn_depth {
            0 => "unlimited spawn depth".to_string(),
            d => format!("spawn depth {d}"),
        };
        format!("{money}, {tokens}, {depth}")
    }
}

fn usd_to_micro_saturated(usd: f64) -> i64 {
    (usd * 1e6).round() as i64
}

fn validate_usd(name: &str, value: Option<f64>) -> Result<(), FlatError> {
    let Some(value) = value else {
        return Ok(());
    };
    if !value.is_finite() || value <= 0.0 || value * 1e6 > i64::MAX as f64 {
        return Err(format!(
            "limits: {name} must be a finite positive USD value representable in micro-dollars"
        )
        .into());
    }
    Ok(())
}

fn validate_tokens(provider: &str, name: &str, value: Option<u64>) -> Result<(), FlatError> {
    if let Some(value) = value
        && (value == 0 || value > i64::MAX as u64)
    {
        return Err(format!(
            "limits.providers.{provider}: {name} must be between 1 and {} tokens",
            i64::MAX
        )
        .into());
    }
    Ok(())
}

fn validate_ceiling(provider: &str, value: Option<u64>) -> Result<(), FlatError> {
    if let Some(value) = value
        && (value == 0 || value > i64::MAX as u64)
    {
        return Err(format!(
            "limits.providers.{provider}: per_turn_ceiling must be between 1 and {} provider-native units",
            i64::MAX
        )
        .into());
    }
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ProviderAccounting {
    pub provider: String,
    pub reports_cost: bool,
    pub reports_token_usage: bool,
    /// Non-retired agents currently selecting this provider.
    pub active_agents: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// A subset of `tokens_in`, reported separately and never added to total.
    pub tokens_cached: u64,
    pub usage_complete_turns: u64,
    pub usage_incomplete_turns: u64,
    pub usage_unreported_turns: u64,
    pub usage_not_tracked_turns: u64,
    pub usage_legacy_unknown_turns: u64,
    pub running_partial_turns: u64,
    pub cost_unreported_turns: u64,
    pub cost_not_priced_turns: u64,
    pub cost_legacy_unknown_turns: u64,
}

impl ProviderAccounting {
    pub fn total_tokens(&self) -> u64 {
        self.tokens_in.saturating_add(self.tokens_out)
    }

    pub fn usage_gaps(&self) -> u64 {
        self.usage_incomplete_turns
            .saturating_add(self.usage_unreported_turns)
            .saturating_add(self.usage_not_tracked_turns)
            .saturating_add(self.usage_legacy_unknown_turns)
    }

    pub fn cost_gaps(&self) -> u64 {
        self.cost_unreported_turns
            .saturating_add(self.cost_legacy_unknown_turns)
            .saturating_add(if self.reports_cost {
                self.cost_not_priced_turns
            } else {
                0
            })
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct AdmissionAccounting {
    pub checked_unix: i64,
    pub since_unix: i64,
    pub reported_spend_micro_usd: u64,
    pub providers: BTreeMap<String, ProviderAccounting>,
}

impl AdmissionAccounting {
    pub fn cost_gaps(&self) -> u64 {
        self.providers.values().fold(0_u64, |total, provider| {
            total.saturating_add(provider.cost_gaps())
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionState {
    Ok,
    Warning,
    Stopped,
    Unobservable,
    Unguarded,
}

impl AdmissionState {
    pub fn automatic_allowed(self) -> bool {
        matches!(self, Self::Ok | Self::Warning)
    }

    pub fn supervised_override_allowed(self) -> bool {
        matches!(self, Self::Unobservable | Self::Unguarded)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Stopped => "stopped",
            Self::Unobservable => "unobservable",
            Self::Unguarded => "unguarded",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GlobalAdmissionStatus {
    pub reported_spend_micro_usd: u64,
    pub daily_warn_micro_usd: Option<u64>,
    pub daily_stop_micro_usd: Option<u64>,
    pub cost_gaps: u64,
    pub state: AdmissionState,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProviderAdmissionStatus {
    #[serde(flatten)]
    pub accounting: ProviderAccounting,
    pub daily_warn_tokens: Option<u64>,
    pub daily_stop_tokens: Option<u64>,
    /// Configured provider-native ceiling for each newly admitted turn.
    pub per_turn_ceiling: Option<u64>,
    /// Whether that separate, per-turn protection can be applied by the
    /// currently registered adapter. This does not describe the rolling
    /// daily stop in `state`.
    pub turn_protection: TurnProtectionStatus,
    /// Exact provider semantics used by an enforced ceiling. Persisting
    /// this snapshot on the turn prevents a restart from silently changing
    /// the meter or enforcement boundary.
    pub turn_ceiling_capability: Option<ciacola_agent::CeilingCapability>,
    pub state: AdmissionState,
    pub automatic_allowed: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnProtectionStatus {
    Enforced,
    Unbounded,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AdmissionReport {
    pub window_seconds: i64,
    pub checked_unix: i64,
    pub since_unix: i64,
    pub global: GlobalAdmissionStatus,
    pub providers: Vec<ProviderAdmissionStatus>,
}

impl Limits {
    pub fn evaluate(&self, accounting: &AdmissionAccounting) -> AdmissionReport {
        let spend = accounting.reported_spend_micro_usd;
        let warn_usd = self.warn_micro_usd().map(|value| value as u64);
        let stop_usd = self.stop_micro_usd().map(|value| value as u64);
        let cost_gaps = accounting.cost_gaps();
        let global_state = if stop_usd.is_some_and(|stop| spend >= stop) {
            AdmissionState::Stopped
        } else if stop_usd.is_some() && cost_gaps > 0 {
            AdmissionState::Unobservable
        } else if warn_usd.is_some_and(|warn| spend >= warn) {
            AdmissionState::Warning
        } else {
            AdmissionState::Ok
        };

        let providers = accounting
            .providers
            .values()
            .cloned()
            .map(|provider| {
                let limits = self.provider(&provider.provider);
                let total_tokens = provider.total_tokens();
                let token_gaps = provider.usage_gaps();
                let known_stop = stop_usd.is_some_and(|stop| spend >= stop)
                    || limits
                        .daily_stop_tokens
                        .is_some_and(|stop| total_tokens >= stop);
                // The USD breaker is global. A gap from any backend that
                // normally reports cost makes that configured stop unknown
                // for every new submission, even one using an unpriced
                // backend; the known reported total may already be low.
                let cost_unobservable = stop_usd.is_some() && cost_gaps > 0;
                let token_unobservable = limits.daily_stop_tokens.is_some() && token_gaps > 0;
                let unguarded = !provider.reports_cost && limits.daily_stop_tokens.is_none();
                let warning = warn_usd.is_some_and(|warn| spend >= warn)
                    || limits
                        .daily_warn_tokens
                        .is_some_and(|warn| total_tokens >= warn);
                let (state, detail) = if known_stop {
                    (
                        AdmissionState::Stopped,
                        Some("a configured hard stop has been reached".into()),
                    )
                } else if cost_unobservable || token_unobservable {
                    (
                        AdmissionState::Unobservable,
                        Some(
                            "a configured hard stop has incomplete telemetry in its rolling window"
                                .into(),
                        ),
                    )
                } else if unguarded {
                    (
                        AdmissionState::Unguarded,
                        Some("this unpriced provider has no token stop for automatic work".into()),
                    )
                } else if warning {
                    (AdmissionState::Warning, None)
                } else {
                    (AdmissionState::Ok, None)
                };
                ProviderAdmissionStatus {
                    accounting: provider,
                    daily_warn_tokens: limits.daily_warn_tokens,
                    daily_stop_tokens: limits.daily_stop_tokens,
                    per_turn_ceiling: limits.per_turn_ceiling,
                    turn_protection: if limits.per_turn_ceiling.is_some() {
                        TurnProtectionStatus::Unavailable
                    } else {
                        TurnProtectionStatus::Unbounded
                    },
                    turn_ceiling_capability: None,
                    state,
                    // Capability resolution happens in Ledger, which owns
                    // the live provider registry. Until then a configured
                    // ceiling is conservatively unavailable rather than a
                    // contradictory "unavailable but automatic" report.
                    automatic_allowed: state.automatic_allowed()
                        && limits.per_turn_ceiling.is_none(),
                    detail,
                }
            })
            .collect();

        AdmissionReport {
            window_seconds: ROLLING_WINDOW_SECS,
            checked_unix: accounting.checked_unix,
            since_unix: accounting.since_unix,
            global: GlobalAdmissionStatus {
                reported_spend_micro_usd: spend,
                daily_warn_micro_usd: warn_usd,
                daily_stop_micro_usd: stop_usd,
                cost_gaps,
                state: global_state,
            },
            providers,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionOverrideKind {
    Unguarded,
    Unobservable,
}

impl AdmissionOverrideKind {
    pub fn from_state(state: AdmissionState) -> Option<Self> {
        match state {
            AdmissionState::Unguarded => Some(Self::Unguarded),
            AdmissionState::Unobservable => Some(Self::Unobservable),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AdmissionOverride {
    pub kind: AdmissionOverrideKind,
    pub reason: String,
    pub source: String,
    pub provider: String,
    pub checked_unix: i64,
    pub reported_spend_micro_usd: u64,
    pub daily_stop_micro_usd: Option<u64>,
    pub reported_tokens: u64,
    pub daily_stop_tokens: Option<u64>,
    pub cost_gaps: u64,
    pub usage_gaps: u64,
}

/// Durable result of resolving the configured per-turn ceiling at the
/// admission boundary.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnProtectionState {
    Enforced,
    Unbounded,
    OverrideUnavailable,
    Legacy,
}

impl TurnProtectionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enforced => "enforced",
            Self::Unbounded => "unbounded",
            Self::OverrideUnavailable => "override_unavailable",
            Self::Legacy => "legacy",
        }
    }
}

/// Separate audit for proceeding when a configured per-turn protection is
/// unavailable. It deliberately does not share the rolling admission
/// override column: both conditions may need acknowledgement on one turn.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct TurnProtectionOverride {
    pub reason: String,
    pub source: String,
    pub checked_unix: i64,
}

/// Versioned, canonical policy snapshot stamped before a turn is queued.
///
/// `capability` is the exact semantic contract (meter, enforcement boundary,
/// and cache treatment), not merely a claim that a provider has some limit.
/// Execution compares it byte-for-value with the live adapter before launch.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct TurnProtectionSnapshot {
    pub version: u8,
    pub provider: String,
    pub state: TurnProtectionState,
    pub configured_limit: Option<u64>,
    pub capability: Option<ciacola_agent::CeilingCapability>,
    pub unavailable_override: Option<TurnProtectionOverride>,
}

impl TurnProtectionSnapshot {
    pub const VERSION: u8 = 1;

    pub fn unbounded(provider: impl Into<String>) -> Self {
        Self {
            version: Self::VERSION,
            provider: provider.into(),
            state: TurnProtectionState::Unbounded,
            configured_limit: None,
            capability: None,
            unavailable_override: None,
        }
    }

    pub fn enforced(
        provider: impl Into<String>,
        limit: u64,
        capability: ciacola_agent::CeilingCapability,
    ) -> Self {
        Self {
            version: Self::VERSION,
            provider: provider.into(),
            state: TurnProtectionState::Enforced,
            configured_limit: Some(limit),
            capability: Some(capability),
            unavailable_override: None,
        }
    }

    pub fn override_unavailable(
        provider: impl Into<String>,
        limit: u64,
        audit: TurnProtectionOverride,
    ) -> Self {
        Self {
            version: Self::VERSION,
            provider: provider.into(),
            state: TurnProtectionState::OverrideUnavailable,
            configured_limit: Some(limit),
            capability: None,
            unavailable_override: Some(audit),
        }
    }

    /// Validate persisted provenance before provider launch and reconstruct
    /// only a ceiling whose complete invariant is intact.
    pub fn validate_for_execution(
        &self,
        persisted_state: &str,
        persisted_provider: &str,
        live_capability: Option<&ciacola_agent::CeilingCapability>,
    ) -> Result<Option<ciacola_agent::TurnCeiling>, String> {
        let resend = "resend the turn so current policy can be admitted and persisted";
        if self.version != Self::VERSION {
            return Err(format!(
                "unsupported turn-protection snapshot version {}; {resend}",
                self.version
            ));
        }
        if self.provider != persisted_provider {
            return Err(format!(
                "turn-protection provider '{}' does not match persisted provider '{}'; {resend}",
                self.provider, persisted_provider
            ));
        }
        if self.state.as_str() != persisted_state {
            return Err(format!(
                "turn-protection state '{}' does not match persisted state '{persisted_state}'; {resend}",
                self.state.as_str()
            ));
        }
        match self.state {
            TurnProtectionState::Enforced => {
                let limit = self.configured_limit.ok_or_else(|| {
                    format!("enforced turn protection has no persisted limit; {resend}")
                })?;
                if limit == 0 || limit > i64::MAX as u64 {
                    return Err(format!(
                        "enforced turn protection has invalid persisted limit {limit}; {resend}"
                    ));
                }
                let capability = self.capability.clone().ok_or_else(|| {
                    format!("enforced turn protection has no capability snapshot; {resend}")
                })?;
                if self.unavailable_override.is_some() {
                    return Err(format!(
                        "enforced turn protection unexpectedly contains an unavailable override; {resend}"
                    ));
                }
                if live_capability != Some(&capability) {
                    return Err(format!(
                        "provider turn-ceiling capability changed since admission; {resend}"
                    ));
                }
                Ok(Some(ciacola_agent::TurnCeiling { capability, limit }))
            }
            TurnProtectionState::Unbounded => {
                if self.configured_limit.is_some()
                    || self.capability.is_some()
                    || self.unavailable_override.is_some()
                {
                    return Err(format!(
                        "unbounded turn protection contains ceiling or override data; {resend}"
                    ));
                }
                Ok(None)
            }
            TurnProtectionState::OverrideUnavailable => {
                let limit = self.configured_limit.ok_or_else(|| {
                    format!("unavailable-protection override has no persisted limit; {resend}")
                })?;
                if limit == 0 || limit > i64::MAX as u64 {
                    return Err(format!(
                        "unavailable-protection override has invalid persisted limit {limit}; {resend}"
                    ));
                }
                if self.capability.is_some() {
                    return Err(format!(
                        "unavailable-protection override unexpectedly contains a capability; {resend}"
                    ));
                }
                let audit = self.unavailable_override.as_ref().ok_or_else(|| {
                    format!("unavailable-protection override has no audit; {resend}")
                })?;
                if audit.reason.trim().is_empty() || audit.source.trim().is_empty() {
                    return Err(format!(
                        "unavailable-protection override has an empty audit field; {resend}"
                    ));
                }
                if live_capability.is_some() {
                    return Err(format!(
                        "provider turn-ceiling capability changed since the unavailable override; {resend}"
                    ));
                }
                Ok(None)
            }
            TurnProtectionState::Legacy => Err(format!(
                "legacy turn has no enforceable per-turn protection provenance; {resend}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    struct UntrackedProvider;

    impl ciacola_agent::Provider for UntrackedProvider {
        fn key(&self) -> ciacola_agent::ProviderKey {
            ciacola_agent::ProviderKey::new("untracked")
        }

        fn capabilities(&self) -> ciacola_agent::Capabilities {
            ciacola_agent::Capabilities::none(self.key())
        }

        fn run<'a>(
            &'a self,
            _intent: &'a ciacola_agent::TurnIntent,
            _events: &'a dyn ciacola_agent::TurnEvents,
        ) -> ciacola_agent::BoxFut<'a, Result<ciacola_agent::TurnOutcome, ciacola_agent::AgentError>>
        {
            Box::pin(async { unreachable!("validation never runs a provider") })
        }

        fn owns_process(&self, _ps_line: &str) -> bool {
            false
        }
    }

    fn accounting(provider: ProviderAccounting) -> AdmissionAccounting {
        AdmissionAccounting {
            checked_unix: 100,
            since_unix: 100 - ROLLING_WINDOW_SECS,
            reported_spend_micro_usd: 0,
            providers: [(provider.provider.clone(), provider)].into(),
        }
    }

    #[test]
    fn cached_input_is_not_added_to_total_tokens() {
        let provider = ProviderAccounting {
            provider: "codex".into(),
            reports_token_usage: true,
            tokens_in: 80,
            tokens_out: 20,
            tokens_cached: 50,
            ..Default::default()
        };
        assert_eq!(provider.total_tokens(), 100);
    }

    #[test]
    fn unpriced_provider_requires_a_token_stop_for_automatic_work() {
        let provider = ProviderAccounting {
            provider: "codex".into(),
            reports_token_usage: true,
            ..Default::default()
        };
        let report = Limits::default().evaluate(&accounting(provider.clone()));
        assert_eq!(report.providers[0].state, AdmissionState::Unguarded);

        let limits = Limits {
            providers: [(
                "codex".into(),
                ProviderLimits {
                    daily_stop_tokens: Some(100),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        assert_eq!(
            limits.evaluate(&accounting(provider)).providers[0].state,
            AdmissionState::Ok
        );
    }

    #[test]
    fn unresolved_configured_protection_is_conservatively_non_automatic() {
        let provider = ProviderAccounting {
            provider: "codex".into(),
            reports_token_usage: true,
            ..Default::default()
        };
        let limits = Limits {
            providers: [(
                "codex".into(),
                ProviderLimits {
                    daily_stop_tokens: Some(1_000),
                    per_turn_ceiling: Some(100),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };

        let status = &limits.evaluate(&accounting(provider)).providers[0];
        assert_eq!(status.state, AdmissionState::Ok);
        assert_eq!(status.turn_protection, TurnProtectionStatus::Unavailable);
        assert!(!status.automatic_allowed);
    }

    #[test]
    fn exact_token_boundary_stops_and_missing_usage_fails_closed() {
        let limits = Limits {
            providers: [(
                "codex".into(),
                ProviderLimits {
                    daily_warn_tokens: Some(75),
                    daily_stop_tokens: Some(100),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        let mut provider = ProviderAccounting {
            provider: "codex".into(),
            reports_token_usage: true,
            tokens_in: 80,
            tokens_out: 20,
            ..Default::default()
        };
        assert_eq!(
            limits.evaluate(&accounting(provider.clone())).providers[0].state,
            AdmissionState::Stopped
        );
        provider.tokens_out = 0;
        provider.usage_unreported_turns = 1;
        assert_eq!(
            limits.evaluate(&accounting(provider)).providers[0].state,
            AdmissionState::Unobservable
        );
    }

    #[test]
    fn priced_provider_without_a_configured_stop_keeps_existing_behavior() {
        let provider = ProviderAccounting {
            provider: "claude".into(),
            reports_cost: true,
            reports_token_usage: true,
            ..Default::default()
        };
        assert_eq!(
            Limits::default().evaluate(&accounting(provider)).providers[0].state,
            AdmissionState::Ok
        );
    }

    #[test]
    fn unknown_and_untracked_provider_limits_fail_boot_validation() {
        let configured = |provider: &str| Limits {
            providers: [(
                provider.into(),
                ProviderLimits {
                    daily_stop_tokens: Some(1_000),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        let registry = ciacola_agent::ProviderRegistry::new()
            .with(Arc::new(UntrackedProvider))
            .expect("registry");

        let unknown = configured("typo")
            .validate_providers(&registry)
            .expect_err("unknown key");
        assert!(unknown.to_string().contains("typo"), "{unknown}");

        let untracked = configured("untracked")
            .validate_providers(&registry)
            .expect_err("untracked tokens");
        assert!(
            untracked.to_string().contains("reports token usage"),
            "{untracked}"
        );
    }

    #[test]
    fn invalid_numeric_limits_fail_before_runtime() {
        let invalid = Limits {
            daily_stop_usd: Some(f64::NAN),
            ..Default::default()
        };
        assert!(invalid.validate().is_err());

        let invalid = Limits {
            providers: [(
                "codex".into(),
                ProviderLimits {
                    daily_stop_tokens: Some(i64::MAX as u64 + 1),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        assert!(invalid.validate().is_err());
    }
}
