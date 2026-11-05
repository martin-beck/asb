"""Experimental controller-owned snapshot custody outside the task PID namespace."""
import os
from pathlib import Path
import select
import subprocess
import tarfile
import time

from asb_execution import ExecutionError
from asb_resources import owned_resource
from asb_staging import upload


def _line(process, deadline):
    value = bytearray()
    fd = process.stdout.fileno()
    while len(value) < 128:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([fd], [], [], remaining)[0]:
            raise ExecutionError('snapshot custody startup timed out')
        byte = os.read(fd, 1)
        if not byte:
            raise ExecutionError('snapshot custody exited before readiness; inspect snapshot-supervisor.log')
        value.extend(byte)
        if byte == b'\n':
            return bytes(value)
    raise ExecutionError('snapshot custody readiness frame is too long')


class IndependentSnapshot:
    """Own a task preparer and separately journalled, namespace-bound custodian."""
    def __init__(self, task, tools, owner, docker_command, deadline, replay=False, extra_roots=()):
        self.task, self.command, self.replay = task, list(docker_command), replay
        self.preparer = self.process = self.sidecar = None
        self.initial = None
        self.log = (task.artifacts/'snapshot-supervisor.log').open('wb')
        self.name = task.attempt + '-snapshot-custody'
        names = ('asb_snapshot_guest.py', 'asb_snapshot_service.py', 'asb_snapshot_send.py',
                 'asb_snapshot_handoff.py', 'asb_snapshot_workspace.py', 'asb_snapshot_topology.py')
        files = [(name, (Path(__file__).parent/name).read_bytes(), 0o600) for name in names]
        try:
            upload(task.workspace.container, '/opt/asb-private', files, temporary_dir=task.artifacts, owner=owner)
            common = ['--target','/testbed','--root','/asb-snapshot-storage/snapshot',
                      '--socket','/opt/asb-private/snapshot.sock',
                      '--output','/asb-snapshot-storage/snapshot-streams',
                      '--btrfs',tools['btrfs'],'--mkfs',tools['mkfs.btrfs'],'--rsync',tools['rsync'],
                      '--apply-final']
            if 'qemu-img' in tools and 'qemu-storage-daemon' in tools:
                common += ['--qemu-img',tools['qemu-img'],'--qemu-storage',tools['qemu-storage-daemon']]
            if replay:
                common += ['--replay-archive','/asb-snapshot-storage/workspace-snapshots.tar']
            for root in extra_roots:
                common += ['--extra-root',root]
            container = task.workspace.container
            self.preparer = subprocess.Popen(self.command+['exec','--user','0','-i',container.id,
                'python3','/opt/asb-private/asb_snapshot_handoff.py','--role','prepare',*common],
                stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=self.log)
            if _line(self.preparer, deadline) != b'PREPARED\n':
                raise ExecutionError('snapshot preparer failed; inspect snapshot-supervisor.log')
            container.reload()
            state = container.attrs['State']
            pid, started = state.get('Pid'), state.get('StartedAt')
            if state.get('Running') is not True or type(pid) is not int or pid <= 1 or not started:
                raise ExecutionError('snapshot task identity is unavailable')
            self.initial = (pid, started)
            probe = container.exec_run(['stat','-Lc','%i','/proc/1/ns/mnt'], user='0')
            expected = probe.output.decode('ascii').strip() if probe.exit_code == 0 else ''
            if not expected.isdigit() or int(expected) <= 0:
                raise ExecutionError('snapshot task mount identity is unavailable')
            labels = task.journal.declare('containers', self.name)
            image = container.attrs.get('Image')
            if not isinstance(image, str) or not image.startswith('sha256:'):
                raise ExecutionError('snapshot custody image identity is unavailable')
            self.sidecar = task.client.containers.create(image, name=self.name, labels=labels,
                entrypoint='/usr/bin/nsenter', command=['-t',str(pid),'-m','-r','--','python3',
                    '/opt/asb-private/asb_snapshot_handoff.py','--role','custody','--verify-mount',*common],
                stdin_open=True, tty=False, privileged=True, pid_mode='host', network_mode='none')
            self.process = subprocess.Popen(self.command+['start','--attach','--interactive',self.sidecar.id],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log)
            if _line(self.process, deadline) != ('MOUNT:'+expected+'\n').encode('ascii'):
                raise ExecutionError('snapshot custodian joined the wrong mount namespace')
            container.reload()
            current = container.attrs['State']
            if current.get('Running') is not True or (current.get('Pid'),current.get('StartedAt')) != self.initial:
                raise ExecutionError('snapshot task changed before custody was admitted')
            self.process.stdin.write(b'START\n'); self.process.stdin.flush()
            if _line(self.process, deadline) != b'READY\n':
                raise ExecutionError('snapshot custodian failed; inspect snapshot-supervisor.log')
        except BaseException as failure:
            try:
                self.abort()
            except BaseException as cleanup:
                failure.add_note(f'snapshot custody cleanup also failed: {type(cleanup).__name__}')
            raise

    def _remove_sidecar(self):
        if self.sidecar is None:
            return
        resource = owned_resource(self.task.client, {'kind':'containers','name':self.name}, self.task.journal.labels)
        if resource is not None:
            resource.remove(force=False)
        self.sidecar = None

    def abort(self):
        failures = []
        for process in (self.process, self.preparer):
            if process is not None and process.poll() is None:
                try:
                    try:
                        process.stdin.close()
                    except BrokenPipeError:
                        # The child may have exited after poll; still reap it.
                        pass
                    try:
                        process.wait(timeout=60)
                    except subprocess.TimeoutExpired:
                        process.kill(); process.wait()
                except BaseException as error:
                    failures.append(error)
                    try:
                        if process.poll() is None:
                            process.kill()
                            process.wait()
                    except BaseException:
                        pass
        # A journalled sidecar is retained on failed startup; the task's
        # existing interrupted-resource path owns any remaining container.
        self.log.close()
        if failures:
            raise failures[0]

    def _settle(self, action):
        process, self.process = self.process, None
        if process is None:
            raise ExecutionError('snapshot custody is unavailable')
        try:
            output, _ = process.communicate(action, timeout=600)
            expected = b'FORENSIC_SAVED_INCOMPLETE\n' if action == b'FORENSIC\n' else b'SAVED\n'
            if process.returncode or output != expected:
                raise ExecutionError('snapshot custodian did not settle; inspect snapshot-supervisor.log')
        finally:
            if process.poll() is None:
                process.kill(); process.wait()

    def close(self):
        try:
            self._settle(b'STOP\n')
            output, _ = self.preparer.communicate(b'CLEANUP\n', timeout=600)
            if self.preparer.returncode or output != b'CLEANED\n':
                raise ExecutionError('snapshot preparer did not settle; inspect snapshot-supervisor.log')
            self._remove_sidecar()
            if not self.replay:
                destination=self.task.workspace.snapshot_storage/'snapshot-streams'
                with tarfile.open(self.task.artifacts/'snapshot-streams.tar','x') as archive:
                    for path in sorted(destination.iterdir()):
                        if not path.is_file() or path.is_symlink():
                            raise ExecutionError('snapshot export contains an unexpected entry')
                        archive.add(path,arcname=path.name,recursive=False)
        except BaseException as failure:
            # A failed custodian or preparer must not leave the other local
            # docker client alive. The journal still owns any container left
            # behind for the task's interrupted-resource cleanup.
            try:
                self.abort()
            except BaseException as cleanup:
                failure.add_note(f'snapshot custody cleanup also failed: {type(cleanup).__name__}')
            raise
        finally:
            self.log.close()

    def interrupt(self):
        try:
            container = self.task.workspace.container
            container.reload()
            state = container.attrs['State']
            current = (state.get('Pid'),state.get('StartedAt'))
            running = state.get('Running')
            if running is True and current == self.initial:
                # The original process tree is not confirmed stopped; EOF
                # invalidates custody without manufacturing forensic output.
                self.abort()
                return
            if running is not False and not (
                running is True and type(current[0]) is int and current[0] > 1
                and isinstance(current[1], str) and current[1] and current != self.initial
            ):
                self.abort()
                raise ExecutionError('snapshot task reset cannot be confirmed; preserve interrupted evidence')
            self._settle(b'FORENSIC\n')
            self._remove_sidecar()
        except BaseException as failure:
            # A failed Docker inspection or forensic export must not leave
            # the attached custodian client running after this scope exits.
            try:
                self.abort()
            except BaseException as cleanup:
                failure.add_note(f'snapshot custody cleanup also failed: {type(cleanup).__name__}')
            raise
        finally:
            # The task reset terminates the in-guest preparer, but the local
            # docker-exec client still needs to be reaped by its owner.
            if self.preparer is not None and self.preparer.poll() is None:
                try:
                    self.preparer.wait(timeout=60)
                except subprocess.TimeoutExpired:
                    self.preparer.kill()
                    self.preparer.wait()
            if not self.log.closed:
                self.log.close()
