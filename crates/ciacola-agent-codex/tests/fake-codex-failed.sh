#!/usr/bin/env bash
set -eu

printf '%s\n' '{"type":"thread.started","thread_id":"thread-failed"}'
printf '%s\n' '{"type":"turn.started"}'
printf '%s\n' '{"type":"turn.failed","error":{"message":"fake provider failure"},"usage":{"input_tokens":9,"output_tokens":2}}'
exit 1
