"""Exercise the optional OpenJiuwen snapshot marker protocol offline."""
import ast
import asyncio
import os
from pathlib import Path
import socket
import socketserver
import tempfile
import threading
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
SOURCE = ROOT / 'crates/asb-cli/src/agents/adapters/openjiuwen/usage_compat.py'
SCRATCH = ROOT / 'target/openjiuwen-snapshot-tests'
SCRATCH.mkdir(parents=True, exist_ok=True)
module = ast.parse(SOURCE.read_text())
helper = next(node for node in module.body if isinstance(node, ast.AsyncFunctionDef) and node.name == '_snapshot_boundary')
namespace = {'asyncio': asyncio, 'os': os, 'socket': socket, 'threading': threading}
exec(compile(ast.fix_missing_locations(ast.Module(body=[helper], type_ignores=[])), str(SOURCE), 'exec'), namespace)
boundary = namespace['_snapshot_boundary']


class MarkerTest(unittest.IsolatedAsyncioTestCase):
    async def test_record_and_replay_send_the_native_invocation_alias(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            path = str(Path(directory) / 'marker.sock')
            messages = []
            class Handler(socketserver.StreamRequestHandler):
                def handle(self):
                    messages.append(self.rfile.readline())
                    self.wfile.write(b'1\n')
            server = socketserver.ThreadingUnixStreamServer(path, Handler)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                with patch.dict(os.environ, {'ASB_FS_MARKER_SOCKET': path, 'ASB_FS_REPLAY_SOCKET': path}):
                    await boundary('record', 'begin', 'openjiuwen-4')
                    await boundary('record', 'end', 'openjiuwen-4')
                    await boundary('replay', 'end', 'openjiuwen-4')
            finally:
                server.shutdown(); server.server_close(); thread.join()
            self.assertRegex(messages[0], rb'^tool-begin:openjiuwen-4:\d+:\d+\n$')
            self.assertRegex(messages[1], rb'^tool-end:openjiuwen-4:\d+:\d+\n$')
            self.assertEqual(messages[2], b'tool-end:openjiuwen-4\n')

    async def test_missing_optional_service_is_noop_and_unavailable_service_fails(self):
        with patch.dict(os.environ, {}, clear=True):
            await boundary('record', 'begin', 'openjiuwen-1')
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            with patch.dict(os.environ, {'ASB_FS_MARKER_SOCKET': str(Path(directory) / 'absent')}):
                with self.assertRaisesRegex(RuntimeError, 'boundary is unavailable'):
                    await boundary('record', 'begin', 'openjiuwen-1')


if __name__ == '__main__':
    unittest.main()
