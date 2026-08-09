//! Codex JSONL events and wrapper errors translated onto the contract.

use std::collections::BTreeMap;
use std::time::Duration;

use ciacola_agent::{
    AgentError, Cost, PartialTelemetry, ProviderKey, ResumeId, TokenUsage, TurnFailure,
    TurnOutcome, Usage,
};
use codex_wrapper::{JsonLineEvent, QueryResult};

pub(crate) fn from_events(events: Vec<JsonLineEvent>, elapsed: Duration) -> TurnOutcome {
    let usage = usage_from(&events)
        .map(Usage::Reported)
        .unwrap_or(Usage::Unreported);
    let failure = events
        .iter()
        .rev()
        .find(|event| event.is_turn_failed())
        .map(|event| TurnFailure::reported(failure_message(event)));
    let result = QueryResult::from_events(events);

    TurnOutcome {
        reply: result.result.trim().to_string(),
        resume: result.thread_id.map(ResumeId::ProviderAssigned),
        cost: Cost::NotPriced,
        usage,
        provider_turns: None,
        elapsed,
        metadata: BTreeMap::new(),
        failure,
    }
}

pub(crate) fn has_terminal_failure(events: &[JsonLineEvent]) -> bool {
    events.iter().any(JsonLineEvent::is_turn_failed)
}

pub(crate) fn partial(events: &[JsonLineEvent], elapsed: Duration) -> PartialTelemetry {
    PartialTelemetry {
        resume: events
            .iter()
            .find_map(JsonLineEvent::thread_id)
            .map(|id| ResumeId::ProviderAssigned(id.to_string())),
        cost: None,
        usage: usage_from(events),
        elapsed: Some(elapsed),
    }
}

pub(crate) fn classify_failure(
    error: codex_wrapper::Error,
    elapsed: Duration,
    events: &[JsonLineEvent],
) -> AgentError {
    let provider = ProviderKey::codex();
    let partial = || partial(events, elapsed).into();
    match error {
        codex_wrapper::Error::NotFound => AgentError::NotFound {
            provider,
            detail: "codex binary not found in PATH".into(),
        },
        codex_wrapper::Error::Io { message, .. }
            if message.starts_with("failed to spawn codex") =>
        {
            AgentError::Launch {
                provider,
                detail: message,
            }
        }
        codex_wrapper::Error::Io { message, .. } => AgentError::Other {
            provider,
            detail: message,
            partial: partial(),
        },
        codex_wrapper::Error::Auth { message, .. } => AgentError::Launch {
            provider,
            detail: format!("authentication failed: {}", first_line(&message)),
        },
        codex_wrapper::Error::Config { message, .. } => AgentError::Launch {
            provider,
            detail: format!("configuration rejected: {}", first_line(&message)),
        },
        codex_wrapper::Error::NotTrustedDirectory { message, .. } => AgentError::Launch {
            provider,
            detail: first_line(&message).to_string(),
        },
        codex_wrapper::Error::SessionNotFound { message, .. } => AgentError::Launch {
            provider,
            detail: format!("thread not found: {}", first_line(&message)),
        },
        codex_wrapper::Error::Timeout { timeout_seconds } => AgentError::Timeout {
            provider,
            elapsed: Duration::from_secs(timeout_seconds),
            partial: partial(),
        },
        codex_wrapper::Error::Cancelled { .. } => AgentError::Cancelled {
            provider,
            partial: partial(),
        },
        codex_wrapper::Error::Json { message, .. } => AgentError::Protocol {
            provider,
            detail: message,
            partial: partial(),
        },
        codex_wrapper::Error::CommandFailed {
            exit_code,
            stdout,
            stderr,
            ..
        } => AgentError::Other {
            provider,
            detail: command_failed_detail(exit_code, &stdout, &stderr),
            partial: partial(),
        },
        other => AgentError::Other {
            provider,
            detail: other.to_string(),
            partial: partial(),
        },
    }
}

pub(crate) fn usage_snapshot(event: &JsonLineEvent) -> Option<TokenUsage> {
    let usage = event.usage()?;
    let reported = usage.input_tokens.is_some()
        || usage.output_tokens.is_some()
        || usage.cached_input_tokens.is_some();
    reported.then_some(TokenUsage {
        input: usage.input_tokens.unwrap_or_default(),
        output: usage.output_tokens.unwrap_or_default(),
        cached_input: usage.cached_input_tokens.unwrap_or_default(),
    })
}

fn usage_from(events: &[JsonLineEvent]) -> Option<TokenUsage> {
    events.iter().rev().find_map(usage_snapshot)
}

fn failure_message(event: &JsonLineEvent) -> String {
    event
        .extra
        .get("error")
        .and_then(|error| {
            error
                .as_str()
                .or_else(|| error.get("message").and_then(|value| value.as_str()))
        })
        .or_else(|| event.extra.get("message").and_then(|value| value.as_str()))
        .unwrap_or("codex reported turn failure")
        .to_string()
}

fn first_line(message: &str) -> &str {
    message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or(message)
}

fn command_failed_detail(exit_code: i32, stdout: &str, stderr: &str) -> String {
    let body = if !stderr.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    if body.is_empty() {
        format!("codex exited with code {exit_code}")
    } else {
        format!("codex exited with code {exit_code}: {body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(lines: &[&str]) -> Vec<JsonLineEvent> {
        lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("event"))
            .collect()
    }

    #[test]
    fn success_keeps_thread_reply_and_real_tokens_without_inventing_price() {
        let outcome = from_events(
            events(&[
                r#"{"type":"thread.started","thread_id":"thread-1"}"#,
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"done"}}"#,
                r#"{"type":"turn.completed","usage":{"input_tokens":12,"cached_input_tokens":7,"output_tokens":3}}"#,
            ]),
            Duration::from_millis(25),
        );
        assert_eq!(outcome.reply, "done");
        assert_eq!(
            outcome.resume,
            Some(ResumeId::ProviderAssigned("thread-1".into()))
        );
        assert_eq!(outcome.cost, Cost::NotPriced);
        assert_eq!(
            outcome.usage,
            Usage::Reported(TokenUsage {
                input: 12,
                output: 3,
                cached_input: 7,
            })
        );
        assert!(outcome.succeeded());
    }

    #[test]
    fn a_failed_terminal_event_keeps_thread_usage_and_failure() {
        let outcome = from_events(
            events(&[
                r#"{"type":"thread.started","thread_id":"thread-failed"}"#,
                r#"{"type":"turn.failed","error":{"message":"tool policy rejected"},"usage":{"input_tokens":8,"output_tokens":1}}"#,
            ]),
            Duration::from_secs(2),
        );
        assert!(!outcome.succeeded());
        assert_eq!(outcome.failure_message(), Some("tool policy rejected"));
        assert_eq!(
            outcome.resume,
            Some(ResumeId::ProviderAssigned("thread-failed".into()))
        );
        assert_eq!(
            outcome.usage,
            Usage::Reported(TokenUsage {
                input: 8,
                output: 1,
                cached_input: 0,
            })
        );
    }

    #[test]
    fn an_empty_usage_object_is_not_a_reported_snapshot() {
        let event = events(&[r#"{"type":"turn.completed","usage":{}}"#])
            .pop()
            .expect("event");
        assert_eq!(usage_snapshot(&event), None);
    }

    #[test]
    fn a_config_rejection_is_pre_turn_and_does_not_leak_argv() {
        let error = codex_wrapper::Error::Config {
            message: "Error loading config.toml: unknown configuration field `bad`".into(),
            command: "codex exec secret prompt".into(),
            exit_code: 1,
            working_dir: None,
        };
        let classified = classify_failure(error, Duration::from_millis(1), &[]);
        assert!(matches!(classified, AgentError::Launch { .. }));
        assert!(!classified.to_string().contains("secret prompt"));
    }
}
