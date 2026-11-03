"""Guest-owned lifecycle for the experimental filesystem operation recorder."""
import os
from pathlib import Path
import re
import subprocess
import sys
import time


def _mount_ids(path):
    """Find exact mount instances, including a pre-existing workspace mount."""
    target = str(path)
    result = set()
    for line in Path('/proc/self/mountinfo').read_text(errors='surrogateescape').splitlines():
        fields = line.split(' ', 5)
        if len(fields) < 5:
            continue
        mountpoint = re.sub(r'\\([0-7]{3})', lambda match: chr(int(match.group(1), 8)), fields[4])
        if mountpoint == target:
            result.add(int(fields[0]))
    return result


class FilesystemRecorder:
    """Mount the recorder over a task workspace until the attempt settles."""
    def __init__(self, workspace, run_dir, proxy, binding, library):
        self.workspace = Path(workspace).resolve()
        self.run_dir = Path(run_dir).resolve()
        self.proxy = Path(proxy).resolve()
        self.binding = Path(binding).resolve()
        self.library = Path(library).resolve()
        self.lower = self.run_dir / 'filesystem-lower'
        self.recording = self.run_dir / 'filesystem-recording'
        self.process = None
        self.bound = False
        self.log = None
        self.original_mounts = None

    def recorder_mounted(self):
        return bool(_mount_ids(self.workspace) - self.original_mounts)

    def __enter__(self):
        if not self.workspace.is_dir() or not self.proxy.is_file() or not self.library.is_file():
            raise RuntimeError('filesystem recorder workspace or staged dependencies are unavailable')
        self.lower.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.recording.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.original_mounts = _mount_ids(self.workspace)
        try:
            subprocess.run(['mount', '--bind', str(self.workspace), str(self.lower)], check=True,
                           stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            self.bound = True
            env = dict(os.environ, PYTHONPATH=str(self.binding), FUSE_LIBRARY_PATH=str(self.library),
                       ASB_PROBE_ROOT_TGID=str(os.getpid()), ASB_PROBE_ALLOW_OTHER='1', ASB_PROBE_NONEMPTY='1')
            self.log = (self.run_dir / 'filesystem-recorder.log').open('wb')
            self.process = subprocess.Popen([sys.executable, str(self.proxy), str(self.lower),
                                             str(self.workspace), str(self.recording)],
                                            env=env, stdin=subprocess.DEVNULL,
                                            stdout=self.log, stderr=subprocess.STDOUT)
            deadline = time.monotonic() + 30
            while not self.recorder_mounted():
                if self.process.poll() is not None:
                    raise RuntimeError('filesystem recorder exited before mounting; inspect its log')
                if time.monotonic() >= deadline:
                    raise RuntimeError('filesystem recorder mount did not become ready; inspect its log')
                time.sleep(.05)
            return self
        except BaseException:
            try:
                self.close()
            except Exception:
                pass
            raise

    def close(self):
        error = None
        if self.process is not None:
            if self.recorder_mounted():
                result = subprocess.run(['umount', str(self.workspace)], stdin=subprocess.DEVNULL,
                                        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
                if result.returncode:
                    error = RuntimeError('filesystem recorder mount could not be unmounted')
            if error is None:
                try:
                    if self.process.wait(timeout=15) != 0:
                        error = RuntimeError('filesystem recorder exited unsuccessfully; inspect its log')
                except subprocess.TimeoutExpired:
                    error = RuntimeError('filesystem recorder did not settle after unmount')
            if self.process.poll() is not None:
                self.process = None
        if self.bound and self.process is None and not self.recorder_mounted():
            result = subprocess.run(['umount', str(self.lower)], stdin=subprocess.DEVNULL,
                                    stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            if result.returncode:
                error = RuntimeError('filesystem recorder backing mount could not be unmounted')
            else:
                self.bound = False
        if self.log is not None and self.process is None:
            self.log.close()
            self.log = None
        if error is not None:
            raise error

    def __exit__(self, *_):
        self.close()
