"""Offline OpenJiuwen embedding via actual llmtape, then four replay sessions."""
import asyncio
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

repo = Path(__file__).resolve().parents[3]
script = str(repo / 'crates/asb-cli/assets/scripts/llmtape.mjs')
python = os.environ['ASB_OPENJIUWEN_PYTHON']
hook_path = str(repo / 'crates/asb-cli/src/agents/adapters/openjiuwen/embedding_route.py')
test_root = repo / 'target/openjiuwen-embedding-tape'
test_root.mkdir(parents=True, exist_ok=True)
seen = []
class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        seen.append((self.path, body, self.headers.get('Authorization')))
        payload = json.dumps({'data': [{'index': 0, 'embedding': [0.25, 0.75]}]}).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
    def log_message(self, *_): pass

def start(root, label, args):
    ready = root / f'{label}.ready'
    env = dict(os.environ, ASB_TAPE_READY_FILE=str(ready))
    p = subprocess.Popen(['node', script, *args, '--port', '0'], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    for _ in range(200):
        if ready.exists():
            return p, json.loads(ready.read_text())['port']
        if p.poll() is not None: raise RuntimeError('tape exited on startup')
        time.sleep(.025)
    p.terminate(); p.wait(); raise RuntimeError('tape startup timed out')

def stop(p):
    p.terminate()
    try: p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill(); p.wait(); raise
    assert p.returncode == 0, p.stderr.read().decode(errors='replace')[-1000:]

async def request(root, original_url, port, mode):
    worker = r"""
import asyncio, json, runpy, sys
from pathlib import Path
from openjiuwen.core.memory.lite.embeddings import OpenAICompatibleEmbeddingProvider
hook = runpy.run_path(sys.argv[3])
metadata = Path(sys.argv[4])
mode = sys.argv[5]
if mode != 'baseline':
    hook['_asb_install_embedding_route'](f'http://127.0.0.1:{sys.argv[2]}')
async def main():
    key = 'fixture-record-only-key' if mode in ('record', 'baseline') else 'asb-replay-placeholder'
    if mode == 'replay':
        identity = json.loads(metadata.read_text())
        hook['_asb_restore_opaque_embedding_identity'](identity)
    provider = OpenAICompatibleEmbeddingProvider(api_key=key, model='fixture-model', base_url=sys.argv[1])
    if mode == 'record':
        identity = hook['_asb_make_embedding_identity'](provider)
    fingerprint = provider.config_fingerprint
    if mode == 'record':
        assert key not in repr(identity)
        metadata.write_text(json.dumps(identity))
    elif mode == 'replay':
        assert fingerprint == identity['fingerprint']
        other = OpenAICompatibleEmbeddingProvider(api_key='another-key', model=identity['model'], base_url=sys.argv[1])
        assert other.config_fingerprint != fingerprint
    try:
        assert await provider.embed_query('fixture input') == [0.25, 0.75]
        assert provider.base_url == sys.argv[1] and provider.config_fingerprint == fingerprint
    finally:
        if provider._client: await provider._client.aclose()
asyncio.run(main())
"""
    child = await asyncio.create_subprocess_exec(
        python, '-c', worker, original_url, str(port), hook_path, str(root / 'identity.json'), mode,
        cwd=root,
        stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.PIPE)
    _, error = await child.communicate()
    if child.returncode: raise RuntimeError(error.decode(errors='replace')[-2000:])

async def main():
    upstream = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=upstream.serve_forever, daemon=True); thread.start()
    origin = f'http://127.0.0.1:{upstream.server_port}'
    userinfo = os.environ.get('ASB_TEST_EMBEDDING_USERINFO') == '1'
    original_url = (f'http://fixture-user:fixture-url-password@127.0.0.1:{upstream.server_port}'
                    if userinfo else origin) + '/v1'
    processes = []
    with tempfile.TemporaryDirectory(dir=test_root) as tmp:
        root = Path(tmp); tape = root / 'record.jsonl'
        try:
            if userinfo:
                await request(root, original_url, upstream.server_port, 'baseline')
            recorder, port = start(root, 'record', ['record', '--upstream', origin, '--out', str(tape)])
            processes.append(recorder)
            await request(root, original_url, port, 'record')
            stop(recorder); processes.remove(recorder)
            assert 'fixture-record-only-key' not in tape.read_text()
            assert 'fixture-record-only-key' not in (root / 'identity.json').read_text()
            if userinfo:
                assert len(seen) == 2 and seen[0] == seen[1]
                assert 'fixture-url-password' not in (root / 'identity.json').read_text()
                assert 'fixture-user' not in (root / 'identity.json').read_text()
                assert 'fixture-url-password' not in tape.read_text()
            else:
                assert len(seen) == 1 and seen[0][0] == '/v1/embeddings'
            upstream.shutdown(); thread.join(); upstream.server_close()
            sessions = []
            for i in range(4):
                replica = root / f'replay-{i}.jsonl'; shutil.copyfile(tape, replica)
                child, child_port = start(root, f'replay-{i}', ['replay', '--cassette', str(replica)])
                processes.append(child); sessions.append((child, child_port, replica))
            await asyncio.gather(*(request(root, original_url, port, 'replay') for _, port, _ in sessions))
            for child, _, replica in sessions:
                stop(child); processes.remove(child)
                progress = json.loads(Path(str(replica) + '.progress.json').read_text())
                assert progress['consumed'] == progress['total'] == 1, progress
            assert len(seen) == (2 if userinfo else 1)
            print('Actual OpenJiuwen client: one embedding recorded and replayed by four independent llmtape processes after upstream shutdown; opaque cache identity preserved.')
        finally:
            for child in processes:
                if child.poll() is None: child.kill(); child.wait()
            if thread.is_alive(): upstream.shutdown(); thread.join(); upstream.server_close()
asyncio.run(main())
