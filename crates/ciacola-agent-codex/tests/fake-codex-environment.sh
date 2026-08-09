#!/usr/bin/env bash
# Capture the complete direct-child environment, then emit the verified JSONL
# shape the adapter consumes for either an opening or resumed turn.
set -eu

capture_path="$1"
shift
/usr/bin/env >"${capture_path}"

for argument in "$@"; do
    if [ "${argument}" = "-" ]; then
        /bin/cat >/dev/null
        break
    fi
done

echo '{"type":"thread.started","thread_id":"thread-environment"}'
echo '{"type":"turn.started"}'
echo '{"type":"item.completed","item":{"id":"item-1","type":"agent_message","text":"environment ok"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0}}'
