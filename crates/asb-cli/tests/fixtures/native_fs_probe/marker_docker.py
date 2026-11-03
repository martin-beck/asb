"""Opt-in Docker/FUSE qualification of actual Hermes recorder boundaries."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import uuid

here = Path(__file__).resolve().parent
root = here.parents[4]
base = root / 'target/native-fs-probe'
base.mkdir(parents=True, exist_ok=True)
image = os.environ['ASB_FUSE_TEST_IMAGE']
fusepy = Path(os.environ['ASB_FUSEPY_MODULE']).resolve()
library = Path(os.environ['ASB_CONTAINER_FUSE_LIBRARY']).resolve()
proxy = here.parents[2] / 'assets/benchmarks/asb_fs_proxy.py'
supervisor = here.parents[2] / 'assets/benchmarks/asb_fs_record.py'
hook = here.parents[2] / 'assets/scripts/hermes-native-tools.py'
with tempfile.TemporaryDirectory(dir=base) as temporary:
    work = Path(temporary)
    for name in ('mount', 'run'):
        (work / name).mkdir()
    script = '''import os
from pathlib import Path
hook = Path('/hook.py')
source = hook.read_text().split('\\ntry:\\n    # Hermes exposes', 1)[0]
namespace = {'__name__':'marker_docker'}
exec(compile(source, str(hook), 'exec'), namespace)
recorder = namespace['Recorder']()
def tool(*_args):
    Path('/work/mount/result').write_text('captured')
    return 'completed'
assert recorder.invoke(tool, 'write', False, None, None, None, False, '.', False, False, None) == 'completed'
recorder.close()
'''
    (work / 'agent.py').write_text(script)
    command = '''set -eu
mkfifo /work/control
exec 3<>/work/control
python3 /supervisor.py /work/mount /work/run /proxy.py /opt /opt/libfuse.so.2 /work/markers.sock < /work/control > /work/supervisor.log 2>&1 &
pid=$!
trap 'umount /work/mount || true; kill "$pid" 2>/dev/null || true; wait "$pid" || true' EXIT
n=0
while ! grep -q READY /work/supervisor.log; do n=$((n+1)); [ "$n" -lt 100 ] || exit 42; sleep 0.05; done
ASB_FS_MARKER_SOCKET=/work/markers.sock ASB_HERMES_NATIVE_MODE=record \
 ASB_HERMES_NATIVE_SIDECAR=/work/native.jsonl ASB_HERMES_NATIVE_PROGRESS=/work/progress.json \
 python3 /work/agent.py
printf 'STOP\\n' >&3
wait "$pid"
trap - EXIT
chown -R "$ASB_FIXTURE_UID:$ASB_FIXTURE_GID" /work
'''
    name = 'asb-fs-marker-' + uuid.uuid4().hex
    args = ['docker','run','--rm','--name',name,'--network','none','--device','/dev/fuse',
            '--cap-add','SYS_ADMIN','--security-opt','apparmor=unconfined',
            '--mount',f'type=bind,src={work},dst=/work',
            '--mount',f'type=bind,src={proxy},dst=/proxy.py,readonly',
            '--mount',f'type=bind,src={supervisor},dst=/supervisor.py,readonly',
            '--mount',f'type=bind,src={hook},dst=/hook.py,readonly',
            '--mount',f'type=bind,src={fusepy},dst=/opt/fuse.py,readonly',
            '--mount',f'type=bind,src={library},dst=/opt/libfuse.so.2,readonly',
            '--env','PYTHONPATH=/opt','--env','FUSE_LIBRARY_PATH=/opt/libfuse.so.2',
            '--env',f'ASB_FIXTURE_UID={os.getuid()}','--env',f'ASB_FIXTURE_GID={os.getgid()}',
            '--entrypoint','/bin/sh',image,'-c',command]
    try:
        subprocess.run(args, check=True, timeout=60, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        events = [json.loads(line) for line in (work / 'run/filesystem-recording/events.jsonl').read_text().splitlines()]
        markers = {event['marker']:event['sequence'] for event in events if event['op'] == 'marker'}
        begin = next(event for event in events if event.get('marker') == 'tool-begin:terminal-1')
        writes = [event for event in events if event['op'] == 'write' and event['path'] == '/result']
        assert markers['tool-begin:terminal-1'] < writes[0]['sequence'] < markers['tool-end:terminal-1']
        assert writes[0]['caller']['tgid'] == begin['origin']['pid']
        assert json.loads((work / 'progress.json').read_text())['complete']
        assert (work / 'mount/result').read_text() == 'captured'
        print('Hermes native call boundaries surround real FUSE filesystem effects')
    finally:
        subprocess.run(['docker','rm','-f',name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
