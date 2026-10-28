"""Only owned local fixtures. No network, package manager, agent or provider."""
import fcntl
import json
from itertools import islice
import os
from pathlib import Path
import pty
import select
import signal
import shutil
import socket
import stat
import struct
import subprocess
import sys
import tempfile
import termios
import time


INSTALLER = r'''#!/usr/bin/python3
import json, os, signal, sys, time
from pathlib import Path
root = Path.cwd()
descriptor = int(os.environ["ASB_INSTALL_PROGRESS_FD"])
try:
    target = os.readlink(f"/proc/self/fd/{descriptor}")
except FileNotFoundError:
    target = "closed"
assert not target.startswith("socket:"), "installer inherited private progress descriptor"
mode = (root / "mode").read_text()
if mode == "fail":
    print("fixture failure with secret credential-looking-value", file=sys.stderr)
    sys.exit(7)
if mode == "success":
    destination = Path(sys.argv[sys.argv.index("--prefix") + 1]) / "node_modules/.bin/pi"
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_text("#!/bin/sh\nexit 99\n")
    destination.chmod(0o755)
    sys.exit(0)
(root / "principal.pid").write_text(str(os.getpid()))
child = os.fork()
if child == 0:
    os.setsid()
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    (root / "child.pid").write_text(str(os.getpid()))
    grandchild = os.fork()
    if grandchild == 0:
        (root / "grandchild.pid").write_text(str(os.getpid()))
        with (root / "writes").open("ab", buffering=0) as output:
            while True:
                output.write(b"x")
                time.sleep(.01)
    os._exit(0)  # double fork: only the gate can retain this descendant
while True:
    time.sleep(1)
'''


def wait_until(predicate, description, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(.01)
    raise AssertionError(description)


def project(parent, name, mode):
    root = parent / name
    for directory in [".asb", "home", "workloads", "fixture-bin"]:
        (root / directory).mkdir(parents=True)
    (root / ".asb/project.toml").write_text("tools_dir='.asb-tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n")
    (root / "workloads/bug-fix.txt").write_text("Never run.\n")
    (root / "mode").write_text(mode)
    npm = root / "fixture-bin/npm"
    npm.write_text(INSTALLER)
    npm.chmod(0o755)
    node = root / "fixture-bin/node"
    node.write_text("#!/bin/sh\nprintf 'v22.0.0\\n'\n")
    node.chmod(0o755)
    # Exercise a current, unique workload identifier without network access.
    # The fake extractor creates the smallest valid pinned AgentBench tree.
    curl = root / "fixture-bin/curl"
    curl.write_text("#!/bin/sh\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --output ]; then shift; printf fixture > \"$1\"; exit 0; fi\n  shift\ndone\nexit 1\n")
    curl.chmod(0o755)
    tar = root / "fixture-bin/tar"
    tar.write_text("#!/bin/sh\ndest=\nwhile [ $# -gt 0 ]; do\n  if [ \"$1\" = --directory ] || [ \"$1\" = -C ]; then shift; dest=$1; fi\n  shift\ndone\n[ -n \"$dest\" ] || exit 2\n/bin/mkdir -p \"$dest/AgentBench/configs/tasks\"\nprintf 'name: fixture\\n' > \"$dest/AgentBench/configs/tasks/fixture.yaml\"\n")
    tar.chmod(0o755)
    return root


def environment(root):
    result = {"PATH": str(root / "fixture-bin"), "HOME": str(root / "home"),
              "TERM": "xterm-256color", "NO_COLOR": "1"}
    if "LLVM_PROFILE_FILE" in os.environ:
        result["LLVM_PROFILE_FILE"] = os.environ["LLVM_PROFILE_FILE"]
    return result


def kill_owned_install_helper(process, binary, root):
    def bounded(path, limit):
        with path.open("rb") as stream:
            value = stream.read(limit + 1)
        assert len(value) <= limit, (path, "oversized fixture process evidence")
        return value

    def identity(pid):
        fields = bounded(Path(f"/proc/{pid}/stat"), 4096).rsplit(b")", 1)[1].split()
        return int(fields[1]), int(fields[19])  # parent PID, start time

    principal = int((root / "principal.pid").read_text())
    chain = []
    candidates = []
    pid = principal
    for _ in range(8):
        parent, started = identity(pid)
        chain.append((pid, parent, started))
        args = bounded(Path(f"/proc/{pid}/cmdline"), 8192).rstrip(b"\0").split(b"\0")
        if (len(args) == 7 and args[1:4] == [b"__asb-install", b"ASBINSTALL1", b"tool"]
                and args[6] == b"pi" and os.path.samefile(args[4], root)
                and os.path.samefile(args[5], root / ".asb-tools")):
            candidates.append((pid, parent, started, args))
        if pid == process.pid:
            break
        pid = parent
    assert chain[-1][0] == process.pid, ("not our TUI ancestry", chain)
    assert len(candidates) == 1, ("expected exactly one owned install helper", chain, candidates)
    helper, parent, started, observed_args = candidates[0]
    assert chain[0][1] != helper, "installer must have its independent inner guardian"
    descriptor = os.pidfd_open(helper)
    try:
        assert identity(helper) == (parent, started), "helper identity changed"
        assert bounded(Path(f"/proc/{helper}/cmdline"), 8192).rstrip(b"\0").split(b"\0") == observed_args
        assert os.path.samefile(f"/proc/{helper}/cwd", root)
        assert os.path.samefile(f"/proc/{helper}/exe", binary)
        print(json.dumps({"helper_loss_owned_ancestry": chain, "helper": helper,
                          "principal_parent": chain[0][1], "image_matched": True,
                          "helper_args": [os.fsdecode(arg) for arg in observed_args]}), flush=True)
        signal.pidfd_send_signal(descriptor, signal.SIGKILL)
    finally:
        os.close(descriptor)


def tui(binary, root, action):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    # A real PTY owns input/raw mode while the actual backend renders to a pipe.
    # Closing only its reader causes EPIPE, retaining the PTY for restoration.
    def controlling_terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    process = subprocess.Popen([binary, "setup", "tui"], cwd=root,
        env=environment(root), stdin=slave, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, preexec_fn=controlling_terminal)
    transcript = bytearray()
    output_open = True
    started = False
    triggered = False
    helper_signalled = False
    deadline = time.monotonic() + 15
    try:
        while time.monotonic() < deadline and process.poll() is None:
            if output_open and select.select([process.stdout], [], [], .02)[0]:
                chunk = os.read(process.stdout.fileno(), 65536)
                transcript.extend(chunk)
                if not started and b"Space" in transcript:
                    if action == "replacement":
                        replacement = root / "replacement"
                        replacement.write_text("#!/bin/sh\nexit 77\n")
                        replacement.chmod(0o755)
                        os.replace(replacement, binary)
                    os.write(master, b"pi ")
                    started = True
            else:
                time.sleep(.01)
            ready = (root / "grandchild.pid").is_file() and (root / "writes").exists()
            if action == "helper-loss" and ready and not helper_signalled:
                kill_owned_install_helper(process, binary, root)
                helper_signalled = True
            if action in ["epipe", "cancel", "escape", "owner-loss"] and ready and not triggered:
                triggered = True
                if action == "epipe":
                    process.stdout.close()
                    output_open = False
                elif action == "owner-loss":
                    process.kill()  # exactly our owned ASB; its guardian survives
                else:
                    os.write(master, b"\x03" if action == "cancel" else b"\x1b")
            if action in ["success", "fail", "replacement", "helper-loss"] and not triggered:
                expected = b"Cannot install" if action in ["fail", "helper-loss"] else b"status is ready"
                if expected in transcript:
                    triggered = True
                    os.write(master, b"\x03")
        assert process.poll() is not None, (action, "TUI exceeded deadline", transcript[-2000:])
        assert started and triggered, (action, transcript[-2000:], process.stderr.read())
        diagnostic = process.stderr.read()
        if action != "owner-loss":
            flags = termios.tcgetattr(slave)[3]
            assert flags & termios.ICANON and flags & termios.ECHO, (action, "terminal not restored")
        if action == "epipe":
            assert b"cannot draw setup install status" in diagnostic, diagnostic
            assert b"cannot restore terminal" in diagnostic, diagnostic
        assert b"credential-looking-value" not in diagnostic + transcript
        if action in ["success", "replacement"]:
            assert (root / ".asb-tools/pi/node_modules/.bin/pi").is_file()
        if action == "fail":
            with (root / ".asb/install.log").open("rb") as stream:
                summary = stream.read(4097)
            assert len(summary) <= 4096
            assert b"agent package installation failed (exit status: 7)" in summary
            assert b"verify the prerequisite tool and rerun `asb tool install`" in summary
            assert b"credential-looking-value" not in summary
            captures = list(islice(root.glob(".asb-install-step-*/output.log"), 9))
            assert 0 < len(captures) <= 8, captures
            failures = 0
            for capture in captures:
                descriptor = os.open(capture, os.O_RDONLY | os.O_NOFOLLOW)
                try:
                    metadata = os.fstat(descriptor)
                    assert stat.S_ISREG(metadata.st_mode)
                    assert stat.S_IMODE(metadata.st_mode) == 0o600
                    content = os.read(descriptor, 1025)
                    assert len(content) <= 1024
                    failures += content == b"fixture failure with secret credential-looking-value\n"
                finally:
                    os.close(descriptor)
            assert failures == 1
        if action == "helper-loss":
            assert helper_signalled
            assert b"missing final status" in transcript, transcript[-2000:]
        assert not list(root.glob("asb-sweep-*.toml")), "cancellation must not publish selection"
    finally:
        if process.poll() is None:
            process.kill()
        process.wait(timeout=10)
        os.close(master)
        os.close(slave)


def assert_quiescent(root):
    pids = [int((root / name).read_text()) for name in ["principal.pid", "child.pid", "grandchild.pid"]]
    wait_until(lambda: all(not Path(f"/proc/{pid}").exists() for pid in pids),
               f"owned installer descendants survived: {pids}")
    before = (root / "writes").read_bytes()
    time.sleep(.2)
    assert (root / "writes").read_bytes() == before, "writes continued after confirmed exit"


def start_helper(binary, root, mode, name):
    receiver, sender = socket.socketpair(socket.AF_UNIX, socket.SOCK_DGRAM)
    receipts, proof = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    env = environment(root)
    env["ASB_INSTALL_PROGRESS_FD"] = str(sender.fileno())
    env["ASB_GATE_RECEIPT_FD"] = str(proof.fileno())
    env["ASB_GATE_OWNER_PID"] = str(os.getpid())
    directory = root / ("workloads" if mode == "workload" else ".asb-tools")
    arguments = [binary, "__asb-gate", "--asb-launch-v1", "0", binary,
                 "__asb-install", "ASBINSTALL1", mode, str(root), str(directory), name]
    child = subprocess.Popen(arguments, cwd=root, env=env, pass_fds=(sender.fileno(), proof.fileno()),
                             stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    sender.close()
    proof.close()
    receipts.settimeout(5)
    hello = receipt(receipts)
    assert hello == b"ASBGATE2" + bytes([1, 0, 0, 0]) + struct.pack("<I", child.pid), hello
    wait_until(lambda: "State:\tT" in Path(f"/proc/{child.pid}/status").read_text(), "gate did not stop")
    os.kill(child.pid, signal.SIGCONT)
    receiver.settimeout(5)
    return child, receiver, receipts


def receipt(channel):
    data = b""
    while len(data) < 16:
        chunk = channel.recv(16 - len(data))
        assert chunk, "EOF is not a quiescence receipt"
        data += chunk
    return data


def finish_helper(child, receipts, expected_final):
    stdout, stderr = child.communicate(timeout=5)
    assert (child.returncode == 0) == (expected_final == 1) and stdout == stderr == b"", (stdout, stderr)
    assert receipt(receipts) == b"ASBGATE2" + bytes([2, child.returncode, 0, 0]) + struct.pack("<I", child.pid)
    receipts.close()


def helper_protocol(binary, root, mode="tool", name="pi", expected_stages=(1, 8), expected_final=1):
    child, receiver, receipts = start_helper(binary, root, mode, name)
    final = None
    stages = set()
    while final is None:
        frame = receiver.recv(9)
        assert len(frame) == 8 and frame[:4] == b"ASI1" and frame[7] == 0, frame
        if frame[4] != 0:
            assert frame[6] == 0, "final must not claim a progress stage"
            final = frame[4]
        else:
            assert 1 <= frame[6] <= 13, frame
            stages.add(frame[6])
    diagnostic = (root / ".asb/install.log").read_text() if (root / ".asb/install.log").exists() else ""
    assert final == expected_final, (final, stages, diagnostic)
    finish_helper(child, receipts, expected_final)
    assert set(expected_stages).issubset(stages), (expected_stages, stages)
    receiver.close()
    invalid = subprocess.run([binary, "__asb-install", "old"], cwd=root,
                             env=environment(root), capture_output=True, timeout=5)
    assert invalid.returncode != 0


def startup_failure(binary, root):
    master, slave = pty.openpty()
    def controlling_terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    try:
        with open("/dev/full", "wb") as full:
            process = subprocess.Popen([binary, "setup", "tui"], cwd=root,
                env=environment(root), stdin=slave, stdout=full,
                stderr=subprocess.PIPE, preexec_fn=controlling_terminal)
            _, diagnostic = process.communicate(timeout=5)
        assert process.returncode != 0 and b"cannot start setup TUI" in diagnostic, diagnostic
        assert b"cannot restore terminal" in diagnostic, diagnostic
        flags = termios.tcgetattr(slave)[3]
        assert flags & termios.ICANON and flags & termios.ECHO, "partial startup retained raw mode"
        assert not (root / "principal.pid").exists()
    finally:
        os.close(master)
        os.close(slave)


def main():
    binary = os.path.abspath(sys.argv[1])
    with tempfile.TemporaryDirectory(prefix="asb-tui-install-") as temporary:
        parent = Path(temporary)
        sentinel = subprocess.Popen(["/usr/bin/sleep", "60"])
        try:
            for action in ["success", "fail", "epipe", "cancel", "escape", "owner-loss", "helper-loss"]:
                root = project(parent, action, action if action in ["success", "fail", "helper-loss"] else "slow")
                tui(binary, root, action)
                if action not in ["success", "fail"]:
                    assert_quiescent(root)
                assert sentinel.poll() is None, "unrelated process was signalled"
            helper_protocol(binary, project(parent, "helper", "success"))
            workload = project(parent, "helper-workload", "success")
            helper_protocol(binary, workload, "workload", "agentbench", (1, 3, 12))
            helper_protocol(binary, project(parent, "helper-dependencies", "fail"),
                            "dependencies", "flamegraph", (2, 3), 2)
            startup_failure(binary, project(parent, "startup", "slow"))
            root = project(parent, "replacement", "success")
            copied = root / "asb"
            shutil.copyfile(binary, copied)
            copied.chmod(0o755)
            tui(str(copied), root, "replacement")
            root = project(parent, "no-terminal", "slow")
            result = subprocess.run([binary, "setup", "tui"], cwd=root, env=environment(root),
                                    stdin=subprocess.DEVNULL, capture_output=True, timeout=5)
            assert result.returncode != 0 and not (root / "principal.pid").exists()
        finally:
            sentinel.terminate()
            sentinel.wait(timeout=5)


if __name__ == "__main__":
    main()
