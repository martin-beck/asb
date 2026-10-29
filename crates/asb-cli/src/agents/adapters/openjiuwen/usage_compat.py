"""Preserve partial Anthropic streaming usage in OpenJiuwen 0.1.19.post2.

The upstream chunk addition replaces the start event's input usage with the
delta's output-only usage. This process-local compatibility fix merges the
cumulative fields; it does not alter model requests or generated content.
"""
import atexit
import base64
import hashlib
import os
import runpy
import sys
import json

try:
    from openjiuwen.core.foundation.llm.schema.message_chunk import AssistantMessageChunk
except ImportError as error:
    sys.stderr.write(f"ASB could not enable OpenJiuwen usage compatibility: {error}\n")
    raise SystemExit(1)

_original_add = AssistantMessageChunk.__add__


def _add_with_usage(left, right):
    result = _original_add(left, right)
    before = left.usage_metadata
    after = right.usage_metadata
    if before is not None and after is not None and result.usage_metadata is not None:
        fields = (
            "input_tokens", "output_tokens", "cache_tokens", "cache_read_tokens",
            "cache_miss_tokens", "cache_write_tokens", "cache_creation_input_tokens",
        )
        merged = {}
        for field in fields:
            a = getattr(before, field, None)
            b = getattr(after, field, None)
            if a is not None or b is not None:
                merged[field] = max(a or 0, b or 0)
        merged["total_tokens"] = merged.get("input_tokens", 0) + merged.get("output_tokens", 0)
        result.usage_metadata = result.usage_metadata.model_copy(update=merged)
    return result


AssistantMessageChunk.__add__ = _add_with_usage

# OpenJiuwen 0.1.19.post2 emits TraceSchema objects for trace events.  Its
# stream-json renderer unconditionally reads ``chunk.index`` (which belongs to
# OutputSchema), aborting after the first tool call.  Keep stream-json output
# compatible by preserving the upstream shape while treating trace events as
# unindexed.  This is deliberately scoped to the launched client process.
try:
    from openjiuwen.harness.cli.ui import runner as _runner
except ImportError:
    # The usage-only compatibility fixture does not ship the CLI UI module.
    # The real pinned client does, so leave the stream renderer untouched in
    # that narrow test environment.
    _runner = None


async def _output_stream_json_compat(stream):
    async for chunk in stream:
        payload = chunk.payload
        line = {
            "type": chunk.type,
            "index": getattr(chunk, "index", None),
            "payload": payload if isinstance(payload, (str, dict)) else str(payload),
        }
        _runner._write_terminal(json.dumps(line, ensure_ascii=False, default=str))
        _runner._write_terminal("\n")


if _runner is not None:
    _runner._output_stream_json = _output_stream_json_compat

# ASB launches OpenJiuwen with stdin closed, so the upstream confirmation rail
# otherwise stops a non-interactive benchmark at the first file edit.  The
# benchmark workspace is an isolated checkout owned by ASB; auto-approve only
# the two file mutation tools in this child process and retain all other
# confirmation decisions from the upstream rail.
try:
    from openjiuwen.harness.rails.interrupt.confirm_rail import ConfirmInterruptRail

    _original_is_auto_confirmed = ConfirmInterruptRail._is_auto_confirmed

    @staticmethod
    def _is_auto_confirmed_for_benchmark(config, key):
        return key in {"write_file", "edit_file"} or _original_is_auto_confirmed(config, key)

    ConfirmInterruptRail._is_auto_confirmed = _is_auto_confirmed_for_benchmark
except ImportError:
    pass

# OpenJiuwen owns its grep implementation directly, rather than exposing an
# MCP transport.  Record exactly its bounded tool result during a live run and
# consume that exact result from ASB's sealed private sidecar during replay.
# The wire payload is deliberately private: only ASB's cassette layer seals it.
_record_path = os.environ.get("ASB_OPENJIUWEN_NATIVE_TOOL_RECORD")
_replay_path = None
if "--asb-native-tool-replay" in sys.argv:
    _index = sys.argv.index("--asb-native-tool-replay")
    if _index + 1 >= len(sys.argv):
        raise SystemExit("ASB OpenJiuwen native replay sidecar is missing")
    _replay_path = sys.argv[_index + 1]
    del sys.argv[_index:_index + 2]

_native_events = []
_native_cursor = 0
_native_alias = "openjiuwen-grep-1"

def _canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")

def _event(name, **fields):
    return json.dumps({"event": name, **fields}, separators=(",", ":"))

def _digest(value):
    return hashlib.sha256(value).hexdigest()

def _payload(value):
    return base64.b64encode(value).decode("ascii")

def _load_replay():
    events = []
    with open(_replay_path, "r", encoding="utf-8") as source:
        for line in source:
            events.append(json.loads(line))
    return events

if _replay_path:
    _native_events = _load_replay()
elif _record_path:
    os.makedirs(os.path.dirname(_record_path), exist_ok=True)
    _native_events = [
        json.loads(_event("launch", session="openjiuwen-native-1", program_sha256=_digest(b"openjiuwen-grep-v1"))),
        json.loads(_event("session_alias", alias=_native_alias, session="openjiuwen-native-1")),
    ]

try:
    from openjiuwen.harness.tools.filesystem import GrepTool
    from openjiuwen.core.foundation.tool.schema import ToolOutput

    _original_grep_invoke = GrepTool.invoke

    async def _grep_with_native_replay(self, inputs, **kwargs):
        global _native_cursor
        request = _canonical({"tool": "grep", "inputs": inputs})
        if _replay_path:
            exchanges = [event for event in _native_events if event.get("event") == "write"]
            if _native_cursor >= len(exchanges):
                raise RuntimeError("ASB OpenJiuwen native replay has no recorded grep request")
            write = exchanges[_native_cursor]
            output = _native_events[_native_events.index(write) + 1]
            if write.get("alias") != _native_alias or write.get("request_b64") != _payload(request):
                raise RuntimeError("ASB OpenJiuwen native replay grep request differs from recording")
            _native_cursor += 1
            return ToolOutput.model_validate(json.loads(base64.b64decode(output["output_b64"])))
        output = await _original_grep_invoke(self, inputs, **kwargs)
        if _record_path:
            encoded = _canonical(output.model_dump())
            _native_events.extend([
                json.loads(_event("poll", alias=_native_alias)),
                json.loads(_event("write", alias=_native_alias, request_sha256=_digest(request), request_b64=_payload(request))),
                json.loads(_event("output", alias=_native_alias, response_sha256=_digest(encoded), output_b64=_payload(encoded))),
            ])
        return output

    GrepTool.invoke = _grep_with_native_replay
except ImportError:
    pass

@atexit.register
def _seal_native_tool_sidecar():
    if not _record_path or len(_native_events) == 2:
        return
    _native_events.extend([
        json.loads(_event("exit", alias=_native_alias, success=True)),
        json.loads(_event("workspace_effect", sha256=_digest(b"openjiuwen-grep-v1"))),
    ])
    with open(_record_path, "w", encoding="utf-8") as destination:
        for event in _native_events:
            destination.write(json.dumps(event, separators=(",", ":")) + "\n")

sys.argv = sys.argv[1:]
runpy.run_path(sys.argv[0], run_name="__main__")
if _replay_path:
    _recorded_exchanges = sum(1 for event in _native_events if event.get("event") == "write")
    if _native_cursor != _recorded_exchanges:
        raise SystemExit("ASB OpenJiuwen native replay left recorded grep exchanges unconsumed")
