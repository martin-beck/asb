"""Experimental per-replica filesystem journal coordinator.

The caller must validate each adapter request before sending its tool-end
boundary. This module is not yet wired into the ASB guest lifecycle.
"""
import hashlib
import re
import os
import socket
import sys
import threading
from pathlib import Path

from asb_fs_replay_cursor import Coordinator, Replay
from asb_fs_initial_reads import restore_initial_reads


class ReplayServer:
    def __init__(self, evidence, workspace, socket_path, timeout=1800, direct_trace=None):
        self.cursor = Replay(evidence, workspace, enforce_admission=True, existing=True)
        try:
            self.coordinator = Coordinator(self.cursor)
            self.direct_trace = Path(direct_trace) if direct_trace is not None else None
            self.direct_prefix_fingerprint = None
            restore_initial_reads(evidence, workspace, evidence)
            self.path = Path(socket_path)
            self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                self.listener.bind(str(self.path))
                os.chmod(self.path, 0o666)
                self.listener.listen(32)
                self.listener.settimeout(.2)
            except BaseException:
                self.listener.close()
                raise
        except BaseException:
            self.cursor.close()
            raise
        self.timeout = timeout
        self.stopping = threading.Event()
        self.workers = []
        self.connections = set()
        self.lock = threading.Lock()
        self.errors = []
        self.thread = threading.Thread(target=self._accept, daemon=True)
        self.thread.start()

    def _trace_fingerprint(self):
        digest = hashlib.sha256()
        for name in ('events.jsonl', 'payload.bin'):
            path = self.direct_trace / name
            before = path.stat()
            digest.update(name.encode('ascii') + b'\0')
            digest.update(before.st_size.to_bytes(8, 'big'))
            with path.open('rb') as stream:
                while chunk := stream.read(65536):
                    digest.update(chunk)
            after = path.stat()
            if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) != (
                    after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns):
                raise RuntimeError('direct filesystem trace changed during validation')
        return digest.digest()

    def _settle_direct_prefix(self):
        if self.direct_trace is None or self.direct_prefix_fingerprint is not None:
            return
        from asb_fs_direct_prefix import validate_direct_prefix, validate_no_direct_tail
        before = self._trace_fingerprint()
        if self.cursor.events.tell() != 0:
            raise RuntimeError('direct filesystem replay cursor already advanced')
        with (self.direct_trace / 'events.jsonl').open(encoding='utf-8') as observed:
            count = validate_direct_prefix(self.cursor.events, self.cursor.evidence / 'payload.bin',
                                           observed, self.direct_trace / 'payload.bin')
        self.cursor.events.seek(0)
        validate_no_direct_tail(self.cursor.events, count)
        self.cursor.events.seek(0)
        after = self._trace_fingerprint()
        if before != after:
            raise RuntimeError('direct filesystem trace changed during validation')
        self.cursor.skip_validated_direct_prefix(count)
        self.direct_prefix_fingerprint = after

    def _accept(self):
        while not self.stopping.is_set():
            try:
                connection, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                if not self.stopping.is_set():
                    with self.lock:
                        self.errors.append('filesystem replay listener failed')
                return
            worker = threading.Thread(target=self._handle, args=(connection,), daemon=True)
            with self.lock:
                self.workers.append(worker)
                self.connections.add(connection)
            worker.start()

    def _handle(self, connection):
        try:
            connection.settimeout(self.timeout)
            with connection, connection.makefile('rwb', buffering=0) as stream:
                line = stream.readline(256)
                match = re.fullmatch(rb'tool-(begin|end):([A-Za-z0-9_-]{1,80})\n', line)
                if match is None:
                    raise RuntimeError('invalid replay boundary')
                action = match.group(1).decode('ascii')
                alias = match.group(2).decode('ascii')
                with self.coordinator.condition:
                    self._settle_direct_prefix()
                    ends = sorted(sequence for sequence, marker in self.coordinator.ends.items()
                                  if marker == 'tool-end:' + alias
                                  and sequence not in self.coordinator.claimed)
                    if not ends:
                        raise RuntimeError('no matching recorded tool boundary')
                    end = ends[0]
                    if action == 'begin':
                        starts = sorted(sequence for sequence, marker in self.coordinator.ends.items()
                                        if marker == 'tool-begin:' + alias
                                        and sequence > self.cursor.sequence
                                        and sequence not in self.coordinator.begun_starts
                                        and sequence < end)
                        if not starts:
                            raise RuntimeError('no matching recorded parent start')
                        boundary = starts[0]
                        self.coordinator.begin(alias, boundary, end, self.timeout, lambda: None)
                    else:
                        boundary = end
                        self.coordinator.invoke(alias, end, self.timeout, lambda: None,
                                                pre_admitted=end in self.coordinator.pre_admitted)
                stream.write(f'{boundary}\n'.encode('ascii'))
        except Exception as error:
            print(f'filesystem replay boundary failed: {type(error).__name__}: {error}',
                  file=sys.stderr, flush=True)
            with self.lock:
                self.errors.append('filesystem replay boundary failed')
            with self.coordinator.condition:
                if self.coordinator.failure is None:
                    self.coordinator.failure = error
                self.coordinator.condition.notify_all()
        finally:
            with self.lock:
                self.connections.discard(connection)

    def finish(self):
        self.stopping.set()
        self.listener.close()
        self.thread.join()
        with self.lock:
            for connection in self.connections:
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
        with self.coordinator.condition:
            if self.coordinator.failure is None and any(worker.is_alive() for worker in self.workers):
                self.coordinator.failure = RuntimeError('filesystem replay ended with pending calls')
                self.coordinator.condition.notify_all()
        for worker in self.workers:
            worker.join()
        try:
            if self.errors or self.coordinator.failure is not None or self.coordinator.pre_admitted:
                raise RuntimeError('filesystem replay did not complete')
            if self.direct_trace is not None:
                if self.direct_prefix_fingerprint is None or self._trace_fingerprint() != self.direct_prefix_fingerprint:
                    raise RuntimeError('direct filesystem trace changed after the validated prefix')
            self.cursor.finish()
        finally:
            self.cursor.close()
            self.path.unlink(missing_ok=True)


if __name__ == '__main__':
    if len(sys.argv) != 4:
        raise SystemExit('usage: asb_fs_replay_server.py EVIDENCE WORKSPACE SOCKET')
    server = ReplayServer(*sys.argv[1:])
    try:
        print('READY', flush=True)
        if sys.stdin.readline() != 'STOP\n':
            raise RuntimeError('filesystem replay control ended unexpectedly')
    finally:
        server.finish()
