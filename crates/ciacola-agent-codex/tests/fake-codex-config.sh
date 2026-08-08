#!/usr/bin/env bash
set -eu

printf '%s\n' 'Error loading config.toml: unknown configuration field `fake_bad_key` in -c/--config override' >&2
exit 1
