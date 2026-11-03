import concurrent.futures, importlib.util, os, shutil, socket, subprocess, tempfile, json, threading, uuid
from pathlib import Path
probe=Path(__file__).resolve().parent
base=probe.parents[4]/'target/native-fs-probe'
base.mkdir(parents=True, exist_ok=True)
fusepy=Path(os.environ['ASB_FUSEPY_MODULE']).resolve()
fuse_library=Path(os.environ['ASB_CONTAINER_FUSE_LIBRARY']).resolve()
image=os.environ['ASB_FUSE_TEST_IMAGE']
agent_fixture=os.environ.get('ASB_HERMES_AGENT_FIXTURE')=='1'
unrestricted=os.environ.get('ASB_HERMES_UNRESTRICTED_FIXTURE')=='1'
native_files=os.environ.get('ASB_HERMES_NATIVE_FILES_FIXTURE')=='1'
mixed=os.environ.get('ASB_HERMES_MIXED_FIXTURE')=='1'
swe_baseline=os.environ.get('ASB_SWE_TEST_BASELINE')
swe_patch=os.environ.get('ASB_SWE_TEST_PATCH')
if bool(swe_baseline) != bool(swe_patch): raise RuntimeError('provide both SWE baseline and patch')
if swe_baseline and (not agent_fixture or native_files): raise RuntimeError('SWE patch fixture requires terminal agent fixture')
node=Path(os.environ['ASB_FIXTURE_NODE']).resolve() if agent_fixture else None
with tempfile.TemporaryDirectory(dir=base) as tmp, tempfile.TemporaryDirectory(dir=base.parents[2],prefix='asb-s-') as sockets:
 root=Path(tmp)
 for part in ('backing','mount','evidence','baseline'): (root/part).mkdir()
 if swe_baseline:
  for part in ('backing','baseline'):
   subprocess.run(['cp','-a','--reflink=auto',str(Path(swe_baseline).resolve())+'/.',str(root/part)],check=True)
 (root/'backing/source').write_text('original')
 (root/'baseline/source').write_text('original')
 if swe_baseline:
  (root/'backing/output').write_text('container')
  (root/'baseline/output').write_text('container')
 (root/'hermes-home').mkdir(mode=0o700)
 (root/'control').mkdir(mode=0o700)
 if swe_patch: shutil.copyfile(Path(swe_patch),root/'control/candidate.patch')
 (root/'hooks').mkdir(mode=0o700)
 shutil.copyfile(probe.parents[2]/'assets/scripts/hermes-native-tools.py',root/'hooks/sitecustomize.py')
 if agent_fixture:
  if unrestricted:
   hook_path=root/'hooks/sitecustomize.py'
   hook_source=hook_path.read_text()
   restriction='toolsets._HERMES_CORE_TOOLS[:] = ["terminal"]'
   assert hook_source.count(restriction)==1
   hook_path.write_text(hook_source.replace(restriction,'pass  # fixture preserves upstream tools'))
  with (root/'hooks/sitecustomize.py').open('a') as hook:
   hook.write('\n'+(probe/'hermes_agent_boundary.py').read_text())
  (root/'mock-record').mkdir(mode=0o700)
  command=('git apply /work/control/candidate.patch && printf ran > /tmp/asb-native-agent-executed' if swe_patch else 'printf fixture > asb-real-agent.txt; printf ran > /tmp/asb-native-agent-executed')
  (root/'mock-record/command.txt').write_text(command)
 script='''set -eu
export ASB_PROBE_ROOT_TGID=$$
python3 /probe/proxy.py /work/backing /work/mount /work/evidence > /work/proxy.log 2>&1 &
p=$!
trap 'fusermount3 -u /work/mount || true; wait "$p" || true' EXIT
n=0
while ! mountpoint -q /work/mount; do n=$((n+1)); [ "$n" -lt 100 ] || exit 42; sleep 0.05; done
cat /work/mount/source
if [ "${ASB_FIXTURE_SWE_PATCH:-0}" != 1 ]; then printf container > /work/mount/output; fi
if [ "${ASB_FIXTURE_SWE_PATCH:-0}" = 1 ]; then
  setpriv --reuid "$ASB_HERMES_UID" --regid "$ASB_HERMES_GID" --clear-groups git -C /work/mount symbolic-ref --short HEAD > /work/control/git-branch.log 2>&1 || true
  setpriv --reuid "$ASB_HERMES_UID" --regid "$ASB_HERMES_GID" --clear-groups git -C /work/mount status --short > /work/control/git-status.log 2>&1 || true
fi
if [ "${ASB_FIXTURE_HERMES:-0}" = 1 ]; then
  if [ "${ASB_FIXTURE_AGENT:-0}" = 1 ]; then
    (cd /work/mount; HERMES_HOME=/work/hermes-home TERMINAL_ENV=local PYTHONPATH=/work/hooks:/opt ASB_FIXTURE_AGENT_BOUNDARY=1 ASB_FIXTURE_MOCK_DIR=/work/mock-record ASB_FIXTURE_LLM_CASSETTE=/work/control/llm-record.jsonl ASB_FIXTURE_EFFECT_SOCKET=/work/control/record.sock ASB_HERMES_NATIVE_MODE=record ASB_HERMES_NATIVE_SIDECAR=/work/control/native.jsonl ASB_HERMES_NATIVE_PROGRESS=/work/control/record-progress.json setpriv --reuid "$ASB_HERMES_UID" --regid "$ASB_HERMES_GID" --clear-groups /bin/sh /probe/hermes_agent_fixture.sh)
  else
    (cd /work/mount; HERMES_HOME=/work/hermes-home TERMINAL_ENV=local PYTHONPATH=/work/hooks:/opt ASB_FIXTURE_EFFECT_SOCKET=/work/control/record.sock ASB_HERMES_NATIVE_MODE=record ASB_HERMES_NATIVE_SIDECAR=/work/control/native.jsonl ASB_HERMES_NATIVE_PROGRESS=/work/control/record-progress.json setpriv --reuid "$ASB_HERMES_UID" --regid "$ASB_HERMES_GID" --clear-groups /asb-hermes/venv/bin/python /probe/hermes_real_terminal.py)
  fi
fi
'''
 name='asb-fuse-probe-'+uuid.uuid4().hex
 cmd=['docker','run','--rm','--name',name,'--network','none','--device','/dev/fuse','--cap-add','SYS_ADMIN','--security-opt','apparmor=unconfined','--mount',f'type=bind,src={root},dst=/work','--mount',f'type=bind,src={probe},dst=/probe,readonly','--mount',f'type=bind,src={fusepy},dst=/opt/fuse.py,readonly','--mount',f'type=bind,src={fuse_library},dst=/opt/libfuse.so.2,readonly','--env','PYTHONPATH=/opt','--env','FUSE_LIBRARY_PATH=/opt/libfuse.so.2','--entrypoint','/bin/sh',image,'-c',script]
 fuse_unmount=os.environ.get('ASB_CONTAINER_FUSERMOUNT')
 if fuse_unmount:
  position=cmd.index('--env')
  cmd[position:position]=['--mount', f'type=bind,src={Path(fuse_unmount).resolve()},dst=/usr/bin/fusermount3,readonly']
 hermes_install=os.environ.get('ASB_HERMES_TEST_INSTALL')
 if mixed and not native_files: raise RuntimeError('mixed fixture requires file-tool fixture')
 if mixed:
  position=cmd.index('--env')
  cmd[position:position]=['--env','ASB_FIXTURE_MIXED=1']
 if native_files and not unrestricted: raise RuntimeError('file-tool fixture requires unrestricted fixture')
 if unrestricted and not agent_fixture: raise RuntimeError('unrestricted fixture requires agent fixture')
 if swe_patch:
  position=cmd.index('--env')
  cmd[position:position]=['--env','ASB_FIXTURE_SWE_PATCH=1']
 if native_files:
  position=cmd.index('--env')
  cmd[position:position]=['--env','ASB_FIXTURE_NATIVE_FILES=1','--env','ASB_FIXTURE_NATIVE_FILE_TRANSCRIPT=/work/control/files.jsonl']
 if unrestricted:
  position=cmd.index('--env')
  cmd[position:position]=['--env','ASB_FIXTURE_UNRESTRICTED=1']
 if agent_fixture and not hermes_install: raise RuntimeError('agent fixture requires ASB_HERMES_TEST_INSTALL')
 if agent_fixture:
  position=cmd.index('--env')
  cmd[position:position]=['--env','ASB_FIXTURE_AGENT=1','--mount',f'type=bind,src={probe.parents[1]/"guest_provider_fixture.py"},dst=/provider-base.py,readonly',
                          '--mount',f'type=bind,src={probe/"hermes_provider_catalog.py"},dst=/provider.py,readonly',
                          '--mount',f'type=bind,src={node},dst=/asb-node,readonly',
                          '--mount',f'type=bind,src={probe.parents[2]/"assets/scripts"},dst=/asb-scripts,readonly']
 if hermes_install:
  position=cmd.index('--env')
  cmd[position:position]=['--mount', f'type=bind,src={Path(hermes_install).resolve()},dst=/asb-hermes,readonly']
  cmd[position:position]=['--env', 'ASB_FIXTURE_HERMES=1', '--env', 'ASB_PROBE_ALLOW_OTHER=1', '--env', 'ASB_PROBE_CONTROL_SOCKET=/work/control/record.sock', '--env', f'ASB_HERMES_UID={os.getuid()}', '--env', f'ASB_HERMES_GID={os.getgid()}']
 try:
  result=subprocess.run(cmd,capture_output=True,text=True)
 except BaseException:
  subprocess.run(['docker','rm','-f',name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,check=False)
  retained=base/('failure-'+uuid.uuid4().hex)
  retained.mkdir(mode=0o700)
  for source in (root/'proxy.log',root/'mock-record/agent.log',root/'mock-record/server.log',root/'mock-record/tape.log'):
   if source.is_file(): shutil.copyfile(source,retained/(source.parent.name+'-'+source.name))
  print(f'Fixture diagnostics retained in {retained}',flush=True)
  raise
 if result.returncode:
  retained=base/('failure-'+uuid.uuid4().hex)
  retained.mkdir(mode=0o700)
  for source in (root/'proxy.log',root/'mock-record/agent.log',root/'mock-record/server.log',root/'mock-record/tape.log'):
   if source.is_file(): shutil.copyfile(source,retained/(source.parent.name+'-'+source.name))
  raise RuntimeError(f'container probe failed; diagnostics in {retained}: {result.stderr[-500:]}')
 assert result.stdout == 'original'
 assert (root/'backing/output').read_text() == 'container'
 print('docker exit:',result.returncode)
 print('stdout:',result.stdout[:200])
 print('stderr:',result.stderr[:500])
 print('file:',(root/'backing/output').read_text() if (root/'backing/output').exists() else 'missing')
 if swe_patch:
  print('git branch probe:',(root/'control/git-branch.log').read_text()[-400:])
  print('git status probe:',(root/'control/git-status.log').read_text()[-400:])
 if not (root/'evidence/events.jsonl').exists(): raise RuntimeError('container produced no operation log')
 if (root/'evidence/events.jsonl').exists():
  events=[json.loads(x) for x in (root/'evidence/events.jsonl').read_text().splitlines()]
  if not swe_patch: assert any(e['op']=='write' and e['path']=='/output' for e in events)
  from collections import Counter
  print('recorded operation counts:',dict(Counter(e['op'] for e in events)))
  if hermes_install:
   expected_files=('asb-real-agent.txt',) if agent_fixture else ('first','second')
   if swe_patch:
    assert any(e['op']=='marker' and e['marker']=='tool-end:one' for e in events)
   elif native_files:
    assert (root/'backing/asb-real-agent.txt').read_text()=='fixture'
    assert (root/'backing/asb-real-agent.txt').stat().st_uid==os.getuid()
    assert (root/'backing/asb-real-agent.txt').stat().st_gid==os.getgid()
    assert any(e['op']=='rename' and e['destination']=='/asb-real-agent.txt' for e in events)
   else:
    assert all(any(e['op']=='write' and e['path']=='/'+name for e in events) for name in expected_files)
   native=[json.loads(line) for line in (root/'control/native.jsonl').read_text().splitlines()]
   if unrestricted:
    tool_names=json.loads((root/'mock-record/tool-names.json').read_text())
    assert {'terminal','read_file','write_file','patch'} <= set(tool_names), tool_names
   expected_calls=(1 if mixed else 0) if native_files else (1 if agent_fixture else 2)
   if native_files: assert len((root/'control/files.jsonl').read_text().splitlines())==(4 if mixed else 3)
   assert sum(event['event']=='write' for event in native)==expected_calls
   assert sum(event['event']=='output' for event in native)==expected_calls
   assert json.loads((root/'control/record-progress.json').read_text())['complete']
  def replay(index):
   destination=root/f'replay-{index}'
   if not hermes_install:
    subprocess.run(['python3',str(probe/'replay.py'),str(root/'evidence'),str(destination),str(root/'baseline')],check=True)
    assert (destination/'output').read_text()=='container'
    return
   spec=importlib.util.spec_from_file_location('fixture_replay',probe/'replay.py')
   module=importlib.util.module_from_spec(spec)
   spec.loader.exec_module(module)
   cursor=module.Replay(root/'evidence',destination,root/'baseline',enforce_admission=True)
   ends={e['marker']:e['sequence'] for e in events if e['op']=='marker'}
   replay_home=root/f'hermes-replay-home-{index}'
   replay_home.mkdir(mode=0o700)
   sock_path=Path(sockets)/f'r-{index}.sock'
   listener=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)
   listener.bind(str(sock_path)); listener.listen(1); listener.settimeout(30)
   errors=[]
   def serve():
    try:
     connection,_=listener.accept()
     with connection, connection.makefile('rwb',buffering=0) as stream:
      names=(('write','read','patch','search','terminal') if mixed else ('write','read','patch')) if native_files else (('one',) if agent_fixture else ('one','two'))
      for name in names:
       assert stream.readline().decode().strip()=='tool-end:'+name
       cursor.admit(name)
       assert cursor.apply_until(ends['tool-end:'+name])
       if swe_patch:
        diff=subprocess.run(['git','-C',str(destination),'diff','--name-only'],capture_output=True,text=True,check=True,env=dict(os.environ,GIT_OPTIONAL_LOCKS='0'))
        assert diff.stdout.strip()
       elif native_files:
        assert (destination/'asb-real-agent.txt').read_text()==('fixture' if name in ('patch','search','terminal') else 'before')
        if mixed: assert (destination/'mixed-marker').exists()==(name=='terminal')
       elif agent_fixture:
        assert (destination/'asb-real-agent.txt').read_text()=='fixture'
       else:
        assert (destination/'first').read_text()=='first'
        assert (destination/'second').exists()==(name=='two')
       stream.write((str(cursor.sequence)+'\n').encode())
    except BaseException as error:
     errors.append(error)
     (root/f'control/replay-server-{index}.error').write_text(f'{type(error).__name__}: {error}')
   server=threading.Thread(target=serve,daemon=True); server.start()
   replay_name='asb-hermes-replay-'+uuid.uuid4().hex
   replay_mock=root/f'mock-replay-{index}'
   if agent_fixture:
    replay_mock.mkdir(mode=0o700)
    (replay_mock/'command.txt').write_text(command)
    shutil.copyfile(root/'control/llm-record.jsonl',root/f'control/llm-replay-{index}.jsonl')
   replay_cmd=['docker','run','--rm','--name',replay_name,'--network','none',
               '--mount',f'type=bind,src={root},dst=/work',
               '--mount',f'type=bind,src={destination},dst=/work/mount',
               '--mount',f'type=bind,src={replay_home},dst=/work/hermes-home',
               '--mount',f'type=bind,src={sockets},dst=/asb-sockets',
               '--mount',f'type=bind,src={probe},dst=/probe,readonly',
               '--mount',f'type=bind,src={Path(hermes_install).resolve()},dst=/asb-hermes,readonly',
               '--workdir','/work/mount',
               '--env','HERMES_HOME=/work/hermes-home',
               '--env','TERMINAL_ENV=local','--env','PYTHONPATH=/work/hooks',
               '--env','ASB_HERMES_NATIVE_MODE=replay',
               '--env',f'ASB_FIXTURE_EFFECT_SOCKET=/asb-sockets/r-{index}.sock',
               '--env','ASB_HERMES_NATIVE_SIDECAR=/work/control/native.jsonl',
               '--env',f'ASB_HERMES_NATIVE_PROGRESS=/work/control/replay-{index}.json',
               '--entrypoint','/asb-hermes/venv/bin/python',image,'/probe/hermes_real_terminal.py']
   if not agent_fixture:
    replay_cmd[2:2]=['--user',f'{os.getuid()}:{os.getgid()}']
   if swe_patch:
    position=replay_cmd.index('--env')
    replay_cmd[position:position]=['--env','ASB_FIXTURE_SWE_PATCH=1']
   if mixed:
    position=replay_cmd.index('--env')
    replay_cmd[position:position]=['--env','ASB_FIXTURE_MIXED=1']
   if native_files:
    position=replay_cmd.index('--env')
    replay_cmd[position:position]=['--env','ASB_FIXTURE_NATIVE_FILES=1',
                                  '--env','ASB_FIXTURE_NATIVE_FILE_TRANSCRIPT=/work/control/files.jsonl',
                                  '--env',f'ASB_FIXTURE_NATIVE_FILES_PROGRESS=/work/control/files-progress-{index}']
   if unrestricted:
    position=replay_cmd.index('--env')
    replay_cmd[position:position]=['--env','ASB_FIXTURE_UNRESTRICTED=1']
   if agent_fixture:
    position=replay_cmd.index('--entrypoint')
    replay_cmd[position:]=['--env','ASB_FIXTURE_AGENT_BOUNDARY=1','--env',f'ASB_FIXTURE_MOCK_DIR=/work/{replay_mock.name}',
                           '--env',f'ASB_FIXTURE_LLM_CASSETTE=/work/control/llm-replay-{index}.jsonl',
                           '--mount',f'type=bind,src={node},dst=/asb-node,readonly',
                           '--mount',f'type=bind,src={probe.parents[2]/"assets/scripts"},dst=/asb-scripts,readonly',
                           '--mount',f'type=bind,src={probe.parents[1]/"guest_provider_fixture.py"},dst=/provider-base.py,readonly',
                           '--mount',f'type=bind,src={probe/"hermes_provider_catalog.py"},dst=/provider.py,readonly',
                           '--entrypoint','/bin/sh',image,'-c',
                           f'exec setpriv --reuid {os.getuid()} --regid {os.getgid()} --clear-groups /bin/sh /probe/hermes_agent_fixture.sh']
   try:
    subprocess.run(replay_cmd,check=True,stdout=subprocess.DEVNULL,stderr=subprocess.PIPE)
    server.join(timeout=5)
    assert not server.is_alive() and not errors, errors
    cursor.finish()
    progress=json.loads((root/f'control/replay-{index}.json').read_text())
    assert progress['complete'] and progress['invocations']==expected_calls
    if native_files:
     assert (root/f'control/files-progress-{index}').read_text()==('4' if mixed else '3')
    if agent_fixture:
     tape_progress=json.loads((root/f'control/llm-replay-{index}.jsonl.progress.json').read_text())
     assert tape_progress['consumed']==tape_progress['total'] and tape_progress['total']>0
    assert (destination/'source').read_text()=='original'
    assert (destination/'output').read_text()=='container'
    if swe_patch:
     subprocess.run(['git','-C',str(destination),'apply','--reverse','--check',str(root/'control/candidate.patch')],check=True,env=dict(os.environ,GIT_OPTIONAL_LOCKS='0'))
   except BaseException:
    subprocess.run(['docker','rm','-f',replay_name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,check=False)
    retained=base/('replay-failure-'+uuid.uuid4().hex)
    retained.mkdir(mode=0o700)
    if agent_fixture:
     for name in ('agent.log','tape.log','mismatch.json'):
      source=replay_mock/name
      if source.is_file(): shutil.copyfile(source,retained/name)
    server_error=root/f'control/replay-server-{index}.error'
    if server_error.is_file(): shutil.copyfile(server_error,retained/'server.error')
    print(f'Replay fixture diagnostics retained in {retained}',flush=True)
    raise
   finally:
    listener.close(); cursor.close()
  with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool: list(pool.map(replay,range(4)))
  print('four independent offline replays: passed')
 if (root/'proxy.log').exists() and result.returncode: print('proxy log:',(root/'proxy.log').read_text()[-1000:])
