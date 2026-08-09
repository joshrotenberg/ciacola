# Security

## Scope

Ciacola is a single-operator, loopback-only product for supervised local use.
It authenticates its own HTTP surfaces and narrows provider authority, but it
does not isolate mutually hostile processes running as the same OS user.

Use a separate OS account, VM, container, or platform sandbox when the provider
process must not inspect the operator's accessible files or process metadata.

## Surfaces and principals

One process exposes four surfaces:

<!-- markdownlint-disable MD013 -->

| Surface | Principal | Authority |
| --- | --- | --- |
| stdio MCP | Interactive human who started the process | Full operator tools |
| `/mcp-operator` | Human root bearer | Same operator profile as stdio |
| `/mcp` | Active agent credential inserted by Ciacola | Agent tools and granted plugins only |
| `/board` | Loopback browser user | Read-only supervision HTML |

<!-- markdownlint-enable MD013 -->

The complete `/mcp` mount, including its transport health route, requires
exactly one valid `x-ciacola-agent` credential on every request. Missing,
malformed, unknown, and retired credentials return the same unauthorized
response before MCP initialization or dispatch. Agent credentials persist
across server and provider-session restarts and are revoked on retirement.
There is no in-place rotation API; retire and recreate the agent.

The HTTP operator mount accepts only the human root bearer. Agent credentials
cannot substitute for it. Provider-backed `surface = "operator"` roles are
rejected. The delegated-supervision policy vocabulary exists only as a
fail-closed seam; runtime authority remains unavailable until a process-isolated
backend can attest the provider principal. See
[ADR 0001](adr/0001-process-isolated-delegated-supervision.md).

## Credential ingress

The simplest operator path is stdio and needs no network credential. Optional
HTTP operator access uses a root bearer delivered through a Unix inherited
descriptor:

```sh
cargo build --locked -p ciacola
operator_token="$(openssl rand -hex 32)"
CIACOLA_OPERATOR_TOKEN_FD=3 ./target/debug/ciacola \
  3< <(printf '%s' "$operator_token")
```

The environment variable contains only the descriptor number. Ciacola reads a
bounded UTF-8 value, closes the descriptor, and removes the metadata before
starting Tokio or any provider child. Supplying `CIACOLA_OPERATOR_TOKEN` is
rejected. Omit the descriptor to disable HTTP operator access; stdio remains.

An authenticated provider home is the normal provider credential source. On
Unix, one startup credential per provider can instead use
`CIACOLA_CLAUDE_TOKEN_FD` or `CIACOLA_CODEX_TOKEN_FD`. Build first so Cargo and
build scripts never inherit those descriptors. The selected adapter receives
one redacted in-memory credential and injects only its canonical child variable.
The secret reaches neither argv, TOML, nor SQLite.

Legacy `token_env` and `codex_token_env` settings are rejected. A persisted
agent carrying the legacy marker must be replaced or retired and recreated;
supplying a new descriptor does not rewrite historical authority silently.

Descriptor credential ingress is Unix-only. Windows uses stdio operator access
and separately authenticated provider homes.

## Provider child environment

Every Claude and Codex opening and resume starts with a cleared direct-child
environment. Ciacola restores only:

- executable path and home/user identity;
- temporary-directory selection;
- locale and timezone values; and
- essential Windows process variables when present.

`[runtime].provider_env_passthrough` adds exact names. There are no globs and
missing values remain absent. Git continues to use HOME-based config; SSH
agents, Git overrides, proxies, cloud credentials, GitHub credentials,
`CIACOLA_*`, and client bearers are absent by default.

The selected adapter removes its own auth, routing, cloud, and config selectors
even when listed, then applies the intended home and credential. Per-turn
`CIACOLA_MCP_*` variables are generated after filtering to carry scoped MCP
headers.

This guarantee covers the direct provider child. A provider can intentionally
copy values to descendants, read accessible files such as credential stores,
or invoke tools that hold their own authority. Exact passthrough of
`SSH_AUTH_SOCK`, proxy URLs, askpass programs, `GH_TOKEN`, or `MCP_BEARER`
grants that authority deliberately.

## Tool and capability ceilings

Every turn is checked against the selected provider's declared capabilities
before launch. Unsupported security constraints fail; unsupported comfort or
telemetry requests are surfaced honestly. An agent can narrow its child's
tools but cannot grant tools it does not hold.

Claude named tools and Codex native provider tools are distinct policies.
Ciacola does not translate a named Claude allowlist into a Codex security
claim. Codex sandbox modes are provider-native containment; Claude permission
controls are not presented as an OS sandbox.

Role-spawn authorization derives parent identity from the authenticated request
context. Caller prose cannot forge lineage. Agent-surface requests without a
valid identity fail before ledger mutation. Depth, tool inheritance, native
provider tools, and operator-surface rules converge in one core preflight.

## Limits

Rolling admission stops and per-turn ceilings reduce exposure but are not hard
spend reservations:

- rolling stops evaluate settled telemetry before admitting a new turn;
- admitted and concurrent turns can finish beyond a rolling threshold;
- provider-native ceilings are checked at response boundaries and can overshoot
  through in-flight root or subagent work; and
- missing terminal telemetry stays missing instead of becoming measured zero.

A configured protection the current runtime cannot enforce blocks automatic
work before provider launch. Human supervised override requires a durable
reason and cannot cross a known reached rolling stop.

## Repository boundary

The repository worker accepts only configured `owner/name` repositories. It
keeps its own bare clones and managed worktrees under a dedicated root; do not
place the operator checkout there.

An issue assignment is reserved durably before network or filesystem mutation.
The assignment key, worktree, branch, and agent ownership are unique. PR
publication validates repository identity, a clean assigned branch, the exact
reviewed commit, one push destination, and a lease before writing. GitHub
requests use an explicit `github.com/owner/name` target. Cleanup is guarded by
dirty-tree, expected-head, PR-state, and exact-ref deletion checks.

These controls prevent accidental or confused writes inside Ciacola's workflow.
They do not remove authority already present in `gh`, Git config, SSH agents,
provider homes, or exact environment passthrough.

## Network assumptions

The HTTP listener binds `127.0.0.1` only. Loopback is an exposure reduction,
not authentication: local processes can connect, so `/mcp` and
`/mcp-operator` authenticate separately. The board is currently read-only and
has no independent browser authentication. Do not forward or publish the
loopback port to an untrusted network.

Webhooks are plugin routes on the same loopback server. A reverse proxy or
tunnel changes the threat model and must provide its own authentication,
request limits, and trusted forwarding policy.

## What Ciacola does not claim

- multi-user or tenant isolation;
- protection from another hostile same-UID process;
- a secure provider-backed operator/supervisor channel;
- that provider permission prompts equal an OS sandbox;
- exact monetary pricing for unpriced providers;
- exact no-overshoot enforcement at provider response boundaries; or
- safe exposure of the board or MCP routes beyond loopback without another
  security layer.
