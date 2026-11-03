"""Record and replay Hermes file tools with the actual filesystem coordinator."""
import json
import os
from pathlib import Path
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'crates/asb-cli/assets/benchmarks'))
from asb_fs_replay_server import ReplayServer

with tempfile.TemporaryDirectory(dir=ROOT / 'target') as directory:
    root = Path(directory); evidence = root / 'evidence'; evidence.mkdir()
    workspace = root / 'workspace'; workspace.mkdir()
    os.environ['ASB_HERMES_NATIVE_SIDECAR'] = str(root / 'sidecar')
    os.environ['ASB_HERMES_NATIVE_PROGRESS'] = str(root / 'progress')
    source = (ROOT / 'crates/asb-cli/assets/scripts/hermes-native-tools.py').read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
    module = {'__name__': 'hermes_file_tool_fixture'}
    exec(compile(source, 'hermes-native-tools.py', 'exec'), module)
    calls = [('write_file', {'path': 'result', 'content': 'before'}),
             ('read_file', {'path': 'result'}),
             ('patch', {'path': 'result', 'new_string': 'after'}),
             ('search_files', {'pattern': 'after'})]
    class Registry:
        def __init__(self): self.called = []
        def dispatch(self, name, args, **kwargs):
            self.called.append(name)
            return {'tool': name, 'value': args}
    registry = Registry(); recorder = module['Recorder']()
    module['install_dispatch_coverage'](registry, recorder, 'record')
    expected = [registry.dispatch(name, args) for name, args in calls]
    recorder.close()
    assert registry.called == [name for name, _ in calls]
    events = []
    def add(op, path=None, **kwargs):
        events.append({'sequence': len(events)+1, 'op': op, 'path': path, **kwargs})
    for index, (name, _) in enumerate(calls, 1):
        add('marker', marker=f'tool-begin:terminal-{index}')
        if name == 'write_file':
            add('create', '/result', handle=1, flags=1, mode=0o600)
            add('write', '/result', handle=1, offset=0, bytes=6, object='payload.bin', payload_offset=0)
            add('release', '/result', handle=1)
        if name == 'patch':
            add('open', '/result', handle=2, flags=2)
            add('truncate', '/result', handle=2, size=0)
            add('write', '/result', handle=2, offset=0, bytes=5, object='payload.bin', payload_offset=6)
            add('release', '/result', handle=2)
        add('marker', marker=f'tool-end:terminal-{index}')
    (evidence / 'payload.bin').write_bytes(b'beforeafter')
    (evidence / 'events.jsonl').write_text(''.join(json.dumps(event)+'\n' for event in events))
    server = ReplayServer(evidence, workspace, root / 'socket', timeout=2)
    os.environ['ASB_FS_REPLAY_SOCKET'] = str(root / 'socket')
    registry = Registry(); replay = module['Replayer']()
    module['install_dispatch_coverage'](registry, replay, 'replay')
    for (name, args), result in zip(calls, expected):
        assert registry.dispatch(name, args) == result
        if name == 'write_file': assert (workspace / 'result').read_bytes() == b'before'
        if name == 'patch': assert (workspace / 'result').read_bytes() == b'after'
    assert not registry.called
    replay.close(); server.finish()
    os.environ.pop('ASB_FS_REPLAY_SOCKET')
    registry = Registry(); replay = module['Replayer']()
    module['install_dispatch_coverage'](registry, replay, 'replay')
    try:
        registry.dispatch('write_file', {'path': 'other', 'content': 'before'})
        raise AssertionError('different request was accepted')
    except RuntimeError:
        pass
    assert replay.failed and not registry.called
    replay.close()
print('Hermes file tool replay passed')
