"""Offline recorder lifecycle tests; no real mount or Docker is used."""
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / 'assets/benchmarks'))
from asb_fs_record import FilesystemRecorder


class RecorderTests(unittest.TestCase):
    def exercise(self, fail, existing_mount=False):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in ('workspace', 'run', 'binding'):
                (root / name).mkdir()
            for name in ('proxy.py', 'library.so'):
                (root / name).write_bytes(b'fixture')
            workspace = root / 'workspace'
            lower = root / 'run/filesystem-lower'
            mounted, calls = set(), []
            def run(command, **kwargs):
                calls.append(command)
                if command[:2] == ['mount','--bind']:
                    mounted.add(str(lower))
                elif command[0] == 'umount':
                    mounted.remove(command[1])
                return SimpleNamespace(returncode=0)
            class Process:
                def __init__(self): self.exit = 1 if fail else None
                def poll(self): return self.exit
                def wait(self, timeout): self.exit = 1 if fail else 0; return self.exit
            def spawn(*args, **kwargs):
                if not fail: mounted.add(str(workspace))
                return Process()
            recorder = FilesystemRecorder(workspace, root/'run', root/'proxy.py', root/'binding', root/'library.so')
            def mount_ids(path):
                original = {1} if existing_mount and path == workspace else set()
                return original | ({42} if str(path) in mounted else set())
            with patch('asb_fs_record.subprocess.run', side_effect=run), patch('asb_fs_record.subprocess.Popen', side_effect=spawn), patch('asb_fs_record._mount_ids', side_effect=mount_ids):
                if fail:
                    with self.assertRaisesRegex(RuntimeError, 'exited before mounting'):
                        recorder.__enter__()
                else:
                    with recorder:
                        self.assertIn(str(workspace), mounted)
            self.assertEqual(mounted, set())
            self.assertEqual(calls[-1], ['umount', str(lower)])
            self.assertTrue((root/'run/filesystem-recorder.log').is_file())

    def test_mount_lifecycle_settles(self):
        self.exercise(False)

    def test_failed_start_releases_backing_mount(self):
        self.exercise(True)

    def test_preserves_a_preexisting_workspace_mount(self):
        self.exercise(False, existing_mount=True)


if __name__ == '__main__':
    unittest.main()
