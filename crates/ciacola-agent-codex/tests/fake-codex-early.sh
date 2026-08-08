#!/usr/bin/env bash
set -eu

marker=$1
printf '%s\n' '{"type":"thread.started","thread_id":"thread-early"}'
while [[ ! -f "$marker" ]]; do
  sleep 0.01
done
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"persisted"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":1}}'
