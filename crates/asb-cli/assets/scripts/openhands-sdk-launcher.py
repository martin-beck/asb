"""ASB-owned OpenHands SDK launcher for sealed native terminal replay.

Python adapts the installed SDK's terminal and file-editor tool types. The
Rust bridge owns the terminal sidecar; this adapter owns the editor transcript.
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import subprocess
import sys
import threading
from pathlib import Path
from typing import Any

from openhands.sdk.tool import ToolExecutor


def filesystem_marker(action: str, alias: str) -> bool:
    """Annotate the independent filesystem journal around a terminal call."""
    path = os.environ.get("ASB_FS_MARKER_SOCKET")
    if not path:
        return True
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(5)
            connection.connect(path)
            connection.sendall(
                f"tool-{action}:{alias}:{os.getpid()}:{threading.get_native_id()}\n".encode("ascii")
            )
            with connection.makefile("rb") as response:
                return response.readline(32).strip().isdigit()
    except (OSError, UnicodeError):
        return False


def filesystem_replay_boundary(alias: str) -> bool:
    path = os.environ.get("ASB_FS_REPLAY_SOCKET")
    if not path:
        return True
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(1800)
            connection.connect(path)
            connection.sendall(f"tool-end:{alias}\n".encode("ascii"))
            with connection.makefile("rb") as response:
                return response.readline(32).strip().isdigit()
    except (OSError, UnicodeError):
        return False


def filesystem_replay_begin(alias: str) -> bool:
    path = os.environ.get("ASB_FS_REPLAY_SOCKET")
    if not path:
        return False
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(1800)
            connection.connect(path)
            connection.sendall(f"tool-begin:{alias}\n".encode("ascii"))
            with connection.makefile("rb") as response:
                return response.readline(32).strip().isdigit()
    except (OSError, UnicodeError):
        return False


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


class EditorTape:
    """Adapter-owned editor transcript; no editor calls are disabled."""

    def __init__(self, mode: str, path: str):
        self.mode = mode
        self.path = Path(path)
        self.lock = threading.RLock()
        self.started = 0
        self.flushed = 0
        self.pending: dict[int, dict[str, Any]] = {}
        self.failed = False
        self.closed = False
        self.entries = []
        if mode == "replay":
            data = self.path.read_bytes()
            if len(data) > 64 * 1024 * 1024 or (data and not data.endswith(b"\n")):
                raise RuntimeError("OpenHands editor transcript is invalid")
            self.entries = [json.loads(line) for line in data.splitlines()]
            if len(self.entries) > 16384:
                raise RuntimeError("OpenHands editor transcript is invalid")

    def reserve(self) -> int:
        with self.lock:
            if self.closed:
                raise RuntimeError("OpenHands editor transcript is closed")
            self.started += 1
            return self.started

    def finish_record(self, sequence: int, request: Any, observation: Any,
                      tool: str = "openhands.file_editor") -> None:
        event = {"schema_version": 1, "sequence": sequence, "tool": tool,
                 "request": request, "result": observation}
        with self.lock:
            try:
                encoded = json.dumps(event, ensure_ascii=False, separators=(",", ":"))
                if sequence > 16384 or len(encoded.encode("utf-8")) + 1 > 2 * 1024 * 1024:
                    raise ValueError("editor transcript event exceeds bound")
                self.pending[sequence] = encoded
                with self.path.open("a", encoding="utf-8") as output:
                    while self.flushed + 1 in self.pending:
                        self.flushed += 1
                        output.write(self.pending.pop(self.flushed) + "\n")
                    output.flush()
                    os.fsync(output.fileno())
            except (OSError, TypeError, ValueError):
                self.failed = True

    def replay(self, sequence: int, request: Any, tool: str = "openhands.file_editor") -> Any:
        with self.lock:
            if sequence > len(self.entries):
                raise RuntimeError("OpenHands editor replay has no recorded call")
            event = self.entries[sequence - 1]
            if (not isinstance(event, dict) or event.get("schema_version") != 1
                    or event.get("sequence") != sequence or event.get("tool") != tool
                    or event.get("request") != request or set(event) != {"schema_version", "sequence", "tool", "request", "result"}):
                raise RuntimeError("OpenHands editor call differs from the recording")
            return event["result"]

    def close(self) -> None:
        with self.lock:
            if self.closed:
                return
            self.closed = True
            if self.failed or (self.mode == "record" and self.flushed != self.started):
                raise RuntimeError("OpenHands editor recording did not settle")
            if self.mode == "replay" and self.started != len(self.entries):
                raise RuntimeError("OpenHands editor replay did not consume the transcript")


class FileEditorTapeExecutor(ToolExecutor):
    def __init__(self, delegate: Any, tape: EditorTape):
        self.delegate = delegate
        self.tape = tape

    def __call__(self, action: Any, conversation: Any = None) -> Any:
        from openhands.tools.file_editor.definition import FileEditorObservation
        sequence = self.tape.reserve()
        alias = f"openhands-editor-{sequence}"
        try:
            request = action.model_dump(mode="json")
        except Exception:
            if self.tape.mode != "record":
                raise
            self.tape.failed = True
            request = None
        if self.tape.mode == "record":
            if not os.environ.get("ASB_FS_MARKER_SOCKET"):
                self.tape.failed = True
            began = filesystem_marker("begin", alias)
            try:
                observation = self.delegate(action, conversation=conversation)
            except BaseException:
                self.tape.failed = True
                raise
            finally:
                ended = filesystem_marker("end", alias)
                if not began or not ended:
                    self.tape.failed = True
            if request is not None:
                try:
                    self.tape.finish_record(sequence, request, observation.model_dump(mode="json"))
                except Exception:
                    self.tape.failed = True
            return observation
        result = self.tape.replay(sequence, request)
        observation = FileEditorObservation.model_validate(result)
        if not os.environ.get("ASB_FS_REPLAY_SOCKET") or not filesystem_replay_boundary(alias):
            raise RuntimeError("OpenHands editor filesystem replay boundary is unavailable")
        return observation

    def close(self) -> None:
        self.delegate.close()


class UnrecordedToolExecutor(ToolExecutor):
    """Let unsupported tools run during recording but never claim a full tape."""
    def __init__(self, delegate: Any, tape: EditorTape):
        self.delegate = delegate
        self.tape = tape

    def __call__(self, *args: Any, **kwargs: Any) -> Any:
        if self.tape.mode != "record":
            raise RuntimeError("OpenHands tool has no recorded replay transport")
        self.tape.failed = True
        return self.delegate(*args, **kwargs)

    def close(self) -> None:
        self.delegate.close()


class DelegationTapeExecutor(ToolExecutor):
    """Run nested agents in both modes so their provider calls are replayed."""
    def __init__(self, delegate: Any, tape: EditorTape, *, tool: str = "openhands.task",
                 marker: str = "openhands-task"):
        self.delegate = delegate
        self.tape = tape
        self.tool = tool
        self.marker = marker

    def __call__(self, action: Any, conversation: Any = None) -> Any:
        sequence = self.tape.reserve()
        alias = f"{self.marker}-{sequence}"
        try:
            request = action.model_dump(mode="json")
        except Exception:
            if self.tape.mode != "record":
                raise
            self.tape.failed = True
            request = None
        if self.tape.mode == "record":
            if not os.environ.get("ASB_FS_MARKER_SOCKET"):
                self.tape.failed = True
            began = filesystem_marker("begin", alias)
            try:
                observation = self.delegate(action, conversation=conversation)
            except BaseException:
                self.tape.failed = True
                raise
            finally:
                ended = filesystem_marker("end", alias)
                if not began or not ended:
                    self.tape.failed = True
            if request is not None:
                try:
                    self.tape.finish_record(sequence, request, observation.model_dump(mode="json"),
                                            tool=self.tool)
                except Exception:
                    self.tape.failed = True
            return observation
        expected = self.tape.replay(sequence, request, tool=self.tool)
        if not filesystem_replay_begin(alias):
            raise RuntimeError("OpenHands delegated replay start boundary is unavailable")
        observation = self.delegate(action, conversation=conversation)
        if observation.model_dump(mode="json") != expected:
            raise RuntimeError("OpenHands delegated result differs from the recording")
        if not filesystem_replay_boundary(alias):
            raise RuntimeError("OpenHands delegated replay end boundary is unavailable")
        return observation

    def close(self) -> None:
        self.delegate.close()



class TaskTrackerTapeExecutor(DelegationTapeExecutor):
    """Rebuild tracker memory while replay applies recorded filesystem effects."""
    def __init__(self, delegate: Any, tape: EditorTape):
        super().__init__(delegate, tape, tool="openhands.task_tracker",
                         marker="openhands-tracker")
        self.lock = threading.Lock()
        self.workspace = Path.cwd().resolve()

    def __call__(self, action: Any, conversation: Any = None) -> Any:
        # Writes in the captured workspace belong to the filesystem journal.
        # SDK-owned persistence outside it is not captured: execute that write
        # normally so subsequent SDK and agent reads can observe it.
        with self.lock:
            save_dir = self.delegate.save_dir
            if (self.tape.mode != "replay" or save_dir is None
                    or not Path(save_dir).resolve().is_relative_to(self.workspace)):
                return super().__call__(action, conversation)
            self.delegate.save_dir = None
            try:
                return super().__call__(action, conversation)
            finally:
                self.delegate.save_dir = save_dir


def install_file_editor_tape(tape: EditorTape) -> None:
    """Keep SDK declarations intact and wrap the editor executor it creates."""
    from openhands.sdk.tool import register_tool
    from openhands.tools.file_editor.definition import FileEditorTool
    create = FileEditorTool.create

    def wrapped(_cls: Any, conv_state: Any) -> Any:
        definitions = create(conv_state)
        return [item.set_executor(FileEditorTapeExecutor(item.executor, tape)) for item in definitions]

    FileEditorTool.create = classmethod(wrapped)
    from openhands.tools.task.definition import TaskToolSet
    from openhands.tools.task_tracker.definition import TaskTrackerTool

    tracker_create = TaskTrackerTool.create

    def tracker(_cls: Any, *args: Any, **kwargs: Any) -> Any:
        return [item.set_executor(TaskTrackerTapeExecutor(item.executor, tape))
                for item in tracker_create(*args, **kwargs)]

    TaskTrackerTool.create = classmethod(tracker)
    task_create = TaskToolSet.create

    def delegated(_cls: Any, *args: Any, **kwargs: Any) -> Any:
        return [item.set_executor(DelegationTapeExecutor(item.executor, tape))
                for item in task_create(*args, **kwargs)]

    TaskToolSet.create = classmethod(delegated)
    # The SDK registry captures each class's bound create method at
    # registration time. Re-register through its public API after wrapping.
    for tool_class in (FileEditorTool, TaskToolSet, TaskTrackerTool):
        register_tool(tool_class.name, tool_class)


class NativeTerminalExecutor(ToolExecutor):
    """Strict stdio client for ASB's already-authorized private bridge."""

    def __init__(self, bridge: str, mode: str, sidecar: str, launch_json: str) -> None:
        self._mode = mode
        self._marker_lock = threading.Lock()
        self._bridge_lock = threading.Lock()
        self._marker_count = 0
        self._marker_failed = False
        self._delegate = None
        self._bridge_args = [bridge, "__asb-agent-bridge", "openhands", "--mode", mode,
                             "--sidecar", sidecar, "--launch-json", launch_json]
        self._bridge = None
        if mode == "record":
            from openhands.tools.terminal.impl import TerminalExecutor
            self._delegate = TerminalExecutor(working_dir=os.getcwd(), terminal_type="subprocess")
        else:
            self._start_bridge()

    def _start_bridge(self) -> None:
        if self._bridge is not None:
            return
        self._bridge = subprocess.Popen(
            self._bridge_args,
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
        alias = None
        if self._delegate is not None or os.environ.get("ASB_FS_REPLAY_SOCKET"):
            with self._marker_lock:
                self._marker_count += 1
                alias = f"openhands-{self._marker_count}"
        if self._delegate is not None:
            began = filesystem_marker("begin", alias)
            try:
                observation = self._delegate(action)
            finally:
                ended = filesystem_marker("end", alias)
                if not began or not ended:
                    with self._marker_lock:
                        self._marker_failed = True
            request = {"kind": "record_terminal", "schema_version": 1,
                       "action": request, "observation": {
                           "kind": "terminal_observation", "schema_version": 1,
                           "content": observation.text, "is_error": observation.is_error,
                           "command": observation.command, "exit_code": observation.exit_code,
                           "timeout": observation.timeout,
                           "metadata": observation.metadata.model_dump(),
                       }}
        # Delegate execution is not serialized. Only the shared stdio frame
        # exchange must remain atomic when multiple agents issue calls.
        with self._bridge_lock:
            self._start_bridge()
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
        if os.environ.get("ASB_FS_REPLAY_SOCKET"):
            if not filesystem_replay_boundary(alias):
                raise RuntimeError("OpenHands filesystem replay boundary is unavailable")
        return TerminalObservation(
            content=[TextContent(text=response["content"])],
            is_error=response["is_error"], command=response["command"],
            exit_code=response["exit_code"], timeout=response["timeout"],
            metadata=CmdOutputMetadata(**response["metadata"]),
        )

    def close(self) -> None:
        if self._delegate is not None:
            self._delegate.close()
        if self._bridge is not None:
            if self._bridge.stdin is not None:
                self._bridge.stdin.close()
            status = self._bridge.wait()
            if status != 0:
                raise RuntimeError("ASB native terminal bridge did not consume the sealed replay")
        if self._marker_failed:
            raise RuntimeError("OpenHands filesystem recording boundary was unavailable")


class UnrecordedTerminalExecutor(ToolExecutor):
    """Replay-only terminal guard when the sealed track has no tool sidecar."""

    def __call__(self, action: Any, conversation: Any = None) -> Any:
        del action, conversation
        raise RuntimeError("OpenHands replay requested an unrecorded terminal action")


class SharedTerminalExecutor(ToolExecutor):
    """A child conversation borrows, but never closes, the parent transport."""
    def __init__(self, transport: ToolExecutor):
        self.transport = transport

    def __call__(self, action: Any, conversation: Any = None) -> Any:
        return self.transport(action, conversation)

    def close(self) -> None:
        pass


def install_subagent_terminal(transport: ToolExecutor) -> None:
    """Attach the same terminal transport to SDK-created child conversations."""
    from openhands.tools.task import manager
    from openhands.tools.terminal import TerminalTool
    original = manager.LocalConversation

    def create(*args: Any, **kwargs: Any) -> Any:
        conversation = original(*args, **kwargs)
        if any(tool.name == TerminalTool.name for tool in conversation.agent.tools):
            conversation.agent = with_terminal_executor(conversation.agent, SharedTerminalExecutor(transport))
        return conversation

    manager.LocalConversation = create


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
    from openhands.tools.preset.default import register_builtins_agents
    from openhands_cli.utils import get_default_cli_agent

    # Match the normal CLI initialization so its built-in subagents remain
    # available in the tape-backed launcher.
    register_builtins_agents(enable_browser=False)
    editor_tape = EditorTape(options.mode, os.environ["ASB_OPENHANDS_EDITOR_TAPE"])
    install_file_editor_tape(editor_tape)
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
    install_subagent_terminal(executor)
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
            try:
                executor.close()
            finally:
                editor_tape.close()
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
