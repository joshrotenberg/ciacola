# Contributing

## The gate

```sh
just
```

Format, clippy with `-D warnings`, tests, and docs with `-D warnings`.
It is the same set CI runs, so a green `just` means a green pull
request. Run it before every push.

## Building

```sh
cargo run -p ciacola
```

Nothing else is needed. The board comes up on `4823` and the MCP
server speaks on stdout.

## The three siblings

ciacola depends on three crates of its own, pinned to git revisions:

```toml
tower-mcp      = { git = "...", rev = "..." }   # the MCP layer
claude-wrapper = { git = "...", rev = "..." }   # the provider
git-spawn      = { git = "...", rev = "..." }   # git, without shelling out
```

Revisions rather than published versions because ciacola uses things
that are not released yet: `QueryResult::usage`, which is how a turn
gets its token counts, is one of twenty commits sitting past
claude-wrapper v0.13.5. These become version requirements once the
three settle enough to release.

Revisions rather than branches because the build has to be
reproducible. Agents build this repository in their own clones, and a
failure there is expensive to read; it should not also be a moving
target.

### Working on a sibling at the same time

If you have the checkouts next to this one:

```sh
just link      # patch the three to ../tower-mcp, ../claude-wrapper, ../git-spawn
just unlink    # back to the pinned revs
```

`link` copies `.cargo/config.toml.example` into place, and that path is
gitignored, so it reaches neither CI nor an agent's clone. This matters
more than it looks: **an override on your machine hides a broken pin
from you and from nobody else.** The nightly `drift` lane exists for
the same reason, and `just drift` runs it locally.

When a sibling change lands, bump the rev here in the same pull request
that needs it.

## Conventions

- Conventional-commit prefixes on commits and pull request titles.
- No em dashes.
- Comments explain why, not what. The doc comments in `ciacola-core`
  are the design record and are meant to be read; `HANDOFF.md` is the
  orientation and the known-broken list.
- New behaviour comes with a test. The workspace is thin on them and
  the direction is one way.

## Tests

CI-safe only. Never a live agent CLI: use the scripted provider and
real temporary sqlite files. A test that needs Claude is a manual smoke
test and does not belong in the suite.

## Where the line falls

`PluginContext` is the definition of core. If something needs a field
that is not on it, either it belongs in core or it is reaching. Nine
plugins ship in-tree and they register through the same trait a third
party would, which is the only thing that keeps that trait honest.
