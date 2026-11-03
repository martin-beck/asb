"""Experimental snapshot workspace provisioner for a disposable guest or VM."""
from pathlib import Path
import fcntl
import json
import os
import stat
import subprocess
import time


class SnapshotWorkspace:
    """Own a sparse filesystem image, dynamic loop device and task subvolume."""
    def __init__(self, image, mountpoint, size, mkfs, btrfs, rsync, qemu_img=None, qemu_storage=None):
        self.image, self.mountpoint = Path(image), Path(mountpoint)
        self.size, self.mkfs, self.btrfs, self.rsync = size, list(mkfs), list(btrfs), list(rsync)
        if (size is not None and (type(size) is not int or size <= 0)) or not all((self.mkfs, self.btrfs, self.rsync)):
            raise ValueError('snapshot workspace configuration is invalid')
        self.loop = None
        self.mounted = False
        self.qemu_img = list(qemu_img) if qemu_img else None
        self.qemu_storage = list(qemu_storage) if qemu_storage else None
        self.qemu_process = None
        self.qemu_log = None

    def _start_qcow_export(self, size):
        if not self.qemu_img or not self.qemu_storage:
            raise RuntimeError('snapshot storage is not sparse; install project-local qemu-img and qemu-storage-daemon or use a VM')
        fuse = Path('/dev/fuse')
        if not fuse.exists():
            try:
                os.mknod(fuse, stat.S_IFCHR | 0o666, os.makedev(10,229))
            except FileExistsError:
                pass
            except OSError as error:
                raise RuntimeError('cannot create FUSE device in the workload container; use a FUSE-capable VM') from error
        if not fuse.is_char_device():
            raise RuntimeError('snapshot qcow2 export needs FUSE in the workload container or VM')
        qcow = self.image.with_suffix('.qcow2')
        descriptor = os.open(qcow, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        os.close(descriptor)
        subprocess.run(self.qemu_img + ['create', '-f', 'qcow2', str(qcow), str(size)],
                       check=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
        logfile = self.image.with_suffix('.qemu.log')
        descriptor = os.open(logfile, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        self.qemu_log = os.fdopen(descriptor, 'wb')
        file_node = {'driver':'file','node-name':'file','filename':str(qcow)}
        qcow_node = {'driver':'qcow2','node-name':'qcow2','file':'file'}
        export = {'type':'fuse','id':'export','node-name':'qcow2',
                  'mountpoint':str(self.image),'writable':True}
        self.qemu_process = subprocess.Popen(self.qemu_storage + [
            '--blockdev',json.dumps(file_node),'--blockdev',json.dumps(qcow_node),
            '--export',json.dumps(export)], stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,stderr=self.qemu_log)
        # The owning preparation deadline and container lifecycle cancel this
        # wait; no shorter backend-specific deadline changes the operation.
        while not os.path.ismount(self.image):
            if self.qemu_process.poll() is not None:
                raise RuntimeError(f'qcow2 snapshot export failed; inspect {logfile}')
            time.sleep(.05)

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
                # Do not expand an image to all available space on a backing
                # filesystem that eagerly allocates truncated zeroes. The
                # small probe is not a workspace size limit.
                probe = min(size, 8 * 1024**2)
                image.truncate(probe)
                image.flush()
                sparse = os.fstat(image.fileno()).st_blocks * 512 < probe
                if sparse:
                    image.truncate(size)
            if not sparse:
                self._start_qcow_export(size)
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
        if self.qemu_process is not None:
            process = self.qemu_process
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill();process.wait()
            if os.path.ismount(self.image):
                raise RuntimeError('qcow2 snapshot export remains mounted')
            self.qemu_process = None
        if self.qemu_log is not None:
            self.qemu_log.close();self.qemu_log = None
