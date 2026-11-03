"""Offline diagnostic of the current Hermes sidecar's missing file effects.

This test documents a known defect. A complete replay backend must reverse the
final assertion: every replay workspace must contain the recorded write without
invoking the original terminal tool.
"""
import concurrent.futures
import os
import socket
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[3]
HOOK = ROOT / 'crates/asb-cli/assets/scripts/hermes-native-tools.py'
BASE = ROOT / 'target/hermes-native-effect-gap'
BASE.mkdir(parents=True, exist_ok=True)


def worker(mode, workspace, sidecar):
    os.environ['ASB_HERMES_NATIVE_MODE'] = mode
    os.environ['ASB_HERMES_NATIVE_SIDECAR'] = str(sidecar)
    os.environ['ASB_HERMES_NATIVE_PROGRESS'] = str(workspace / 'progress.json')
    os.chdir(workspace)
    source = HOOK.read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
    namespace = {'__name__': 'hermes_fixture'}
    exec(compile(source, str(HOOK), 'exec'), namespace)
    call = ('write fixture', False, None, None, None, False, '.', False, False, None)
    called = []
    def write_fixture(*args):
        called.append(True)
        Path('result.txt').write_text('recorded contents')
        return 'write succeeded'
    transport = namespace['Recorder' if mode == 'record' else 'Replayer']()
    control_fd = os.environ.get('ASB_PROBE_CONTROL_FD')
    if mode == 'record' and control_fd is not None:
        with socket.socket(fileno=int(control_fd)) as control:
            with control.makefile('rwb', buffering=0) as stream:
                stream.write(b'tool-begin:hermes\n')
                assert stream.readline().strip().isdigit()
                assert transport.invoke(write_fixture, *call) == 'write succeeded'
                stream.write(b'tool-end:hermes\n')
                assert stream.readline().strip().isdigit()
    else:
        assert transport.invoke(write_fixture, *call) == 'write succeeded'
    transport.close()
    if mode == 'record':
        assert called and Path('result.txt').read_text() == 'recorded contents'
    else:
        assert not called, 'replay unexpectedly invoked the original tool'
        if mode == 'replay-with-effects':
            assert Path('result.txt').read_text() == 'recorded contents'
        else:
            # This is the known defect, not the desired end state.
            assert not Path('result.txt').exists(), 'the known missing-effect defect was repaired; update this test'


if len(sys.argv) == 4:
    worker(sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3]))
else:
    with tempfile.TemporaryDirectory(dir=BASE) as tmp:
        root = Path(tmp)
        record = root / 'record'
        record.mkdir()
        sidecar = root / 'native.jsonl'
        subprocess.run([sys.executable, __file__, 'record', str(record), str(sidecar)], check=True)
        def replay(index):
            workspace = root / f'replay-{index}'
            workspace.mkdir()
            subprocess.run([sys.executable, __file__, 'replay', str(workspace), str(sidecar)], check=True)
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            list(pool.map(replay, range(4)))
        print('Confirmed known Hermes defect: four replays return the recorded result without the file effect')
