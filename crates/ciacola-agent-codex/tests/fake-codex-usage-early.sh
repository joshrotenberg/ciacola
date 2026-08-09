#!/usr/bin/env bash
set -eu

marker=$1
printf '%s\n' '{"type":"thread.started","thread_id":"thread-usage-early"}'
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"usage persisted"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":34,"cached_input_tokens":21,"output_tokens":8}}'
while [[ ! -f "$marker" ]]; do
  sleep 0.01
done
