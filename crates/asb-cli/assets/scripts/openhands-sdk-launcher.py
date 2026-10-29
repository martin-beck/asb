"""ASB's dormant OpenHands SDK terminal-executor seam.

The ordinary ``openhands`` CLI has no terminal-executor flag.  When ASB gains
the native cassette transport, its project-local launcher can load the normal
agent specification, call :func:`with_terminal_executor`, and construct its
own SDK conversation.  It must not alter the installed CLI or the process-wide
tool registry.

This module deliberately has no entry point and does not persist actions,
observations, prompts, or tool payloads.  The future native transport owns its
own bounded cassette schema and invokes this helper in-process.
"""

from __future__ import annotations

from typing import Any


def with_terminal_executor(agent: Any, executor: Any) -> Any:
    """Return an OpenHands agent configured with one explicit terminal executor.

    OpenHands resolves ``Tool(name=TerminalTool.name, params=...)`` by calling
    ``TerminalTool.create(..., executor=...)``.  Supplying the executor through
    that documented factory parameter avoids monkeypatching the CLI, registry,
    or a constructed agent's private tool map.  The replacement is limited to
    the terminal specification; all other tool specifications are preserved.

    The caller supplies an SDK-compatible ``ToolExecutor``.  ASB intentionally
    supplies none until native cassette transport has a bounded schema.
    """
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
