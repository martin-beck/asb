"""Opt-in Linux FUSE ordering experiment; this is not a replay backend."""
import concurrent.futures
import json
import importlib.util
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import time

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[4]
python = os.environ.get('ASB_FUSE_TEST_PYTHON')
if not python or not shutil.which('fusermount3'):
    raise SystemExit('Set ASB_FUSE_TEST_PYTHON to Python with fusepy; install fusermount3.')
base = ROOT / 'target' / 'native-fs-probe'
base.mkdir(parents=True, exist_ok=True)

def wait_for(predicate, message):
    deadline = time.monotonic() + 4
    while not predicate():
        if time.monotonic() > deadline:
            raise RuntimeError(message)
        time.sleep(.01)

with tempfile.TemporaryDirectory(dir=base) as temp:
    root = Path(temp)
    baseline, backing, mount, evidence = (root / name for name in ('baseline', 'backing', 'mount', 'evidence'))
    for directory in (baseline, backing, mount, evidence):
        directory.mkdir()
    (baseline / 'one').write_bytes(b'initial')
    os.link(baseline / 'one', baseline / 'alias')
    (baseline / 'other').write_bytes(b'independent')
    (baseline / 'two').write_bytes(b'initial')
    os.link(baseline / 'two', baseline / 'two-alias')
    subprocess.run(['/bin/cp', '-a', str(baseline) + '/.', str(backing)], check=True)
    control, peer = socket.socketpair()
    env = dict(os.environ, ASB_PROBE_HOLD_WRITE='1', ASB_PROBE_CONTROL_FD=str(peer.fileno()), ASB_PROBE_ROOT_TGID=str(os.getpid()))
    with (root / 'fuse.log').open('wb') as log:
        process = subprocess.Popen([python, str(HERE / 'proxy.py'), str(backing), str(mount), str(evidence)], env=env, stdout=log, stderr=log, pass_fds=(peer.fileno(),))
        peer.close()
        markers = control.makefile('rwb', buffering=0)
        def mark(name):
            markers.write((name + '\n').encode('ascii'))
            return int(markers.readline())
        mounted = False
        try:
            wait_for(lambda: os.path.ismount(mount) or process.poll() is not None, 'FUSE mount timed out')
            if process.poll() is not None:
                raise RuntimeError('FUSE process exited before mounting')
            mounted = True
            writer_tids = {}
            def write(name):
                writer_tids[name] = threading.get_native_id()
                fd = os.open(mount / name, os.O_WRONLY)
                try:
                    os.pwrite(fd, b'changed', 0)
                finally:
                    os.close(fd)
            with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
                writer = pool.submit(write, 'one')
                try:
                    wait_for(lambda: (evidence / 'write-applied').exists(), 'write did not reach the test barrier')
                    truncation = pool.submit(os.truncate, mount / 'alias', 0)
                    unrelated = pool.submit((mount / 'other').read_bytes)
                    assert unrelated.result(timeout=2) == b'independent'
                    time.sleep(.1)
                    assert not truncation.done(), 'dependent truncate passed the write barrier'
                finally:
                    (evidence / 'write-release').touch()
                writer.result(timeout=5)
                truncation.result(timeout=5)
            assert (backing / 'alias').read_bytes() == b''
            (evidence / 'write-applied').unlink()
            (evidence / 'write-release').unlink()
            def open_truncate():
                fd = os.open(mount / 'two-alias', os.O_WRONLY | os.O_TRUNC)
                os.close(fd)
            with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
                writer = pool.submit(write, 'two')
                try:
                    wait_for(lambda: (evidence / 'write-applied').exists(), 'second write did not reach barrier')
                    truncation = pool.submit(open_truncate)
                    unrelated = pool.submit((mount / 'other').read_bytes)
                    assert unrelated.result(timeout=2) == b'independent'
                    time.sleep(.1)
                    assert not truncation.done(), 'open with truncation passed write barrier'
                finally:
                    (evidence / 'write-release').touch()
                writer.result(timeout=5)
                truncation.result(timeout=5)
            assert (backing / 'two').read_bytes() == b''
            begin = mark('tool-begin:child')
            child = subprocess.Popen([python, '-c',
                'from pathlib import Path; Path("child").write_bytes(b"child data")'], cwd=mount)
            child_pid = child.pid
            assert child.wait(timeout=5) == 0
            end = mark('tool-end:child')
            assert (backing / 'child').read_bytes() == b'child data'
            hermes_sidecar = root / 'hermes.jsonl'
            hermes_script = ROOT / 'crates/asb-cli/tests/hermes_native_effect_gap.py'
            hermes_env = dict(os.environ, ASB_PROBE_CONTROL_FD=str(control.fileno()))
            subprocess.run([python, str(hermes_script), 'record', str(mount), str(hermes_sidecar)],
                           check=True, env=hermes_env, pass_fds=(control.fileno(),))
            markers_so_far = [json.loads(line) for line in (evidence / 'events.jsonl').read_text().splitlines()]
            hermes_begin = next(event['sequence'] for event in markers_so_far if event.get('marker') == 'tool-begin:hermes')
            hermes_end = next(event['sequence'] for event in markers_so_far if event.get('marker') == 'tool-end:hermes')
            assert (backing / 'result.txt').read_text() == 'recorded contents'
            a_begin = mark('tool-begin:a')
            b_begin = mark('tool-begin:b')
            overlap = subprocess.Popen([python, '-c',
                'import subprocess,sys; subprocess.run([sys.executable, "-c", '
                '\'from pathlib import Path; Path("overlap").write_bytes(b"tool b")\'], check=True)'], cwd=mount)
            overlap_pid = overlap.pid
            assert overlap.wait(timeout=5) == 0
            a_end = mark('tool-end:a')
            b_end = mark('tool-end:b')
            bg_begin = mark('tool-begin:bg')
            background = subprocess.Popen([python, '-c',
                'import sys; from pathlib import Path; sys.stdin.read(1); '
                'Path("background").write_bytes(b"later write")'], cwd=mount, stdin=subprocess.PIPE)
            bg_pid = background.pid
            bg_end = mark('tool-end:bg')
            background.stdin.write(b'x')
            background.stdin.close()
            assert background.wait(timeout=5) == 0
            after_begin = mark('tool-begin:after')
            after_end = mark('tool-end:after')
        finally:
            (evidence / 'write-release').touch()
            markers.close()
            control.close()
            if mounted:
                subprocess.run(['fusermount3', '-u', str(mount)], check=True)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=5)
        if process.returncode != 0:
            raise RuntimeError(f'FUSE process failed: {process.returncode}')
    events = [json.loads(line) for line in (evidence / 'events.jsonl').read_text().splitlines()]
    ops = [event['op'] for event in events]
    assert ops.index('write') < ops.index('truncate'), ops
    second_write = next(i for i, event in enumerate(events) if event['op'] == 'write' and event['path'] == '/two')
    second_truncate = next(i for i, event in enumerate(events) if event['op'] == 'truncate' and event['path'] == '/two-alias')
    assert second_write < second_truncate, ops
    child_writes = [event for event in events if event['op'] == 'write' and event['path'] == '/child']
    assert begin < child_writes[0]['sequence'] < end
    hermes_writes = [event for event in events if event['op'] == 'write' and event['path'] == '/result.txt']
    assert hermes_writes and hermes_begin < hermes_writes[0]['sequence'] < hermes_end
    overlap_writes = [event for event in events if event['op'] == 'write' and event['path'] == '/overlap']
    assert overlap_writes and a_begin < b_begin < overlap_writes[0]['sequence'] < a_end < b_end
    for event in overlap_writes:
        caller = event['caller']
        assert caller['tgid'] != overlap_pid and caller['ppid'] == overlap_pid, caller
        ancestry = caller['ancestry']
        assert [entry['tgid'] for entry in ancestry[:3]] == [caller['tgid'], overlap_pid, os.getpid()], ancestry
        assert all(entry['start_ticks'] > 0 for entry in ancestry[:3])
    background_writes = [event for event in events if event['op'] == 'write' and event['path'] == '/background']
    assert background_writes and bg_begin < bg_end < background_writes[0]['sequence'] < after_begin < after_end
    assert all(event['caller']['tgid'] == bg_pid for event in background_writes)
    assert child_writes and all(event['caller']['tid'] == child_pid and event['caller']['tgid'] == child_pid and event['caller']['ppid'] == os.getpid() and event['caller']['start_ticks'] is not None for event in child_writes), child_writes
    parent_writes = [event for event in events if event['op'] == 'write' and event['path'] == '/one']
    assert parent_writes and all(event['caller']['tid'] == writer_tids['one'] and event['caller']['tgid'] == os.getpid() and event['caller']['start_ticks'] is not None for event in parent_writes), parent_writes
    # A future caller needs effects before it returns each tool response, not
    # one final replay of the whole filesystem history.
    module_spec = importlib.util.spec_from_file_location('native_fs_replay', HERE / 'replay.py')
    module = importlib.util.module_from_spec(module_spec)
    module_spec.loader.exec_module(module)
    staged_root = root / 'staged-replay'
    staged = module.Replay(evidence, staged_root, baseline)
    try:
        staged.apply_until(parent_writes[0]['sequence'])
        assert (staged_root / 'one').read_bytes() == b'changed'
        try:
            staged.apply_until(0)
        except RuntimeError:
            pass
        else:
            raise AssertionError('replay cursor moved backwards')
        staged.apply_until(end)
        assert (staged_root / 'child').read_bytes() == b'child data'
        assert not (staged_root / 'result.txt').exists()
        staged.apply_until(hermes_end)
        assert (staged_root / 'result.txt').read_text() == 'recorded contents'
        subprocess.run([python, str(hermes_script), 'replay-with-effects', str(staged_root), str(hermes_sidecar)], check=True)
        staged.finish()
        assert (staged_root / 'one').read_bytes() == b''
        assert (staged_root / 'child').read_bytes() == b'child data'
    finally:
        staged.close()
    def replay(index):
        destination = root / f'replay-{index}'
        gated = module.Replay(evidence, destination, baseline, enforce_admission=True)
        try:
            for tool in ('child', 'hermes', 'a'):
                gated.admit(tool)
            assert gated.apply_until(a_end) is False
            assert gated.sequence == b_begin - 1
            assert not (destination / 'overlap').exists()
            gated.admit('b')
            assert gated.apply_until(a_end) is True
            assert (destination / 'overlap').read_bytes() == b'tool b'
            gated.admit('bg')
            assert gated.apply_until(bg_end) is True
            assert not (destination / 'background').exists()
            gated.admit('after')
            assert gated.apply_until(after_begin) is True
            assert (destination / 'background').read_bytes() == b'later write'
            gated.finish()
        finally:
            gated.close()
        assert (destination / 'one').read_bytes() == b''
        assert (destination / 'other').read_bytes() == b'independent'
        assert (destination / 'two').read_bytes() == b''
        assert (destination / 'child').read_bytes() == b'child data'
        subprocess.run([python, str(hermes_script), 'replay-with-effects', str(destination), str(hermes_sidecar)], check=True)
        assert os.stat(destination / 'two').st_ino == os.stat(destination / 'two-alias').st_ino
        assert os.stat(destination / 'one').st_ino == os.stat(destination / 'alias').st_ino
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        list(pool.map(replay, range(4)))
    print('FUSE ordering experiment passed: truncate and open-truncate, unrelated reads, four operation replays')
