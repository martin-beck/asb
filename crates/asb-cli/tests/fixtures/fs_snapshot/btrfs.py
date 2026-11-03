"""Opt-in offline Btrfs snapshot and live synchronization probe."""
import os
import re
import stat
import uuid
from pathlib import Path
import shutil
import subprocess
import tempfile

project=Path(__file__).resolve().parents[5]
root=project/'target'/'snapshot-fixtures'
root.mkdir(parents=True,exist_ok=True)
mkfs=Path(os.environ['ASB_SNAPSHOT_MKFS_BTRFS']).resolve(strict=True)
btrfs=Path(os.environ['ASB_SNAPSHOT_BTRFS']).resolve(strict=True)
rsync=Path(os.environ['ASB_SNAPSHOT_RSYNC']).resolve(strict=True)
libraries=Path(os.environ['ASB_SNAPSHOT_LIBRARY_DIR']).resolve(strict=True)
loader=os.environ['ASB_SNAPSHOT_LOADER']
loop=Path(os.environ['ASB_SNAPSHOT_LOOP_DEVICE']).resolve(strict=True)
image=os.environ['ASB_SNAPSHOT_IMAGE']
if (not re.fullmatch(r'[A-Za-z0-9_.+-]+',loader) or not (libraries/loader).is_file()
        or loop.parent != Path('/dev') or not re.fullmatch(r'loop[0-9]+',loop.name)
        or not stat.S_ISBLK(loop.stat().st_mode)):
 raise ValueError('snapshot fixture requires a loader name and loop device')
d=Path(tempfile.mkdtemp(dir=root,prefix='btrfs-probe-'))
success=False
container_name='asb-btrfs-fixture-'+uuid.uuid4().hex
try:
 script='''set -eu
truncate -s 512M /work/disk.img
/hostlib/LOADER --library-path /hostlib /opt/mkfs.btrfs -q /work/disk.img
losetup /dev/LOOPDEV /work/disk.img
cleanup(){ umount /work/fs || true; losetup -d /dev/LOOPDEV || true; chmod -R a+rwX /work; }
trap cleanup EXIT
mkdir /work/fs
mount -t btrfs /dev/LOOPDEV /work/fs
btrfs_cmd(){ /hostlib/LOADER --library-path /hostlib /opt/btrfs "$@"; }
btrfs_cmd subvolume create /work/fs/base >/dev/null
printf old > /work/fs/base/file
printf remove > /work/fs/base/deleted
ln /work/fs/base/file /work/fs/base/link
for i in 0 1 2 3 4; do btrfs_cmd subvolume snapshot /work/fs/base /work/fs/s$i >/dev/null; done
python3 - <<'EOPY'
import errno,os,subprocess,concurrent.futures
from pathlib import Path
roots=[Path('/work/fs/s'+str(i)) for i in range(5)]
st=[(r/'file').stat() for r in roots]
checks={
 'initial_inode_equal':len({x.st_ino for x in st})==1,
 'initial_mtime_equal':len({x.st_mtime_ns for x in st})==1,
}
print('initial_device_equal',len({x.st_dev for x in st})==1)
fds=[os.open(r/'file',os.O_RDONLY) for r in roots]
subprocess.run(['python3','-c','from pathlib import Path; Path(\"file\").write_bytes(b\"new\"); Path(\"deleted\").unlink(); Path(\"/work/invocations\").open(\"ab\").write(b\"1\")'],cwd=roots[0],check=True)
checks['record_hardlink_correct']=(roots[0]/'link').read_bytes()==b'new'
checks['record_open_descriptor_correct']=os.pread(fds[0],3,0)==b'new'
checks['other_replicas_unchanged']=all((r/'file').read_bytes()==b'old' and (r/'link').read_bytes()==b'old' and (r/'deleted').exists() for r in roots[1:])
checks['base_unchanged']=Path('/work/fs/base/file').read_bytes()==b'old'
(roots[0]/'binary').write_bytes(bytes(range(256)));os.symlink('binary',roots[0]/'symlink')
os.chmod(roots[0]/'file',0o640)
xattr=True
try:os.setxattr(roots[0]/'file','user.asb-fixture',b'value')
except OSError:xattr=False
def capture(name):
 path=Path('/work/fs')/name
 subprocess.run(['/hostlib/LOADER','--library-path','/hostlib','/opt/btrfs','subvolume','snapshot','-r',str(roots[0]),str(path)],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,timeout=30,check=True)
 return path
first=capture('captured-1')
try:
 (first/'should-not-write').write_bytes(b'bad')
 checks['capture_is_read_only']=False
except OSError as error:
 checks['capture_is_read_only']=error.errno==errno.EROFS
def sync(source,root):
 return subprocess.run(['/hostlib/LOADER','--library-path','/hostlib','/opt/rsync','-aHAX','--inplace','--checksum','--delete',str(source)+'/',str(root)+'/'],stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=60)
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:results=list(pool.map(lambda root:sync(first,root),roots[1:]))
if any(x.returncode for x in results):raise RuntimeError('rsync failed: '+next(x.stderr.decode(errors='replace')[-300:] for x in results if x.returncode))
checks['replay_contents_correct']=all((r/'file').read_bytes()==b'new' and (r/'link').read_bytes()==b'new' for r in roots[1:])
checks['replay_hardlinks_correct']=all((r/'file').stat().st_ino==(r/'link').stat().st_ino for r in roots[1:])
checks['replay_open_descriptors_correct']=all(os.pread(fd,3,0)==b'new' for fd in fds[1:])
checks['replay_binary_symlink_correct']=all((r/'binary').read_bytes()==bytes(range(256)) and os.readlink(r/'symlink')=='binary' for r in roots[1:])
checks['replay_deletion_correct']=all(not (r/'deleted').exists() and not (r/'deleted').is_symlink() for r in roots[1:])
checks['replay_mode_correct']=all((r/'file').stat().st_mode & 0o777==0o640 for r in roots[1:])
checks['base_still_unchanged']=Path('/work/fs/base/file').read_bytes()==b'old' and Path('/work/fs/base/deleted').exists() and not Path('/work/fs/base/binary').exists()
if xattr:checks['replay_xattr_correct']=all(os.getxattr(r/'file','user.asb-fixture')==b'value' for r in roots[1:])
subprocess.run(['python3','-c','from pathlib import Path; Path(\"file\").write_bytes(b\"end\"); Path(\"symlink\").unlink(); Path(\"/work/invocations\").open(\"ab\").write(b\"2\")'],cwd=roots[0],check=True)
second=capture('captured-2')
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:results=list(pool.map(lambda root:sync(second,root),roots[1:]))
if any(x.returncode for x in results):raise RuntimeError('second rsync failed: '+next(x.stderr.decode(errors='replace')[-300:] for x in results if x.returncode))
checks['second_boundary_contents_correct']=all((r/'file').read_bytes()==b'end' and (r/'link').read_bytes()==b'end' and not (r/'symlink').exists() and not (r/'symlink').is_symlink() for r in roots[1:])
checks['second_boundary_open_descriptors_correct']=all(os.pread(fd,3,0)==b'end' for fd in fds[1:])
checks['second_boundary_hardlinks_correct']=all((r/'file').stat().st_ino==(r/'link').stat().st_ino for r in roots[1:])
checks['base_unchanged_after_second_boundary']=(Path('/work/fs/base/file').read_bytes()==b'old' and Path('/work/fs/base/link').read_bytes()==b'old' and Path('/work/fs/base/file').stat().st_ino==Path('/work/fs/base/link').stat().st_ino)
checks['first_capture_unchanged']=(first/'file').read_bytes()==b'new' and (first/'symlink').is_symlink()
link_fds=[os.open(root/'link',os.O_RDONLY) for root in roots[1:]]
subprocess.run(['python3','-c','from pathlib import Path; Path(\"link\").unlink(); Path(\"link\").write_bytes(b\"sep\"); Path(\"/work/invocations\").open(\"ab\").write(b\"3\")'],cwd=roots[0],check=True)
third=capture('captured-3')
control=Path('/work/fs/control')
subprocess.run(['/hostlib/LOADER','--library-path','/hostlib','/opt/btrfs','subvolume','snapshot',str(second),str(control)],stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,timeout=30,check=True)
naive=sync(third,control)
if naive.returncode:raise RuntimeError('control rsync failed: '+naive.stderr.decode(errors='replace')[-300:])
naive_correct=(control/'file').read_bytes()==b'end' and (control/'link').read_bytes()==b'sep' and (control/'file').stat().st_ino!=(control/'link').stat().st_ino
print('naive_split_sync_correct',naive_correct)
if ((second/'file').stat().st_ino!=(second/'link').stat().st_ino
        or (third/'file').stat().st_ino!=(second/'file').stat().st_ino
        or (third/'link').stat().st_ino==(second/'link').stat().st_ino):
 raise RuntimeError('fixture did not produce the expected hard-link split')
for root in roots[1:]:
 if (root/'file').stat().st_ino!=(root/'link').stat().st_ino:
  raise RuntimeError('replica hard-link state differs before split')
 (root/'link').unlink()
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:results=list(pool.map(lambda root:sync(third,root),roots[1:]))
if any(x.returncode for x in results):raise RuntimeError('third rsync failed: '+next(x.stderr.decode(errors='replace')[-300:] for x in results if x.returncode))
checks['split_link_correct']=all((r/'file').read_bytes()==b'end' and (r/'link').read_bytes()==b'sep' and (r/'file').stat().st_ino!=(r/'link').stat().st_ino for r in roots[1:])
checks['split_link_existing_descriptors_correct']=all(os.pread(fd,3,0)==b'end' for fd in fds[1:]+link_fds)
checks['original_subprocess_only_three_times']=Path('/work/invocations').read_bytes()==b'123'
for fd in link_fds:os.close(fd)
for name,passed in checks.items():print(name,passed)
print('xattr_exercised',xattr)
if not all(checks.values()):raise RuntimeError('Btrfs snapshot checks failed')
for fd in fds:os.close(fd)
EOPY
'''.replace('LOADER',loader).replace('LOOPDEV',loop.name)
 cmd=['docker','run','--rm','--name',container_name,'--pull=never','--network','none','--device','/dev/loop-control','--device',str(loop),'--cap-add','SYS_ADMIN','--security-opt','apparmor=unconfined','--mount',f'type=bind,src={d},dst=/work','--mount',f'type=bind,src={mkfs},dst=/opt/mkfs.btrfs,readonly','--mount',f'type=bind,src={btrfs},dst=/opt/btrfs,readonly','--mount',f'type=bind,src={rsync},dst=/opt/rsync,readonly','--mount',f'type=bind,src={libraries},dst=/hostlib,readonly','--entrypoint','/bin/sh',image,'-c',script]
 try:
  r=subprocess.run(cmd,text=True,capture_output=True,timeout=90)
 except subprocess.TimeoutExpired:
  subprocess.run(['docker','stop','-t','5',container_name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=20,check=False)
  raise RuntimeError(f'fixture timed out; retained diagnostics: {d}')
 print(r.stdout,end='')
 if r.returncode:raise RuntimeError(f'fixture exit {r.returncode}; retained diagnostics: {d}; {r.stderr[-800:]}')
 backing=Path('/sys/class/block')/loop.name/'loop/backing_file'
 if backing.exists() and str(d/'disk.img') in backing.read_text(errors='replace'):
  raise RuntimeError(f'fixture loop device remains attached; retained diagnostics: {d}')
 success=True
finally:
 if success:shutil.rmtree(d)
