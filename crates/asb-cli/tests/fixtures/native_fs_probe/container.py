import concurrent.futures, os, shutil, subprocess, tempfile, json, uuid
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
 (root/'hermes-home').mkdir(mode=0o700)
 (root/'control').mkdir(mode=0o700)
 (root/'hooks').mkdir(mode=0o700)
 shutil.copyfile(probe.parents[2]/'assets/scripts/hermes-native-tools.py',root/'hooks/sitecustomize.py')
 script='''set -eu
export ASB_PROBE_ROOT_TGID=$$
python3 /probe/proxy.py /work/backing /work/mount /work/evidence > /work/proxy.log 2>&1 &
p=$!
trap 'fusermount3 -u /work/mount || true; wait "$p" || true' EXIT
n=0
while ! mountpoint -q /work/mount; do n=$((n+1)); [ "$n" -lt 100 ] || exit 42; sleep 0.05; done
cat /work/mount/source
printf container > /work/mount/output
if [ "${ASB_FIXTURE_HERMES:-0}" = 1 ]; then
  (cd /work/mount; HERMES_HOME=/work/hermes-home TERMINAL_ENV=local PYTHONPATH=/work/hooks:/opt ASB_HERMES_NATIVE_MODE=record ASB_HERMES_NATIVE_SIDECAR=/work/control/native.jsonl ASB_HERMES_NATIVE_PROGRESS=/work/control/record-progress.json setpriv --reuid "$ASB_HERMES_UID" --regid "$ASB_HERMES_GID" --clear-groups /asb-hermes/venv/bin/python /probe/hermes_real_terminal.py)
fi
'''
 name='asb-fuse-probe-'+uuid.uuid4().hex
 cmd=['docker','run','--rm','--name',name,'--network','none','--device','/dev/fuse','--cap-add','SYS_ADMIN','--security-opt','apparmor=unconfined','--mount',f'type=bind,src={root},dst=/work','--mount',f'type=bind,src={probe},dst=/probe,readonly','--mount',f'type=bind,src={fusepy},dst=/opt/fuse.py,readonly','--mount',f'type=bind,src={fuse_library},dst=/opt/libfuse.so.2,readonly','--env','PYTHONPATH=/opt','--env','FUSE_LIBRARY_PATH=/opt/libfuse.so.2','--entrypoint','/bin/sh',image,'-c',script]
 fuse_unmount=os.environ.get('ASB_CONTAINER_FUSERMOUNT')
 if fuse_unmount:
  position=cmd.index('--env')
  cmd[position:position]=['--mount', f'type=bind,src={Path(fuse_unmount).resolve()},dst=/usr/bin/fusermount3,readonly']
 hermes_install=os.environ.get('ASB_HERMES_TEST_INSTALL')
 if hermes_install:
  position=cmd.index('--env')
  cmd[position:position]=['--mount', f'type=bind,src={Path(hermes_install).resolve()},dst=/asb-hermes,readonly']
  cmd[position:position]=['--env', 'ASB_FIXTURE_HERMES=1', '--env', 'ASB_PROBE_ALLOW_OTHER=1', '--env', f'ASB_HERMES_UID={os.getuid()}', '--env', f'ASB_HERMES_GID={os.getgid()}']
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
  if hermes_install:
   assert any(e['op']=='write' and e['path']=='/hermes-tool-output' for e in events)
   native=[json.loads(line) for line in (root/'control/native.jsonl').read_text().splitlines()]
   assert sum(event['event']=='write' for event in native)==1
   assert sum(event['event']=='output' for event in native)==1
   assert json.loads((root/'control/record-progress.json').read_text())['complete']
  print('recorded operations:',[(e['op'],e['path']) for e in events])
  def replay(index):
   destination=root/f'replay-{index}'
   subprocess.run(['python3',str(probe/'replay.py'),str(root/'evidence'),str(destination),str(root/'baseline')],check=True)
   assert (destination/'source').read_text()=='original'
   assert (destination/'output').read_text()=='container'
   if hermes_install:
    assert (destination/'hermes-tool-output').read_text()=='hermes'
    replay_home=root/f'hermes-replay-home-{index}'
    replay_home.mkdir(mode=0o700)
    replay_name='asb-hermes-replay-'+uuid.uuid4().hex
    replay_cmd=['docker','run','--rm','--name',replay_name,'--network','none',
                '--user',f'{os.getuid()}:{os.getgid()}',
                '--mount',f'type=bind,src={root},dst=/work',
                '--mount',f'type=bind,src={destination},dst=/work/mount',
                '--mount',f'type=bind,src={probe},dst=/probe,readonly',
                '--mount',f'type=bind,src={Path(hermes_install).resolve()},dst=/asb-hermes,readonly',
                '--workdir','/work/mount',
                '--env',f'HERMES_HOME=/work/{replay_home.name}',
                '--env','TERMINAL_ENV=local','--env','PYTHONPATH=/work/hooks',
                '--env','ASB_HERMES_NATIVE_MODE=replay',
                '--env','ASB_HERMES_NATIVE_SIDECAR=/work/control/native.jsonl',
                '--env',f'ASB_HERMES_NATIVE_PROGRESS=/work/control/replay-{index}.json',
                '--entrypoint','/asb-hermes/venv/bin/python',image,'/probe/hermes_real_terminal.py']
    try:
     subprocess.run(replay_cmd,check=True,timeout=30,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
    except BaseException:
     subprocess.run(['docker','rm','-f',replay_name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,check=False)
     raise
    progress=json.loads((root/f'control/replay-{index}.json').read_text())
    assert progress['complete'] and progress['invocations']==1
  with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool: list(pool.map(replay,range(4)))
  print('four independent offline replays: passed')
 if (root/'proxy.log').exists() and result.returncode: print('proxy log:',(root/'proxy.log').read_text()[-1000:])
