"""Opt-in Pi Bash and real Btrfs supervisor: one record, four offline replays.

Run in a disposable VM with passwordless sudo and the tools named below.
The provider is synthetic; this is not a full ASB cassette or SWE-bench run.
"""
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile

HERE = Path(__file__).resolve().parent
PROJECT = HERE.parents[2]
SUPERVISOR = PROJECT/'assets/benchmarks/asb_snapshot_guest.py'


def supervise(root, label, phase, uid, gid, node, bundle, bash):
    base = root/label
    target, temporary = root/'workspace', root/'temporary'
    subprocess.run(['mount','--bind',str(base/'workspace'),str(target)],check=True)
    subprocess.run(['mount','--bind',str(base/'temporary'),str(temporary)],check=True)
    subprocess.run(['ip','link','set','lo','up'],check=True)
    args=[sys.executable,str(SUPERVISOR),'--target',str(target),'--extra-root',str(temporary),
          '--root',str(base/'storage'),'--socket',str(base/'marker'),'--output',str(base/'streams'),
          '--btrfs','/usr/bin/btrfs','--mkfs','/usr/sbin/mkfs.btrfs','--rsync','/usr/bin/rsync',
          '--qemu-img','/usr/bin/qemu-img','--qemu-storage','/usr/bin/qemu-storage-daemon',
          '--size',str(512*1024*1024),'--apply-final']
    if phase=='replay':args+=['--replay-archive',str(root/'snapshots.tar')]
    with (base/'supervisor.log').open('wb') as log:
        service=subprocess.Popen(args,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=log,text=True)
    try:
        if service.stdout.readline()!='READY\n':
            service.wait(timeout=30)
            raise RuntimeError(f'snapshot supervisor failed; inspect {base}/supervisor.log')
        env={'PATH':str(Path(node).parent)+':/usr/bin:/bin','HOME':str(root),
             'ASB_TEST_PI_BUNDLE':bundle,'ASB_TEST_PI_BASH':bash,'PI_FIXTURE_ROOT':str(root),
             'PI_FIXTURE_WORKSPACE':str(target),'PI_FIXTURE_TEMP':str(temporary),
             'PI_FIXTURE_MARKER':str(base/'marker'),'PI_FIXTURE_PRIVATE':str(base),
             'PI_FIXTURE_PHASE':phase}
        child=subprocess.run(['setpriv','--reuid',uid,'--regid',gid,'--init-groups',node,str(HERE/'snapshot_http.mjs')],
                             env=env,text=True,capture_output=True,timeout=180)
        if child.returncode:raise RuntimeError(f'Pi fixture failed: {child.stderr[-2000:]}')
        output,_=service.communicate('STOP\n',timeout=180)
        if service.returncode or 'SAVED\n' not in output:raise RuntimeError(f'snapshot settlement failed; inspect {base}/supervisor.log')
        if phase=='record':
            with tarfile.open(root/'snapshots.tar','x') as archive:
                for path in sorted((base/'streams').iterdir()):archive.add(path,arcname=path.name,recursive=False)
        print(child.stdout.strip(),flush=True)
    finally:
        if service.poll() is None:service.communicate('STOP\n',timeout=180)


def main():
    if len(sys.argv)>1 and sys.argv[1]=='--supervisor':
        _,_,root,label,phase,uid,gid,node,bundle,bash=sys.argv
        supervise(Path(root),label,phase,uid,gid,node,bundle,bash)
        return
    fixture=Path(os.environ['ASB_PI_FIXTURE_DIR']).resolve(strict=True)
    bundle=Path(os.environ['ASB_TEST_PI_BUNDLE']).resolve(strict=True)
    bash=Path(os.environ['ASB_TEST_PI_BASH']).resolve(strict=True)
    node=shutil.which('node')
    if not node or os.getuid()==0:raise SystemExit('Run as an unprivileged user with Node and passwordless sudo')
    uid,gid=str(os.getuid()),str(os.getgid())
    root=Path(tempfile.mkdtemp(prefix='pi-snapshot-c4-',dir=fixture))
    (root/'workspace').mkdir();(root/'temporary').mkdir()
    def run(label,phase):
        base=root/label;base.mkdir();(base/'workspace').mkdir();(base/'temporary').mkdir()
        (base/'workspace'/'fixture.txt').write_text('before')
        result=subprocess.run(['sudo','-n','unshare','--mount','--net','--propagation','private',
                               sys.executable,str(Path(__file__).resolve()),'--supervisor',str(root),label,
                               phase,uid,gid,node,str(bundle),str(bash)],capture_output=True,text=True,timeout=300)
        if result.returncode:raise RuntimeError(f'{label} failed: {result.stderr[-3000:]}')
        if (base/'workspace'/'fixture.txt').read_text()!='after':raise RuntimeError('workspace effect differs')
        if (base/'temporary'/'effect').read_text()!='temp':raise RuntimeError('temporary effect differs')
        return result.stdout.strip().splitlines()[-1]
    try:
        if run('record','record')!='record passed':raise RuntimeError('record result differs')
        with ThreadPoolExecutor(max_workers=4) as pool:
            results=list(pool.map(lambda i:run(f'replay-{i}','replay'),range(4)))
        if results!=['replay passed']*4 or (root/'counter').read_text()!='x':
            raise RuntimeError('replay differed or reran the Bash command')
        print('installed Pi Bash: one HTTP recording, four offline Btrfs-backed replays passed')
    finally:
        print('fixture evidence:',root,flush=True)


if __name__=='__main__':main()
