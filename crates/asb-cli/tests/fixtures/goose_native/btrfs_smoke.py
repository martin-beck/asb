import concurrent.futures, io, os, pathlib, socket, subprocess, tarfile
if os.geteuid() != 0 or not os.environ.get('ASB_BTRFS_FIXTURE_DIR'):
    raise SystemExit('Run as root with ASB_BTRFS_FIXTURE_DIR set to a new disposable directory')
root = pathlib.Path(os.environ['ASB_BTRFS_FIXTURE_DIR']).resolve()
root.mkdir(exist_ok=False)
guest = str(pathlib.Path(__file__).resolve().parents[3] / 'assets/benchmarks/asb_snapshot_guest.py')
def start(label, archive=None):
    base = root/label; base.mkdir(); target=base/'target';target.mkdir(); (target/'file').write_text('baseline')
    args=['python3',guest,'--target',str(target),'--root',str(base/'root'),'--socket',str(base/'marker.sock'),'--output',str(base/'streams'),'--btrfs','/usr/bin/btrfs','--mkfs','/usr/sbin/mkfs.btrfs','--rsync','/usr/bin/rsync','--size',str(512*1024*1024),'--apply-final']
    if archive:args += ['--replay-archive',str(archive)]
    p=subprocess.Popen(args,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=(base/'error.log').open('wb'),text=True)
    line=p.stdout.readline()
    if line!='READY\n':
        p.wait(timeout=30);raise RuntimeError(f'{label} failed to start: {line!r} exit {p.returncode}')
    return base,target,p
def marker(path,msg):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(30);s.connect(str(path));s.sendall((msg+'\n').encode());return s.recv(128).decode().strip()
def stop(p):
    out,_=p.communicate('STOP\n',timeout=120)
    if p.returncode or 'SAVED\n' not in out:raise RuntimeError(f'supervisor failed {p.returncode}: {out!r}')
base,target,p=start('record')
try:
    marker(base/'marker.sock','tool-begin:write');(target/'file').write_text('recorded');marker(base/'marker.sock','tool-end:write')
    stop(p)
except BaseException:
    if p.poll() is None:p.communicate('STOP\n',timeout=120)
    raise
archive=root/'snapshots.tar'
with tarfile.open(archive,'x') as tar:
    for path in sorted((base/'streams').iterdir()):tar.add(path,arcname=path.name,recursive=False)
def replay(index):
    base,target,p=start('replay-'+str(index),archive)
    try:
        marker(base/'marker.sock','tool-end:write')
        result=(target/'file').read_text()
        stop(p)
        return result
    except BaseException:
        if p.poll() is None:p.communicate('STOP\n',timeout=120)
        raise
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
    results=list(pool.map(replay,range(1,5)))
assert results==['recorded']*4,results
print('record: success; replay: 4/4 success; matching Btrfs workspace effects:',results)
