# Configuration

Ciacola reads `ciacola.toml` from the current directory. Set `CIACOLA_CONFIG`
to require a different path. A missing default file means an empty
configuration; a missing explicitly selected file is an error.

The complete annotated template is
[`ciacola.example.toml`](../ciacola.example.toml). Configuration parsing denies
unknown fields so misspellings fail during startup instead of changing policy
silently.

## Environment

<!-- markdownlint-disable MD013 -->

| Variable | Meaning | Default |
| --- | --- | --- |
| `CIACOLA_CONFIG` | Explicit configuration file | `ciacola.toml` when present, otherwise empty config |
| `CIACOLA_DB` | SQLite ledger path | `$XDG_DATA_HOME/ciacola/ciacola.db`, then `$HOME/.local/share/ciacola/ciacola.db` |
| `CIACOLA_HTTP` | Loopback HTTP port | `4823` |
| `CIACOLA_CONCURRENCY` | Maximum provider turns in flight | `4` |
| `CIACOLA_EXECUTOR` | `channel` selects notification dispatch; any other value uses polling | polling |
| `CIACOLA_OPERATOR_TOKEN_FD` | Unix descriptor containing the human HTTP root bearer | HTTP operator access disabled |
| `CIACOLA_CLAUDE_TOKEN_FD` | Unix descriptor containing one Claude OAuth token | authenticated provider home |
| `CIACOLA_CODEX_TOKEN_FD` | Unix descriptor containing one Codex API key | authenticated provider home |
| `RUST_LOG` | Tracing filter | `warn` |

<!-- markdownlint-enable MD013 -->

`CIACOLA_NO_RECOVER` is a test and diagnostic escape hatch that skips explicit
startup reconciliation. It is not a normal operating mode; polling still sees
queued ledger rows.

Secrets do not belong in TOML, argv, or exported environment values. The three
`*_TOKEN_FD` variables contain descriptor numbers, not credentials. See
[Security](security.md#credential-ingress).

## Runtime

`[runtime]` defines defaults inherited by roles and agents.

```toml
[runtime]
default_provider = "claude" # or "codex"
hermetic = "full"           # full, project, or none
sandbox = "workspace-write-no-network" # Codex only; omit for Claude
claude_home = "~/.local/share/ciacola/claude"
codex_home = "~/.local/share/ciacola/codex"
house_rules_file = "~/.config/ciacola/house-rules.md"
provider_env_passthrough = ["SSH_AUTH_SOCK", "HTTPS_PROXY"]
```

Every provider child starts from an empty environment. Ciacola restores a
small path, home, identity, temporary-directory, locale, and platform baseline,
then exact names in `provider_env_passthrough`. There are no globs. Missing
names remain absent. The selected adapter removes its own auth, routing, cloud,
and config selectors before applying the intended provider home and credential.

Opting in SSH agents, proxies, askpass programs, GitHub tokens, or client
bearers grants their authority to the direct provider child. Review the
[security boundary](security.md#provider-child-environment) first.

## Limits

```toml
[limits]
daily_warn_usd = 5.0
daily_stop_usd = 10.0
max_spawn_depth = 3

[limits.providers.codex]
daily_warn_tokens = 2_000_000
daily_stop_tokens = 4_000_000
per_turn_ceiling = 250_000

[limits.providers.claude]
per_turn_ceiling = 2_000_000 # $2.00 in integer micro-USD
```

There are two independent limit planes:

1. Rolling 24-hour USD or token thresholds decide whether another turn may be
   admitted. They are circuit breakers, not reservations. A turn admitted
   below the threshold runs to completion, and concurrent turns can multiply
   the overshoot.
2. `per_turn_ceiling` is copied onto one queued turn and reapplied on opening
   and resume. Its unit and cache treatment are provider-declared. Both current
   adapters check at response boundaries, so in-flight work can overshoot.

A missing per-turn ceiling is visibly `UNBOUNDED`. A configured ceiling that
the detected provider cannot honor is `UNSUPPORTED` and blocks automatic work
before provider launch. A supervised send may acknowledge unavailable
protection or incomplete telemetry with a durable reason, but it cannot cross
a known reached rolling stop.

`max_spawn_depth = 0` disables the depth limit. Positive values count durable
agent lineage and refuse a child beyond the configured depth.

## Providers

### Claude

- Auth: an authenticated `claude_home`, the normal HOME-based Claude login, or
  `CIACOLA_CLAUDE_TOKEN_FD` on Unix.
- Resume: provider-assigned Claude session id.
- Usage: Claude usage and USD cost when present; missing values remain
  unreported rather than zero.
- Tools: named Claude tool grants.
- Sandbox: Ciacola does not claim Claude permission settings are an OS sandbox.
- Ceiling: integer micro-USD through Claude's native maximum-budget setting,
  observed at provider-response boundaries.

### Codex

- Auth: an authenticated `codex_home`, the normal HOME-based Codex login, or
  `CIACOLA_CODEX_TOKEN_FD` on Unix.
- Resume: provider-assigned Codex thread id.
- Usage: input, output, and cached tokens; Ciacola intentionally carries no
  provider price table, so Codex turns are unpriced.
- Tools and sandbox: Codex native execution policy with `read-only`,
  `workspace-write`, or `workspace-write-no-network` containment.
- Ceiling: tested Codex CLI 0.145 and 0.146 use weighted non-cached input plus
  output; 0.147 prefers provider-supplied rollout units and otherwise uses that
  fallback. Untested versions advertise no enforceable ceiling.

Codex budget failures can omit terminal usage
([openai/codex#37676](https://github.com/openai/codex/issues/37676)). Ciacola
stores usage as unreported or retains an earlier partial snapshot. It never
invents a terminal measurement.

Stored conversations cannot change provider after their first recorded turn.
Retire and recreate the agent to change backend.

## Repository worker

```toml
[plugins.repo-worker]
root = "~/.local/share/ciacola/repos"
repos = ["OWNER/REPO", "OWNER/SECOND-REPO"]
branch_templates = { "OWNER/REPO" = "fix/{slug}" }
```

Only listed repositories can be assigned. Ciacola owns bare clones and
worktrees under `root`; do not point it at an operator checkout. The default
branch policy is `agent/{slug}`. A repository template may contain the one
`{slug}` placeholder and is validated before durable or Git mutation.

The repository worker ships `repo-manager` and `issue-implementer` roles. A
configured role with the same name replaces the shipped definition. The
`issue-implementer` override must declare exactly `repo`, `issue`, and
`worktree` arguments because `start_issue` supplies those values.

## Roles and persistent agents

Roles are reusable definitions exposed to `spawn_role`. Persistent agents are
upserted by name on every boot: their definition follows config while identity
and conversation remain durable.

```toml
[[roles]]
name = "summarizer"
description = "Summarizes one pull request."
provider = "codex"
inherit_provider_tools = true
sandbox = "read-only"
arguments = ["repo", "pr"]
system_prompt = "Summarize PR {{pr}} in {{repo}}."

[[agents]]
name = "daily-summary"
role = "summarizer"
arguments = { repo = "OWNER/REPO", pr = "123" }
```

Named provider tools and `inherit_provider_tools` are mutually exclusive.
Provider-backed `surface = "operator"` roles are rejected. Delegation policy
has a reserved parseable shape, but any configured policy fails startup until
a process-isolated backend exists.

Plugin-specific persistent-agent configuration lives under
`[agents.plugins.<plugin>]`; top-level plugin configuration lives under
`[plugins.<plugin>]`. Core passes those tables to the owning plugin without
interpreting them.

## Webhooks and schedules

Webhooks and schedules can submit unattended work, so configure observable
rolling stops and enforceable per-turn ceilings first. A backend with missing
required telemetry or unsupported configured protection fails automatic
admission closed.

See the annotated example for current plugin shapes. Use the MCP `health`,
`hooks`, and `schedules` tools to inspect effective runtime state.

## Freshness rule

Behavior-changing changes should update this reference, the annotated example,
or the relevant operations/security document in the same pull request. Config
examples should be parsed or exercised by tests whenever practical.
