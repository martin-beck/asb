"""Offline checks for guest-local libfuse build orchestration."""
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / 'assets/benchmarks'))
from asb_fuse_build import build_libfuse


class FuseBuildTests(unittest.TestCase):
    def test_build_runs_bounded_argument_arrays_in_guest_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'source.tar.gz'
            source.write_bytes(b'fixture')
            destination = root / 'build'
            commands = []
            def run(command, **kwargs):
                commands.append(command)
                self.assertEqual(kwargs['cwd'], destination)
                if command[0] == 'make':
                    library = destination / 'lib/.libs/libfuse.so.2.9.9'
                    library.parent.mkdir(parents=True)
                    library.write_bytes(b'library')
                return SimpleNamespace(returncode=0)
            with patch('asb_fuse_build.shutil.which', return_value='/usr/bin/fixture'), patch('asb_fuse_build.subprocess.run', side_effect=run):
                self.assertTrue(build_libfuse(source, destination).is_file())
            self.assertEqual([Path(command[0]).name for command in commands], ['tar','configure','make'])
            self.assertTrue((destination / 'build.log').is_file())

    def test_missing_compiler_is_reported_before_build(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'source.tar.gz'
            source.write_bytes(b'fixture')
            with patch('asb_fuse_build.shutil.which', side_effect=lambda name: None if name == 'cc' else '/usr/bin/fixture'):
                with self.assertRaisesRegex(RuntimeError, 'cc'):
                    build_libfuse(source, root / 'build')
            self.assertFalse((root / 'build').exists())


if __name__ == '__main__':
    unittest.main()
