"""Replay a small recorded operation fixture; not an ASB replay backend."""
import json
import os
from pathlib import Path
import subprocess
import sys


class Replay:
    """Apply a prefix of recorded operations and keep handles for later calls."""
    def __init__(self, evidence, root, baseline=None):
        self.evidence = Path(evidence)
        self.root = Path(root)
        self.root.mkdir(parents=True, exist_ok=False)
        if baseline is not None:
            subprocess.run(['/bin/cp', '-a', str(Path(baseline)) + '/.', str(self.root)], check=True)
        self.events = (self.evidence / 'events.jsonl').open(encoding='utf-8')
        self.handles = {}
        self.sequence = 0
        self.pending = None

    def apply_until(self, limit):
        if limit < self.sequence:
            raise RuntimeError('cannot reverse replay progress')
        while True:
            if self.pending is None:
                line = self.events.readline()
                if not line:
                    return
                self.pending = json.loads(line)
            event = self.pending
            sequence = event['sequence']
            if sequence != self.sequence + 1:
                raise RuntimeError('recorded operation sequence has a gap')
            if sequence > limit:
                return
            self.apply(event)
            self.sequence = sequence
            self.pending = None

    def apply(self, event):
        op = event['op']
        path = self.root / event['path'].lstrip('/') if event['path'] is not None else None
        if op == 'marker':
            return
        if op in ('open', 'create'):
            flags = event['flags'] | (os.O_CREAT if op == 'create' else 0)
            fd = os.open(path, flags, event.get('mode', 0o600) & 0o7777)
            assert event['handle'] not in self.handles
            self.handles[event['handle']] = fd
        elif op == 'release':
            os.close(self.handles.pop(event['handle']))
        elif op == 'write':
            fd = self.handles[event['handle']]
            offset = event['offset']
            with (self.evidence / event['object']).open('rb') as source:
                while chunk := source.read(64 * 1024):
                    count = os.pwrite(fd, chunk, offset)
                    if count != len(chunk):
                        raise RuntimeError('short recorded write')
                    offset += count
        elif op == 'read':
            fd = self.handles[event['handle']]
            offset = event['offset']
            with (self.evidence / event['object']).open('rb') as expected:
                while chunk := expected.read(64 * 1024):
                    if os.pread(fd, len(chunk), offset) != chunk:
                        raise RuntimeError('recorded read differs')
                    offset += len(chunk)
        elif op == 'truncate':
            if event['handle'] is None:
                os.truncate(path, event['size'])
            else:
                os.ftruncate(self.handles[event['handle']], event['size'])
        elif op == 'rename':
            os.rename(path, self.root / event['destination'].lstrip('/'))
        elif op == 'link':
            os.link(path, self.root / event['destination'].lstrip('/'))
        elif op == 'symlink':
            os.symlink(event['destination'], path)
        elif op == 'unlink':
            os.unlink(path)
        elif op == 'mkdir':
            os.mkdir(path, event['mode'] & 0o7777)
        elif op == 'rmdir':
            os.rmdir(path)
        else:
            raise RuntimeError('unsupported recorded operation')

    def finish(self):
        self.apply_until(sys.maxsize)
        if self.handles:
            raise RuntimeError('recorded handles were not closed')

    def close(self):
        for fd in self.handles.values():
            os.close(fd)
        self.handles.clear()
        self.events.close()


if __name__ == '__main__':
    replay = Replay(sys.argv[1], sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None)
    try:
        replay.finish()
    finally:
        replay.close()
