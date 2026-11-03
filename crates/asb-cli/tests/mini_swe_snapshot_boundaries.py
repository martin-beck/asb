"""Exercise the adapter's snapshot marker path with an offline tool fixture."""
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

from mini_swe_binary_replay import LocalEnvironment, transport

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / 'crates/asb-cli/assets/benchmarks'))
from asb_snapshot_service import MarkerServer, SnapshotReplay, SnapshotService, load_replay_boundaries

SCRATCH = ROOT / 'target/mini-swe-snapshot-tests'
SCRATCH.mkdir(parents=True, exist_ok=True)


class SnapshotBoundaryTest(unittest.TestCase):
    def test_adapter_records_tool_result_and_snapshot(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root = Path(directory)
            workspace = root / 'workspace'
            workspace.mkdir()
            service = SnapshotService(workspace, root / 'store', ['fixture'],
                                      snapshot=lambda destination: shutil.copytree(workspace, destination))
            path = root / 'marker.sock'
            server = MarkerServer(path, service)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            tape = root / 'tape.jsonl'
            try:
                with patch.dict(os.environ, {'ASB_MINI_SWE_TOOL_TAPE': str(tape),
                                            'ASB_MINI_SWE_TOOL_MODE': 'record',
                                            'ASB_FS_MARKER_SOCKET': str(path)}):
                    namespace = {}
                    exec(compile(transport, 'asb_mini_swe_tools.py', 'exec'), namespace)
                    result = namespace['AsbTapeEnvironment'](cwd=str(workspace)).execute({'command': 'run tests'})
                self.assertEqual(result['returncode'], 0)
            finally:
                server.shutdown(); server.server_close(); thread.join(); path.unlink(missing_ok=True)
            self.assertEqual(service.finish(), 3)
            entry = json.loads(tape.read_text())
            self.assertNotIn('effects', entry)
            self.assertEqual((root/'store/snapshot-00000002/generated.png').read_bytes(), b'\x89PNG\x00\xff\x01')
            events = [json.loads(line) for line in (root/'store/markers.jsonl').read_text().splitlines()]
            self.assertEqual([(event['kind'], event['alias']) for event in events],
                             [('begin', 'mini-swe-1'), ('end', 'mini-swe-1')])
            snapshot = root/'store/snapshot-00000002'
            for index in range(4):
                replica = root/f'replica-{index}'
                replica.mkdir()
                replay_socket = root/f'replay-{index}.sock'
                replay = SnapshotReplay(load_replay_boundaries(root/'store'),
                                        lambda _: shutil.copytree(snapshot, replica, dirs_exist_ok=True))
                replay_server = MarkerServer(replay_socket, replay)
                replay_thread = threading.Thread(target=replay_server.handle_request, daemon=True)
                replay_thread.start()
                try:
                    with patch.dict(os.environ, {'ASB_MINI_SWE_TOOL_TAPE': str(tape),
                                                'ASB_MINI_SWE_TOOL_MODE': 'replay',
                                                'ASB_FS_REPLAY_SOCKET': str(replay_socket)}, clear=True):
                        with patch.object(LocalEnvironment, 'execute', side_effect=AssertionError('tool reran')):
                            namespace = {}
                            exec(compile(transport, 'asb_mini_swe_tools.py', 'exec'), namespace)
                            replay_result = namespace['AsbTapeEnvironment'](cwd=str(replica)).execute({'command': 'run tests'})
                    self.assertEqual(replay_result, result)
                    self.assertEqual((replica/'generated.png').read_bytes(), b'\x89PNG\x00\xff\x01')
                    replay.finish()
                finally:
                    replay_thread.join(timeout=3); replay_server.server_close(); replay_socket.unlink(missing_ok=True)

    def test_unavailable_boundary_prevents_tool_execution(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root = Path(directory)
            with patch.dict(os.environ, {'ASB_MINI_SWE_TOOL_TAPE': str(root/'tape'),
                                        'ASB_MINI_SWE_TOOL_MODE': 'record',
                                        'ASB_FS_MARKER_SOCKET': str(root/'missing.sock')}):
                namespace = {}
                exec(compile(transport, 'asb_mini_swe_tools.py', 'exec'), namespace)
                with self.assertRaisesRegex(RuntimeError, 'boundary is unavailable'):
                    namespace['AsbTapeEnvironment'](cwd=str(root)).execute({'command': 'run tests'})
            self.assertFalse((root/'generated.png').exists())


if __name__ == '__main__':
    unittest.main()
