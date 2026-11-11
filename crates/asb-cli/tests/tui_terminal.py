"""Real bounded PTY fixture. No live provider, tool installation or agent run."""
import errno
import codecs
import fcntl
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import sys
import tempfile
import termios
import time
import tomllib


class Screen:
    """Read Ratatui's cursor-addressed screen, including partial ANSI chunks."""
    def __init__(self):
        self.rows = [[" "] * 120 for _ in range(40)]
        self.row = self.column = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, chunk):
        self.pending += self.decoder.decode(chunk)
        while self.pending:
            if self.pending == "\x1b":
                return
            if self.pending.startswith("\x1b["):
                match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", self.pending)
                if not match:
                    return
                parameters, _, command = match.groups()
                values = [int(value or "0") for value in parameters.split(";")] if re.fullmatch(r"[0-9;]*", parameters) else []
                if command in ["H", "f"]:
                    self.row = (values[0] or 1) - 1 if values else 0
                    self.column = (values[1] or 1) - 1 if len(values) > 1 else 0
                elif command == "J" and values == [2]:
                    self.rows = [[" "] * 120 for _ in range(40)]
                self.pending = self.pending[match.end():]
                continue
            char, self.pending = self.pending[0], self.pending[1:]
            if char == "\r":
                self.column = 0
            elif char == "\n":
                self.row += 1
            elif char >= " " and self.row < 40 and self.column < 120:
                self.rows[self.row][self.column] = char
                self.column += 1

    def contains(self, text):
        return text.decode() in "\n".join("".join(row) for row in self.rows)


def session(binary, root, arguments, keys, path="", checkpoints=()):
    pid, master = pty.fork()
    if pid == 0:
        os.chdir(root)
        environment = {"PATH": path, "HOME": str(root / "home"),
                       "TERM": "xterm-256color", "NO_COLOR": "1"}
        if "LLVM_PROFILE_FILE" in os.environ:
            environment["LLVM_PROFILE_FILE"] = os.environ["LLVM_PROFILE_FILE"]
        os.execve(binary, [binary, *arguments], environment)
    return drive_session(master, pid, keys, checkpoints)


def drive_session(master, pid, keys, checkpoints=(), *, status=None, read_size=65536):
    """Own/reap the child and drain its PTY through EOF, not merely exit."""
    transcript = bytearray()
    deadline = time.monotonic() + 15
    sent = False
    eof = False
    screen = Screen()
    pending = iter(checkpoints)
    checkpoint = next(pending, None)
    try:
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        while time.monotonic() < deadline:
            if not eof and select.select([master], [], [], .05)[0]:
                try:
                    chunk = os.read(master, read_size)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    chunk = b""
                eof = not chunk
                transcript.extend(chunk)
                screen.feed(chunk)
                if not sent and b"ASB" in transcript and b"Space" in transcript:
                    if keys:
                        os.write(master, keys)
                    sent = True
                elif sent and checkpoint and screen.contains(checkpoint[0]):
                    _, next_keys, inspect = checkpoint
                    inspect()
                    os.write(master, next_keys)
                    checkpoint = next(pending, None)
            if status is None:
                found, value = os.waitpid(pid, os.WNOHANG)
                if found:
                    status = value
            if status is not None and eof:
                break
            if eof:
                # EOF can precede waitpid readiness. Do not spin on repeated
                # EIO, and do not extend the original absolute deadline.
                time.sleep(.005)
        assert status is not None, f"TUI exceeded bounded deadline: {bytes(transcript[-3000:])!r}"
        assert eof, f"TUI exited without PTY EOF before deadline: {bytes(transcript[-3000:])!r}"
        assert sent, f"TUI did not draw its landing screen: {bytes(transcript)!r}"
        assert checkpoint is None, f"TUI exited before checkpoint: {checkpoint[0]!r}"
        assert b"\x1b[?1049h" in transcript, "must enter alternate screen"
        assert b"\x1b[?1049l" in transcript, "must restore alternate screen"
        flags = termios.tcgetattr(master)[3]
        assert flags & termios.ICANON and flags & termios.ECHO, "must restore canonical input and echo"
        return os.waitstatus_to_exitcode(status), bytes(transcript)
    finally:
        if status is None:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(master)


def reaped_child_tail_regression():
    """Real PTY: child exit precedes every tiny read of its queued final tail."""
    payload = b"\x1b[?1049hASB Space " + b"queued tail " * 8 + b"\x1b[?1049l"
    pid, master = pty.fork()
    if pid == 0:
        # Below PTY capacity, so exit never depends on concurrent parent reads.
        os.write(1, payload)
        os._exit(0)
    status = None
    deadline = time.monotonic() + 5
    try:
        while time.monotonic() < deadline:
            found, value = os.waitpid(pid, os.WNOHANG)
            if found:
                status = value
                break
            time.sleep(.005)
        assert status is not None, "queued-tail fixture child did not exit"
    except BaseException:
        if status is None:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        os.close(master)
        raise
    code, transcript = drive_session(master, pid, b"", status=status, read_size=7)
    assert code == 0 and transcript == payload, transcript


def workload_sessions(binary, parent, path):
    """Advertised finish/edit/back keys only; check disk while the TUI is alive."""
    for scenario in ["finish", "edit", "back", "multiple", "deselect", "reselect",
                     "cancel-saved", "cancel-edit", "cancel-existing", "missing-subset"]:
        root = parent / scenario
        for directory in [".asb", "home", "workloads/alpha/source", "workloads/beta/source"]:
            (root / directory).mkdir(parents=True)
        (root / ".asb/project.toml").write_text("tools_dir='.asb-tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n")
        for workload in ["alpha", "beta"]:
            (root / f"workloads/{workload}.txt").write_text("Offline task, never executed.\n")
        prior = root / "workloads/alpha.workload.toml"
        prior_bytes = b"workload='alpha'\n[[trials]]\nid='extended'\ndescription='Existing selection'\n"
        if scenario == "cancel-existing":
            prior.write_bytes(prior_bytes)

        def unpublished():
            paths = list((root / "workloads").glob("*.workload.toml"))
            if scenario == "cancel-existing":
                assert paths == [prior] and prior.read_bytes() == prior_bytes, scenario
            else:
                assert not paths, scenario

        saved = b"trials saved in memory"
        editing = b"Choose enabled trials for alpha"
        start = b"alpha \r \r"  # select workload, open, select smoke, save
        checkpoints = [(saved, b"\r", unpublished)]
        expected = {"alpha": ["smoke"]}
        if scenario == "edit":
            start = b"alpha \r \x1b[B \r"  # smoke + standard
            checkpoints = [(saved, b"\t", unpublished),
                           (editing, b" \r\r", unpublished)]
            expected = {"alpha": ["standard"]}
        elif scenario == "back":
            checkpoints = [(saved, b"\t", unpublished),
                           (editing, b" \x1b[Z\r\r\r", unpublished)]
        elif scenario in ["multiple", "deselect", "reselect", "missing-subset"]:
            # A search/move resumes browsing; deleting the alpha filter reaches beta.
            checkpoints = [(saved, b"\x7f" * 5 + b"beta ", unpublished)]
            if scenario == "missing-subset":
                # Alpha's saved subset cannot authorize publication of unconfigured beta.
                checkpoints += [(b"Enter/Tab opens", b"\x7f" * 4 + b"alpha\r\r\r", unpublished),
                                (b"Save trials for every selected workload", b"\x03", unpublished)]
                expected = {}
            else:
                checkpoints += [(b"Enter/Tab opens", b"\r \r", unpublished)]
                if scenario == "multiple":
                    checkpoints += [(saved, b"\r", unpublished)]
                    expected = {"alpha": ["smoke"], "beta": ["smoke"]}
                else:
                    # Deselect alpha after both have saved drafts, then re-open beta.
                    checkpoints += [(saved, b"\x7f" * 4 + b"alpha ", unpublished),
                                    (b"Unsaved edits discarded", b"\x7f" * 5 + b"beta\r\r", unpublished)]
                    if scenario == "deselect":
                        checkpoints += [(saved, b"\r", unpublished)]
                        expected = {"beta": ["smoke"]}
                    else:
                        # Re-select alpha: its prior smoke choice must be restored.
                        checkpoints += [(saved, b"\x7f" * 4 + b"alpha \r\r\r", unpublished)]
                        expected = {"alpha": ["smoke"], "beta": ["smoke"]}
        elif scenario in ["cancel-saved", "cancel-existing"]:
            checkpoints = [(saved, b"\x03", unpublished)]
            expected = {}
        elif scenario == "cancel-edit":
            start = b"alpha \r "
            checkpoints = [(editing, b"\x03", unpublished)]
            expected = {}
        code, transcript = session(binary, root, ["workload", "tui"], start, path, checkpoints)
        assert (code == 0) == bool(expected), (scenario, transcript)
        if scenario == "cancel-existing":
            unpublished()
            continue
        paths = list((root / "workloads").glob("*.workload.toml"))
        assert {item.name for item in paths} == {f"{name}.workload.toml" for name in expected}, scenario
        for name, ids in expected.items():
            config = tomllib.loads((root / f"workloads/{name}.workload.toml").read_text())
            assert config["workload"] == name, scenario
            assert [trial["id"] for trial in config["trials"]] == ids, (scenario, config)


def main():
    reaped_child_tail_regression()
    binary = str(Path(sys.argv[1]).resolve())
    target = Path(__file__).resolve().parents[3] / "target"
    target.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="tui-pty-", dir=target) as temporary:
        root = Path(temporary)
        for directory in [".asb", "workloads/custom/source", "home", ".asb-tools/pi/node_modules/.bin"]:
            (root / directory).mkdir(parents=True, exist_ok=True)
        (root / ".asb/project.toml").write_text("tools_dir='.asb-tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n")
        for workload in ["bug-fix", "custom"]:
            (root / f"workloads/{workload}.txt").write_text("Offline task, never executed.\n")
        agent = root / ".asb-tools/pi/node_modules/.bin/pi"
        agent.write_text("#!/bin/sh\nprintf unexpected > invoked-agent\nexit 99\n")
        agent.chmod(0o755)
        # The navigation sequence selects perf. Supply an inert project-local
        # executable so this offline PTY fixture never downloads host tooling.
        perf = root / ".asb-tools/perf/bin/perf"
        perf.parent.mkdir(parents=True)
        perf.write_text("#!/bin/sh\nexit 99\n")
        perf.chmod(0o755)

        # Positive navigation must not inherit host collectors: their default
        # selection can prepare project-local dependencies and consume queued
        # navigation keys during installation. The empty child PATH keeps
        # these cases independent of installed host tools. Install/error cases
        # below instead pass an explicit PATH of inert tool fixtures.
        # Real search/select/step/concurrency events, including backwards nav.
        code, transcript = session(binary, root, ["setup", "tui"],
            b"pi \t\x1b[Z\tlocal \t \t\tbug-fix \t\t\x1b[B \t\t")
        assert code == 0, transcript
        assert b"Generated 1 benchmark configurations" in transcript, transcript
        sweep_files = list(root.glob("asb-sweep-*.toml"))
        assert len(sweep_files) == 1
        manifest = tomllib.loads(sweep_files[0].read_text())
        assert len(manifest["configs"]) == 1
        config = tomllib.loads((root / manifest["configs"][0]).read_text())
        assert config["agent"] == "pi" and config["provider"] == "local"
        assert config["workload"] == "bug-fix" and config["concurrency"] == 2
        assert "runtime" not in config, "portable default should not write runtime settings"
        assert not (root / "invoked-agent").exists()
        assert not (root / ".asb/install.log").exists(), "navigation must not install dependencies"

        # Select the CPU-pool option, edit both fields and opt in to CPU-wide
        # recording through real terminal events. No benchmark is executed.
        code, transcript = session(binary, root, ["setup", "tui"],
            b"pi \tlocal \t \t\tbug-fix \t\t\x1b[B \t\x1b[B \x10" +
            b"1-3,5-7\r\x073\r\x17\t")
        assert code == 0, transcript[-3000:]
        assert b"CPU affinity: pool" in transcript
        assert b"CPUs (Ctrl-P):" in transcript
        assert b"CPUs per instance (Ctrl-G):" in transcript
        assert b"not configured" in transcript
        runtime_sweeps = set(root.glob("asb-sweep-*.toml")) - set(sweep_files)
        assert len(runtime_sweeps) == 1, transcript[-3000:]
        runtime_manifest = tomllib.loads(runtime_sweeps.pop().read_text())
        runtime_config = tomllib.loads((root / runtime_manifest["configs"][0]).read_text())
        assert runtime_config["runtime"]["perf_record_scope"] == "cpu-wide"
        assert runtime_config["runtime"]["cpu_pool"] == {"cpus": "1-3,5-7", "num_cpus_per_instance": 3}
        sweep_files = list(root.glob("asb-sweep-*.toml"))

        # End reaches the real public maximum; publication must retain numeric
        # 512, and single-selection must replace the default rather than add it.
        code, transcript = session(binary, root, ["setup", "tui"],
            b"pi \tlocal \t \t\tbug-fix \t\t\x1b[F \t\t")
        assert code == 0, transcript
        next_sweeps = set(root.glob("asb-sweep-*.toml")) - set(sweep_files)
        assert len(next_sweeps) == 1, transcript
        manifest64 = tomllib.loads(next_sweeps.pop().read_text())
        assert len(manifest64["configs"]) == 1
        config64 = tomllib.loads((root / manifest64["configs"][0]).read_text())
        assert config64["concurrency"] == 512 and isinstance(config64["concurrency"], int)
        assert not (root / "invoked-agent").exists()
        assert not (root / ".asb/install.log").exists(), "navigation must not install dependencies"

        # Reach the real final publication event with a configuration which
        # would exceed run's 64KiB reader. No sweep/config/private staging is
        # created, prior valid generations remain byte-identical, and terminal
        # restoration must still happen on this late error.
        prior_setup = {item: item.read_bytes() for item in root.glob("asb-sweep-*.toml")}
        prior_paths = set(root.iterdir())
        project_file = root / ".asb/project.toml"
        original_project = project_file.read_text()
        project_file.write_text("tools_dir='.asb-tools'\nworkloads_dir='workloads'\nworkspace_dir='" + "x" * 70_000 + "'\n")
        code, transcript = session(binary, root, ["setup", "tui"],
            b"pi \tlocal \t \t\tbug-fix \t\t\t\t")
        assert code != 0 and b"64 KiB" in transcript, transcript[-3000:]
        assert b"no setup files were written" in transcript, transcript[-3000:]
        assert set(root.iterdir()) == prior_paths
        assert all(item.read_bytes() == data for item, data in prior_setup.items())
        project_file.write_text(original_project)

        # The actual project path can contain raw OS bytes even though the
        # generated configuration wire format cannot represent an absolute
        # measurement path there. Refuse before any setup/staging creation.
        raw_root = root / os.fsdecode(b"project-\xff")
        for directory in [".asb", "workloads", "home", ".asb-tools/pi/node_modules/.bin"]:
            (raw_root / directory).mkdir(parents=True, exist_ok=True)
        (raw_root / ".asb/project.toml").write_text(original_project)
        (raw_root / "workloads/bug-fix.txt").write_text("Offline task, never executed.\n")
        raw_agent = raw_root / ".asb-tools/pi/node_modules/.bin/pi"
        raw_agent.write_bytes(agent.read_bytes())
        raw_agent.chmod(0o755)
        raw_perf = raw_root / ".asb-tools/perf/bin/perf"
        raw_perf.parent.mkdir(parents=True)
        raw_perf.write_bytes(perf.read_bytes())
        raw_perf.chmod(0o755)
        raw_prior = raw_root / "prior.toml"
        raw_prior.write_bytes(b"prior configuration unchanged")
        raw_paths = set(raw_root.iterdir())
        code, transcript = session(binary, raw_root, ["setup", "tui"],
            b"pi \tlocal \t \t\tbug-fix \t\t\t\t")
        assert code != 0 and b"UTF-8" in transcript, transcript[-3000:]
        assert b"no setup files were written" in transcript, transcript[-3000:]
        assert set(raw_root.iterdir()) == raw_paths
        assert raw_prior.read_bytes() == b"prior configuration unchanged"
        assert not (raw_root / "invoked-agent").exists()

        # Both cancellation encodings exercise restoration and bounded exit.
        for arguments in [["setup", "tui"], ["workload", "tui"]]:
            for key in [b"\x03", b"\x1b"]:
                code, transcript = session(binary, root, arguments, key)
                assert code != 0, transcript
                assert b"cancelled" in transcript, transcript
        assert not (root / "invoked-agent").exists()

        # Trigger actual background install/error paths. Every external fetch
        # or package manager is an inert fixture and cannot forward requests.
        fixture_bin = root / "fixture-bin"
        fixture_bin.mkdir()
        invoked = root / "fixture-installer-invoked"
        for name in ["curl", "npm", "node", "sudo", "git", "make", "tar"]:
            tool = fixture_bin / name
            tool.write_text(f"#!/bin/sh\nprintf '{name}\\n' >> '{invoked}'\nexit 7\n")
            tool.chmod(0o755)
        agent.unlink()
        # Cancellation can now stop an admitted helper before its first tool
        # starts. Wait for this fixture's failed attempt before cancelling;
        # the dedicated custody matrix tests cancellation during live installs.
        failed_install = [(b"Cannot install", b"\x03", lambda: None)]
        code, transcript = session(binary, root, ["setup", "tui"], b"pi ",
                                   str(fixture_bin), failed_install)
        assert code != 0, transcript
        assert invoked.is_file(), "missing agent selection must attempt installation"
        assert not agent.exists(), "failed package install must not mark agent installed"
        assert set(root.glob("asb-sweep-*.toml")) == set(prior_setup), "failed selection must not publish a sweep"

        (root / "workloads/custom.txt").unlink()
        invoked.unlink()
        code, transcript = session(binary, root, ["workload", "tui"],
                                   b"custom ", str(fixture_bin), failed_install)
        assert code != 0, transcript
        assert not (root / "workloads/custom.txt").exists(), "failed source install must not fabricate workload"
        assert not (root / "invoked-agent").exists()

        # A configured/discovered provider cannot be installed, and selecting
        # an installed collector must still prepare its missing dependency.
        agent.write_text("#!/bin/sh\nexit 99\n")
        agent.chmod(0o755)
        code, transcript = session(binary, root, ["setup", "tui"],
            b"pi \t\x1b[105;5u\x03", str(fixture_bin))
        assert code != 0, transcript
        flamegraph = root / ".asb-tools/flamegraph/bin/flamegraph"
        flamegraph.parent.mkdir(parents=True)
        flamegraph.write_text("#!/bin/sh\nexit 99\n")
        flamegraph.chmod(0o755)
        perf.unlink()
        invoked.unlink(missing_ok=True)
        code, transcript = session(binary, root, ["setup", "tui"],
            b"pi \tlocal \t \t\tbug-fix \tflamegraph ", str(fixture_bin),
            [(b"Cannot install required collector", b"\x03", lambda: None)])
        assert code != 0, transcript
        assert invoked.is_file(), "missing collector dependency must attempt installation"
        assert not (root / ".asb-tools/perf/bin/perf").exists(), "failed dependency must not be marked installed"
        assert set(root.glob("asb-sweep-*.toml")) == set(prior_setup)

        workload_sessions(binary, root, str(fixture_bin))
        assert not (root / "invoked-agent").exists()


if __name__ == "__main__":
    main()
