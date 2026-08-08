# ciacola

A laptop-local server that runs coding agents as durable, resumable
conversations, fronted entirely by MCP.

*Ciacola* is Venetian for a long, circular conversation. It is what a
swarm of agents talking each other into a conclusion looks like from
the outside, and it is what this system's unit actually is: not a job,
not a task, but a conversation that persists.

**Status: early.** It has taken one real GitHub issue to a merged pull
request unattended. Everything here works and nothing here is stable.

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

Agents are given these same verbs over loopback HTTP, which is all
"multi-agent orchestration" turns out to require. A conductor spawning
debaters is a prompt, not a framework.

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

It is also not a lock-in, which is what lets it stay small. Agents are
handed an MCP config; any other MCP server can be added to it and the
agent cannot tell the difference. A plugin earns its keep only when it
needs something core owns.

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

## Running it

For a durable server, copy the annotated config and start it:

```sh
cp ciacola.example.toml ciacola.toml
cargo run -p ciacola
```

Then open the board at `http://127.0.0.1:4823/board`.

`ciacola.toml` is optional; when absent the server starts empty. The
ledger defaults to `$XDG_DATA_HOME/ciacola/ciacola.db`, or to
`$HOME/.local/share/ciacola/ciacola.db` when `XDG_DATA_HOME` is unset.
Set `CIACOLA_DB` to override it. The resolved path is printed at startup.
Before adding schedules, set the `daily_warn_usd` and `daily_stop_usd`
circuit breakers in the copied config. See `ciacola.example.toml` for the
annotated settings and `HANDOFF.md` for the design, the known-broken list,
and what to do next.

## License

MIT or Apache-2.0, at your option.
