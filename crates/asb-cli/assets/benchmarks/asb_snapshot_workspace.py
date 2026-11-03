"""Experimental snapshot workspace provisioner for a disposable guest or VM."""
from pathlib import Path
import fcntl
import os
import stat
import subprocess


class SnapshotWorkspace:
    """Own a sparse filesystem image, dynamic loop device and task subvolume."""
    def __init__(self, image, mountpoint, size, mkfs, btrfs, rsync):
        self.image, self.mountpoint = Path(image), Path(mountpoint)
        self.size, self.mkfs, self.btrfs, self.rsync = size, list(mkfs), list(btrfs), list(rsync)
        if type(size) is not int or size <= 0 or not all((self.mkfs, self.btrfs, self.rsync)):
            raise ValueError('snapshot workspace configuration is invalid')
        self.loop = None
        self.mounted = False

    def prepare(self, source):
        source = Path(source)
        if not source.is_dir():
            raise ValueError('snapshot workspace source is unavailable')
        try:
            with self.image.open('xb') as image:
                image.truncate(self.size)
            subprocess.run(self.mkfs + ['-q', str(self.image)], check=True, stdout=subprocess.DEVNULL, timeout=60)
            # The container may expose loop-control without pre-created loop
            # nodes. Allocate a free number dynamically and create its node
            # inside the guest, rather than requiring a particular host device.
            with open('/dev/loop-control', 'rb', buffering=0) as control:
                number = fcntl.ioctl(control.fileno(), 0x4C82)
            node = Path('/dev') / f'loop{number}'
            if not node.exists():
                os.mknod(node, stat.S_IFBLK | 0o600, os.makedev(7, number))
            subprocess.run(['losetup', str(node), str(self.image)], check=True, timeout=60)
            self.loop = str(node)
            if not self.loop.startswith('/dev/loop') or not self.loop[9:].isdigit():
                raise RuntimeError('snapshot loop device result is invalid')
            self.mountpoint.mkdir(parents=True, exist_ok=False)
            subprocess.run(['mount', '-t', 'btrfs', self.loop, str(self.mountpoint)], check=True, timeout=60)
            self.mounted = True
            workspace = self.mountpoint/'workspace'
            subprocess.run(self.btrfs + ['subvolume', 'create', str(workspace)], check=True,
                           stdout=subprocess.DEVNULL, timeout=60)
            subprocess.run(self.rsync + ['-aHAX', str(source)+'/', str(workspace)+'/'], check=True,
                           stdout=subprocess.DEVNULL, timeout=300)
            return workspace
        except BaseException as error:
            try:
                self.close()
            except BaseException as cleanup:
                raise RuntimeError('snapshot workspace preparation failed and cleanup did not settle; retained image') from cleanup
            if isinstance(error, (OSError, subprocess.SubprocessError)):
                raise RuntimeError('snapshot workspace preparation failed; retained image') from error
            raise

    def close(self):
        if self.mounted:
            try:
                subprocess.run(['umount', str(self.mountpoint)], check=True, timeout=60)
            except (OSError, subprocess.SubprocessError) as error:
                raise RuntimeError('snapshot workspace is still mounted') from error
            self.mounted = False
        if self.loop:
            try:
                subprocess.run(['losetup', '-d', self.loop], check=True, timeout=60)
            except (OSError, subprocess.SubprocessError) as error:
                raise RuntimeError('snapshot workspace loop device remains attached') from error
            self.loop = None
