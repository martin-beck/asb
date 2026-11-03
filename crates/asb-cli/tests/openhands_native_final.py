"""Offline regression for the native OpenHands launcher response boundary."""
import importlib.util
import io
import json
import pathlib
import sys
import threading
import time
import types
import unittest
from unittest.mock import patch


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
    def test_native_execution_remains_parallel_during_recording(self):
        terminal=types.ModuleType('openhands.tools.terminal')
        metadata=types.ModuleType('openhands.tools.terminal.metadata')
        terminal.TerminalObservation=Event
        metadata.CmdOutputMetadata=Event
        barrier=threading.Barrier(2,timeout=2)
        def delegate(action):
            barrier.wait()
            return Event(text='ok',is_error=False,command=action.command,exit_code=0,
                         timeout=False,metadata=Event(model_dump=lambda:{}))
        executor=launcher.NativeTerminalExecutor.__new__(launcher.NativeTerminalExecutor)
        executor._bridge_lock=threading.Lock();executor._delegate=delegate
        executor._bridge=Event(stdin=io.StringIO(),stdout=io.StringIO(
            '{"kind":"ack","schema_version":1}\n'*2))
        results=[]
        def call(command):
            results.append(executor(Event(command=command,is_input=False,timeout=5,reset=False)).command)
        with patch.dict(sys.modules,{'openhands.tools':types.ModuleType('openhands.tools'),
                                    'openhands.tools.terminal':terminal,
                                    'openhands.tools.terminal.metadata':metadata}):
            threads=[threading.Thread(target=call,args=(name,)) for name in ('first','second')]
            for thread in threads: thread.start()
            for thread in threads: thread.join()
        self.assertCountEqual(results,['first','second'])

    def test_parallel_callers_receive_their_own_bridge_observation(self):
        terminal=types.ModuleType('openhands.tools.terminal')
        metadata=types.ModuleType('openhands.tools.terminal.metadata')
        terminal.TerminalObservation=Event
        metadata.CmdOutputMetadata=Event
        class BridgeIO:
            def write(self, line):
                self.command=json.loads(line)['command']
                time.sleep(0.03)
            def flush(self): pass
            def readline(self):
                return json.dumps({'kind':'terminal_observation','schema_version':1,
                    'content':self.command,'is_error':False,'command':self.command,
                    'exit_code':0,'timeout':False,'metadata':{}})+'\n'
        io_pair=BridgeIO()
        executor=launcher.NativeTerminalExecutor.__new__(launcher.NativeTerminalExecutor)
        executor._bridge_lock=threading.Lock();executor._delegate=None
        executor._bridge=Event(stdin=io_pair,stdout=io_pair)
        results={}
        def call(command):
            results[command]=executor(Event(command=command,is_input=False,timeout=5,reset=False)).command
        with patch.dict(sys.modules,{'openhands.tools':types.ModuleType('openhands.tools'),
                                    'openhands.tools.terminal':terminal,
                                    'openhands.tools.terminal.metadata':metadata}):
            threads=[threading.Thread(target=call,args=(name,)) for name in ('first','second')]
            for thread in threads: thread.start()
            for thread in threads: thread.join()
        self.assertEqual(results,{'first':'first','second':'second'})

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
