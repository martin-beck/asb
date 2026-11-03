import http.server
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time

project = Path(__file__).resolve().parents[2]
hook = project / 'assets/scripts/hermes-native-tools.py'
scratch = project / 'target'; scratch.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(dir=scratch) as temp:
    root = Path(temp); (root/'home').mkdir()
    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_args): pass
        def do_GET(self):
            (root/'connected').touch(); time.sleep(.4)
            self.send_response(200); self.send_header('Content-Length','2'); self.end_headers(); self.wfile.write(b'ok')
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True); thread.start()
    code = '''import importlib.util, os, pathlib, threading, time, httpx\nroot=pathlib.Path(os.environ["FIXTURE_ROOT"])\nspec=importlib.util.spec_from_file_location("asb_hermes_http_fixture",os.environ["FIXTURE_HOOK"]); m=importlib.util.module_from_spec(spec); spec.loader.exec_module(m)\ndef fetch():\n r=httpx.get(os.environ["FIXTURE_URL"], timeout=3); (root/"delivered").write_text(str(r.status_code))\nthreading.Thread(target=fetch,daemon=True).start()\nfor _ in range(300):\n if (root/"connected").exists(): break\n time.sleep(.01)\nelse: raise RuntimeError("HTTP request did not start")\nos._exit(0)\n'''
    env = dict(os.environ, ASB_HERMES_NATIVE_MODE='record', ASB_HERMES_NATIVE_SIDECAR=str(root/'native'), ASB_HERMES_NATIVE_PROGRESS=str(root/'progress'), HERMES_HOME=str(root/'home'), FIXTURE_ROOT=str(root), FIXTURE_HOOK=str(hook), FIXTURE_URL=f'http://127.0.0.1:{server.server_port}/models')
    try:
        result = subprocess.run([sys.executable, '-c', code], env=env, capture_output=True, timeout=15)
        assert result.returncode == 0, result.stderr.decode(errors='replace')[-1000:]
        assert (root/'delivered').read_text() == '200', 'in-flight response was not delivered before exit'
        print('Hermes exit drained in-flight HTTPX response')
    finally:
        server.shutdown(); server.server_close(); thread.join()
