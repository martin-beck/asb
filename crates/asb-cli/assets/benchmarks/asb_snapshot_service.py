"""Experimental Btrfs snapshots at adapter-provided tool boundaries.

This service does not capture paths outside the supplied workspace and does
not itself replay tools, transport the snapshot image, or control child lifetime.
"""
import json
import os
from pathlib import Path
import re
import shutil
import socketserver
import subprocess
import sys
import threading
import time

_MARKER = re.compile(rb'tool-(begin|end):([A-Za-z0-9_-]{1,80})(?::([0-9]+):([0-9]+))?\n')


class SnapshotService:
    def __init__(self, workspace, store, command, snapshot=None):
        self.workspace = Path(workspace)
        self.store = Path(store)
        self.command = list(command)
        if not self.command:
            raise ValueError('snapshot command is unavailable')
        self.lock = threading.RLock()
        self.active = set()
        self.sequence = 0
        self.event_sequence = 0
        self.closed = False
        self.failed = False
        self.store.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.journal = (self.store / 'boundaries.jsonl').open('x', encoding='utf-8', buffering=1)
        self.events = (self.store / 'markers.jsonl').open('x', encoding='utf-8', buffering=1)
        self.snapshot = snapshot or self._snapshot
        try:
            self._capture('baseline', None)
        except BaseException:
            self.journal.close()
            self.events.close()
            raise

    def _snapshot(self, destination):
        try:
            subprocess.run(self.command + ['subvolume', 'snapshot', '-r', str(self.workspace), str(destination)],
                           check=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=30)
        except (OSError, subprocess.SubprocessError) as error:
            raise RuntimeError('workspace snapshot failed; retained snapshot directory') from error

    def _capture(self, kind, alias):
        sequence = self.sequence + 1
        target = self.store / f'snapshot-{sequence:08d}'
        self.snapshot(target)
        entry = {'sequence': sequence, 'kind': kind, 'alias': alias, 'snapshot': target.name}
        self.journal.write(json.dumps(entry, separators=(',', ':')) + '\n')
        self.journal.flush()
        os.fsync(self.journal.fileno())
        self.sequence = sequence
        return sequence

    def _event(self, action, alias, process, thread, snapshot):
        entry = {'sequence': self.event_sequence + 1, 'kind': action, 'alias': alias,
                 'process': process, 'thread': thread, 'snapshot_sequence': snapshot}
        self.events.write(json.dumps(entry, separators=(',', ':')) + '\n')
        self.events.flush()
        os.fsync(self.events.fileno())
        self.event_sequence += 1

    def marker(self, action, alias, process=None, thread=None):
        if not isinstance(alias, str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,80}', alias):
            raise ValueError('snapshot marker alias is invalid')
        with self.lock:
            if self.closed or self.failed:
                raise RuntimeError('snapshot service is not active')
            if action == 'begin':
                if alias in self.active:
                    raise ValueError('snapshot marker is already active')
                try:
                    self._event(action, alias, process, thread, self.sequence)
                except BaseException:
                    self.failed = True
                    raise
                self.active.add(alias)
                return self.sequence
            if action != 'end' or alias not in self.active:
                raise ValueError('snapshot marker has no active invocation')
            try:
                sequence = self._capture('tool-end', alias)
                self._event(action, alias, process, thread, sequence)
            except BaseException:
                self.failed = True
                raise
            self.active.remove(alias)
            return sequence

    def invalidate(self):
        with self.lock:
            self.failed = True

    def finish(self):
        with self.lock:
            if self.closed:
                raise RuntimeError('snapshot service is already closed')
            self.closed = True
            try:
                if self.failed or self.active:
                    raise RuntimeError('snapshot service has unfinished tool invocations')
                return self._capture('final', None)
            finally:
                self.journal.close()
                self.events.close()


def load_replay_boundaries(store):
    """Read a completed snapshot journal and return ordered tool boundaries."""
    store = Path(store)
    try:
        boundaries = [json.loads(line) for line in (store/'boundaries.jsonl').read_text().splitlines()]
        events = [json.loads(line) for line in (store/'markers.jsonl').read_text().splitlines()]
    except (OSError, ValueError) as error:
        raise ValueError('snapshot recording journals are unreadable') from error
    if len(boundaries) < 2:
        raise ValueError('snapshot recording is incomplete')
    for index, item in enumerate(boundaries, 1):
        if (not isinstance(item, dict) or item.get('sequence') != index or
                item.get('snapshot') != f'snapshot-{index:08d}' or
                not (store/item['snapshot']).is_dir()):
            raise ValueError('snapshot recording boundaries are malformed')
        expected = 'baseline' if index == 1 else 'final' if index == len(boundaries) else 'tool-end'
        if item.get('kind') != expected or (expected != 'tool-end' and item.get('alias') is not None):
            raise ValueError('snapshot recording boundaries are incomplete')
    active = set()
    ends = []
    for index, event in enumerate(events, 1):
        if not isinstance(event, dict) or event.get('sequence') != index:
            raise ValueError('snapshot marker journal is malformed')
        action, alias = event.get('kind'), event.get('alias')
        if not isinstance(alias, str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,80}', alias):
            raise ValueError('snapshot marker journal is malformed')
        if action == 'begin' and alias not in active:
            if event.get('snapshot_sequence') != len(ends) + 1:
                raise ValueError('snapshot marker journal has an invalid begin state')
            active.add(alias)
        elif action == 'end' and alias in active:
            active.remove(alias)
            ends.append((alias, event.get('snapshot_sequence')))
        else:
            raise ValueError('snapshot marker journal has unmatched boundaries')
    expected = [(item.get('alias'), item['sequence']) for item in boundaries[1:-1]]
    if active or ends != expected:
        raise ValueError('snapshot marker and boundary journals differ')
    return expected


def export_snapshot_streams(store, destination, command):
    """Export one baseline and incremental Btrfs streams to files."""
    store, destination = Path(store), Path(destination)
    load_replay_boundaries(store)
    entries = [json.loads(line) for line in (store/'boundaries.jsonl').read_text().splitlines()]
    destination.mkdir(parents=True, exist_ok=False)
    previous = None
    try:
        for entry in entries:
            snapshot = store / entry['snapshot']
            stream = destination / (entry['snapshot'] + '.btrfs')
            args = list(command) + ['send']
            if previous is not None:
                args += ['-p', str(previous)]
            with stream.open('xb') as output:
                subprocess.run(args + [str(snapshot)], check=True, stdin=subprocess.DEVNULL,
                               stdout=output, stderr=subprocess.DEVNULL, timeout=300)
            previous = snapshot
        for name in ('boundaries.jsonl', 'markers.jsonl'):
            shutil.copyfile(store/name, destination/name)
    except (OSError, subprocess.SubprocessError) as error:
        raise RuntimeError('snapshot stream export failed; retained partial output') from error


def receive_snapshot_streams(source, destination, command):
    """Receive a complete ordered stream set onto a Btrfs filesystem."""
    source, destination = Path(source), Path(destination)
    try:
        entries = [json.loads(line) for line in (source/'boundaries.jsonl').read_text().splitlines()]
        if len(entries) < 2:
            raise ValueError('snapshot stream set is incomplete')
        destination.mkdir(parents=True, exist_ok=False)
        for index, entry in enumerate(entries, 1):
            name = f'snapshot-{index:08d}'
            if not isinstance(entry, dict) or entry.get('sequence') != index or entry.get('snapshot') != name:
                raise ValueError('snapshot stream set is malformed')
            with (source/(name+'.btrfs')).open('rb') as stream:
                subprocess.run(list(command) + ['receive', str(destination)], check=True, stdin=stream,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=300)
        for name in ('boundaries.jsonl', 'markers.jsonl'):
            shutil.copyfile(source/name, destination/name)
        return load_replay_boundaries(destination)
    except (OSError, subprocess.SubprocessError) as error:
        raise RuntimeError('snapshot stream receive failed; retained partial output') from error


class SnapshotApplier:
    """Apply successive received snapshots to one already-restored workspace."""
    def __init__(self, store, workspace, rsync_command):
        self.store = Path(store)
        self.workspace = Path(workspace)
        self.command = list(rsync_command)
        self.previous = 1
        if not self.command or not self.workspace.is_dir():
            raise ValueError('snapshot replay workspace or sync command is unavailable')

    def __call__(self, sequence):
        from asb_snapshot_topology import (regular_files, directories,
                                           plan_tree_transitions, apply_tree_transitions)
        if sequence != self.previous + 1:
            raise ValueError('snapshot replay sequence is out of order')
        before = self.store / f'snapshot-{self.previous:08d}'
        after = self.store / f'snapshot-{sequence:08d}'
        if not before.is_dir() or not after.is_dir():
            raise ValueError('snapshot replay image is unavailable')
        plan = plan_tree_transitions(regular_files(before), regular_files(after), regular_files(self.workspace),
                                     directories(before), directories(after), directories(self.workspace))
        apply_tree_transitions(self.workspace, plan)
        try:
            subprocess.run(self.command + ['-aHAX', '--inplace', '--checksum', '--delete',
                                           str(after)+'/', str(self.workspace)+'/'],
                           check=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL, timeout=60)
        except (OSError, subprocess.SubprocessError) as error:
            raise RuntimeError('snapshot replay sync failed; workspace may be partially updated') from error
        self.previous = sequence


class SnapshotReplay:
    """Apply recorded tool-end boundaries to one isolated replay workspace.

    The caller owns snapshot verification, initial workspace restoration and
    the actual application mechanism. This protocol is an offline prototype.
    """
    def __init__(self, boundaries, apply, wait_seconds=1800):
        self.boundaries = list(boundaries)
        if any(not isinstance(item, tuple) or len(item) != 2 or
               not isinstance(item[0], str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,80}', item[0]) or
               not isinstance(item[1], int) or isinstance(item[1], bool) or item[1] < 1
               for item in self.boundaries):
            raise ValueError('snapshot replay boundaries are malformed')
        self.apply = apply
        if not isinstance(wait_seconds, (int, float)) or wait_seconds <= 0:
            raise ValueError('snapshot replay wait is invalid')
        self.wait_seconds = wait_seconds
        self.position = 0
        self.failed = False
        self.closed = False
        self.lock = threading.RLock()
        self.changed = threading.Condition(self.lock)

    def marker(self, action, alias, process=None, thread=None):
        with self.changed:
            deadline = time.monotonic() + self.wait_seconds
            while True:
                if self.closed or self.failed:
                    raise RuntimeError('snapshot replay is not active')
                if action != 'end' or alias not in [item[0] for item in self.boundaries[self.position:]]:
                    self.failed = True
                    self.changed.notify_all()
                    raise ValueError('snapshot replay boundary differs from the recording')
                if self.boundaries[self.position][0] == alias:
                    break
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    self.failed = True
                    self.changed.notify_all()
                    raise RuntimeError('snapshot replay timed out waiting for an earlier boundary')
                self.changed.wait(remaining)
            sequence = self.boundaries[self.position][1]
            try:
                self.apply(sequence)
            except BaseException:
                self.failed = True
                self.changed.notify_all()
                raise
            self.position += 1
            self.changed.notify_all()
            return sequence

    def invalidate(self):
        with self.changed:
            self.failed = True
            self.changed.notify_all()

    def finish(self):
        with self.changed:
            if self.closed:
                raise RuntimeError('snapshot replay is already closed')
            self.closed = True
            self.changed.notify_all()
            if self.failed or self.position != len(self.boundaries):
                raise RuntimeError('snapshot replay has unconsumed or failed boundaries')


class _Handler(socketserver.StreamRequestHandler):
    def handle(self):
        self.request.settimeout(5)
        try:
            line = self.rfile.readline(256)
            match = _MARKER.fullmatch(line)
            if match is None:
                self.server.snapshots.invalidate()
                return
            process, thread = (int(value) if value is not None else None for value in match.group(3, 4))
            sequence = self.server.snapshots.marker(match.group(1).decode(), match.group(2).decode(), process, thread)
            self.wfile.write(f'{sequence}\n'.encode('ascii'))
        except (OSError, ValueError, RuntimeError) as error:
            print(f"snapshot boundary failed: {type(error).__name__}: {error}", file=sys.stderr, flush=True)
            self.server.snapshots.invalidate()


class MarkerServer(socketserver.ThreadingUnixStreamServer):
    daemon_threads = False
    block_on_close = True
    def __init__(self, path, snapshots):
        self.snapshots = snapshots
        super().__init__(str(path), _Handler)
        os.chmod(path, 0o666)


def main(argv=None):
    """Run the marker service until its owning guest sends STOP."""
    import argparse
    import sys

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workspace', required=True)
    parser.add_argument('--store', required=True)
    parser.add_argument('--socket', required=True)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    snapshots = SnapshotService(args.workspace, args.store, command)
    try:
        server = MarkerServer(args.socket, snapshots)
    except BaseException:
        snapshots.journal.close()
        snapshots.events.close()
        raise
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        print('READY', flush=True)
        if sys.stdin.readline() != 'STOP\n':
            snapshots.invalidate()
            raise RuntimeError('snapshot service control ended unexpectedly')
    finally:
        server.shutdown()
        server.server_close()
        worker.join()
        Path(args.socket).unlink(missing_ok=True)
        snapshots.finish()


if __name__ == '__main__':
    main()
