#!/usr/bin/env bash
set -eu

printf '%s\n' '{"type":"thread.started","thread_id":"thread-success"}'
printf '%s\n' '{"type":"turn.started"}'
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"fake reply"}}'
printf '%s\n' '{"type":"turn.completed","usage":{"input_tokens":21,"cached_input_tokens":13,"output_tokens":5}}'
