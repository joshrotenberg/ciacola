# ciacola

A laptop-local server that runs coding agents as durable, resumable
conversations, fronted entirely by MCP.

*Ciacola* is Venetian for a long, circular conversation. It is what a
swarm of agents talking each other into a conclusion looks like from
the outside, and it is what this system's unit actually is: not a job,
not a task, but a conversation that persists.

**Status: early and supervised.** Real repository work has reached reviewed
pull requests through both provider paths, but unattended operation is still
being hardened. The product shape works; its interfaces are not yet stable.

## The idea

An agent is a durable conversation. The provider keeps the
conversation; ciacola keeps its id and what it cost. So:

- an agent exists while nothing is running,
- a *turn* is one process execution against it,
- and recovery is **resume**, not retry.

That last point is why there is no work queue at the centre. A queue's
durability buys re-execution, which is the one thing paid agent work
must never do, and the durable record a queue would hold already exists:
a turn is written to the ledger before anything is told to run it. The
default executor polls that record, so a turn queued before a crash is
picked up after one, with no queue and no recovery pass involved.

## Six verbs

```
spawn   define an agent; runs nothing, costs nothing
send    say something; returns immediately with a turn number
wait    block until a turn finishes
get     one agent, with its whole conversation
list    every agent, with state, cost, and lineage
kill    stop a running turn; the agent survives
```

Agents are given these same verbs through an authenticated, least-authority
loopback scope, which is all "multi-agent orchestration" turns out to
require. A conductor spawning debaters is a prompt, not a framework.

## Everything else is a plugin

Including the parts it leans on hardest: the kanban, memory, findings,
schedules, references, git state, webhooks, model statistics, and the
repository worker. They register through the same trait a third party
would, because a built-in with a privileged path leaves the plugin API
a second-class citizen that rots.

A plugin contributes to every cross-cutting surface rather than merely
adding tools: board sections, HTTP routes, health statistics, its own
retention policy, background loops, and the roles that know how to use
its tools.

It is also not a lock-in, which is what lets it stay small. Agents are handed
a strict MCP scope; any other MCP server can be added to it and the agent
cannot tell the difference. A plugin earns its keep only when it needs
something core owns.

## Providers

Claude and Codex are built-in adapters behind the same provider contract.
An agent can select `provider = "claude"` or `provider = "codex"`; a
server-wide `default_provider` covers definitions that omit it. Existing
stored conversations cannot be moved between providers after their first
recorded turn; retire one and create a new agent to change backends.

Provider controls stay explicit where the CLIs differ. Claude accepts named
tool grants. Codex uses its native execution policy plus `read-only`,
`workspace-write`, or `workspace-write-no-network` containment. Codex reports
real token usage, but not a monetary price, so the ledger records those turns
as unpriced rather than inventing a dollar value. See `ciacola.example.toml`
for the complete configuration surface.

The same rule applies to per-turn ceilings: the contract is shared, but the
unit is provider-native and versioned. Claude enforces micro-USD through its
native maximum-budget setting. Supported Codex CLIs enforce native rollout
units; depending on the CLI version, those are either weighted non-cached
input plus output or provider-supplied rollout units with that calculation as
a fallback. Neither adapter claims an exact portable-token boundary. Both
providers observe the ceiling at response boundaries, so work already in
flight can cross the configured value. Codex root and subagent responses may
be concurrent, so this is not an upper bound of exactly one response.

## Running it

For a durable server, copy the annotated config and start it:

```sh
cp ciacola.example.toml ciacola.toml
cargo run -p ciacola
```

Then open the board at `http://127.0.0.1:4823/board`.

For interactive operator work, run the server through its stdio MCP surface:

```sh
mcp-repl -- cargo run -p ciacola
```

The ordinary HTTP MCP surface at `/mcp` is for agents, not anonymous clients.
Ciacola injects each active agent's scoped `x-ciacola-agent` credential into
every loopback MCP request. The complete mount, including `/mcp/health`,
rejects missing, malformed, unknown, and retired credentials before MCP
initialization or tool dispatch. The credential remains stable across server
restarts and provider-session rotation, and is revoked when the agent retires;
there is no in-place credential rotation API, so retire and recreate an agent
to mint a replacement. Humans should use stdio or the separately authenticated
operator surface below; there is deliberately no public liveness exception
inside `/mcp`.

The HTTP operator surface at `/mcp-operator` is bearer authenticated. Keep
the root secret in a credential manager, pass it to the server through a
dedicated inherited descriptor, and pass it to an HTTP client through an
ephemeral descriptor-backed profile:

```sh
# Build before the secret descriptor exists, so Cargo and build scripts never
# inherit it. Then start the actual binary in the first terminal.
cargo build --locked -p ciacola
operator_token="$(openssl rand -hex 32)"
CIACOLA_OPERATOR_TOKEN_FD=3 ./target/debug/ciacola \
  3< <(printf '%s' "$operator_token")

# Second terminal. Load or paste the same value into an unexported variable.
mcp-repl --config /dev/fd/3 --server ciacola_operator 3< <(
  printf '[servers.ciacola_operator]\ntransport = "http"\nurl = "http://127.0.0.1:4823/mcp-operator"\nbearer = "%s"\n' \
    "$operator_token"
)
```

These bash/zsh examples carry the secret through pipes. It reaches neither
argv, an exported environment variable, nor a persistent config file.
`mcp-repl` currently warns about a literal bearer in the profile; in this
recipe the complete profile exists only on the pipe, so nothing is saved.
`CIACOLA_OPERATOR_TOKEN_FD` contains only a descriptor number. Ciacola reads
and closes it before starting Tokio or any provider process. Supplying the
secret itself as `CIACOLA_OPERATOR_TOKEN`, or starting the server with an
ambient client-side `MCP_BEARER`, is rejected because startup environment
values can remain visible to same-user process inspection after being unset.
Omit the descriptor to disable human HTTP operator access;
stdio remains available. Changing the root secret and restarting rotates it.
Provider-backed agent credentials are explicitly refused on this mount,
including roles that previously selected `surface = "operator"`. A secure
delegated supervisor channel needs stronger process provenance than a bearer
shared between provider processes and is tracked separately.

`ciacola.toml` is optional; when absent the server starts empty. The
ledger defaults to `$XDG_DATA_HOME/ciacola/ciacola.db`, or to
`$HOME/.local/share/ciacola/ciacola.db` when `XDG_DATA_HOME` is unset.
Set `CIACOLA_DB` to override it. The resolved path is printed at startup.
Before adding schedules, configure both limit planes for the selected
backend:

- The rolling 24-hour admission breakers decide whether another turn may be
  queued. Priced providers use `daily_stop_usd`; unpriced providers such as
  Codex also need `[limits.providers.codex].daily_stop_tokens`. The rolling
  token total is reported input + output, with cached input already included
  in input. USD and token stops are independent, and Ciacola carries no
  provider price table.
- `[limits.providers.<provider>].per_turn_ceiling` bounds each admitted
  provider execution in the provider's declared unit. The effective value,
  meter, cache treatment, and enforcement granularity are copied onto the
  queued turn before dispatch. Recovery and config changes therefore cannot
  silently widen it, and the same ceiling is reapplied whether the turn opens
  or resumes a provider conversation.

Omitting `per_turn_ceiling` is an explicit unbounded default: it does not
block automatic work that otherwise passes admission, and the board labels it
`UNBOUNDED`. Configuring a ceiling that the detected provider/CLI cannot honor
labels it `UNSUPPORTED` and refuses automatic work before provider side
effects. A human may use `send_supervised` with a persisted reason to override
only that unavailable protection (or incomplete rolling telemetry); no
override crosses a known rolling stop.

The two planes solve different problems. Admission is not a reservation: an
already-admitted turn can finish beyond the rolling threshold, and concurrent
turns multiply that exposure. A per-turn ceiling narrows one execution, but a
response-boundary provider can still overshoot through work already in flight;
Codex root/subagent concurrency can multiply that soft-boundary overshoot, and
concurrent Ciacola turns each receive their own ceiling. Codex budget failures
currently omit a terminal usage object
([openai/codex#37676](https://github.com/openai/codex/issues/37676)), so Ciacola
records usage as unreported—or preserves an earlier partial snapshot—rather
than inventing a measurement. See
`ciacola.example.toml` for the annotated settings and `HANDOFF.md` for deeper
design context.

Ciacola is a single-operator, laptop-local product, not a multi-tenant
security boundary. The loopback listener and bearer checks reject callers
that do not hold authority, and strict internal Codex turns ask the CLI to
exclude Ciacola credentials from model-launched shells. Those controls are
defense in depth, not cross-process isolation: a hostile process already
running as the same OS user may still inspect another process on platforms or
provider modes without an OS sandbox. Use a separate OS account or stronger
containment for mutually untrusted local workloads.

## License

MIT or Apache-2.0, at your option.
