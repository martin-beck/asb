"""Run one Claude Read/Edit recording and four isolated loopback-only replays.

Set ASB_CLAUDE_BINARY and ASB_CLAUDE_FIXTURE_DIR. This opt-in Linux fixture
uses passwordless sudo to create private mount and network namespaces, then
runs the actual Claude executable as the invoking unprivileged user.
"""
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

fixture_dir = Path(os.environ['ASB_CLAUDE_FIXTURE_DIR']).resolve()
binary = Path(os.environ['ASB_CLAUDE_BINARY']).resolve()
script = Path(__file__).with_name('edit.mjs').resolve()
root = Path(tempfile.mkdtemp(prefix='c4-edit-', dir=fixture_dir))
target = root/'workspace'
target.mkdir()
cassette = root/'cassette.json'
uid, gid = str(os.getuid()), str(os.getgid())
if uid == '0':
    raise SystemExit('Run as an unprivileged user with passwordless sudo')
preserved = ','.join(['ASB_CLAUDE_FIXTURE_DIR', 'ASB_CLAUDE_BINARY',
                      'ASB_CLAUDE_FIXED_WORKSPACE', 'ASB_CLAUDE_PHASE', 'ASB_CLAUDE_EVENTS'])
command = ('mount --bind "$1" "$2" && ip link set lo up && '
           'exec setpriv --reuid "$3" --regid "$4" --init-groups "$5" "$6"')
node = shutil.which('node')
if node is None:
    raise SystemExit('node is required for the Claude fixture')


def run(label, phase):
    source = root/label
    source.mkdir()
    env = dict(os.environ, ASB_CLAUDE_FIXTURE_DIR=str(fixture_dir), ASB_CLAUDE_BINARY=str(binary),
               ASB_CLAUDE_FIXED_WORKSPACE=str(target), ASB_CLAUDE_PHASE=phase,
               ASB_CLAUDE_EVENTS=str(cassette))
    result = subprocess.run(['sudo', '-n', '--preserve-env='+preserved, 'unshare', '--mount', '--net',
                             '--propagation', 'private', 'sh', '-c', command, 'asb-fixture',
                             str(source), str(target), uid, gid, node, str(script)],
                            env=env, text=True, capture_output=True, timeout=180)
    if result.returncode:
        raise RuntimeError(f'{label} failed: {result.stderr[-2000:]}')
    if (source/'fixture.txt').read_text() != 'after\n':
        raise RuntimeError(f'{label} produced an unexpected file')
    return result.stdout.strip()


try:
    print(run('record', 'record'))
    with ThreadPoolExecutor(max_workers=4) as pool:
        results = list(pool.map(lambda index: run(f'replay-{index}', 'replay'), range(4)))
    if results != ['replay passed']*4 or (target/'fixture.txt').exists():
        raise RuntimeError('replicas did not remain isolated')
    print('one recording, four independent loopback-only replays passed')
finally:
    shutil.rmtree(root)
