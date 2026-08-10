#!/bin/bash
# Whole-binary graceful-shutdown fixture: answer the startup version probe,
# then hold one turn in flight forever. The marker file carries this process
# id so the test can wait on observable state and reap the sleeper.
set -eu

for argument in "$@"; do
    if [ "$argument" = "--version" ]; then
        echo 'codex-cli 0.145.0'
        exit 0
    fi
done

for argument in "$@"; do
    if [ "$argument" = "-" ]; then
        /bin/cat >/dev/null
        break
    fi
done

echo '{"type":"thread.started","thread_id":"thread-graceful-shutdown"}'
echo '{"type":"turn.started"}'
echo "$$" >"$TEST_MARKER_DIR/turn.pid"
# exec keeps the pid written above, so killing it kills the sleep too.
exec /bin/sleep 600
