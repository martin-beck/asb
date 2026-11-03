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
mkdir /work/fs/base/folder
printf alpha > /work/fs/base/folder/data
ln /work/fs/base/file /work/fs/base/link
for i in 0 1 2 3 4; do btrfs_cmd subvolume snapshot /work/fs/base /work/fs/s$i >/dev/null; done
python3 - <<'EOPY'
import errno,os,socket,subprocess,concurrent.futures,sys,threading,json
from pathlib import Path
sys.path.insert(0,"/opt")
from asb_snapshot_service import SnapshotService,SnapshotReplay,MarkerServer
roots=[Path('/work/fs/s'+str(i)) for i in range(5)]
st=[(r/'file').stat() for r in roots]
checks={
 'initial_inode_equal':len({x.st_ino for x in st})==1,
 'initial_mtime_equal':len({x.st_mtime_ns for x in st})==1,
}
print('initial_device_equal',len({x.st_dev for x in st})==1)
fds=[os.open(r/'file',os.O_RDONLY) for r in roots]
service=SnapshotService(roots[0],Path('/work/fs/recorded'),['/hostlib/LOADER','--library-path','/hostlib','/opt/btrfs'])
socket_path=Path('/work/snapshot.sock');server=MarkerServer(socket_path,service)
server_thread=threading.Thread(target=server.serve_forever,daemon=True);server_thread.start()
def mark(action,alias):
 with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as client:
  client.settimeout(30);client.connect(str(socket_path));client.sendall(f'tool-{action}:{alias}:{os.getpid()}:{threading.get_native_id()}\\n'.encode())
  response=client.makefile('rb').readline()
  if not response.strip().isdigit():raise RuntimeError('snapshot marker failed')
  return int(response)
mark('begin','captured-1')
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
 sequence=mark('end',name)
 return Path('/work/fs/recorded')/f'snapshot-{sequence:08d}'
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
mark('begin','captured-2')
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
mark('begin','captured-3')
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
import sys
sys.path.insert(0,'/opt')
from topology import regular_files,plan_transitions,apply_transitions,directories,plan_tree_transitions,apply_tree_transitions
before_topology=regular_files(second);after_topology=regular_files(third)
for root in roots[1:]:
 plan=plan_transitions(before_topology,after_topology,regular_files(root))
 if plan!={'moves':[],'detach':['link']}:
  raise RuntimeError('snapshot hard-link split plan differs')
 apply_transitions(root,plan)
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:results=list(pool.map(lambda root:sync(third,root),roots[1:]))
if any(x.returncode for x in results):raise RuntimeError('third rsync failed: '+next(x.stderr.decode(errors='replace')[-300:] for x in results if x.returncode))
checks['split_link_correct']=all((r/'file').read_bytes()==b'end' and (r/'link').read_bytes()==b'sep' and (r/'file').stat().st_ino!=(r/'link').stat().st_ino for r in roots[1:])
checks['split_link_existing_descriptors_correct']=all(os.pread(fd,3,0)==b'end' for fd in fds[1:]+link_fds)
split_fds=[os.open(root/'link',os.O_RDONLY) for root in roots[1:]]
mark('begin','captured-4')
subprocess.run(['python3','-c','import os; from pathlib import Path; Path(\"link\").unlink(); os.link(\"file\",\"link\"); Path(\"/work/invocations\").open(\"ab\").write(b\"4\")'],cwd=roots[0],check=True)
fourth=capture('captured-4')
before_topology=regular_files(third);after_topology=regular_files(fourth)
for root in roots[1:]:
 plan=plan_transitions(before_topology,after_topology,regular_files(root))
 if plan!={'moves':[],'detach':['link']}:
  raise RuntimeError('snapshot hard-link join plan differs')
 apply_transitions(root,plan)
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:results=list(pool.map(lambda root:sync(fourth,root),roots[1:]))
if any(x.returncode for x in results):raise RuntimeError('fourth rsync failed: '+next(x.stderr.decode(errors='replace')[-300:] for x in results if x.returncode))
checks['joined_link_correct']=all((r/'file').read_bytes()==b'end' and (r/'link').read_bytes()==b'end' and (r/'file').stat().st_ino==(r/'link').stat().st_ino for r in roots[1:])
checks['joined_old_descriptors_correct']=all(os.pread(fd,3,0)==b'sep' for fd in split_fds)
mark('begin','captured-5')
subprocess.run(['python3','-c','import os; from pathlib import Path; os.rename(\"file\",\"moved-file\"); os.rename(\"link\",\"moved-link\"); Path(\"moved-file\").write_bytes(b\"fin\"); Path(\"/work/invocations\").open(\"ab\").write(b\"5\")'],cwd=roots[0],check=True)
fifth=capture('captured-5')
before_topology=regular_files(fourth);after_topology=regular_files(fifth)
for root in roots[1:]:
 plan=plan_transitions(before_topology,after_topology,regular_files(root))
 if plan['moves']!=[('file','moved-file')]:
  raise RuntimeError('snapshot move plan differs')
 apply_transitions(root,plan)
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:results=list(pool.map(lambda root:sync(fifth,root),roots[1:]))
if any(x.returncode for x in results):raise RuntimeError('fifth rsync failed: '+next(x.stderr.decode(errors='replace')[-300:] for x in results if x.returncode))
checks['moved_link_correct']=all((r/'moved-file').read_bytes()==b'fin' and (r/'moved-link').read_bytes()==b'fin' and (r/'moved-file').stat().st_ino==(r/'moved-link').stat().st_ino and not (r/'file').exists() and not (r/'link').exists() for r in roots[1:])
checks['moved_old_descriptors_correct']=all(os.pread(fd,3,0)==b'fin' for fd in fds[1:]+link_fds) and all(os.pread(fd,3,0)==b'sep' for fd in split_fds)
dir_fds=[os.open(root/'folder',os.O_RDONLY|os.O_DIRECTORY) for root in roots[1:]]
nested_fds=[os.open(root/'folder/data',os.O_RDONLY) for root in roots[1:]]
mark('begin','captured-6')
subprocess.run(['python3','-c','import os; from pathlib import Path; os.rename(\"folder\",\"renamed-folder\"); Path(\"renamed-folder/data\").write_bytes(b\"bravo\"); Path(\"/work/invocations\").open(\"ab\").write(b\"6\")'],cwd=roots[0],check=True)
sixth=capture('captured-6')
for root in roots[1:]:
 plan=plan_tree_transitions(regular_files(fifth),regular_files(sixth),regular_files(root),directories(fifth),directories(sixth),directories(root))
 if plan['directory_moves']!=[('folder','renamed-folder')]:raise RuntimeError('snapshot directory move plan differs')
 apply_tree_transitions(root,plan)
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:results=list(pool.map(lambda root:sync(sixth,root),roots[1:]))
if any(x.returncode for x in results):raise RuntimeError('sixth rsync failed: '+next(x.stderr.decode(errors='replace')[-300:] for x in results if x.returncode))
checks['moved_directory_correct']=all((r/'renamed-folder/data').read_bytes()==b'bravo' and not (r/'folder').exists() for r in roots[1:])
checks['moved_directory_descriptors_correct']=all('data' in os.listdir(fd) for fd in dir_fds) and all(os.pread(fd,5,0)==b'bravo' for fd in nested_fds)
mark('begin','captured-7')
subprocess.run(['python3','-c','import shutil; from pathlib import Path; shutil.rmtree(\"renamed-folder\"); Path(\"renamed-folder\").mkdir(); Path(\"renamed-folder/data\").write_bytes(b\"other\"); Path(\"/work/invocations\").open(\"ab\").write(b\"7\")'],cwd=roots[0],check=True)
seventh=capture('captured-7')
for root in roots[1:]:
 plan=plan_tree_transitions(regular_files(sixth),regular_files(seventh),regular_files(root),directories(sixth),directories(seventh),directories(root))
 if plan['directory_replacements']!=['renamed-folder']:raise RuntimeError('snapshot directory replacement plan differs')
 apply_tree_transitions(root,plan)
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:results=list(pool.map(lambda root:sync(seventh,root),roots[1:]))
if any(x.returncode for x in results):raise RuntimeError('seventh rsync failed: '+next(x.stderr.decode(errors='replace')[-300:] for x in results if x.returncode))
checks['replaced_directory_correct']=all((r/'renamed-folder/data').read_bytes()==b'other' for r in roots[1:])
checks['replaced_directory_old_descriptors_correct']=all(os.pread(fd,5,0)==b'bravo' for fd in nested_fds) and all(os.fstat(fd).st_nlink==0 for fd in dir_fds)
checks['original_subprocess_only_seven_times']=Path('/work/invocations').read_bytes()==b'1234567'
server.shutdown();server.server_close();server_thread.join();socket_path.unlink(missing_ok=True)
checks['final_snapshot_sequence']=service.finish()==9
entries=[json.loads(line) for line in Path('/work/fs/recorded/boundaries.jsonl').read_text().splitlines()]
checks['boundary_journal_correct']=[entry['alias'] for entry in entries]==[None,'captured-1','captured-2','captured-3','captured-4','captured-5','captured-6','captured-7',None]
events=[json.loads(line) for line in Path('/work/fs/recorded/markers.jsonl').read_text().splitlines()]
checks['marker_journal_correct']=(len(events)==14 and
 all(event['sequence']==index+1 and event['kind']==('begin' if index%2==0 else 'end') and
     event['alias']==f'captured-{index//2+1}' and event['process']==os.getpid() and
     event['snapshot_sequence']==index//2+1+(index%2)
     for index,event in enumerate(events)))
adapter_service=SnapshotService(roots[0],Path('/work/fs/adapter-recorded'),['/hostlib/LOADER','--library-path','/hostlib','/opt/btrfs'])
adapter_socket=Path('/work/adapter-record.sock');adapter_server=MarkerServer(adapter_socket,adapter_service)
adapter_thread=threading.Thread(target=adapter_server.serve_forever,daemon=True);adapter_thread.start()
def adapter_client(root,mode,socket_path):
 env=os.environ.copy();env.update({'ASB_MINI_SWE_TOOL_TAPE':'/work/adapter-tape.jsonl',
  'ASB_MINI_SWE_TOOL_MODE':mode,'ASB_FIXTURE_WORKSPACE':str(root),
  'ASB_FS_MARKER_SOCKET' if mode=='record' else 'ASB_FS_REPLAY_SOCKET':str(socket_path)})
 return subprocess.run(['python3','/opt/mini_swe_client.py'],env=env,capture_output=True,timeout=60)
try:
 recorded=adapter_client(roots[0],'record',adapter_socket)
 if recorded.returncode:raise RuntimeError('adapter record failed: '+recorded.stderr.decode()[-1000:])
finally:
 adapter_server.shutdown();adapter_server.server_close();adapter_thread.join();adapter_socket.unlink(missing_ok=True)
adapter_service.finish()
adapter_before=Path('/work/fs/adapter-recorded/snapshot-00000001')
adapter_after=Path('/work/fs/adapter-recorded/snapshot-00000002')
with Path('/work/adapter-baseline.btrfs').open('wb') as output:
 baseline_send=subprocess.run(['/hostlib/LOADER','--library-path','/hostlib','/opt/btrfs','send',str(adapter_before)],stdout=output,stderr=subprocess.PIPE,timeout=60)
with Path('/work/adapter-increment.btrfs').open('wb') as output:
 increment_send=subprocess.run(['/hostlib/LOADER','--library-path','/hostlib','/opt/btrfs','send','-p',str(adapter_before),str(adapter_after)],stdout=output,stderr=subprocess.PIPE,timeout=60)
if baseline_send.returncode or increment_send.returncode:raise RuntimeError('snapshot stream export failed: '+(baseline_send.stderr+increment_send.stderr).decode()[-500:])
checks['snapshot_streams_created']=Path('/work/adapter-baseline.btrfs').stat().st_size>0 and Path('/work/adapter-increment.btrfs').stat().st_size>0
print('snapshot_stream_bytes',Path('/work/adapter-baseline.btrfs').stat().st_size,Path('/work/adapter-increment.btrfs').stat().st_size)
received=Path('/work/fs/received');received.mkdir()
for stream in ('adapter-baseline.btrfs','adapter-increment.btrfs'):
 with (Path('/work')/stream).open('rb') as source:
  result=subprocess.run(['/hostlib/LOADER','--library-path','/hostlib','/opt/btrfs','receive',str(received)],stdin=source,stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=60)
  if result.returncode:raise RuntimeError('snapshot stream receive failed: '+result.stderr.decode()[-500:])
checks['incremental_receive_restored']=(received/'snapshot-00000002/trial-result').read_bytes()==b'fixed'
adapter_before=received/'snapshot-00000001'
adapter_after=received/'snapshot-00000002'
def replay_adapter(index):
 root=roots[index];path=Path(f'/work/adapter-replay-{index}.sock')
 def apply(sequence):
  if sequence!=2:raise RuntimeError('unexpected adapter snapshot')
  plan=plan_tree_transitions(regular_files(adapter_before),regular_files(adapter_after),regular_files(root),directories(adapter_before),directories(adapter_after),directories(root))
  apply_tree_transitions(root,plan)
  result=sync(adapter_after,root)
  if result.returncode:raise RuntimeError('adapter rsync failed: '+result.stderr.decode()[-500:])
 replay=SnapshotReplay([('mini-swe-1',2)],apply);server=MarkerServer(path,replay)
 thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
 try:
  result=adapter_client(root,'replay',path)
  if result.returncode:raise RuntimeError('adapter replay failed: '+result.stderr.decode()[-1000:])
  replay.finish()
  return (root/'trial-result').read_bytes()==b'fixed'
 finally:
  server.shutdown();server.server_close();thread.join();path.unlink(missing_ok=True)
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
 checks['adapter_four_concurrent_replays']=all(pool.map(replay_adapter,range(1,5)))
checks['adapter_original_tool_ran_once']=Path('/work/adapter-invocations').read_bytes()==b'1'
for fd in link_fds+split_fds+dir_fds+nested_fds:os.close(fd)
for name,passed in checks.items():print(name,passed)
print('xattr_exercised',xattr)
if not all(checks.values()):raise RuntimeError('Btrfs snapshot checks failed')
for fd in fds:os.close(fd)
EOPY
'''.replace('LOADER',loader).replace('LOOPDEV',loop.name)
 cmd=['docker','run','--rm','--name',container_name,'--pull=never','--network','none','--device','/dev/loop-control','--device',str(loop),'--cap-add','SYS_ADMIN','--security-opt','apparmor=unconfined','--mount',f'type=bind,src={d},dst=/work','--mount',f'type=bind,src={mkfs},dst=/opt/mkfs.btrfs,readonly','--mount',f'type=bind,src={btrfs},dst=/opt/btrfs,readonly','--mount',f'type=bind,src={rsync},dst=/opt/rsync,readonly','--mount',f'type=bind,src={Path(__file__).with_name("topology.py").resolve()},dst=/opt/topology.py,readonly','--mount',f'type=bind,src={Path(__file__).with_name("mini_swe_client.py").resolve()},dst=/opt/mini_swe_client.py,readonly','--mount',f'type=bind,src={(project/"crates/asb-cli/src/agents/adapters/mini_swe.rs").resolve()},dst=/opt/mini_swe.rs,readonly','--mount',f'type=bind,src={(project/"crates/asb-cli/assets/benchmarks/asb_snapshot_service.py").resolve()},dst=/opt/asb_snapshot_service.py,readonly','--mount',f'type=bind,src={libraries},dst=/hostlib,readonly','--entrypoint','/bin/sh',image,'-c',script]
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
