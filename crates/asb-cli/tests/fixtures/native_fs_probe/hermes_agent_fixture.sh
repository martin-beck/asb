#!/bin/sh
set -eu
/usr/bin/python3 -S /provider.py "$ASB_FIXTURE_MOCK_DIR" > "$ASB_FIXTURE_MOCK_DIR/server.log" 2>&1 &
server=$!
trap 'kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true' EXIT
n=0
while [ ! -f "$ASB_FIXTURE_MOCK_DIR/port" ]; do
  n=$((n+1)); [ "$n" -lt 200 ] || exit 42; sleep 0.05
done
export OPENAI_API_KEY=fixture-token
export OPENAI_BASE_URL="http://127.0.0.1:$(cat "$ASB_FIXTURE_MOCK_DIR/port")/v1"
timeout 1800 /asb-hermes/venv/bin/python -c 'import sys; from hermes_cli.main import main; sys.exit(main())' \
  -z 'Create the requested fixture file.' --provider openai-api --model fixture-model --toolsets terminal \
  --usage-file "$ASB_FIXTURE_MOCK_DIR/usage.json" > "$ASB_FIXTURE_MOCK_DIR/agent.log" 2>&1
test "$(cat asb-real-agent.txt)" = fixture
if [ "$ASB_HERMES_NATIVE_MODE" = record ]; then
  test -f /tmp/asb-native-agent-executed
else
  test ! -e /tmp/asb-native-agent-executed
fi
