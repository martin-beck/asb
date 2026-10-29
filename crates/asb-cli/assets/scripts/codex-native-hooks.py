"""Adapter-owned Codex local-tool recorder/replayer.

Codex invokes configured lifecycle hooks around every local tool call.  This
script records only the hook-provided structured request/result objects in the
private native sidecar. During replay the pre-tool hook replaces the native
call with this script's inert emitter, so no recorded command is executed.
"""
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path
import sys

MAX_BYTES = 4 * 1024 * 1024
MAX_PAYLOAD_BYTES = 512 * 1024
SESSION = "codex-tools-1"
MODE = os.environ.get("ASB_CODEX_NATIVE_MODE", "")
SIDECAR = Path(os.environ["ASB_CODEX_NATIVE_SIDECAR"])
PROGRESS = Path(os.environ["ASB_CODEX_NATIVE_PROGRESS"])
STATE = Path(os.environ["ASB_CODEX_NATIVE_STATE"])


def _encode(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True).encode("utf-8")


def _digest(value):
    return hashlib.sha256(value).hexdigest()


def _read_input():
    value = json.load(sys.stdin)
    if not isinstance(value, dict):
        raise RuntimeError("Codex hook input is invalid")
    return value


def _write_json(path, value):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    with open(temporary, "x", encoding="utf-8") as output:
        json.dump(value, output, separators=(",", ":"), sort_keys=True)
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, path)


def _progress(complete, error=None):
    value = {"schema_version": 1, "mode": MODE, "complete": complete}
    if error:
        value["error"] = error
    _write_json(PROGRESS, value)


def _event(file, value):
    line = _encode(value)
    if len(line) > MAX_BYTES:
        raise RuntimeError("Codex native sidecar event exceeds its bound")
    file.write(line + b"\n")
    file.flush()
    os.fsync(file.fileno())


def _load_state():
    if not STATE.is_file():
        return {"pending": {}, "next": 0, "replay": []}
    value = json.loads(STATE.read_text(encoding="utf-8"))
    if not isinstance(value, dict) or not isinstance(value.get("pending"), dict):
        raise RuntimeError("Codex native hook state is invalid")
    return value


def _save_state(value):
    _write_json(STATE, value)


def _with_lock(fn):
    SIDECAR.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    lock = SIDECAR.with_name(SIDECAR.name + ".lock")
    with open(lock, "a+b") as file:
        fcntl.flock(file.fileno(), fcntl.LOCK_EX)
        try:
            return fn()
        finally:
            fcntl.flock(file.fileno(), fcntl.LOCK_UN)


def _request(value):
    name = value.get("tool_name")
    tool_input = value.get("tool_input")
    if not isinstance(name, str) or not name:
        raise RuntimeError("Codex native hook did not provide a tool name")
    return _encode({"schema_version": 1, "kind": "codex-local-tool", "tool_name": name, "tool_use_id": _tool_id(value), "input": tool_input})


def _tool_id(value):
    identifier = value.get("tool_use_id")
    if not isinstance(identifier, str) or not identifier:
        raise RuntimeError("Codex native hook did not provide a tool identity")
    return identifier


def _initialize(file):
    if file.tell() == 0:
        program = Path(sys.executable).read_bytes()
        _event(file, {"event": "launch", "session": "launch-1", "program_sha256": _digest(program)})
        _event(file, {"event": "session_alias", "alias": SESSION, "session": "launch-1"})


def _record_pre(value):
    request = _request(value)
    if len(request) > MAX_PAYLOAD_BYTES:
        raise RuntimeError("Codex native tool request exceeds its bound")
    identifier = _tool_id(value)

    def operation():
        state = _load_state()
        if identifier in state["pending"]:
            raise RuntimeError("Codex native tool identity was reused before completion")
        with open(SIDECAR, "a+b") as file:
            _initialize(file)
            _event(file, {"event": "poll", "alias": SESSION})
            _event(file, {"event": "write", "alias": SESSION, "request_sha256": _digest(request), "request_b64": base64.b64encode(request).decode("ascii")})
            # Some Codex releases do not deliver PostToolUse for every
            # successful native invocation. The provider's following turn is
            # the adapter-authoritative result envelope, so reserve a sealed
            # response slot now and let the adapter bind it after recording.
            placeholder = _encode(None)
            _event(file, {"event": "output", "alias": SESSION, "response_sha256": _digest(placeholder), "output_b64": base64.b64encode(placeholder).decode("ascii")})
        state["pending"][identifier] = base64.b64encode(request).decode("ascii")
        _save_state(state)
        _progress(False)
    _with_lock(operation)


def _record_post(value):
    response = _encode(value.get("tool_response"))
    if len(response) > MAX_PAYLOAD_BYTES:
        raise RuntimeError("Codex native tool response exceeds its bound")
    identifier = _tool_id(value)

    def operation():
        state = _load_state()
        # PreToolUse already sealed the output slot. A later provider-facing
        # result is the value Codex replays, so this optional hook merely
        # acknowledges completion without creating a duplicate sidecar row.
        state["pending"].pop(identifier, None)
        _save_state(state)
    _with_lock(operation)


def _replay_exchanges():
    rows = [json.loads(line) for line in SIDECAR.read_bytes().splitlines()]
    if len(rows) < 4 or rows[0].get("event") != "launch" or rows[1] != {"event": "session_alias", "alias": SESSION, "session": "launch-1"}:
        raise RuntimeError("Codex native replay sidecar is invalid")
    exchanges = []
    index = 2
    while index + 2 < len(rows) and rows[index] == {"event": "poll", "alias": SESSION}:
        write, output = rows[index + 1], rows[index + 2]
        if write.get("event") != "write" or output.get("event") != "output" or write.get("alias") != SESSION or output.get("alias") != SESSION:
            raise RuntimeError("Codex native replay sidecar is invalid")
        request = base64.b64decode(write["request_b64"], validate=True)
        response = base64.b64decode(output["output_b64"], validate=True)
        if write.get("request_sha256") != _digest(request) or output.get("response_sha256") != _digest(response):
            raise RuntimeError("Codex native replay sidecar digest is invalid")
        exchanges.append((request, response))
        index += 3
    if index + 2 != len(rows) or rows[index] != {"event": "exit", "alias": SESSION, "success": True} or rows[index + 1].get("event") != "workspace_effect":
        raise RuntimeError("Codex native replay sidecar is incomplete")
    return exchanges


def _quote(argument):
    return "'" + argument.replace("'", "'\"'\"'") + "'"


def _replay_pre(value):
    request = _request(value)
    identifier = _tool_id(value)
    def operation():
        state = _load_state()
        if not state["replay"]:
            state["replay"] = [[base64.b64encode(a).decode("ascii"), base64.b64encode(b).decode("ascii")] for a, b in _replay_exchanges()]
        index = state["next"]
        if index >= len(state["replay"]) or base64.b64decode(state["replay"][index][0]) != request:
            raise RuntimeError("Codex native tool request differs from its recording")
        state["pending"][identifier] = index
        state["next"] = index + 1
        _save_state(state)
        command = "{} {} emit {}".format(_quote(sys.executable), _quote(str(Path(__file__).resolve())), index)
        print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow", "updatedInput": {"command": command}}}, separators=(",", ":")))
    _with_lock(operation)


def _response_text(value):
    if isinstance(value, str):
        return value, 0
    if not isinstance(value, dict):
        return json.dumps(value, separators=(",", ":")), 0
    text = value.get("stdout", value.get("output", value.get("aggregated_output", value.get("text", ""))))
    if not isinstance(text, str):
        text = json.dumps(text, separators=(",", ":"))
    code = value.get("exit_code", value.get("exitCode", 0))
    return text, code if isinstance(code, int) else 0


def _replay_post(value):
    identifier = _tool_id(value)

    def operation():
        state = _load_state()
        index = state["pending"].get(identifier)
        if not isinstance(index, int):
            raise RuntimeError("Codex native replay response has no matching request")
        try:
            encoded = state["replay"][index][1]
            response = json.loads(base64.b64decode(encoded).decode("utf-8"))
        except (IndexError, KeyError, ValueError, UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RuntimeError("Codex native replay response is unavailable") from error
        del state["pending"][identifier]
        _save_state(state)
        # Codex serializes a tool result after the native command returns.  A
        # replayed command alone cannot reproduce that wrapper exactly.  Its
        # documented post-tool hook accepts the original structured result,
        # so restore that adapter-owned value before Codex prepares the next
        # provider request.
        print(json.dumps({"hookSpecificOutput": {"hookEventName": "PostToolUse", "updatedMCPToolOutput": response}}, separators=(",", ":")))

    _with_lock(operation)


def _emit(index):
    state = _load_state()
    try:
        encoded = state["replay"][index][1]
        response = json.loads(base64.b64decode(encoded).decode("utf-8"))
    except (IndexError, KeyError, ValueError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError("Codex native replay response is unavailable") from error
    text, code = _response_text(response)
    sys.stdout.write(text)
    raise SystemExit(code)


def _finish():
    def operation():
        state = _load_state()
        if MODE == "replay" and state.get("next", 0) != len(state.get("replay", [])):
            _progress(False, "native Codex replay did not consume its recording")
            return
        if MODE == "record":
            with open(SIDECAR, "a+b") as file:
                _event(file, {"event": "exit", "alias": SESSION, "success": True})
                file.flush()
                file.seek(0)
                _event(file, {"event": "workspace_effect", "sha256": _digest(file.read())})
        _progress(True)
    _with_lock(operation)


def main():
    if len(sys.argv) < 2:
        raise RuntimeError("Codex native hook mode is required")
    command = sys.argv[1]
    if command == "pre":
        value = _read_input()
        if MODE == "record":
            _record_pre(value)
        elif MODE == "replay":
            _replay_pre(value)
        else:
            raise RuntimeError("Codex native hook mode is invalid")
    elif command == "post":
        if MODE == "record":
            _record_post(_read_input())
        elif MODE == "replay":
            _replay_post(_read_input())
        else:
            raise RuntimeError("Codex native hook mode is invalid")
    elif command == "finish":
        _finish()
    elif command == "emit" and len(sys.argv) == 3:
        _emit(int(sys.argv[2]))
    else:
        raise RuntimeError("Codex native hook command is invalid")


if __name__ == "__main__":
    try:
        main()
    except BaseException as error:
        try:
            _progress(False, "Codex native hook failed")
        except BaseException:
            pass
        print(str(error), file=sys.stderr)
        raise
