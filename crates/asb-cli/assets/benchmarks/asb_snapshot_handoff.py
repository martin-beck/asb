"""Experimental two-phase snapshot preparation and separate custody.

The controller owns both processes and must establish their common mount
namespace, process identities, and teardown order before admitting an agent.
This module does not independently establish those ownership facts.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import threading

from asb_snapshot_guest import receive_archive, unmount_when_idle
from asb_snapshot_service import (MarkerServer, SnapshotApplier, SnapshotReplay,
                                  SnapshotService, export_snapshot_streams,
                                  load_replay_boundaries)
from asb_snapshot_workspace import SnapshotWorkspace, replay_roots


def prepare(args):
    root = Path(args.root)
    root.mkdir(parents=True, exist_ok=False)
    workspace = SnapshotWorkspace(root/'workspace.img', root/'filesystem', args.size,
                                  [args.mkfs], [args.btrfs], [args.rsync])
    bound = []
    apply_final = False
    try:
        roots = [args.target] + args.extra_root
        mounted = workspace.prepare(args.target, args.extra_root, seed=not bool(args.replay_archive))
        if args.replay_archive:
            store = root/'filesystem/snapshots'
            receive_archive(args.replay_archive, store, [args.btrfs])
            baseline = store/'snapshot-00000001'
            roots = replay_roots(baseline, args.target, args.extra_root)
            mounted = workspace.restore_baseline(baseline)
        for index, target in enumerate(roots):
            source = mounted/str(index) if len(roots) > 1 else mounted
            subprocess.run(['mount', '--bind', str(source), target], check=True, timeout=60)
            bound.append((source, target))
        print('PREPARED', flush=True)
        if sys.stdin.readline() != 'CLEANUP\n':
            raise RuntimeError('snapshot preparer control ended unexpectedly')
        apply_final = args.apply_final
    finally:
        try:
            for source, target in reversed(bound):
                unmount_when_idle(target)
                if apply_final:
                    subprocess.run([args.rsync, '-aHAX', '--inplace', '--checksum', '--delete',
                                    str(source)+'/', target+'/'], check=True, timeout=300)
        finally:
            workspace.close()
    print('CLEANED', flush=True)


def custody(args):
    root = Path(args.root)/'filesystem'
    mounted, store = root/'workspace', root/'snapshots'
    if args.verify_mount:
        # The task's procfs names its init as PID 1 even though this
        # custodian is in a separate PID namespace. The controller compares
        # this inode with the task's pinned mount namespace before START.
        print('MOUNT:'+str(os.stat('/proc/1/ns/mnt').st_ino), flush=True)
        if sys.stdin.readline() != 'START\n':
            raise RuntimeError('snapshot custody identity was not accepted')
    if args.replay_archive:
        service = SnapshotReplay(load_replay_boundaries(store), SnapshotApplier(store, mounted, [args.rsync]))
    else:
        service = SnapshotService(mounted, store, [args.btrfs])
    server = MarkerServer(args.socket, service)
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    settled = False
    try:
        print('READY', flush=True)
        action = sys.stdin.readline()
        server.shutdown()
        try:
            if action == 'FORENSIC\n':
                service.invalidate()
                try:
                    service.finish()
                except RuntimeError:
                    pass
                forensic = root/'forensic-interrupted'
                subprocess.run([args.btrfs, 'subvolume', 'snapshot', '-r', str(mounted), str(forensic)],
                               check=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, timeout=120)
                evidence = Path(args.root).parent/'forensic-interrupted.btrfs'
                with evidence.open('xb') as output:
                    subprocess.run([sys.executable, str(Path(__file__).with_name('asb_snapshot_send.py')),
                                    str(forensic)], check=True, stdin=subprocess.DEVNULL, stdout=output,
                                   stderr=subprocess.DEVNULL, timeout=300)
                    output.flush()
                    os.fsync(output.fileno())
                metadata = Path(args.root).parent/'forensic-interrupted.json'
                with metadata.open('x', encoding='utf-8') as output:
                    json.dump({'schema_version': 1, 'complete': False,
                               'scope': 'interrupted-forensic-snapshot'}, output)
                    output.flush()
                    os.fsync(output.fileno())
                settled = True
            elif action != 'STOP\n':
                service.invalidate()
                raise RuntimeError('snapshot custodian stopped without complete settlement')
            else:
                service.finish()
                settled = True
        finally:
            server.interrupt_clients()
            server.server_close()
            worker.join()
            Path(args.socket).unlink(missing_ok=True)
        if action == 'STOP\n' and not args.replay_archive:
            export_snapshot_streams(store, args.output, [args.btrfs], direct_send=True)
        print('FORENSIC_SAVED_INCOMPLETE' if action == 'FORENSIC\n' else 'SAVED', flush=True)
    finally:
        if not settled:
            service.invalidate()
            server.shutdown()
            server.interrupt_clients()
            server.server_close()
            worker.join()
            Path(args.socket).unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--role', choices=('prepare', 'custody'), required=True)
    for name in ('target', 'root', 'socket', 'output', 'btrfs', 'mkfs', 'rsync'):
        parser.add_argument('--'+name, required=True)
    parser.add_argument('--size', type=int)
    parser.add_argument('--apply-final', action='store_true')
    parser.add_argument('--verify-mount', action='store_true')
    parser.add_argument('--replay-archive')
    parser.add_argument('--extra-root', action='append', default=[])
    args = parser.parse_args()
    (prepare if args.role == 'prepare' else custody)(args)


if __name__ == '__main__':
    main()
