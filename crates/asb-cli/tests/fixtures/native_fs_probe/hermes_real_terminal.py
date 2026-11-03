"""Run two real Hermes terminal calls with experimental effect boundaries."""
import json
import os
from pathlib import Path
import socket
import sitecustomize
from tools.terminal_tool import terminal_tool

mode = os.environ['ASB_HERMES_NATIVE_MODE']
control = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
control.connect(os.environ['ASB_FIXTURE_EFFECT_SOCKET'])
stream = control.makefile('rwb', buffering=0)
original = sitecustomize.transport.invoke
ordinal = 0

def exchange(marker):
    stream.write((marker + '\n').encode('ascii'))
    if not stream.readline().strip().isdigit():
        raise RuntimeError('fixture effect boundary failed')

def invoke(*args, **kwargs):
    global ordinal
    name = ('one', 'two')[ordinal]
    ordinal += 1
    if mode == 'record':
        exchange('tool-begin:' + name)
    result = original(*args, **kwargs)
    if mode == 'replay' and sitecustomize.transport.failed:
        raise RuntimeError('recorded Hermes request did not match')
    exchange('tool-end:' + name)
    return result

sitecustomize.transport.invoke = invoke
try:
    for name, command in (
        ('one', 'printf first > first; printf ran > /tmp/asb-native-tool-one'),
        ('two', 'test "$(cat first)" = first && printf second > second; printf ran > /tmp/asb-native-tool-two'),
    ):
        sentinel = Path('/tmp/asb-native-tool-' + name)
        assert not sentinel.exists()
        if name == 'one':
            assert not Path('second').exists()
        result = json.loads(terminal_tool(command, workdir='/work/mount'))
        assert result['exit_code'] == 0
        assert Path('first').read_text() == 'first'
        assert Path('second').exists() == (name == 'two')
        if name == 'two':
            assert Path('second').read_text() == 'second'
        assert sentinel.exists() == (mode == 'record')
finally:
    stream.close()
    control.close()
