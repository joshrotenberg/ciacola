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
    ROLLING_WINDOW_SECS,
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
        admission_override: Option<AdmissionOverride>,
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
}

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
        Ok(limits.evaluate(&accounting))
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
        self.providers()
            .get(&definition.provider)
            .map_err(|error| -> FlatError { error.to_string().into() })?;

        let active_agents = load_active_agent_counts(&mut *tx).await?;
        let rows = load_window_rows(&mut *tx, checked_unix - ROLLING_WINDOW_SECS).await?;
        let accounting = self.accounting_from_rows(checked_unix, rows, active_agents)?;
        let report = limits.evaluate(&accounting);
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
        let row: Option<(i64,)> = sqlx::query_as(
            "INSERT INTO turns
                 (agent_id, seq, prompt, state, at_unix, provider, admission_override)
             SELECT ?1,
                    (SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE agent_id = ?1),
                    ?2, 'queued', ?3, ?4, ?5
              WHERE EXISTS (SELECT 1 FROM agents WHERE agent_id = ?1 AND retired = 0)
                AND NOT EXISTS (SELECT 1 FROM turns
                                WHERE agent_id = ?1 AND state IN ('queued', 'running'))
             RETURNING seq",
        )
        .bind(agent_id)
        .bind(prompt)
        .bind(checked_unix)
        .bind(&provider)
        .bind(override_json)
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
            admission_override,
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
                    }
                    "legacy" if row.cost_micro_usd != 0 => {
                        reported_spend_micro_usd = reported_spend_micro_usd
                            .saturating_add(nonnegative(row.cost_micro_usd, "legacy cost")?);
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
}

#[derive(Debug)]
struct WindowRow {
    provider: String,
    state: String,
    cost_micro_usd: i64,
    cost_state: String,
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
                t.state, t.cost_micro_usd, t.cost_state,
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
    }

    impl Provider for FakeProvider {
        fn key(&self) -> ProviderKey {
            ProviderKey::new(self.key)
        }

        fn capabilities(&self) -> Capabilities {
            let mut capabilities = Capabilities::none(self.key());
            capabilities.reports_cost = self.reports_cost;
            capabilities.reports_token_usage = self.reports_usage;
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
            }))
            .and_then(|registry| {
                registry.with(Arc::new(FakeProvider {
                    key: "codex",
                    reports_cost: false,
                    reports_usage: true,
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
            usage: Usage::Reported(TokenUsage {
                input: 80,
                output: 20,
                cached_input: 50,
            }),
            usage_complete: true,
            provider_turns: None,
            elapsed_ms: 1,
            error: None,
        };
        assert!(
            ledger
                .complete_turn(&agent, seq, &exchange)
                .await
                .expect("complete")
        );

        let decision = ledger
            .admit_turn(
                &codex_limits(100),
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
            usage: Usage::Unreported,
            usage_complete: false,
            provider_turns: None,
            elapsed_ms: 1,
            error: Some("failed".into()),
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
            usage: Usage::Reported(TokenUsage {
                input: 70,
                output: 5,
                cached_input: 20,
            }),
            usage_complete: false,
            provider_turns: None,
            elapsed_ms: 1,
            error: Some("timed out".into()),
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
            usage: Usage::Reported(TokenUsage {
                input: 7,
                output: 3,
                cached_input: 5,
            }),
            usage_complete: true,
            provider_turns: None,
            elapsed_ms: 1,
            error: Some("provider failure".into()),
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
