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
        self.config = types.SimpleNamespace(cwd=kwargs['cwd'], timeout=kwargs.get('timeout', 30))

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
    def test_more_than_128_tool_calls_record_and_replay(self):
        import os
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);tape=root/'tape.jsonl'
            with patch.dict(os.environ, {'ASB_MINI_SWE_TOOL_TAPE':str(tape), 'ASB_MINI_SWE_TOOL_MODE':'record'}, clear=True):
                namespace={};exec(compile(transport,'asb_mini_swe_tools.py','exec'),namespace)
                recorder=namespace['AsbTapeEnvironment'](cwd=str(root))
                for index in range(129):
                    recorder._record({'command':f'command-{index}', 'cwd':'', 'timeout':None, 'effective_timeout':30},{'returncode':0,'output':'done'},[])
            with patch.dict(os.environ, {'ASB_MINI_SWE_TOOL_TAPE':str(tape), 'ASB_MINI_SWE_TOOL_MODE':'replay'}, clear=True):
                namespace={};exec(compile(transport,'asb_mini_swe_tools.py','exec'),namespace)
                replay=namespace['AsbTapeEnvironment'](cwd=str(root))
                for index in range(129):
                    self.assertEqual(replay.execute({'command':f'command-{index}'})['output'],'done')
            self.assertFalse((root/'generated.png').exists())

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
            self.assertEqual(entry['request'], {'command':'run tests', 'cwd':'', 'timeout':None, 'effective_timeout':30})
            self.assertIn('write_binary', [effect['kind'] for effect in entry['effects']])
            os.environ['ASB_MINI_SWE_TOOL_MODE'] = 'replay'
            namespace = {}
            exec(compile(transport, 'asb_mini_swe_tools.py', 'exec'), namespace)
            result = namespace['AsbTapeEnvironment'](cwd=str(replayed)).execute({'command': 'run tests'})
            self.assertEqual(result['returncode'], 0)
            self.assertEqual((replayed / 'generated.png').read_bytes(), b'\x89PNG\x00\xff\x01')
            self.assertEqual((replayed / 'result.txt').read_text(), 'fixed\n')

    def test_replay_rejects_different_cwd_or_timeout(self):
        import os
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); tape=root/'tape.jsonl'
            request={'command':'run tests','cwd':'','timeout':12,'effective_timeout':12}
            tape.write_text(json.dumps({'schema_version':1,'sequence':1,'tool':'mini-swe.bash',
                                        'request':request,'result':{'returncode':0,'output':'done'}})+'\n')
            with patch.dict(os.environ, {'ASB_MINI_SWE_TOOL_TAPE':str(tape), 'ASB_MINI_SWE_TOOL_MODE':'replay'}, clear=True):
                namespace={};exec(compile(transport,'asb_mini_swe_tools.py','exec'),namespace)
                environment=namespace['AsbTapeEnvironment']
                for cwd,timeout in [('other',12),('',13),('',None)]:
                    with self.subTest(cwd=cwd,timeout=timeout):
                        with self.assertRaisesRegex(RuntimeError,'differs from the recording'):
                            environment(cwd=str(root)).execute({'command':'run tests'},cwd=cwd,timeout=timeout)
                self.assertEqual(environment(cwd=str(root)).execute({'command':'run tests'},cwd='',timeout=12)['output'],'done')
                tape.write_text(json.dumps({'schema_version':1,'sequence':1,'tool':'mini-swe.bash',
                                            'request':{'command':'run tests'},'result':{'returncode':0,'output':'done'}})+'\n')
                with self.assertRaisesRegex(RuntimeError,'differs from the recording'):
                    environment(cwd=str(root)).execute({'command':'run tests'},cwd='',timeout=12)

    def test_replay_rejects_changed_configured_default_timeout(self):
        import os
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); tape=root/'tape.jsonl'
            request={'command':'run tests','cwd':'','timeout':None,'effective_timeout':30}
            tape.write_text(json.dumps({'schema_version':1,'sequence':1,'tool':'mini-swe.bash',
                                        'request':request,'result':{'returncode':0,'output':'done'}})+'\n')
            with patch.dict(os.environ, {'ASB_MINI_SWE_TOOL_TAPE':str(tape), 'ASB_MINI_SWE_TOOL_MODE':'replay'}, clear=True):
                namespace={};exec(compile(transport,'asb_mini_swe_tools.py','exec'),namespace)
                environment=namespace['AsbTapeEnvironment']
                self.assertEqual(environment(cwd=str(root),timeout=30).execute({'command':'run tests'})['output'],'done')
                with self.assertRaisesRegex(RuntimeError,'execution context differs'):
                    environment(cwd=str(root),timeout=60).execute({'command':'run tests'})


if __name__ == '__main__':
    unittest.main()
