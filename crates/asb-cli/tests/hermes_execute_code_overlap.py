"""Interleaved Hermes parent calls must replay their own child boundaries."""
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
    root = Path(directory); evidence = root/'evidence'; evidence.mkdir()
    os.environ['ASB_HERMES_NATIVE_SIDECAR'] = str(root/'sidecar')
    os.environ['ASB_HERMES_NATIVE_PROGRESS'] = str(root/'progress')
    source = (ROOT/'crates/asb-cli/assets/scripts/hermes-native-tools.py').read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
    module = {'__name__':'hermes_overlap_fixture'}
    exec(compile(source, 'hermes-native-tools.py', 'exec'), module)
    markers = []; module['_filesystem_marker'] = lambda kind, alias: markers.append((kind, alias)) or True
    rendezvous = threading.Barrier(2)
    class Registry:
        def __init__(self): self.calls = []
        def dispatch(self, name, args, **kwargs):
            self.calls.append(name)
            if name == 'execute_code':
                rendezvous.wait(timeout=5)
                self.dispatch('read_file', {'path':args['code']})
            return {'name':name, 'args':args}
    registry = Registry(); recorder = module['Recorder']()
    module['install_dispatch_coverage'](registry, recorder, 'record')
    errors = []
    def invoke(reg, code):
        try:
            assert reg.dispatch('execute_code', {'code':code}) == {'name':'execute_code', 'args':{'code':code}}
        except BaseException as error: errors.append(error)
    threads = [threading.Thread(target=invoke, args=(registry, code)) for code in ('first','second')]
    for thread in threads: thread.start()
    for thread in threads: thread.join(5)
    assert not errors and all(not thread.is_alive() for thread in threads)
    recorder.close()
    events = [{'sequence':i, 'op':'marker', 'path':None, 'marker':f'tool-{kind}:{alias}'}
              for i,(kind,alias) in enumerate(markers,1)]
    (evidence/'events.jsonl').write_text(''.join(json.dumps(e)+'\n' for e in events))
    (evidence/'payload.bin').write_bytes(b'')
    for replica in range(4):
        workspace = root/f'workspace-{replica}'; workspace.mkdir()
        socket = root/f'socket-{replica}'
        server = ReplayServer(evidence, workspace, socket, timeout=3)
        os.environ['ASB_FS_REPLAY_SOCKET'] = str(socket)
        registry = Registry(); replay = module['Replayer']()
        module['install_dispatch_coverage'](registry, replay, 'replay')
        threads = [threading.Thread(target=invoke, args=(registry, code)) for code in ('second','first')]
        for thread in threads: thread.start()
        for thread in threads: thread.join(8)
        assert not errors and all(not thread.is_alive() for thread in threads)
        assert registry.calls == []
        replay.close(); server.finish()
print('Overlapping Hermes parents and children replayed in four workspaces')
