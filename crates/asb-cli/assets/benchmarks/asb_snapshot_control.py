"""Experimental controller-owned lifetime for the guest snapshot supervisor."""
from pathlib import Path
import selectors
import subprocess
import tarfile
import time

from asb_execution import ExecutionError
from asb_staging import upload


def settle_on_exit(supervisor, exc_type, exc, traceback):
    """Preserve the execution failure when snapshot cleanup also fails."""
    try:
        supervisor.close()
    except BaseException as cleanup:
        if exc is None:
            raise
        raise ExecutionError(f'{exc}; snapshot cleanup also failed: {cleanup}') from exc
    return False


class GuestSnapshot:
    def __init__(self, task, tools, owner, docker_command, deadline, replay=False, extra_roots=()):
        self.task=task;self.docker_command=list(docker_command);self.process=None
        self.replay=replay
        self.log=(task.artifacts/'snapshot-supervisor.log').open('wb')
        files=[]
        for name in ('asb_snapshot_guest.py','asb_snapshot_service.py',
                     'asb_snapshot_workspace.py','asb_snapshot_topology.py'):
            files.append((name,(Path(__file__).parent/name).read_bytes(),0o600))
        upload(task.workspace.container,'/opt/asb-private',files,temporary_dir=task.artifacts,owner=owner)
        container=task.workspace.container.id
        args=self.docker_command+['exec','--user','0','-i',container,'python3',
            '/opt/asb-private/asb_snapshot_guest.py','--target','/testbed',
            '--root','/asb-snapshot-storage/snapshot','--socket','/opt/asb-private/snapshot.sock',
            '--output','/asb-snapshot-storage/snapshot-streams','--btrfs',tools['btrfs'],
            '--mkfs',tools['mkfs.btrfs'],'--rsync',tools['rsync'],'--apply-final']
        if 'qemu-img' in tools and 'qemu-storage-daemon' in tools:
            args += ['--qemu-img',tools['qemu-img'],'--qemu-storage',tools['qemu-storage-daemon']]
        if replay:
            args += ['--replay-archive','/asb-snapshot-storage/workspace-snapshots.tar']
        for root in extra_roots:
            args += ['--extra-root',root]
        self.process=subprocess.Popen(args,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=self.log)
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(self.process.stdout,selectors.EVENT_READ)
                if not selector.select(max(0,deadline-time.monotonic())):
                    raise ExecutionError('snapshot workspace preparation timed out')
                if self.process.stdout.readline()!=b'READY\n':
                    raise ExecutionError('snapshot workspace preparation failed; inspect snapshot-supervisor.log')
        except BaseException:
            self.abort()
            raise

    def abort(self):
        if self.process is not None and self.process.poll() is None:
            self.process.stdin.close()
            try:self.process.wait(timeout=60)
            except subprocess.TimeoutExpired:
                self.process.kill();self.process.wait()
        self.log.close()

    def close(self):
        if self.process is None:return
        process=self.process;self.process=None
        try:
            stdout,_=process.communicate(b'STOP\n',timeout=600)
            if process.returncode or b'SAVED\n' not in stdout:
                logs = [name for name in ('guest-error-stdout.log','guest-error-stderr.log')
                        if (self.task.artifacts/name).is_file()]
                if logs:
                    raise ExecutionError('snapshot workspace did not settle after guest failure; inspect '+', '.join(logs)+' and snapshot-supervisor.log')
                raise ExecutionError('snapshot workspace did not settle; inspect snapshot-supervisor.log')
            if self.replay:
                return
            destination=self.task.workspace.snapshot_storage/'snapshot-streams'
            archive=self.task.artifacts/'snapshot-streams.tar'
            with tarfile.open(archive,'x') as output:
                for path in sorted(destination.iterdir()):
                    if not path.is_file() or path.is_symlink():
                        raise ExecutionError('snapshot export contains an unexpected entry')
                    output.add(path,arcname=path.name,recursive=False)
        except (OSError,subprocess.SubprocessError) as error:
            raise ExecutionError('snapshot streams could not be exported from the guest') from error
        finally:
            if process.poll() is None:
                process.kill();process.wait()
            self.log.close()
