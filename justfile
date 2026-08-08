# The same gate CI runs, so a green `just` means a green PR.
default: fmt lint test doc

fmt:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

doc:
    RUSTDOCFLAGS=-Dwarnings cargo doc --workspace --no-deps

# Format in place, for when the check above fails.
fix:
    cargo fmt --all

run *ARGS:
    cargo run -p ciacola {{ARGS}}

# The pleasant way to drive it. See HANDOFF.md.
repl:
    mcp-repl -- cargo run -p ciacola

# Develop against ../tower-mcp, ../claude-wrapper, ../codex-wrapper, and
# ../git-spawn instead of the pinned revs. The config itself is gitignored,
# so it never reaches CI or an agent. What it produces can: see `unlink`.
link:
    cp .cargo/config.toml.example .cargo/config.toml
    @echo "siblings patched in; run 'just unlink' before committing"

# Drops the patch and repairs Cargo.lock, which is the part that bites.
# Building while linked rewrites the four entries to local paths by
# dropping their `source` line, and a lockfile like that builds on
# exactly one machine: the one that produced it.
unlink:
    rm -f .cargo/config.toml
    cargo update -w
    @git diff --quiet Cargo.lock || echo "Cargo.lock repaired; commit it"

# What the nightly drift lane does: are the pins still good against the
# four mains? Answers without touching the checked-in Cargo.toml.
drift:
    #!/usr/bin/env bash
    set -euo pipefail
    tmp=$(mktemp -d)
    git archive HEAD | tar -x -C "$tmp"
    python3 -c "
    import pathlib, re
    p = pathlib.Path('$tmp/Cargo.toml')
    s, n = re.subn(r'rev = \"[0-9a-f]{40}\"', 'branch = \"main\"', p.read_text())
    assert n == 4, f'expected 4 pinned revs, rewrote {n}'
    p.write_text(s)"
    cargo build --manifest-path "$tmp/Cargo.toml" --workspace
    rm -rf "$tmp"
