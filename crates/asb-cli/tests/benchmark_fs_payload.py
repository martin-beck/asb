"""Offline coverage for the filesystem recorder's payload storage."""
from pathlib import Path
import json
import runpy
import sys
import tempfile
from types import ModuleType
import unittest
from unittest.mock import patch


def fake_fuse(proxy, *args, **kwargs):
    proxy.log.close()
    proxy.payload.file.close()


class PayloadTests(unittest.TestCase):
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
