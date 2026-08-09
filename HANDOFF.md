# ciacola: handoff

Historical design orientation, retained for the reasoning it records. Current
startup and product guidance lives in `README.md` and
`ciacola.example.toml`. This was written when the design spike became a
project, so that
whoever picks it up (including a later you, or an agent) does not have
to reconstruct the reasoning from the code.

The original Apalis-based work lives in `../apalis-agent`. The current server
keeps the small executor seam that survived that experiment, without an
Apalis implementation in this workspace.

## The one sentence

**An agent is a durable conversation.** The provider keeps the
conversation, ciacola keeps its id. So an agent exists while nothing is
running, a *turn* is one process execution against it, and recovery is
resume rather than retry.

That last clause is why there is no work queue at the centre. A queue's
durability buys re-execution, which is exactly what paid agent work
must never do. This was tested rather than assumed: an identical server
was built on Apalis and on a hand-rolled executor, both were killed
mid-run, and neither self-healed without the ledger. The production server
now uses the hand-rolled channel or polling executor behind `TurnExecutor`.
Both executors start behind a closed, process-local dispatch boundary. Startup
assembles every plugin and router, binds and starts the complete loopback HTTP
server, reconciles crash recovery while dispatch is still closed, and only
then permits a turn to be claimed. A bad config, plugin failure, or occupied
port therefore leaves durable queued work queued instead of spending a
provider run against an unavailable MCP endpoint.

## Running it

```sh
cp ciacola.example.toml ciacola.toml
mkdir -p "$HOME/.local/share/ciacola"
CIACOLA_DB="$HOME/.local/share/ciacola/ciacola.db" cargo run -p ciacola
```

The default database is durable under `$XDG_DATA_HOME/ciacola`, or
`$HOME/.local/share/ciacola` when XDG is unset. `CIACOLA_DB` selects an
explicit location. Before adding a scheduled agent, configure both an
observable rolling admission stop and the provider's enforceable per-turn
ceiling in `ciacola.toml`.

| variable | meaning |
|---|---|
| `CIACOLA_DB` | ledger path; default the user data directory |
| `CIACOLA_CONFIG` | config file; default `ciacola.toml` when present, otherwise empty |
| `CIACOLA_HTTP` | port for the board and the agents' MCP endpoint |
| `CIACOLA_CONCURRENCY` | turns in flight, default 4 |
| `CIACOLA_NO_RECOVER` | skip startup recovery |
| `CIACOLA_OPERATOR_TOKEN_FD` | inherited descriptor containing the human HTTP root bearer |
| `RUST_LOG` | tracing filter, default `warn` |

Four surfaces, one process:

- **stdio MCP** for the operator: every verb including the destructive
  ones (`kill`, `prune`, `resolve_finding`, `open_pr`, the schedule
  tools).
- **Authenticated HTTP MCP at `/mcp`** for the agents themselves, which is
  what makes recursion work. Same router, fewer tools. Every request carries
  the active agent's server-injected `x-ciacola-agent` credential; anonymous,
  unknown, and retired callers are refused before MCP dispatch.
- **Authenticated HTTP MCP at `/mcp-operator`** for a human holding the root
  bearer. Provider-backed agent credentials are refused.
- **The board at `/board`**, plain HTML, auto-refreshing.

`ciacola.example.toml` is annotated and is the fastest way to see what
is configurable.

## Two limit planes

The rolling 24-hour stop and the per-turn ceiling are intentionally different
contracts.

The rolling USD/provider-token stop is an admission breaker. It observes
settled ledger telemetry and decides whether another turn may be queued. It is
not a reservation: an admitted turn can finish beyond it, and concurrent turns
can multiply the overshoot. A known reached stop is final; even the supervised
submission path cannot cross it.

`[limits.providers.<provider>].per_turn_ceiling` is passed to the provider for
one execution. Admission copies the effective value plus the provider's meter,
cache treatment, and enforcement granularity onto the queued turn. Execution
uses that snapshot instead of rereading config, so restart, recovery, and a
later config change cannot silently widen it. Opening and resumed conversations
take the same ceiling.

The unit is deliberately provider-native. Claude declares integer micro-USD.
Supported Codex versions declare a versioned rollout meter: 0.145-0.146 use
weighted non-cached input plus output; 0.147 prefers provider-supplied rollout
units and falls back to that weighted calculation. Both providers observe the
counter at response boundaries, so work already in flight may cross the value
before the provider stops. Codex root and subagent responses may be concurrent
and multiply that soft-boundary overshoot; it is not bounded to exactly one
response. This is enforceable work protection, not a claim of exact
portable-token parity.

No configured ceiling is explicit `UNBOUNDED` behavior and preserves the
backward-compatible automatic path. A configured ceiling that the detected
runtime cannot honor is `UNSUPPORTED`: automatic submission fails before
provider side effects. `send_supervised` may run it only with a durable human
reason, recorded as `OVERRIDDEN`. Every terminal limit result remains a failed
turn with `failure_kind = limit`, its per-turn session, elapsed time, cost, and
whatever usage the provider actually exposed.

Codex's current budget-exhaustion `turn.failed` event omits usage
([openai/codex#37676](https://github.com/openai/codex/issues/37676)). Ciacola
therefore stores usage as unreported, or keeps an earlier partial snapshot,
rather than manufacturing an exact terminal count. The board keeps rolling
admission, live protection support, the durable turn snapshot, and measured
usage visibly separate.

A paid 0.145.0 proof used a one-unit ceiling on both an opening turn and its
resume. Each execution emitted its requested one-word reply and then ended as
`failed` / `failure_kind = limit` with `shared rollout token budget exhausted`.
The opening settled in 3.061 seconds and the resume in 2.490 seconds; both kept
the same provider session and the same persisted
`weighted_non_cached_input_plus_output.v1` response-boundary snapshot. This
demonstrates the expected boundary overshoot—one complete short response was
already available when enforcement stopped further work—but no numeric
overshoot is claimed because 0.145.0 omitted terminal usage on both turns.

## Layout, and where the line falls

```
ciacola-core        the primitive and everything nothing works without
ciacola-kanban      work items, lanes, per-item journeys
ciacola-memory      namespaced key-value that outlives any agent
ciacola-findings    what agents notice about the system
ciacola-schedule    interval schedules; a fire is an ordinary turn
ciacola-refs        reference material; writes no SQL
ciacola-git         live git state; stores nothing at all
ciacola-webhook     inbound HTTP that pokes an agent
ciacola-tuning      what each model has actually cost and achieved
ciacola-repo-worker issue to pull request, in the system's own clone
ciacola             config, the plugin list, main
```

`PluginContext` is the precise statement of what core is: pool, ledger,
executor, notifier, db path, limits, runtime, roles, and loopback config paths.
If a plugin needs something not on that struct, either it belongs in core or
the plugin is reaching.

Everything else is a plugin, including the parts the system leans on
hardest, and they register through the same trait a third party would.
That is deliberate: a built-in with a privileged path leaves the plugin
API a second-class citizen that rots.

Three plugin shapes exist and are worth copying from:

- **SQL-backed** (`kanban`): declares tables and migrations, needs
  UPSERT and subquery DELETE, so it takes the pool.
- **Key-value** (`refs`): declares nothing, uses `Store`, still
  contributes tools, a resource, a board section, and health.
- **Stateless** (`git`, `tuning`): no storage at all, every answer read
  live.

None of the nine implements the same subset of the trait. That is the
defaults working, not a gap.

## Two rules that were learned expensively

**A guard on one path is not a guard.** This bit three times. The spend
limit was added to one submission path while the primary path walked
past it and spent four times the configured stop. Runtime defaults were
applied on two of three agent-creation paths, so the third produced
agents with no isolation. A plugin built its own `Roles` with a default
`Runtime` and silently opted out of every server-wide setting. All
three are now enforced where paths converge: `plugin::submit` for
submission, `Ledger::create_agent` for creation. **When you add a
policy, find the convergence point first.**

**A correct fix can arm a latent bug.** `ensure_clone` passed
`--prune` for its whole life and it did nothing, because a bare clone
configures no refspec for prune to work against. Supplying the missing
refspec was right, and it armed the prune, which then deleted the
worktree branches this system creates and turned agents' commits into
orphans. The next fix exposed a third failure in the same line. When
you fix something that was silently doing nothing, look at what was
relying on it doing nothing.

**Isolation has to be paired with putting back what it removes.**
Hermetic agents inherit no ambient config, which is the point, and
which silently removed the operator's own standing rules: the first
real pull request carried a `Co-Authored-By` trailer those rules
forbid. House rules are now an explicit layer of the system prompt.

## Security boundary

Both HTTP MCP mounts fail closed before dispatch. `/mcp` requires the scoped
credential of an active agent on initialization and every later session
request, including its transport health route. That credential persists
across server restarts and provider-session rotation and is revoked by agent
retirement. There is no in-place credential rotation API; retire and recreate
the agent to mint a replacement. `/mcp-operator` requires a distinct human
root bearer delivered to the server through an inherited descriptor. Agent
identity headers are refused rather than treated as delegated operator
authority, and the two token types cannot substitute for one another. Humans
use stdio or `/mcp-operator`; anonymous `/mcp` clients are no longer supported.
Stdio remains the simplest human operator path.

For upgrade safety, the historical `/tmp/ciacola-mcp-operator.json` path is
still materialized but now points to ordinary `/mcp`. Existing persisted
supervisor conversations therefore lose destructive tools in the safe
direction instead of retaining anonymous authority or failing on a missing
file. Retire or re-provision them as ordinary coordinator agents; new
provider roles with `surface = "operator"` are refused.

This is process authorization inside a single-user laptop product, not
multi-tenant isolation. A hostile process already running as the same OS user
may be able to inspect another process when no OS sandbox prevents it. Use a
separate account or stronger containment when workloads do not share that
trust boundary.

## Known broken, in the order it will annoy you

1. **`CLAUDE_CONFIG_DIR` isolates the login.** A fresh `claude_home`
   authenticates as nobody. `claude setup-token` plus `token_env` is
   the route through, and the server warns at boot.
2. **`wait` is shadowed in mcp-repl.** `wait` is one of the six verbs
   and also an mcp-repl built-in (wait for a background task), and the
   built-in wins: typing `wait` gets "no tasks in this session to wait
   for". `find wait` lists both, so the client knows about the clash
   and simply has no way to express "the tool". `call wait
   {"agent_id": "...", "seq": 1}` reaches it. Renaming the verb to suit
   one client is the wrong direction, so this is filed upstream as
   [mcp-repl#87](https://github.com/joshrotenberg/mcp-repl/issues/87)
   and nothing here changes.
## What to do next, roughly in order

**Real use, on more than one repository.** The system has taken exactly
one issue to a merged pull request. That went well (it moved code
*down* a crate rather than adding a dependency edge, and audited a test
before deleting it), but one is not a sample. Try a second repo, and
try something that is not software: roles assume a system prompt plus a
thin template, the kanban assumes discrete items, `git` simply goes
quiet. Whether the primitive holds outside code is the interesting
unknown.

**Continue dogfooding the Codex provider.** The adapter is now a separate crate behind
the same registry as Claude. It preserves Codex thread ids as soon as the
JSONL stream announces them, resumes through the provider's separate command,
records real tokens as unpriced, applies strict scoped MCP with env-backed
identity headers, and treats sandbox plus native execution policy as Codex's
authority surface. Scripted tests cover the contract. Configure both its
rolling provider token breaker and native per-turn ceiling before scheduling
it unattended; startup advertises protection only for wrapper-tested CLI
versions. A paid one-unit 0.145.0 proof now covers both opening and resume and
records the response-boundary result above. The useful next proof is broader
repository coverage and provider versions. A supervised real run has since
produced and validated `claude-wrapper` PR #780.

**Session mining, later.** Provider-specific `claude_home` and `codex_home` make
the server's transcripts a separate, attributable corpus, which is what makes
mining them meaningful. Solito is changing substantially, so integration is
deliberately on hold. When it settles, the sharp first question is not
clustering but **granted versus
used tools**: which tools does a role grant that its agents never call, and
does an agent that reports verifying actually have the tool call to show for
it. Findings are self-report; transcripts are observed behaviour, and the gap
between them is the interesting signal.

**Streaming, scoped.** Not token text, which nobody is watching. Tool calls
and phase transitions, recorded as turn events and rendered, answer "is it
doing something sensible" without reading everything. Codex JSONL now arrives
live, while the provider-neutral event sink deliberately carries only durable
session and cumulative token-usage observations; widen that contract deliberately
before exposing provider-specific event shapes.

**The board, properly.** It is 355 lines of hand-rolled HTML with a
five-second meta refresh, and it has carried further than it deserves
to. Two separable questions.

*Making it optional* is easy and consistent: move it to a
`ciacola-board` crate that takes a `Ledger` and an `Arc<PluginHost>`
and returns a `Router`, and let the binary merge it or not. The eight
plugins that contribute sections already import `ciacola_core::board`
for three helpers (`esc`, `usd`, `chip`), so those helpers stay in core
and the renderer moves out. Nothing in core needs the board.

*Making it good* is the interesting one, and the toolkit question has a
clearer answer here than it would for most apps, because of a property
worth protecting: **the board has no build step.** `cargo run` and it
is there. Adding `trunk`, `cargo-leptos`, or `npm` is a permanent tax
on every contributor and every CI run, and it should be paid only for
something that cannot be had otherwise.

The LiveView instinct is right, and the reason it is right is
architectural rather than aesthetic: the state is entirely server-side.
The ledger *is* the state. A client-side framework's core value is
owning client state, and this board has none worth owning. What it
needs is live updates and a handful of coarse actions (answer a gate,
kill a turn, resolve a finding, open a pull request), which is
precisely the shape htmx serves: the server renders fragments, the
client swaps them, nothing is built.

So the recommendation is **axum plus htmx plus SSE first**, which keeps
the no-build-step property and reuses machinery that already exists
(tower-mcp's HTTP transport is already an SSE server, and notifications
already flow through it). Reach for Leptos or Dioxus when the board
grows genuine client state that a server round trip cannot serve:
client-side filtering of large tables, a dependency graph you can drag,
optimistic updates. Those are real reasons and none of them apply yet.
A React or Svelte SPA is the least appropriate of the options here,
because it buys client state management at the cost of a second
language, a second toolchain, and a JSON API that duplicates the MCP
surface.

**The board is for *watching*; the REPL is for *doing*.** That split
was decided rather than defaulted into, and it is why there is no
ciacola REPL. mcp-repl is 18,757 lines, of which the reusable
machinery is roughly 7,000 (schema contracts, surface search, the wire
tracer, the reedline editor) and the REPL loop itself is the other
5,600 in `lib.rs`. Writing a native one duplicates all of it, and the
copy drifts.

The alternative that was taken instead: **make the server worth
completing against.** A generic client already knows `send` takes an
`agent_id`, because the schema says so, and what it cannot know is
which ids exist right now. So the server answers `completion/complete`
from the ledger, closed sets are real enums instead of documented
strings, and `instructions` carries a front door that mcp-repl
markdown-renders into its banner. None of it names a client and all of
it works in any client that implements the protocol.

That leaves the board free to get much better at watching (timelines,
per-agent transcripts, spend over time, what is stuck and for how
long) and to stay read-only.

**Publishing.** Names are free on crates.io. `ciacola-core` is the only
one with a public API worth stabilising; the plugins are examples as
much as products.

## Things deliberately not built

Recorded so they do not get re-proposed without new evidence.

- **Stage hooks** (pre/post create, pre/post prompt). The motivating
  case, model and effort selection, is answered by `model_stats` plus a
  `spawn_role` override, which keeps the judgment with the agent. A
  hook moves it into config, the opposite direction from everything
  else here. Mechanical context injection is the case that would
  justify one, and there is exactly one instance of it.
- **A typed event enum or pub-sub bus.** Cron, webhooks, and any future
  poller differ only in what wakes them; they all end at
  `submit_turn`. One producer shape, one consumer.
- **Plugin dependencies.** If plugin A needs plugin B's data, A calls
  B's MCP tool like anyone else.
- **A CRUD facade over the pool.** Every storage plugin needs SQL a
  key-value API cannot express, and the admission guard that makes
  turns correct is a subquery inside an INSERT. `Store` is offered on
  top for plugins that want the easy path.
- **A shutdown hook on the plugin trait.** Core has no graceful
  shutdown to honour it yet.
