# Operations

## Start and observe

Build from the checked-in lockfile and start through the operator REPL:

```sh
cargo build --locked -p ciacola
mcp-repl -- ./target/debug/ciacola
```

The startup banner reports:

- resolved ledger and config paths;
- whether human HTTP operator auth is enabled;
- registered providers and detected per-turn protection;
- effective limits, roles, and plugins;
- crash-recovery results; and
- the board URL and dispatch executor.

Dispatch remains closed until the complete loopback router is bound and
recovery finishes. A bad config, plugin failure, or occupied port therefore
leaves queued work durable and launches no provider.

Open `http://127.0.0.1:4823/board`. The overview prioritizes latest failures,
queued and running turns, and repository journeys. It refreshes through
server-sent events and labels connection loss as reconnecting.

Useful read-only MCP commands include:

```text
health
list
repo_state
worktrees
model_stats
findings
schedules
```

## Ledger and backups

The ledger path is resolved in this order:

1. `CIACOLA_DB`;
2. `$XDG_DATA_HOME/ciacola/ciacola.db`;
3. `$HOME/.local/share/ciacola/ciacola.db`;
4. a loudly marked temporary fallback when no user data directory exists.

The resolved path is always printed at startup. For a simple cold backup,
stop Ciacola gracefully and copy the database:

```sh
cp "$HOME/.local/share/ciacola/ciacola.db" \
  "$HOME/.local/share/ciacola/ciacola.db.backup"
```

For an online backup, use SQLite's backup operation against the printed path:

```sh
sqlite3 "$HOME/.local/share/ciacola/ciacola.db" \
  ".backup '$HOME/.local/share/ciacola/ciacola.db.backup'"
```

Restore only while Ciacola is stopped. Preserve the original until the restored
server completes migrations and the board, `health`, and repository state are
verified.

## Upgrade

Ciacola is not packaged yet. Upgrade the source checkout deliberately:

```sh
git pull --ff-only
cargo build --locked -p ciacola
```

Keep the currently running binary in place until the new build succeeds. Then:

1. drain and stop the old server;
2. back up the printed ledger path;
3. launch the new `./target/debug/ciacola`; and
4. inspect the startup migrations, recovery report, board, and `health` before
   admitting new work.

Schema migrations are forward-only and run during setup. Binary downgrade is
not a supported recovery strategy: an older binary can lack the authorization
or lifecycle fences introduced by a newer schema. Restore the matching backup
and binary together if a release must be rolled back.

Provider CLI upgrades are separate. The Codex adapter advertises a per-turn
ceiling only for tested meter semantics; a newly installed untested CLI can
turn configured protection into `UNSUPPORTED` and block automatic work safely.
Read the startup provider summary after every CLI upgrade.

## Limits and admission

Use both limit planes before enabling schedules, webhooks, or recursive agents:

- Rolling 24-hour USD and provider-token stops refuse new submissions at the
  boundary. They do not reserve capacity. Already admitted and concurrent
  turns can settle above the configured value.
- Provider-native per-turn ceilings narrow one opening or resumed execution.
  Response-boundary providers can still overshoot through in-flight work.

The board distinguishes `ENFORCED`, `UNSUPPORTED`, `UNBOUNDED`, and
`OVERRIDDEN` protection. A known reached rolling stop cannot be overridden.
Incomplete cost or token telemetry can make automatic admission unobservable;
use a supervised send only after inspecting the durable reason and accepting
the exposure.

## Shutdown and restart recovery

The first `Ctrl-C`:

1. stops accepting HTTP work;
2. ends live board streams;
3. waits up to ten minutes for in-flight turns; and
4. exits cleanly when the executor drains.

A second `Ctrl-C` abandons in-flight turns. On the next start, recovery:

- resubmits queued turns without changing their persisted policy snapshot;
- settles pre-crash running rows conservatively;
- asks provider adapters to identify and kill verified orphan processes; and
- reports anything it could not verify.

Recovery does not blindly repeat a paid provider call. When a provider session
was captured, the next admitted turn resumes it. When telemetry or session
evidence is incomplete, automatic work can fail closed and require explicit
operator inspection.

## Repository journeys

`start_issue` reserves one durable `(repository, issue)` assignment before
clone or worktree mutation. Concurrent and repeated calls reuse or conflict
with that assignment rather than creating a second owner.

The normal lifecycle is:

```text
preparing -> active -> finishing -> retained or completed
                         |
                         +-> stale on a recoverable invariant failure
```

Publication is orthogonal: unpublished, publishing, published, or failed,
plus the observed PR state. `open_pr` pins a reviewed full commit OID, validates
the assigned branch and clean tree, pushes that exact OID with a lease, and
reconciles one durable PR identity.

`finish_issue keep=true` retires the agent and retains the managed worktree.
Removal without explicit discard is allowed only when there is no committed
delta or the recorded PR is merged at the exact expected head and base. Dirty
work is never discarded. Open, closed-unmerged, unpublished, or drifted work
requires an exact current `discard_head` acknowledgement.

Stale assignments remain visible. When no agent exists, identify them by
`assignment_id` during cleanup. Do not manually delete a worktree, branch, or
assignment row unless the operator surface cannot represent the recovery; the
durable record is what makes restart and retry safe.

## Logs

Logs go to stderr because stdout is the stdio MCP transport. Set a tracing
filter with `RUST_LOG`:

```sh
RUST_LOG=ciacola=info,ciacola_core=info mcp-repl -- ./target/debug/ciacola
```

Do not enable broad trace logging around credentials. Provider credentials are
redacted and excluded from argv/config/ledger by design, but third-party tools
and user opt-in environment values may have their own logging behavior.

## Retention and pruning

Nothing prunes automatically. Run the operator-only destructive tool with an
explicit minimum age:

```text
prune older_than_days=30
```

Pruning blanks old finished-turn prompt/reply text and lets plugins delete old
closed state, then vacuums SQLite. Turn states, costs, usage, and timings
remain. Back up first when the history matters.

Repository worktrees and branches use their own journey cleanup. The general
prune tool does not replace `finish_issue`.

## Troubleshooting

### Startup refuses the config

Ciacola denies unknown TOML fields, invalid provider names, unsupported role
surfaces, incompatible tool policies, malformed limits, and configured
delegation. Fix the reported field; do not remove safety checks to make startup
continue.

### The port is already in use

Set another loopback port before starting:

```sh
CIACOLA_HTTP=4824 mcp-repl -- ./target/debug/ciacola
```

The board and internal MCP endpoint use the same port. Persistent agents store
the generated loopback configuration created at startup.

### Automatic work is refused

Inspect `health` and the board admission section. Common causes are a reached
rolling stop, missing token/cost telemetry, a configured per-turn ceiling the
installed CLI cannot enforce, an idle provider without authentication, or an
agent tool/sandbox request the provider cannot honor.

### Codex protection is unsupported

Ciacola probes the Codex CLI once at startup. Tested versions declare a
versioned native meter. An untested version remains usable only when no ceiling
is configured; with a configured ceiling, automatic work fails closed. Upgrade
or pin a tested CLI before unattended operation.

### A repository assignment is stale

Read its `phase`, `last_error`, worktree, branch, publication, and cleanup state
on the board or through `repo_state`. Retry the same safe operation when the
cause is transient. Use `finish_issue` by assignment id for an agentless stale
claim. Exact-head and dirty-tree refusals are safety decisions, not transient
errors.

### The REPL command `wait` does not call Ciacola

`wait` is also an mcp-repl task command. Invoke the Ciacola tool explicitly:

```text
call wait agent_id=AGENT_ID seq=1 timeout_secs=600
```

### The database is growing

Use `health` to inspect row counts and bytes. Back up, then run `prune` with a
deliberate age. Worktree growth is separate; inspect `worktrees` and complete
or retain repository journeys explicitly.
