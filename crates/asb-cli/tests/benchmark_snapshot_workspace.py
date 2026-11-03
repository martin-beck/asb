"""Offline failure-path tests for experimental workspace provisioning."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[3]
sys.path.insert(0,str(ROOT/'crates/asb-cli/assets/benchmarks'))
from asb_snapshot_workspace import SnapshotWorkspace

SCRATCH=ROOT/'target/snapshot-workspace-tests'
SCRATCH.mkdir(parents=True,exist_ok=True)


class WorkspaceTest(unittest.TestCase):
    def test_loop_allocation_retries_busy_without_detaching_another_owner(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);source=root/'source';source.mkdir();calls=[]
            def run(args,**kwargs):
                calls.append(args)
                if args[:2] == ['losetup','/dev/loop42']:
                    raise subprocess.CalledProcessError(1,args,stderr=b'Device or resource busy')
                return subprocess.CompletedProcess(args,0)
            workspace=SnapshotWorkspace(root/'image',root/'mount',1024,['mkfs'],['btrfs'],['rsync'])
            with patch('asb_snapshot_workspace.subprocess.run',side_effect=run), \
                 patch('builtins.open',unittest.mock.mock_open()), \
                 patch('asb_snapshot_workspace.fcntl.ioctl',side_effect=[42,43]), \
                 patch('asb_snapshot_workspace.os.mknod'):
                workspace.prepare(source)
                workspace.close()
            self.assertIn(['losetup','-d','/dev/loop43'],calls)
            self.assertNotIn(['losetup','-d','/dev/loop42'],calls)

    def test_failed_copy_can_detach_owned_resources(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);source=root/'source';source.mkdir();calls=[]
            def run(args,**kwargs):
                calls.append(args)
                if args[0]=='rsync':raise subprocess.CalledProcessError(1,args)
                return subprocess.CompletedProcess(args,0)
            workspace=SnapshotWorkspace(root/'image',root/'mount',1024,['mkfs'],['btrfs'],['rsync'])
            with patch('asb_snapshot_workspace.subprocess.run',side_effect=run), \
                 patch('builtins.open',unittest.mock.mock_open()), \
                 patch('asb_snapshot_workspace.fcntl.ioctl',return_value=42), \
                 patch('asb_snapshot_workspace.os.mknod'):
                with self.assertRaisesRegex(RuntimeError,'preparation failed'):workspace.prepare(source)
                self.assertIsNone(workspace.loop)
                self.assertFalse(workspace.mounted)
                workspace.close()
            self.assertIn(['umount',str(root/'mount')],calls)
            self.assertIn(['losetup','-d','/dev/loop42'],calls)
            self.assertTrue((root/'image').exists())

    def test_unavailable_source_does_not_create_image(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory)
            workspace=SnapshotWorkspace(root/'image',root/'mount',1024,['mkfs'],['btrfs'],['rsync'])
            with self.assertRaisesRegex(ValueError,'source is unavailable'):workspace.prepare(root/'missing')
            workspace.close()
            self.assertFalse((root/'image').exists())

    def test_failed_unmount_keeps_loop_for_recovery(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);source=root/'source';source.mkdir();calls=[]
            def run(args,**kwargs):
                calls.append(args)
                if args[0] in ('rsync','umount'):raise subprocess.CalledProcessError(1,args)
                return subprocess.CompletedProcess(args,0)
            workspace=SnapshotWorkspace(root/'image',root/'mount',1024,['mkfs'],['btrfs'],['rsync'])
            with patch('asb_snapshot_workspace.subprocess.run',side_effect=run), \
                 patch('builtins.open',unittest.mock.mock_open()), \
                 patch('asb_snapshot_workspace.fcntl.ioctl',return_value=42), \
                 patch('asb_snapshot_workspace.os.mknod'):
                with self.assertRaisesRegex(RuntimeError,'cleanup did not settle'):workspace.prepare(source)
            self.assertTrue(workspace.mounted)
            self.assertEqual(workspace.loop,'/dev/loop42')
            self.assertNotIn(['losetup','-d','/dev/loop42'],calls)


if __name__=='__main__':unittest.main()
