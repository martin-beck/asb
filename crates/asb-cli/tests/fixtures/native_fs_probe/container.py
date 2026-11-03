import concurrent.futures, os, subprocess, tempfile, json, uuid
from pathlib import Path
probe=Path(__file__).resolve().parent
base=probe.parents[4]/'target/native-fs-probe'
base.mkdir(parents=True, exist_ok=True)
fusepy=Path(os.environ['ASB_FUSEPY_MODULE']).resolve()
fuse_library=Path(os.environ['ASB_CONTAINER_FUSE_LIBRARY']).resolve()
image=os.environ['ASB_FUSE_TEST_IMAGE']
with tempfile.TemporaryDirectory(dir=base) as tmp:
 root=Path(tmp)
 for part in ('backing','mount','evidence','baseline'): (root/part).mkdir()
 (root/'backing/source').write_text('original')
 (root/'baseline/source').write_text('original')
 script='''set -eu
export ASB_PROBE_ROOT_TGID=$$
python3 /probe/proxy.py /work/backing /work/mount /work/evidence > /work/proxy.log 2>&1 &
p=$!
trap 'fusermount3 -u /work/mount || true; wait "$p" || true' EXIT
n=0
while ! mountpoint -q /work/mount; do n=$((n+1)); [ "$n" -lt 100 ] || exit 42; sleep 0.05; done
cat /work/mount/source
printf container > /work/mount/output
'''
 name='asb-fuse-probe-'+uuid.uuid4().hex
 cmd=['docker','run','--rm','--name',name,'--network','none','--device','/dev/fuse','--cap-add','SYS_ADMIN','--security-opt','apparmor=unconfined','--mount',f'type=bind,src={root},dst=/work','--mount',f'type=bind,src={probe},dst=/probe,readonly','--mount',f'type=bind,src={fusepy},dst=/opt/fuse.py,readonly','--mount',f'type=bind,src={fuse_library},dst=/opt/libfuse.so.2,readonly','--env','PYTHONPATH=/opt','--env','FUSE_LIBRARY_PATH=/opt/libfuse.so.2','--entrypoint','/bin/sh',image,'-c',script]
 fuse_unmount=os.environ.get('ASB_CONTAINER_FUSERMOUNT')
 if fuse_unmount:
  position=cmd.index('--env')
  cmd[position:position]=['--mount', f'type=bind,src={Path(fuse_unmount).resolve()},dst=/usr/bin/fusermount3,readonly']
 try:
  result=subprocess.run(cmd,capture_output=True,text=True,timeout=30)
 except BaseException:
  subprocess.run(['docker','rm','-f',name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,check=False)
  raise
 if result.returncode:
  raise RuntimeError(f'container probe failed: {result.stderr[-500:]}')
 assert result.stdout == 'original'
 assert (root/'backing/output').read_text() == 'container'
 print('docker exit:',result.returncode)
 print('stdout:',result.stdout[:200])
 print('stderr:',result.stderr[:500])
 print('file:',(root/'backing/output').read_text() if (root/'backing/output').exists() else 'missing')
 if not (root/'evidence/events.jsonl').exists(): raise RuntimeError('container produced no operation log')
 if (root/'evidence/events.jsonl').exists():
  events=[json.loads(x) for x in (root/'evidence/events.jsonl').read_text().splitlines()]
  assert any(e['op']=='write' and e['path']=='/output' for e in events)
  print('recorded operations:',[(e['op'],e['path']) for e in events])
  def replay(index):
   destination=root/f'replay-{index}'
   subprocess.run(['python3',str(probe/'replay.py'),str(root/'evidence'),str(destination),str(root/'baseline')],check=True)
   assert (destination/'source').read_text()=='original'
   assert (destination/'output').read_text()=='container'
  with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool: list(pool.map(replay,range(4)))
  print('four independent offline replays: passed')
 if (root/'proxy.log').exists() and result.returncode: print('proxy log:',(root/'proxy.log').read_text()[-1000:])
