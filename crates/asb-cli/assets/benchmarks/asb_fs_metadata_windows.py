"""Experimental causal windows for serving recorded metadata observations.

Only adjacent metadata observations may reorder. The caller must execute each
intervening recorded operation before moving to the following window.
"""
from collections import defaultdict, deque
import errno
import os
import posixpath
import stat
from functools import wraps
from pathlib import Path
import threading

_FIELDS = {
    'observe-getattr': (),
    'observe-readdir': (),
    'observe-readlink': (),
    'observe-getxattr': ('name', 'position'),
    'observe-listxattr': (),
}


def _synchronized(method):
    @wraps(method)
    def locked(self, *args, **kwargs):
        with self.lock:
            return method(self, *args, **kwargs)
    return locked


class InferredAbsent(FileNotFoundError):
    """A complete, unchanged recorded parent listing proves this absence."""
    def __init__(self, parent):
        super().__init__(errno.ENOENT, 'recorded parent listing proves absence')
        self.parent = parent


class BorrowedMetadata:
    """A later recorded observation proven stable across earlier operations."""
    def __init__(self, result, sequence):
        self.result = result
        self.sequence = sequence


class ObservationWindow:
    def __init__(self, events, payload):
        self.lock = threading.RLock()
        self.events = iter(events)
        self.buffer = deque()
        self.borrowed = set()
        self.payload = Path(payload)
        self.pending = defaultdict(deque)
        self.boundary = None
        self.ended = False
        self.in_flight = False
        self.in_flight_event = None
        self.directory_listings = {}
        self._load()

    @staticmethod
    def _key(op, path, detail):
        if op not in _FIELDS or not isinstance(path, str) or not path.startswith('/'):
            raise ValueError('filesystem observation request is invalid')
        if set(detail) != set(_FIELDS[op]):
            raise ValueError('filesystem observation request is invalid')
        values = tuple(detail[key] for key in _FIELDS[op])
        if op == 'observe-getxattr' and (not isinstance(values[0], str) or type(values[1]) is not int or values[1] < 0):
            raise ValueError('filesystem observation request is invalid')
        return (op, path, *values)

    def _next_event(self):
        if self.buffer:
            return self.buffer.popleft()
        return next(self.events, None)

    def _load(self):
        while True:
            event = self._next_event()
            if event is None:
                self.ended = True
                self.boundary = None
                return
            if event.get('sequence') in self.borrowed:
                self.borrowed.remove(event['sequence'])
                continue
            op = event.get('op')
            if op not in _FIELDS:
                self.boundary = event
                return
            detail = {name: event.get(name) for name in _FIELDS[op]}
            key = self._key(op, event.get('path'), detail)
            self.pending[key].append(event)

    def _borrow_stable_getattr(self, path):
        # Restrict lookahead to a root-level regular file with one link. Never
        # cross a mutation, a tool marker, or an access to this same path.
        if path.count('/') != 1 or path in ('/', '/.', '/..') or not self.boundary:
            return None
        def safe(event):
            op = event.get('op')
            if event.get('path') == path:
                return False
            if op in _FIELDS or op in ('read', 'release'):
                return True
            flags = event.get('flags')
            return (op == 'open' and type(flags) is int and flags & os.O_ACCMODE == os.O_RDONLY
                    and not flags & (os.O_CREAT | os.O_TRUNC))
        if not safe(self.boundary):
            return None
        index = 0
        while True:
            if index == len(self.buffer):
                event = next(self.events, None)
                if event is None:
                    return None
                self.buffer.append(event)
            event = self.buffer[index]; index += 1
            if event.get('sequence') in self.borrowed:
                continue
            if event.get('op') == 'observe-getattr' and event.get('path') == path:
                result = event.get('result'); sequence = event.get('sequence')
                fields = {'st_gid','st_ino','st_mode','st_nlink','st_size','st_uid',
                          'st_blocks','st_blksize','st_atime','st_mtime','st_ctime'}
                if (type(sequence) is int and sequence > 0 and isinstance(result, dict)
                        and set(result) == fields and all(type(value) is int for value in result.values())
                        and stat.S_ISREG(result['st_mode']) and result['st_nlink'] == 1
                        and 'error' not in event and 'object' not in event):
                    self.borrowed.add(sequence)
                    return BorrowedMetadata(result, sequence)
                return None
            if not safe(event):
                return None

    @_synchronized
    def observe(self, op, path, **detail):
        if self.in_flight:
            raise RuntimeError('recorded filesystem operation has not been applied')
        key = self._key(op, path, detail)
        queue = self.pending.get(key)
        if not queue:
            parts = path.split('/')
            if (op == 'observe-getattr' and len(parts) >= 2 and all(part not in ('', '.', '..') for part in parts[1:])):
                parent, name = posixpath.split(path)
                parent = parent or '/'
                listing = self.directory_listings.get(parent)
                if listing is not None and name not in listing:
                    raise InferredAbsent(parent)
                if listing is not None and name in listing:
                    borrowed = self._borrow_stable_getattr(path)
                    if borrowed is not None:
                        return borrowed
            raise RuntimeError('no recorded filesystem observation in the current causal window')
        event = queue.popleft()
        if not queue:
            del self.pending[key]
        failure = 'error' in event
        if failure:
            number = event['error']
            if type(number) is not int or number <= 0 or 'result' in event or 'object' in event:
                raise ValueError('recorded filesystem observation is invalid')
            raise OSError(number, errno.errorcode.get(number, 'recorded filesystem error'))
        if op == 'observe-getxattr':
            offset, length = event.get('payload_offset'), event.get('bytes')
            if (event.get('object') != 'payload.bin' or type(offset) is not int or offset < 0
                    or type(length) is not int or length < 0 or 'result' in event):
                raise ValueError('recorded filesystem observation is invalid')
            with self.payload.open('rb') as payload:
                payload.seek(offset)
                value = payload.read(length)
            if len(value) != length:
                raise ValueError('recorded filesystem observation payload is truncated')
            return value
        if 'result' not in event or 'object' in event:
            raise ValueError('recorded filesystem observation is invalid')
        result = event['result']
        if op == 'observe-getattr':
            fields = {'st_gid','st_ino','st_mode','st_nlink','st_size','st_uid',
                      'st_blocks','st_blksize','st_atime','st_mtime','st_ctime'}
            if not isinstance(result, dict) or set(result) != fields or any(type(value) is not int for value in result.values()):
                raise ValueError('recorded filesystem observation is invalid')
        elif op in ('observe-readdir', 'observe-listxattr'):
            if not isinstance(result, list) or any(not isinstance(value, str) for value in result):
                raise ValueError('recorded filesystem observation is invalid')
            if op == 'observe-readdir':
                if (len(result) != len(set(result)) or '.' not in result or '..' not in result
                        or any('/' in value or not value for value in result)):
                    raise ValueError('recorded filesystem directory listing is invalid')
                self.directory_listings[path] = frozenset(result)
        elif op == 'observe-readlink' and not isinstance(result, str):
            raise ValueError('recorded filesystem observation is invalid')
        return result

    @_synchronized
    def advance(self):
        """Return the next operation only after this window is consumed.

        The caller must apply the returned operation before serving the next
        window; no agent request may race ahead of that application.
        """
        if self.in_flight:
            raise RuntimeError('recorded filesystem operation has not been applied')
        if self.pending:
            raise RuntimeError('recorded filesystem observations remain in the current causal window')
        event = self.boundary
        if event is None:
            return None
        self.boundary = None
        self.in_flight = True
        self.in_flight_event = event
        return event

    @_synchronized
    def operation_applied(self):
        if not self.in_flight or self.boundary is not None or self.pending or self.ended:
            raise RuntimeError('filesystem observation window is not ready to advance')
        event = self.in_flight_event
        op = event.get('op')
        flags = event.get('flags')
        read_only_open = (op == 'open' and type(flags) is int
                          and flags & os.O_ACCMODE == os.O_RDONLY
                          and not flags & (os.O_CREAT | os.O_TRUNC))
        if op not in ('read', 'release') and not read_only_open:
            self.directory_listings.clear()
        self.in_flight = False
        self.in_flight_event = None
        self._load()
