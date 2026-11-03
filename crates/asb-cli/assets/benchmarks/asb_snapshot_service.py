"""Experimental Btrfs snapshots at adapter-provided tool boundaries.

This service does not capture paths outside the supplied workspace and does
not itself replay tools, transport the snapshot image, or control child lifetime.
"""
import json
import os
from pathlib import Path
import re
import socketserver
import subprocess
import threading

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
        except (OSError, ValueError, RuntimeError):
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
