"""Offline coverage for the filesystem recorder's payload storage."""
from pathlib import Path
import json
import os
import runpy
import sys
import tempfile
import threading
from types import ModuleType
import unittest
from unittest.mock import patch


def fake_fuse(proxy, *args, **kwargs):
    proxy.log.close()
    proxy.payload.file.close()


class PayloadTests(unittest.TestCase):
    def test_open_records_initial_regular_file_metadata(self):
        from types import SimpleNamespace
        fuse = ModuleType('fuse')
        fuse.FUSE = fake_fuse
        fuse.FuseOSError = OSError
        fuse.Operations = object
        fuse.fuse_get_context = lambda: (0, 0, 0)
        script = Path(__file__).parents[1] / 'assets/benchmarks/asb_fs_proxy.py'
        with tempfile.TemporaryDirectory() as temporary, patch.dict(sys.modules, {'fuse': fuse}), \
                patch.dict(os.environ, {'ASB_PROBE_BENCHMARK_NO_IDENTITY': '1'}), \
                patch.object(sys, 'argv', [str(script), temporary, temporary, temporary]):
            (Path(temporary) / 'file').write_bytes(b'fixture')
            namespace = runpy.run_path(str(script))
            proxy = namespace['Proxy']()
            handle = SimpleNamespace(flags=os.O_RDONLY, fh=None)
            self.assertEqual(proxy.open('/file', handle), 0)
            proxy.release('/file', handle)
            proxy.log.close(); proxy.payload.file.close()
            event = json.loads((Path(temporary) / 'events.jsonl').read_text().splitlines()[0])
            self.assertEqual(event['initial_size'], 7)
            self.assertEqual(event['initial_nlink'], 1)
            self.assertEqual(event['initial_mode'], (Path(temporary) / 'file').stat().st_mode)

    def test_hermes_markers_bind_calls_and_failure_does_not_block_the_tool(self):
        fuse = ModuleType('fuse')
        fuse.FUSE = fake_fuse
        fuse.FuseOSError = OSError
        fuse.Operations = object
        fuse.fuse_get_context = lambda: (0, 0, 0)
        base = Path(__file__).parents[1]
        proxy_script = base / 'assets/benchmarks/asb_fs_proxy.py'
        hook = base / 'assets/scripts/hermes-native-tools.py'
        with tempfile.TemporaryDirectory() as temporary, patch.dict(sys.modules, {'fuse': fuse}), \
                patch.object(sys, 'argv', [str(proxy_script), temporary, temporary, temporary]):
            namespace = runpy.run_path(str(proxy_script))
            socket_path = str(Path(temporary) / 'markers.sock')
            variables = {'ASB_PROBE_CONTROL_SOCKET':socket_path, 'ASB_FS_MARKER_SOCKET':socket_path,
                         'ASB_HERMES_NATIVE_MODE':'record',
                         'ASB_HERMES_NATIVE_SIDECAR':str(Path(temporary) / 'native.jsonl'),
                         'ASB_HERMES_NATIVE_PROGRESS':str(Path(temporary) / 'progress.json')}
            with patch.dict(os.environ, variables):
                proxy = namespace['Proxy']()
                source = hook.read_text().split('\ntry:\n    # Hermes exposes', 1)[0]
                hermes = {'__name__':'marker_test'}
                exec(compile(source, str(hook), 'exec'), hermes)
                recorder = hermes['Recorder']()
                self.assertEqual(recorder.invoke(lambda *_args:'tool result', 'echo', False, None,
                    None, None, False, '.', False, False, None), 'tool result')
                barrier = threading.Barrier(2)
                failures = []
                def call():
                    try:
                        def tool(*_args):
                            barrier.wait(timeout=5)
                            return 'parallel result'
                        recorder.invoke(tool, 'parallel', False, None, None, None, False, '.', False, False, None)
                    except BaseException as error:
                        failures.append(error)
                threads = [threading.Thread(target=call) for _ in range(2)]
                for thread in threads: thread.start()
                for thread in threads: thread.join(timeout=10)
                self.assertFalse(failures)
                self.assertTrue(all(not thread.is_alive() for thread in threads))
                recorder.close()
                proxy.listener.close()
                proxy.log.close()
                proxy.payload.file.close()
                events = [json.loads(line) for line in (Path(temporary) / 'events.jsonl').read_text().splitlines()]
                markers = [event['marker'] for event in events]
                self.assertEqual(markers[:2], ['tool-begin:terminal-1', 'tool-end:terminal-1'])
                self.assertEqual(events[0]['origin']['pid'], os.getpid())
                self.assertEqual(events[0]['origin']['tid'], threading.get_native_id())
                for alias in ('terminal-2', 'terminal-3'):
                    self.assertEqual(markers.count('tool-begin:' + alias), 1)
                    self.assertEqual(markers.count('tool-end:' + alias), 1)
                    self.assertLess(markers.index('tool-begin:' + alias), markers.index('tool-end:' + alias))
                self.assertTrue(json.loads((Path(temporary) / 'progress.json').read_text())['complete'])
                os.environ['ASB_FS_MARKER_SOCKET'] = str(Path(temporary) / 'missing.sock')
                os.environ['ASB_HERMES_NATIVE_SIDECAR'] = str(Path(temporary) / 'second.jsonl')
                os.environ['ASB_HERMES_NATIVE_PROGRESS'] = str(Path(temporary) / 'second-progress.json')
                failed = {'__name__':'marker_failure_test'}
                exec(compile(source, str(hook), 'exec'), failed)
                recorder = failed['Recorder']()
                self.assertEqual(recorder.invoke(lambda *_args:'still runs', 'echo', False, None,
                    None, None, False, '.', False, False, None), 'still runs')
                recorder.close()
                self.assertFalse(json.loads((Path(temporary) / 'second-progress.json').read_text())['complete'])

    def test_recorded_events_keep_independent_order_with_reused_payload(self):
        fuse = ModuleType('fuse')
        fuse.FUSE = fake_fuse
        fuse.FuseOSError = OSError
        fuse.Operations = object
        fuse.fuse_get_context = lambda: (0, 0, 0)
        script = Path(__file__).parents[1] / 'assets/benchmarks/asb_fs_proxy.py'
        with tempfile.TemporaryDirectory() as temporary, patch.dict(sys.modules, {'fuse': fuse}), \
                patch.dict('os.environ', {'ASB_PROBE_BENCHMARK_NO_IDENTITY': '1'}), \
                patch.object(sys, 'argv', [str(script), temporary, temporary, temporary]):
            namespace = runpy.run_path(str(script))
            proxy = namespace['Proxy']()
            proxy.record('read', '/first', b'same')
            proxy.record('read', '/second', b'same')
            proxy.log.close()
            proxy.payload.file.close()
            events = [json.loads(line) for line in (Path(temporary) / 'events.jsonl').read_text().splitlines()]
            self.assertEqual([event['sequence'] for event in events], [1, 2])
            self.assertEqual([event['payload_offset'] for event in events], [0, 0])
            self.assertEqual((Path(temporary) / 'payload.bin').read_bytes(), b'same')

    def test_reuse_keeps_exact_bytes_and_bounded_index(self):
        fuse = ModuleType('fuse')
        fuse.FUSE = fake_fuse
        fuse.FuseOSError = OSError
        fuse.Operations = object
        fuse.fuse_get_context = lambda: (0, 0, 0)
        script = Path(__file__).parents[1] / 'assets/benchmarks/asb_fs_proxy.py'
        with tempfile.TemporaryDirectory() as temporary, patch.dict(sys.modules, {'fuse': fuse}), \
                patch.object(sys, 'argv', [str(script), temporary, temporary, temporary]):
            namespace = runpy.run_path(str(script))
            path = Path(temporary) / 'payload.bin'
            store = namespace['PayloadStore'](path, capacity=2)
            self.addCleanup(store.file.close)
            first = store.append(b'first')
            second = store.append(b'second')
            self.assertEqual(store.append(b'first'), first)
            self.assertEqual(path.stat().st_size, 11)
            third = store.append(b'third')
            self.assertEqual(len(store.recent), 2)
            second_again = store.append(b'second')
            self.assertNotEqual(second_again, second)
            with path.open('rb') as source:
                for offset, value in [(first,b'first'), (second,b'second'),
                                      (third,b'third'), (second_again,b'second')]:
                    source.seek(offset)
                    self.assertEqual(source.read(len(value)), value)

    def test_hash_collision_never_reuses_different_bytes(self):
        fuse = ModuleType('fuse')
        fuse.FUSE = fake_fuse
        fuse.FuseOSError = OSError
        fuse.Operations = object
        fuse.fuse_get_context = lambda: (0, 0, 0)
        script = Path(__file__).parents[1] / 'assets/benchmarks/asb_fs_proxy.py'
        with tempfile.TemporaryDirectory() as temporary, patch.dict(sys.modules, {'fuse': fuse}), \
                patch.object(sys, 'argv', [str(script), temporary, temporary, temporary]):
            namespace = runpy.run_path(str(script))
            store = namespace['PayloadStore'](Path(temporary) / 'payload.bin')
            self.addCleanup(store.file.close)
            with patch.object(namespace['hashlib'], 'sha256') as digest:
                digest.return_value.digest.return_value = b'collision'
                first = store.append(b'abc')
                second = store.append(b'xyz')
            self.assertNotEqual(first, second)
            store.file.seek(second)
            self.assertEqual(store.file.read(3), b'xyz')


if __name__ == '__main__':
    unittest.main()
