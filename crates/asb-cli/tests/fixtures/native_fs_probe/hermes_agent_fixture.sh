#!/bin/sh
set -eu
server=''
tape=''
cleanup() {
  if [ -n "$tape" ]; then kill "$tape" 2>/dev/null || true; wait "$tape" 2>/dev/null || true; fi
  if [ -n "$server" ]; then kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true; fi
}
trap cleanup EXIT
if [ "$ASB_HERMES_NATIVE_MODE" = record ]; then
  /usr/bin/python3 -B -S /provider.py "$ASB_FIXTURE_MOCK_DIR" > "$ASB_FIXTURE_MOCK_DIR/server.log" 2>&1 &
  server=$!
  n=0
  while [ ! -f "$ASB_FIXTURE_MOCK_DIR/port" ]; do
    n=$((n+1)); [ "$n" -lt 200 ] || exit 42; sleep 0.05
  done
  upstream="http://127.0.0.1:$(cat "$ASB_FIXTURE_MOCK_DIR/port")"
  ASB_TAPE_READY_FILE="$ASB_FIXTURE_MOCK_DIR/tape-ready.json" /asb-node /asb-scripts/llmtape.mjs record --port 0 --upstream "$upstream" --out "$ASB_FIXTURE_LLM_CASSETTE" > "$ASB_FIXTURE_MOCK_DIR/tape.log" 2>&1 &
else
  ASB_TAPE_MISMATCH_EVIDENCE="$ASB_FIXTURE_MOCK_DIR/mismatch.json" ASB_TAPE_READY_FILE="$ASB_FIXTURE_MOCK_DIR/tape-ready.json" /asb-node /asb-scripts/llmtape.mjs replay --port 0 --cassette "$ASB_FIXTURE_LLM_CASSETTE" > "$ASB_FIXTURE_MOCK_DIR/tape.log" 2>&1 &
fi
tape=$!
n=0
while [ ! -f "$ASB_FIXTURE_MOCK_DIR/tape-ready.json" ]; do
  n=$((n+1)); [ "$n" -lt 200 ] || exit 43; sleep 0.05
done
port=$(/usr/bin/python3 -S -c 'import json,sys; print(json.load(open(sys.argv[1]))["port"])' "$ASB_FIXTURE_MOCK_DIR/tape-ready.json")
export OPENAI_API_KEY=fixture-token
export OPENAI_BASE_URL="http://127.0.0.1:$port/v1"
set -- -z 'Create the requested fixture file.' --provider openai-api --model fixture-model
if [ "${ASB_FIXTURE_UNRESTRICTED:-0}" != 1 ]; then set -- "$@" --toolsets terminal; fi
timeout 1800 /asb-hermes/venv/bin/python -c 'import sys; from hermes_cli.main import main; sys.exit(main())' \
  "$@" --usage-file "$ASB_FIXTURE_MOCK_DIR/usage.json" > "$ASB_FIXTURE_MOCK_DIR/agent.log" 2>&1
if [ "${ASB_FIXTURE_SWE_PATCH:-0}" = 1 ]; then
  test -n "$(git diff --name-only)"
else
  test "$(cat asb-real-agent.txt)" = fixture
fi
if [ "${ASB_FIXTURE_NATIVE_FILES:-0}" != 1 ]; then
  if [ "$ASB_HERMES_NATIVE_MODE" = record ]; then
    test -f /tmp/asb-native-agent-executed
  else
    test ! -e /tmp/asb-native-agent-executed
  fi
fi
