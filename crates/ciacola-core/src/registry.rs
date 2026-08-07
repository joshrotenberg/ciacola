//! The registry: every agent the server knows, by id.
//!
//! A conversation is serial, so the one rule enforced here is that an
//! agent runs at most one turn at a time. The map is behind a std mutex
//! (never held across an await); the turn itself runs outside the lock
//! on a clone, and the updated agent is written back when it completes.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::agent::{Agent, AgentDef, FlatError, Turn, prompt};

#[derive(Clone, Default)]
pub struct Registry {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    agents: HashMap<String, Agent>,
    /// Agents currently mid-turn. Guarded by [`BusyGuard`] so a dropped
    /// or failed turn cannot leave an agent stuck busy.
    busy: HashSet<String>,
}

/// Clears the busy flag however the turn ends: success, error, or the
/// future being dropped mid-run.
struct BusyGuard {
    inner: Arc<Mutex<Inner>>,
    id: String,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.busy.remove(&self.id);
        }
    }
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Define an agent. It exists from here on, whether or not it ever
    /// runs.
    pub fn create(&self, def: AgentDef) -> Agent {
        let agent = Agent::new(def);
        let mut inner = self.inner.lock().expect("registry lock");
        inner.agents.insert(agent.id.clone(), agent.clone());
        agent
    }

    pub fn get(&self, id: &str) -> Option<Agent> {
        self.inner
            .lock()
            .expect("registry lock")
            .agents
            .get(id)
            .cloned()
    }

    pub fn list(&self) -> Vec<Agent> {
        let inner = self.inner.lock().expect("registry lock");
        let mut agents: Vec<Agent> = inner.agents.values().cloned().collect();
        agents.sort_by(|a, b| a.id.cmp(&b.id));
        agents
    }

    /// Is the agent mid-turn right now?
    pub fn is_busy(&self, id: &str) -> bool {
        self.inner.lock().expect("registry lock").busy.contains(id)
    }

    /// Run one turn. Fails fast if the agent does not exist or is
    /// already mid-turn; otherwise blocks until the provider replies.
    pub async fn prompt(&self, id: &str, text: &str) -> Result<Turn, FlatError> {
        let (mut agent, _guard) = {
            let mut inner = self.inner.lock().expect("registry lock");
            let Some(agent) = inner.agents.get(id).cloned() else {
                return Err(format!("no agent '{id}'").into());
            };
            if !inner.busy.insert(id.to_string()) {
                return Err(format!("agent '{id}' is mid-turn; wait for it").into());
            }
            (
                agent,
                BusyGuard {
                    inner: self.inner.clone(),
                    id: id.to_string(),
                },
            )
        };

        let turn = prompt(&mut agent, text).await?.clone();

        let mut inner = self.inner.lock().expect("registry lock");
        inner.agents.insert(id.to_string(), agent);
        Ok(turn)
    }
}
