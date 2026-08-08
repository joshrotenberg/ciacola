//! The backend itself: what it is called, what it can do, how it runs a
//! turn, and how it recognises its own processes.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::BoxFut;
use crate::capability::Capabilities;
use crate::error::AgentError;
use crate::events::TurnEvents;
use crate::intent::TurnIntent;
use crate::outcome::TurnOutcome;

/// The stable string an agent's definition carries to say which backend
/// runs it.
///
/// A string rather than an enum because it is persisted: every agent in
/// the live ledger has one serialized into its definition, and a build
/// that dropped a variant would make those rows unreadable. The
/// registry maps it to an adapter at runtime, so adding a backend
/// changes no type here.
///
/// [`ProviderKey::default`] is `claude`, which is what makes every
/// definition written before this field existed keep working.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderKey(String);

impl ProviderKey {
    /// The default and, for now, only backend.
    pub const CLAUDE: &'static str = "claude";

    /// A key from a string. Not validated against the registry here;
    /// resolution is [`ProviderRegistry::get`], and its failure names
    /// what is actually registered.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The default backend.
    pub fn claude() -> Self {
        Self(Self::CLAUDE.to_string())
    }

    /// The key as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ProviderKey {
    /// `claude`.
    ///
    /// This is the single most load-bearing default in the change that
    /// introduced this crate: there is a live ledger full of real
    /// conversations whose serialized definitions have no provider
    /// field at all, and they must come back as Claude agents that
    /// resume their existing sessions.
    fn default() -> Self {
        Self::claude()
    }
}

impl fmt::Display for ProviderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ProviderKey {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ProviderKey {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// One backend that can run a turn.
///
/// Async-only, and boxed rather than `async fn`, because this is used
/// behind `dyn`: the whole point of [`ProviderRegistry`] is resolving
/// an adapter from a string that came out of the ledger.
pub trait Provider: Send + Sync {
    /// The key this adapter registers under.
    fn key(&self) -> ProviderKey;

    /// What it can and cannot honour. Called before every turn, so it
    /// must be cheap; it is a declaration, not a probe.
    fn capabilities(&self) -> Capabilities;

    /// Run one turn.
    ///
    /// **A run that ended badly is `Ok`.** A ceiling we set, or a
    /// result the provider itself called an error, comes back as a
    /// [`TurnOutcome`] carrying its spend, its usage, its conversation
    /// id, and a [`TurnFailure`](crate::TurnFailure). Getting this
    /// backwards is how a five minute run once landed in the ledger as
    /// free and unresumable.
    ///
    /// `Err` means the turn produced no usable result. For most
    /// [`AgentError`] variants that also means nothing was spent and no
    /// conversation was opened, but not for all of them: a cancelled,
    /// timed-out, or unparseable run may have done real paid work
    /// first. An adapter returning one of those three puts whatever it
    /// still knows on
    /// [`PartialTelemetry`](crate::PartialTelemetry) rather than
    /// discarding it.
    ///
    /// `events` is told about a conversation id the moment the backend
    /// reveals one, which may be long before this future resolves.
    fn run<'a>(
        &'a self,
        intent: &'a TurnIntent,
        events: &'a dyn TurnEvents,
    ) -> BoxFut<'a, Result<TurnOutcome, AgentError>>;

    /// Does this `ps` line belong to one of this backend's processes?
    ///
    /// Startup recovery has to find provider processes that outlived a
    /// crash, and it used to do that by looking for the literal string
    /// `claude` in argv. That is the backend's knowledge, not the
    /// core's, and it is wrong for the second backend on the day it
    /// arrives.
    ///
    /// The caller owns the safety rules and keeps them: it builds the
    /// needle from the first line of the prompt only (because `ps`
    /// renders a newline as a line break, so a needle crossing one
    /// matches nothing), refuses to search on anything shorter than a
    /// dozen characters, and reports rather than guesses when it cannot
    /// search. This method answers one question and must not widen: a
    /// predicate that matched too much would kill an operator's own
    /// interactive session.
    fn owns_process(&self, ps_line: &str) -> bool;
}

/// Two adapters tried to register under the same key.
///
/// A boot-time misconfiguration, not a turn outcome, which is why this
/// is not an [`AgentError`] variant: that type's whole contract is
/// about turns, and nothing here has run one. Checked before any turn
/// can be sent, so there is no partial telemetry question either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateProvider {
    /// The key both adapters claimed.
    pub key: String,
}

impl fmt::Display for DuplicateProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "an adapter is already registered as '{}'; registration refuses to \
             replace it rather than silently prefer whichever registered last",
            self.key
        )
    }
}

impl std::error::Error for DuplicateProvider {}

/// Adapters by name, resolved at runtime.
///
/// A `BTreeMap` rather than a `HashMap` so [`ProviderRegistry::keys`]
/// is stable: it ends up in error messages and in the health report,
/// and an ordering that changes between boots is noise.
#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    /// An empty registry. A server with one is not usable; the binary
    /// registers what it was built with.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an adapter under its own key.
    ///
    /// Refuses a key that is already taken rather than replacing it.
    /// Two adapters claiming `claude` at boot is a misconfiguration --
    /// a build that links both a real adapter and a test shim under the
    /// same name, say -- and silently keeping whichever registered last
    /// means every turn after that runs on an adapter nobody chose on
    /// purpose. There is no legitimate reason to overwrite a live
    /// registration; a caller that wants a different adapter for the
    /// same key builds a fresh [`ProviderRegistry`] instead.
    pub fn register(
        &mut self,
        provider: Arc<dyn Provider>,
    ) -> Result<&mut Self, DuplicateProvider> {
        let key = provider.key().as_str().to_string();
        if self.providers.contains_key(&key) {
            return Err(DuplicateProvider { key });
        }
        self.providers.insert(key, provider);
        Ok(self)
    }

    /// Builder-shaped [`register`](Self::register), for `main`.
    ///
    /// Panics on a duplicate key rather than returning a `Result`,
    /// because this is boot-time wiring: a binary that links two
    /// adapters under the same key has a bug in the list it builds, not
    /// a recoverable runtime condition, and failing closed here means
    /// the process never starts serving turns on the wrong adapter.
    /// Code that wants to handle the collision (tests, an operator
    /// shim) calls [`register`](Self::register) directly instead.
    #[must_use]
    pub fn with(mut self, provider: Arc<dyn Provider>) -> Self {
        self.register(provider)
            .expect("duplicate provider key registered at boot");
        self
    }

    /// The adapter for this key, or an error that names what is
    /// registered.
    pub fn get(&self, key: &ProviderKey) -> Result<Arc<dyn Provider>, AgentError> {
        self.providers
            .get(key.as_str())
            .cloned()
            .ok_or_else(|| AgentError::UnknownProvider {
                requested: key.as_str().to_string(),
                known: self.keys(),
            })
    }

    /// Every registered key, sorted.
    pub fn keys(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    /// Whether anything is registered at all. A server with an empty
    /// registry cannot run a turn, which is worth saying at boot rather
    /// than discovering on the first send.
    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("providers", &self.keys())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The minimum viable adapter, for exercising the registry without
    /// dragging in a full fake with a run script.
    struct Stub(&'static str);

    impl Provider for Stub {
        fn key(&self) -> ProviderKey {
            ProviderKey::new(self.0)
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::none(self.key())
        }
        fn run<'a>(
            &'a self,
            _intent: &'a TurnIntent,
            _events: &'a dyn TurnEvents,
        ) -> BoxFut<'a, Result<TurnOutcome, AgentError>> {
            Box::pin(async { Ok(TurnOutcome::ok("stub")) })
        }
        fn owns_process(&self, _ps_line: &str) -> bool {
            false
        }
    }

    /// The concrete misconfiguration this refusal exists for: two
    /// adapters claiming the same key at boot must not silently resolve
    /// to whichever registered last.
    #[test]
    fn registering_a_duplicate_key_is_refused_rather_than_replacing() {
        let mut registry = ProviderRegistry::new();
        registry
            .register(Arc::new(Stub("claude")))
            .expect("first registration succeeds");

        let err = registry
            .register(Arc::new(Stub("claude")))
            .expect_err("a second adapter under the same key must be refused");
        assert_eq!(err.key, "claude");
        assert!(err.to_string().contains("claude"), "{err}");

        // The original registration is untouched by the refused attempt.
        assert_eq!(registry.keys(), vec!["claude".to_string()]);
    }

    /// `with` is the boot-time builder; a collision there is a bug in
    /// the list the binary assembles, not a runtime condition to route
    /// around, so it fails closed by panicking rather than starting a
    /// server with an ambiguous adapter.
    #[test]
    #[should_panic(expected = "duplicate provider key")]
    fn with_panics_on_a_duplicate_key_at_boot() {
        let _ = ProviderRegistry::new()
            .with(Arc::new(Stub("claude")))
            .with(Arc::new(Stub("claude")));
    }

    /// The compatibility property the live ledger depends on: a
    /// definition written before the field existed deserializes as
    /// Claude.
    #[test]
    fn an_absent_provider_key_defaults_to_claude() {
        #[derive(Deserialize)]
        struct Def {
            #[serde(default)]
            provider: ProviderKey,
        }
        let def: Def = serde_json::from_str("{}").expect("an empty definition still parses");
        assert_eq!(def.provider, ProviderKey::claude());
    }

    /// It is a bare string on the wire, not a tagged enum, so a
    /// definition written by a newer build is still readable and a key
    /// nobody has an adapter for still round-trips.
    #[test]
    fn a_key_is_a_bare_string_on_the_wire() {
        let json = serde_json::to_string(&ProviderKey::new("codex")).expect("serialize");
        assert_eq!(json, "\"codex\"");
        let back: ProviderKey = serde_json::from_str("\"something-unbuilt\"").expect("deserialize");
        assert_eq!(back.as_str(), "something-unbuilt");
    }
}
