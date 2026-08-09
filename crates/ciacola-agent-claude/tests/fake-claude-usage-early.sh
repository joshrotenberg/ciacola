#!/usr/bin/env bash
set -euo pipefail

marker="$1"

printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-early"}'
# Neither an empty object nor null buckets are a reported zero.
printf '%s\n' '{"type":"assistant","message":{"id":"empty","usage":{}},"session_id":"sess-early"}'
printf '%s\n' '{"type":"assistant","message":{"id":"nulls","usage":{"input_tokens":null,"output_tokens":null}},"session_id":"sess-early"}'
# Missing message ids cannot safely be deduplicated; each complete event
# is a distinct provider turn and advances the cumulative snapshot.
printf '%s\n' '{"type":"assistant","message":{"usage":{"input_tokens":4,"output_tokens":1}},"session_id":"sess-early"}'
printf '%s\n' '{"type":"assistant","message":{"usage":{"input_tokens":3,"cache_read_input_tokens":2,"output_tokens":2}},"session_id":"sess-early"}'

while [[ ! -f "$marker" ]]; do
  sleep 0.01
done

# Keep the provider future alive after the snapshot was accepted so the
# test can cancel it at the same boundary as an operator kill.
sleep 30
