"""Run the installed Hermes terminal tool with the real ASB sidecar hook."""
import json
import os
from pathlib import Path
from tools.terminal_tool import terminal_tool

sentinel = Path('/tmp/asb-native-tool-executed')
assert not sentinel.exists()
command = 'printf hermes > hermes-tool-output; printf ran > /tmp/asb-native-tool-executed'
result = json.loads(terminal_tool(command, workdir='/work/mount'))
assert result['exit_code'] == 0
assert Path('/work/mount/hermes-tool-output').read_text() == 'hermes'
assert sentinel.exists() == (os.environ['ASB_HERMES_NATIVE_MODE'] == 'record')
