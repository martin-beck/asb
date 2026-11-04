"""Independent disposable replay workspace using ASB's snapshot coordinator."""
import shutil
import sys
import threading
from pathlib import Path

sys.path.insert(0, sys.argv[1])
from asb_snapshot_service import MarkerServer, SnapshotReplay, load_replay_boundaries

store, workspace, socket = map(Path, sys.argv[2:5])
def apply(number):
    for child in workspace.iterdir():
        shutil.rmtree(child) if child.is_dir() and not child.is_symlink() else child.unlink()
    shutil.copytree(store / f'snapshot-{number:08d}', workspace, dirs_exist_ok=True)

service = SnapshotReplay(load_replay_boundaries(store), apply)
server = MarkerServer(socket, service)
worker = threading.Thread(target=server.serve_forever)
worker.start()
print('READY', flush=True)
try:
    if sys.stdin.readline() != 'STOP\n':
        raise RuntimeError('control ended unexpectedly')
    server.shutdown()
    service.finish()
finally:
    server.interrupt_clients()
    server.server_close()
    worker.join()
    socket.unlink(missing_ok=True)
