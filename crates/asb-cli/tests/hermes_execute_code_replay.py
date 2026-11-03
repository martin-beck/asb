"""Replay nested Hermes code calls without running the recorded Python code."""
import contextvars
import json
import os
from pathlib import Path
import sys
import tempfile
import threading

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'crates/asb-cli/assets/benchmarks'))
from asb_fs_replay_server import ReplayServer

with tempfile.TemporaryDirectory(dir=ROOT / 'target') as directory:
    root = Path(directory); evidence = root / 'evidence'; evidence.mkdir()
    workspace = root / 'workspace'; workspace.mkdir()
    os.environ['ASB_HERMES_NATIVE_SIDECAR'] = str(root / 'sidecar')
    os.environ['ASB_HERMES_NATIVE_PROGRESS'] = str(root / 'progress')
    source = (ROOT / 'crates/asb-cli/assets/scripts/hermes-native-tools.py').read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
    module = {'__name__': 'hermes_code_replay_fixture'}
    exec(compile(source, 'hermes-native-tools.py', 'exec'), module)
    class Registry:
        def __init__(self): self.calls = []
        def dispatch(self, name, args, **kwargs):
            self.calls.append(name)
            if name == 'execute_code':
                context = contextvars.copy_context(); errors = []
                def child():
                    try:
                        assert context.run(self.dispatch, 'write_file', {'path':'child', 'content':'child'}) == {'child': True}
                    except BaseException as error: errors.append(error)
                worker = threading.Thread(target=child); worker.start(); worker.join(5)
                assert not worker.is_alive() and not errors
                return {'parent': True}
            return {'child': True}
    calls = {'code':'fixture'}
    registry = Registry(); recorder = module['Recorder']()
    module['install_dispatch_coverage'](registry, recorder, 'record')
    assert registry.dispatch('execute_code', calls) == {'parent': True}
    recorder.close(); assert registry.calls == ['execute_code', 'write_file']
    events = [
        {'sequence':1,'op':'marker','path':None,'marker':'tool-begin:terminal-1'},
        {'sequence':2,'op':'marker','path':None,'marker':'tool-begin:terminal-2'},
        {'sequence':3,'op':'create','path':'/child','handle':1,'flags':1,'mode':0o600},
        {'sequence':4,'op':'write','path':'/child','handle':1,'offset':0,'bytes':5,'object':'payload.bin','payload_offset':0},
        {'sequence':5,'op':'release','path':'/child','handle':1},
        {'sequence':6,'op':'marker','path':None,'marker':'tool-end:terminal-2'},
        {'sequence':7,'op':'create','path':'/parent','handle':2,'flags':1,'mode':0o600},
        {'sequence':8,'op':'write','path':'/parent','handle':2,'offset':0,'bytes':6,'object':'payload.bin','payload_offset':5},
        {'sequence':9,'op':'release','path':'/parent','handle':2},
        {'sequence':10,'op':'marker','path':None,'marker':'tool-end:terminal-1'},
    ]
    (evidence/'payload.bin').write_bytes(b'childparent')
    (evidence/'events.jsonl').write_text(''.join(json.dumps(e)+'\n' for e in events))
    for replica in range(4):
        workspace = root / f'replica-{replica}'; workspace.mkdir()
        socket = root / f'socket-{replica}'
        server = ReplayServer(evidence, workspace, socket, timeout=3)
        os.environ['ASB_FS_REPLAY_SOCKET'] = str(socket)
        registry = Registry(); replay = module['Replayer']()
        module['install_dispatch_coverage'](registry, replay, 'replay')
        assert registry.dispatch('execute_code', calls) == {'parent': True}
        assert not registry.calls
        assert (workspace/'child').read_bytes() == b'child'
        assert (workspace/'parent').read_bytes() == b'parent'
        replay.close(); server.finish()
print('Hermes nested code replay passed')
