"""Offline PTY exercise of the actual debugger normalizer workflow."""
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

binary, root, mode = sys.argv[1:]
args = [binary, "--project", root, "--workload", "swe-bench"]
if mode == "mismatch":
    args.append(os.path.join(root, "case.mismatch.json"))
else:
    args += ["--cassette", os.path.join(root, "recording.jsonl")]
pid, master = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"
    os.environ["VISUAL"] = os.path.join(root, "editor")
    os.execv(binary, args)
fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
transcript = bytearray()
reaped = False
def wait_for(text):
    target = text.encode()
    end = time.monotonic() + 15
    start = len(transcript)
    while time.monotonic() < end:
        readable, _, _ = select.select([master], [], [], 0.2)
        if readable:
            try:
                data = os.read(master, 65536)
            except OSError:
                break
            if not data:
                break
            transcript.extend(data)
            if target in transcript[start:]:
                return
    raise RuntimeError(f"did not see {text!r}; tail={bytes(transcript[-500:])!r}")
def wait_until(predicate):
    end = time.monotonic() + 15
    while time.monotonic() < end:
        if predicate():
            return
        readable, _, _ = select.select([master], [], [], 0.1)
        if readable:
            try:
                transcript.extend(os.read(master, 65536))
            except OSError:
                break
    raise RuntimeError("expected file state did not occur")

try:
    wait_for("ASB Replay Debugger" if mode == "mismatch" else "Tracks")
    os.write(master, b"m")
    wait_for("Profiles")
    os.write(master, b"n")
    module = os.path.join(root, "normalizers/debugger-suggestion-1.mjs")
    config = os.path.join(root, ".asb/project.toml")
    wait_until(lambda: os.path.exists(module) and "debugger-suggestion-1" in open(config).read())
    time.sleep(0.2)
    os.write(master, b"e")
    wait_until(lambda: "edited by test" in open(module).read())
    time.sleep(0.3)
    os.write(master, b" ")
    wait_until(lambda: "enabled = true" in open(config).read())
    os.write(master, b"q")
    time.sleep(0.3)
    os.write(master, b"q")
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        finished, status = os.waitpid(pid, os.WNOHANG)
        if finished:
            reaped = True
            if not os.WIFEXITED(status) or os.WEXITSTATUS(status) != 0:
                raise RuntimeError(f"debugger exited with status {status}")
            break
        readable, _, _ = select.select([master], [], [], 0.05)
        if readable:
            try:
                transcript.extend(os.read(master, 65536))
            except OSError:
                pass
    else:
        raise RuntimeError("debugger did not exit")
finally:
    if not reaped:
        try:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        except ProcessLookupError:
            pass
    os.close(master)
