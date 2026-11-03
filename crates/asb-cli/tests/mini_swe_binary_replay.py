"""Exercise the actual embedded mini-SWE adapter with binary tool effects."""
import json
from pathlib import Path
import sys
import tempfile
import types
import unittest

source = (Path(__file__).parents[1] / 'src/agents/adapters/mini_swe.rs').read_text()
transport = source.split('const TOOL_TRANSPORT: &str = r#"', 1)[1].split('"#;', 1)[0]


class Submitted(Exception):
    def __init__(self, *messages):
        super().__init__(*messages)
        self.messages = messages


class LocalEnvironment:
    def __init__(self, **kwargs):
        self.config = types.SimpleNamespace(cwd=kwargs['cwd'])

    def execute(self, action, cwd='', *, timeout=None):
        root = Path(cwd or self.config.cwd)
        (root / 'generated.png').write_bytes(b'\x89PNG\x00\xff\x01')
        (root / 'result.txt').write_text('fixed\n')
        return {'returncode': 0, 'output': 'done'}


sys.modules['minisweagent'] = types.ModuleType('minisweagent')
sys.modules['minisweagent.environments'] = types.ModuleType('minisweagent.environments')
local = types.ModuleType('minisweagent.environments.local')
local.LocalEnvironment = LocalEnvironment
sys.modules[local.__name__] = local
exceptions = types.ModuleType('minisweagent.exceptions')
exceptions.Submitted = Submitted
sys.modules[exceptions.__name__] = exceptions


class BinaryReplayTests(unittest.TestCase):
    def test_filesystem_markers_use_the_recorder_socket(self):
        import os
        import socket
        import threading
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = str(root / 'markers.sock')
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(path)
            listener.listen(2)
            messages = []
            def serve():
                for sequence in (1, 2):
                    with listener.accept()[0] as connection:
                        messages.append(connection.recv(1024).decode('ascii').strip())
                        connection.sendall(f'{sequence}\n'.encode('ascii'))
            thread = threading.Thread(target=serve, daemon=True)
            thread.start()
            env = {'ASB_MINI_SWE_TOOL_TAPE': str(root / 'tape.jsonl'),
                   'ASB_MINI_SWE_TOOL_MODE': 'record', 'ASB_FS_MARKER_SOCKET': path}
            try:
                with patch.dict(os.environ, env):
                    namespace = {}
                    exec(compile(transport, 'asb_mini_swe_tools.py', 'exec'), namespace)
                    namespace['AsbTapeEnvironment'](cwd=str(root)).execute({'command': 'run tests'})
                thread.join(timeout=5)
                self.assertFalse(thread.is_alive())
                self.assertEqual(len(messages), 2)
                self.assertRegex(messages[0], r'^tool-begin:mini-swe-1:\d+:\d+$')
                self.assertRegex(messages[1], r'^tool-end:mini-swe-1:\d+:\d+$')
            finally:
                listener.close()

    def test_filesystem_journal_boundaries_keep_the_real_tool_and_no_snapshot_effects(self):
        import os
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tape = root / 'tape.jsonl'
            env = {'ASB_MINI_SWE_TOOL_TAPE': str(tape), 'ASB_MINI_SWE_TOOL_MODE': 'record',
                   'ASB_FS_MARKER_SOCKET': str(root / 'markers.sock')}
            with patch.dict(os.environ, env):
                namespace = {}
                exec(compile(transport, 'asb_mini_swe_tools.py', 'exec'), namespace)
                markers = []
                namespace['_filesystem_marker'] = lambda action, alias: markers.append((action, alias)) or True
                agent = namespace['AsbTapeEnvironment'](cwd=str(root))
                agent._snapshot = lambda *_: self.fail('filesystem capture must not snapshot')
                self.assertEqual(agent.execute({'command': 'run tests'})['output'], 'done')
                self.assertEqual(markers, [('begin', 'mini-swe-1'), ('end', 'mini-swe-1')])
                self.assertEqual((root / 'result.txt').read_text(), 'fixed\n')
                event = json.loads(tape.read_text())
                self.assertNotIn('effects', event)
                self.assertEqual(event['request']['command'], 'run tests')

    def test_failed_marker_preserves_tool_execution_and_records_failure(self):
        import os
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tape = root / 'tape.jsonl'
            env = {'ASB_MINI_SWE_TOOL_TAPE': str(tape), 'ASB_MINI_SWE_TOOL_MODE': 'record',
                   'ASB_FS_MARKER_SOCKET': str(root / 'missing.sock')}
            with patch.dict(os.environ, env):
                namespace = {}
                exec(compile(transport, 'asb_mini_swe_tools.py', 'exec'), namespace)
                agent = namespace['AsbTapeEnvironment'](cwd=str(root))
                self.assertEqual(agent.execute({'command': 'run tests'})['output'], 'done')
                self.assertTrue((root / 'result.txt').is_file())
                event = json.loads(tape.read_text())
                self.assertEqual(event['failure'], 'Mini-SWE filesystem operation boundary is unavailable')
                self.assertIsNone(event['result'])

    def test_submission_still_closes_the_filesystem_boundary(self):
        import os
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tape = root / 'tape.jsonl'
            env = {'ASB_MINI_SWE_TOOL_TAPE': str(tape), 'ASB_MINI_SWE_TOOL_MODE': 'record',
                   'ASB_FS_MARKER_SOCKET': str(root / 'markers.sock')}
            with patch.dict(os.environ, env):
                namespace = {}
                exec(compile(transport, 'asb_mini_swe_tools.py', 'exec'), namespace)
                markers = []
                namespace['_filesystem_marker'] = lambda action, alias: markers.append(action) or True
                with patch.object(LocalEnvironment, 'execute', side_effect=Submitted('finished')):
                    with self.assertRaises(Submitted):
                        namespace['AsbTapeEnvironment'](cwd=str(root)).execute({'command': 'submit'})
                self.assertEqual(markers, ['begin', 'end'])
                self.assertEqual(json.loads(tape.read_text())['result'], {'submitted': ['finished']})

    def test_binary_and_text_changes_replay_without_executing_command(self):
        import os
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            recorded = root / 'recorded'
            replayed = root / 'replayed'
            recorded.mkdir()
            replayed.mkdir()
            tape = root / 'tape.jsonl'
            os.environ['ASB_MINI_SWE_TOOL_TAPE'] = str(tape)
            os.environ['ASB_MINI_SWE_TOOL_MODE'] = 'record'
            namespace = {}
            exec(compile(transport, 'asb_mini_swe_tools.py', 'exec'), namespace)
            result = namespace['AsbTapeEnvironment'](cwd=str(recorded)).execute({'command': 'run tests'})
            self.assertEqual(result['returncode'], 0)
            entry = json.loads(tape.read_text())
            self.assertIn('write_binary', [effect['kind'] for effect in entry['effects']])
            os.environ['ASB_MINI_SWE_TOOL_MODE'] = 'replay'
            namespace = {}
            exec(compile(transport, 'asb_mini_swe_tools.py', 'exec'), namespace)
            result = namespace['AsbTapeEnvironment'](cwd=str(replayed)).execute({'command': 'run tests'})
            self.assertEqual(result['returncode'], 0)
            self.assertEqual((replayed / 'generated.png').read_bytes(), b'\x89PNG\x00\xff\x01')
            self.assertEqual((replayed / 'result.txt').read_text(), 'fixed\n')


if __name__ == '__main__':
    unittest.main()
