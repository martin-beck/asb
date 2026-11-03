"""Offline regression for the native OpenHands launcher response boundary."""
import importlib.util
import io
import json
import os
import pathlib
import sys
import threading
import types
import unittest
from unittest.mock import MagicMock, patch


def module(name):
    value = types.ModuleType(name)
    sys.modules[name] = value
    return value

for name in ['openhands', 'openhands.sdk', 'openhands.sdk.tool',
             'openhands.sdk.event', 'openhands.sdk.event.llm_convertible',
             'openhands.sdk.event.llm_convertible.message',
             'openhands.sdk.event.llm_convertible.action',
             'openhands.sdk.event.llm_convertible.observation',
             'openhands.sdk.event.conversation_error',
             'openhands.sdk.tool.builtins', 'openhands.sdk.tool.builtins.finish',
             'openhands.sdk.conversation', 'openhands.sdk.conversation.state']:
    module(name)

class Event:
    def __init__(self, **fields): self.__dict__.update(fields)
class MessageEvent(Event): pass
class ActionEvent(Event): pass
class UserRejectObservation(Event): pass
class AgentErrorEvent(Event): pass
class ConversationErrorEvent(Event): pass
class FinishAction(Event): pass
class TextContent(Event): pass
class Status:
    FINISHED = 'finished'
    RUNNING = 'running'

sys.modules['openhands.sdk.tool'].ToolExecutor = type('ToolExecutor', (), {})
sys.modules['openhands.sdk'].TextContent = TextContent
sys.modules['openhands.sdk.event.llm_convertible.message'].MessageEvent = MessageEvent
sys.modules['openhands.sdk.event.llm_convertible.action'].ActionEvent = ActionEvent
sys.modules['openhands.sdk.event.llm_convertible.observation'].UserRejectObservation = UserRejectObservation
sys.modules['openhands.sdk.event.llm_convertible.observation'].AgentErrorEvent = AgentErrorEvent
sys.modules['openhands.sdk.event.conversation_error'].ConversationErrorEvent = ConversationErrorEvent
sys.modules['openhands.sdk.tool.builtins.finish'].FinishAction = FinishAction
sys.modules['openhands.sdk.conversation.state'].ConversationExecutionStatus = Status

path = pathlib.Path(__file__).resolve().parents[1] / 'assets/scripts/openhands-sdk-launcher.py'
spec = importlib.util.spec_from_file_location('launcher', path)
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)

def conversation(events, status=Status.FINISHED):
    return Event(state=Event(events=events, execution_status=status))

def message(source, text):
    return MessageEvent(source=source, llm_message=Event(role='assistant' if source == 'agent' else 'user', content=[TextContent(text=text)]))

class NativeFinalTest(unittest.TestCase):
    def test_terminal_record_brackets_the_actual_tool_execution(self):
        terminal=types.ModuleType('openhands.tools.terminal')
        metadata=types.ModuleType('openhands.tools.terminal.metadata')
        terminal.TerminalObservation=Event
        metadata.CmdOutputMetadata=Event
        timeline=[]
        observation=Event(text='ok',is_error=False,command='edit',exit_code=0,
                          timeout=False,metadata=Event(model_dump=lambda:{}))
        executor=launcher.NativeTerminalExecutor.__new__(launcher.NativeTerminalExecutor)
        executor._sequence=0;executor._sequence_lock=threading.Lock()
        executor._delegate=lambda action:(timeline.append('execute') or observation)
        executor._bridge=Event(stdin=io.StringIO(),stdout=io.StringIO('{"kind":"ok","schema_version":1}\n'))
        action=Event(command='edit',is_input=False,timeout=5,reset=False)
        with patch.dict(sys.modules,{'openhands.tools':types.ModuleType('openhands.tools'),
                                    'openhands.tools.terminal':terminal,
                                    'openhands.tools.terminal.metadata':metadata}), \
             patch.object(launcher,'snapshot_boundary',side_effect=lambda mode,action,alias:timeline.append(action)):
            self.assertIs(executor(action),observation)
        self.assertEqual(timeline,['begin','execute','end'])

    def test_terminal_replay_applies_snapshot_before_returning_observation(self):
        terminal=types.ModuleType('openhands.tools.terminal')
        metadata=types.ModuleType('openhands.tools.terminal.metadata')
        terminal.TerminalObservation=Event
        metadata.CmdOutputMetadata=Event
        action=Event(command='edit',is_input=False,timeout=5,reset=False)
        response={'kind':'terminal_observation','schema_version':1,'content':'ok',
                  'is_error':False,'command':'edit','exit_code':0,'timeout':False,'metadata':{}}
        executor=launcher.NativeTerminalExecutor.__new__(launcher.NativeTerminalExecutor)
        executor._sequence=0;executor._sequence_lock=threading.Lock();executor._delegate=None
        executor._bridge=Event(stdin=io.StringIO(),stdout=io.StringIO(json.dumps(response)+'\n'))
        with patch.dict(sys.modules,{'openhands.tools':types.ModuleType('openhands.tools'),
                                    'openhands.tools.terminal':terminal,
                                    'openhands.tools.terminal.metadata':metadata}), \
             patch.object(launcher,'snapshot_boundary') as boundary:
            result=executor(action)
        boundary.assert_called_once_with('replay','end','openhands-1')
        self.assertEqual(result.exit_code,0)
        self.assertEqual(json.loads(executor._bridge.stdin.getvalue())['command'],'edit')

    def test_optional_snapshot_boundaries_use_adapter_alias_and_fail_on_rejection(self):
        with patch.dict(os.environ, {}, clear=True), patch.object(launcher.socket, 'socket') as factory:
            launcher.snapshot_boundary('record', 'begin', 'openhands-1')
            factory.assert_not_called()
        connection=MagicMock()
        connection.__enter__.return_value=connection
        connection.makefile.return_value=io.BytesIO(b'2\n')
        with patch.dict(os.environ, {'ASB_FS_REPLAY_SOCKET':'/tmp/fixture'}, clear=True), \
             patch.object(launcher.socket, 'socket', return_value=connection):
            launcher.snapshot_boundary('replay','end','openhands-1')
        connection.connect.assert_called_once_with('/tmp/fixture')
        connection.sendall.assert_called_once_with(b'tool-end:openhands-1\n')
        connection.makefile.return_value=io.BytesIO(b'')
        with patch.dict(os.environ, {'ASB_FS_MARKER_SOCKET':'/tmp/fixture'}, clear=True), \
             patch.object(launcher.socket, 'socket', return_value=connection), \
             self.assertRaisesRegex(RuntimeError, 'rejected'):
            launcher.snapshot_boundary('record','begin','openhands-1')

    def test_completed_message_and_finish_action(self):
        self.assertEqual(launcher.final_agent_text(conversation([message('agent', 'answer')])), 'answer')
        finish = ActionEvent(source='agent', tool_name='finish', action=FinishAction(message='done'))
        self.assertEqual(launcher.final_agent_text(conversation([finish])), 'done')

    def test_no_fabricated_or_stale_answer(self):
        answer = message('agent', 'stale')
        for events, status in [([], Status.FINISHED), ([answer], Status.RUNNING),
                ([answer, message('user', 'continue')], Status.FINISHED),
                ([answer, message('environment', 'stop hook feedback')], Status.FINISHED),
                ([answer, UserRejectObservation()], Status.FINISHED),
                ([answer, AgentErrorEvent()], Status.FINISHED),
                ([answer, ConversationErrorEvent()], Status.FINISHED),
                ([answer, ActionEvent(source='agent', tool_name='terminal', action=Event())], Status.FINISHED)]:
            with self.subTest(events=events, status=status):
                self.assertIsNone(launcher.final_agent_text(conversation(events, status)))

if __name__ == '__main__':
    if sys.argv[1:] == ['--emit-empty-result']:
        print(json.dumps({'type': 'result', 'text': launcher.final_agent_text(conversation([]))}))
    else:
        unittest.main()
