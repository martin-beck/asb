"""Experimental guest-owned snapshot supervisor for a disposable task container."""
import argparse
from pathlib import Path
import subprocess
import sys
import threading
import re
import shutil
import tarfile
import os
import time

from asb_snapshot_service import (MarkerServer, SnapshotService, SnapshotReplay,
                                  SnapshotApplier, export_snapshot_streams,
                                  receive_snapshot_streams)
from asb_snapshot_workspace import SnapshotWorkspace


def unmount_when_idle(target, timeout=25):
    """Wait for short-lived readers; never force or lazily detach the mount."""
    deadline = time.monotonic() + timeout
    while True:
        try:
            subprocess.run(['umount', target], check=True, timeout=60,
                           capture_output=True, text=True, env=dict(os.environ, LC_ALL='C'))
            return
        except subprocess.CalledProcessError as error:
            remaining = deadline - time.monotonic()
            if 'target is busy' not in (error.stderr or '') or remaining <= 0:
                if error.stderr:
                    print(error.stderr, end='', file=sys.stderr)
                raise
            time.sleep(min(0.2, remaining))


def unpack_streams(archive, destination):
    """Stream the flat snapshot archive into a fresh private directory."""
    destination = Path(destination)
    destination.mkdir(parents=True, exist_ok=False)
    names = set()
    with tarfile.open(archive, 'r|*') as source:
        for member in source:
            name = member.name
            if (not member.isfile() or name in names or
                    not (name in ('boundaries.jsonl', 'markers.jsonl') or
                         re.fullmatch(r'snapshot-[0-9]{8}\.btrfs', name))):
                raise ValueError('snapshot archive contains an unexpected entry')
            names.add(name)
            content = source.extractfile(member)
            if content is None:
                raise ValueError('snapshot archive entry is unreadable')
            with content, (destination/name).open('xb') as output:
                shutil.copyfileobj(content, output, length=1024*1024)
    if not {'boundaries.jsonl', 'markers.jsonl'}.issubset(names):
        raise ValueError('snapshot archive is missing its journals')


def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('target','root','socket','output','btrfs','mkfs','rsync'):
        parser.add_argument('--'+name,required=True)
    parser.add_argument('--size',type=int)
    parser.add_argument('--qemu-img')
    parser.add_argument('--qemu-storage')
    parser.add_argument('--apply-final',action='store_true')
    parser.add_argument('--replay-archive')
    args=parser.parse_args(argv)
    if bool(args.qemu_img) != bool(args.qemu_storage):
        parser.error('qemu-img and qemu-storage must be supplied together')
    root=Path(args.root);root.mkdir(parents=True,exist_ok=False)
    workspace=SnapshotWorkspace(root/'workspace.img',root/'filesystem',args.size,
                                [args.mkfs],[args.btrfs],[args.rsync],
                                [args.qemu_img] if args.qemu_img else None,
                                [args.qemu_storage] if args.qemu_storage else None)
    bound=False;server=None;worker=None;service=None
    try:
        mounted=workspace.prepare(args.target)
        subprocess.run(['mount','--bind',str(mounted),args.target],check=True,timeout=60)
        bound=True
        store = root/'filesystem/snapshots'
        if args.replay_archive:
            unpack_streams(args.replay_archive, root/'streams')
            boundaries = receive_snapshot_streams(root/'streams', store, [args.btrfs])
            subprocess.run([args.rsync, '-aHAX', '--inplace', '--checksum', '--delete',
                            str(store/'snapshot-00000001')+'/', str(mounted)+'/'],
                           check=True, timeout=300)
            service = SnapshotReplay(boundaries, SnapshotApplier(store, mounted, [args.rsync]))
        else:
            service=SnapshotService(mounted,store,[args.btrfs])
        server=MarkerServer(args.socket,service)
        worker=threading.Thread(target=server.serve_forever,daemon=True);worker.start()
        print('READY',flush=True)
        if sys.stdin.readline()!='STOP\n':
            service.invalidate()
            raise RuntimeError('snapshot supervisor control ended unexpectedly')
        server.shutdown()
        try:
            # Closing the service wakes replay markers whose predecessor never
            # arrived. Joining server threads first would otherwise deadlock.
            service.finish()
        finally:
            server.interrupt_clients()
            server.server_close();worker.join();server=None
            Path(args.socket).unlink(missing_ok=True)
        service=None
        if not args.replay_archive:
            export_snapshot_streams(store,args.output,[args.btrfs])
        print('SAVED',flush=True)
    finally:
        if server is not None:
            if service is not None:
                service.invalidate()
            server.shutdown();server.interrupt_clients();server.server_close();worker.join()
            Path(args.socket).unlink(missing_ok=True)
        if service is not None:
            service.invalidate()
            try:service.finish()
            except RuntimeError:pass
        if bound:
            unmount_when_idle(args.target)
        try:
            if bound and args.apply_final:
                subprocess.run([args.rsync,'-aHAX','--inplace','--checksum','--delete',
                                str(workspace.mountpoint/'workspace')+'/',args.target+'/'],
                               check=True,timeout=300)
        finally:
            workspace.close()


if __name__=='__main__':main()
