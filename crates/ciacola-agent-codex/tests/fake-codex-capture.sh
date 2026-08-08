#!/usr/bin/env bash
set -eu

capture=$1
shift
printf '%s\n' "$@" > "$capture"
printf '%s\n' '{"type":"thread.started","thread_id":"thread-resumed"}'
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"continued"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":4,"output_tokens":2}}'
