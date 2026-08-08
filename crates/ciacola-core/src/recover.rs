//! Startup recovery: what a restart owes the ledger.
//!
//! The flat thesis is that resume-from-ledger replaces durable
//! queueing. This is the whole of what that costs. After a crash the
//! ledger can hold turns in two non-terminal states, and each has one
//! correct answer:
//!
//! - **queued**: the crash lost only the in-memory delivery. Resubmit
//!   to the executor. A queue-backed executor may duplicate a delivery
//!   its backend also makes; `claim_turn` makes the duplicate free.
//! - **running**: the exchange was in flight. Its provider process
//!   survived our death (kill_on_drop is a destructor, and destructors
//!   do not run on SIGKILL; the child sits in its own process group),
//!   so it is billing with no parent to report to, and worse, it will
//!   keep appending to the agent's session. Kill it by scanning argv
//!   for the turn's prompt (the wrapper exposes no pid; see the filed
//!   issue), then mark the turn failed. The *conversation* loses
//!   nothing: the session id from the last completed turn is in the
//!   ledger, which is the entire point.

use serde::Serialize;

use crate::exec::TurnExecutor;
use crate::ledger::Ledger;

#[derive(Debug, Default, Serialize)]
pub struct RecoveryReport {
    /// Queued turns handed back to the executor.
    pub resubmitted: usize,
    /// Running turns marked failed; the conversation itself is intact.
    pub orphaned: usize,
    /// Provider processes from before the crash found and killed.
    pub orphans_killed: usize,
    /// Running turns whose provider process could not be searched for
    /// (prompt too short or unmatchable in argv). Any of these may
    /// still be alive, billing, and appending to its session; the
    /// operator has to check by hand. Never hide this.
    pub orphans_unverified: usize,
}

/// Find provider processes whose argv carries this turn's prompt and
/// kill them. Substring match over `ps`, not a regex, so arbitrary
/// prompt text cannot break it.
/// Returns (killed, searched): searched=false means the scan could not
/// be attempted and the orphan's fate is unknown.
fn kill_orphans(prompt: &str) -> (usize, bool) {
    // ps renders a newline in argv as a line break, so only the first
    // line of the prompt can ever match; a needle crossing it would
    // silently match nothing (review finding).
    let needle: String = prompt
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(60)
        .collect();
    if needle.len() < 12 {
        // Too short to identify a process safely; report, do not guess.
        return (0, false);
    }
    let Ok(out) = std::process::Command::new("ps")
        .args(["-axo", "pid=,command="])
        .output()
    else {
        return (0, false);
    };
    let mut killed = 0;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line.contains("claude") && line.contains(needle.as_str()) {
            if let Some(pid) = line.split_whitespace().next() {
                if std::process::Command::new("kill")
                    .args(["-9", pid])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
                {
                    killed += 1;
                }
            }
        }
    }
    (killed, true)
}

/// Bring the ledger back to truth after a restart. Safe to run when
/// nothing crashed: with no non-terminal turns it does nothing.
pub async fn recover(
    ledger: &Ledger,
    exec: &dyn TurnExecutor,
) -> Result<RecoveryReport, crate::agent::FlatError> {
    let mut report = RecoveryReport::default();

    for turn in ledger.turns_in_state("running").await? {
        let (killed, searched) = kill_orphans(&turn.prompt);
        report.orphans_killed += killed;
        if !searched {
            report.orphans_unverified += 1;
        }
        // Assigned at creation, so this is normally true. It is false
        // for agents that predate assignment and never completed a turn.
        let resumable = ledger
            .get_agent(&turn.agent_id)
            .await
            .ok()
            .flatten()
            .is_some_and(|a| a.session.is_some());
        ledger
            .fail_turn(
                &turn.agent_id,
                turn.seq,
                "failed",
                // "conversation intact" used to be asserted rather
                // than known, and was wrong in exactly the case it was
                // written for: the session id was recorded only at turn
                // end, so a crash lost it and "send again" started over.
                // Ids are assigned before the turn runs now, so the
                // claim is true when there is one and this says so
                // either way rather than guessing.
                &match (searched, resumable) {
                    (true, true) => {
                        "orphaned by server crash; session kept, send again to resume".to_string()
                    }
                    (true, false) => "orphaned by server crash; no session recorded, \
                                      send again to start over"
                        .to_string(),
                    (false, true) => "orphaned by server crash; provider process could NOT be \
                                      verified dead, check by hand; session kept"
                        .to_string(),
                    (false, false) => "orphaned by server crash; provider process could NOT be \
                                       verified dead, check by hand; no session recorded"
                        .to_string(),
                },
                0,
                None,
            )
            .await?;
        report.orphaned += 1;
    }

    for turn in ledger.turns_in_state("queued").await? {
        exec.submit(turn.agent_id.clone(), turn.seq);
        report.resubmitted += 1;
    }

    Ok(report)
}
