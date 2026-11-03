"""Hermes applies recorded workspace effects before returning a tool result."""
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
    sidecar = root / 'sidecar'; progress = root / 'progress'
    os.environ['ASB_HERMES_NATIVE_SIDECAR'] = str(sidecar)
    os.environ['ASB_HERMES_NATIVE_PROGRESS'] = str(progress)
    source = (ROOT / 'crates/asb-cli/assets/scripts/hermes-native-tools.py').read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
    scope = {'__name__': 'hermes_fs_fixture'}
    exec(compile(source, 'hermes-native-tools.py', 'exec'), scope)
    args = ('write fixture', False, None, None, None, False, '.', False, False, None)
    recorder = scope['Recorder']()
    assert recorder.invoke(lambda *_: 'completed', *args) == 'completed'
    recorder.close()
    events = [
        {'sequence': 1, 'op': 'marker', 'path': None, 'marker': 'tool-begin:terminal-1'},
        {'sequence': 2, 'op': 'create', 'path': '/result', 'handle': 1, 'flags': 1, 'mode': 0o600},
        {'sequence': 3, 'op': 'write', 'path': '/result', 'handle': 1, 'offset': 0, 'bytes': 7, 'object': 'payload.bin', 'payload_offset': 0},
        {'sequence': 4, 'op': 'release', 'path': '/result', 'handle': 1},
        {'sequence': 5, 'op': 'marker', 'path': None, 'marker': 'tool-end:terminal-1'},
    ]
    (evidence / 'payload.bin').write_bytes(b'changed')
    (evidence / 'events.jsonl').write_text(''.join(json.dumps(e) + '\n' for e in events))
    server = ReplayServer(evidence, workspace, root / 'socket', timeout=2)
    os.environ['ASB_FS_REPLAY_SOCKET'] = str(root / 'socket')
    replay = scope['Replayer']()
    def forbidden(*_): raise AssertionError('live tool executed')
    assert replay.invoke(forbidden, *args) == 'completed'
    assert (workspace / 'result').read_bytes() == b'changed'
    replay.close(); server.finish()
    os.environ['ASB_FS_REPLAY_SOCKET'] = str(root / 'missing-socket')
    replay = scope['Replayer']()
    try:
        replay.invoke(forbidden, *args)
        raise AssertionError('missing coordinator accepted')
    except RuntimeError as error:
        assert 'boundary' in str(error)
    assert replay.index == 0 and replay.failed
    replay.close()
print('Hermes filesystem replay handoff passed')
