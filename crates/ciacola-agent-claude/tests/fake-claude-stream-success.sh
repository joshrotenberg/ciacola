#!/usr/bin/env bash
set -euo pipefail

printf '%s\n' '{"type":"system","subtype":"init","session_id":"sess-stream","tools":[],"mcp_servers":[]}'
printf '%s\n' '{"type":"assistant","message":{"id":"msg-1","usage":{"input_tokens":10,"cache_read_input_tokens":4,"cache_creation_input_tokens":3,"output_tokens":2}},"session_id":"sess-stream"}'
# A repeated full assistant event must not be charged twice.
printf '%s\n' '{"type":"assistant","message":{"id":"msg-1","usage":{"input_tokens":10,"cache_read_input_tokens":4,"cache_creation_input_tokens":3,"output_tokens":2}},"session_id":"sess-stream"}'
printf '%s\n' '{"type":"assistant","message":{"id":"msg-2","usage":{"input_tokens":5,"output_tokens":3}},"session_id":"sess-stream"}'
printf '%s\n' '{"type":"result","subtype":"success","result":"streamed reply","session_id":"sess-stream","total_cost_usd":0.02,"num_turns":2,"is_error":false,"usage":{"input_tokens":20,"cache_read_input_tokens":7,"cache_creation_input_tokens":2,"output_tokens":6}}'
