import importlib.util
import os
import shutil
from pathlib import Path
import stat
import struct
import subprocess
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[3] / 'assets/scripts/perf-symbol-custody.py'
spec = importlib.util.spec_from_file_location('perf_symbol_custody', SCRIPT)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class CustodyTest(unittest.TestCase):
    @unittest.skipUnless(shutil.which('cc') and shutil.which('objcopy'), 'requires compiler and binutils')
    def test_project_debug_file_must_match_and_never_replaces_existing_cache(self):
        with tempfile.TemporaryDirectory(prefix='asb-symbol-companion-') as directory:
            root = Path(directory); source = root / 'test.c'; binary = root / 'test'
            source.write_text('int main(void) { return 0; }\n')
            subprocess.run(['cc', '-g', '-Wl,--build-id', '-o', str(binary), str(source)], check=True)
            with binary.open('rb') as stream:
                buildid = module.elf_buildid(stream.fileno())
            image_dir = root / 'home/.debug/.build-id' / buildid[:2] / buildid[2:]
            image_dir.mkdir(parents=True)
            subprocess.run(['objcopy', '--strip-all', str(binary), str(image_dir / 'elf')], check=True)
            debug = root / 'symbols/.build-id' / buildid[:2] / (buildid[2:] + '.debug')
            debug.parent.mkdir(parents=True)
            subprocess.run(['objcopy', '--only-keep-debug', '--compress-debug-sections=zlib', str(binary), str(debug)], check=True)
            result = module.publish_host_debug(buildid, root / 'symbols', root / 'home', os.getuid(), os.getgid(), 16 * 1024 * 1024)
            self.assertEqual(result, debug.stat().st_size)
            self.assertEqual(module.publish_host_debug(buildid, root / 'symbols', root / 'home', os.getuid(), os.getgid(), 16 * 1024 * 1024), 0)
            (image_dir / 'debug').write_bytes(b'conflicting evidence')
            self.assertIsNone(module.publish_host_debug(buildid, root / 'symbols', root / 'home', os.getuid(), os.getgid(), 16 * 1024 * 1024))
            self.assertEqual((image_dir / 'debug').read_bytes(), b'conflicting evidence')

    def test_build_id_parser_reads_bounded_elf_notes(self):
        with tempfile.TemporaryFile() as image:
            header = bytearray(64)
            header[:6] = b'\x7fELF\x02\x01'
            struct.pack_into('<Q', header, 32, 64)
            struct.pack_into('<HH', header, 54, 56, 1)
            segment = bytearray(56)
            struct.pack_into('<I', segment, 0, 4)
            struct.pack_into('<Q', segment, 8, 120)
            struct.pack_into('<Q', segment, 32, 36)
            note = struct.pack('<III', 4, 20, 3) + b'GNU\0' + bytes(range(20))
            image.write(header + segment + note)
            image.flush()
            self.assertEqual(module.elf_buildid(image.fileno()), bytes(range(20)).hex())
            image.seek(54)
            image.write(struct.pack('<HH', 56, 513))
            image.flush()
            self.assertIsNone(module.elf_buildid(image.fileno()))

    def test_cache_verification_rejects_mismatches_and_unresolved_entries(self):
        buildid = bytes(range(20)).hex()
        with tempfile.TemporaryDirectory(prefix='asb-symbol-verify-') as root:
            cache = Path(root) / '.debug' / '.build-id' / buildid[:2] / buildid[2:]
            cache.mkdir(parents=True)
            header = bytearray(64)
            header[:6] = b'\x7fELF\x02\x01'
            struct.pack_into('<Q', header, 32, 64)
            struct.pack_into('<HH', header, 54, 56, 1)
            segment = bytearray(56)
            struct.pack_into('<I', segment, 0, 4)
            struct.pack_into('<Q', segment, 8, 120)
            struct.pack_into('<Q', segment, 32, 36)
            image = header + segment + struct.pack('<III', 4, 20, 3) + b'GNU\0' + bytes(range(20))
            (cache / 'elf').write_bytes(image)
            (cache / 'debug').write_bytes(image)  # matching ID but no DWARF
            listing = f'{buildid} /guest/agent\n   /tmp/perf-jit.map\n{buildid} [kernel.kallsyms]\n'
            result = module.verify_buildids(listing, root)
            self.assertEqual(result['missing'], [])
            self.assertEqual(result['missing_debug'], [buildid])
            self.assertEqual(result['unverified'], 2)
            (cache / 'elf').write_bytes(b'not an ELF')
            self.assertEqual(module.verify_buildids(listing, root)['missing'], [buildid])
            with self.assertRaises(ValueError):
                module.verify_buildids('', root)

    def test_generation_checked_for_root_and_file(self):
        pid = os.getpid()
        _, start = module.identity(pid)
        self.assertIsNotNone(module.tree([(pid, start)]))
        self.assertIsNone(module.tree([(pid, start + 1)]))
        self.assertFalse(module.cache_mapped_file(pid, start + 1, '/bin/sh', (0, 0), '/', '/bin/false', os.getuid(), os.getgid()))

    def test_observed_child_remains_tracked_after_parent_exits(self):
        program = "import subprocess,time; p=subprocess.Popen(['/bin/sleep','20']); print(p.pid,flush=True); time.sleep(20)"
        parent = subprocess.Popen(['/usr/bin/python3', '-c', program], stdout=subprocess.PIPE, text=True)
        child = int(parent.stdout.readline())
        try:
            _, start = module.identity(parent.pid)
            first = module.tree([(parent.pid, start)])
            self.assertTrue(any(pid == child for pid, _ in first))
            parent.terminate()
            parent.wait(timeout=5)
            after = module.tree(first)
            self.assertTrue(any(pid == child for pid, _ in after))
        finally:
            try:
                os.kill(child, 15)
            except ProcessLookupError:
                pass
            if parent.poll() is None:
                parent.terminate()
                parent.wait(timeout=5)
            parent.stdout.close()

    def test_mapped_elf_passes_open_descriptor_to_unprivileged_tool(self):
        pid = os.getpid()
        _, start = module.identity(pid)
        matches = []
        for path, key in module.mappings(pid):
            try:
                with open(f'/proc/{pid}/root{path}', 'rb') as stream:
                    if stream.read(4) == b'\x7fELF':
                        matches.append((path, key))
            except OSError:
                continue
        self.assertTrue(matches)
        with tempfile.TemporaryDirectory(prefix='asb-symbol-custody-') as root:
            program = Path(root) / 'perf'
            program.write_text("#!/usr/bin/python3\nimport os,sys\nassert open(sys.argv[3], 'rb').read(4) == b'\\x7fELF'\nopen(os.path.join(os.environ['HOME'], 'ok'), 'w').write(str(os.getuid()))\n")
            program.chmod(stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR)
            path, key = matches[0]
            self.assertTrue(module.cache_mapped_file(pid, start, path, key, root, str(program), os.getuid(), os.getgid()))
            self.assertEqual((Path(root) / 'ok').read_text(), str(os.getuid()))
            self.assertFalse(module.cache_mapped_file(pid, start, path, (key[0], key[1] + 1), root, str(program), os.getuid(), os.getgid()))
            with mock.patch.object(module.subprocess, 'run') as publish:
                self.assertFalse(module.cache_mapped_file(pid, start, path, key, root, str(program), os.getuid(), os.getgid(), expected_buildid='deadbeef'))
                publish.assert_not_called()


    def test_changing_source_is_never_published(self):
        pid = os.getpid()
        _, start = module.identity(pid)
        with tempfile.TemporaryDirectory(prefix='asb-symbol-race-') as root:
            source = Path(root) / 'image'
            source.write_bytes(b'\x7fELF' + b'x' * 1024)
            file_stat = source.stat()
            original = os.pread
            calls = 0

            def changing(fd, size, offset):
                nonlocal calls
                data = original(fd, size, offset)
                calls += 1
                if calls == 2:
                    source.write_bytes(b'\x7fELF' + b'y' * 2048)
                return data

            with mock.patch.object(module.os, 'pread', side_effect=changing), \
                 mock.patch.object(module.subprocess, 'run') as publish:
                accepted = module.cache_mapped_file(pid, start, str(source), (file_stat.st_dev, file_stat.st_ino), root, '/bin/false', os.getuid(), os.getgid())
            self.assertFalse(accepted)
            publish.assert_not_called()
            self.assertEqual(list(Path(root).glob('.asb-perf-image-*')), [])


if __name__ == '__main__':
    unittest.main()
