"""Offline tests for the experimental per-replica replay server."""
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
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

    def test_nested_delegation_admits_parent_before_child_effects(self):
        workspace = self.root / 'nested'; workspace.mkdir()
        path = self.root / 'nested.sock'
        events = [
            {'sequence':1, 'op':'marker', 'path':None, 'marker':'tool-begin:parent-1'},
            {'sequence':2, 'op':'marker', 'path':None, 'marker':'tool-begin:child-1'},
            {'sequence':3, 'op':'create', 'path':'/result', 'handle':1, 'flags':1, 'mode':0o600},
            {'sequence':4, 'op':'write', 'path':'/result', 'handle':1, 'offset':0,
             'object':'payload.bin', 'payload_offset':0, 'bytes':5},
            {'sequence':5, 'op':'release', 'path':'/result', 'handle':1},
            {'sequence':6, 'op':'marker', 'path':None, 'marker':'tool-end:child-1'},
            {'sequence':7, 'op':'marker', 'path':None, 'marker':'tool-end:parent-1'},
        ]
        (self.evidence / 'events.jsonl').write_text(''.join(json.dumps(event)+'\n' for event in events))
        server = ReplayServer(self.evidence, workspace, path, timeout=2)
        def boundary(value):
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.connect(str(path)); client.sendall(value)
                return client.recv(32)
        self.assertEqual(boundary(b'tool-begin:parent-1\n'), b'1\n')
        self.assertFalse((workspace/'result').exists())
        self.assertEqual(boundary(b'tool-end:child-1\n'), b'6\n')
        self.assertEqual((workspace/'result').read_bytes(), b'hello')
        self.assertEqual(boundary(b'tool-end:parent-1\n'), b'7\n')
        server.finish()

    def test_unfinished_admitted_parent_cannot_complete(self):
        workspace = self.root / 'unfinished'; workspace.mkdir()
        path = self.root / 'unfinished.sock'
        server = ReplayServer(self.evidence, workspace, path, timeout=2)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.connect(str(path)); client.sendall(b'tool-begin:mini-swe-1\n')
            self.assertEqual(client.recv(32), b'1\n')
        with self.assertRaisesRegex(RuntimeError, 'did not complete'):
            server.finish()
        self.assertFalse((workspace/'result').exists())

    def test_four_nested_delegation_replicas_are_isolated(self):
        events = [
            {'sequence':1, 'op':'marker', 'path':None, 'marker':'tool-begin:parent-1'},
            {'sequence':2, 'op':'marker', 'path':None, 'marker':'tool-begin:child-1'},
            {'sequence':3, 'op':'create', 'path':'/result', 'handle':1, 'flags':1, 'mode':0o600},
            {'sequence':4, 'op':'write', 'path':'/result', 'handle':1, 'offset':0,
             'object':'payload.bin', 'payload_offset':0, 'bytes':5},
            {'sequence':5, 'op':'release', 'path':'/result', 'handle':1},
            {'sequence':6, 'op':'marker', 'path':None, 'marker':'tool-end:child-1'},
            {'sequence':7, 'op':'marker', 'path':None, 'marker':'tool-end:parent-1'},
        ]
        (self.evidence / 'events.jsonl').write_text(''.join(json.dumps(event)+'\n' for event in events))
        def replica(index):
            workspace = self.root / f'nested-{index}'; workspace.mkdir()
            path = self.root / f'nested-{index}.sock'
            server = ReplayServer(self.evidence, workspace, path, timeout=2)
            for message, expected in [(b'tool-begin:parent-1\n', b'1\n'),
                                      (b'tool-end:child-1\n', b'6\n'),
                                      (b'tool-end:parent-1\n', b'7\n')]:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                    client.connect(str(path)); client.sendall(message)
                    self.assertEqual(client.recv(32), expected)
            server.finish()
            return (workspace/'result').read_bytes()
        with ThreadPoolExecutor(max_workers=4) as pool:
            self.assertEqual(list(pool.map(replica, range(4))), [b'hello'] * 4)

    def test_overlapping_nested_children_wait_for_each_other(self):
        workspace = self.root / 'overlap'; workspace.mkdir()
        path = self.root / 'overlap.sock'
        names = ['tool-begin:parent-1', 'tool-begin:child-1', 'tool-begin:child-2',
                 'tool-end:child-2', 'tool-end:child-1', 'tool-end:parent-1']
        (self.evidence / 'events.jsonl').write_text(''.join(
            json.dumps({'sequence':index, 'op':'marker', 'path':None, 'marker':name})+'\n'
            for index, name in enumerate(names, 1)))
        server = ReplayServer(self.evidence, workspace, path, timeout=3)
        def call(value):
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.connect(str(path)); client.sendall(value)
                return client.recv(32)
        self.assertEqual(call(b'tool-begin:parent-1\n'), b'1\n')
        with ThreadPoolExecutor(max_workers=2) as pool:
            first = pool.submit(call, b'tool-end:child-1\n')
            time.sleep(.1)
            self.assertFalse(first.done())
            second = pool.submit(call, b'tool-end:child-2\n')
            self.assertEqual(second.result(timeout=3), b'4\n')
            self.assertEqual(first.result(timeout=3), b'5\n')
        self.assertEqual(call(b'tool-end:parent-1\n'), b'6\n')
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
