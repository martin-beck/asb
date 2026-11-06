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


@dataclass(frozen=True)
class SnapshotRequest:
    task: object
    tools: dict
    owner: object
    transport: object
    deadline: float
    target: str = '/testbed'
    replay: bool = False
    extra_roots: tuple = ()
    independent_custody: bool = False


class SnapshotBackend(Protocol):
    def start(self, request: SnapshotRequest) -> SnapshotSession: ...


class BtrfsSnapshotBackend:
    """Btrfs workspace snapshots in an already provisioned guest."""
    def start(self, request):
        if request.independent_custody:
            from asb_snapshot_independent import IndependentSnapshot
            supervisor = IndependentSnapshot
        else:
            from asb_snapshot_control import GuestSnapshot
            supervisor = GuestSnapshot
        return supervisor(request.task, request.tools, request.owner,
                          request.transport, request.deadline, target=request.target,
                          replay=request.replay, extra_roots=list(request.extra_roots))


BTRFS = BtrfsSnapshotBackend()
