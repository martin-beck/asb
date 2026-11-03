"""Experimental guest replay with a live trace of direct workspace I/O."""
import os
import sys

from asb_fs_record import FilesystemRecorder
from asb_fs_replay_server import ReplayServer


def run(workspace, run_dir, proxy, binding, library, evidence, socket, control):
    # Replay effects use the backing mount directly. Agent operations use the
    # visible FUSE mount and are checked against the direct startup prefix.
    with FilesystemRecorder(workspace, run_dir, proxy, binding, library,
                            root_pid=os.getppid()) as recorder:
        server = ReplayServer(evidence, recorder.lower, socket,
                              direct_trace=recorder.recording)
        try:
            print('READY', flush=True)
            if control.readline() != 'STOP\n':
                raise RuntimeError('filesystem replay control ended unexpectedly')
        finally:
            server.finish()


if __name__ == '__main__':
    if len(sys.argv) != 8:
        raise SystemExit('usage: asb_fs_observed_replay.py WORKSPACE RUN_DIR PROXY BINDING LIBRARY EVIDENCE SOCKET')
    run(*sys.argv[1:], sys.stdin)
