"""Offline tests for the experimental per-replica replay server."""
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parents[1] / 'assets/benchmarks'))
from asb_fs_replay_server import ReplayServer


class ReplayServerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.evidence = self.root / 'evidence'
        self.evidence.mkdir()
        (self.evidence / 'payload.bin').write_bytes(b'hello')
        events = [
            {'sequence': 1, 'op': 'marker', 'path': None, 'marker': 'tool-begin:mini-swe-1'},
            {'sequence': 2, 'op': 'create', 'path': '/result', 'handle': 1, 'flags': 1, 'mode': 0o600},
            {'sequence': 3, 'op': 'write', 'path': '/result', 'handle': 1, 'offset': 0,
             'object': 'payload.bin', 'payload_offset': 0, 'bytes': 5},
            {'sequence': 4, 'op': 'release', 'path': '/result', 'handle': 1},
            {'sequence': 5, 'op': 'marker', 'path': None, 'marker': 'tool-end:mini-swe-1'},
        ]
        (self.evidence / 'events.jsonl').write_text(''.join(json.dumps(event) + '\n' for event in events))

    def replica(self, index):
        workspace = self.root / f'workspace-{index}'; workspace.mkdir()
        path = self.root / f'socket-{index}'
        server = ReplayServer(self.evidence, workspace, path, timeout=2)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.connect(str(path)); client.sendall(b'tool-end:mini-swe-1\n')
            self.assertEqual(client.recv(32), b'5\n')
        self.assertEqual((workspace / 'result').read_bytes(), b'hello')
        server.finish()
        self.assertFalse(path.exists())

    def test_four_isolated_replicas_apply_before_acknowledging(self):
        with ThreadPoolExecutor(max_workers=4) as pool:
            list(pool.map(self.replica, range(4)))

    def test_server_prepares_evidenced_initial_bytes_before_ready(self):
        workspace = self.root / 'workspace'; workspace.mkdir()
        initial = workspace / 'baseline'; initial.write_bytes(b'xxxxx')
        mode = initial.stat().st_mode
        events = [
            {'sequence':1, 'op':'open', 'path':'/baseline', 'handle':1, 'flags':0,
             'initial_size':5, 'initial_mode':mode, 'initial_nlink':1},
            {'sequence':2, 'op':'read', 'path':'/baseline', 'handle':1,
             'offset':0, 'bytes':5, 'object':'payload.bin', 'payload_offset':0},
            {'sequence':3, 'op':'release', 'path':'/baseline', 'handle':1},
        ]
        (self.evidence / 'events.jsonl').write_text(''.join(json.dumps(event) + '\n' for event in events))
        server = ReplayServer(self.evidence, workspace, self.root / 'socket', timeout=2)
        self.assertEqual(initial.read_bytes(), b'hello')
        server.finish()

    def test_uninvoked_boundary_fails_at_finish(self):
        workspace = self.root / 'workspace'; workspace.mkdir()
        server = ReplayServer(self.evidence, workspace, self.root / 'socket', timeout=2)
        with self.assertRaisesRegex(RuntimeError, 'waiting for a recorded tool invocation'):
            server.finish()
        self.assertFalse((workspace / 'result').exists())

    def test_wrong_boundary_fails_and_does_not_apply(self):
        workspace = self.root / 'workspace'; workspace.mkdir()
        path = self.root / 'socket'
        server = ReplayServer(self.evidence, workspace, path, timeout=2)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.connect(str(path)); client.sendall(b'tool-end:wrong\n')
            self.assertEqual(client.recv(32), b'')
        with self.assertRaisesRegex(RuntimeError, 'did not complete'):
            server.finish()
        self.assertFalse((workspace / 'result').exists())

    def test_incomplete_client_does_not_block_finish(self):
        workspace = self.root / 'workspace'; workspace.mkdir()
        path = self.root / 'socket'
        server = ReplayServer(self.evidence, workspace, path, timeout=2)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.connect(str(path)); client.sendall(b'tool-end:')
            with self.assertRaises(RuntimeError):
                server.finish()

    def test_supervisor_requires_ready_boundary_and_clean_stop(self):
        workspace = self.root / 'workspace'; workspace.mkdir()
        path = self.root / 'socket'
        script = Path(__file__).parents[1] / 'assets/benchmarks/asb_fs_replay_server.py'
        process = subprocess.Popen([sys.executable, str(script), str(self.evidence),
                                    str(workspace), str(path)], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            self.assertEqual(process.stdout.readline(), 'READY\n')
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.connect(str(path)); client.sendall(b'tool-end:mini-swe-1\n')
                self.assertEqual(client.recv(32), b'5\n')
            output, error = process.communicate('STOP\n', timeout=5)
            self.assertEqual(process.returncode, 0, error)
            self.assertFalse(path.exists())
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()

    def test_supervisor_rejects_incomplete_journal(self):
        workspace = self.root / 'workspace'; workspace.mkdir()
        path = self.root / 'socket'
        script = Path(__file__).parents[1] / 'assets/benchmarks/asb_fs_replay_server.py'
        process = subprocess.Popen([sys.executable, str(script), str(self.evidence),
                                    str(workspace), str(path)], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            self.assertEqual(process.stdout.readline(), 'READY\n')
            output, error = process.communicate('STOP\n', timeout=5)
            self.assertNotEqual(process.returncode, 0)
            self.assertIn('waiting for a recorded tool invocation', error)
            self.assertFalse(path.exists())
        finally:
            if process.poll() is None:
                process.kill()
                process.communicate()


if __name__ == '__main__':
    unittest.main()
