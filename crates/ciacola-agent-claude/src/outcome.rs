//! Mapping `claude-wrapper` results and errors onto the contract's
//! [`TurnOutcome`] and [`AgentError`].

use std::collections::BTreeMap;
use std::time::Duration;

use ciacola_agent::{
    AgentError, Cost, PartialTelemetry, ProviderKey, ResumeId, TokenUsage, TurnFailure,
    TurnOutcome, Usage,
};
use claude_wrapper::QueryResult;

/// A successful `execute_json` call, translated. Every field on
/// [`QueryResult`] is optional-by-construction, so there is nothing
/// here that can fail.
pub(crate) fn from_query_result(result: QueryResult, elapsed: Duration) -> TurnOutcome {
    let resume = (!result.session_id.is_empty())
        .then(|| ResumeId::ProviderAssigned(result.session_id.clone()));

    let cost = match result.cost_usd {
        Some(usd) => Cost::Reported {
            micro_usd: (usd * 1_000_000.0) as u64,
        },
        None => Cost::Unreported,
    };

    // A gap ("this run came back without them") reads differently from
    // "nothing was ever reported": see `Usage`'s own docs. Claude
    // counts tokens, so a missing or empty usage block here is the
    // former, not zero.
    let usage = match result.usage.as_ref() {
        Some(u) if !u.is_empty() => Usage::Reported(TokenUsage {
            input: u.input_tokens.unwrap_or_default(),
            output: u.output_tokens.unwrap_or_default(),
            cached_input: u.cached_input_tokens.unwrap_or_default(),
        }),
        _ => Usage::Unreported,
    };

    TurnOutcome {
        reply: result.result.trim().to_string(),
        resume,
        cost,
        usage,
        provider_turns: result.num_turns,
        elapsed,
        metadata: BTreeMap::new(),
        failure: result
            .is_error
            .then(|| TurnFailure::reported(result.result.trim().to_string())),
    }
}

/// A run that hit `--max-turns` or `--max-budget-usd`, reproducing
/// `ciacola-core::agent`'s `capped()`: the provider ran, at length, and
/// stopped at a ceiling we set. That is data, not a failure to run, so
/// it comes back as `Some(TurnOutcome)` rather than falling through to
/// [`classify_failure`]. `None` means "this was not a cap", and the
/// caller classifies the error normally.
///
/// Cost and usage use the same three-state types every other outcome
/// does. The cap events carry a spend figure but never a token
/// breakdown, so usage reads [`Usage::Unreported`] here rather than a
/// zeroed [`TokenUsage`], and an absent cost reads [`Cost::Unreported`]
/// rather than zero -- the exact "free and unreported look the same"
/// bug issue 53 exists to fix. The old `Exchange`-shaped code flattened
/// both to zero because it had no type that could say otherwise; this
/// one does, so it is not reproduced.
pub(crate) fn capped(
    error: &claude_wrapper::Error,
    elapsed: Duration,
    assigned_resume: Option<&ResumeId>,
) -> Option<TurnOutcome> {
    let (cost_usd, num_turns, session_id, message) = match error {
        claude_wrapper::Error::MaxTurnsExceeded {
            cost_usd,
            num_turns,
            session_id,
            max_turns,
            ..
        } => (
            *cost_usd,
            *num_turns,
            session_id.clone(),
            match max_turns {
                Some(n) => format!("reached maximum number of turns ({n})"),
                None => "reached maximum number of turns".to_string(),
            },
        ),
        claude_wrapper::Error::MaxBudgetExceeded {
            cost_usd,
            num_turns,
            session_id,
            max_usd,
            ..
        } => (
            *cost_usd,
            *num_turns,
            session_id.clone(),
            match max_usd {
                Some(usd) => format!("reached maximum budget (${usd:.2})"),
                None => "reached maximum budget".to_string(),
            },
        ),
        _ => return None,
    };

    // A cap is a terminal provider result, so it proves the assigned
    // session was opened even if this CLI version omitted the id from
    // its error payload.
    let resume = session_id
        .map(ResumeId::ProviderAssigned)
        .or_else(|| assigned_resume.map(|r| ResumeId::ProviderAssigned(r.value().to_string())));

    Some(TurnOutcome {
        reply: String::new(),
        resume,
        cost: match cost_usd {
            Some(usd) => Cost::Reported {
                micro_usd: (usd * 1_000_000.0) as u64,
            },
            None => Cost::Unreported,
        },
        usage: Usage::Unreported,
        provider_turns: num_turns,
        elapsed,
        metadata: BTreeMap::new(),
        failure: Some(TurnFailure::limit(message)),
    })
}

/// Everything that is not a cap: typed, and honest about spend.
///
/// [`AgentError::Timeout`], [`AgentError::Protocol`], and the
/// post-launch shapes of [`AgentError::Other`] carry a
/// [`PartialTelemetry`] with `elapsed` set, because all three can
/// follow a completed process launch: a JSON parse failure means the
/// CLI ran to completion and produced output, and an ordinary nonzero
/// exit means the same. Money and tokens are left `None` on that
/// partial because `claude-wrapper`'s failure variants do not carry the
/// result event's spend fields the way the two cap variants
/// ([`capped`]) do -- there is nothing more honest to report here than
/// "this much time passed".
///
/// No branch that names a variant calls `claude_wrapper::Error`'s own
/// `Display`: `CommandFailed`, `Auth`, `MaxTurnsExceeded`, and
/// `MaxBudgetExceeded` all render the full command line, and `--` plus
/// the prompt is part of that command line. `AgentError`'s `detail`
/// fields are meant for a board or a log line, and the prompt is not
/// argv's to leak there.
///
/// The catch-all arm is the one exception, and it is a deliberate
/// trade rather than an oversight. `claude_wrapper::Error` is
/// `#[non_exhaustive]`, so that arm exists only for variants added
/// upstream after this was written. For those, an opaque message would
/// leave an operator with nothing to debug from, so it keeps `Display`
/// and accepts that a future variant might render argv. Every variant
/// that exists at the pinned revision is named above and does not
/// reach it. If upstream adds a variant that carries a command line,
/// name it explicitly here rather than letting it fall through.
pub(crate) fn classify_failure(
    error: claude_wrapper::Error,
    elapsed: Duration,
    provider: ProviderKey,
) -> AgentError {
    let launched_partial = || {
        PartialTelemetry {
            elapsed: Some(elapsed),
            ..PartialTelemetry::none()
        }
        .into()
    };

    match error {
        claude_wrapper::Error::NotFound => AgentError::NotFound {
            provider,
            detail: "claude binary not found in PATH".to_string(),
        },
        claude_wrapper::Error::Io { message, .. } => {
            // At the pinned revision `exec.rs` produces this variant
            // from four message shapes: "failed to spawn claude" is the
            // only pre-launch one, and "failed to write to claude
            // stdin", "failed to flush claude stdin", and "failed to
            // wait for claude process" all happen once the child is
            // already running. Splitting on the spawn prefix therefore
            // classifies every one of them correctly. The wording is
            // read from the pinned checkout, not guessed, which is also
            // why it is matched rather than parsed: a message that
            // drifts upstream degrades to the post-launch branch, which
            // over-reports telemetry rather than losing it.
            if message.starts_with("failed to spawn claude") {
                AgentError::Launch {
                    provider,
                    detail: message,
                }
            } else {
                AgentError::Other {
                    provider,
                    detail: message,
                    partial: launched_partial(),
                }
            }
        }
        claude_wrapper::Error::Timeout { timeout_seconds } => AgentError::Timeout {
            provider,
            elapsed: Duration::from_secs(timeout_seconds),
            partial: launched_partial(),
        },
        claude_wrapper::Error::Json { message, .. } => AgentError::Protocol {
            provider,
            detail: message,
            partial: launched_partial(),
        },
        claude_wrapper::Error::Auth { kind, message, .. } => AgentError::Other {
            provider,
            detail: format!("auth error ({kind:?}): {message}"),
            partial: launched_partial(),
        },
        claude_wrapper::Error::CommandFailed {
            exit_code,
            stdout,
            stderr,
            ..
        } => AgentError::Other {
            provider,
            detail: command_failed_detail(exit_code, &stdout, &stderr),
            partial: launched_partial(),
        },
        // The wrapper's own BudgetTracker ceiling, checked before the
        // CLI is dispatched: this adapter never attaches one today, so
        // this arm is unreachable in practice, but it is pre-launch by
        // the wrapper's own contract when it does fire.
        claude_wrapper::Error::BudgetExceeded { total_usd, max_usd } => AgentError::Launch {
            provider,
            detail: format!("budget exceeded: ${total_usd:.4} spent, ${max_usd:.4} max"),
        },
        other => AgentError::Other {
            provider,
            detail: other.to_string(),
            partial: launched_partial(),
        },
    }
}

/// A `CommandFailed` message that never carries argv. The variant's own
/// `Display` embeds the full command line (including the prompt);
/// stdout/stderr are the CLI's own diagnostics and safe to surface.
fn command_failed_detail(exit_code: i32, stdout: &str, stderr: &str) -> String {
    let body = if !stderr.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    if body.is_empty() {
        format!("claude exited with code {exit_code}")
    } else {
        format!("claude exited with code {exit_code}: {body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_wrapper::Error as WrapperError;

    fn capped_error(result_json: &str) -> WrapperError {
        WrapperError::from_command_failure(
            "claude -p ...".into(),
            1,
            result_json.into(),
            String::new(),
            None,
        )
    }

    /// The bug issue 53 exists for: a run that worked for minutes and
    /// hit its cap must keep its spend and its session, not flatten to
    /// zero and unresumable.
    #[test]
    fn a_capped_run_keeps_its_spend_and_session_as_reported_not_zero() {
        let e = capped_error(
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,
                "total_cost_usd":1.25,"num_turns":60,"session_id":"sess-1",
                "errors":["Reached maximum number of turns (60)"]}"#,
        );
        let outcome = capped(&e, Duration::from_millis(323_000), None)
            .expect("a cap is an outcome, not a failure to run");

        assert!(!outcome.succeeded());
        assert_eq!(
            outcome.cost,
            Cost::Reported {
                micro_usd: 1_250_000
            }
        );
        assert_eq!(
            outcome.resume,
            Some(ResumeId::ProviderAssigned("sess-1".into()))
        );
        assert_eq!(outcome.provider_turns, Some(60));
        assert_eq!(outcome.elapsed, Duration::from_millis(323_000));
        assert_eq!(
            outcome.failure_message(),
            Some("reached maximum number of turns (60)")
        );
    }

    /// A cap without a reported cost is a gap, not a free run: this is
    /// exactly the distinction `Cost::Unreported` exists to preserve
    /// where the old `Exchange` type could only flatten to zero.
    #[test]
    fn a_capped_run_without_a_reported_cost_is_unreported_not_zero() {
        let e = capped_error(
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,
                "errors":["Reached maximum number of turns (60)"]}"#,
        );
        let assigned = ResumeId::ClientAssigned("assigned-1".into());
        let outcome = capped(&e, Duration::from_millis(10), Some(&assigned)).expect("still capped");

        assert_eq!(outcome.cost, Cost::Unreported);
        assert!(outcome.cost.is_missing());
        assert_eq!(outcome.usage, Usage::Unreported);
        assert_eq!(
            outcome.resume,
            Some(ResumeId::ProviderAssigned("assigned-1".into())),
            "the cap itself proves the preassigned session opened"
        );
    }

    /// A `--max-budget-usd` cap is the same shape as a `--max-turns`
    /// one, from the other constructor.
    #[test]
    fn a_max_budget_cap_is_recognised_and_carries_its_spend() {
        let e = capped_error(
            r#"{"type":"result","subtype":"error_max_budget_usd","is_error":true,
                "errors":["Reached maximum budget ($0.01)"],"num_turns":1,
                "total_cost_usd":0.1273986,"session_id":"s1"}"#,
        );
        let outcome =
            capped(&e, Duration::from_millis(5_000), None).expect("a budget cap is a cap");
        assert_eq!(
            outcome.failure_message(),
            Some("reached maximum budget ($0.01)")
        );
        assert_eq!(
            outcome.resume,
            Some(ResumeId::ProviderAssigned("s1".into()))
        );
    }

    /// Everything else is still a failure to run and must not be
    /// silently downgraded into a capped outcome.
    #[test]
    fn an_ordinary_failure_is_not_treated_as_a_cap() {
        let e = capped_error("command not found");
        assert!(capped(&e, Duration::from_millis(10), None).is_none());
    }

    #[test]
    fn a_json_parse_failure_is_protocol_and_keeps_elapsed_time() {
        let e = WrapperError::Json {
            message: "unexpected end of input".into(),
            source: serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
        };
        let err = classify_failure(e, Duration::from_secs(4), ProviderKey::claude());
        match &err {
            AgentError::Protocol {
                detail, partial, ..
            } => {
                assert!(detail.contains("unexpected end of input"));
                assert_eq!(partial.elapsed, Some(Duration::from_secs(4)));
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    /// `Error::CommandFailed`'s `Display` embeds the full command line,
    /// which is argv, which is where the prompt lives. The mapped
    /// `AgentError::Other` must not carry it, even though it does carry
    /// the CLI's own stderr.
    #[test]
    fn a_command_failure_detail_never_carries_the_command_line() {
        let e = WrapperError::CommandFailed {
            command: "claude --print -- MY-SECRET-PROMPT".into(),
            exit_code: 2,
            stdout: String::new(),
            stderr: "permission denied".into(),
            working_dir: None,
        };
        let err = classify_failure(e, Duration::from_secs(1), ProviderKey::claude());
        match &err {
            AgentError::Other {
                detail, partial, ..
            } => {
                assert!(!detail.contains("MY-SECRET-PROMPT"), "{detail}");
                assert!(detail.contains("permission denied"));
                assert_eq!(partial.elapsed, Some(Duration::from_secs(1)));
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    /// Same leak, different variant: `Error::Auth`'s `Display` also
    /// embeds the command line.
    #[test]
    fn an_auth_failure_detail_never_carries_the_command_line() {
        let e = WrapperError::Auth {
            kind: claude_wrapper::auth::AuthErrorKind::NotAuthenticated,
            command: "claude --print -- MY-SECRET-PROMPT".into(),
            exit_code: 1,
            message: "Not authenticated. Run `claude login`.".into(),
        };
        let err = classify_failure(e, Duration::from_secs(1), ProviderKey::claude());
        match &err {
            AgentError::Other { detail, .. } => {
                assert!(!detail.contains("MY-SECRET-PROMPT"), "{detail}");
                assert!(detail.contains("Not authenticated"));
            }
            other => panic!("expected Other, got {other:?}"),
        }
    }

    /// A spawn failure is pre-launch and must carry no partial
    /// telemetry at all.
    #[test]
    fn a_spawn_failure_is_launch_with_no_partial_telemetry() {
        let e = WrapperError::Io {
            message: "failed to spawn claude: No such file or directory".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "boom"),
            working_dir: None,
        };
        let err = classify_failure(e, Duration::from_secs(1), ProviderKey::claude());
        assert!(matches!(err, AgentError::Launch { .. }));
        assert!(err.partial().is_none());
    }

    #[test]
    fn a_successful_result_reports_usage_and_cost_and_resume() {
        let qr: QueryResult = serde_json::from_str(
            r#"{"result":"hi there","session_id":"sess-7","total_cost_usd":0.02,
                "num_turns":2,"is_error":false,
                "usage":{"input_tokens":10,"output_tokens":5}}"#,
        )
        .unwrap();
        let outcome = from_query_result(qr, Duration::from_millis(500));

        assert_eq!(outcome.reply, "hi there");
        assert_eq!(
            outcome.resume,
            Some(ResumeId::ProviderAssigned("sess-7".into()))
        );
        assert_eq!(outcome.cost, Cost::Reported { micro_usd: 20_000 });
        assert_eq!(
            outcome.usage,
            Usage::Reported(TokenUsage {
                input: 10,
                output: 5,
                cached_input: 0
            })
        );
        assert_eq!(outcome.provider_turns, Some(2));
        assert!(outcome.succeeded());
    }

    /// A result event with no usage block at all is a gap, not zero
    /// usage: Claude counts tokens, so an absent block means this run
    /// came back without them.
    #[test]
    fn a_result_without_a_usage_block_is_unreported_not_zero() {
        let qr: QueryResult = serde_json::from_str(
            r#"{"result":"ok","session_id":"s1","total_cost_usd":0.01,"is_error":false}"#,
        )
        .unwrap();
        let outcome = from_query_result(qr, Duration::from_millis(10));
        assert_eq!(outcome.usage, Usage::Unreported);
        assert!(outcome.usage.is_missing());
    }

    #[test]
    fn a_reported_error_result_carries_its_usage_and_reads_as_failed() {
        let qr: QueryResult = serde_json::from_str(
            r#"{"result":"could not complete the task","session_id":"s1",
                "total_cost_usd":0.05,"is_error":true}"#,
        )
        .unwrap();
        let outcome = from_query_result(qr, Duration::from_millis(10));
        assert!(!outcome.succeeded());
        assert_eq!(
            outcome.failure_message(),
            Some("could not complete the task")
        );
        assert_eq!(outcome.cost, Cost::Reported { micro_usd: 50_000 });
    }
}
