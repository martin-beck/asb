import concurrent.futures, hashlib, os, subprocess, tempfile, uuid
from pathlib import Path
probe=Path(__file__).resolve().parent
base=probe.parents[4]/'target/native-fs-probe'
base.mkdir(parents=True,exist_ok=True)
image=os.environ['ASB_FUSE_TEST_IMAGE']
low=Path(os.environ['ASB_SWE_TEST_BASELINE']).resolve()
overlay=Path(os.environ['ASB_CONTAINER_FUSE_OVERLAY']).resolve()
lib=Path(os.environ['ASB_CONTAINER_FUSE3_LIBRARY']).resolve()
unmount=Path(os.environ['ASB_CONTAINER_FUSERMOUNT']).resolve()
lib2=Path(os.environ['ASB_CONTAINER_FUSE_LIBRARY']).resolve()
fusepy=Path(os.environ['ASB_FUSEPY_MODULE']).resolve()
marker='.asb-overlay-test-'+uuid.uuid4().hex
assert not (low/marker).exists()
lower_index=hashlib.sha256((low/'.git/index').read_bytes()).hexdigest()
with tempfile.TemporaryDirectory(dir=base) as tmp:
 root=Path(tmp)
 def one(index):
  work=root/str(index);work.mkdir()
  for n in ('upper','work','mount','observed','evidence'): (work/n).mkdir()
  script='''set -eu
/opt/fuse-overlayfs -o allow_other,lowerdir=/lower,upperdir=/work/upper,workdir=/work/work /work/mount
export ASB_PROBE_ROOT_TGID=$$ ASB_PROBE_ALLOW_OTHER=1 PYTHONPATH=/opt FUSE_LIBRARY_PATH=/opt/libfuse.so.2
python3 /probe/proxy.py /work/mount /work/observed /work/evidence > /work/proxy.log 2>&1 &
p=$!
cleanup() { code=$?; trap - EXIT; fusermount3 -u /work/observed || code=1; wait "$p" || code=1; fusermount3 -u /work/mount || code=1; chown -R "$ASB_UID:$ASB_GID" /work || code=1; exit "$code"; }
trap cleanup EXIT
n=0
while ! mountpoint -q /work/observed; do n=$((n+1)); [ "$n" -lt 200 ] || exit 42; sleep .05; done
printf "before "; sha256sum /work/observed/.git/index | cut -d" " -f1
printf "stat "; stat -c "%d %i %Z" /work/observed/README.md
start=$(date +%s%N)
setpriv --reuid "$ASB_UID" --regid "$ASB_GID" --clear-groups git -C /work/observed status --porcelain=2 --branch > /work/status
stop=$(date +%s%N)
printf "elapsed_ms %s\n" "$(((stop-start)/1000000))"
printf "after "; sha256sum /work/observed/.git/index | cut -d" " -f1
setpriv --reuid "$ASB_UID" --regid "$ASB_GID" --clear-groups /bin/sh -c 'printf "%s" "$ASB_REPLICA" > "/work/observed/$ASB_MARKER"'
'''
  name='asb-overlay-probe-'+uuid.uuid4().hex
  cmd=['docker','run','--rm','--name',name,'--network','none','--device','/dev/fuse','--cap-add','SYS_ADMIN','--security-opt','apparmor=unconfined','--mount',f'type=bind,src={low},dst=/lower,readonly','--mount',f'type=bind,src={work},dst=/work','--mount',f'type=bind,src={overlay},dst=/opt/fuse-overlayfs,readonly','--mount',f'type=bind,src={lib},dst=/usr/lib/x86_64-linux-gnu/libfuse3.so.3,readonly','--mount',f'type=bind,src={unmount},dst=/usr/bin/fusermount3,readonly','--mount',f'type=bind,src={probe},dst=/probe,readonly','--mount',f'type=bind,src={fusepy},dst=/opt/fuse.py,readonly','--mount',f'type=bind,src={lib2},dst=/opt/libfuse.so.2,readonly','--env',f'ASB_UID={os.getuid()}','--env',f'ASB_GID={os.getgid()}','--env',f'ASB_REPLICA={index}','--env',f'ASB_MARKER={marker}','--entrypoint','/bin/sh',image,'-c',script]
  try:
   r=subprocess.run(cmd,capture_output=True,text=True,timeout=120)
  except BaseException:
   subprocess.run(['docker','rm','-f',name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,check=False);raise
  if r.returncode: raise RuntimeError(r.stderr[-1000:])
  assert (work/'upper'/marker).read_text()==str(index)
  return r.stdout.strip().splitlines(),(work/'status').read_text()
 with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool: results=list(pool.map(one,range(4)))
 assert len({status for _,status in results})==1
 assert not (low/marker).exists()
 results=[row for row,_ in results]
 before=[row[0].split()[1] for row in results]
 stats=[row[1].split()[1:] for row in results]
 elapsed=[int(row[2].split()[1]) for row in results]
 after=[row[3].split()[1] for row in results]
 assert len(set(before))==1 and before==after and before[0]==lower_index
 assert hashlib.sha256((low/'.git/index').read_bytes()).hexdigest()==lower_index
 assert len({row[1] for row in stats})==1 and len({row[2] for row in stats})==1
 print('initial indexes identical:',len(set(before))==1)
 print('indexes unchanged:',before==after)
 print('resulting indexes identical:',len(set(after))==1)
 print('device numbers identical:',len({row[0] for row in stats})==1)
 print('inode numbers identical:',len({row[1] for row in stats})==1)
 print('change times identical:',len({row[2] for row in stats})==1)
 print('parallel git status elapsed ms:',elapsed)
