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
        if (size is not None and (type(size) is not int or size <= 0)) or not all((self.mkfs, self.btrfs, self.rsync)):
            raise ValueError('snapshot workspace configuration is invalid')
        self.loop = None
        self.mounted = False

    def prepare(self, source):
        source = Path(source)
        if not source.is_dir():
            raise ValueError('snapshot workspace source is unavailable')
        try:
            size = self.size
            if size is None:
                try:
                    space = os.statvfs(self.image.parent)
                except OSError as error:
                    raise RuntimeError('cannot determine snapshot storage capacity') from error
                size = space.f_bavail * (space.f_frsize or space.f_bsize)
                if size <= 0:
                    raise RuntimeError('snapshot storage has no available space')
            with self.image.open('xb') as image:
                image.truncate(size)
            subprocess.run(self.mkfs + ['-q', str(self.image)], check=True, stdout=subprocess.DEVNULL, timeout=60)
            # The container may expose loop-control without pre-created loop
            # nodes. Allocate a free number dynamically and create its node
            # inside the guest, rather than requiring a particular host device.
            # GET_FREE does not reserve the device. Another replica can take it
            # before losetup, so retry only that collision and never detach a
            # device unless this owner successfully attached it.
            for attempt in range(32):
                with open('/dev/loop-control', 'rb', buffering=0) as control:
                    number = fcntl.ioctl(control.fileno(), 0x4C82)
                node = Path('/dev') / f'loop{number}'
                if not node.exists():
                    try:
                        os.mknod(node, stat.S_IFBLK | 0o600, os.makedev(7, number))
                    except FileExistsError:
                        pass
                try:
                    subprocess.run(['losetup', str(node), str(self.image)], check=True,
                                   stderr=subprocess.PIPE, timeout=60,
                                   env=dict(os.environ, LC_ALL='C'))
                except subprocess.CalledProcessError as error:
                    if b'busy' in (error.stderr or b'').lower() and attempt < 31:
                        continue
                    raise
                self.loop = str(node)
                break
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
