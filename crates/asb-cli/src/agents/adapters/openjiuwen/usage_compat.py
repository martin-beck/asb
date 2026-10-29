"""Preserve partial Anthropic streaming usage in OpenJiuwen 0.1.19.post2.

The upstream chunk addition replaces the start event's input usage with the
delta's output-only usage. This process-local compatibility fix merges the
cumulative fields; it does not alter model requests or generated content.
"""
import atexit
import base64
import contextvars
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
_record_dispatch_cursor = 0
# OpenJiuwen can start independent direct tools concurrently.  Invocation
# ancestry is therefore task-local: a process-global mutable list would turn a
# concurrently-started root call into a child of whichever task happened to
# yield first.
_record_invocation_stack = contextvars.ContextVar(
    "asb_openjiuwen_native_invocation_stack", default=()
)
_native_alias = "openjiuwen-native-tools-1"
_message_cursor = 0
_history_cursor = 0
_final_history = []
_history_trace = []
_history_trace_seen = set()
_context_history_hook = False
_queued_final_events = {}
_sealed_history = None
_sealed_final_outputs = None

def _trace_once(item):
    key = (item["kind"], item["index"])
    if key not in _history_trace_seen:
        _history_trace_seen.add(key)
        _history_trace.append(item)
_recorded_outputs = {}
_replayed_renders = {}
_replayed_output_events = {}
# Private, process-local provenance. A final ToolMessage is eligible only
# when AbilityManager rendered the exact output returned by a wrapped native
# tool. Values and tool-call ids are not provenance: synthetic messages can
# reuse a direct call's id.
_final_event_by_result = {}
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

def _owned_replayed_event(output):
    replayed = _replayed_output_events.get(id(output))
    if replayed is not None and replayed[0] is output:
        return replayed[1]
    return None

def _bind_final_event(result, tool_call, event):
    tool_call_id = getattr(tool_call, "id", None)
    if not isinstance(tool_call_id, str) or not tool_call_id or event is None:
        return
    previous = _final_event_by_result.get(id(result))
    if previous is not None and (
        previous[0] is not result or previous[1] is not event or previous[2] != tool_call_id
    ):
        raise RuntimeError("ASB OpenJiuwen native replay final tool provenance changed")
    _final_event_by_result[id(result)] = (result, event, tool_call_id)

def _bound_final_event(result, tool_call_id):
    bound = _final_event_by_result.get(id(result))
    if bound is not None and bound[0] is result and bound[2] == tool_call_id:
        return bound[1]
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
    """Return root exchanges in dispatch order and reject malformed sidecars."""
    exchanges = []
    for index, event in enumerate(events):
        if event.get("event") != "write":
            continue
        if index + 1 >= len(events) or events[index + 1].get("event") != "output":
            raise RuntimeError("ASB OpenJiuwen native replay sidecar has an incomplete exchange")
        invocation = _invocation(event)
        if invocation["depth"] != 0 or invocation["parent_ordinal"] is not None:
            raise RuntimeError("ASB OpenJiuwen native replay sidecar has non-root exchange")
        exchanges.append((invocation["ordinal"], event, events[index + 1]))
    exchanges.sort(key=lambda exchange: exchange[0])
    if len({ordinal for ordinal, _write, _output in exchanges}) != len(exchanges):
        raise RuntimeError("ASB OpenJiuwen native replay sidecar has duplicate root ordinal")
    if any(
        later <= earlier
        for (earlier, _write, _output), (later, _next_write, _next_output)
        in zip(exchanges, exchanges[1:])
    ):
        raise RuntimeError("ASB OpenJiuwen native replay sidecar has unordered root ordinal")
    return [(write, output) for _ordinal, write, output in exchanges]


def _load_final_history():
    """Return one adapter-private final-history stream from sealed outputs."""
    global _sealed_history, _sealed_final_outputs
    if _sealed_history is not None:
        return _sealed_history, _sealed_final_outputs
    history = []
    final_outputs = []
    exchanges = _replay_exchanges(_native_events) if _replay_path else []
    for output in (event for event in _native_events if event.get("event") == "output"):
        envelope = json.loads(base64.b64decode(output["output_b64"]))
        if "history" in envelope:
            if history:
                raise RuntimeError("ASB OpenJiuwen native replay has duplicate final history")
            history = envelope["history"]
        if "history_trace" in envelope:
            trace = envelope["history_trace"]
            if not isinstance(trace, list) or any(
                not isinstance(item, dict)
                or item.get("v") != 1
                or item.get("kind") not in ("history", "dispatch")
                or not isinstance(item.get("index"), int)
                or item["index"] < 0
                or any(not isinstance(value, str) or len(value) != 64 for key, value in item.items() if key.endswith("_sha256"))
                for item in trace
            ):
                raise RuntimeError("ASB OpenJiuwen native replay history trace is malformed")
            if len({(item["kind"], item["index"]) for item in trace}) != len(trace):
                raise RuntimeError("ASB OpenJiuwen native replay history trace is duplicated")
    for _write, output in exchanges:
        envelope = json.loads(base64.b64decode(output["output_b64"]))
        if isinstance(envelope.get("message"), dict):
            final_outputs.append(output)
    if _replay_path and not isinstance(history, list):
        raise RuntimeError("ASB OpenJiuwen native replay final history is missing")
    _sealed_history, _sealed_final_outputs = history, final_outputs
    return history, final_outputs


def _recorded_request(write):
    try:
        value = json.loads(base64.b64decode(write["request_b64"]))
    except (KeyError, TypeError, ValueError):
        raise RuntimeError("ASB OpenJiuwen native replay sidecar has invalid nesting metadata")
    if not isinstance(value, dict) or set(value) != {"tool", "inputs", "_asb_openjiuwen_invocation"}:
        raise RuntimeError("ASB OpenJiuwen native replay sidecar has invalid nesting metadata")
    return value


def _invocation(write):
    """Read the adapter-private direct-tool nesting receipt.

    Some OpenJiuwen tools delegate to another direct tool while producing one
    outer result.  Record sees the delegated invocation first; replay returns
    the sealed outer result and consequently must consume that private child
    exchange without executing it.  The receipt makes that otherwise-implicit
    relationship explicit and fails closed for recordings from a different
    adapter profile.
    """
    value = _recorded_request(write)["_asb_openjiuwen_invocation"]
    if (
        not isinstance(value, dict)
        or set(value) != {"ordinal", "parent_ordinal", "depth"}
        or not isinstance(value["ordinal"], int)
        or value["ordinal"] < 0
        or value["parent_ordinal"] is not None
        and (not isinstance(value["parent_ordinal"], int) or value["parent_ordinal"] < 0)
        or not isinstance(value["depth"], int)
        or value["depth"] < 0
        or (value["depth"] == 0) != (value["parent_ordinal"] is None)
    ):
        raise RuntimeError("ASB OpenJiuwen native replay sidecar has invalid nesting metadata")
    return value


def _matches_request(write, request):
    recorded = _recorded_request(write)
    return (
        recorded["tool"] == request["tool"]
        and recorded["inputs"] == request["inputs"]
    )


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
                rendered = replayed
            else:
                rendered = _original_render_tool_result(tool, tool_call, result)
            event = _owned_replayed_event(result) if _replay_path else _owned_recorded_event(result)
            if event is not None:
                _remember_recorded_render(event, rendered)
                _bind_final_event(result, tool_call, event)
            return rendered

        AbilityManager._render_tool_result = _render_tool_result_with_native_replay
        _original_execute = AbilityManager.execute
        async def _execute_with_native_messages(self, *args, **kwargs):
            global _message_cursor, _history_cursor, _final_history
            results = await _original_execute(self, *args, **kwargs)
            if _context_history_hook:
                # ReActAgent owns the actual model-context insertion. Retain
                # only exact object identity here, then capture/restore at
                # that later boundary rather than guessing from this returned
                # list (which can contain entries later skipped or rewritten).
                for entry in results:
                    if not isinstance(entry, tuple) or len(entry) != 2:
                        continue
                    result, message = entry
                    if result is None or not isinstance(getattr(message, "tool_call_id", None), str):
                        continue
                    event = _bound_final_event(result, message.tool_call_id)
                    if event is not None:
                        _queued_final_events[id(message)] = (message, event)
                return results
            outputs = [event for event in _native_events if event.get("event") == "output"]
            history = []
            for output in outputs:
                envelope = json.loads(base64.b64decode(output["output_b64"]))
                if "history" in envelope:
                    if history:
                        raise RuntimeError("ASB OpenJiuwen native replay has duplicate final history")
                    history = envelope["history"]
                if "history_trace" in envelope:
                    trace = envelope["history_trace"]
                    if not isinstance(trace, list) or any(
                        not isinstance(item, dict)
                        or item.get("v") != 1
                        or item.get("kind") not in ("history", "dispatch")
                        or not isinstance(item.get("index"), int)
                        or item["index"] < 0
                        or any(not isinstance(value, str) or len(value) != 64 for key, value in item.items() if key.endswith("_sha256"))
                        for item in trace
                    ):
                        raise RuntimeError("ASB OpenJiuwen native replay history trace is malformed")
                    if len({(item["kind"], item["index"]) for item in trace}) != len(trace):
                        raise RuntimeError("ASB OpenJiuwen native replay history trace is duplicated")
            if _replay_path and not isinstance(history, list):
                raise RuntimeError("ASB OpenJiuwen native replay final history is missing")
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
                # Direct-tool provenance remains independent from the full
                # provider history capture below.
                event = _bound_final_event(_result, message.tool_call_id)
                if event is not None:
                    envelope = json.loads(base64.b64decode(event["output_b64"]))
                    if _replay_path:
                        if _history_cursor < len(history) and history[_history_cursor].get("native"):
                            if _message_cursor >= len(final_outputs) or event is not final_outputs[_message_cursor]:
                                raise RuntimeError("ASB OpenJiuwen native replay final tool message is missing or out of order")
                            _message_cursor += 1
                    else:
                        if envelope.get("message") is not None:
                            raise RuntimeError("ASB OpenJiuwen native replay has an extra final tool message")
                        envelope["message"] = {"tool_call_id": message.tool_call_id, "content": _normalize_workspace(message.content)}
                        envelope["rendered"] = envelope["message"]["content"]
                        encoded = _canonical(envelope)
                        event["response_sha256"] = _digest(encoded)
                        event["output_b64"] = _payload(encoded)
                if _replay_path:
                    if _history_cursor >= len(history):
                        raise RuntimeError("ASB OpenJiuwen native replay has an extra final history message")
                    recorded = history[_history_cursor]
                    expected = recorded.get("message")
                    if (
                        not isinstance(expected, dict)
                        or not isinstance(expected.get("tool_call_id"), str)
                        or not isinstance(expected.get("content"), str)
                    ):
                        raise RuntimeError(
                            "ASB OpenJiuwen native replay final history message is missing or out of order "
                            f"at index {_history_cursor} "
                            f"(recorded_native={recorded.get('native')!r}, actual_native={event is not None!r})"
                        )
                    if recorded.get("native") and event is None:
                        raise RuntimeError(
                            "ASB OpenJiuwen native replay final history message is missing or out of order "
                            f"at index {_history_cursor} (recorded native message was not reconstructed)"
                        )
                    if recorded.get("native"):
                        sealed = json.loads(base64.b64decode(event["output_b64"])).get("message")
                        if sealed != expected:
                            raise RuntimeError("ASB OpenJiuwen native replay final history/native message disagrees")
                    message.tool_call_id = expected["tool_call_id"]
                    message.content = _materialize_workspace(expected["content"])
                    _trace_once({"v": 1, "kind": "history", "index": _history_cursor,
                        "id_sha256": _digest(message.tool_call_id.encode("utf-8")),
                        "content_sha256": _digest(message.content.encode("utf-8"))})
                    _history_cursor += 1
                else:
                    _final_history.append({
                        "native": event is not None,
                        "message": {
                            "tool_call_id": message.tool_call_id,
                            "content": _normalize_workspace(message.content),
                        },
                    })
            return results
        AbilityManager.execute = _execute_with_native_messages

        # Context insertion is the authoritative final boundary: it receives
        # each ToolMessage after all rails and result filtering. Capture and
        # restore there, never from renderer values or an earlier result list.

        async def _add_final_history_message(message):
            global _message_cursor, _history_cursor
            if isinstance(message, (list, tuple)):
                for item in message:
                    await _add_final_history_message(item)
                return
            if (
                not isinstance(getattr(message, "tool_call_id", None), str)
                or not isinstance(getattr(message, "content", None), str)
            ):
                return
            queued = _queued_final_events.pop(id(message), None)
            event = queued[1] if queued is not None and queued[0] is message else None
            if _replay_path:
                history, final_outputs = _load_final_history()
                if _history_cursor >= len(history):
                    raise RuntimeError("ASB OpenJiuwen native replay has an extra final history message")
                recorded = history[_history_cursor]
                expected = recorded.get("message") if isinstance(recorded, dict) else None
                if (
                    not isinstance(expected, dict)
                    or not isinstance(expected.get("tool_call_id"), str)
                    or not isinstance(expected.get("content"), str)
                ):
                    raise RuntimeError("ASB OpenJiuwen native replay final history message is missing or out of order")
                if recorded.get("native"):
                    if event is None:
                        raise RuntimeError("ASB OpenJiuwen native replay final native message was not inserted")
                    if _message_cursor >= len(final_outputs) or event is not final_outputs[_message_cursor]:
                        raise RuntimeError("ASB OpenJiuwen native replay final tool message is missing or out of order")
                    _message_cursor += 1
                    sealed = json.loads(base64.b64decode(event["output_b64"])).get("message")
                    if sealed != expected:
                        raise RuntimeError("ASB OpenJiuwen native replay final history/native message disagrees")
                message.tool_call_id = expected["tool_call_id"]
                message.content = _materialize_workspace(expected["content"])
                _trace_once({"v": 1, "kind": "history", "index": _history_cursor,
                    "id_sha256": _digest(message.tool_call_id.encode("utf-8")),
                    "content_sha256": _digest(message.content.encode("utf-8"))})
                _history_cursor += 1
            else:
                if event is not None:
                    envelope = json.loads(base64.b64decode(event["output_b64"]))
                    if envelope.get("message") is not None:
                        raise RuntimeError("ASB OpenJiuwen native replay has an extra final tool message")
                    envelope["message"] = {
                        "tool_call_id": message.tool_call_id,
                        "content": _normalize_workspace(message.content),
                    }
                    envelope["rendered"] = envelope["message"]["content"]
                    encoded = _canonical(envelope)
                    event["response_sha256"] = _digest(encoded)
                    event["output_b64"] = _payload(encoded)
                _final_history.append({
                    "native": event is not None,
                    "message": {
                        "tool_call_id": message.tool_call_id,
                        "content": _normalize_workspace(message.content),
                    },
                })

        # Patch the concrete context insertion method rather than one
        # ReActAgent call frame. Some OpenJiuwen paths add a tool result after
        # `_execute_tool_call` returns (and several calls can overlap); an
        # instance-local temporary wrapper misses those model-visible writes.
        # This remains process-local to the adapter launch and binds only the
        # exact queued ToolMessage identity from AbilityManager.
        if _record_path or _replay_path:
            from openjiuwen.core.context_engine.context.context import Context as _ModelContext
            _context_history_hook = True
            _original_context_add_messages = _ModelContext.add_messages

            async def _context_add_messages_with_final_history(self, messages, *args, **kwargs):
                await _add_final_history_message(messages)
                return await _original_context_add_messages(self, messages, *args, **kwargs)

            _ModelContext.add_messages = _context_add_messages_with_final_history
    except ImportError:
        pass

    def _wrap_native_tool(tool_name, tool_type):
        original_invoke = tool_type.invoke
        original_render = tool_type.render_for_llm

        async def _invoke_with_native_replay(self, inputs, **kwargs):
            global _native_cursor, _record_dispatch_cursor
            request = {"tool": tool_name, "inputs": _normalize_workspace(inputs)}
            dispatch_index = _native_cursor if _replay_path else _record_dispatch_cursor
            _trace_once({"v": 1, "kind": "dispatch", "index": dispatch_index,
                "tool_sha256": _digest(tool_name.encode("utf-8")), "shape_sha256": _digest(_canonical(request))})
            if _replay_path:
                exchanges = _replay_exchanges(_native_events)
                if _native_cursor >= len(exchanges):
                    raise RuntimeError(
                        f"ASB OpenJiuwen native replay has no recorded {tool_name} request"
                    )
                write, output = exchanges[_native_cursor]
                invocation = _invocation(write)
                if (
                    write.get("alias") != _native_alias
                    or not _matches_request(write, request)
                    or invocation["depth"] != 0
                    or invocation["parent_ordinal"] is not None
                ):
                    expected_request = _recorded_request(write)
                    actual_request = request
                    raise RuntimeError(
                        "ASB OpenJiuwen native replay "
                        f"{tool_name} request differs from recording "
                        f"(expected_sha256={write.get('request_sha256')}, "
                        f"actual_sha256={_digest(_canonical(request))}, "
                        f"expected_shape={json.dumps(_request_shape(expected_request), sort_keys=True)}, "
                        f"actual_shape={json.dumps(_request_shape(actual_request), sort_keys=True)})"
                    )
                _native_cursor += 1
                envelope = json.loads(base64.b64decode(output["output_b64"]))
                if (
                    not isinstance(envelope, dict)
                    or not {"tool_output", "rendered", "message"}.issubset(envelope)
                    or set(envelope) - {"tool_output", "rendered", "message", "history", "history_trace"}
                    or (envelope["rendered"] is not None and not isinstance(envelope["rendered"], str))
                ):
                    raise RuntimeError(
                        "ASB OpenJiuwen native replay sidecar has no rendered tool result"
                    )
                result = ToolOutput.model_validate(_materialize_workspace(
                    envelope["tool_output"]
                ))
                rendered = envelope["rendered"]
                if isinstance(rendered, str):
                    _replayed_renders[id(result)] = _materialize_workspace(rendered)
                # Retain the result object with its event. ``id()`` alone is
                # not a provenance marker: a reconstructed result can let a
                # discarded object's id be reused before execute() runs.
                _replayed_output_events[id(result)] = (result, output)
                return result
            invocation = None
            invocation_token = None
            if _record_path:
                stack = _record_invocation_stack.get()
                invocation = {
                    "ordinal": _record_dispatch_cursor,
                    "parent_ordinal": stack[-1] if stack else None,
                    "depth": len(stack),
                }
                _record_dispatch_cursor += 1
                invocation_token = _record_invocation_stack.set((*stack, invocation["ordinal"]))
            try:
                output = await original_invoke(self, inputs, **kwargs)
            finally:
                if invocation is not None:
                    _record_invocation_stack.reset(invocation_token)
            # A direct OpenJiuwen tool can invoke wrapped helpers below the
            # model-dispatched call. Their parent may itself be a framework
            # helper without a model-visible result, so sealing a child as an
            # independent replay exchange creates an orphaned cursor entry.
            # The top-level result already contains the helper effect; record
            # only that root interaction for deterministic replay.
            if invocation is not None and invocation["depth"] != 0:
                return output
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
                        request_sha256=_digest(_canonical({**request, "_asb_openjiuwen_invocation": invocation})),
                        request_b64=_payload(_canonical({**request, "_asb_openjiuwen_invocation": invocation})),
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
        if final is None:
            continue
        if (
            not isinstance(envelope.get("rendered"), str)
            or not isinstance(final, dict)
            or not isinstance(final.get("tool_call_id"), str)
            or not isinstance(final.get("content"), str)
        ):
            return
        final_messages += 1
    if final_messages == 0:
        return
    if not _final_history:
        return
    for event in _native_events:
        if event.get("event") == "output":
            envelope = json.loads(base64.b64decode(event["output_b64"]))
            envelope["history"] = _final_history
            envelope["history_trace"] = _history_trace
            encoded = _canonical(envelope)
            event["response_sha256"] = _digest(encoded)
            event["output_b64"] = _payload(encoded)
            break
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
    _recorded_final_messages = sum(
        1
        for event in _native_events
        if event.get("event") == "output"
        and isinstance(json.loads(base64.b64decode(event["output_b64"])).get("message"), dict)
    )
    if _message_cursor != _recorded_final_messages:
        raise SystemExit("ASB OpenJiuwen native replay left final tool messages unconsumed")
    _recorded_history_messages = 0
    for event in _native_events:
        if event.get("event") == "output":
            envelope = json.loads(base64.b64decode(event["output_b64"]))
            if "history" in envelope:
                _recorded_history_messages += len(envelope["history"])
    if _history_cursor != _recorded_history_messages:
        raise SystemExit("ASB OpenJiuwen native replay left final history messages unconsumed")
