"""Replay a small recorded operation fixture; not an ASB replay backend."""
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time


class Replay:
    """Apply a prefix of recorded operations and keep handles for later calls."""
    def __init__(self, evidence, root, baseline=None, enforce_admission=False, existing=False):
        self.evidence = Path(evidence)
        self.root = Path(root)
        if existing:
            if baseline is not None or not self.root.is_dir():
                raise ValueError('existing replay root must be an existing directory without a baseline')
        else:
            self.root.mkdir(parents=True, exist_ok=False)
        if baseline is not None:
            subprocess.run(['/bin/cp', '-a', '--reflink=auto', str(Path(baseline)) + '/.', str(self.root)], check=True)
        self.events = (self.evidence / 'events.jsonl').open(encoding='utf-8')
        self.handles = {}
        self.sequence = 0
        self.pending = None
        self.enforce_admission = enforce_admission
        self.admitted = {}

    def admit(self, tool):
        self.admitted[tool] = self.admitted.get(tool, 0) + 1

    def apply_until(self, limit):
        if limit < self.sequence:
            raise RuntimeError('cannot reverse replay progress')
        while True:
            if self.pending is None:
                line = self.events.readline()
                if not line:
                    if limit != sys.maxsize and self.sequence < limit:
                        raise RuntimeError('recorded operation stream ended before requested boundary')
                    return True
                if not line.endswith('\n'):
                    raise RuntimeError('recorded operation stream has a truncated final event')
                self.pending = json.loads(line)
            event = self.pending
            sequence = event['sequence']
            if sequence != self.sequence + 1:
                raise RuntimeError('recorded operation sequence has a gap')
            if sequence > limit:
                return True
            if self.enforce_admission and event['op'] == 'marker':
                marker = event['marker']
                if marker.startswith('tool-begin:'):
                    tool = marker.removeprefix('tool-begin:')
                    if self.admitted.get(tool, 0) == 0:
                        return False
                    self.admitted[tool] -= 1
            try:
                self.apply(event)
            except Exception as error:
                raise RuntimeError(f'recorded operation {sequence} ({event["op"]}, {event.get("path")}) failed: {error}') from error
            self.sequence = sequence
            self.pending = None

    def payload_chunks(self, event):
        remaining = event.get('bytes')
        with (self.evidence / event['object']).open('rb') as source:
            source.seek(event.get('payload_offset', 0))
            while remaining is None or remaining > 0:
                chunk = source.read(65536 if remaining is None else min(65536, remaining))
                if not chunk:
                    if remaining:
                        raise RuntimeError('recorded payload is truncated')
                    break
                if remaining is not None:
                    remaining -= len(chunk)
                yield chunk

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
            for chunk in self.payload_chunks(event):
                count = os.pwrite(fd, chunk, offset)
                if count != len(chunk):
                    raise RuntimeError('short recorded write')
                offset += count
        elif op == 'read':
            fd = self.handles[event['handle']]
            offset = event['offset']
            for chunk in self.payload_chunks(event):
                if os.pread(fd, len(chunk), offset) != chunk:
                    raise RuntimeError('recorded read differs')
                offset += len(chunk)
        elif op == 'truncate':
            if event['handle'] is None:
                os.truncate(path, event['size'])
            else:
                os.ftruncate(self.handles[event['handle']], event['size'])
        elif op == 'chmod':
            os.chmod(path, event['mode'])
        elif op == 'utimens':
            os.utime(path, ns=(event['atime_ns'], event['mtime_ns']))
        elif op == 'setxattr':
            value = b''.join(self.payload_chunks(event))
            os.setxattr(path, event['name'], value, event['options'], follow_symlinks=False)
        elif op == 'removexattr':
            os.removexattr(path, event['name'], follow_symlinks=False)
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
        if not self.apply_until(sys.maxsize):
            raise RuntimeError('replay is waiting for a recorded tool invocation')
        if self.handles:
            raise RuntimeError('recorded handles were not closed')

    def close(self):
        for fd in self.handles.values():
            os.close(fd)
        self.handles.clear()
        self.events.close()


class Coordinator:
    """Experimental condition-based coordinator for explicitly marked calls."""
    def __init__(self, replay):
        if not replay.enforce_admission:
            raise ValueError('coordinator requires invocation admission')
        self.replay = replay
        self.condition = threading.Condition()
        self.claimed = set()
        self.failure = None
        with (replay.evidence / 'events.jsonl').open(encoding='utf-8') as stream:
            self.ends = {event['sequence']: event.get('marker')
                         for event in map(json.loads, stream) if event['op'] == 'marker'}

    def invoke(self, tool, end_sequence, timeout, validate):
        if self.ends.get(end_sequence) != f'tool-end:{tool}':
            raise RuntimeError('invocation end marker does not match tool')
        deadline = time.monotonic() + timeout
        with self.condition:
            if end_sequence in self.claimed:
                raise RuntimeError('invocation end marker already claimed')
            earlier = [sequence for sequence, marker in self.ends.items()
                       if marker == f'tool-end:{tool}' and sequence < end_sequence
                       and sequence not in self.claimed]
            if earlier:
                raise RuntimeError('tool invocation arrived out of recorded order')
            # Validation must succeed before advancing the operation cursor.
            response = validate()
            self.claimed.add(end_sequence)
            self.replay.admit(tool)
            self.condition.notify_all()
            try:
                while self.replay.sequence < end_sequence:
                    if self.failure is not None:
                        raise RuntimeError('concurrent replay failed') from self.failure
                    self.replay.apply_until(end_sequence)
                    self.condition.notify_all()
                    if self.replay.sequence >= end_sequence:
                        break
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise TimeoutError('recorded concurrent invocation did not arrive')
                    self.condition.wait(remaining)
            except BaseException as error:
                self.failure = error
                self.condition.notify_all()
                raise
            return response


if __name__ == '__main__':
    replay = Replay(sys.argv[1], sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None)
    try:
        replay.finish()
    finally:
        replay.close()
