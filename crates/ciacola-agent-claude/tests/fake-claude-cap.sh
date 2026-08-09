#!/usr/bin/env bash
set -euo pipefail

printf '%s\n' '{"type":"assistant","message":{"id":"cap-msg","usage":{"input_tokens":8,"output_tokens":2}}}'
printf '%s\n' '{"type":"result","subtype":"error_max_turns","is_error":true,"total_cost_usd":1.25,"num_turns":60,"errors":["Reached maximum number of turns (60)"]}'
exit 1
