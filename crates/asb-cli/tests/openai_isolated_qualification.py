"""Opt-in isolated SWE-bench gateway qualification; no live provider is used.

Set ASB_TEST_ASB, ASB_TEST_SWE_SEED, and ASB_TEST_TEMP_ROOT to an ASB build,
a project with installed swebench/pi/node and its catalog, and a directory
for the retained fixture. A local Docker daemon, task image, and sudo -n for
Docker are required. The fake agent exercises routing, not task resolution.
"""
import http.server
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import zipfile


def main():
    binary = Path(os.environ['ASB_TEST_ASB']).resolve(strict=True)
    seed = Path(os.environ['ASB_TEST_SWE_SEED']).resolve(strict=True)
    root = Path(tempfile.mkdtemp(prefix='asb-openai-isolated-', dir=os.environ['ASB_TEST_TEMP_ROOT']))
    home = root / 'home'
    home.mkdir()
    tools = root / 'host-bin'
    tools.mkdir()
    docker = tools / 'docker'
    docker.write_text('#!/bin/sh\nexec sudo -n /usr/bin/docker "$@"\n')
    docker.chmod(0o755)
    env = {'HOME': str(home), 'PATH': str(tools) + ':' + os.environ.get('PATH', ''),
           'ASB_INIT_OFFLINE': '1', 'OPENAI_API_KEY': 'fixture-only-key'}
    subprocess.run([str(binary), 'init'], cwd=root, env=env, stdin=subprocess.DEVNULL,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=True, timeout=300)
    for name in ('swebench', 'pi', 'node'):
        subprocess.run(['cp', '-a', '--reflink=auto', str(seed / '.asb-tools' / name),
                        str(root / '.asb-tools' / name)], check=True)
    subprocess.run(['cp', '-a', '--reflink=auto', str(seed / 'workloads/swe-bench'),
                    str(root / 'workloads/swe-bench')], check=True)
    agent = root / '.asb-tools/pi/node_modules/@mariozechner/pi-coding-agent/dist/cli.js'
    agent.write_text('''#!/usr/bin/env node
import fs from 'node:fs';
if (!fs.existsSync('/.dockerenv')) { console.error('expected Docker task container'); process.exit(17); }
const conf = JSON.parse(fs.readFileSync(process.env.PI_CODING_AGENT_DIR + '/models.json'));
const endpoint = conf.providers.asb.baseUrl;
fetch(endpoint + '/responses', {method:'POST', headers:{'content-type':'application/json', 'authorization':'Bearer '+process.env.ASB_AGENT_API_KEY}, body:JSON.stringify({model:'fixture-model',input:'isolated',tools:[{type:'function',name:'read_file'}]})}).then(r=>{if(!r.ok) process.exit(7); console.log('isolated fixture agent');}).catch(e=>{console.error(e); process.exit(8)});
''')
    requests = []
    class Gateway(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_): pass
        def reply(self, body):
            payload = json.dumps(body).encode()
            self.send_response(200); self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(payload))); self.end_headers(); self.wfile.write(payload)
        def do_GET(self):
            requests.append(('GET', self.path))
            self.reply({'data': [{'id': 'fixture-model', 'supported_parameters': ['tools']}]})
        def do_POST(self):
            self.rfile.read(int(self.headers.get('Content-Length', '0')))
            requests.append(('POST', self.path))
            self.reply({'id': 'fixture', 'object': 'response', 'output': []})
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Gateway)
    thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
    (home / '.api_base_url_openai').write_text(f'http://127.0.0.1:{server.server_port}/gateway\n')
    (root / 'benchmark.toml').write_text('''agent="pi"
workload="swe-bench"
provider="openai"
model="fixture-model"
trial="sympy__sympy-14774"
workspace="workspace"
log_file="run.log"
[planning.budgets]
environment_preparation_seconds=900
execution_seconds=60
evaluation_seconds=120
''')
    command = [str(binary), 'run', 'benchmark.toml', '--llm-record', '--cassette', 'tape.jsonl']
    try:
        with (root / 'run-output.log').open('wb') as log:
            process = subprocess.Popen(command, cwd=root, env=env, stdin=subprocess.DEVNULL,
                                       stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                status = process.wait(timeout=900)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGTERM)
                try: process.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL); process.wait()
                raise RuntimeError(f'qualification timed out; inspect {root}') from None
    finally:
        server.shutdown(); server.server_close(); thread.join()
    (root / 'requests.json').write_text(json.dumps(requests))
    if ('POST', '/gateway/responses') not in requests:
        raise RuntimeError(f'isolated agent did not reach the configured gateway; inspect {root}')
    if not (root / 'tape.jsonl').is_file():
        raise RuntimeError(f'gateway request was not recorded; inspect {root}')
    run_logs = list(root.glob('results/asb-executions/*/batch-1/run.jsonl'))
    if len(run_logs) != 1 or not json.loads(run_logs[0].read_text().splitlines()[-1]).get('process_success'):
        raise RuntimeError(f'isolated fixture agent did not exit successfully; inspect {root}')
    with zipfile.ZipFile(root / 'tape.jsonl') as cassette:
        tracks = [name for name in cassette.namelist() if name.startswith('tracks/') and name.endswith('.jsonl')]
        events = [json.loads(line) for name in tracks for line in cassette.read(name).splitlines() if line]
        if not any(event.get('request', {}).get('url') == '/v1/responses' and event.get('response', {}).get('status') == 200 for event in events):
            raise RuntimeError(f'cassette does not contain the isolated request and response; inspect {root}')
    print(json.dumps({'fixture': str(root), 'gateway_requests': requests, 'exit_status': status,
                      'scope': 'isolated fake agent and loopback gateway; task resolution not qualified'}))


if __name__ == '__main__':
    main()
