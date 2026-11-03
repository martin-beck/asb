"""Loopback mock provider that records only offered tool names."""
import http.server
import importlib.util
import io
import json
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
        self.rfile = io.BytesIO(body)
        super().do_POST()

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
(base.root / 'port').write_text(str(server.server_address[1]))
server.serve_forever()
