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
_message_cursor = 0
_recorded_outputs = {}
_replayed_renders = {}
_replayed_output_events = {}
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

def _owned_recorded_event(output):
    recorded = _recorded_outputs.get(id(output))
    if recorded is not None and recorded[0] is output:
        return recorded[1]
    return None

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
                # Native direct tools are captured as an ordered adapter
                # transcript. Keep the benchmark launch profile serial so a
                # batch of concurrent tool completions cannot reorder the
                # model-facing results between record and replay.
                config.parallel_tool_calls = False
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
    from openjiuwen.core.foundation.tool.base import Tool
    from openjiuwen.core.foundation.tool.schema import ToolOutput

    _original_base_render = Tool.render_for_llm

    def _remember_recorded_render(output_event, rendered):
        envelope = json.loads(base64.b64decode(output_event["output_b64"]))
        if envelope["rendered"] is None:
            envelope["rendered"] = _normalize_workspace(rendered)
            encoded = _canonical(envelope)
            output_event["response_sha256"] = _digest(encoded)
            output_event["output_b64"] = _payload(encoded)

    def _base_render_with_native_replay(self, output):
        replayed = _replayed_renders.get(id(output))
        if replayed is not None:
            return replayed
        rendered = _original_base_render(self, output)
        event = _owned_recorded_event(output)
        if event is not None:
            _remember_recorded_render(event, rendered)
        return rendered

    Tool.render_for_llm = _base_render_with_native_replay

    # AbilityManager is the final adapter-owned boundary before a native tool
    # result becomes a provider message. Capture/replay here as well: upstream
    # may route through a concrete renderer, its base fallback, or a copied
    # ToolOutput, but this method owns the exact final model-facing bytes.
    try:
        from openjiuwen.core.single_agent.ability_manager import AbilityManager
        _original_render_tool_result = AbilityManager._render_tool_result

        @staticmethod
        def _render_tool_result_with_native_replay(tool, tool_call, result):
            replayed = _replayed_renders.get(id(result))
            if replayed is not None:
                return replayed
            rendered = _original_render_tool_result(tool, tool_call, result)
            event = _owned_recorded_event(result)
            if event is not None:
                _remember_recorded_render(event, rendered)
            return rendered

        AbilityManager._render_tool_result = _render_tool_result_with_native_replay
        _original_execute = AbilityManager.execute
        async def _execute_with_native_messages(self, *args, **kwargs):
            global _message_cursor
            results = await _original_execute(self, *args, **kwargs)
            outputs = [event for event in _native_events if event.get("event") == "output"]
            final_outputs = []
            for output in outputs:
                envelope = json.loads(base64.b64decode(output["output_b64"]))
                final = envelope.get("message")
                if isinstance(final, dict):
                    final_outputs.append(output)
            for entry in results:
                if not isinstance(entry, tuple) or len(entry) != 2:
                    continue
                _result, message = entry
                if (
                    _result is None
                    or not isinstance(getattr(message, "tool_call_id", None), str)
                    or not isinstance(getattr(message, "content", None), str)
                ):
                    continue
                if _replay_path:
                    replayed = _replayed_output_events.get(id(_result))
                    event = replayed[1] if replayed is not None and replayed[0] is _result else None
                else:
                    event = _owned_recorded_event(_result)
                if event is None:
                    # Upstream rails may reconstruct a ToolOutput before
                    # returning it. During replay the sealed final-message
                    # sequence is the only admissible candidate. During
                    # recording, accept exactly one unsealed native output;
                    # an ambiguous duplicate is deliberately left unsealed.
                    try:
                        actual = _normalize_workspace(_result.model_dump())
                        if _replay_path:
                            candidates = final_outputs[_message_cursor:_message_cursor + 1]
                        else:
                            candidates = []
                            for output in outputs:
                                envelope = json.loads(base64.b64decode(output["output_b64"]))
                                if envelope.get("message") is None:
                                    candidates.append(output)
                        matches = [
                            candidate for candidate in candidates
                            if json.loads(base64.b64decode(candidate["output_b64"])).get("tool_output") == actual
                        ]
                        if len(matches) == 1:
                            event = matches[0]
                    except (AttributeError, KeyError, TypeError, ValueError):
                        pass
                if event is None:
                    continue
                envelope = json.loads(base64.b64decode(event["output_b64"]))
                if _replay_path:
                    if _message_cursor >= len(final_outputs):
                        raise RuntimeError("ASB OpenJiuwen native replay has an extra final tool message")
                    if event is not final_outputs[_message_cursor]:
                        raise RuntimeError("ASB OpenJiuwen native replay final tool message is out of order")
                    final = envelope.get("message")
                    if not isinstance(final, dict) or final.get("tool_call_id") != message.tool_call_id or not isinstance(final.get("content"), str):
                        raise RuntimeError("ASB OpenJiuwen native replay final tool message is missing or out of order")
                    message.content = _materialize_workspace(final["content"])
                    _message_cursor += 1
                else:
                    if envelope.get("message") is not None:
                        raise RuntimeError("ASB OpenJiuwen native replay has an extra final tool message")
                    envelope["message"] = {"tool_call_id": message.tool_call_id, "content": _normalize_workspace(message.content)}
                    # The final ToolMessage is the model-facing rendering.
                    # Store it here rather than guessing from copied output
                    # values or an earlier renderer invocation.
                    envelope["rendered"] = envelope["message"]["content"]
                    encoded = _canonical(envelope)
                    event["response_sha256"] = _digest(encoded)
                    event["output_b64"] = _payload(encoded)
            return results
        AbilityManager.execute = _execute_with_native_messages
    except ImportError:
        pass

    def _wrap_native_tool(tool_name, tool_type):
        original_invoke = tool_type.invoke
        original_render = tool_type.render_for_llm

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
                envelope = json.loads(base64.b64decode(output["output_b64"]))
                if (
                    not isinstance(envelope, dict)
                    or set(envelope) != {"tool_output", "rendered", "message"}
                    or not isinstance(envelope["rendered"], str)
                ):
                    raise RuntimeError(
                        "ASB OpenJiuwen native replay sidecar has no rendered tool result"
                    )
                result = ToolOutput.model_validate(_materialize_workspace(
                    envelope["tool_output"]
                ))
                rendered = _materialize_workspace(envelope["rendered"])
                _replayed_renders[id(result)] = rendered
                # Retain the result object with its event. ``id()`` alone is
                # not a provenance marker: a reconstructed result can let a
                # discarded object's id be reused before execute() runs.
                _replayed_output_events[id(result)] = (result, output)
                return result
            output = await original_invoke(self, inputs, **kwargs)
            if _record_path:
                # Keep the opaque sidecar portable across fresh replay
                # workspaces. The foreground client still receives the live
                # output; only the sealed replay artifact uses this marker.
                encoded = _canonical({
                    "tool_output": _normalize_workspace(output.model_dump()),
                    "rendered": None,
                    "message": None,
                })
                output_event = json.loads(_event(
                    "output",
                    alias=_native_alias,
                    response_sha256=_digest(encoded),
                    output_b64=_payload(encoded),
                ))
                _native_events.extend([
                    json.loads(_event("poll", alias=_native_alias)),
                    json.loads(_event(
                        "write",
                        alias=_native_alias,
                        request_sha256=_digest(request),
                        request_b64=_payload(request),
                    )),
                    output_event,
                ])
                _recorded_outputs[id(output)] = (output, output_event)
            return output

        def _render_with_native_replay(self, output):
            replayed = _replayed_renders.get(id(output))
            if replayed is not None:
                return replayed
            rendered = original_render(self, output)
            event = _owned_recorded_event(output)
            if event is not None:
                _remember_recorded_render(event, rendered)
            return rendered

        tool_type.invoke = _invoke_with_native_replay
        tool_type.render_for_llm = _render_with_native_replay

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
    final_messages = 0
    for event in _native_events:
        if event.get("event") != "output":
            continue
        envelope = json.loads(base64.b64decode(event["output_b64"]))
        final = envelope.get("message")
        if envelope.get("rendered") is None:
            return
        if final is None:
            continue
        if not isinstance(final, dict) or not isinstance(final.get("tool_call_id"), str) or not isinstance(final.get("content"), str):
            return
        final_messages += 1
    if final_messages == 0:
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
