"""Opt-in real Claude and Btrfs: one recording, four isolated offline replays.

Runs private mount/network namespaces via passwordless sudo; the Claude client
itself runs as the invoking unprivileged user. The loopback provider is a
fixture, not an online LLM or a SWE-bench qualification.
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
GUEST = PROJECT/'assets/benchmarks/asb_snapshot_guest.py'
NODE_SCRIPT = HERE/'edit.mjs'


def supervisor(root, label, phase, uid, gid, node, binary, fixture, delegated):
    base=root/label
    target=root/'workspace'; temporary=root/'temporary'
    subprocess.run(['mount','--bind',str(base/'workspace'),str(target)],check=True)
    subprocess.run(['mount','--bind',str(base/'temporary'),str(temporary)],check=True)
    if delegated:
        subprocess.run(['mount','--bind',str(base/'config'),str(root/'config')],check=True)
    subprocess.run(['ip','link','set','lo','up'],check=True)
    args=[sys.executable,str(GUEST),'--target',str(target),'--extra-root',str(temporary),
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
        if phase=='replay' and ((base/'storage/streams').exists() or not (root/'snapshots.tar').is_file()):
            raise RuntimeError('replay did not release extracted streams while preserving its archive')
        env=dict(os.environ,ASB_CLAUDE_FIXTURE_DIR=str(fixture),ASB_CLAUDE_BINARY=str(binary),
                 ASB_CLAUDE_FIXED_WORKSPACE=str(target),ASB_CLAUDE_PHASE=phase,
                 ASB_CLAUDE_EVENTS=str(root/'cassette.json'),ASB_CLAUDE_EXTERNAL_MARKER=str(base/'marker'),
                 ASB_CLAUDE_TEST_TMP=str(temporary),ASB_CLAUDE_TEST_COUNTER=str(root/'counter'))
        if delegated:
            env.update(ASB_CLAUDE_FIXED_CONFIG=str(root/'config'),ASB_CLAUDE_STRICT_TAPE='1')
        script=HERE/'delegate.mjs' if delegated else NODE_SCRIPT
        child=subprocess.run(['setpriv','--reuid',uid,'--regid',gid,'--init-groups',node,str(script)],
                             env=env,text=True,capture_output=True,timeout=180)
        if child.returncode:raise RuntimeError(f'Claude fixture failed: {child.stderr[-2000:]}')
        output,_=service.communicate('STOP\n',timeout=180)
        if service.returncode or 'SAVED\n' not in output:raise RuntimeError('snapshot settlement failed')
        if phase=='record':
            with tarfile.open(root/'snapshots.tar','x') as archive:
                for path in sorted((base/'streams').iterdir()):archive.add(path,arcname=path.name,recursive=False)
        print(child.stdout.strip())
    finally:
        if service.poll() is None:
            service.communicate('STOP\n',timeout=180)


def main():
    if len(sys.argv)>1 and sys.argv[1]=='--supervisor':
        _,_,root,label,phase,uid,gid,node,binary,fixture,delegated=sys.argv
        supervisor(Path(root),label,phase,uid,gid,node,binary,fixture,delegated=='1')
        return
    fixture=Path(os.environ['ASB_CLAUDE_FIXTURE_DIR']).resolve()
    binary=Path(os.environ['ASB_CLAUDE_BINARY']).resolve()
    node=shutil.which('node')
    if not node or os.getuid()==0:raise SystemExit('Run as an unprivileged user with node and passwordless sudo')
    uid,gid=str(os.getuid()),str(os.getgid())
    delegated=os.environ.get('ASB_CLAUDE_TEST_DELEGATE')=='1'
    root=Path(tempfile.mkdtemp(prefix='snapshot-c4-',dir=fixture))
    (root/'workspace').mkdir();(root/'temporary').mkdir();(root/'config').mkdir()
    def run(label,phase):
        base=root/label;base.mkdir();(base/'workspace').mkdir();(base/'temporary').mkdir();(base/'config').mkdir()
        (base/'workspace'/'fixture.txt').write_text('fixture child content\n' if delegated else 'before\n')
        result=subprocess.run(['sudo','-n','unshare','--mount','--net','--propagation','private',
                               sys.executable,str(Path(__file__).resolve()),'--supervisor',str(root),label,
                               phase,uid,gid,node,str(binary),str(fixture),'1' if delegated else '0'],
                              capture_output=True,text=True,timeout=300)
        if result.returncode:raise RuntimeError(f'{label} failed: {result.stderr[-3000:]}')
        expected='fixture child content\n' if delegated else 'after\n'
        if (base/'workspace'/'fixture.txt').read_text()!=expected:raise RuntimeError('workspace result mismatch')
        if not delegated and (base/'temporary'/'shell-output').read_text()!='fixture':raise RuntimeError('temporary result mismatch')
        return result.stdout.strip()
    try:
        print(run('record','record'),flush=True)
        with ThreadPoolExecutor(max_workers=4) as pool:
            results=list(pool.map(lambda i:run(f'replay-{i}','replay'),range(4)))
        if results!=['replay passed']*4 or (not delegated and (root/'counter').read_text()!='x'):
            raise RuntimeError('replay differed or reran the shell command')
        print('one recording, four offline Btrfs-backed Claude replays passed')
    finally:
        # Btrfs images and logs are owned by the private supervisor. Retain
        # them for inspection rather than deleting root-owned evidence here.
        print('fixture evidence:',root)


if __name__=='__main__':main()
