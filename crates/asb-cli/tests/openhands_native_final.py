"""Offline regression for the native OpenHands launcher response boundary."""
import importlib.util
import io
import json
import pathlib
import os
import socket
import sys
import tempfile
import types
import threading
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
    def test_tracker_mismatch_restores_persistence_and_does_not_acknowledge(self):
        class Action:
            def model_dump(self, mode): return {'command': 'view'}
        class Observation:
            def model_dump(self, mode): return {'state': 'different'}
        class Delegate:
            save_dir = str(pathlib.Path.cwd() / 'fixture')
            def __call__(self, action, conversation=None):
                assert self.save_dir is None
                return Observation()
            def close(self): pass
        with tempfile.TemporaryDirectory() as directory:
            sidecar = pathlib.Path(directory) / 'tape'
            sidecar.write_text(json.dumps({'schema_version': 1, 'sequence': 1,
                'tool': 'openhands.task_tracker', 'request': {'command': 'view'},
                'result': {'state': 'expected'}}) + '\n')
            delegate = Delegate()
            tape = launcher.EditorTape('replay', str(sidecar))
            with patch.object(launcher, 'filesystem_replay_begin', return_value=True), \
                    patch.object(launcher, 'filesystem_replay_boundary') as finish:
                with self.assertRaisesRegex(RuntimeError, 'result differs'):
                    launcher.TaskTrackerTapeExecutor(delegate, tape)(Action())
                finish.assert_not_called()
            self.assertEqual(delegate.save_dir, str(pathlib.Path.cwd() / 'fixture'))

    def test_delegated_result_mismatch_does_not_acknowledge_completion(self):
        class Action:
            def model_dump(self, mode): return {'task':'fixture'}
        class Observation:
            def model_dump(self, mode): return {'result':'different'}
        class Delegate:
            def __call__(self, action, conversation=None): return Observation()
            def close(self): pass
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory)/'tape'
            path.write_text(json.dumps({'schema_version':1,'sequence':1,'tool':'openhands.task',
                                        'request':{'task':'fixture'},'result':{'result':'expected'}})+'\n')
            tape = launcher.EditorTape('replay', str(path))
            with patch.object(launcher, 'filesystem_replay_begin', return_value=True), \
                    patch.object(launcher, 'filesystem_replay_boundary') as finish:
                with self.assertRaisesRegex(RuntimeError, 'result differs'):
                    launcher.DelegationTapeExecutor(Delegate(), tape)(Action())
                finish.assert_not_called()

    def test_delegated_editor_replay_uses_real_nested_coordinator(self):
        sys.path.insert(0, str(pathlib.Path(__file__).parents[1] / 'assets/benchmarks'))
        from asb_fs_replay_server import ReplayServer
        definition = module('openhands.tools.file_editor.definition')
        class Observation:
            def __init__(self, value): self.value = value
            def model_dump(self, mode): return {'value':self.value}
            @classmethod
            def model_validate(cls, value): return cls(value['value'])
        definition.FileEditorObservation = Observation
        class Action:
            def __init__(self, value): self.value = value
            def model_dump(self, mode): return {'value':self.value}
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory); evidence = root/'evidence'; evidence.mkdir()
            workspace = root/'workspace'; workspace.mkdir()
            (evidence/'payload.bin').write_bytes(b'child effect')
            events = [
                {'sequence':1,'op':'marker','path':None,'marker':'tool-begin:openhands-task-1'},
                {'sequence':2,'op':'marker','path':None,'marker':'tool-begin:openhands-editor-2'},
                {'sequence':3,'op':'create','path':'/result','handle':1,'flags':1,'mode':0o600},
                {'sequence':4,'op':'write','path':'/result','handle':1,'offset':0,'bytes':12,'object':'payload.bin','payload_offset':0},
                {'sequence':5,'op':'release','path':'/result','handle':1},
                {'sequence':6,'op':'marker','path':None,'marker':'tool-end:openhands-editor-2'},
                {'sequence':7,'op':'marker','path':None,'marker':'tool-end:openhands-task-1'},
            ]
            (evidence/'events.jsonl').write_text(''.join(json.dumps(event)+'\n' for event in events))
            entries = [
                {'schema_version':1,'sequence':1,'tool':'openhands.task','request':{'value':'parent'},'result':{'value':'parent-result'}},
                {'schema_version':1,'sequence':2,'tool':'openhands.file_editor','request':{'value':'child'},'result':{'value':'child-result'}},
            ]
            sidecar = root/'tools.jsonl'; sidecar.write_text(''.join(json.dumps(entry)+'\n' for entry in entries))
            tape = launcher.EditorTape('replay', str(sidecar))
            class Editor:
                def __call__(self, *_args, **_kwargs): raise AssertionError('editor executed during replay')
                def close(self): pass
            editor = launcher.FileEditorTapeExecutor(Editor(), tape)
            class Worker:
                def __call__(self, action, conversation=None):
                    assert editor(Action('child')).value == 'child-result'
                    assert (workspace/'result').read_bytes() == b'child effect'
                    return Observation('parent-result')
                def close(self): pass
            server = ReplayServer(evidence, workspace, root/'socket', timeout=2)
            with patch.dict(os.environ, {'ASB_FS_REPLAY_SOCKET':str(root/'socket')}):
                result = launcher.DelegationTapeExecutor(Worker(), tape)(Action('parent'))
            self.assertEqual(result.value, 'parent-result')
            tape.close(); server.finish()

    def test_delegated_task_runs_in_both_modes_and_orders_nested_editor(self):
        definition = module('openhands.tools.file_editor.definition')
        class Observation:
            def __init__(self, value): self.value = value
            def model_dump(self, mode): return {'value': self.value}
            @classmethod
            def model_validate(cls, value): return cls(value['value'])
        definition.FileEditorObservation = Observation
        class Action:
            def __init__(self, value): self.value = value
            def model_dump(self, mode): return {'value':self.value}
        class Editor:
            def __call__(self, action, conversation=None): return Observation('child-result')
            def close(self): pass
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory)/'tape'; path.touch()
            boundaries = []; calls = []
            def exercise(mode):
                tape = launcher.EditorTape(mode, str(path))
                child = launcher.FileEditorTapeExecutor(Editor(), tape)
                class Task:
                    def __call__(self, action, conversation=None):
                        calls.append(mode)
                        self_child = child(Action('child'))
                        assert self_child.value == 'child-result'
                        return Observation('parent-result')
                    def close(self): pass
                task = launcher.DelegationTapeExecutor(Task(), tape)
                with patch.dict(os.environ, {'ASB_FS_MARKER_SOCKET':'/fixture', 'ASB_FS_REPLAY_SOCKET':'/fixture'}), \
                        patch.object(launcher, 'filesystem_marker', side_effect=lambda kind, alias: boundaries.append((mode, kind, alias)) or True), \
                        patch.object(launcher, 'filesystem_replay_begin', side_effect=lambda alias: boundaries.append((mode, 'begin', alias)) or True), \
                        patch.object(launcher, 'filesystem_replay_boundary', side_effect=lambda alias: boundaries.append((mode, 'end', alias)) or True):
                    self.assertEqual(task(Action('parent')).value, 'parent-result')
                tape.close()
            exercise('record'); exercise('replay')
            self.assertEqual(calls, ['record', 'replay'])
            expected = [('begin','openhands-task-1'), ('begin','openhands-editor-2'),
                        ('end','openhands-editor-2'), ('end','openhands-task-1')]
            self.assertEqual([(kind, alias) for mode,kind,alias in boundaries if mode=='record'], expected)
            self.assertEqual([(kind, alias) for mode,kind,alias in boundaries if mode=='replay'],
                             [('begin','openhands-task-1'),('end','openhands-editor-2'),('end','openhands-task-1')])

    def test_subagent_terminal_borrows_parent_transport_without_closing_it(self):
        task = module('openhands.tools.task'); manager = module('openhands.tools.task.manager')
        task.manager = manager
        terminal = sys.modules.get('openhands.tools.terminal') or module('openhands.tools.terminal')
        terminal.TerminalTool = type('TerminalTool', (), {'name':'terminal'})
        sys.modules['openhands.sdk'].Tool = lambda name, params: Event(name=name, params=params)
        other = Event(name='file_editor', params={})
        agent = Event(tools=[Event(name='terminal', params={}), other])
        agent.model_copy = lambda update: Event(**update)
        manager.LocalConversation = lambda *args, **kwargs: Event(agent=agent)
        calls = []
        class Transport:
            def __call__(self, action, conversation=None): calls.append(action); return 'ok'
            def close(self): self.fail('child closed parent transport')
        transport = Transport()
        launcher.install_subagent_terminal(transport)
        conversation = manager.LocalConversation()
        self.assertIs(conversation.agent.tools[1], other)
        shared = conversation.agent.tools[0].params['executor']
        self.assertEqual(shared('action'), 'ok')
        shared.close()
        self.assertEqual(calls, ['action'])

    def test_editor_only_recording_does_not_create_empty_terminal_sidecar(self):
        implementation = module('openhands.tools.terminal.impl')
        class Delegate:
            def __init__(self, **_kwargs): pass
            def close(self): pass
        implementation.TerminalExecutor = Delegate
        with patch.object(launcher.subprocess, 'Popen') as spawn:
            executor = launcher.NativeTerminalExecutor('/bridge', 'record', '/sidecar', '')
            executor.close()
            spawn.assert_not_called()

    def test_unrecorded_default_tool_still_runs_but_cannot_claim_complete_recording(self):
        class Delegate:
            def __call__(self, value): return 'ran:' + value
            def close(self): pass
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / 'editor.jsonl'; path.touch()
            tape = launcher.EditorTape('record', str(path))
            executor = launcher.UnrecordedToolExecutor(Delegate(), tape)
            self.assertEqual(executor('task'), 'ran:task')
            with self.assertRaisesRegex(RuntimeError, 'recording did not settle'):
                tape.close()
            replay = launcher.EditorTape('replay', str(path))
            with self.assertRaisesRegex(RuntimeError, 'no recorded replay transport'):
                launcher.UnrecordedToolExecutor(Delegate(), replay)('task')

    def test_editor_transcript_preserves_start_order_for_overlapping_calls(self):
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / 'editor.jsonl'; path.touch()
            tape = launcher.EditorTape('record', str(path))
            one, two = tape.reserve(), tape.reserve()
            tape.finish_record(two, {'call': 2}, {'result': 2})
            self.assertEqual(path.read_text(), '')
            tape.finish_record(one, {'call': 1}, {'result': 1})
            tape.close()
            self.assertEqual([json.loads(line)['sequence'] for line in path.read_text().splitlines()], [1, 2])

    def test_editor_recording_without_filesystem_capture_fails_after_real_call(self):
        definition = module('openhands.tools.file_editor.definition')
        definition.FileEditorObservation = Event
        class Delegate:
            def __call__(self, action, conversation=None): return Event(model_dump=lambda mode: {'result':'ok'})
            def close(self): pass
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / 'editor.jsonl'; path.touch()
            tape = launcher.EditorTape('record', str(path))
            executor = launcher.FileEditorTapeExecutor(Delegate(), tape)
            action = Event(model_dump=lambda mode: {'command':'edit'})
            with patch.dict(os.environ, {}, clear=True):
                self.assertIsInstance(executor(action), Event)
            with self.assertRaisesRegex(RuntimeError, 'recording did not settle'):
                tape.close()

    def test_editor_serialization_failure_does_not_skip_the_real_call(self):
        definition = sys.modules.get('openhands.tools.file_editor.definition') or module('openhands.tools.file_editor.definition')
        definition.FileEditorObservation = Event
        called = []
        class Delegate:
            def __call__(self, action, conversation=None):
                called.append(True)
                return Event(model_dump=lambda mode: {'result':'ok'})
            def close(self): pass
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / 'editor.jsonl'; path.touch()
            tape = launcher.EditorTape('record', str(path))
            executor = launcher.FileEditorTapeExecutor(Delegate(), tape)
            def broken(mode): raise ValueError('cannot serialize')
            action = Event(model_dump=broken)
            with patch.dict(os.environ, {'ASB_FS_MARKER_SOCKET':'/fixture'}), \
                    patch.object(launcher, 'filesystem_marker', return_value=True):
                self.assertIsInstance(executor(action), Event)
            self.assertEqual(called, [True])
            with self.assertRaisesRegex(RuntimeError, 'recording did not settle'):
                tape.close()

    def test_editor_record_and_replay_preserve_tool_and_apply_effect_before_response(self):
        definition = module('openhands.tools.file_editor.definition')
        class EditorObservation:
            def __init__(self, content): self.content = content
            def model_dump(self, mode): return {'content': self.content}
            @classmethod
            def model_validate(cls, value): return cls(value['content'])
        definition.FileEditorObservation = EditorObservation
        class Action:
            def __init__(self, value): self.value = value
            def model_dump(self, mode): return {'command': 'edit', 'value': self.value}
        class Delegate:
            def __init__(self): self.calls = 0
            def __call__(self, action, conversation=None):
                self.calls += 1
                return EditorObservation('edited')
            def close(self): pass
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / 'editor.jsonl'
            tape = launcher.EditorTape('record', str(path)); delegate = Delegate()
            executor = launcher.FileEditorTapeExecutor(delegate, tape)
            markers = []
            with patch.dict(os.environ, {'ASB_FS_MARKER_SOCKET':'/fixture'}), \
                    patch.object(launcher, 'filesystem_marker', side_effect=lambda action, alias: markers.append((action, alias)) or True):
                self.assertEqual(executor(Action('a')).content, 'edited')
            tape.close()
            self.assertEqual(delegate.calls, 1)
            self.assertEqual(markers, [('begin','openhands-editor-1'),('end','openhands-editor-1')])
            saved = json.loads(path.read_text())
            self.assertEqual(saved['tool'], 'openhands.file_editor')
            replay = launcher.EditorTape('replay', str(path)); delegate = Delegate()
            executor = launcher.FileEditorTapeExecutor(delegate, replay)
            effect = pathlib.Path(directory) / 'effect'
            def apply(alias):
                self.assertEqual(alias, 'openhands-editor-1')
                effect.write_text('recorded effect')
                return True
            with patch.dict(os.environ, {'ASB_FS_REPLAY_SOCKET':'/fixture'}), \
                    patch.object(launcher, 'filesystem_replay_boundary', side_effect=apply):
                self.assertEqual(executor(Action('a')).content, 'edited')
            self.assertEqual(delegate.calls, 0)
            self.assertEqual(effect.read_text(), 'recorded effect')
            replay.close()

    def test_editor_replay_rejects_mismatch_and_missing_boundary(self):
        definition = sys.modules.get('openhands.tools.file_editor.definition') or module('openhands.tools.file_editor.definition')
        class EditorObservation:
            @classmethod
            def model_validate(cls, value): return value
        definition.FileEditorObservation = EditorObservation
        action = Event(model_dump=lambda mode: {'command':'edit'})
        entry = {'schema_version':1,'sequence':1,'tool':'openhands.file_editor',
                 'request':{'command':'edit'},'result':{'content':'ok'}}
        with tempfile.TemporaryDirectory() as directory:
            path = pathlib.Path(directory) / 'editor.jsonl'; path.write_text(json.dumps(entry)+'\n')
            tape = launcher.EditorTape('replay', str(path))
            executor = launcher.FileEditorTapeExecutor(Event(close=lambda:None), tape)
            wrong = Event(model_dump=lambda mode: {'command':'other'})
            with self.assertRaisesRegex(RuntimeError, 'differs'):
                executor(wrong)
            tape = launcher.EditorTape('replay', str(path))
            executor = launcher.FileEditorTapeExecutor(Event(close=lambda:None), tape)
            with patch.dict(os.environ, {'ASB_FS_REPLAY_SOCKET':'/fixture'}), \
                    patch.object(launcher, 'filesystem_replay_boundary', return_value=False):
                with self.assertRaisesRegex(RuntimeError, 'boundary is unavailable'):
                    executor(action)

    def test_terminal_replay_waits_for_filesystem_effects_after_bridge_validation(self):
        terminal = sys.modules.get('openhands.tools.terminal') or module('openhands.tools.terminal')
        metadata = sys.modules.get('openhands.tools.terminal.metadata') or module('openhands.tools.terminal.metadata')
        terminal.TerminalObservation = Event; metadata.CmdOutputMetadata = Event
        executor = launcher.NativeTerminalExecutor.__new__(launcher.NativeTerminalExecutor)
        executor._marker_lock = threading.Lock(); executor._bridge_lock = threading.Lock(); executor._marker_count = 0; executor._marker_failed = False
        executor._delegate = None
        response = {'kind':'terminal_observation', 'schema_version':1, 'content':'done',
                    'is_error':False, 'command':'true', 'exit_code':0, 'timeout':False, 'metadata':{}}
        class Bridge:
            def __init__(self):
                self.stdin = io.StringIO(); self.stdout = io.StringIO(json.dumps(response) + '\n')
            def wait(self): return 0
        executor._bridge = Bridge()
        action = Event(command='true', is_input=False, timeout=None, reset=False)
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / 'result'
            def apply(alias):
                self.assertEqual(alias, 'openhands-1')
                self.assertTrue(executor._bridge.stdin.getvalue())
                output.write_text('journal effect')
                return True
            with patch.dict(os.environ, {'ASB_FS_REPLAY_SOCKET':'/fixture'}), \
                    patch.object(launcher, 'filesystem_replay_boundary', side_effect=apply):
                result = executor(action)
            self.assertEqual(result.content[0].text, 'done')
            self.assertEqual(output.read_text(), 'journal effect')
            executor.close()

    def test_terminal_replay_rejects_unavailable_filesystem_coordinator(self):
        terminal = sys.modules.get('openhands.tools.terminal') or module('openhands.tools.terminal')
        metadata = sys.modules.get('openhands.tools.terminal.metadata') or module('openhands.tools.terminal.metadata')
        terminal.TerminalObservation = Event; metadata.CmdOutputMetadata = Event
        executor = launcher.NativeTerminalExecutor.__new__(launcher.NativeTerminalExecutor)
        executor._marker_lock = threading.Lock(); executor._bridge_lock = threading.Lock(); executor._marker_count = 0; executor._marker_failed = False
        executor._delegate = None
        response = {'kind':'terminal_observation', 'schema_version':1, 'content':'done',
                    'is_error':False, 'command':'true', 'exit_code':0, 'timeout':False, 'metadata':{}}
        executor._bridge = Event(stdin=io.StringIO(), stdout=io.StringIO(json.dumps(response)+'\n'), wait=lambda:0)
        action = Event(command='true', is_input=False, timeout=None, reset=False)
        with patch.dict(os.environ, {'ASB_FS_REPLAY_SOCKET':'/missing'}):
            with self.assertRaisesRegex(RuntimeError, 'boundary is unavailable'):
                executor(action)
        executor.close()

    def test_terminal_marker_uses_recorder_socket(self):
        with tempfile.TemporaryDirectory() as directory:
            path = str(pathlib.Path(directory) / 'marker.sock')
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(path); listener.listen(1)
            messages = []
            def serve():
                with listener.accept()[0] as connection:
                    messages.append(connection.recv(256).decode('ascii').strip())
                    connection.sendall(b'7\n')
            thread = threading.Thread(target=serve, daemon=True); thread.start()
            try:
                with patch.dict(os.environ, {'ASB_FS_MARKER_SOCKET': path}):
                    self.assertTrue(launcher.filesystem_marker('begin', 'openhands-1'))
                thread.join(timeout=5)
                self.assertFalse(thread.is_alive())
                self.assertRegex(messages[0], r'^tool-begin:openhands-1:\d+:\d+$')
            finally:
                listener.close()

    def test_terminal_wrapper_preserves_other_declared_tools(self):
        terminal = sys.modules.get('openhands.tools.terminal') or module('openhands.tools.terminal')
        terminal.TerminalTool = type('TerminalTool', (), {'name': 'terminal'})
        sys.modules['openhands.sdk'].Tool = lambda name, params: Event(name=name, params=params)
        other = Event(name='file_editor', params={'setting': 'retained'})
        agent = Event(tools=[Event(name='terminal', params={}), other])
        agent.model_copy = lambda update: Event(**update)
        replacement = object()
        result = launcher.with_terminal_executor(agent, replacement)
        self.assertIs(result.tools[1], other)
        self.assertIs(result.tools[0].params['executor'], replacement)

    def test_terminal_recording_marks_real_delegate_without_replacing_other_tools(self):
        module('openhands.tools'); terminal = module('openhands.tools.terminal')
        metadata = module('openhands.tools.terminal.metadata')
        terminal.TerminalObservation = Event
        metadata.CmdOutputMetadata = Event
        executor = launcher.NativeTerminalExecutor.__new__(launcher.NativeTerminalExecutor)
        executor._marker_lock = threading.Lock()
        executor._bridge_lock = threading.Lock()
        executor._marker_count = 0
        executor._marker_failed = False
        events = []
        observation = Event(text='done', is_error=False, command='echo ok', exit_code=0,
                            timeout=False, metadata=Event(model_dump=lambda: {}))
        class Delegate:
            def __call__(self, action):
                events.append('execute')
                return observation
            def close(self): pass
        class Bridge:
            def __init__(self):
                self.stdin = io.StringIO()
                self.stdout = io.StringIO('{"kind":"ack","schema_version":1}\n')
            def wait(self): return 0
        executor._delegate = Delegate()
        executor._bridge = Bridge()
        action = Event(command='echo ok', is_input=False, timeout=None, reset=False)
        with patch.object(launcher, 'filesystem_marker', side_effect=lambda op, alias: events.append((op, alias)) or True):
            self.assertIs(executor(action), observation)
        self.assertEqual(events, [('begin', 'openhands-1'), 'execute', ('end', 'openhands-1')])
        executor.close()

    def test_marker_failure_does_not_skip_terminal_and_fails_recording_on_close(self):
        terminal = sys.modules.get('openhands.tools.terminal') or module('openhands.tools.terminal')
        metadata = sys.modules.get('openhands.tools.terminal.metadata') or module('openhands.tools.terminal.metadata')
        terminal.TerminalObservation = Event
        metadata.CmdOutputMetadata = Event
        executor = launcher.NativeTerminalExecutor.__new__(launcher.NativeTerminalExecutor)
        executor._marker_lock = threading.Lock(); executor._bridge_lock = threading.Lock(); executor._marker_count = 0; executor._marker_failed = False
        observation = Event(text='done', is_error=False, command='true', exit_code=0,
                            timeout=False, metadata=Event(model_dump=lambda: {}))
        class Delegate:
            def __call__(self, action): return observation
            def close(self): pass
        class Bridge:
            def __init__(self):
                self.stdin = io.StringIO()
                self.stdout = io.StringIO('{"kind":"ack","schema_version":1}\n')
            def wait(self): return 0
        executor._delegate = Delegate(); executor._bridge = Bridge()
        action = Event(command='true', is_input=False, timeout=None, reset=False)
        with patch.object(launcher, 'filesystem_marker', return_value=False):
            self.assertIs(executor(action), observation)
        with self.assertRaisesRegex(RuntimeError, 'boundary was unavailable'):
            executor.close()

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
