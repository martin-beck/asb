"""Experimental guest-owned snapshot supervisor for a disposable task container."""
import argparse
from pathlib import Path
import subprocess
import sys
import threading

from asb_snapshot_service import MarkerServer, SnapshotService, export_snapshot_streams
from asb_snapshot_workspace import SnapshotWorkspace


def main(argv=None):
    parser=argparse.ArgumentParser(description=__doc__)
    for name in ('target','root','socket','output','btrfs','mkfs','rsync'):
        parser.add_argument('--'+name,required=True)
    parser.add_argument('--size',type=int,required=True)
    parser.add_argument('--apply-final',action='store_true')
    args=parser.parse_args(argv)
    root=Path(args.root);root.mkdir(parents=True,exist_ok=False)
    workspace=SnapshotWorkspace(root/'workspace.img',root/'filesystem',args.size,
                                [args.mkfs],[args.btrfs],[args.rsync])
    bound=False;server=None;worker=None;service=None
    try:
        mounted=workspace.prepare(args.target)
        subprocess.run(['mount','--bind',str(mounted),args.target],check=True,timeout=60)
        bound=True
        service=SnapshotService(mounted,root/'filesystem/snapshots',[args.btrfs])
        server=MarkerServer(args.socket,service)
        worker=threading.Thread(target=server.serve_forever,daemon=True);worker.start()
        print('READY',flush=True)
        if sys.stdin.readline()!='STOP\n':
            service.invalidate()
            raise RuntimeError('snapshot supervisor control ended unexpectedly')
        server.shutdown();server.server_close();worker.join();server=None
        Path(args.socket).unlink(missing_ok=True)
        service.finish();service=None
        export_snapshot_streams(root/'filesystem/snapshots',args.output,[args.btrfs])
        print('SAVED',flush=True)
    finally:
        if server is not None:
            server.shutdown();server.server_close();worker.join()
            Path(args.socket).unlink(missing_ok=True)
        if service is not None:
            service.invalidate()
            try:service.finish()
            except RuntimeError:pass
        if bound:
            subprocess.run(['umount',args.target],check=True,timeout=60)
        try:
            if bound and args.apply_final:
                subprocess.run([args.rsync,'-aHAX','--inplace','--checksum','--delete',
                                str(workspace.mountpoint/'workspace')+'/',args.target+'/'],
                               check=True,timeout=300)
        finally:
            workspace.close()


if __name__=='__main__':main()
