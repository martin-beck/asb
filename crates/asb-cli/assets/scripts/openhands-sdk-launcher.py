"""ASB-owned OpenHands SDK launcher for sealed native terminal replay.

Python only adapts the installed SDK's public TerminalAction and
TerminalObservation types. It does not open, parse, or retain a cassette or
native sidecar: the ASB Rust bridge is the sole sidecar consumer.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from typing import Any

from openhands.sdk.tool import ToolExecutor


def with_terminal_executor(agent: Any, executor: Any) -> Any:
    """Return an agent with exactly its SDK TerminalTool executor replaced."""
    from openhands.sdk import Tool
    from openhands.tools.terminal import TerminalTool

    configured = False
    tools = []
    for tool in agent.tools:
        if tool.name != TerminalTool.name:
            tools.append(tool)
            continue
        if configured or "executor" in tool.params:
            raise ValueError("OpenHands agent must declare exactly one unconfigured TerminalTool")
        params = dict(tool.params)
        params["executor"] = executor
        tools.append(Tool(name=TerminalTool.name, params=params))
        configured = True
    if not configured:
        raise ValueError("OpenHands agent does not declare a TerminalTool")
    return agent.model_copy(update={"tools": tools})


class NativeTerminalExecutor(ToolExecutor):
    """Strict stdio client for ASB's already-authorized private bridge."""

    def __init__(self, bridge: str, mode: str, sidecar: str, launch_json: str) -> None:
        self._mode = mode
        self._delegate = None
        if mode == "record":
            from openhands.tools.terminal.impl import TerminalExecutor
            self._delegate = TerminalExecutor(working_dir=os.getcwd(), terminal_type="subprocess")
        self._bridge = subprocess.Popen(
            [bridge, "__asb-agent-bridge", "openhands", "--mode", mode,
             "--sidecar", sidecar, "--launch-json", launch_json],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=sys.stderr,
            text=True, encoding="utf-8",
        )

    def __call__(self, action: Any, conversation: Any = None) -> Any:
        del conversation
        from openhands.sdk import TextContent
        from openhands.tools.terminal import TerminalObservation
        from openhands.tools.terminal.metadata import CmdOutputMetadata

        request = {
            "kind": "terminal_action", "schema_version": 1,
            "command": action.command, "is_input": action.is_input,
            "timeout": action.timeout, "reset": action.reset,
        }
        if self._delegate is not None:
            observation = self._delegate(action)
            request = {"kind": "record_terminal", "schema_version": 1,
                       "action": request, "observation": {
                           "kind": "terminal_observation", "schema_version": 1,
                           "content": observation.text, "is_error": observation.is_error,
                           "command": observation.command, "exit_code": observation.exit_code,
                           "timeout": observation.timeout,
                           "metadata": observation.metadata.model_dump(),
                       }}
        assert self._bridge.stdin is not None
        assert self._bridge.stdout is not None
        self._bridge.stdin.write(json.dumps(request, separators=(",", ":")) + "\n")
        self._bridge.stdin.flush()
        line = self._bridge.stdout.readline()
        if not line:
            raise RuntimeError("ASB native terminal bridge stopped without an observation")
        response = json.loads(line)
        if response.get("kind") == "error" or response.get("schema_version") != 1:
            raise RuntimeError("ASB native terminal bridge rejected terminal action")
        if self._delegate is not None:
            return observation
        expected = {"kind", "schema_version", "content", "is_error", "command", "exit_code", "timeout", "metadata"}
        if set(response) != expected or response["kind"] != "terminal_observation":
            raise RuntimeError("ASB native terminal bridge returned an invalid observation")
        return TerminalObservation(
            content=[TextContent(text=response["content"])],
            is_error=response["is_error"], command=response["command"],
            exit_code=response["exit_code"], timeout=response["timeout"],
            metadata=CmdOutputMetadata(**response["metadata"]),
        )

    def close(self) -> None:
        if self._delegate is not None:
            self._delegate.close()
        if self._bridge.stdin is not None:
            self._bridge.stdin.close()
        status = self._bridge.wait()
        if status != 0:
            raise RuntimeError("ASB native terminal bridge did not consume the sealed replay")


class UnrecordedTerminalExecutor(ToolExecutor):
    """Replay-only terminal guard when the sealed track has no tool sidecar."""

    def __call__(self, action: Any, conversation: Any = None) -> Any:
        del action, conversation
        raise RuntimeError("OpenHands replay requested an unrecorded terminal action")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=("record", "replay"), required=True)
    parser.add_argument("--sidecar", required=True)
    parser.add_argument("--launch-json", required=True)
    parser.add_argument("--task", required=True)
    options = parser.parse_args()
    bridge = os.environ.get("ASB_RUNTIME_EXECUTABLE")
    if not bridge:
        raise RuntimeError("ASB runtime bridge is unavailable")
    from openhands.sdk import LLM, LocalConversation
    from openhands_cli.utils import get_default_cli_agent

    llm = LLM(model=os.environ["LLM_MODEL"], api_key=os.environ["LLM_API_KEY"],
              base_url=os.environ.get("LLM_BASE_URL"), usage_id="agent")
    sidecar = options.sidecar or os.environ.get("ASB_OPENHANDS_NATIVE_RECORD_SIDECAR", "")
    # ConversationState serializes the agent in LocalConversation's constructor.
    # Keep that persisted agent declarative, then attach the bridge only to the
    # in-memory agent used by this run: the bridge owns OS handles and is not
    # serializable state.
    base_agent = get_default_cli_agent(llm)
    conversation = LocalConversation(
        base_agent,
        workspace=os.getcwd(),
        persistence_dir=os.environ.get("OPENHANDS_CONVERSATIONS_DIR"),
    )
    if options.mode == "replay" and not sidecar:
        # No record-side terminal event is still a valid LLM-only track.  Keep
        # the SDK tool declaration identical, but reject any newly requested
        # native action rather than manufacturing a bridge input.
        executor = UnrecordedTerminalExecutor()
    else:
        if not sidecar:
            raise RuntimeError("ASB native terminal sidecar is unavailable")
        executor = NativeTerminalExecutor(bridge, options.mode, sidecar, options.launch_json)
    conversation.agent = with_terminal_executor(base_agent, executor)
    try:
        # ask_agent is intentionally stateless and never executes tools. The
        # ordinary conversation loop is required for the SDK to dispatch the
        # injected TerminalTool through this adapter-owned executor.
        conversation.send_message(options.task)
        conversation.run()
        answer = final_agent_text(conversation)
        print(json.dumps({"type": "result", "text": answer}, separators=(",", ":")))
    finally:
        try:
            conversation.close()
        finally:
            executor.close()
    return 0


def final_agent_text(conversation: Any) -> str | None:
    """Return an authoritative completed answer, or leave it unavailable."""
    from openhands.sdk.conversation.state import ConversationExecutionStatus
    from openhands.sdk.event.conversation_error import ConversationErrorEvent
    from openhands.sdk.event.llm_convertible.action import ActionEvent
    from openhands.sdk.event.llm_convertible.message import MessageEvent
    from openhands.sdk.event.llm_convertible.observation import AgentErrorEvent, UserRejectObservation
    from openhands.sdk.tool.builtins.finish import FinishAction
    from openhands.sdk import TextContent

    if conversation.state.execution_status != ConversationExecutionStatus.FINISHED:
        return None
    for event in reversed(list(conversation.state.events)):
        if isinstance(event, (AgentErrorEvent, ConversationErrorEvent, UserRejectObservation)):
            return None
        if isinstance(event, MessageEvent):
            if event.llm_message.role == "user":
                return None
            if event.source == "agent" and event.llm_message.role == "assistant":
                text = "".join(
                    content.text
                    for content in event.llm_message.content
                    if isinstance(content, TextContent)
                ).strip()
                return text or None
        if isinstance(event, ActionEvent) and event.source == "agent":
            if event.tool_name == "finish" and isinstance(event.action, FinishAction):
                return event.action.message.strip() or None
            return None
    return None


if __name__ == "__main__":
    raise SystemExit(main())
