import importlib.util
import contextlib
import io
import json
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
    def test_guest_helper_output_is_bounded_and_timeout_is_enforced(self):
        with tempfile.TemporaryDirectory(prefix='asb-guest-command-') as directory:
            root = Path(directory); docker = root / 'docker'; script = root / 'helper'
            script.write_text('fixture')
            docker.write_text('#!/usr/bin/python3\nimport sys,time\n'
                              'data=sys.stdin.read()\n'
                              'time.sleep(0.2 if "slow" in sys.argv else 0)\n'
                              'sys.stdout.buffer.write(b"x" * 100)\n')
            docker.chmod(0o755)
            with mock.patch.dict(os.environ, {'PATH': str(root)}):
                with tempfile.TemporaryFile(dir=root) as out:
                    self.assertTrue(module.guest_command('a' * 64, script, ['scan'], out, 100, 2))
                    self.assertEqual(out.tell(), 100)
                with tempfile.TemporaryFile(dir=root) as out:
                    self.assertFalse(module.guest_command('a' * 64, script, ['scan'], out, 99, 2))
                with tempfile.TemporaryFile(dir=root) as out:
                    self.assertFalse(module.guest_command('a' * 64, script, ['slow'], out, 100, 0.01))
                with tempfile.TemporaryFile(dir=root) as out:
                    self.assertFalse(module.guest_command('invalid', script, ['scan'], out, 100, 2))

    @unittest.skipUnless(shutil.which('cc'), 'requires a C compiler')
    def test_symfs_preserves_verified_images_and_reports_missing_images(self):
        with tempfile.TemporaryDirectory(prefix='asb-symbol-mirror-') as directory:
            root = Path(directory); source = root / 'test.c'; binary = root / 'binary'
            source.write_text('int main(void) { return 0; }\n')
            subprocess.run(['cc', '-g', '-Wl,--build-id', '-o', str(binary), str(source)], check=True)
            with binary.open('rb') as image:
                buildid = module.elf_buildid(image.fileno())
            cached = root / '.debug/.build-id' / buildid[:2] / buildid[2:] / 'elf'
            cached.parent.mkdir(parents=True)
            shutil.copyfile(binary, cached)
            noid = root / 'symfs/guest/noid'
            noid.parent.mkdir(parents=True)
            noid.write_bytes(b'\x7fELFno-build-id')
            listing = f'{buildid} /guest/withid\n    /guest/noid\n'
            self.assertFalse(module.prepare_symfs(listing, root, {'/guest/noid'}))
            self.assertFalse(module.prepare_symfs(listing + 'deadbeef /guest/noid\n', root, set()))
            self.assertFalse((root / 'symfs-ready').exists())
            self.assertTrue(module.prepare_symfs(listing + 'deadbeef /guest/missing-known\n    /guest/missing-noid\n', root, set()))
            self.assertTrue((root / 'symfs-ready').is_file())
            marker = json.loads((root / 'symfs-ready').read_text())
            self.assertEqual(marker['missing_no_build_id_images'], 1)
            self.assertEqual(marker['missing_build_id_images'], 1)
            self.assertTrue(os.path.samefile(cached, root / 'symfs/guest/withid'))

    def test_container_identity_requires_one_cgroup_and_same_process_generation(self):
        container = 'b' * 64
        with mock.patch.object(module, 'identity', side_effect=[(1, 7), (1, 7)]), \
             mock.patch.object(Path, 'read_text', return_value=f'0::/system.slice/docker-{container}.scope\n'):
            self.assertEqual(module.container_for(123, 7), container)
        with mock.patch.object(module, 'identity', side_effect=[(1, 7), (1, 8)]), \
             mock.patch.object(Path, 'read_text', return_value=f'0::/docker/{container}\n'):
            self.assertIsNone(module.container_for(123, 7))
        with mock.patch.object(module, 'identity', side_effect=[(1, 7), (1, 7)]), \
             mock.patch.object(Path, 'read_text', return_value=f'0::/{container}/{"c" * 64}\n'):
            self.assertIsNone(module.container_for(123, 7))
        with mock.patch.object(module, 'identity', side_effect=[(1, 7), (1, 7)]), \
             mock.patch.object(Path, 'read_text', return_value=f'0::/other/{container}\n'):
            self.assertIsNone(module.container_for(123, 7))

    @unittest.skipUnless(shutil.which('cc'), 'requires a C compiler')
    def test_container_export_is_bounded_and_build_id_checked(self):
        with tempfile.TemporaryDirectory(prefix='asb-symbol-container-') as directory:
            root = Path(directory); source = root / 'test.c'; binary = root / 'test'
            source.write_text('int main(void) { return 0; }\n')
            subprocess.run(['cc', '-g', '-Wl,--build-id', '-o', str(binary), str(source)], check=True)
            with binary.open('rb') as stream:
                buildid = module.elf_buildid(stream.fileno())
            perf = root / 'perf'
            perf.write_text('#!/bin/sh\nexit 0\n'); perf.chmod(0o755)
            original = subprocess.run
            def export(args, **kwargs):
                if args[0] == 'docker':
                    self.assertEqual(args[1:6], ['exec', '--user', '0', '--workdir', '/'])
                    kwargs['stdout'].write(binary.read_bytes())
                    kwargs['stdout'].flush()
                    return subprocess.CompletedProcess(args, 0)
                return original(args, **kwargs)
            with mock.patch.object(module.subprocess, 'run', side_effect=export):
                args = ('a' * 64, '/usr/bin/test', buildid, str(root), str(perf), os.getuid(), os.getgid())
                self.assertTrue(module.cache_container_file(*args, 1024 * 1024))
                self.assertFalse(module.cache_container_file(*args, 8))
                self.assertFalse(module.cache_container_file('x' * 64, *args[1:], 1024 * 1024))
                self.assertFalse(module.cache_container_file(args[0], '/../../etc/passwd', *args[2:], 1024 * 1024))
                self.assertFalse(module.cache_container_file(args[0], args[1], 'deadbeef', *args[3:], 1024 * 1024))

    def test_inaccessible_guest_root_reports_the_permission_problem(self):
        stderr = io.StringIO()
        argv = ['helper', '12', '34', '1000', '1000', '/cache', '/perf',
                '/symbols', 'fifo:/request,/ack', '56', '78', '/perf.data']
        with mock.patch.object(module.sys, 'argv', argv), \
             mock.patch.object(module, 'identity', side_effect=[(1, 34), (1, 78)]), \
             mock.patch.object(module, 'container_for', return_value=None), \
             mock.patch.object(module.os, 'open', side_effect=PermissionError('denied')), \
             contextlib.redirect_stderr(stderr):
            self.assertEqual(module.main(), 2)
        self.assertIn('Cannot access the benchmark process root', stderr.getvalue())
        self.assertIn('denied', stderr.getvalue())

    @unittest.skipUnless(shutil.which('cc'), 'requires a C compiler')
    def test_settlement_recovers_short_lived_recorded_image_from_pinned_root(self):
        with tempfile.TemporaryDirectory(prefix='asb-symbol-settle-') as directory:
            root = Path(directory); guest = root / 'guest'; guest.mkdir()
            source = root / 'test.c'; source.write_text('int main(void) { return 0; }\n')
            image = guest / 'short-lived'
            subprocess.run(['cc', '-g', '-Wl,--build-id', '-o', str(image), str(source)], check=True)
            with image.open('rb') as stream:
                buildid = module.elf_buildid(stream.fileno())
            home = root / 'home'; home.mkdir()
            perf = root / 'perf'
            perf.write_text("#!/usr/bin/python3\nimport os,pathlib,shutil,sys\n"
                f"identity={buildid!r}\n"
                "if sys.argv[1]=='buildid-list': print(identity+' /short-lived')\n"
                "else:\n p=pathlib.Path(os.environ['HOME'])/'.debug/.build-id'/identity[:2]/identity[2:];p.mkdir(parents=True,exist_ok=True);shutil.copyfile(sys.argv[3],p/'elf')\n")
            perf.chmod(0o755)
            root_fd = os.open(guest, os.O_PATH | os.O_DIRECTORY)
            try:
                result = module.settle_recorded_images(root_fd, str(root / 'perf.data'), str(home), str(perf), None, os.getuid(), os.getgid(), 16 * 1024 * 1024)
                self.assertEqual(result['recovered_recorded_images'], 1)
                self.assertEqual(result['missing_recorded_images'], 0)
                self.assertFalse(module.cache_pinned_file(root_fd, '/../test', buildid, str(home), str(perf), os.getuid(), os.getgid(), 1000000))
                self.assertFalse(module.cache_pinned_file(root_fd, '/short-lived', 'deadbeef', str(home), str(perf), os.getuid(), os.getgid(), 1000000))
            finally:
                os.close(root_fd)

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
