"""Loopback mock provider that records only offered tool names."""
import http.server
import importlib.util
import io
import json
import os
from pathlib import Path
import sys

spec = importlib.util.spec_from_file_location('provider_base', '/provider-base.py')
base = importlib.util.module_from_spec(spec)
spec.loader.exec_module(base)
base.root = Path(sys.argv[1])

class Handler(base.Handler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        parsed = json.loads(body)
        names = []
        for tool in parsed.get('tools', []):
            entry = tool.get('function', tool)
            if isinstance(entry, dict) and isinstance(entry.get('name'), str):
                names.append(entry['name'])
        (base.root / 'tool-names.json').write_text(json.dumps(names))
        if os.environ.get('ASB_FIXTURE_NATIVE_FILES') == '1' and self.path == '/v1/chat/completions':
            self.native_file_response(parsed)
            return
        self.rfile = io.BytesIO(body)
        super().do_POST()

    def native_file_response(self, body):
        base.requests += 1
        (base.root / 'requests.json').write_text(json.dumps({'requests': base.requests}))
        results = sum(message.get('role') == 'tool' for message in body.get('messages', []))
        calls = [
            ('write_file', {'path': '/work/mount/asb-real-agent.txt', 'content': 'before'}),
            ('read_file', {'path': '/work/mount/asb-real-agent.txt'}),
            ('patch', {'mode': 'replace', 'path': '/work/mount/asb-real-agent.txt', 'old_string': 'before', 'new_string': 'fixture'}),
        ]
        if os.environ.get('ASB_FIXTURE_MIXED') == '1':
            calls.append(('search_files', {'pattern':'fixture','path':'/work/mount/asb-real-agent.txt','target':'content'}))
            calls.append(('skills_list', {}))
            calls.append(('terminal', {'command':base.command() if os.environ.get('ASB_FIXTURE_SWE_PATCH') == '1' else 'printf mixed > mixed-marker; printf ran > /tmp/asb-native-agent-executed'}))
        if results < len(calls):
            name, args = calls[results]
            assert name in [entry.get('function', entry).get('name') for entry in body.get('tools', [])]
            delta = {'role':'assistant','tool_calls':[{'index':0,'id':f'fixture-{results}', 'type':'function',
                     'function':{'name':name,'arguments':json.dumps(args)}}]}
            finish = 'tool_calls'
        else:
            delta = {'role':'assistant','content':'Fixture completed.'}
            finish = 'stop'
        usage={'prompt_tokens':17,'completion_tokens':5,'total_tokens':22,
               'prompt_tokens_details':{'cached_tokens':0}}
        self.send_response(200)
        self.send_header('Content-Type','text/event-stream')
        self.send_header('Connection','close')
        self.end_headers()
        for choices,tokens in [([{'index':0,'delta':delta,'finish_reason':None}],None),
                               ([{'index':0,'delta':{},'finish_reason':finish}],usage)]:
            event={'id':'fixture-response','object':'chat.completion.chunk','model':body.get('model'),'choices':choices}
            if tokens is not None: event['usage']=tokens
            self.wfile.write(('data: '+json.dumps(event)+'\n\n').encode())
        self.wfile.write(b'data: [DONE]\n\n'); self.wfile.flush()

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
(base.root / 'port').write_text(str(server.server_address[1]))
server.serve_forever()
