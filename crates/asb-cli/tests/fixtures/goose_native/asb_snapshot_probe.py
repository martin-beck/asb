"""Disposable local fixture using ASB's real snapshot marker service."""
import shutil
import sys
import threading
from pathlib import Path

sys.path.insert(0, sys.argv[1])
from asb_snapshot_service import MarkerServer, SnapshotService, SnapshotReplay, load_replay_boundaries

workspace, socket = map(Path, sys.argv[2:4])
store = workspace.parent / 'snapshots'

class Switching:
    def __init__(self):
        self.record = SnapshotService(workspace, store, ['fixture'], snapshot=lambda dst: shutil.copytree(workspace, dst))
        self.replay = None

    def marker(self, action, alias, process=None, thread=None):
        if self.replay is not None:
            return self.replay.marker(action, alias, process, thread)
        return self.record.marker(action, alias, process, thread)

    def seal(self):
        self.record.finish()
        def apply(number):
            for child in workspace.iterdir():
                shutil.rmtree(child) if child.is_dir() and not child.is_symlink() else child.unlink()
            shutil.copytree(store / f'snapshot-{number:08d}', workspace, dirs_exist_ok=True)
        self.replay = SnapshotReplay(load_replay_boundaries(store), apply)

    def invalidate(self):
        (self.replay or self.record).invalidate()

service = Switching()
server = MarkerServer(socket, service)
worker = threading.Thread(target=server.serve_forever)
worker.start()
print('READY', flush=True)
try:
    if sys.stdin.readline() != 'SEAL\n':
        raise RuntimeError('record control ended unexpectedly')
    service.seal()
    print('SEALED', flush=True)
    if sys.stdin.readline() != 'STOP\n':
        raise RuntimeError('control ended unexpectedly')
    server.shutdown()
    service.replay.finish()
finally:
    server.interrupt_clients()
    server.server_close()
    worker.join()
    socket.unlink(missing_ok=True)
