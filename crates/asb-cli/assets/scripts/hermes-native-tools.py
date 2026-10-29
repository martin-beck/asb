"""ASB-owned Hermes terminal-tool recorder/replayer.

This module is imported by Python's standard ``sitecustomize`` hook before
Hermes imports its tool registry.  It replaces the terminal tool entry point,
not subprocess APIs: every recorded/replayed operation is an explicit Hermes
terminal request and response.  The sidecar is private to an ASB attempt.
"""
import atexit
import base64
import hashlib
import json
import os
from pathlib import Path
import sys


MODE = os.environ.get("ASB_HERMES_NATIVE_MODE", "")
SIDECAR = Path(os.environ["ASB_HERMES_NATIVE_SIDECAR"])
PROGRESS = Path(os.environ["ASB_HERMES_NATIVE_PROGRESS"])
DIAGNOSTIC = Path(os.environ["ASB_HERMES_NATIVE_DIAGNOSTIC"]) if os.environ.get("ASB_HERMES_NATIVE_DIAGNOSTIC") else None
MAX_BYTES = 4 * 1024 * 1024
MAX_PAYLOAD_BYTES = 512 * 1024
SESSION = "terminal-1"


def _encoded(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def _digest(value):
    return hashlib.sha256(value).hexdigest()


def _stable_workdir(workdir):
    """Canonicalize only an attempt-local absolute directory.

    Hermes reports a terminal tool's working directory back to its model.  A
    remote record and replay use distinct guest roots, so that absolute value
    is volatile even though the requested directory is semantically the same.
    Preserve relative and foreign absolute paths exactly: only a path under
    the guest's current workspace becomes a stable workspace-relative name.
    """
    if not isinstance(workdir, str):
        return workdir
    try:
        candidate = Path(workdir)
        if not candidate.is_absolute():
            return workdir
        root = Path.cwd().resolve()
        relative = candidate.resolve().relative_to(root)
    except (OSError, ValueError):
        return workdir
    return "." if not relative.parts else f"./{relative.as_posix()}"


def _request(command, background, timeout, _task_id, _session_id, force, workdir, pty, notify_on_complete, watch_patterns):
    return _encoded({
        "kind": "terminal",
        "schema_version": 1,
        "command": command,
        "background": background,
        "timeout": timeout,
        # Hermes generates these IDs for each process. They route no terminal
        # operation themselves, so exclude only them from the adapter-owned
        # replay identity; command, options, workdir and every response byte
        # remain exact.
        "force": force,
        "workdir": _stable_workdir(workdir),
        "pty": pty,
        "notify_on_complete": notify_on_complete,
        "watch_patterns": watch_patterns,
    })


def _process_request(args):
    """Identity for a registry status action, excluding its ephemeral ID."""
    return _encoded({
        "kind": "process",
        "schema_version": 1,
        "action": args.get("action"),
        "offset": args.get("offset", 0),
        "limit": args.get("limit", 200),
        "timeout": args.get("timeout"),
    })


def _write_progress(complete, error=None, invocations=0, expected_invocations=None):
    PROGRESS.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    document = {"schema_version": 1, "mode": MODE, "complete": complete, "invocations": invocations}
    if error is not None:
        document["error"] = error
    if expected_invocations is not None:
        document["expected_invocations"] = expected_invocations
    temporary = PROGRESS.with_suffix(".tmp")
    with open(temporary, "x", encoding="utf-8") as output:
        json.dump(document, output, separators=(",", ":"))
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, PROGRESS)
    if DIAGNOSTIC is not None:
        DIAGNOSTIC.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        diagnostic_temporary = DIAGNOSTIC.with_suffix(".tmp")
        with open(diagnostic_temporary, "x", encoding="utf-8") as output:
            json.dump(document, output, separators=(",", ":"))
            output.flush()
            os.fsync(output.fileno())
        os.replace(diagnostic_temporary, DIAGNOSTIC)


class Recorder:
    def __init__(self):
        try:
            program = Path(sys.executable).read_bytes()
        except OSError as error:
            raise RuntimeError("cannot read Hermes interpreter for native recording") from error
        SIDECAR.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.file = open(SIDECAR, "x", encoding="utf-8")
        self.entries = []
        self._event({"event": "launch", "session": "launch-1", "program_sha256": _digest(program)})
        self._event({"event": "session_alias", "alias": SESSION, "session": "launch-1"})
        self.complete = False
        self.failed = False

    def _event(self, event):
        line = json.dumps(event, ensure_ascii=False, separators=(",", ":"))
        if len(line.encode("utf-8")) > MAX_BYTES:
            raise RuntimeError("Hermes native tool event exceeds the private sidecar bound")
        self.file.write(line + "\n")
        self.file.flush()
        os.fsync(self.file.fileno())

    def invoke(self, original, *args, **kwargs):
        request = _request(*args, **kwargs)
        if len(request) > MAX_PAYLOAD_BYTES:
            self.failed = True
            raise RuntimeError("Hermes terminal request exceeds the native replay bound")
        self._event({"event": "poll", "alias": SESSION})
        self._event({
            "event": "write", "alias": SESSION,
            "request_sha256": _digest(request),
            "request_b64": base64.b64encode(request).decode("ascii"),
        })
        try:
            output = original(*args, **kwargs)
        except BaseException:
            self.failed = True
            raise
        if not isinstance(output, str):
            self.failed = True
            raise RuntimeError("Hermes terminal tool returned a non-text response")
        response = output.encode("utf-8")
        if len(response) > MAX_PAYLOAD_BYTES:
            self.failed = True
            raise RuntimeError("Hermes terminal response exceeds the native replay bound")
        self._event({
            "event": "output", "alias": SESSION,
            "response_sha256": _digest(response),
            "output_b64": base64.b64encode(response).decode("ascii"),
        })
        self.entries.extend((request, response))
        return output

    def invoke_process(self, original, args, **kwargs):
        request = _process_request(args)
        if len(request) > MAX_PAYLOAD_BYTES:
            self.failed = True
            raise RuntimeError("Hermes process request exceeds the native replay bound")
        self._event({"event": "poll", "alias": SESSION})
        self._event({"event": "write", "alias": SESSION,
            "request_sha256": _digest(request),
            "request_b64": base64.b64encode(request).decode("ascii")})
        try:
            output = original(args, **kwargs)
        except BaseException:
            self.failed = True
            raise
        if not isinstance(output, str):
            self.failed = True
            raise RuntimeError("Hermes process tool returned a non-text response")
        response = output.encode("utf-8")
        if len(response) > MAX_PAYLOAD_BYTES:
            self.failed = True
            raise RuntimeError("Hermes process response exceeds the native replay bound")
        self._event({"event": "output", "alias": SESSION,
            "response_sha256": _digest(response),
            "output_b64": base64.b64encode(response).decode("ascii")})
        self.entries.extend((request, response))
        return output

    def close(self):
        if self.complete:
            return
        self.complete = True
        try:
            if self.failed:
                _write_progress(False, "terminal tool did not settle", len(self.entries) // 2)
                return
            self._event({"event": "exit", "alias": SESSION, "success": True})
            effect = _digest(b"".join(self.entries))
            self._event({"event": "workspace_effect", "sha256": effect})
            _write_progress(True, invocations=len(self.entries) // 2)
        except BaseException:
            _write_progress(False, "native sidecar finalization failed", len(self.entries) // 2)
        finally:
            self.file.close()


class Replayer:
    def __init__(self):
        try:
            bytes_ = SIDECAR.read_bytes()
        except OSError as error:
            raise RuntimeError("Hermes native replay sidecar is unavailable") from error
        if not bytes_ or len(bytes_) > MAX_BYTES or not bytes_.endswith(b"\n"):
            raise RuntimeError("Hermes native replay sidecar is invalid")
        try:
            events = [json.loads(line) for line in bytes_.splitlines()]
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RuntimeError("Hermes native replay sidecar is invalid") from error
        if len(events) < 4:
            raise RuntimeError("Hermes native replay sidecar is incomplete")
        if events[0].get("event") != "launch" or events[1] != {
            "event": "session_alias", "alias": SESSION, "session": "launch-1"
        }:
            raise RuntimeError("Hermes native replay sidecar has an unsupported session")
        self.exchanges = []
        index = 2
        while index + 2 < len(events) and events[index] == {"event": "poll", "alias": SESSION}:
            write, output = events[index + 1], events[index + 2]
            if write.get("event") != "write" or write.get("alias") != SESSION:
                raise RuntimeError("Hermes native replay sidecar is invalid")
            if output.get("event") != "output" or output.get("alias") != SESSION:
                raise RuntimeError("Hermes native replay sidecar is invalid")
            try:
                request = base64.b64decode(write["request_b64"], validate=True)
                response = base64.b64decode(output["output_b64"], validate=True)
            except (KeyError, ValueError) as error:
                raise RuntimeError("Hermes native replay sidecar is invalid") from error
            if write.get("request_sha256") != _digest(request) or output.get("response_sha256") != _digest(response):
                raise RuntimeError("Hermes native replay sidecar digest is invalid")
            self.exchanges.append((request, response))
            index += 3
        if events[index:index + 2] != [
            {"event": "exit", "alias": SESSION, "success": True},
            {"event": "workspace_effect", "sha256": events[index + 1].get("sha256") if index + 1 < len(events) else None},
        ] or index + 2 != len(events):
            raise RuntimeError("Hermes native replay sidecar is incomplete")
        self.index = 0
        self.failed = False
        self.failure = None

    def invoke(self, _original, *args, **kwargs):
        request = _request(*args, **kwargs)
        if self.index >= len(self.exchanges) or self.exchanges[self.index][0] != request:
            self.failed = True
            expected = self.exchanges[self.index][0] if self.index < len(self.exchanges) else None
            component = "sequence" if expected is None else _request_difference(expected, request)
            self.failure = "native terminal request differs:" + component
            _write_progress(False, self.failure, self.index, len(self.exchanges))
            # A bounded field name is safe to retain in the private process
            # diagnostic; never write either tool request or response.
            print("ASB_HERMES_NATIVE_MISMATCH:" + component, file=sys.stderr, flush=True)
            # Hermes turns a raised tool exception into a runtime-rendered
            # error envelope. Return only the fixed field category instead so
            # the next provider request can diagnose the binding without
            # disclosing either side of the tool exchange.
            return json.dumps({"asb_native_mismatch": component}, separators=(",", ":"))
        response = self.exchanges[self.index][1]
        self.index += 1
        # Persist a bounded cursor after every successful replayed call.  A
        # provider-tape rejection can terminate Hermes before its exit hook,
        # so finalization alone cannot prove how far the adapter dispatch got.
        _write_progress(
            False,
            "native terminal replay in progress",
            self.index,
            len(self.exchanges),
        )
        try:
            return response.decode("utf-8")
        except UnicodeDecodeError as error:
            self.failed = True
            raise RuntimeError("Hermes terminal replay response is not text") from error

    def invoke_process(self, _original, args, **kwargs):
        request = _process_request(args)
        if len(request) > MAX_PAYLOAD_BYTES:
            self.failed = True
            raise RuntimeError("Hermes process request exceeds the native replay bound")
        if self.index >= len(self.exchanges) or self.exchanges[self.index][0] != request:
            self.failed = True
            self.failure = "native terminal request differs:sequence"
            _write_progress(False, self.failure, self.index, len(self.exchanges))
            return json.dumps({"asb_native_mismatch": "sequence"}, separators=(",", ":"))
        response = self.exchanges[self.index][1]
        self.index += 1
        _write_progress(False, "native terminal replay in progress", self.index, len(self.exchanges))
        try:
            return response.decode("utf-8")
        except UnicodeDecodeError as error:
            self.failed = True
            raise RuntimeError("Hermes process replay response is not text") from error

    def close(self):
        complete = not self.failed and self.index == len(self.exchanges)
        _write_progress(
            complete,
            None if complete else self.failure or "native terminal replay is incomplete",
            self.index,
            len(self.exchanges),
        )


def _request_difference(expected, actual):
    """Return a bounded field name only; never retain a tool request value."""
    try:
        expected = json.loads(expected)
        actual = json.loads(actual)
    except (TypeError, ValueError):
        return "encoding"
    for field in ("command", "background", "timeout", "force", "workdir", "pty", "notify_on_complete", "watch_patterns"):
        if expected.get(field) != actual.get(field):
            return field
    return "encoding"


try:
    # Hermes exposes file/code/process tools through its always-enabled core
    # list even when `--toolsets terminal` is selected. They have no stable
    # external interception API, so do not advertise them during a complete
    # ASB tool recording/replay session.
    import toolsets
    toolsets._HERMES_CORE_TOOLS[:] = ["terminal"]
    import tools.terminal_tool as terminal_tool
    original = terminal_tool.terminal_tool
    transport = Recorder() if MODE == "record" else Replayer() if MODE == "replay" else None
    if transport is None:
        raise RuntimeError("Hermes native tool mode is invalid")

    def terminal(command, background=False, timeout=None, task_id=None, session_id=None, force=False,
                 workdir=None, pty=False, notify_on_complete=False, watch_patterns=None):
        return transport.invoke(original, command, background, timeout, task_id, session_id, force, workdir,
                                pty, notify_on_complete, watch_patterns)

    terminal_tool.terminal_tool = terminal
    # The CLI can initialize the central registry before its tool module is
    # consulted. Rebind that captured terminal entry too, so model dispatch
    # reaches the same adapter-owned wrapper in record and replay.
    from tools.registry import registry

    def registered_terminal(args, **kw):
        return terminal(
            args.get("command"),
            args.get("background", False),
            args.get("timeout"),
            kw.get("task_id"),
            kw.get("session_id"),
            False,
            args.get("workdir"),
            args.get("pty", False),
            args.get("notify_on_complete", False),
            args.get("watch_patterns"),
        )

    entry = registry._tools.get("terminal")
    if entry is not None:
        entry.handler = registered_terminal

    process_entry = registry._tools.get("process")
    if process_entry is not None:
        original_process = process_entry.handler

        def registered_process(args, **kw):
            return transport.invoke_process(original_process, args, **kw)

        process_entry.handler = registered_process

    # Hermes normally persists a large terminal result into its ephemeral
    # sandbox and replaces it with a message containing that sandbox path.
    # The result is then placed in the next Responses request.  An ASB native
    # recording already retains the exact terminal response in the private
    # sidecar, whereas the sandbox path is process-local and consequently
    # differs between record and replay.  Keep only this adapter-owned
    # terminal result inline while native replay is active.  Other tools and
    # normal Hermes invocations retain Hermes' standard persistence policy.
    import agent.tool_executor as tool_executor
    original_persist_tool_result = tool_executor.maybe_persist_tool_result

    def persist_terminal_result(content, tool_name, tool_use_id, **kwargs):
        if tool_name == "terminal":
            return content
        return original_persist_tool_result(content, tool_name, tool_use_id, **kwargs)

    tool_executor.maybe_persist_tool_result = persist_terminal_result
    if MODE == "replay":
        # Establish a bounded zero cursor before Hermes starts.  This survives
        # a provider-tape rejection that can terminate the one-shot process
        # before a terminal call returns or an exit hook runs.
        _write_progress(
            False,
            "native terminal replay awaiting invocation",
            0,
            len(transport.exchanges),
        )
    atexit.register(transport.close)

    # Hermes terminates its one-shot CLI through ``os._exit``.  That bypasses
    # Python's atexit handlers, so finalize the adapter-owned terminal stream
    # at that boundary as well.  The delegate remains the real process exit;
    # this hook only makes already-flushed adapter evidence terminal.
    original_os_exit = os._exit

    def closing_os_exit(code):
        transport.close()
        original_os_exit(code)

    os._exit = closing_os_exit
except BaseException as error:
    # Python suppresses sitecustomize import failures and continues starting
    # Hermes.  Keep only a fixed sidecar category so ASB can fail the adapter
    # qualification instead of mistaking an unhooked native tool for replay.
    known = {
        "Hermes native replay sidecar is unavailable",
        "Hermes native replay sidecar is invalid",
        "Hermes native replay sidecar is incomplete",
        "Hermes native replay sidecar has an unsupported session",
        "Hermes native replay sidecar digest is invalid",
    }
    detail = str(error) if isinstance(error, RuntimeError) and str(error) in known else None
    _write_progress(False, detail or "native terminal transport initialization failed")
    raise
