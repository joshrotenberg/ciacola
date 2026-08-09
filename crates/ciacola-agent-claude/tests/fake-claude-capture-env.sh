#!/usr/bin/env bash
set -euo pipefail

capture_dir="$1"
phase="$2"
shift 2

/usr/bin/env | /usr/bin/sort > "$capture_dir/$phase.env"
printf '%s\n' "$@" > "$capture_dir/$phase.args"
printf '%s\n' "launched" > "$capture_dir/$phase.marker"
printf '%s\n' "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"sess-$phase\"}"
printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"$phase reply\",\"session_id\":\"sess-$phase\",\"total_cost_usd\":0.01,\"num_turns\":1,\"is_error\":false,\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}"
