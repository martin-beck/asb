"""Exercise installed Mini-SWE with deterministic local model responses."""
import os
from pathlib import Path

from minisweagent.agents.default import DefaultAgent

source = Path('/opt/mini_swe.rs').read_text()
transport = source.split('const TOOL_TRANSPORT: &str = r#"', 1)[1].split('"#;', 1)[0]
namespace = {}
exec(compile(transport, 'asb_mini_swe_tools.py', 'exec'), namespace)

class Model:
    def __init__(self): self.calls = 0
    def get_template_vars(self): return {}
    def format_message(self, role, content, **kwargs): return {'role': role, 'content': content, **kwargs}
    def format_observation_messages(self, message, outputs, variables):
        return [{'role': 'user', 'content': str(outputs)}]
    def query(self, messages):
        self.calls += 1
        if self.calls == 1:
            command = "printf fixed > trial-result; printf 1 >> /work/real-agent-invocations"
        else:
            command = "printf 2 >> /work/real-agent-invocations; printf 'COMPLETE_TASK_AND_SUBMIT_FINAL_OUTPUT\\nfixture finished\\n'"
        return {'role': 'assistant', 'content': 'fixture', 'extra': {'actions': [{'command': command}]}}
    def serialize(self): return {}

agent = DefaultAgent(Model(), namespace['AsbTapeEnvironment'](cwd=os.environ['ASB_FIXTURE_WORKSPACE']),
                     system_template='You are a software engineer.', instance_template='{{ task }}')
result = agent.run('Apply the fixture change and submit it.')
if result.get('exit_status') != 'Submitted' or result.get('submission') != 'fixture finished\n':
    raise RuntimeError('real Mini-SWE agent did not submit the fixture result')
