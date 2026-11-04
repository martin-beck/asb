import importlib.util
import io
import os
import shutil
import subprocess
from pathlib import Path
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[3] / 'assets/scripts/perf-guest-mappings.py'
spec = importlib.util.spec_from_file_location('perf_guest_mappings', SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def process(root, pid, parent, start, maps=''):
    directory = root / str(pid)
    directory.mkdir()
    fields = ['S', str(parent)] + ['0'] * 17 + [str(start)]
    (directory / 'stat').write_text(f'{pid} (test worker) ' + ' '.join(fields) + '\n')
    (directory / 'maps').write_text(maps)


class GuestMappingsTest(unittest.TestCase):
    @unittest.skipUnless(shutil.which('cc'), 'requires a C compiler')
    def test_detects_build_id_and_absence_without_classifying_invalid_as_present(self):
        with tempfile.TemporaryDirectory() as path:
            root = Path(path); source = root / 'test.c'; source.write_text('int main(void) { return 0; }\n')
            for name, flags, expected in [('withid', ['-Wl,--build-id'], True),
                                          ('noid', ['-Wl,--build-id=none'], False)]:
                binary = root / name
                subprocess.run(['cc', *flags, '-o', str(binary), str(source)], check=True)
                with binary.open('rb') as image:
                    self.assertEqual(module.has_build_id(image.fileno()), expected)
            invalid = root / 'invalid'; invalid.write_bytes(b'\x7fELF')
            with invalid.open('rb') as image:
                self.assertFalse(module.has_build_id(image.fileno()))

    def test_copy_requires_the_observed_inode_and_live_generation(self):
        with tempfile.TemporaryDirectory() as path:
            proc = Path(path)
            process(proc, 10, 1, 100)
            root = proc / '10' / 'root' / 'usr' / 'bin'
            root.mkdir(parents=True)
            image = root / 'tool'; image.write_bytes(b'\x7fELFpayload')
            info = image.stat()
            args = (proc, 10, 100, '/usr/bin/tool', os.major(info.st_dev), os.minor(info.st_dev), info.st_ino)
            output = io.BytesIO()
            self.assertEqual(module.copy_image(*args, output), image.stat().st_size)
            self.assertEqual(output.getvalue(), image.read_bytes())
            stamp = (info.st_size, info.st_mtime_ns, info.st_ctime_ns)
            self.assertEqual(module.copy_image(*args, io.BytesIO(), expected_stamp=stamp), info.st_size)
            with self.assertRaisesRegex(ValueError, 'observed mapping'):
                module.copy_image(*args, io.BytesIO(), expected_stamp=(info.st_size, info.st_mtime_ns + 1, info.st_ctime_ns))
            class ShortWriter(io.BytesIO):
                def write(self, data):
                    return super().write(data[:1])
            short = ShortWriter()
            module.copy_image(*args, short)
            self.assertEqual(short.getvalue(), image.read_bytes())
            class MutatingWriter(io.BytesIO):
                def write(self, data):
                    result = super().write(data)
                    image.write_bytes(b'\x7fELFchanged')
                    return result
            with self.assertRaisesRegex(ValueError, 'changed during capture'):
                module.copy_image(*args, MutatingWriter())
            with self.assertRaisesRegex(ValueError, 'observed mapping'):
                module.copy_image(*args[:-1], info.st_ino + 1, io.BytesIO())
            with self.assertRaisesRegex(ValueError, 'observed mapping'):
                module.copy_image(*args, io.BytesIO(), limit=4)
            with self.assertRaisesRegex(ValueError, 'generation'):
                module.copy_image(proc, 10, 99, *args[3:], io.BytesIO())
            with self.assertRaisesRegex(ValueError, 'invalid'):
                module.copy_image(proc, 10, 100, '/../tool', *args[4:], io.BytesIO())

    def test_stamped_scan_verifies_the_mapped_inode(self):
        with tempfile.TemporaryDirectory() as path:
            proc = Path(path)
            process(proc, 10, 1, 100)
            root = proc / '10' / 'root' / 'usr' / 'bin'; root.mkdir(parents=True)
            image = root / 'tool'; image.write_bytes(b'\x7fELFpayload')
            info = image.stat()
            (proc / '10' / 'maps').write_text(
                f'001-002 r-xp 0 {os.major(info.st_dev):x}:{os.minor(info.st_dev):x} {info.st_ino} /usr/bin/tool\n')
            row = module.scan(proc, [(10, 100)], include_stamps=True)['mappings'][0]
            self.assertEqual((row['size'], row['mtime_ns'], row['ctime_ns']),
                             (info.st_size, info.st_mtime_ns, info.st_ctime_ns))

    def test_follows_only_verified_descendants_and_retains_reparented_roots(self):
        with tempfile.TemporaryDirectory() as path:
            proc = Path(path)
            mapping = '001-002 r-xp 0 08:01 12 /usr/bin/python3\n'
            process(proc, 10, 1, 100, mapping)
            process(proc, 11, 10, 101, mapping)
            process(proc, 12, 1, 102, mapping)
            result = module.scan(proc, [(10, 100)])
            self.assertEqual(set(map(tuple, result['members'])), {(10, 100), (11, 101)})
            self.assertEqual({item['pid'] for item in result['mappings']}, {10, 11})
            self.assertEqual(result['mappings'][0]['inode'], 12)
            process_root = proc / '10' / 'stat'
            process_root.unlink()
            result = module.scan(proc, [(10, 100), (11, 101)])
            self.assertEqual(result['members'], [(11, 101)])
            with self.assertRaisesRegex(ValueError, 'no longer live'):
                module.scan(proc, [(11, 100)])

    def test_rejects_oversized_and_traversal_paths(self):
        with tempfile.TemporaryDirectory() as path:
            proc = Path(path)
            process(proc, 10, 1, 100, '001-002 r-xp 0 08:01 12 /safe/../other\n')
            with self.assertRaisesRegex(ValueError, 'traversal'):
                module.scan(proc, [(10, 100)])
            (proc / '10' / 'maps').write_text('001-002 r-xp 0 08:01 12 /' + 'a' * 5000 + '\n')
            with self.assertRaisesRegex(ValueError, 'bound'):
                module.scan(proc, [(10, 100)])

    def test_disappearing_process_does_not_emit_stale_mappings(self):
        with tempfile.TemporaryDirectory() as path:
            proc = Path(path)
            process(proc, 10, 1, 100, '001-002 r-xp 0 08:01 12 /usr/bin/test\n')
            real = module.identity
            calls = 0
            def reused(root, pid):
                nonlocal calls
                calls += 1
                return (1, 101) if calls >= 3 else real(root, pid)
            with mock.patch.object(module, 'identity', side_effect=reused):
                self.assertEqual(module.scan(proc, [(10, 100)])['mappings'], [])


if __name__ == '__main__':
    unittest.main()
