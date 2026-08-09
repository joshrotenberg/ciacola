#!/usr/bin/env bash
set -euo pipefail

printf '%s\n' 'Not authenticated. Run `claude login`.' >&2
exit 1
