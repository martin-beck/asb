"""Offline tape owner-loss and outbound-child cleanup qualification."""
import ctypes
import http.client
import json
import os
from pathlib import Path
import pty
import signal
import subprocess
import sys
import time
from test_environment import coverage_env

from native_gate_owner import cleanup_children, publish_pid, signal_pinned, until


def owner(scripts, root, phase):
    mode = "replay" if phase == "replay" else "record"
    args = ["node", str(scripts / "llmtape.mjs"), mode, "--port", "0"]
    args += (["--cassette", str(root / "tape")] if mode == "replay" else
             ["--upstream", "http://127.0.0.1:1", "--out", str(root / "tape")])
    env = {"PATH": str(root / "bin") + os.pathsep + os.environ["PATH"],
           "HOME": str(root), "ASB_TAPE_OWNER_PIPE": "1",
           "ASB_TAPE_READY_FILE": str(root / "ready"),
           "http_proxy": "http://127.0.0.1:1", "PYTHONDONTWRITEBYTECODE": "1"}
    proxy = subprocess.Popen(args, env=env, stdin=subprocess.PIPE,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    publish_pid(root / "proxy", proxy.pid)
    if phase == "closed-before-ready":
        proxy.stdin.close()
    time.sleep(30)


def public_cli(binary, root, sibling):
    for measured in (False, True):
        case = root / ("cli-measured" if measured else "cli-unmeasured")
        case.mkdir()
        env = coverage_env({"PATH": os.environ["PATH"], "HOME": str(case), "PYTHONDONTWRITEBYTECODE": "1"})
        initialized = subprocess.run([binary, "init"], cwd=case, env=env, input=b"",
                                     stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=20)
        assert initialized.returncode == 0, initialized.stderr.decode()
        agent = case / ".asb-tools/pi/bin/pi"
        agent.parent.mkdir(parents=True)
        agent.write_text(f"#!{sys.executable}\nimport os\n"
                         f"os.execv({sys.executable!r}, [{sys.executable!r}, "
                         f"{str(Path(__file__).with_name('native_gate_owner.py'))!r}, '--client', {str(case)!r}])\n")
        agent.chmod(0o755)
        (case / "tape").write_text(json.dumps({
            "request": {"method": "POST", "url": "/v1/responses", "body": json.dumps({
                "model": "test-model", "input": "fixture", "tools": [{"type": "function"}]
            })},
            "response": {"status": 200, "headers": {}, "body": "{}"}
        }) + "\n")
        config = "agent='pi'\nprovider='cli2key'\nmodel='test-model'\nworkload='bug-fix'\n"
        if measured:
            config += "[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=[]\n"
        (case / "benchmark.toml").write_text(config)
        # The capacity guard intentionally accepts low-space execution only
        # after an interactive confirmation. Give the public binary a PTY and
        # answer that real prompt; do not relax its non-interactive policy.
        terminal, child_terminal = pty.openpty()
        process = subprocess.Popen([binary, "run", "benchmark.toml", "--replay", "--cassette", "tape"],
                                   cwd=case, env=env, stdin=child_terminal,
                                   stdout=child_terminal, stderr=child_terminal)
        os.close(child_terminal)
        os.write(terminal, b"y\n")
        try:
            def started():
                if process.poll() is not None:
                    os.set_blocking(terminal, False)
                    output = bytearray()
                    while True:
                        try:
                            chunk = os.read(terminal, 8192)
                        except BlockingIOError:
                            break
                        if not chunk:
                            break
                        output.extend(chunk)
                    raise AssertionError(output.decode(errors="replace"))
                return (case / "principal").exists() and (case / "helper").exists()
            # The PTY-confirmed capacity admission can add a scheduling turn
            # before the native client is launched. Keep this owner-loss
            # qualification bounded, but leave enough room for a loaded CI
            # runner to publish the two process markers.
            until(started, "public CLI did not start native client", seconds=30)
            ready_files = list(case.glob(".asb-tape-*/ready.json"))
            assert len(ready_files) == 1
            proxy = json.loads(ready_files[0].read_text())["pid"]
            children = Path(f"/proc/{process.pid}/task/{process.pid}/children").read_text().split()
            gates = [int(pid) for pid in children if b"__asb-gate" in Path(f"/proc/{pid}/cmdline").read_bytes()]
            assert len(gates) == 1, "public CLI did not install its native gate"
            process.kill()
            process.wait(timeout=3)
            for pid in [proxy, *gates]:
                until(lambda: os.waitpid(pid, os.WNOHANG)[0] == pid,
                      "public CLI owner loss left a gate/proxy running")
            for name in ("principal", "helper"):
                pid = int((case / name).read_text())
                assert not Path(f"/proc/{pid}").exists(), "public CLI left an agent/helper running"
            assert not (case / "late-write").exists()
            assert sibling.poll() is None
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=3)
            os.close(terminal)


def guardian(scripts, root, binary):
    assert ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) == 0
    sibling = subprocess.Popen([sys.executable, "-c", "import time;time.sleep(60)"])
    try:
        for phase in ("record", "replay", "outbound", "signal", "closed-before-ready"):
            case = root / phase
            (case / "bin").mkdir(parents=True)
            (case / "tape").write_text("")
            curl = case / "bin/curl"
            curl.write_text(f"#!{sys.executable}\nimport os,signal,time\n"
                            "from pathlib import Path\n"
                            "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                            f"Path({str(case / 'curl')!r}).write_text(str(os.getpid()))\n"
                            "time.sleep(30)\n")
            curl.chmod(0o755)
            process = subprocess.Popen([sys.executable, "-B", __file__, "--owner", str(scripts), str(case), phase])
            connection = None
            try:
                until(lambda: (case / "proxy").exists(), "owner did not publish its proxy")
                proxy = int((case / "proxy").read_text())
                if phase != "closed-before-ready":
                    until(lambda: (case / "ready").exists(), "proxy never became ready")
                    ready = json.loads((case / "ready").read_text())
                    assert ready["pid"] == proxy
                    if phase in ("outbound", "signal"):
                        connection = http.client.HTTPConnection("127.0.0.1", ready["port"], timeout=2)
                        connection.request("POST", "/v1/responses", "{}", {"Content-Type": "application/json"})
                        until(lambda: (case / "curl").exists(), "proxy did not start outbound subprocess")
                if phase == "signal":
                    signal_pinned(proxy, signal.SIGTERM)
                else:
                    process.kill()
                    process.wait(timeout=3)

                def stopped():
                    try:
                        return os.waitpid(proxy, os.WNOHANG)[0] == proxy
                    except ChildProcessError:
                        return not Path(f"/proc/{proxy}").exists() or (
                            phase == "signal" and "\nState:\tZ" in Path(f"/proc/{proxy}/status").read_text()
                        )
                until(stopped, f"proxy survived {phase}", seconds=4)
                if (case / "curl").exists():
                    pid = int((case / "curl").read_text())
                    assert not Path(f"/proc/{pid}").exists(), "outbound helper was not stopped and reaped"
                assert sibling.poll() is None, "unrelated sibling was stopped"
                assert (case / "tape").read_text() == "", "interrupted response fabricated a recording"
                if phase == "signal":
                    process.kill()
                    process.wait(timeout=3)
                    until(stopped, "proxy was not reaped after owner exit")
            finally:
                if connection:
                    connection.close()
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=3)
        public_cli(binary, root, sibling)
    finally:
        cleanup_children()


if __name__ == "__main__":
    if sys.argv[1] == "--owner":
        owner(Path(sys.argv[2]), Path(sys.argv[3]), sys.argv[4])
    else:
        guardian(Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3])
