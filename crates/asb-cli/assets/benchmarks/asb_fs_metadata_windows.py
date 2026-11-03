"""Experimental causal windows for serving recorded metadata observations.

Only adjacent metadata observations may reorder. The caller must execute each
intervening recorded operation before moving to the following window.
"""
from collections import defaultdict, deque
import errno
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


class ObservationWindow:
    def __init__(self, events, payload):
        self.lock = threading.RLock()
        self.events = iter(events)
        self.payload = Path(payload)
        self.pending = defaultdict(deque)
        self.boundary = None
        self.ended = False
        self.in_flight = False
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

    def _load(self):
        for event in self.events:
            op = event.get('op')
            if op not in _FIELDS:
                self.boundary = event
                return
            detail = {name: event.get(name) for name in _FIELDS[op]}
            key = self._key(op, event.get('path'), detail)
            self.pending[key].append(event)
        self.ended = True
        self.boundary = None

    @_synchronized
    def observe(self, op, path, **detail):
        if self.in_flight:
            raise RuntimeError('recorded filesystem operation has not been applied')
        key = self._key(op, path, detail)
        queue = self.pending.get(key)
        if not queue:
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
        return event

    @_synchronized
    def operation_applied(self):
        if not self.in_flight or self.boundary is not None or self.pending or self.ended:
            raise RuntimeError('filesystem observation window is not ready to advance')
        self.in_flight = False
        self._load()
