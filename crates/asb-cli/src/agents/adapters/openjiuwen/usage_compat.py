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

# Consume the adapter-private replay selector before importing OpenJiuwen.
# Its package initialization may inspect argv while building provider features,
# so record and replay must expose an identical upstream CLI shape.
_record_path = os.environ.get("ASB_OPENJIUWEN_NATIVE_TOOL_RECORD")
_replay_path = None
if "--asb-native-tool-replay" in sys.argv:
    _index = sys.argv.index("--asb-native-tool-replay")
    if _index + 1 >= len(sys.argv):
        raise SystemExit("ASB OpenJiuwen native replay sidecar is missing")
    _replay_path = sys.argv[_index + 1]
    del sys.argv[_index:_index + 2]

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

# OpenJiuwen owns its filesystem, shell, and code implementations directly,
# rather than exposing an MCP transport. Record each bounded tool result during
# a live run and consume that exact result from ASB's sealed private sidecar
# during replay. The wire payload is deliberately private: only ASB's cassette
# layer seals it. Replaying one tool selectively is unsafe: a later direct
# filesystem or shell call would otherwise execute against the replay
# workspace and change the provider conversation.
_native_events = []
_native_cursor = 0
_native_alias = "openjiuwen-native-tools-1"
_native_workspace = os.path.realpath(
    os.environ.get("ASB_OPENJIUWEN_NATIVE_TOOL_WORKSPACE", "")
) if os.environ.get("ASB_OPENJIUWEN_NATIVE_TOOL_WORKSPACE") else ""

def _canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")

def _event(name, **fields):
    return json.dumps({"event": name, **fields}, separators=(",", ":"))

def _digest(value):
    return hashlib.sha256(value).hexdigest()

def _payload(value):
    return base64.b64encode(value).decode("ascii")

def _normalize_workspace(value):
    if isinstance(value, str):
        return value.replace(_native_workspace, "<asb-workspace>") if _native_workspace else value
    if isinstance(value, list):
        return [_normalize_workspace(item) for item in value]
    if isinstance(value, dict):
        return {key: _normalize_workspace(item) for key, item in value.items()}
    return value

def _materialize_workspace(value):
    if isinstance(value, str):
        return value.replace("<asb-workspace>", _native_workspace) if _native_workspace else value
    if isinstance(value, list):
        return [_materialize_workspace(item) for item in value]
    if isinstance(value, dict):
        return {key: _materialize_workspace(item) for key, item in value.items()}
    return value

def _request_shape(value):
    if isinstance(value, str):
        return {
            "kind": "string",
            "length": len(value),
            "workspace_marker": "<asb-workspace>" in value,
            "sha256": _digest(value.encode("utf-8")),
        }
    if isinstance(value, list):
        return {"kind": "list", "items": [_request_shape(item) for item in value]}
    if isinstance(value, dict):
        return {"kind": "object", "fields": {key: _request_shape(item) for key, item in sorted(value.items())}}
    return {"kind": type(value).__name__}

def _load_replay():
    events = []
    with open(_replay_path, "r", encoding="utf-8") as source:
        for line in source:
            events.append(json.loads(line))
    return events


def _replay_exchanges(events):
    """Return ordered write/output pairs and reject malformed sidecars."""
    exchanges = []
    for index, event in enumerate(events):
        if event.get("event") != "write":
            continue
        if index + 1 >= len(events) or events[index + 1].get("event") != "output":
            raise RuntimeError("ASB OpenJiuwen native replay sidecar has an incomplete exchange")
        exchanges.append((event, events[index + 1]))
    return exchanges

if _replay_path:
    _native_events = _load_replay()
elif _record_path:
    os.makedirs(os.path.dirname(_record_path), exist_ok=True)
    _native_events = [
        json.loads(_event(
            "launch",
            session="openjiuwen-native-1",
            program_sha256=_digest(b"openjiuwen-native-tools-v2"),
        )),
        json.loads(_event("session_alias", alias=_native_alias, session="openjiuwen-native-1")),
    ]

# Vector-memory requests and asynchronously-created subagents have state that
# does not belong to this benchmark attempt (an embedding cache and scheduling
# respectively).  They make the provider request order depend on leftovers
# outside the sealed tape.  The normal interactive client keeps them; ASB's
# record/replay launch profile deliberately uses only the selected foreground
# agent and its sealed direct-tool sidecar.
if _record_path or _replay_path:
    try:
        from openjiuwen.harness.cli.agent import factory as _agent_factory

        _agent_factory._build_memory_rail = lambda _cfg: None
        _agent_factory._build_subagents = lambda _model: []

        # The upstream CLI otherwise starts an asynchronous image-capability
        # probe in every fresh process.  It is unrelated to these text-only
        # benchmarks, consumes an unrecorded provider request, and makes a
        # fresh replay begin on a different request than its recording.  Pin
        # this adapter launch profile to text-only before the agent is started;
        # interactive OpenJiuwen remains unchanged.
        _create_agent = _agent_factory.create_agent

        def _create_text_only_agent(*args, **kwargs):
            agent, tracker = _create_agent(*args, **kwargs)
            config = getattr(agent, "_deep_config", None)
            if config is not None:
                config.enable_read_image_multimodal = False
            return agent, tracker

        _agent_factory.create_agent = _create_text_only_agent
    except ImportError:
        pass

try:
    from openjiuwen.harness.tools.filesystem import (
        EditFileTool,
        GlobTool,
        GrepTool,
        ListDirTool,
        ReadFileTool,
        WriteFileTool,
    )
    from openjiuwen.harness.tools.shell import BashTool
    from openjiuwen.harness.tools.code import CodeTool
    from openjiuwen.core.foundation.tool.schema import ToolOutput

    def _wrap_native_tool(tool_name, tool_type):
        original_invoke = tool_type.invoke

        async def _invoke_with_native_replay(self, inputs, **kwargs):
            global _native_cursor
            request = _canonical({"tool": tool_name, "inputs": _normalize_workspace(inputs)})
            if _replay_path:
                exchanges = _replay_exchanges(_native_events)
                if _native_cursor >= len(exchanges):
                    raise RuntimeError(
                        f"ASB OpenJiuwen native replay has no recorded {tool_name} request"
                    )
                write, output = exchanges[_native_cursor]
                if (
                    write.get("alias") != _native_alias
                    or write.get("request_b64") != _payload(request)
                ):
                    expected_request = json.loads(base64.b64decode(write["request_b64"]))
                    actual_request = json.loads(request)
                    raise RuntimeError(
                        "ASB OpenJiuwen native replay "
                        f"{tool_name} request differs from recording "
                        f"(expected_sha256={write.get('request_sha256')}, "
                        f"actual_sha256={_digest(request)}, "
                        f"expected_shape={json.dumps(_request_shape(expected_request), sort_keys=True)}, "
                        f"actual_shape={json.dumps(_request_shape(actual_request), sort_keys=True)})"
                    )
                _native_cursor += 1
                return ToolOutput.model_validate(_materialize_workspace(
                    json.loads(base64.b64decode(output["output_b64"]))
                ))
            output = await original_invoke(self, inputs, **kwargs)
            if _record_path:
                # Keep the opaque sidecar portable across fresh replay
                # workspaces. The foreground client still receives the live
                # output; only the sealed replay artifact uses this marker.
                encoded = _canonical(_normalize_workspace(output.model_dump()))
                _native_events.extend([
                    json.loads(_event("poll", alias=_native_alias)),
                    json.loads(_event(
                        "write",
                        alias=_native_alias,
                        request_sha256=_digest(request),
                        request_b64=_payload(request),
                    )),
                    json.loads(_event(
                        "output",
                        alias=_native_alias,
                        response_sha256=_digest(encoded),
                        output_b64=_payload(encoded),
                    )),
                ])
            return output

        tool_type.invoke = _invoke_with_native_replay

    for _tool_name, _tool_type in (
        ("read_file", ReadFileTool),
        ("write_file", WriteFileTool),
        ("edit_file", EditFileTool),
        ("glob", GlobTool),
        ("list_dir", ListDirTool),
        ("grep", GrepTool),
        ("bash", BashTool),
        ("code", CodeTool),
    ):
        _wrap_native_tool(_tool_name, _tool_type)
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
        raise SystemExit("ASB OpenJiuwen native replay left recorded native tool exchanges unconsumed")
