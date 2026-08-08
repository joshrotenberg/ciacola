//! Fake adapters, exercising the contract without a CLI.
//!
//! CI-safe by construction: nothing here spawns a process, so the five
//! shapes a real backend can end in are pinned by tests that run
//! everywhere. They are also the worked example a second adapter can be
//! written against.
//!
//! The five: a plain success; a run that failed and still carries real
//! usage; a backend that names its own conversation and says so through
//! the event sink; a cancelled run; and a turn asking for something the
//! backend cannot honour.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use ciacola_agent::{
    AgentError, BoxFut, Capabilities, Constraint, Cost, Isolation, NoEvents, Provider, ProviderKey,
    ProviderRegistry, ResumeId, Severity, TokenUsage, TurnEvents, TurnFailure, TurnIntent,
    TurnOutcome,
};

/// What the fake should do this time. One adapter with a script beats
/// five adapters that drift apart.
enum Script {
    Succeed,
    /// Ran, failed, and still knows what it spent.
    FailWithUsage,
    /// Named its own conversation partway through.
    NameItsOwnConversation(&'static str),
    /// Stopped because we asked it to.
    Cancel,
}

struct Fake {
    key: ProviderKey,
    caps: Capabilities,
    script: Script,
}

impl Fake {
    /// A backend that honours every boundary and reports money, which is
    /// the shape Claude has.
    fn new(script: Script) -> Self {
        let key = ProviderKey::new("fake");
        let mut caps = Capabilities::none(key.clone());
        caps.isolation = true;
        caps.credential_isolation = true;
        caps.scoped_mcp = true;
        caps.strict_mcp = true;
        caps.allowed_tools = true;
        caps.client_assigned_resume = true;
        caps.reports_token_usage = true;
        caps.reports_cost = true;
        Self { key, caps, script }
    }

    /// A backend that reports tokens and no money at all, which is the
    /// shape codex has and the asymmetry [`Cost::NotPriced`] exists for.
    fn unpriced(mut self) -> Self {
        self.caps.reports_cost = false;
        self
    }

    /// What a successful run of this backend costs. The declaration and
    /// the outcome have to agree, or the capability is decoration.
    fn cost(&self) -> Cost {
        if self.caps.reports_cost {
            Cost::Reported { micro_usd: 4_200 }
        } else {
            Cost::NotPriced
        }
    }
}

impl Provider for Fake {
    fn key(&self) -> ProviderKey {
        self.key.clone()
    }

    fn capabilities(&self) -> Capabilities {
        self.caps.clone()
    }

    fn run<'a>(
        &'a self,
        intent: &'a TurnIntent,
        events: &'a dyn TurnEvents,
    ) -> BoxFut<'a, Result<TurnOutcome, AgentError>> {
        Box::pin(async move {
            match self.script {
                Script::Cancel => Err(AgentError::Cancelled {
                    provider: self.key.clone(),
                }),
                Script::Succeed => Ok(TurnOutcome {
                    resume: intent.resume.clone(),
                    cost: self.cost(),
                    usage: TokenUsage {
                        input: 120,
                        output: 40,
                        cached_input: 100,
                    },
                    provider_turns: Some(3),
                    elapsed: Duration::from_millis(1_500),
                    ..TurnOutcome::ok("done")
                }),
                Script::FailWithUsage => Ok(TurnOutcome {
                    resume: intent.resume.clone(),
                    cost: self.cost(),
                    usage: TokenUsage {
                        input: 900,
                        output: 12,
                        cached_input: 0,
                    },
                    provider_turns: Some(60),
                    elapsed: Duration::from_millis(323_000),
                    failure: Some(TurnFailure::limit("reached maximum number of turns (60)")),
                    ..TurnOutcome::ok("")
                }),
                Script::NameItsOwnConversation(id) => {
                    // The point of the sink: told the moment the id
                    // exists, not only when the turn resolves.
                    let learned = ResumeId::ProviderAssigned(id.to_string());
                    events.resume_id(&learned).await;
                    Ok(TurnOutcome {
                        resume: Some(learned),
                        ..TurnOutcome::ok("done")
                    })
                }
            }
        })
    }

    fn owns_process(&self, ps_line: &str) -> bool {
        ps_line.contains("fake-backend")
    }
}

/// A sink that records what it was told, standing in for the ledger.
#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl Recorder {
    fn seen(&self) -> Vec<String> {
        self.0.lock().expect("recorder").clone()
    }
}

impl TurnEvents for Recorder {
    fn resume_id<'a>(&'a self, id: &'a ResumeId) -> BoxFut<'a, ()> {
        Box::pin(async move {
            self.0
                .lock()
                .expect("recorder")
                .push(format!("{}:{}", id.is_open(), id.value()));
        })
    }
}

fn intent() -> TurnIntent {
    let mut intent = TurnIntent::new("do the thing");
    intent.instructions = Some("you are a fake".into());
    intent.isolation = Isolation::Full;
    intent.allowed_tools = vec!["Read".into()];
    intent
}

/// A registry resolves by the string an agent's definition carries, and
/// says what it has when it cannot.
#[test]
fn the_registry_resolves_by_name_and_names_what_it_has() {
    let registry = ProviderRegistry::new().with(Arc::new(Fake::new(Script::Succeed)));
    assert!(registry.get(&ProviderKey::new("fake")).is_ok());
    let Err(err) = registry.get(&ProviderKey::claude()) else {
        panic!("nothing is registered as claude here");
    };
    assert!(err.to_string().contains("fake"), "{err}");
}

/// Recovery asks the backend whether a surviving process is one of its
/// own, rather than looking for the literal string `claude` in argv the
/// way core does today. The predicate must not widen: a match on an
/// unrelated line is an operator's own interactive session getting
/// killed by a restart.
#[test]
fn a_backend_recognises_its_own_processes_and_no_others() {
    let fake = Fake::new(Script::Succeed);
    assert!(fake.owns_process("54321 fake-backend --resume sess-1 do the thing"));
    assert!(
        !fake.owns_process("54322 claude -p do the thing"),
        "another backend's process is not this backend's to kill"
    );
    assert!(!fake.owns_process("54323 -zsh"));
}

/// Plain success, and the portable measure surviving a backend that
/// reports no money at all.
#[tokio::test]
async fn a_successful_turn_reports_usage_without_inventing_a_price() {
    let fake = Fake::new(Script::Succeed).unpriced();
    let outcome = fake
        .run(&intent(), &NoEvents)
        .await
        .expect("a run that happened is Ok");

    assert!(outcome.succeeded());
    assert_eq!(outcome.reply, "done");
    assert_eq!(outcome.usage.input, 120);
    assert_eq!(outcome.usage.cached_input, 100);
    assert_eq!(outcome.provider_turns, Some(3));
    assert_eq!(
        outcome.cost,
        Cost::NotPriced,
        "a backend that never prices must say so, not report zero"
    );
    assert!(
        !outcome.cost.is_missing(),
        "'never here' is not a gap to chase"
    );
    assert!(!fake.capabilities().reports_cost);
}

/// The same contract from the other side. Two backends that differ only
/// in whether they price their work must produce outcomes that differ in
/// a way the ledger can see, or the capability is decoration and the
/// always-`None` field this crate exists to avoid has been reinvented.
#[tokio::test]
async fn a_pricing_backend_and_an_unpriced_one_are_distinguishable() {
    let priced = Fake::new(Script::Succeed)
        .run(&intent(), &NoEvents)
        .await
        .expect("ran");
    let unpriced = Fake::new(Script::Succeed)
        .unpriced()
        .run(&intent(), &NoEvents)
        .await
        .expect("ran");

    assert_eq!(priced.cost, Cost::Reported { micro_usd: 4_200 });
    assert_eq!(unpriced.cost, Cost::NotPriced);
    assert_ne!(
        priced.cost, unpriced.cost,
        "the difference must survive into the outcome, not just the declaration"
    );
    assert_eq!(
        priced.usage, unpriced.usage,
        "tokens are the portable measure and must not vary with pricing"
    );
}

/// The property `capped()` has always had, now in the contract: a run
/// that hit a ceiling comes back as data. It spent real time, it may
/// have opened the conversation, and `Err` would throw both away.
#[tokio::test]
async fn a_failed_turn_still_carries_its_usage_and_its_conversation() {
    let fake = Fake::new(Script::FailWithUsage);
    let mut asked = intent();
    asked.resume = Some(ResumeId::ProviderAssigned("sess-1".into()));

    let outcome = fake
        .run(&asked, &NoEvents)
        .await
        .expect("a cap is an outcome, not a failure to run");

    assert!(!outcome.succeeded());
    assert_eq!(
        outcome.failure_message(),
        Some("reached maximum number of turns (60)")
    );
    assert_eq!(
        outcome.usage.input, 900,
        "usage that was really spent must survive the failure"
    );
    assert_eq!(
        outcome.resume.as_ref().map(ResumeId::value),
        Some("sess-1"),
        "a failed turn must leave the agent resumable"
    );
    assert_eq!(
        outcome.cost,
        Cost::Reported { micro_usd: 4_200 },
        "the bug this exists for: a long run that hit a ceiling was \
         recorded as costing nothing, which is invisible to the spend limit"
    );
    assert_eq!(outcome.elapsed, Duration::from_millis(323_000));
}

/// A backend that names its own conversation tells the sink the moment
/// it knows, rather than only through the terminal outcome. That is
/// what stops a crash mid-turn from losing the conversation.
#[tokio::test]
async fn a_provider_assigned_id_reaches_the_sink_before_the_outcome() {
    let fake = Fake::new(Script::NameItsOwnConversation("thread-77"));
    let recorder = Recorder::default();

    let outcome = fake.run(&intent(), &recorder).await.expect("ran");

    assert_eq!(
        recorder.seen(),
        vec!["true:thread-77".to_string()],
        "the sink must be told, and told that the conversation is open"
    );
    assert_eq!(
        outcome.resume,
        Some(ResumeId::ProviderAssigned("thread-77".into()))
    );
}

/// Stopping a turn on purpose is `Err`, because nothing was completed,
/// and it is distinguishable without reading a message.
#[tokio::test]
async fn a_cancelled_turn_is_a_typed_error_rather_than_a_string() {
    let fake = Fake::new(Script::Cancel);
    let err = fake
        .run(&intent(), &NoEvents)
        .await
        .expect_err("a cancelled run produced no outcome");

    assert!(err.is_cancelled());
    assert_eq!(err.provider().map(ProviderKey::as_str), Some("fake"));
    assert!(matches!(err, AgentError::Cancelled { .. }));
}

/// The dangerous case, and the one the whole capability mechanism is
/// for: a backend that cannot seal a turn off from ambient
/// configuration must refuse the turn, not run it wide open.
#[test]
fn an_unsupported_security_constraint_refuses_the_turn() {
    let mut fake = Fake::new(Script::Succeed);
    fake.caps.isolation = false;

    let validation = fake.capabilities().validate(&intent());
    let blocking = validation
        .blocking()
        .expect("dropping isolation must stop the turn");

    assert_eq!(blocking.constraint, Constraint::Isolation);
    assert_eq!(blocking.severity, Severity::Fail);
    assert!(
        blocking.detail.contains("sealed"),
        "the message has to say what was lost: {}",
        blocking.detail
    );
}

/// The other half of the same rule: a constraint that degrades the run
/// without widening it is said out loud and the turn proceeds.
#[test]
fn an_unsupported_comfort_constraint_only_warns() {
    let mut fake = Fake::new(Script::Succeed);
    fake.caps.max_provider_turns = false;
    let mut asked = intent();
    asked.max_provider_turns = Some(40);

    let validation = fake.capabilities().validate(&asked);
    assert!(
        validation.blocking().is_none(),
        "a turn ceiling is not an authority boundary"
    );
    assert_eq!(
        validation.warnings().map(|u| u.constraint).next(),
        Some(Constraint::MaxProviderTurns)
    );
}
