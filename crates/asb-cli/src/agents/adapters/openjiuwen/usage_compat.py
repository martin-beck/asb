"""Preserve partial Anthropic streaming usage in OpenJiuwen 0.1.19.post2.

The upstream chunk addition replaces the start event's input usage with the
delta's output-only usage. This process-local compatibility fix merges the
cumulative fields; it does not alter model requests or generated content.
"""
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

sys.argv = sys.argv[1:]
runpy.run_path(sys.argv[0], run_name="__main__")
