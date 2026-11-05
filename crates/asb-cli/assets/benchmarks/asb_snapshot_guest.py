"""Experimental guest-owned snapshot supervisor for a disposable task container."""
import argparse
import json
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
                                  load_replay_boundaries)
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


def receive_archive(archive, destination, command):
    """Receive snapshot members directly, retaining the verified input archive."""
    destination = Path(destination)
    with tarfile.open(archive, 'r:*') as source:
        members = source.getmembers()
        names = [member.name for member in members]
        if (len(names) != len(set(names)) or any(not member.isfile() for member in members)
                or any(not (name in ('boundaries.jsonl', 'markers.jsonl') or
                            re.fullmatch(r'snapshot-[0-9]{8}\.btrfs', name)) for name in names)):
            raise ValueError('snapshot archive contains an unexpected entry')
        if not {'boundaries.jsonl', 'markers.jsonl'}.issubset(names):
            raise ValueError('snapshot archive is missing its journals')
        journals = {}
        for name in ('boundaries.jsonl', 'markers.jsonl'):
            with source.extractfile(name) as stream:
                journals[name] = stream.read()
        entries = [json.loads(line) for line in journals['boundaries.jsonl'].splitlines()]
        if len(entries) < 2:
            raise ValueError('snapshot stream set is incomplete')
        expected = []
        for index, entry in enumerate(entries, 1):
            name = f'snapshot-{index:08d}'
            if not isinstance(entry, dict) or entry.get('sequence') != index or entry.get('snapshot') != name:
                raise ValueError('snapshot stream set is malformed')
            expected.append(name+'.btrfs')
        if set(names) != set(expected) | set(journals):
            raise ValueError('snapshot stream set does not match its journal')
        destination.mkdir(parents=True, exist_ok=False)
        for name in expected:
            process = subprocess.Popen(list(command)+['receive',str(destination)], stdin=subprocess.PIPE,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            failures = []
            def feed():
                try:
                    with source.extractfile(name) as stream:
                        shutil.copyfileobj(stream, process.stdin, length=1024*1024)
                except (OSError, tarfile.TarError) as error:
                    failures.append(error)
                finally:
                    try: process.stdin.close()
                    except OSError: pass
            worker = threading.Thread(target=feed, daemon=True)
            worker.start()
            try:
                code = process.wait(timeout=300)
            except BaseException:
                process.kill(); process.wait(); worker.join(timeout=10)
                raise
            worker.join(timeout=10)
            if worker.is_alive() or failures or code:
                raise RuntimeError('snapshot stream receive failed; retained partial output')
        for name, content in journals.items():
            with (destination/name).open('xb') as output:
                output.write(content)
        return load_replay_boundaries(destination)


def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('target','root','socket','output','btrfs','mkfs','rsync'):
        parser.add_argument('--'+name,required=True)
    parser.add_argument('--size',type=int)
    parser.add_argument('--qemu-img')
    parser.add_argument('--qemu-storage')
    parser.add_argument('--apply-final',action='store_true')
    parser.add_argument('--replay-archive')
    parser.add_argument('--extra-root', action='append', default=[])
    args=parser.parse_args(argv)
    if bool(args.qemu_img) != bool(args.qemu_storage):
        parser.error('qemu-img and qemu-storage must be supplied together')
    root=Path(args.root);root.mkdir(parents=True,exist_ok=False)
    workspace=SnapshotWorkspace(root/'workspace.img',root/'filesystem',args.size,
                                [args.mkfs],[args.btrfs],[args.rsync],
                                [args.qemu_img] if args.qemu_img else None,
                                [args.qemu_storage] if args.qemu_storage else None)
    bound=[];server=None;worker=None;service=None
    try:
        if args.replay_archive:
            mounted=workspace.prepare(args.target,args.extra_root,seed=False)
        else:
            mounted=(workspace.prepare(args.target,args.extra_root) if args.extra_root
                     else workspace.prepare(args.target))
        store = root/'filesystem/snapshots'
        if args.replay_archive:
            boundaries = receive_archive(args.replay_archive, store, [args.btrfs])
            if args.extra_root:
                baseline = store/'snapshot-00000001'
                try:
                    recorded = json.loads((baseline/'.asb-snapshot-roots.json').read_text(encoding='utf-8'))
                except (OSError, ValueError) as error:
                    raise ValueError('snapshot archive does not describe its captured roots') from error
                if (recorded != [str(Path(item).resolve()) for item in [args.target]+args.extra_root]
                        or not all((baseline/str(i)).is_dir() for i in range(len(recorded)))):
                    raise ValueError('snapshot archive does not match the configured roots')
            mounted=workspace.restore_baseline(store/'snapshot-00000001')
            service = SnapshotReplay(boundaries, SnapshotApplier(store, mounted, [args.rsync]))
        roots = [args.target] + args.extra_root
        for index, target in enumerate(roots):
            source = mounted/str(index) if args.extra_root else mounted
            subprocess.run(['mount','--bind',str(source),target],check=True,timeout=60)
            bound.append((source,target))
        if not args.replay_archive:
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
        try:
            for source,target in reversed(bound):
                unmount_when_idle(target)
                if args.apply_final:
                    subprocess.run([args.rsync,'-aHAX','--inplace','--checksum','--delete',
                                    str(source)+'/',target+'/'],check=True,timeout=300)
        finally:
            workspace.close()


if __name__=='__main__':main()
