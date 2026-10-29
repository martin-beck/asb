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


class NativeTerminalExecutor:
    """Strict stdio client for ASB's already-authorized private bridge."""

    def __init__(self, bridge: str, sidecar: str, launch_json: str) -> None:
        self._bridge = subprocess.Popen(
            [bridge, "__asb-openhands-native-bridge", "--sidecar", sidecar,
             "--launch-json", launch_json],
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
        if self._bridge.stdin is not None:
            self._bridge.stdin.close()
        status = self._bridge.wait()
        if status != 0:
            raise RuntimeError("ASB native terminal bridge did not consume the sealed replay")


def main() -> int:
    parser = argparse.ArgumentParser()
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
    executor = NativeTerminalExecutor(bridge, options.sidecar, options.launch_json)
    agent = with_terminal_executor(get_default_cli_agent(llm), executor)
    conversation = LocalConversation(agent, workspace=os.getcwd())
    try:
        answer = conversation.ask_agent(options.task)
        print(json.dumps({"type": "result", "text": answer}, separators=(",", ":")))
    finally:
        try:
            conversation.close()
        finally:
            executor.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
