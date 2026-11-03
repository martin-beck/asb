"""Offline, test-only Hermes replay experiment on an isolated SWE checkout."""
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import uuid

PROBE = Path(__file__).resolve().parent
BASE = PROBE.parents[4] / 'target/native-fs-probe'
BASE.mkdir(parents=True, exist_ok=True)
IMAGE = os.environ['ASB_FUSE_TEST_IMAGE']
LOWER = Path(os.environ['ASB_SWE_TEST_BASELINE']).resolve()
PATCH = Path(os.environ['ASB_SWE_TEST_PATCH']).resolve()
HERMES = Path(os.environ['ASB_HERMES_TEST_INSTALL']).resolve()
NODE = Path(os.environ['ASB_FIXTURE_NODE']).resolve()
OVERLAY = Path(os.environ['ASB_CONTAINER_FUSE_OVERLAY']).resolve()
LIB3 = Path(os.environ['ASB_CONTAINER_FUSE3_LIBRARY']).resolve()
LIB2 = Path(os.environ['ASB_CONTAINER_FUSE_LIBRARY']).resolve()
FUSER = Path(os.environ['ASB_CONTAINER_FUSERMOUNT']).resolve()
FUSEPY = Path(os.environ['ASB_FUSEPY_MODULE']).resolve()
uid, gid = os.getuid(), os.getgid()
mixed=os.environ.get('ASB_HERMES_MIXED_FIXTURE')=='1'

with tempfile.TemporaryDirectory(dir=BASE) as tmp:
    root = Path(tmp)
    for part in ('control','hooks','evidence','mock-record','home-record','record-upper','record-work','mount','lower-view'):
        (root / part).mkdir(mode=0o700)
    initial_index = hashlib.sha256((LOWER / '.git/index').read_bytes()).hexdigest()
    shutil.copyfile(PATCH, root / 'control/candidate.patch')
    hook = (PROBE.parents[2] / 'assets/scripts/hermes-native-tools.py').read_text()
    restriction = 'toolsets._HERMES_CORE_TOOLS[:] = ["terminal"]'
    assert hook.count(restriction) == 1
    (root / 'hooks/sitecustomize.py').write_text(hook.replace(restriction, 'pass  # fixture retains core tools') + '\n' + (PROBE / 'hermes_agent_boundary.py').read_text())
    command = 'git apply /work/control/candidate.patch && ' + ('printf mixed > mixed-marker; ' if mixed else '') + 'printf ran > /tmp/asb-native-agent-executed'
    (root / 'mock-record/command.txt').write_text(command)

    def mounts():
        pairs = [(LOWER,'/lower'),(root,'/work'),(PROBE,'/probe'),
                 (HERMES,'/asb-hermes'),(NODE,'/asb-node'),
                 (OVERLAY,'/opt/fuse-overlayfs'),(LIB3,'/usr/lib/x86_64-linux-gnu/libfuse3.so.3'),
                 (LIB2,'/opt/libfuse.so.2'),(FUSER,'/usr/bin/fusermount3'),(FUSEPY,'/opt/fuse.py'),
                 (PROBE.parents[1]/'guest_provider_fixture.py','/provider-base.py'),
                 (PROBE/'hermes_provider_catalog.py','/provider.py'),
                 (PROBE.parents[2]/'assets/scripts','/asb-scripts')]
        result=[]
        for source,destination in pairs:
            suffix='' if source==root else ',readonly'
            result.extend(['--mount',f'type=bind,src={source},dst={destination}{suffix}'])
        return result

    def launch(label, script, extra):
        name='asb-overlay-agent-'+uuid.uuid4().hex
        cmd=['docker','run','--rm','--name',name,'--network','none','--device','/dev/fuse',
             '--cap-add','SYS_ADMIN','--security-opt','apparmor=unconfined',*mounts(),
             '--env',f'ASB_UID={uid}','--env',f'ASB_GID={gid}',*extra,
             '--entrypoint','/bin/sh',IMAGE,'-c',script]
        try:
            result=subprocess.run(cmd,capture_output=True,text=True)
        except BaseException:
            subprocess.run(['docker','rm','-f',name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,check=False)
            raise
        if result.returncode:
            retained=BASE/f'overlay-agent-failure-{uuid.uuid4().hex}'
            retained.mkdir(mode=0o700)
            for source in (root/'proxy.log',root/f'{label}/agent.log',root/f'{label}/tape.log',root/f'{label}/mismatch.json',root/f'control/effect-{label}.log'):
                if source.is_file(): shutil.copyfile(source,retained/(source.parent.name+'-'+source.name))
            raise RuntimeError(f'fixture failed; diagnostics in {retained}: {result.stderr[-500:]}')
        return result

    common='''export HERMES_HOME=/work/home-record TERMINAL_ENV=local PYTHONPATH=/work/hooks:/opt
export ASB_FIXTURE_AGENT_BOUNDARY=1 ASB_FIXTURE_UNRESTRICTED=1 ASB_FIXTURE_SWE_PATCH=1
export ASB_HERMES_NATIVE_SIDECAR=/work/control/native.jsonl
''' + ('''export ASB_FIXTURE_NATIVE_FILES=1 ASB_FIXTURE_MIXED=1 ASB_FIXTURE_CHECK_TOOL_ID=1 ASB_FIXTURE_NATIVE_FILE_TRANSCRIPT=/work/control/files.jsonl
''' if mixed else '')
    record_script='''set -eu
/opt/fuse-overlayfs -o allow_other,lowerdir=/lower,upperdir=/work/record-upper,workdir=/work/record-work /work/lower-view
export ASB_PROBE_ROOT_TGID=$$ ASB_PROBE_ALLOW_OTHER=1 ASB_PROBE_CONTROL_SOCKET=/work/control/record.sock PYTHONPATH=/opt FUSE_LIBRARY_PATH=/opt/libfuse.so.2
python3 /probe/proxy.py /work/lower-view /work/mount /work/evidence > /work/proxy.log 2>&1 &
p=$!
cleanup() { code=$?; trap - EXIT; cd /; n=0; while ! fusermount3 -u /work/mount 2>/work/control/unmount.log; do n=$((n+1)); if [ "$n" -ge 100 ]; then kill "$p" 2>/dev/null || true; code=1; break; fi; sleep .1; done; wait "$p" || code=1; fusermount3 -u /work/lower-view || code=1; chown -R "$ASB_UID:$ASB_GID" /work/record-upper /work/record-work || code=1; exit "$code"; }
trap cleanup EXIT
n=0; while ! mountpoint -q /work/mount; do n=$((n+1)); [ "$n" -lt 200 ] || exit 42; sleep .05; done
''' + common + '''export ASB_HERMES_NATIVE_MODE=record ASB_HERMES_NATIVE_PROGRESS=/work/control/record-progress.json
export ASB_FIXTURE_MOCK_DIR=/work/mock-record ASB_FIXTURE_LLM_CASSETTE=/work/control/llm-record.jsonl ASB_FIXTURE_EFFECT_SOCKET=/work/control/record.sock
cd /work/mount
setpriv --reuid "$ASB_UID" --regid "$ASB_GID" --clear-groups /bin/sh /probe/hermes_agent_fixture.sh
setpriv --reuid "$ASB_UID" --regid "$ASB_GID" --clear-groups git diff --binary > /work/control/record.diff
'''
    launch('mock-record',record_script,[])
    native=[json.loads(line) for line in (root/'control/native.jsonl').read_text().splitlines()]
    assert sum(item['event']=='write' for item in native)==1
    assert json.loads((root/'control/record-progress.json').read_text())['complete']
    if mixed:
        file_events=[json.loads(line) for line in (root/'control/files.jsonl').read_text().splitlines()]
        assert [event['request']['tool'] for event in file_events]==['write_file','read_file','patch','search_files','read_file','skills_list']
        for event in file_events:
            output=json.loads(event['output'])
            assert not output.get('error'), output
        assert 'before' in file_events[1]['output']
        assert 'fixture' in file_events[3]['output']
        assert 'fixture' in file_events[4]['output']
    offered=set(json.loads((root/'mock-record/tool-names.json').read_text()))
    assert {'terminal','read_file','write_file','patch'} <= offered
    assert (root/'control/record.diff').stat().st_size > 0
    events=[json.loads(line) for line in (root/'evidence/events.jsonl').read_text().splitlines()]
    assert any(item.get('marker')==('tool-end:terminal' if mixed else 'tool-end:one') for item in events)

    def replay(index):
        for suffix in ('upper','work','home','mock'):
            (root/f'{suffix}-{index}').mkdir(mode=0o700)
        (root/f'mock-{index}/command.txt').write_text(command)
        shutil.copyfile(root/'control/llm-record.jsonl',root/f'control/llm-{index}.jsonl')
        script='''set -eu
/opt/fuse-overlayfs -o allow_other,lowerdir=/lower,upperdir=/work/upper-{i},workdir=/work/work-{i} /work/mount
cleanup() {{ code=$?; trap - EXIT; cd /; if [ -n "${{effect:-}}" ]; then kill "$effect" 2>/dev/null || true; wait "$effect" 2>/dev/null || true; fi; n=0; while ! fusermount3 -u /work/mount 2>/work/control/unmount-{i}.log; do n=$((n+1)); if [ "$n" -ge 100 ]; then code=1; break; fi; sleep .1; done; chown -R "$ASB_UID:$ASB_GID" /work/upper-{i} /work/work-{i} || code=1; exit "$code"; }}
trap cleanup EXIT
ASB_FIXTURE_MIXED={mixed} setpriv --reuid "$ASB_UID" --regid "$ASB_GID" --clear-groups /usr/bin/python3 -B -S /probe/overlay_effect_server.py {i} > /work/control/effect-mock-{i}.log 2>&1 &
effect=$!
n=0; while [ ! -S /work/control/effect-{i}.sock ]; do n=$((n+1)); [ "$n" -lt 200 ] || exit 43; sleep .05; done
export HERMES_HOME=/work/home-record TERMINAL_ENV=local PYTHONPATH=/work/hooks:/opt
export ASB_FIXTURE_AGENT_BOUNDARY=1 ASB_FIXTURE_UNRESTRICTED=1 ASB_FIXTURE_SWE_PATCH=1
export ASB_FIXTURE_NATIVE_FILES={mixed} ASB_FIXTURE_MIXED={mixed} ASB_FIXTURE_CHECK_TOOL_ID={mixed} ASB_FIXTURE_NATIVE_FILE_TRANSCRIPT=/work/control/files.jsonl ASB_FIXTURE_NATIVE_FILES_PROGRESS=/work/control/files-progress-{i}
export ASB_HERMES_NATIVE_MODE=replay ASB_HERMES_NATIVE_SIDECAR=/work/control/native.jsonl ASB_HERMES_NATIVE_PROGRESS=/work/control/progress-{i}.json
export ASB_FIXTURE_MOCK_DIR=/work/mock-{i} ASB_FIXTURE_LLM_CASSETTE=/work/control/llm-{i}.jsonl ASB_FIXTURE_EFFECT_SOCKET=/work/control/effect-{i}.sock
cd /work/mount
setpriv --reuid "$ASB_UID" --regid "$ASB_GID" --clear-groups /bin/sh /probe/hermes_agent_fixture.sh
wait "$effect"
effect=
setpriv --reuid "$ASB_UID" --regid "$ASB_GID" --clear-groups git apply --reverse --check /work/control/candidate.patch
setpriv --reuid "$ASB_UID" --regid "$ASB_GID" --clear-groups git diff --binary > /work/control/replay-{i}.diff
'''.format(i=index,mixed=int(mixed))
        # Each container sees its own home at the same path.
        launch(f'mock-{index}',script,['--mount',f'type=bind,src={root/f"home-{index}"},dst=/work/home-record'])
        progress=json.loads((root/f'control/progress-{index}.json').read_text())
        tape=json.loads((root/f'control/llm-{index}.jsonl.progress.json').read_text())
        assert progress['complete'] and progress['invocations']==1
        if mixed: assert (root/f'control/files-progress-{index}').read_text()=='6'
        assert tape['consumed']==tape['total'] and tape['total']>0
        assert (root/f'control/replay-{index}.diff').read_bytes()==(root/'control/record.diff').read_bytes()

    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        list(pool.map(replay,range(4)))
    assert hashlib.sha256((LOWER/'.git/index').read_bytes()).hexdigest()==initial_index
    print('Hermes fixture: one recording and four offline overlay replays applied the supplied patch')
