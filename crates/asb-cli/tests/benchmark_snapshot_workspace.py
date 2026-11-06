"""Offline failure-path tests for experimental workspace provisioning."""
from pathlib import Path
import json
import os
import stat
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[3]
sys.path.insert(0,str(ROOT/'crates/asb-cli/assets/benchmarks'))
from asb_snapshot_workspace import SnapshotWorkspace

SCRATCH=ROOT/'target/snapshot-workspace-tests'
SCRATCH.mkdir(parents=True,exist_ok=True)


class WorkspaceTest(unittest.TestCase):
    def test_replay_creates_workspace_from_received_snapshot_without_copying_source(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory).resolve();source=root/'source';source.mkdir()
            workspace=SnapshotWorkspace(root/'image',root/'mount',1024**3,['mkfs'],['btrfs'],['rsync'])
            with patch('asb_snapshot_workspace.subprocess.run') as run, \
                 patch('builtins.open',unittest.mock.mock_open()), \
                 patch('asb_snapshot_workspace.fcntl.ioctl',return_value=42), \
                 patch('asb_snapshot_workspace.os.mknod'):
                mounted=workspace.prepare(source,seed=False)
                self.assertFalse(mounted.exists())
                baseline=root/'mount'/'snapshots'/'snapshot-00000001';baseline.mkdir(parents=True)
                self.assertEqual(workspace.restore_baseline(baseline),mounted)
                commands=[call.args[0] for call in run.call_args_list]
                self.assertIn(['btrfs','subvolume','snapshot',str(baseline),str(mounted)],commands)
                self.assertFalse(any(command[0]=='rsync' for command in commands))
                workspace.close()

    def test_replay_refuses_missing_or_already_restored_baseline(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);workspace=SnapshotWorkspace(root/'image',root/'mount',1024,['mkfs'],['btrfs'],['rsync'])
            workspace.mounted=True;workspace.mountpoint.mkdir()
            with self.assertRaisesRegex(ValueError,'baseline or workspace is unavailable'):
                workspace.restore_baseline(root/'missing')
            baseline=root/'baseline';baseline.mkdir();(workspace.mountpoint/'workspace').mkdir()
            with self.assertRaisesRegex(ValueError,'baseline or workspace is unavailable'):
                workspace.restore_baseline(baseline)

    def test_multiple_roots_share_one_snapshot_and_preserve_contents(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory).resolve();source=root/'source';extra=root/'extra'
            source.mkdir();extra.mkdir()
            (source/'task').write_text('workspace');(extra/'patch').write_text('temporary')
            workspace=SnapshotWorkspace(root/'image',root/'mount',1024**3,['mkfs'],['btrfs'],['rsync'])
            real_run=subprocess.run
            def run(args,**kwargs):
                if args[:3] == ['btrfs','subvolume','create']:
                    Path(args[3]).mkdir()
                elif args[0] == 'rsync':
                    return real_run(args,**kwargs)
                return subprocess.CompletedProcess(args,0)
            with patch('asb_snapshot_workspace.subprocess.run',side_effect=run), \
                 patch('builtins.open',unittest.mock.mock_open()), \
                 patch('asb_snapshot_workspace.fcntl.ioctl',return_value=42), \
                 patch('asb_snapshot_workspace.os.mknod'):
                mounted=workspace.prepare(source,[extra])
                self.assertEqual((mounted/'0'/'task').read_text(),'workspace')
                self.assertEqual((mounted/'1'/'patch').read_text(),'temporary')
                self.assertEqual(json.loads((mounted/'.asb-snapshot-roots.json').read_text()),
                                 [str(source),str(extra)])
                workspace.close()

    def test_multiple_roots_reject_overlap_before_allocating_storage(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory).resolve();source=root/'source';nested=source/'nested'
            nested.mkdir(parents=True)
            workspace=SnapshotWorkspace(root/'image',root/'mount',1024,['mkfs'],['btrfs'],['rsync'])
            with self.assertRaisesRegex(ValueError,'must not overlap'):
                workspace.prepare(source,[nested])
            self.assertFalse((root/'image').exists())

    def test_automatic_image_capacity_tracks_available_project_storage(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);source=root/'source';source.mkdir()
            workspace=SnapshotWorkspace(root/'image',root/'mount',None,['mkfs'],['btrfs'],['rsync'])
            space=SimpleNamespace(f_bavail=6*1024*1024,f_frsize=1024)
            with patch('asb_snapshot_workspace.os.statvfs',return_value=space) as statvfs, \
                 patch('asb_snapshot_workspace.subprocess.run'), \
                 patch('builtins.open',unittest.mock.mock_open()), \
                 patch('asb_snapshot_workspace.fcntl.ioctl',return_value=42), \
                 patch('asb_snapshot_workspace.os.mknod'):
                workspace.prepare(source);workspace.close()
            statvfs.assert_called_once_with(root)
            self.assertEqual((root/'image').stat().st_size,6*1024**3)

    def test_automatic_capacity_failure_does_not_create_an_image(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);source=root/'source';source.mkdir()
            workspace=SnapshotWorkspace(root/'image',root/'mount',None,['mkfs'],['btrfs'],['rsync'])
            with patch('asb_snapshot_workspace.os.statvfs',return_value=SimpleNamespace(f_bavail=0,f_frsize=4096)):
                with self.assertRaisesRegex(RuntimeError,'no available space'):workspace.prepare(source)
            self.assertFalse((root/'image').exists())

    def test_automatic_capacity_uses_block_size_when_fragment_size_is_zero(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);source=root/'source';source.mkdir()
            workspace=SnapshotWorkspace(root/'image',root/'mount',None,['mkfs'],['btrfs'],['rsync'])
            space=SimpleNamespace(f_bavail=1024,f_frsize=0,f_bsize=4096)
            with patch('asb_snapshot_workspace.os.statvfs',return_value=space), \
                 patch('asb_snapshot_workspace.subprocess.run'), \
                 patch('builtins.open',unittest.mock.mock_open()), \
                 patch('asb_snapshot_workspace.fcntl.ioctl',return_value=42), \
                 patch('asb_snapshot_workspace.os.mknod'):
                workspace.prepare(source);workspace.close()
            self.assertEqual((root/'image').stat().st_size,4*1024**2)

    def test_capacity_probe_error_does_not_claim_an_image_was_retained(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);source=root/'source';source.mkdir()
            workspace=SnapshotWorkspace(root/'image',root/'mount',None,['mkfs'],['btrfs'],['rsync'])
            with patch('asb_snapshot_workspace.os.statvfs',side_effect=OSError('unavailable')):
                with self.assertRaisesRegex(RuntimeError,'cannot determine snapshot storage capacity'):
                    workspace.prepare(source)
            self.assertFalse((root/'image').exists())

    def test_non_sparse_storage_does_not_expand_the_image_to_all_free_space(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root=Path(directory);source=root/'source';source.mkdir()
            workspace=SnapshotWorkspace(root/'image',root/'mount',None,['mkfs'],['btrfs'],['rsync'])
            space=SimpleNamespace(f_bavail=100*1024*1024,f_frsize=1024)
            with patch('asb_snapshot_workspace.os.statvfs',return_value=space), \
                 patch('asb_snapshot_workspace.os.fstat',return_value=SimpleNamespace(st_blocks=8*1024**2//512)), \
                 patch('asb_snapshot_workspace.subprocess.run') as run:
                with self.assertRaisesRegex(RuntimeError,'snapshot storage is not sparse'):
                    workspace.prepare(source)
            self.assertEqual((root/'image').stat().st_size,8*1024**2)
            run.assert_not_called()

    def test_creates_missing_loop_device_before_attaching(self):
        with tempfile.TemporaryDirectory(dir=SCRATCH) as directory:
            root = Path(directory); source = root/'source'; source.mkdir()
            workspace = SnapshotWorkspace(root/'image',root/'mount',1024,['mkfs'],['btrfs'],['rsync'])
            real_exists = Path.exists
            def exists(path):
                return False if path == Path('/dev/loop42') else real_exists(path)
            with patch.object(Path,'exists',autospec=True,side_effect=exists), \
                 patch('asb_snapshot_workspace.subprocess.run') as run, \
                 patch('builtins.open',unittest.mock.mock_open()), \
                 patch('asb_snapshot_workspace.fcntl.ioctl',return_value=42), \
                 patch('asb_snapshot_workspace.os.mknod') as mknod:
                workspace.prepare(source)
                workspace.close()
            mknod.assert_called_once_with(Path('/dev/loop42'),stat.S_IFBLK | 0o600,os.makedev(7,42))
            commands = [call.args[0] for call in run.call_args_list]
            self.assertIn(['losetup','/dev/loop42',str(root/'image')],commands)
            self.assertIn(['losetup','-d','/dev/loop42'],commands)

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
