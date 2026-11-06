"""Workspace snapshot backend selection, independent of execution backend.

The caller supplies a guest transport. A snapshot backend owns only workspace
capture and settlement; it does not choose or start the task's execution VM or
container. New backends can implement the same start contract.
"""
from dataclasses import dataclass
from typing import Protocol


class SnapshotSession(Protocol):
    def close(self): ...
    def abort(self): ...


class SnapshotTransport(Protocol):
    """Execution adapter supplies snapshot lifecycle without selecting a VM."""
    def start_btrfs(self, request: 'SnapshotRequest') -> SnapshotSession: ...


@dataclass(frozen=True)
class SnapshotRequest:
    task: object
    tools: dict
    owner: object
    transport: SnapshotTransport
    deadline: float
    target: str = '/testbed'
    replay: bool = False
    extra_roots: tuple = ()
    independent_custody: bool = False


class SnapshotBackend(Protocol):
    def start(self, request: SnapshotRequest) -> SnapshotSession: ...


class BtrfsSnapshotBackend:
    """Delegate Btrfs operations to the already selected guest transport."""
    def start(self, request):
        return request.transport.start_btrfs(request)


class DockerSnapshotTransport:
    """Btrfs custody inside an existing Docker-owned task environment."""
    def __init__(self, docker_command):
        self.command = list(docker_command)

    def start_btrfs(self, request):
        if request.independent_custody:
            from asb_snapshot_independent import IndependentSnapshot
            supervisor = IndependentSnapshot
        else:
            from asb_snapshot_control import GuestSnapshot
            supervisor = GuestSnapshot
        return supervisor(request.task, request.tools, request.owner,
                          self.command, request.deadline, target=request.target,
                          replay=request.replay, extra_roots=list(request.extra_roots))


BTRFS = BtrfsSnapshotBackend()
