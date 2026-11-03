import base64
import importlib.util
import json
import os
from pathlib import Path
import model_tools
import tools.code_execution_tool
from tools.registry import registry

hook = Path(__file__).resolve().parents[2] / 'assets/scripts/hermes-native-tools.py'
spec = importlib.util.spec_from_file_location('asb_hermes_actual', hook)
module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
root = Path(os.environ['ASB_HERMES_NATIVE_SIDECAR']).parent
parent = root / 'parent-sentinel'; child = root / 'child-sentinel'
code = f'from pathlib import Path\nfrom hermes_tools import terminal\nPath({str(parent)!r}).write_text("parent")\nprint(terminal(command="touch {child}"))'
request = {'code': code}
recorded = registry.dispatch('execute_code', request, task_id='fixture', enabled_tools={'terminal'})
module.transport.close()
assert parent.exists() and child.exists(), 'live fixture did not execute both operations'
parent.unlink(); child.unlink()
rows = [json.loads(line) for line in (root/'native.jsonl').read_text().splitlines()]
requests = [json.loads(base64.b64decode(row['request_b64'])) for row in rows if row['event']=='write']
assert len(requests) == 2 and requests[1]['parent_alias'] == 'terminal-1'
module.MODE = 'replay'
replayer = module.Replayer()
module.install_dispatch_coverage(registry, replayer, 'replay')
replayed = registry.dispatch('execute_code', request, task_id='fixture', enabled_tools={'terminal'})
replayer.close()
assert replayed == recorded and not parent.exists() and not child.exists()
assert json.loads((root/'progress.json').read_text())['complete']
print('Real Hermes code and nested terminal replay did not execute either sentinel')
