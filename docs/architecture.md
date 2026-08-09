# Architecture

## Durable conversations

An agent is a durable provider conversation. It exists while no process is
running. A turn is one execution against that conversation. The provider owns
its session or thread; Ciacola owns the durable identity, prompt, policy,
lineage, state, telemetry, and recovery evidence.

That distinction drives the system:

- `spawn` creates state and spends nothing;
- `send` writes a queued turn before notifying an executor;
- an executor atomically claims one queued row;
- provider session events are persisted while work is running;
- terminal settlement records only telemetry the provider actually supplied;
  and
- recovery resumes known conversation state instead of treating paid work as
  an idempotent queue job.

The SQLite ledger, not an in-memory queue, is the source of truth. The default
polling executor discovers durable queued rows. The channel executor is a lower
latency alternative behind the same `TurnExecutor` trait.

## Startup and dispatch boundary

Both executors start behind one closed, monotonic readiness gate. Startup:

1. parses configuration and captures the provider-child environment;
2. consumes credential descriptors;
3. opens and migrates the ledger;
4. detects provider capabilities;
5. assembles roles and plugins;
6. builds every MCP and board route;
7. binds the complete loopback listener;
8. reconciles crash recovery while dispatch remains closed; and
9. opens dispatch.

Configuration, plugin, router, or bind failure therefore cannot launch a paid
provider turn against an unavailable internal MCP endpoint. Recovery cannot
race a newly claimed row and mistake it for a pre-crash orphan.

## Core boundary

`PluginContext` is the practical line around core. Core owns:

- provider-neutral agent intent, capability checks, outcome, and telemetry;
- ledger schema and transactional state transitions;
- turn admission, execution, cancellation, and recovery;
- authenticated identity and role-spawn authorization;
- the six core MCP verbs; and
- the board shell and shared rendering vocabulary.

Provider implementations live in separate Claude and Codex adapter crates.
Core resolves a provider key through `ProviderRegistry` and does not depend on
either wrapper.

## Plugins

In-tree plugins use the same `Plugin` contract intended for third-party code.
A plugin may contribute:

- MCP tools for agent and operator surfaces;
- roles;
- board sections;
- HTTP routes;
- health data;
- background loops;
- persistent-agent configuration; and
- pruning behavior for its own state.

Current plugins include repository work, schedules, webhooks, findings,
kanban, memory, references, Git inspection, roles, and model tuning.

The repository worker remains a plugin even though it is central to dogfood.
That keeps issue assignment and GitHub workflow policy out of the durable turn
engine.

## Surfaces

One process serves:

- interactive operator MCP over stdio;
- human-bearer operator MCP at `/mcp-operator`;
- authenticated agent MCP at `/mcp`;
- the server-rendered board at `/board`; and
- plugin HTTP routes on the same loopback listener.

Agent identity is inserted by authenticated transport middleware, not parsed
from tool arguments. Operator and agent routers are assembled separately, so a
tool absent from an agent surface cannot be reached by claiming a different
role in prose.

## Roles and authority

A role is a reusable provider definition with prompt, model, effort, tools,
containment, turn caps, and typed arguments. Persistent agents instantiate
roles at startup; operators and agents can create ephemeral role instances at
runtime.

Role-spawn preflight derives the authenticated parent, checks the requested
surface and provider-tool policy, prevents child tools from exceeding the
parent, and enforces durable lineage depth. The returned authorization is the
only parentage persisted by spawn paths.

Provider-backed operator roles are disabled. The delegated-supervision ADR
requires a distinct isolated broker principal and platform attestation before
any narrow privileged action can be enabled.

## Repository journey

The repository worker models ownership and publication as durable state rather
than inferring it from directories:

```text
repository + issue
        |
        v
durable assignment claim
        |
        v
bare clone -> managed worktree -> implementer agent
        |
        v
reviewed exact commit -> leased push -> one PR identity
        |
        v
retained inspection or guarded cleanup
```

Agent creation and assignment activation share one SQLite transaction. A crash
before activation leaves a discoverable non-active claim; a failure during the
transaction cannot leave an unmanaged agent. Restart reconciliation validates
the stored agent, worktree, branch, bare repository, and lifecycle invariants.

Publication has its own durable expected-head and observed-PR state. Cleanup
persists intent and authorization before filesystem mutation so retry after a
partial failure remains safe and auditable.

## Telemetry and limits

Cost and usage are typed provenance, not nullable numbers presented as zero.
Reported complete cost, reported partial cost, unpriced work, and unavailable
cost remain distinct. Usage can be complete, a streamed lower bound, or
unreported.

Rolling admission evaluates durable windows. Per-turn protection is a provider
capability copied into each queued row. Execution reconstructs policy from the
persisted snapshot and refuses adapter drift after restart.

The board reads the same ledger and plugin projections used by MCP. Its live
refresh is an invalidation signal, not a second client-side state model.

## Repository layout

<!-- markdownlint-disable MD013 -->

| Crate | Responsibility |
| --- | --- |
| `ciacola-agent` | Provider-neutral intent, capabilities, events, outcomes, environment |
| `ciacola-agent-claude` | Claude wrapper adapter |
| `ciacola-agent-codex` | Codex wrapper adapter |
| `ciacola-core` | Ledger, execution, admission, recovery, identity, roles, core MCP |
| `ciacola` | Product assembly, configuration, credentials, HTTP and stdio transports |
| `ciacola-board` | Server-rendered supervision board |
| `ciacola-repo-worker` | Durable issue, worktree, PR, and cleanup journey |
| remaining `ciacola-*` crates | In-tree plugins through the common contract |

<!-- markdownlint-enable MD013 -->

## Contributor orientation

Run the same gate as CI:

```sh
just
```

The workspace pins `tower-mcp`, `claude-wrapper`, `codex-wrapper`, and
`git-spawn` to exact Git revisions. See [CONTRIBUTING.md](../CONTRIBUTING.md)
before linking sibling checkouts or changing the lockfile.

[HANDOFF.md](../HANDOFF.md) records historical design evidence and paid dogfood
observations. It is useful context, but current startup, configuration,
operations, and security behavior belong in the product documentation and code
tests.
