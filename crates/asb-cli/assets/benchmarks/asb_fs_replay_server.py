"""Experimental per-replica filesystem journal coordinator.

The caller must validate each adapter request before sending its tool-end
boundary. This module is not yet wired into the ASB guest lifecycle.
"""
import re
import os
import socket
import sys
import threading
from pathlib import Path

from asb_fs_replay_cursor import Coordinator, Replay


class ReplayServer:
    def __init__(self, evidence, workspace, socket_path, timeout=1800):
        self.cursor = Replay(evidence, workspace, enforce_admission=True, existing=True)
        try:
            self.coordinator = Coordinator(self.cursor)
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
                match = re.fullmatch(rb'tool-end:([A-Za-z0-9_-]{1,80})\n', line)
                if match is None:
                    raise RuntimeError('invalid replay boundary')
                alias = match.group(1).decode('ascii')
                with self.coordinator.condition:
                    ends = sorted(sequence for sequence, marker in self.coordinator.ends.items()
                                  if marker == 'tool-end:' + alias
                                  and sequence not in self.coordinator.claimed)
                    if not ends:
                        raise RuntimeError('no matching recorded tool boundary')
                    end = ends[0]
                    self.coordinator.invoke(alias, end, self.timeout, lambda: None)
                stream.write(f'{end}\n'.encode('ascii'))
        except Exception as error:
            print(f'filesystem replay boundary failed: {type(error).__name__}: {error}',
                  file=sys.stderr, flush=True)
            with self.lock:
                self.errors.append('filesystem replay boundary failed')
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
            if self.errors or self.coordinator.failure is not None:
                raise RuntimeError('filesystem replay did not complete')
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
