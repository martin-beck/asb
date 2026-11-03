"""ASB-owned Hermes terminal-tool recorder/replayer.

This module is imported by Python's standard ``sitecustomize`` hook before
Hermes imports its tool registry.  It replaces the terminal tool entry point,
not subprocess APIs: every recorded/replayed operation is an explicit Hermes
terminal request and response.  The sidecar is private to an ASB attempt.
"""
import atexit
import base64
import contextvars
import functools
import hashlib
import json
import os
from pathlib import Path
import socket
import sys
import threading
import time


MODE = os.environ.get("ASB_HERMES_NATIVE_MODE", "")
SIDECAR = Path(os.environ["ASB_HERMES_NATIVE_SIDECAR"])
PROGRESS = Path(os.environ["ASB_HERMES_NATIVE_PROGRESS"])
DIAGNOSTIC = Path(os.environ["ASB_HERMES_NATIVE_DIAGNOSTIC"]) if os.environ.get("ASB_HERMES_NATIVE_DIAGNOSTIC") else None
MAX_BYTES = 4 * 1024 * 1024
MAX_PAYLOAD_BYTES = 512 * 1024
SESSION = "terminal-1"
PARENT_CALL = contextvars.ContextVar("asb_hermes_parent_call", default=None)


def _filesystem_marker(action, alias):
    """Associate a native call with the independent filesystem event stream."""
    path = os.environ.get("ASB_FS_MARKER_SOCKET")
    if not path:
        return True
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(5)
            connection.connect(path)
            connection.sendall(f"tool-{action}:{alias}:{os.getpid()}:{threading.get_native_id()}\n".encode("ascii"))
            with connection.makefile("rb") as response:
                return response.readline(32).strip().isdigit()
    except (OSError, UnicodeError):
        return False


def _filesystem_replay_boundary(alias):
    """Wait for the replica coordinator to apply this recorded tool's effects."""
    if not os.environ.get("ASB_FS_REPLAY_SOCKET"):
        return True
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(1800)
            connection.connect(os.environ["ASB_FS_REPLAY_SOCKET"])
            connection.sendall(f"tool-end:{alias}\n".encode("ascii"))
            with connection.makefile("rb") as response:
                return response.readline(32).strip().isdigit()
    except (OSError, UnicodeError):
        return False


def _filesystem_replay_begin(alias):
    if not os.environ.get("ASB_FS_REPLAY_SOCKET"):
        return True
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(1800)
            connection.connect(os.environ["ASB_FS_REPLAY_SOCKET"])
            connection.sendall(f"tool-begin:{alias}\n".encode("ascii"))
            with connection.makefile("rb") as response:
                return response.readline(32).strip().isdigit()
    except (OSError, UnicodeError):
        return False


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


def _call_identity():
    # Hermes sets this context while dispatching a provider tool call. Nested
    # and non-provider calls may legitimately have no such identity.
    try:
        from tools.approval import _approval_tool_call_id
        return _approval_tool_call_id.get()
    except (ImportError, AttributeError):
        return ""


def _request(command, background, timeout, _task_id, _session_id, force, workdir, pty, notify_on_complete, watch_patterns):
    value = {
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
    }
    identity = _call_identity()
    if identity:
        value["tool_call_id"] = identity
    parent = PARENT_CALL.get()
    if parent:
        value["parent_alias"] = parent
    return _encoded(value)


def _process_request(args):
    """Identity for a registry status action, excluding its ephemeral ID."""
    value = {
        "kind": "process",
        "schema_version": 1,
        "action": args.get("action"),
        "offset": args.get("offset", 0),
        "limit": args.get("limit", 200),
        "timeout": args.get("timeout"),
    }
    identity = _call_identity()
    if identity:
        value["tool_call_id"] = identity
    parent = PARENT_CALL.get()
    if parent:
        value["parent_alias"] = parent
    return _encoded(value)


def _file_request(name, args):
    value = {"kind": "file-tool", "schema_version": 1, "name": name, "arguments": args}
    identity = _call_identity()
    if identity:
        value["tool_call_id"] = identity
    parent = PARENT_CALL.get()
    if parent:
        value["parent_alias"] = parent
    return _encoded(value)


def _matches_request(expected, actual):
    if expected == actual:
        return True
    try:
        recorded = json.loads(expected)
        current = json.loads(actual)
    except (ValueError, TypeError):
        return False
    if "tool_call_id" not in recorded:
        current.pop("tool_call_id", None)
        return recorded == current
    return False


def _differs_only_in_tool_call_id(expected, actual):
    try:
        recorded = json.loads(expected)
        current = json.loads(actual)
        if not isinstance(recorded, dict) or not isinstance(current, dict):
            return False
        recorded_id = recorded.pop("tool_call_id", None)
        current_id = current.pop("tool_call_id", None)
        return recorded == current and recorded_id != current_id
    except (ValueError, TypeError):
        return False


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
        self.lock = threading.RLock()
        self.started = 0
        self.flushed = 0
        self._event({"event": "launch", "session": "launch-1", "program_sha256": _digest(program)})
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
        return self._invoke(original, request, *args, **kwargs)

    def _invoke(self, original, request, *args, **kwargs):
        if len(request) > MAX_PAYLOAD_BYTES:
            self.failed = True
            raise RuntimeError("Hermes terminal request exceeds the native replay bound")
        with self.lock:
            if self.complete:
                raise RuntimeError("Hermes native sidecar is already closed")
            self.started += 1
            alias = f"terminal-{self.started}"
            self._event({"event": "session_alias", "alias": alias, "session": "launch-1"})
            self._event({"event": "poll", "alias": alias})
            self._event({"event": "write", "alias": alias,
                "request_sha256": _digest(request), "request_b64": base64.b64encode(request).decode("ascii")})
            if not _filesystem_marker("begin", alias):
                self.failed = True
        token = PARENT_CALL.set(alias)
        try:
            output = original(*args, **kwargs)
        except BaseException:
            self.failed = True
            raise
        finally:
            PARENT_CALL.reset(token)
            if not _filesystem_marker("end", alias):
                self.failed = True
        if not isinstance(output, str):
            self.failed = True
            raise RuntimeError("Hermes terminal tool returned a non-text response")
        response = output.encode("utf-8")
        if len(response) > MAX_PAYLOAD_BYTES:
            self.failed = True
            raise RuntimeError("Hermes terminal response exceeds the native replay bound")
        with self.lock:
            if self.complete:
                raise RuntimeError("Hermes native sidecar closed before tool completed")
            self._event({"event": "output", "alias": alias,
                "response_sha256": _digest(response), "output_b64": base64.b64encode(response).decode("ascii")})
            self._event({"event": "exit", "alias": alias, "success": True})
            self.entries.extend((request, response))
            self.flushed += 1
        return output

    def invoke_process(self, original, args, **kwargs):
        request = _process_request(args)
        return self._invoke(original, request, args, **kwargs)

    def invoke_file(self, original, name, args, **kwargs):
        try:
            request = _file_request(name, args)
        except (TypeError, ValueError, RecursionError):
            self.failed = True
            return self.untaped_capture.invoke(original, name, args, kwargs)
        if len(request) > MAX_PAYLOAD_BYTES:
            self.failed = True
            return self.untaped_capture.invoke(original, name, args, kwargs)
        saved = []
        def execute():
            result = original(name, args, **kwargs)
            saved.append(result)
            return json.dumps({"result": result}, ensure_ascii=False, separators=(",", ":"))
        try:
            encoded = self._invoke(execute, request)
        except (RuntimeError, TypeError, ValueError):
            self.failed = True
            if saved:
                return self.untaped_capture.invoke(lambda *_args, **_kwargs: saved[0], name, args, kwargs)
            raise
        return json.loads(encoded)["result"]

    def close(self):
        with self.lock:
            self._close_locked()

    def _close_locked(self):
        if self.complete:
            return
        self.complete = True
        try:
            if hasattr(self, "untaped_capture"):
                self.untaped_capture.close()
            if self.failed or self.flushed != self.started:
                _write_progress(False, "terminal tool did not settle", len(self.entries) // 2)
                return
            if not self.started:
                _write_progress(False, "native terminal recording has no invocations", 0)
                return
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
        aliases = [event for event in events if event.get("event") == "session_alias"]
        self.dependencies = None
        if len(aliases) > 1:
            sessions = {}
            completed = set()
            exchanges = []
            aliases_by_index = []
            dependencies = []
            completion_order = []
            for event in events[1:-1]:
                kind, alias = event.get("event"), event.get("alias")
                if kind == "session_alias":
                    if alias in sessions or event.get("session") != "launch-1":
                        raise RuntimeError("Hermes native replay sidecar is invalid")
                    sessions[alias] = {"phase": "idle", "dependencies": set(completed)}
                elif alias not in sessions:
                    raise RuntimeError("Hermes native replay sidecar is invalid")
                elif kind == "poll" and sessions[alias]["phase"] == "idle":
                    sessions[alias]["phase"] = "poll"
                elif kind == "write" and sessions[alias]["phase"] == "poll":
                    try:
                        request = base64.b64decode(event["request_b64"], validate=True)
                    except (KeyError, ValueError) as error:
                        raise RuntimeError("Hermes native replay sidecar is invalid") from error
                    if event.get("request_sha256") != _digest(request):
                        raise RuntimeError("Hermes native replay sidecar digest is invalid")
                    sessions[alias].update(phase="write", request=request, index=len(exchanges))
                    exchanges.append(None)
                    aliases_by_index.append(alias)
                    dependencies.append(sessions[alias]["dependencies"])
                elif kind == "output" and sessions[alias]["phase"] == "write":
                    try:
                        response = base64.b64decode(event["output_b64"], validate=True)
                    except (KeyError, ValueError) as error:
                        raise RuntimeError("Hermes native replay sidecar is invalid") from error
                    if event.get("response_sha256") != _digest(response):
                        raise RuntimeError("Hermes native replay sidecar digest is invalid")
                    entry = sessions[alias]
                    exchanges[entry["index"]] = (entry["request"], response)
                    completed.add(entry["index"])
                    completion_order.append(entry["index"])
                    entry["phase"] = "output"
                elif kind == "exit" and sessions[alias]["phase"] == "output" and event.get("success") is True:
                    sessions[alias]["phase"] = "exit"
                else:
                    raise RuntimeError("Hermes native replay sidecar is invalid")
            if (events[-1].get("event") != "workspace_effect" or
                    any(entry["phase"] != "exit" for entry in sessions.values())):
                raise RuntimeError("Hermes native replay sidecar is incomplete")
            self.exchanges = exchanges
            self.aliases = aliases_by_index
            self.dependencies = dependencies
            self.consumed = set()
            self.delivered = set()
            self.completion_order = completion_order
            self.completion_cursor = 0
            self.lock = threading.RLock()
            self.condition = threading.Condition(self.lock)
            self.closing = False
        else:
            self.exchanges = []
            self.aliases = []
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
                self.aliases.append(SESSION)
                index += 3
            if events[index:index + 2] != [
                {"event": "exit", "alias": SESSION, "success": True},
                {"event": "workspace_effect", "sha256": events[index + 1].get("sha256") if index + 1 < len(events) else None},
            ] or index + 2 != len(events):
                raise RuntimeError("Hermes native replay sidecar is incomplete")
        self.children = {index: [] for index in range(len(self.exchanges))}
        if self.dependencies is not None:
            alias_indices = {alias: index for index, alias in enumerate(self.aliases)}
            for index, (request, _) in enumerate(self.exchanges):
                try:
                    parent_alias = json.loads(request).get("parent_alias")
                except (ValueError, TypeError, AttributeError) as error:
                    raise RuntimeError("Hermes native replay sidecar is invalid") from error
                if parent_alias is not None:
                    parent = alias_indices.get(parent_alias)
                    if parent is None or parent >= index:
                        raise RuntimeError("Hermes native replay sidecar has invalid nested lineage")
                    self.children[parent].append(index)
        self.recorded_to_runtime = {}
        self.runtime_to_recorded = {}
        self.index = 0
        self.failed = False
        self.failure = None

    def _select_candidate(self, request, eligible):
        """Bind regenerated IDs only when all other fields select one call."""
        try:
            actual = json.loads(request)
            actual_id = actual.get("tool_call_id") if isinstance(actual, dict) else None
        except (ValueError, TypeError):
            return None
        exact = []
        remapped = []
        for index in eligible:
            expected = self.exchanges[index][0]
            try:
                decoded = json.loads(expected)
                recorded_id = decoded.get("tool_call_id") if isinstance(decoded, dict) else None
            except (ValueError, TypeError):
                continue
            if recorded_id is not None and actual_id is not None:
                if not isinstance(recorded_id, str) or not isinstance(actual_id, str):
                    continue
                if self.recorded_to_runtime.get(recorded_id, actual_id) != actual_id:
                    continue
                if self.runtime_to_recorded.get(actual_id, recorded_id) != recorded_id:
                    continue
            if _matches_request(expected, request):
                exact.append((index, recorded_id, actual_id))
            elif (recorded_id is not None and actual_id is not None
                  and _differs_only_in_tool_call_id(expected, request)):
                remapped.append((index, recorded_id, actual_id))
        if exact:
            selected = exact[0]
        elif len(remapped) == 1:
            selected = remapped[0]
        else:
            return None
        index, recorded_id, actual_id = selected
        if recorded_id is not None and actual_id is not None:
            self.recorded_to_runtime[recorded_id] = actual_id
            self.runtime_to_recorded[actual_id] = recorded_id
        return index

    def invoke(self, _original, *args, **kwargs):
        request = _request(*args, **kwargs)
        if self.dependencies is not None:
            return self._invoke_multi(request)
        if self.index >= len(self.exchanges) or self._select_candidate(request, [self.index]) is None:
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
        if not _filesystem_replay_boundary(self.aliases[self.index]):
            self.failed = True
            self.failure = "native filesystem replay boundary unavailable"
            _write_progress(False, self.failure, self.index, len(self.exchanges))
            raise RuntimeError("Hermes filesystem replay boundary is unavailable")
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
        if self.dependencies is not None:
            return self._invoke_multi(request)
        if len(request) > MAX_PAYLOAD_BYTES:
            self.failed = True
            raise RuntimeError("Hermes process request exceeds the native replay bound")
        if self.index >= len(self.exchanges) or self._select_candidate(request, [self.index]) is None:
            self.failed = True
            self.failure = "native terminal request differs:sequence"
            _write_progress(False, self.failure, self.index, len(self.exchanges))
            return json.dumps({"asb_native_mismatch": "sequence"}, separators=(",", ":"))
        response = self.exchanges[self.index][1]
        if not _filesystem_replay_boundary(self.aliases[self.index]):
            self.failed = True
            self.failure = "native filesystem replay boundary unavailable"
            _write_progress(False, self.failure, self.index, len(self.exchanges))
            raise RuntimeError("Hermes filesystem replay boundary is unavailable")
        self.index += 1
        _write_progress(False, "native terminal replay in progress", self.index, len(self.exchanges))
        try:
            return response.decode("utf-8")
        except UnicodeDecodeError as error:
            self.failed = True
            raise RuntimeError("Hermes process replay response is not text") from error

    def invoke_file(self, _original, name, args, **kwargs):
        request = _file_request(name, args)
        if self.dependencies is not None:
            if name == "execute_code":
                encoded = self._invoke_nested_parent(request)
            else:
                encoded = self._invoke_multi(request)
        else:
            encoded = self._invoke_file_serial(request)
        result = json.loads(encoded)
        if not isinstance(result, dict) or set(result) != {"result"}:
            self.failed = True
            raise RuntimeError("Hermes file tool replay result is invalid")
        return result["result"]

    def _invoke_file_serial(self, request):
        if self.index >= len(self.exchanges) or self._select_candidate(request, [self.index]) is None:
            self.failed = True
            self.failure = "native terminal request differs:sequence"
            _write_progress(False, self.failure, self.index, len(self.exchanges))
            raise RuntimeError("Hermes file tool replay differs from recording")
        response = self.exchanges[self.index][1]
        if not _filesystem_replay_boundary(self.aliases[self.index]):
            self.failed = True
            self.failure = "native filesystem replay boundary unavailable"
            _write_progress(False, self.failure, self.index, len(self.exchanges))
            raise RuntimeError("Hermes filesystem replay boundary is unavailable")
        self.index += 1
        _write_progress(False, "native terminal replay in progress", self.index, len(self.exchanges))
        return response.decode("utf-8")

    def _invoke_nested_parent(self, request):
        with self.condition:
            eligible = [index for index in range(len(self.exchanges))
                        if index not in self.consumed and self.dependencies[index] <= self.delivered]
            index = self._select_candidate(request, eligible)
            if index is None:
                self.failed = True
                self.failure = "native terminal request differs:sequence"
                self.condition.notify_all()
                raise RuntimeError("Hermes nested tool differs from recording")
            self.consumed.add(index)
        return self._replay_nested_node(index, preconsumed=True)

    def _replay_nested_node(self, index, preconsumed=False):
        children = self.children[index]
        if children and not _filesystem_replay_begin(self.aliases[index]):
            with self.condition:
                self.failed = True
                self.condition.notify_all()
            raise RuntimeError("Hermes nested filesystem replay start is unavailable")
        workers = []
        errors = []
        def replay_child(child):
            try:
                self._replay_nested_node(child)
            except BaseException as error:
                with self.condition:
                    errors.append(error)
                    self.failed = True
                    self.condition.notify_all()
        for child in children:
            worker = threading.Thread(target=replay_child, args=(child,), daemon=True)
            workers.append(worker)
            worker.start()
        try:
            result = self._invoke_multi(self.exchanges[index][0], target=index,
                                        preconsumed=preconsumed)
        except BaseException:
            with self.condition:
                self.failed = True
                self.condition.notify_all()
            raise
        finally:
            for worker in workers:
                worker.join(timeout=5)
        if errors or any(worker.is_alive() for worker in workers):
            with self.condition:
                self.failed = True
                self.condition.notify_all()
            raise RuntimeError("Hermes nested native replay did not settle")
        return result

    def _invoke_multi(self, request, target=None, preconsumed=False):
        with self.condition:
            if self.failed or self.closing:
                raise RuntimeError("Hermes concurrent native replay did not settle")
            if target is not None:
                deadline = time.monotonic() + 1800
                while not self.dependencies[target] <= self.delivered:
                    if self.failed or self.closing or time.monotonic() >= deadline:
                        raise RuntimeError("Hermes nested native replay dependencies did not settle")
                    self.condition.wait(timeout=min(1, max(0, deadline - time.monotonic())))
                if not _matches_request(self.exchanges[target][0], request):
                    raise RuntimeError("Hermes nested native request differs")
                if not preconsumed:
                    if target in self.consumed:
                        raise RuntimeError("Hermes nested native call was already consumed")
                    self.consumed.add(target)
                index = target
            else:
                eligible = [index for index in range(len(self.exchanges))
                            if index not in self.consumed and self.dependencies[index] <= self.delivered]
                index = self._select_candidate(request, eligible)
                if index is None:
                    self.failed = True
                    id_only = any(index not in self.consumed and self.dependencies[index] <= self.delivered
                                  and _differs_only_in_tool_call_id(expected, request)
                                  for index, (expected, _) in enumerate(self.exchanges))
                    component = "tool_call_id" if id_only else "sequence"
                    self.failure = "native terminal request differs:" + component
                    _write_progress(False, self.failure, self.index, len(self.exchanges))
                    self.condition.notify_all()
                    return json.dumps({"asb_native_mismatch": component}, separators=(",", ":"))
                self.consumed.add(index)
            while self.completion_order[self.completion_cursor] != index:
                if self.failed or self.closing:
                    raise RuntimeError("Hermes concurrent native replay did not settle")
                self.condition.wait()
            # Do not hold the transport lock while the coordinator waits:
            # other concurrent calls may still need to enter the replay queue.
            self.condition.release()
            try:
                applied = _filesystem_replay_boundary(self.aliases[index])
            finally:
                self.condition.acquire()
            if not applied or self.failed or self.closing:
                self.failed = True
                self.failure = "native filesystem replay boundary unavailable"
                _write_progress(False, self.failure, self.index, len(self.exchanges))
                self.condition.notify_all()
                raise RuntimeError("Hermes filesystem replay boundary is unavailable")
            self.delivered.add(index)
            self.completion_cursor += 1
            self.index += 1
            _write_progress(False, "native terminal replay in progress", self.index, len(self.exchanges))
            self.condition.notify_all()
            try:
                return self.exchanges[index][1].decode("utf-8")
            except UnicodeDecodeError as error:
                self.failed = True
                raise RuntimeError("Hermes terminal replay response is not text") from error

    def close(self):
        if self.dependencies is not None:
            with self.condition:
                self.closing = True
                self.condition.notify_all()
                self._close_replay()
            return
        self._close_replay()

    def _close_replay(self):
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
    for field in ("tool_call_id", "command", "background", "timeout", "force", "workdir", "pty", "notify_on_complete", "watch_patterns"):
        if expected.get(field) != actual.get(field):
            return field
    return "encoding"


class UntapedToolCapture:
    """Private evidence for unsupported tools; never changes tool outcomes."""
    LIMIT = 8 * 1024 * 1024

    def __init__(self, path):
        self.path = path
        self.lock = threading.Lock()
        self.start = time.monotonic_ns()
        self.sequence = 0
        self.completed = 0
        self.bytes_written = 0
        self.incomplete = False

    @staticmethod
    def _fallback(value):
        if isinstance(value, (set, frozenset)):
            return sorted(value, key=lambda item: repr(item))
        if isinstance(value, Path):
            return str(value)
        raise TypeError("unsupported value in untaped tool evidence")

    def _write(self, event):
        try:
            payload = (json.dumps(event, ensure_ascii=False, separators=(",", ":"),
                                  default=self._fallback) + "\n").encode("utf-8")
            if self.bytes_written + len(payload) > self.LIMIT:
                self.incomplete = True
                return
            self.path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            flags = os.O_WRONLY | os.O_CREAT | os.O_APPEND
            if hasattr(os, "O_NOFOLLOW"):
                flags |= os.O_NOFOLLOW
            fd = os.open(self.path, flags, 0o600)
            with os.fdopen(fd, "ab") as output:
                output.write(payload)
                output.flush()
                os.fsync(output.fileno())
            self.bytes_written += len(payload)
        except (OSError, ValueError, TypeError, RecursionError):
            self.incomplete = True

    def close(self):
        with self.lock:
            if not self.sequence:
                return
            status = {"schema_version": 1, "calls_started": self.sequence,
                      "calls_completed": self.completed, "bytes_written": self.bytes_written,
                      "complete": not self.incomplete and self.completed == self.sequence}
            try:
                path = self.path.with_name("untaped-tools-status.json")
                flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
                if hasattr(os, "O_NOFOLLOW"):
                    flags |= os.O_NOFOLLOW
                fd = os.open(path, flags, 0o600)
                with os.fdopen(fd, "w", encoding="utf-8") as output:
                    json.dump(status, output, separators=(",", ":"))
                    output.flush()
                    os.fsync(output.fileno())
            except OSError:
                self.incomplete = True

    def invoke(self, original, name, args, kwargs):
        with self.lock:
            self.sequence += 1
            sequence = self.sequence
            alias = f"untaped-{sequence}"
            self._write({"schema_version": 1, "event": "begin", "sequence": sequence,
                         "alias": alias, "parent_alias": PARENT_CALL.get(),
                         "relative_ns": time.monotonic_ns() - self.start,
                         "tool": name, "arguments": args, "context": kwargs,
                         "tool_call_id": _call_identity()})
            if not _filesystem_marker("begin", alias):
                self.incomplete = True
        token = PARENT_CALL.set(alias)
        try:
            result = original(name, args, **kwargs)
        except BaseException as error:
            with self.lock:
                self._write({"schema_version": 1, "event": "error", "sequence": sequence,
                             "relative_ns": time.monotonic_ns() - self.start,
                             "error_type": type(error).__name__, "error": str(error)})
                self.completed += 1
            raise
        finally:
            PARENT_CALL.reset(token)
            if not _filesystem_marker("end", alias):
                self.incomplete = True
        with self.lock:
            self._write({"schema_version": 1, "event": "end", "sequence": sequence,
                         "relative_ns": time.monotonic_ns() - self.start, "result": result})
            self.completed += 1
        return result


def install_dispatch_coverage(registry, transport, mode, capture=None):
    """Preserve the catalogue without claiming unsupported calls were taped."""
    original_dispatch = registry.dispatch
    if capture is None and mode == "record":
        capture = UntapedToolCapture(SIDECAR.with_name("untaped-tools.jsonl"))
    if mode == "record":
        transport.untaped_capture = capture
    covered_dispatch = set()

    def dispatch_with_coverage(name, args, **kwargs):
        if name in ("read_file", "write_file", "patch", "search_files", "execute_code"):
            return transport.invoke_file(original_dispatch, name, args, **kwargs)
        if name not in covered_dispatch:
            if mode == "record":
                with transport.lock:
                    transport.failed = True
                return capture.invoke(original_dispatch, name, args, kwargs)
            else:
                transport.failed = True
                transport.failure = "native tool is not recorded"
                raise RuntimeError("Hermes tool has no recorded replay transport")
        return original_dispatch(name, args, **kwargs)

    registry.dispatch = dispatch_with_coverage
    return covered_dispatch


class SynchronousHttpDrain:
    """Let already-started HTTPX requests settle before Hermes calls os._exit."""
    def __init__(self):
        self.condition = threading.Condition()
        self.active = 0

    def install(self, client_class):
        original = client_class.send
        @functools.wraps(original)
        def send(client, *args, **kwargs):
            with self.condition:
                self.active += 1
            try:
                return original(client, *args, **kwargs)
            finally:
                with self.condition:
                    self.active -= 1
                    self.condition.notify_all()
        client_class.send = send

    def wait(self, timeout):
        deadline = time.monotonic() + timeout
        with self.condition:
            while self.active:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return False
                self.condition.wait(remaining)
            return True


try:
    # Hermes exposes file and code tools through its normal core catalogue.
    # Preserve that catalogue; unsupported calls run normally when recording
    # but make the evidence incomplete rather than claiming full coverage.
    import httpx
    http_drain = SynchronousHttpDrain()
    http_drain.install(httpx.Client)
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
    covered_dispatch = install_dispatch_coverage(registry, transport, MODE)

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
        covered_dispatch.add("terminal")

    process_entry = registry._tools.get("process")
    if process_entry is not None:
        original_process = process_entry.handler

        def registered_process(args, **kw):
            return transport.invoke_process(original_process, args, **kw)

        process_entry.handler = registered_process
        covered_dispatch.add("process")

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
        if not http_drain.wait(5):
            transport.failed = True
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
