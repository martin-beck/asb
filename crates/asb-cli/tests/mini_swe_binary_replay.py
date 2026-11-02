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
    pass


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
