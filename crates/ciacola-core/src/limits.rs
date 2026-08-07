//! Circuit breakers, not budgets.
//!
//! Stage 6 concluded "track, do not cap" from a 6x cost spread on
//! identical prompts, and the spread has only widened: across every
//! real turn this session, $0.0163 to $0.8858, a factor of 54. That
//! reasoning still holds against a *budget*, which is a number meant to
//! be correct. It does not hold against a *circuit breaker*, which only
//! has to be wrong in the safe direction: if it trips early you raise
//! it, and the cost of tripping is a delay while the cost of not having
//! one is unbounded.
//!
//! What changed is that the system now fires unattended. One manager on
//! a thirty minute schedule is 48 wakes a day at $0.33 to $0.89 each,
//! so three repositories is $48 to $128 a day of *steady state*, before
//! anything goes wrong.
//!
//! Two controls, deliberately different in kind:
//!
//! - **Spend is a lagging indicator.** By the time a runaway shows up
//!   in dollars it has already spent them. So the money limit is
//!   opt-in (we cannot pick your number) and applies to *new
//!   submissions only*: work already running finishes. A hard stop
//!   mid-supervision is worse than the overspend, because it leaves a
//!   half-finished branch and an agent that never learns how it ended.
//! - **Depth is a leading indicator.** A conductor that spawns
//!   conductors compounds faster than any spend check can catch it, and
//!   depth is structural rather than arbitrary. So it is opt-*out*,
//!   with a real default: an orchestrator, its spokes, and their
//!   helpers is three.

use serde::Deserialize;

pub const DEFAULT_MAX_SPAWN_DEPTH: i64 = 3;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Notify once per crossing when the rolling day passes this.
    pub daily_warn_usd: Option<f64>,
    /// Refuse new submissions past this. Running turns are untouched.
    pub daily_stop_usd: Option<f64>,
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
            max_spawn_depth: DEFAULT_MAX_SPAWN_DEPTH,
        }
    }
}

impl Limits {
    pub fn stop_micro_usd(&self) -> Option<i64> {
        self.daily_stop_usd.map(|usd| (usd * 1e6) as i64)
    }

    pub fn warn_micro_usd(&self) -> Option<i64> {
        self.daily_warn_usd.map(|usd| (usd * 1e6) as i64)
    }

    /// Describes itself for the board and the banner, because a limit
    /// nobody can see is a limit nobody remembers setting.
    pub fn summary(&self) -> String {
        let money = match (self.daily_warn_usd, self.daily_stop_usd) {
            (Some(w), Some(s)) => format!("warn ${w:.2}/day, stop ${s:.2}/day"),
            (Some(w), None) => format!("warn ${w:.2}/day"),
            (None, Some(s)) => format!("stop ${s:.2}/day"),
            (None, None) => "no spend limit".into(),
        };
        let depth = match self.max_spawn_depth {
            0 => "unlimited spawn depth".to_string(),
            d => format!("spawn depth {d}"),
        };
        format!("{money}, {depth}")
    }
}
