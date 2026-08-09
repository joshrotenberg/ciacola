#!/usr/bin/env bash
set -euo pipefail

capture="$1"
shift
printf '%s\n' "$@" > "$capture"
printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-existing"}'
printf '%s\n' '{"type":"result","subtype":"success","result":"resumed reply","session_id":"sess-existing","total_cost_usd":0.01,"num_turns":1,"is_error":false,"usage":{"input_tokens":3,"output_tokens":1}}'
