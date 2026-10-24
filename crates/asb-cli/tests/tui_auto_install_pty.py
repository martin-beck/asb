"""Exercise the real interactive terminal with local, credential-free fixtures."""
import fcntl
import codecs
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time

exe, root, mode = sys.argv[1:]
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 180, 0, 0))
args = [exe, "workload", "tui"] if mode == "workload" else [exe, "setup", "tui"]
process = subprocess.Popen(args, cwd=root, stdin=slave, stdout=slave, stderr=slave, env=os.environ)
os.close(slave)
output = bytearray()
screen = [[" "] * 180 for _ in range(32)]
row = col = 0
pending = ""
decoder = codecs.getincrementaldecoder("utf-8")("replace")

def render(data):
    global row, col, pending
    pending += decoder.decode(data)
    while pending:
        if pending.startswith("\x1b["):
            match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", pending)
            if not match:
                return
            params, _, command = match.groups()
            if command in ("H", "f"):
                parts = params.split(";")
                row = max(0, int(parts[0] or 1) - 1)
                col = max(0, int(parts[1] or 1) - 1) if len(parts) > 1 else 0
            pending = pending[match.end():]
        elif pending[0] == "\x1b" and len(pending) == 1:
            return
        elif pending[0] == "\x1b":
            pending = pending[2:]
        else:
            char, pending = pending[0], pending[1:]
            if char >= " " and row < 32 and col < 180:
                screen[row][col] = char
                col += 1

def visible():
    return "\n".join("".join(line) for line in screen)

def wait_for(needle):
    deadline = time.monotonic() + 20
    while needle.decode() not in visible():
        if time.monotonic() >= deadline or process.poll() is not None:
            raise AssertionError(f"missing {needle!r}; exit={process.poll()}; screen={visible()!r}")
        if select.select([master], [], [], 0.1)[0]:
            data = os.read(master, 65536)
            output.extend(data)
            render(data)

try:
    wait_for(b"agentbench" if mode == "workload" else b"codex")
    # Setup reserves the first row for the non-domain `[all]` operation;
    # installation coverage must select the first concrete agent instead.
    if mode != "workload":
        os.write(master, b"\x1b[B")
    os.write(master, b" ")
    if mode in ("failure", "no-executable", "not-runnable"):
        # The installer validates the expected executable before returning, so
        # a missing/non-runnable executable is a concrete install failure.
        wait_for(b"Cannot install")
        os.write(master, b"\r")
        wait_for(b"Select at least one item before continuing")
    else:
        wait_for(b"Installed or refreshed")
        os.write(master, b"\r")
        wait_for(b"Choose enabled trials" if mode == "workload" else b"Choose the provider endpoint")
        if mode == "collector":
            os.write(master, b"local")
            wait_for(b"search: local")
            os.write(master, b" \r")
            wait_for(b"Choose tool-capable models")
            os.write(master, b" \r")
            wait_for(b"Choose one or more workload prompts")
            # The setup workload pane has the same leading `[all]` control.
            os.write(master, b"\x1b[B \r")
            wait_for(b"Choose the measurement collectors")
            os.write(master, b"flamegraph")
            wait_for(b"search: flamegraph")
            os.write(master, b" ")
            wait_for(b"Installed or refreshed `flamegraph`")
            os.write(master, b"\r")
            wait_for(b"Choose how many identical executions")
            os.write(master, b"\r")
            if process.wait(timeout=10) != 0:
                raise AssertionError(f"setup did not complete: {process.returncode}")
finally:
    if process.poll() is None:
        process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)
    os.close(master)
