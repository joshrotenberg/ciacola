//! Mapping `claude-wrapper` results and errors onto the contract's
//! [`TurnOutcome`] and [`AgentError`].

use std::collections::BTreeMap;
use std::time::Duration;

use ciacola_agent::{
    AgentError, Cost, PartialTelemetry, ProviderKey, ResumeId, TokenUsage, TurnFailure,
    TurnOutcome, Usage,
};
use claude_wrapper::{QueryResult, streaming::StreamEvent};

/// Token usage on one complete assistant message in Claude's JSONL
/// stream.
///
/// Assistant events are provider-internal turns, not cumulative ciacola
/// totals. The adapter deduplicates them by message id and adds them to
/// the cumulative snapshot it publishes through `TurnEvents`.
pub(crate) fn assistant_usage(event: &StreamEvent) -> Option<(Option<String>, TokenUsage)> {
    if event.event_type() != Some("assistant") {
        return None;
    }
    let message = event.data.get("message")?;
    let usage = message.get("usage")?;
    let field = |name: &str| usage.get(name).and_then(serde_json::Value::as_u64);
    let reported = [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cached_input_tokens",
        "cache_creation_input_tokens",
        "cache_write_input_tokens",
        "reasoning_output_tokens",
    ]
    .iter()
    .any(|name| field(name).is_some());
    if !reported {
        return None;
    }
    let message_id = message
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let cached_input = field("cache_read_input_tokens")
        .or_else(|| field("cached_input_tokens"))
        .unwrap_or_default();
    let cache_write = field("cache_creation_input_tokens")
        .or_else(|| field("cache_write_input_tokens"))
        .unwrap_or_default();
    Some((
        message_id,
        TokenUsage {
            // Claude splits non-cached, cache-read, and cache-write
            // input into separate buckets. The portable contract keeps
            // total input with cache-read as a subset.
            input: field("input_tokens")
                .unwrap_or_default()
                .saturating_add(cached_input)
                .saturating_add(cache_write),
            output: field("output_tokens")
                .unwrap_or_default()
                .saturating_add(field("reasoning_output_tokens").unwrap_or_default()),
            cached_input,
        },
    ))
}

/// Add a provider-internal assistant message to a cumulative snapshot.
pub(crate) fn add_usage(total: TokenUsage, next: TokenUsage) -> TokenUsage {
    TokenUsage {
        input: total.input.saturating_add(next.input),
        output: total.output.saturating_add(next.output),
        cached_input: total.cached_input.saturating_add(next.cached_input),
    }
}

/// Reported terminal usage, preserving a genuine all-zero report.
pub(crate) fn reported_usage(result: &QueryResult) -> Option<TokenUsage> {
    result.usage.as_ref().and_then(|usage| {
        let reported = usage.input_tokens.is_some()
            || usage.cached_input_tokens.is_some()
            || usage.cache_write_input_tokens.is_some()
            || usage.output_tokens.is_some()
            || usage.reasoning_output_tokens.is_some();
        reported.then(|| {
            let cached_input = usage.cached_input_tokens.unwrap_or_default();
            TokenUsage {
                input: usage
                    .input_tokens
                    .unwrap_or_default()
                    .saturating_add(cached_input)
                    .saturating_add(usage.cache_write_input_tokens.unwrap_or_default()),
                output: usage
                    .output_tokens
                    .unwrap_or_default()
                    .saturating_add(usage.reasoning_output_tokens.unwrap_or_default()),
                cached_input,
            }
        })
    })
}

/// A terminal Claude result, translated. Every field on
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
    let usage = reported_usage(&result)
        .map(Usage::Reported)
        .unwrap_or(Usage::Unreported);
    let failure = result.is_error.then(|| result_failure(&result));

    TurnOutcome {
        reply: result.result.trim().to_string(),
        resume,
        cost,
        usage,
        provider_turns: result.num_turns,
        elapsed,
        metadata: BTreeMap::new(),
        failure,
    }
}

/// A terminal `stream-json` result, with observations made earlier in
/// the stream filling only fields the terminal event omitted.
///
/// The terminal result is authoritative when it reports usage. The
/// cumulative assistant-message total is a fallback, never something
/// added to that terminal number. A terminal event also proves a
/// client-assigned or earlier stream-observed session was opened even
/// when this CLI version omitted `session_id` from the result itself.
pub(crate) fn from_stream_result(
    result: QueryResult,
    cumulative_usage: Option<TokenUsage>,
    elapsed: Duration,
    fallback_resume: Option<&ResumeId>,
) -> TurnOutcome {
    let mut outcome = from_query_result(result, elapsed);
    if matches!(outcome.usage, Usage::Unreported)
        && let Some(usage) = cumulative_usage
    {
        outcome.usage = Usage::Reported(usage);
    }
    if outcome.resume.is_none()
        && let Some(resume) = fallback_resume
    {
        outcome.resume = Some(ResumeId::ProviderAssigned(resume.value().to_string()));
    }
    outcome
}

fn result_failure(result: &QueryResult) -> TurnFailure {
    let message = result_error_message(result);
    match result
        .extra
        .get("subtype")
        .and_then(serde_json::Value::as_str)
    {
        Some("error_max_turns" | "error_max_budget_usd") => {
            TurnFailure::limit(message.to_lowercase())
        }
        _ => TurnFailure::reported(message),
    }
}

fn result_error_message(result: &QueryResult) -> String {
    let reply = result.result.trim();
    if !reply.is_empty() {
        return reply.to_string();
    }
    result
        .extra
        .get("errors")
        .and_then(serde_json::Value::as_array)
        .and_then(|errors| errors.first())
        .and_then(serde_json::Value::as_str)
        .unwrap_or("claude reported an error")
        .to_string()
}

/// Map a wrapper failure into a typed provider error while preserving
/// any telemetry observed before the stream ended.
///
/// [`AgentError::Timeout`], [`AgentError::Protocol`], and the
/// post-launch shapes of [`AgentError::Other`] carry a
/// [`PartialTelemetry`] with `elapsed` set, because all three can
/// follow a completed process launch: a JSON parse failure means the
/// CLI ran to completion and produced output, and an ordinary nonzero
/// exit means the same. Money and tokens are left `None` on that
/// partial when the stream did not report them -- there is nothing more
/// honest to retain in that case than "this much time passed".
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
#[cfg(test)]
pub(crate) fn classify_failure(
    error: claude_wrapper::Error,
    elapsed: Duration,
    provider: ProviderKey,
) -> AgentError {
    classify_failure_with_partial(error, elapsed, provider, PartialTelemetry::none())
}

/// Classify a stream failure without discarding usage or a session that
/// arrived before the terminal error.
pub(crate) fn classify_failure_with_partial(
    error: claude_wrapper::Error,
    elapsed: Duration,
    provider: ProviderKey,
    mut observed: PartialTelemetry,
) -> AgentError {
    observed.elapsed = Some(elapsed);
    let launched_partial = || observed.clone().into();

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
        } => {
            let detail = command_failed_detail(exit_code, &stdout, &stderr);
            let detail = match claude_wrapper::auth::classify_failure(exit_code, &stdout, &stderr) {
                Some(kind) => format!("auth error ({kind:?}): {detail}"),
                None => detail,
            };
            AgentError::Other {
                provider,
                detail,
                partial: launched_partial(),
            }
        }
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

    fn stream_event(json: &str) -> StreamEvent {
        serde_json::from_str(json).expect("stream event")
    }

    #[test]
    fn assistant_usage_requires_a_numeric_bucket_but_keeps_reported_zero() {
        let empty = stream_event(r#"{"type":"assistant","message":{"usage":{}}}"#);
        let nulls = stream_event(
            r#"{"type":"assistant","message":{"usage":{"input_tokens":null,"output_tokens":null}}}"#,
        );
        let zero = stream_event(
            r#"{"type":"assistant","message":{"id":"zero","usage":{"input_tokens":0,"output_tokens":0}}}"#,
        );
        let cached = stream_event(
            r#"{"type":"assistant","message":{"id":"cached","usage":{"input_tokens":3,"cache_read_input_tokens":5,"cache_creation_input_tokens":7,"output_tokens":2}}}"#,
        );

        assert_eq!(assistant_usage(&empty), None);
        assert_eq!(assistant_usage(&nulls), None);
        assert_eq!(
            assistant_usage(&zero),
            Some((Some("zero".into()), TokenUsage::default())),
            "an explicit numeric zero is a report, not an absent measurement"
        );
        assert_eq!(
            assistant_usage(&cached),
            Some((
                Some("cached".into()),
                TokenUsage {
                    input: 15,
                    output: 2,
                    cached_input: 5,
                }
            )),
            "portable input includes Claude's non-cached, cache-read, and cache-write buckets"
        );
    }

    fn query_result(result_json: &str) -> QueryResult {
        serde_json::from_str(result_json).expect("query result")
    }

    /// The bug issue 53 exists for: a run that worked for minutes and
    /// hit its cap must keep its spend and its session, not flatten to
    /// zero and unresumable.
    #[test]
    fn a_capped_run_keeps_its_spend_and_session_as_reported_not_zero() {
        let result = query_result(
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,
                "total_cost_usd":1.25,"num_turns":60,"session_id":"sess-1",
                "errors":["Reached maximum number of turns (60)"]}"#,
        );
        let outcome = from_stream_result(result, None, Duration::from_millis(323_000), None);

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
        let result = query_result(
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,
                "errors":["Reached maximum number of turns (60)"]}"#,
        );
        let assigned = ResumeId::ClientAssigned("assigned-1".into());
        let outcome = from_stream_result(result, None, Duration::from_millis(10), Some(&assigned));

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
        let result = query_result(
            r#"{"type":"result","subtype":"error_max_budget_usd","is_error":true,
                "errors":["Reached maximum budget ($0.01)"],"num_turns":1,
                "total_cost_usd":0.1273986,"session_id":"s1"}"#,
        );
        let outcome = from_stream_result(result, None, Duration::from_millis(5_000), None);
        assert_eq!(
            outcome.failure_message(),
            Some("reached maximum budget ($0.01)")
        );
        assert_eq!(outcome.cost, Cost::Reported { micro_usd: 127_398 });
        assert_eq!(
            outcome.resume,
            Some(ResumeId::ProviderAssigned("s1".into()))
        );
    }

    /// Everything else is still a failure to run and must not be
    /// silently downgraded into a capped outcome.
    #[test]
    fn an_ordinary_failure_is_not_treated_as_a_cap() {
        let result = query_result(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,
                "result":"command not found"}"#,
        );
        let outcome = from_stream_result(result, None, Duration::from_millis(10), None);
        assert_eq!(
            outcome.failure,
            Some(TurnFailure::reported("command not found"))
        );
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

    #[test]
    fn a_terminal_result_normalizes_claudes_split_input_buckets() {
        let qr: QueryResult = serde_json::from_str(
            r#"{"result":"done","is_error":false,
                "usage":{"input_tokens":10,"cache_read_input_tokens":6,
                         "cache_creation_input_tokens":4,"output_tokens":5}}"#,
        )
        .unwrap();

        let outcome = from_query_result(qr, Duration::from_millis(1));
        assert_eq!(
            outcome.usage,
            Usage::Reported(TokenUsage {
                input: 20,
                output: 5,
                cached_input: 6,
            })
        );
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
