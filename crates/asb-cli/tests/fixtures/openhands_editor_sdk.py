"""Offline opt-in test using a project-installed OpenHands SDK interpreter."""
import importlib.util
import os
from pathlib import Path
import tempfile
from types import SimpleNamespace

here = Path(__file__).resolve()
root = here.parents[4]
launcher = here.parents[2] / 'assets/scripts/openhands-sdk-launcher.py'
spec = importlib.util.spec_from_file_location('asb_openhands_editor_test', launcher)
module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
from openhands.tools.file_editor.definition import FileEditorAction, FileEditorTool
from openhands.tools.task.definition import TaskToolSet
from openhands.tools.task_tracker.definition import TaskTrackerTool
from openhands.tools.preset.default import register_builtins_agents
from openhands.sdk.subagent import get_registered_agent_definitions
from openhands.sdk import LLM, Tool
from openhands.sdk.tool import ToolExecutor, resolve_tool
from openhands.tools.task import manager
from openhands_cli.utils import get_default_cli_agent

register_builtins_agents(enable_browser=False)
assert get_registered_agent_definitions(), 'built-in delegated agents were not registered'

scratch = root / 'target/openhands-editor-sdk-fixture'; scratch.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(dir=scratch) as directory:
    workspace = Path(directory).resolve(); sidecar = workspace / 'editor.jsonl'; sidecar.touch()
    os.environ['ASB_FS_MARKER_SOCKET'] = '/fixture'
    module.filesystem_marker = lambda *_: True
    tape = module.EditorTape('record', str(sidecar)); module.install_file_editor_tape(tape)
    state = SimpleNamespace(workspace=SimpleNamespace(working_dir=str(workspace)),
        persistence_dir=workspace,
        agent=SimpleNamespace(llm=SimpleNamespace(vision_is_active=lambda: False)))
    tool = resolve_tool(Tool(name=FileEditorTool.name), state)[0]
    for kind, expected in ((TaskTrackerTool, module.UnrecordedToolExecutor),
                           (TaskToolSet, module.DelegationTapeExecutor)):
        definitions = resolve_tool(Tool(name=kind.name), state)
        assert definitions and all(isinstance(item.executor, expected)
                                   for item in definitions)
    class Terminal(ToolExecutor):
        def __call__(self, action, conversation=None): return 'transport'
    agent = get_default_cli_agent(LLM(model='openai/fixture', api_key='fixture',
                                     base_url='http://127.0.0.1:1/v1', usage_id='fixture'))
    manager.LocalConversation = lambda *_args, **_kwargs: SimpleNamespace(agent=agent)
    module.install_subagent_terminal(Terminal())
    child = manager.LocalConversation()
    terminal = next(item for item in child.agent.tools if item.name == 'terminal')
    assert isinstance(terminal.params['executor'], module.SharedTerminalExecutor)
    file = workspace / 'sample.txt'
    action = FileEditorAction(command='create', path=str(file), file_text='recorded content\n')
    result = tool.executor(action); tape.close()
    assert file.read_text() == 'recorded content\n'
    file.unlink()
    tape = module.EditorTape('replay', str(sidecar))
    class UnavailableDelegate:
        def __call__(self, *_args, **_kwargs): raise AssertionError('live editor was called')
        def close(self): pass
    executor = module.FileEditorTapeExecutor(UnavailableDelegate(), tape)
    def apply(alias):
        assert alias == 'openhands-editor-1'
        file.write_text('recorded content\n')
        return True
    os.environ['ASB_FS_REPLAY_SOCKET'] = '/fixture'
    module.filesystem_replay_boundary = apply
    replay = executor(action); tape.close()
    assert replay.model_dump(mode='json') == result.model_dump(mode='json')
    assert file.read_text() == 'recorded content\n'
print('Installed OpenHands SDK editor recording and replay passed')
