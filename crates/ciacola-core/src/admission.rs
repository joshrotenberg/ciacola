//! The single, transactional boundary for starting new paid work.
//!
//! Admission is linearizable against telemetry already recorded in
//! SQLite: an immediate transaction reads the rolling window, decides,
//! and inserts the queued turn with any supervised audit record before
//! committing. It is intentionally not a reservation for future usage,
//! so work already running can still carry the system past a stop.

use std::collections::BTreeMap;

use sqlx::{Executor, Row, Sqlite};

use crate::agent::{AgentDef, FlatError};
use crate::ledger::Ledger;
use crate::limits::{
    AdmissionAccounting, AdmissionOverride, AdmissionOverrideKind, AdmissionReport, AdmissionState,
    GlobalAdmissionStatus, Limits, ProviderAccounting, ProviderAdmissionStatus,
    ROLLING_WINDOW_SECS, TurnProtectionOverride, TurnProtectionSnapshot, TurnProtectionStatus,
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum AdmissionAuthority<'a> {
    Automatic,
    /// Constructed only by the interactive stdio router. A reason is
    /// mandatory because an exception without intent is not auditable.
    Supervised {
        reason: &'a str,
    },
}

#[derive(Debug, Clone)]
pub(crate) enum AdmissionDecision {
    Admitted {
        seq: i64,
        status: Box<ProviderAdmissionStatus>,
        global: GlobalAdmissionStatus,
        admission_override: Option<Box<AdmissionOverride>>,
        turn_protection_override: Option<TurnProtectionOverride>,
    },
    Busy {
        reason: String,
    },
    OverBudget {
        spent_micro_usd: u64,
        limit_micro_usd: u64,
    },
    OverTokens {
        provider: String,
        used_tokens: u64,
        limit_tokens: u64,
    },
    Unobservable {
        provider: String,
        detail: String,
    },
    Unguarded {
        provider: String,
        detail: String,
    },
    ProtectionUnavailable {
        provider: String,
        detail: String,
    },
}

/// The one guarded queued-turn INSERT: busy fence, retirement fence, and
/// race-safe seq allocation in a single statement. `admit_turn` runs it
/// inside the admission transaction; the test-support enqueue path runs
/// the identical statement so the two cannot drift.
pub(crate) const GUARDED_TURN_INSERT: &str = "INSERT INTO turns
                 (agent_id, seq, prompt, state, at_unix, provider, admission_override,
                  turn_protection_state, turn_protection, failure_kind)
             SELECT ?1,
                    (SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE agent_id = ?1),
                    ?2, 'queued', ?3, ?4, ?5, ?6, ?7, 'none'
              WHERE EXISTS (SELECT 1 FROM agents WHERE agent_id = ?1 AND retired = 0)
                AND NOT EXISTS (SELECT 1 FROM turns
                                WHERE agent_id = ?1 AND state IN ('queued', 'running'))
             RETURNING seq";

impl Ledger {
    pub async fn admission_report(&self, limits: &Limits) -> Result<AdmissionReport, FlatError> {
        self.admission_report_at(limits, crate::time::now_unix())
            .await
    }

    pub async fn admission_report_at(
        &self,
        limits: &Limits,
        checked_unix: i64,
    ) -> Result<AdmissionReport, FlatError> {
        let active_agents = load_active_agent_counts(self.pool()).await?;
        let rows = load_window_rows(self.pool(), checked_unix - ROLLING_WINDOW_SECS).await?;
        let accounting = self.accounting_from_rows(checked_unix, rows, active_agents)?;
        let mut report = limits.evaluate(&accounting);
        self.resolve_turn_protection(&mut report)?;
        Ok(report)
    }

    /// Decide and enqueue under one SQLite writer lock.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn admit_turn(
        &self,
        limits: &Limits,
        authority: AdmissionAuthority<'_>,
        agent_id: &str,
        prompt: &str,
        source: &str,
    ) -> Result<AdmissionDecision, FlatError> {
        let checked_unix = crate::time::now_unix();
        let mut tx = self.pool().begin_with("BEGIN IMMEDIATE").await?;

        let agent: Option<(String, i64)> =
            sqlx::query_as("SELECT def, retired FROM agents WHERE agent_id = ?1")
                .bind(agent_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((definition, retired)) = agent else {
            tx.rollback().await?;
            return Err(format!("no agent '{agent_id}'").into());
        };
        if retired != 0 {
            tx.rollback().await?;
            return Err(format!("agent '{agent_id}' is retired").into());
        }
        let in_flight: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM turns
              WHERE agent_id = ?1 AND state IN ('queued', 'running')",
        )
        .bind(agent_id)
        .fetch_one(&mut *tx)
        .await?;
        if in_flight.0 != 0 {
            tx.rollback().await?;
            return Ok(AdmissionDecision::Busy {
                reason: format!("agent '{agent_id}' already has a turn in flight"),
            });
        }

        let definition: AgentDef = serde_json::from_str(&definition)?;
        let provider = definition.provider.as_str().to_string();
        let selected_provider = self
            .providers()
            .get(&definition.provider)
            .map_err(|error| -> FlatError { error.to_string().into() })?;

        let active_agents = load_active_agent_counts(&mut *tx).await?;
        let rows = load_window_rows(&mut *tx, checked_unix - ROLLING_WINDOW_SECS).await?;
        let accounting = self.accounting_from_rows(checked_unix, rows, active_agents)?;
        let mut report = limits.evaluate(&accounting);
        self.resolve_turn_protection(&mut report)?;
        let status = report
            .providers
            .iter()
            .find(|status| status.accounting.provider == provider)
            .cloned()
            .ok_or_else(|| format!("provider '{provider}' missing from admission report"))?;

        if let Some(stop) = report.global.daily_stop_micro_usd
            && report.global.reported_spend_micro_usd >= stop
        {
            tx.rollback().await?;
            return Ok(AdmissionDecision::OverBudget {
                spent_micro_usd: report.global.reported_spend_micro_usd,
                limit_micro_usd: stop,
            });
        }
        if let Some(stop) = status.daily_stop_tokens
            && status.accounting.total_tokens() >= stop
        {
            tx.rollback().await?;
            return Ok(AdmissionDecision::OverTokens {
                provider,
                used_tokens: status.accounting.total_tokens(),
                limit_tokens: stop,
            });
        }

        // Resolve and freeze the per-turn protection only after every known
        // rolling stop. A supervised reason can acknowledge unavailable
        // protection, but it can never cross a stop whose reached value is
        // known.
        let configured_ceiling = limits.provider(&provider).per_turn_ceiling;
        let ceiling_capability = selected_provider.capabilities().turn_ceiling.clone();
        let (turn_protection, turn_protection_override) = match (
            configured_ceiling,
            ceiling_capability,
        ) {
            (None, _) => (TurnProtectionSnapshot::unbounded(provider.clone()), None),
            (Some(limit), Some(capability)) => (
                TurnProtectionSnapshot::enforced(provider.clone(), limit, capability),
                None,
            ),
            (Some(limit), None) => match authority {
                AdmissionAuthority::Automatic => {
                    tx.rollback().await?;
                    return Ok(AdmissionDecision::ProtectionUnavailable {
                        provider,
                        detail: format!(
                            "per-turn ceiling {limit} is configured, but the provider cannot enforce it; automatic work is refused"
                        ),
                    });
                }
                AdmissionAuthority::Supervised { reason } => {
                    let reason = reason.trim();
                    if reason.is_empty() {
                        tx.rollback().await?;
                        return Err(
                            "a supervised unavailable-protection override requires a non-empty reason"
                                .into(),
                        );
                    }
                    let audit = TurnProtectionOverride {
                        reason: reason.to_string(),
                        source: source.to_string(),
                        checked_unix,
                    };
                    (
                        TurnProtectionSnapshot::override_unavailable(
                            provider.clone(),
                            limit,
                            audit.clone(),
                        ),
                        Some(audit),
                    )
                }
            },
        };

        let admission_override = match status.state {
            AdmissionState::Unobservable | AdmissionState::Unguarded => match authority {
                AdmissionAuthority::Automatic => {
                    tx.rollback().await?;
                    let detail = status
                        .detail
                        .clone()
                        .unwrap_or_else(|| format!("{} provider admission", status.state.as_str()));
                    return Ok(if status.state == AdmissionState::Unobservable {
                        AdmissionDecision::Unobservable { provider, detail }
                    } else {
                        AdmissionDecision::Unguarded { provider, detail }
                    });
                }
                AdmissionAuthority::Supervised { reason } => {
                    let reason = reason.trim();
                    if reason.is_empty() {
                        tx.rollback().await?;
                        return Err(
                            "a supervised admission override requires a non-empty reason".into(),
                        );
                    }
                    Some(AdmissionOverride {
                        kind: AdmissionOverrideKind::from_state(status.state)
                            .expect("overrideable state"),
                        reason: reason.to_string(),
                        source: source.to_string(),
                        provider: provider.clone(),
                        checked_unix,
                        reported_spend_micro_usd: report.global.reported_spend_micro_usd,
                        daily_stop_micro_usd: report.global.daily_stop_micro_usd,
                        reported_tokens: status.accounting.total_tokens(),
                        daily_stop_tokens: status.daily_stop_tokens,
                        cost_gaps: report.global.cost_gaps,
                        usage_gaps: status.accounting.usage_gaps(),
                    })
                }
            },
            AdmissionState::Stopped => {
                tx.rollback().await?;
                return Err(
                    "admission status stopped without a matching configured threshold".into(),
                );
            }
            AdmissionState::Ok | AdmissionState::Warning => None,
        };

        let override_json = admission_override
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let turn_protection_json = serde_json::to_string(&turn_protection)?;
        let row: Option<(i64,)> = sqlx::query_as(GUARDED_TURN_INSERT)
            .bind(agent_id)
            .bind(prompt)
            .bind(checked_unix)
            .bind(&provider)
            .bind(override_json)
            .bind(turn_protection.state.as_str())
            .bind(turn_protection_json)
            .fetch_optional(&mut *tx)
            .await?;
        let Some((seq,)) = row else {
            tx.rollback().await?;
            return Ok(AdmissionDecision::Busy {
                reason: format!("agent '{agent_id}' already has a turn in flight"),
            });
        };
        tx.commit().await?;
        Ok(AdmissionDecision::Admitted {
            seq,
            status: Box::new(status),
            global: report.global,
            admission_override: admission_override.map(Box::new),
            turn_protection_override,
        })
    }

    fn accounting_from_rows(
        &self,
        checked_unix: i64,
        rows: Vec<WindowRow>,
        mut active_agents: BTreeMap<String, u64>,
    ) -> Result<AdmissionAccounting, FlatError> {
        let mut providers = BTreeMap::new();
        for key in self.providers().keys() {
            let capability = self
                .providers()
                .get(&ciacola_agent::ProviderKey::new(key.clone()))?
                .capabilities();
            let active_agents = active_agents.remove(&key).unwrap_or_default();
            providers.insert(
                key.clone(),
                ProviderAccounting {
                    provider: key,
                    reports_cost: capability.reports_cost,
                    reports_token_usage: capability.reports_token_usage,
                    active_agents,
                    ..Default::default()
                },
            );
        }
        for (provider, active_agents) in active_agents {
            providers.insert(
                provider.clone(),
                ProviderAccounting {
                    provider,
                    active_agents,
                    ..Default::default()
                },
            );
        }

        let mut reported_spend_micro_usd = 0_u64;
        for row in rows {
            let provider =
                providers
                    .entry(row.provider.clone())
                    .or_insert_with(|| ProviderAccounting {
                        provider: row.provider.clone(),
                        ..Default::default()
                    });
            let attempted = row.elapsed_state != "not_attempted";
            let terminal = matches!(row.state.as_str(), "ok" | "failed" | "killed");

            if terminal {
                match row.cost_state.as_str() {
                    "reported" => {
                        reported_spend_micro_usd = reported_spend_micro_usd
                            .saturating_add(nonnegative(row.cost_micro_usd, "cost")?);
                        if attempted {
                            if row.cost_complete {
                                provider.cost_complete_turns += 1;
                            } else {
                                provider.cost_incomplete_turns += 1;
                            }
                        }
                    }
                    "legacy" if row.cost_micro_usd != 0 => {
                        reported_spend_micro_usd = reported_spend_micro_usd
                            .saturating_add(nonnegative(row.cost_micro_usd, "legacy cost")?);
                        if attempted {
                            provider.cost_legacy_unknown_turns += 1;
                        }
                    }
                    "unreported" if attempted => provider.cost_unreported_turns += 1,
                    "not_priced" if attempted => provider.cost_not_priced_turns += 1,
                    "legacy" if attempted => provider.cost_legacy_unknown_turns += 1,
                    _ => {}
                }
            }

            let legacy_has_tokens = row.usage_state == "legacy"
                && (row.tokens_in != 0 || row.tokens_out != 0 || row.tokens_cached != 0);
            let has_measured_tokens = row.usage_state == "reported" || legacy_has_tokens;
            if has_measured_tokens {
                provider.tokens_in = provider
                    .tokens_in
                    .saturating_add(nonnegative(row.tokens_in, "tokens_in")?);
                provider.tokens_out = provider
                    .tokens_out
                    .saturating_add(nonnegative(row.tokens_out, "tokens_out")?);
                provider.tokens_cached = provider
                    .tokens_cached
                    .saturating_add(nonnegative(row.tokens_cached, "tokens_cached")?);
            }

            if row.state == "running" {
                if row.usage_state == "reported" && !row.usage_complete {
                    provider.running_partial_turns += 1;
                }
                continue;
            }
            if !terminal || !attempted {
                if terminal && row.usage_state == "reported" && row.usage_complete {
                    provider.usage_complete_turns += 1;
                }
                continue;
            }
            match row.usage_state.as_str() {
                "reported" if row.usage_complete => provider.usage_complete_turns += 1,
                "reported" => provider.usage_incomplete_turns += 1,
                "unreported" => provider.usage_unreported_turns += 1,
                "not_tracked" => provider.usage_not_tracked_turns += 1,
                // Before usage provenance existed, a positive bucket was
                // still an observed terminal value. Zero is the ambiguous
                // legacy case: it may mean a measured zero or no report.
                "legacy" if legacy_has_tokens => provider.usage_complete_turns += 1,
                "legacy" => provider.usage_legacy_unknown_turns += 1,
                _ => provider.usage_incomplete_turns += 1,
            }
        }

        Ok(AdmissionAccounting {
            checked_unix,
            since_unix: checked_unix - ROLLING_WINDOW_SECS,
            reported_spend_micro_usd,
            providers,
        })
    }

    fn resolve_turn_protection(&self, report: &mut AdmissionReport) -> Result<(), FlatError> {
        for status in &mut report.providers {
            let key = ciacola_agent::ProviderKey::new(status.accounting.provider.clone());
            let capability = self
                .providers()
                .get(&key)
                .ok()
                .and_then(|provider| provider.capabilities().turn_ceiling.clone());
            if status.per_turn_ceiling.is_none() {
                status.turn_protection = TurnProtectionStatus::Unbounded;
                // Showing available semantics even while unbounded makes the
                // absence of policy legible without pretending the provider
                // lacks a meter.
                status.turn_ceiling_capability = capability;
                status.automatic_allowed = status.state.automatic_allowed();
                continue;
            }
            status.turn_protection = if capability.is_some() {
                TurnProtectionStatus::Enforced
            } else {
                TurnProtectionStatus::Unavailable
            };
            status.turn_ceiling_capability = capability;
            status.automatic_allowed = status.state.automatic_allowed()
                && status.turn_protection == TurnProtectionStatus::Enforced;
            if status.turn_protection == TurnProtectionStatus::Unavailable {
                status.detail.get_or_insert_with(|| {
                    "a per-turn ceiling is configured but this provider cannot enforce it".into()
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct WindowRow {
    provider: String,
    state: String,
    cost_micro_usd: i64,
    cost_state: String,
    cost_complete: bool,
    tokens_in: i64,
    tokens_out: i64,
    tokens_cached: i64,
    usage_state: String,
    usage_complete: bool,
    elapsed_state: String,
}

async fn load_window_rows<'e, E>(executor: E, since_unix: i64) -> Result<Vec<WindowRow>, FlatError>
where
    E: Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query(
        "WITH window_turns AS (
             SELECT * FROM turns
              WHERE state IN ('ok', 'failed', 'killed')
                AND settled_unix >= ?1
             UNION ALL
             SELECT * FROM turns
              WHERE state IN ('ok', 'failed', 'killed')
                AND settled_unix IS NULL AND at_unix >= ?1
             UNION ALL
             SELECT * FROM turns WHERE state = 'running'
         )
         SELECT COALESCE(NULLIF(t.provider, ''),
                         NULLIF(json_extract(a.def, '$.provider'), ''),
                         'claude') AS provider,
                t.state, t.cost_micro_usd, t.cost_state, t.cost_complete,
                t.tokens_in, t.tokens_out, t.tokens_cached, t.usage_state,
                t.usage_complete, t.elapsed_state
           FROM window_turns t
           LEFT JOIN agents a ON a.agent_id = t.agent_id",
    )
    .bind(since_unix)
    .fetch_all(executor)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(WindowRow {
                provider: row.try_get("provider")?,
                state: row.try_get("state")?,
                cost_micro_usd: row.try_get("cost_micro_usd")?,
                cost_state: row.try_get("cost_state")?,
                cost_complete: row.try_get::<i64, _>("cost_complete")? != 0,
                tokens_in: row.try_get("tokens_in")?,
                tokens_out: row.try_get("tokens_out")?,
                tokens_cached: row.try_get("tokens_cached")?,
                usage_state: row.try_get("usage_state")?,
                usage_complete: row.try_get::<i64, _>("usage_complete")? != 0,
                elapsed_state: row.try_get("elapsed_state")?,
            })
        })
        .collect()
}

async fn load_active_agent_counts<'e, E>(executor: E) -> Result<BTreeMap<String, u64>, FlatError>
where
    E: Executor<'e, Database = Sqlite>,
{
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT COALESCE(NULLIF(json_extract(def, '$.provider'), ''), 'claude'), COUNT(*)
           FROM agents
          WHERE retired = 0
          GROUP BY 1",
    )
    .fetch_all(executor)
    .await?;
    rows.into_iter()
        .map(|(provider, count)| {
            let count = u64::try_from(count)
                .map_err(|_| -> FlatError { "negative active-agent count".into() })?;
            Ok((provider, count))
        })
        .collect()
}

fn nonnegative(value: i64, field: &str) -> Result<u64, FlatError> {
    u64::try_from(value)
        .map_err(|_| format!("negative {field} in the ledger would make admission unsafe").into())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ciacola_agent::{
        AgentError, BoxFut, Capabilities, Cost, Provider, ProviderKey, ProviderRegistry,
        TokenUsage, TurnEvents, TurnIntent, TurnOutcome, Usage,
    };

    use super::*;
    use crate::agent::Exchange;

    struct FakeProvider {
        key: &'static str,
        reports_cost: bool,
        reports_usage: bool,
        turn_ceiling: Option<ciacola_agent::CeilingCapability>,
    }

    impl Provider for FakeProvider {
        fn key(&self) -> ProviderKey {
            ProviderKey::new(self.key)
        }

        fn capabilities(&self) -> Capabilities {
            let mut capabilities = Capabilities::none(self.key());
            capabilities.reports_cost = self.reports_cost;
            capabilities.reports_token_usage = self.reports_usage;
            capabilities.turn_ceiling = self.turn_ceiling.clone();
            capabilities
        }

        fn run<'a>(
            &'a self,
            _intent: &'a TurnIntent,
            _events: &'a dyn TurnEvents,
        ) -> BoxFut<'a, Result<TurnOutcome, AgentError>> {
            Box::pin(async { unreachable!("admission tests do not run providers") })
        }

        fn owns_process(&self, _ps_line: &str) -> bool {
            false
        }
    }

    fn providers() -> ProviderRegistry {
        ProviderRegistry::new()
            .with(Arc::new(FakeProvider {
                key: "claude",
                reports_cost: true,
                reports_usage: true,
                turn_ceiling: None,
            }))
            .and_then(|registry| {
                registry.with(Arc::new(FakeProvider {
                    key: "codex",
                    reports_cost: false,
                    reports_usage: true,
                    turn_ceiling: None,
                }))
            })
            .expect("providers")
    }

    async fn ledger() -> Ledger {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        Ledger::setup(pool)
            .await
            .expect("ledger")
            .with_providers(providers())
    }

    fn ceiling_capability(meter: &str) -> ciacola_agent::CeilingCapability {
        ciacola_agent::CeilingCapability {
            meter: ciacola_agent::MeterId::new(meter),
            granularity: ciacola_agent::EnforcementGranularity::ProviderResponseBoundary,
            cache_treatment: ciacola_agent::CacheTreatment::ProviderDefinedWithExcludedFallback,
        }
    }

    async fn ledger_with_codex_ceiling(
        capability: Option<ciacola_agent::CeilingCapability>,
    ) -> Ledger {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("pool");
        let registry = ProviderRegistry::new()
            .with(Arc::new(FakeProvider {
                key: "codex",
                reports_cost: false,
                reports_usage: true,
                turn_ceiling: capability,
            }))
            .expect("provider");
        Ledger::setup(pool)
            .await
            .expect("ledger")
            .with_providers(registry)
    }

    fn per_turn_limits(ceiling: u64) -> Limits {
        Limits {
            providers: [(
                "codex".into(),
                crate::limits::ProviderLimits {
                    daily_stop_tokens: Some(1_000_000),
                    per_turn_ceiling: Some(ceiling),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        }
    }

    fn codex_limits(stop: u64) -> Limits {
        Limits {
            providers: [(
                "codex".into(),
                crate::limits::ProviderLimits {
                    daily_stop_tokens: Some(stop),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn per_turn_protection_is_resolved_and_persisted_in_the_admission_transaction() {
        let capability = ceiling_capability("codex_rollout_units_v1");
        let ledger = ledger_with_codex_ceiling(Some(capability.clone())).await;
        let agent = ledger
            .create_agent(&AgentDef::new("codex", "s").provider("codex"), None)
            .await
            .expect("agent");
        let decision = ledger
            .admit_turn(
                &per_turn_limits(321),
                AdmissionAuthority::Automatic,
                &agent,
                "work",
                "test",
            )
            .await
            .expect("decision");
        let AdmissionDecision::Admitted { seq, status, .. } = decision else {
            panic!("expected admission")
        };
        assert_eq!(status.turn_protection, TurnProtectionStatus::Enforced);
        assert_eq!(status.turn_ceiling_capability, Some(capability.clone()));
        assert!(status.automatic_allowed);

        let row = ledger.get_turn(&agent, seq).await.unwrap().unwrap();
        assert_eq!(row.turn_protection_state, "enforced");
        assert_eq!(row.failure_kind, "none");
        let snapshot: TurnProtectionSnapshot =
            serde_json::from_str(row.turn_protection.as_deref().expect("snapshot")).unwrap();
        assert_eq!(snapshot.configured_limit, Some(321));
        assert_eq!(snapshot.capability, Some(capability));
        assert!(snapshot.unavailable_override.is_none());
    }

    #[tokio::test]
    async fn unavailable_protection_blocks_automatic_without_a_row_and_supervised_audits_both_gaps()
    {
        let ledger = ledger_with_codex_ceiling(None).await;
        let agent = ledger
            .create_agent(&AgentDef::new("codex", "s").provider("codex"), None)
            .await
            .expect("agent");
        let automatic = ledger
            .admit_turn(
                &per_turn_limits(123),
                AdmissionAuthority::Automatic,
                &agent,
                "automatic",
                "schedule",
            )
            .await
            .expect("decision");
        assert!(matches!(
            automatic,
            AdmissionDecision::ProtectionUnavailable { .. }
        ));
        assert!(ledger.conversation(&agent).await.unwrap().is_empty());

        // Remove the rolling token stop too: this one supervised request
        // acknowledges both the old rolling unguarded state and the separate
        // unavailable per-turn protection, each in its own durable audit.
        let limits = Limits {
            providers: [(
                "codex".into(),
                crate::limits::ProviderLimits {
                    per_turn_ceiling: Some(123),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        };
        let supervised = ledger
            .admit_turn(
                &limits,
                AdmissionAuthority::Supervised {
                    reason: "operator accepted both gaps",
                },
                &agent,
                "supervised",
                "send_supervised",
            )
            .await
            .expect("decision");
        let AdmissionDecision::Admitted {
            seq,
            admission_override: Some(rolling),
            turn_protection_override: Some(protection),
            ..
        } = supervised
        else {
            panic!("expected both audits")
        };
        assert_eq!(rolling.reason, "operator accepted both gaps");
        assert_eq!(protection.reason, "operator accepted both gaps");
        let row = ledger.get_turn(&agent, seq).await.unwrap().unwrap();
        assert_eq!(row.turn_protection_state, "override_unavailable");
        let snapshot: TurnProtectionSnapshot =
            serde_json::from_str(row.turn_protection.as_deref().expect("snapshot")).unwrap();
        assert_eq!(snapshot.unavailable_override, Some(protection));
        assert!(row.admission_override.is_some());
    }

    #[tokio::test]
    async fn no_configured_ceiling_is_explicitly_unbounded() {
        let ledger = ledger().await;
        let agent = ledger
            .create_agent(&AgentDef::new("claude", "s"), None)
            .await
            .expect("agent");
        let decision = ledger
            .admit_turn(
                &Limits::default(),
                AdmissionAuthority::Automatic,
                &agent,
                "work",
                "test",
            )
            .await
            .expect("decision");
        let AdmissionDecision::Admitted { seq, status, .. } = decision else {
            panic!("expected admission")
        };
        assert_eq!(status.turn_protection, TurnProtectionStatus::Unbounded);
        let row = ledger.get_turn(&agent, seq).await.unwrap().unwrap();
        assert_eq!(row.turn_protection_state, "unbounded");
        let snapshot: TurnProtectionSnapshot =
            serde_json::from_str(row.turn_protection.as_deref().expect("snapshot")).unwrap();
        assert_eq!(
            snapshot.state,
            crate::limits::TurnProtectionState::Unbounded
        );
    }

    #[tokio::test]
    async fn unpriced_automatic_is_blocked_but_supervised_reason_is_durable() {
        let ledger = ledger().await;
        let agent = ledger
            .create_agent(&AgentDef::new("codex", "s").provider("codex"), None)
            .await
            .expect("agent");
        let automatic = ledger
            .admit_turn(
                &Limits::default(),
                AdmissionAuthority::Automatic,
                &agent,
                "work",
                "test",
            )
            .await
            .expect("decision");
        assert!(matches!(automatic, AdmissionDecision::Unguarded { .. }));

        let supervised = ledger
            .admit_turn(
                &Limits::default(),
                AdmissionAuthority::Supervised {
                    reason: "weekend dogfood",
                },
                &agent,
                "work",
                "send_supervised",
            )
            .await
            .expect("decision");
        let AdmissionDecision::Admitted {
            seq,
            admission_override: Some(audit),
            ..
        } = supervised
        else {
            panic!("expected audited admission")
        };
        assert_eq!(audit.reason, "weekend dogfood");
        let row = ledger
            .get_turn(&agent, seq)
            .await
            .expect("get")
            .expect("row");
        assert!(
            row.admission_override
                .as_deref()
                .is_some_and(|json| json.contains("weekend dogfood"))
        );
    }

    #[tokio::test]
    async fn exact_token_stop_cannot_be_overridden_and_counts_cached_once() {
        let ledger = ledger().await;
        let agent = ledger
            .create_agent(&AgentDef::new("codex", "s").provider("codex"), None)
            .await
            .expect("agent");
        let seq = ledger.enqueue_turn(&agent, "first").await.expect("turn");
        assert!(ledger.claim_turn(&agent, seq).await.expect("claim"));
        let exchange = Exchange {
            reply: "done".into(),
            session: None,
            cost: Cost::NotPriced,
            cost_complete: false,
            usage: Usage::Reported(TokenUsage {
                input: 80,
                output: 20,
                cached_input: 50,
            }),
            usage_complete: true,
            provider_turns: None,
            elapsed_ms: 1,
            error: None,
            failure_kind: None,
        };
        assert!(
            ledger
                .complete_turn(&agent, seq, &exchange)
                .await
                .expect("complete")
        );

        let mut limits = codex_limits(100);
        limits
            .providers
            .get_mut("codex")
            .expect("codex limits")
            .per_turn_ceiling = Some(7);
        let decision = ledger
            .admit_turn(
                &limits,
                AdmissionAuthority::Supervised {
                    reason: "cannot bypass",
                },
                &agent,
                "second",
                "test",
            )
            .await
            .expect("decision");
        assert!(matches!(
            decision,
            AdmissionDecision::OverTokens {
                used_tokens: 100,
                limit_tokens: 100,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn terminal_missing_usage_fails_closed_but_running_without_a_snapshot_does_not() {
        let missing_ledger = ledger().await;
        let missing = missing_ledger
            .create_agent(&AgentDef::new("missing", "s").provider("codex"), None)
            .await
            .expect("agent");
        let seq = missing_ledger
            .enqueue_turn(&missing, "first")
            .await
            .expect("turn");
        assert!(
            missing_ledger
                .claim_turn(&missing, seq)
                .await
                .expect("claim")
        );
        let exchange = Exchange {
            reply: String::new(),
            session: None,
            cost: Cost::NotPriced,
            cost_complete: false,
            usage: Usage::Unreported,
            usage_complete: false,
            provider_turns: None,
            elapsed_ms: 1,
            error: Some("failed".into()),
            failure_kind: Some(ciacola_agent::FailureKind::Reported),
        };
        assert!(
            missing_ledger
                .fail_exchange(&missing, seq, "failed", "failed", &exchange)
                .await
                .expect("settle")
        );
        assert!(matches!(
            missing_ledger
                .admit_turn(
                    &codex_limits(1_000),
                    AdmissionAuthority::Automatic,
                    &missing,
                    "again",
                    "test",
                )
                .await
                .expect("decision"),
            AdmissionDecision::Unobservable { .. }
        ));

        let running_ledger = ledger().await;
        let running = running_ledger
            .create_agent(&AgentDef::new("running", "s").provider("codex"), None)
            .await
            .expect("agent");
        let seq = running_ledger
            .enqueue_turn(&running, "running")
            .await
            .expect("turn");
        assert!(
            running_ledger
                .claim_turn(&running, seq)
                .await
                .expect("claim")
        );
        let report = running_ledger
            .admission_report(&codex_limits(1_000))
            .await
            .expect("report");
        let status = report
            .providers
            .iter()
            .find(|status| status.accounting.provider == "codex")
            .expect("codex");
        assert_eq!(status.state, AdmissionState::Ok);
        assert_eq!(status.accounting.running_partial_turns, 0);
    }

    #[tokio::test]
    async fn failed_partial_usage_counts_as_a_lower_bound_and_fails_closed() {
        let ledger = ledger().await;
        let agent = ledger
            .create_agent(&AgentDef::new("partial", "s").provider("codex"), None)
            .await
            .expect("agent");
        let seq = ledger.enqueue_turn(&agent, "first").await.expect("turn");
        assert!(ledger.claim_turn(&agent, seq).await.expect("claim"));
        let exchange = Exchange {
            reply: String::new(),
            session: None,
            cost: Cost::NotPriced,
            cost_complete: false,
            usage: Usage::Reported(TokenUsage {
                input: 70,
                output: 5,
                cached_input: 20,
            }),
            usage_complete: false,
            provider_turns: None,
            elapsed_ms: 1,
            error: Some("timed out".into()),
            failure_kind: Some(ciacola_agent::FailureKind::Reported),
        };
        assert!(
            ledger
                .fail_exchange(&agent, seq, "failed", "timed out", &exchange)
                .await
                .expect("settle")
        );

        let report = ledger
            .admission_report(&codex_limits(1_000))
            .await
            .expect("report");
        let codex = report
            .providers
            .iter()
            .find(|status| status.accounting.provider == "codex")
            .expect("codex");
        assert_eq!(codex.accounting.total_tokens(), 75);
        assert_eq!(codex.accounting.usage_incomplete_turns, 1);
        assert_eq!(codex.state, AdmissionState::Unobservable);
    }

    #[tokio::test]
    async fn failed_reported_usage_counts_and_settlement_cutoff_is_inclusive() {
        let ledger = ledger().await;
        let agent = ledger
            .create_agent(&AgentDef::new("codex", "s").provider("codex"), None)
            .await
            .expect("agent");
        let seq = ledger.enqueue_turn(&agent, "failed").await.expect("turn");
        assert!(ledger.claim_turn(&agent, seq).await.expect("claim"));
        let exchange = Exchange {
            reply: String::new(),
            session: None,
            cost: Cost::NotPriced,
            cost_complete: false,
            usage: Usage::Reported(TokenUsage {
                input: 7,
                output: 3,
                cached_input: 5,
            }),
            usage_complete: true,
            provider_turns: None,
            elapsed_ms: 1,
            error: Some("provider failure".into()),
            failure_kind: Some(ciacola_agent::FailureKind::Reported),
        };
        assert!(
            ledger
                .fail_exchange(&agent, seq, "failed", "provider failure", &exchange)
                .await
                .expect("settle")
        );

        let checked = crate::time::now_unix();
        let cutoff = checked - ROLLING_WINDOW_SECS;
        sqlx::query("UPDATE turns SET settled_unix = ?3 WHERE agent_id = ?1 AND seq = ?2")
            .bind(&agent)
            .bind(seq)
            .bind(cutoff - 1)
            .execute(ledger.pool())
            .await
            .expect("age row");
        let before = ledger
            .admission_report_at(&codex_limits(10), checked)
            .await
            .expect("report");
        let codex = before
            .providers
            .iter()
            .find(|status| status.accounting.provider == "codex")
            .expect("codex");
        assert_eq!(codex.accounting.total_tokens(), 0);
        assert_eq!(codex.state, AdmissionState::Ok);

        sqlx::query("UPDATE turns SET settled_unix = ?3 WHERE agent_id = ?1 AND seq = ?2")
            .bind(&agent)
            .bind(seq)
            .bind(cutoff)
            .execute(ledger.pool())
            .await
            .expect("move to boundary");
        let at = ledger
            .admission_report_at(&codex_limits(10), checked)
            .await
            .expect("report");
        let codex = at
            .providers
            .iter()
            .find(|status| status.accounting.provider == "codex")
            .expect("codex");
        assert_eq!(codex.accounting.total_tokens(), 10);
        assert_eq!(codex.accounting.usage_complete_turns, 1);
        assert_eq!(codex.state, AdmissionState::Stopped);
    }

    #[tokio::test]
    async fn positive_legacy_usage_is_observed_but_legacy_zero_is_ambiguous() {
        let ledger = ledger().await;
        let agent = ledger
            .create_agent(&AgentDef::new("codex", "s").provider("codex"), None)
            .await
            .expect("agent");
        let seq = ledger.enqueue_turn(&agent, "legacy").await.expect("turn");
        assert!(ledger.claim_turn(&agent, seq).await.expect("claim"));
        assert!(
            ledger
                .abort_claimed_turn(&agent, seq, "fixture")
                .await
                .expect("settle")
        );
        sqlx::query(
            "UPDATE turns
                SET tokens_in = 7, tokens_out = 3, usage_state = 'legacy', usage_complete = 0,
                    elapsed_state = 'measured'
              WHERE agent_id = ?1 AND seq = ?2",
        )
        .bind(&agent)
        .bind(seq)
        .execute(ledger.pool())
        .await
        .expect("make legacy row");

        let measured = ledger
            .admission_report(&codex_limits(100))
            .await
            .expect("report");
        let codex = measured
            .providers
            .iter()
            .find(|status| status.accounting.provider == "codex")
            .expect("codex");
        assert_eq!(codex.accounting.total_tokens(), 10);
        assert_eq!(codex.accounting.usage_complete_turns, 1);
        assert_eq!(codex.accounting.usage_gaps(), 0);
        assert_eq!(codex.state, AdmissionState::Ok);

        sqlx::query(
            "UPDATE turns SET tokens_in = 0, tokens_out = 0
              WHERE agent_id = ?1 AND seq = ?2",
        )
        .bind(&agent)
        .bind(seq)
        .execute(ledger.pool())
        .await
        .expect("make ambiguous zero");
        let ambiguous = ledger
            .admission_report(&codex_limits(100))
            .await
            .expect("report");
        let codex = ambiguous
            .providers
            .iter()
            .find(|status| status.accounting.provider == "codex")
            .expect("codex");
        assert_eq!(codex.accounting.usage_legacy_unknown_turns, 1);
        assert_eq!(codex.state, AdmissionState::Unobservable);
    }

    #[tokio::test]
    async fn concurrent_admissions_linearize_to_one_queued_turn() {
        use sqlx::sqlite::SqlitePoolOptions;

        let path = std::env::temp_dir().join(format!(
            "ciacola-admission-{}-{}.db",
            std::process::id(),
            ulid::Ulid::new()
        ));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("pool");
        let ledger = Ledger::setup(pool.clone())
            .await
            .expect("ledger")
            .with_providers(providers());
        let agent = ledger
            .create_agent(&AgentDef::new("claude", "s"), None)
            .await
            .expect("agent");

        let first = ledger.clone();
        let second = ledger.clone();
        let limits = Limits::default();
        let (a, b) = tokio::join!(
            first.admit_turn(
                &limits,
                AdmissionAuthority::Automatic,
                &agent,
                "one",
                "test",
            ),
            second.admit_turn(
                &limits,
                AdmissionAuthority::Automatic,
                &agent,
                "two",
                "test",
            )
        );
        let decisions = [a.expect("first"), b.expect("second")];
        assert_eq!(
            decisions
                .iter()
                .filter(|decision| matches!(decision, AdmissionDecision::Admitted { .. }))
                .count(),
            1
        );
        assert_eq!(
            decisions
                .iter()
                .filter(|decision| matches!(decision, AdmissionDecision::Busy { .. }))
                .count(),
            1
        );

        drop(ledger);
        pool.close().await;
        std::fs::remove_file(path).expect("remove test database");
    }
}
