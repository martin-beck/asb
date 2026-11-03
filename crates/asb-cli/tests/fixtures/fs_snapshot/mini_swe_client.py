"""Run the embedded Mini-SWE adapter around an offline fixture tool."""
import os
from pathlib import Path
import subprocess
import sys
import types

source = Path('/opt/mini_swe.rs').read_text()
transport = source.split('const TOOL_TRANSPORT: &str = r#"', 1)[1].split('"#;', 1)[0]

class LocalEnvironment:
    def __init__(self, **kwargs):
        self.config = types.SimpleNamespace(cwd=kwargs['cwd'])
    def execute(self, action, cwd='', *, timeout=None):
        subprocess.run(['python3', '-c',
                        'from pathlib import Path; Path("trial-result").write_bytes(b"fixed"); '
                        'Path("/work/adapter-invocations").open("ab").write(b"1")'],
                       cwd=cwd or self.config.cwd, check=True)
        return {'returncode': 0, 'output': 'done'}

class Submitted(Exception):
    pass

sys.modules['minisweagent'] = types.ModuleType('minisweagent')
sys.modules['minisweagent.environments'] = types.ModuleType('minisweagent.environments')
local = types.ModuleType('minisweagent.environments.local')
local.LocalEnvironment = LocalEnvironment
sys.modules[local.__name__] = local
exceptions = types.ModuleType('minisweagent.exceptions')
exceptions.Submitted = Submitted
sys.modules[exceptions.__name__] = exceptions
namespace = {}
exec(compile(transport, 'asb_mini_swe_tools.py', 'exec'), namespace)
result = namespace['AsbTapeEnvironment'](cwd=os.environ['ASB_FIXTURE_WORKSPACE']).execute({'command': 'fixture tool'})
if result != {'returncode': 0, 'output': 'done'}:
    raise RuntimeError('fixture tool response differs')
