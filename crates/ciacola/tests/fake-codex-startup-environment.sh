#!/bin/bash
# Whole-binary #80 fixture: capture version/open/resume direct-child
# environments and emit the Codex JSONL shapes Ciacola consumes.
set -eu

for argument in "$@"; do
    if [ "$argument" = "--version" ]; then
        /usr/bin/env >"$TEST_CAPTURE_DIR/version.env"
        echo 'codex-cli 0.145.0'
        exit 0
    fi
done

turn=open
for argument in "$@"; do
    if [ "$argument" = "resume" ]; then
        turn=resume
    fi
done
/usr/bin/env >"$TEST_CAPTURE_DIR/$turn.env"
if /bin/test -e /dev/fd/9; then
    echo open >"$TEST_CAPTURE_DIR/$turn.fd9"
else
    echo closed >"$TEST_CAPTURE_DIR/$turn.fd9"
fi

for argument in "$@"; do
    if [ "$argument" = "-" ]; then
        /bin/cat >/dev/null
        break
    fi
done

echo '{"type":"thread.started","thread_id":"thread-startup-environment"}'
echo '{"type":"turn.started"}'
echo '{"type":"item.completed","item":{"id":"item-1","type":"agent_message","text":"environment ok"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0}}'
