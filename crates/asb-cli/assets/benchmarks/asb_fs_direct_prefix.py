"""Validate direct startup effects before acknowledging a replay prefix.

Experimental evidence comparison only; not a mounted replay filesystem.
"""
import hashlib
import json
from pathlib import Path

from asb_fs_attribution import Attribution

_FIELDS = {
    'open': ('path', 'flags', 'initial_size', 'initial_mode', 'initial_nlink'),
    'create': ('path', 'flags', 'mode'),
    'read': ('path', 'offset', 'requested_size', 'bytes'),
    'write': ('path', 'offset', 'requested_size', 'request_sha256', 'bytes'),
    'release': ('path',),
    'truncate': ('path', 'size'),
    'chmod': ('path', 'mode'),
    'utimens': ('path', 'atime_ns', 'mtime_ns'),
    'setxattr': ('path', 'name', 'options', 'bytes'),
    'removexattr': ('path', 'name'),
    'rename': ('path', 'destination'),
    'link': ('path', 'destination'),
    'symlink': ('path', 'destination'),
    'unlink': ('path',),
    'mkdir': ('path', 'mode'),
    'rmdir': ('path',),
}
_HANDLE_OPS = {'read', 'write', 'release', 'truncate'}
_PAYLOAD_OPS = {'read', 'write', 'setxattr'}


def _digest_range(stream, event):
    offset, length = event.get('payload_offset'), event.get('bytes')
    if (event.get('object') != 'payload.bin' or type(offset) is not int or offset < 0
            or type(length) is not int or length < 0):
        raise ValueError('direct filesystem payload metadata is invalid')
    stream.seek(offset)
    digest = hashlib.sha256()
    while length:
        chunk = stream.read(min(length, 65536))
        if not chunk:
            raise ValueError('direct filesystem payload is truncated')
        digest.update(chunk); length -= len(chunk)
    return digest.digest()


def validate_direct_prefix(expected_events, expected_payload, observed_events, observed_payload):
    """Validate a fully observed startup prefix and return its event count.

    Requires new lineage and request-shape evidence. Every expected operation
    must be direct or a handle release from a direct open; all handles must be
    closed before the first tool marker. Unknown metadata, extra operations and
    unrecorded request sizes fail validation rather than being ignored.
    """
    expected_attribution, observed_attribution = Attribution(), Attribution()
    handles, used_observed = {}, set()
    count = 0
    with Path(expected_payload).open('rb') as expected_data, Path(observed_payload).open('rb') as observed_data:
        expected = iter(expected_events); observed = iter(observed_events)
        for expected_line in expected:
            if not expected_line.endswith('\n'):
                raise ValueError('direct filesystem event stream is truncated')
            event = json.loads(expected_line)
            scope = expected_attribution.observe(event)
            if scope['kind'] == 'marker':
                if not event['marker'].startswith('tool-begin:'):
                    raise ValueError('direct filesystem prefix has an unexpected marker')
                break
            if scope['kind'] == 'handle-release':
                if scope['opened_as'] != {'kind':'direct'}:
                    raise ValueError('direct filesystem release has an unproven opener')
            elif scope['kind'] != 'direct':
                raise ValueError('direct filesystem prefix has unproven attribution')
            try:
                actual_line = next(observed)
            except StopIteration as error:
                raise ValueError('direct filesystem replay ended before the recorded prefix') from error
            if not actual_line.endswith('\n'):
                raise ValueError('observed direct filesystem event stream is truncated')
            actual = json.loads(actual_line)
            actual_scope = observed_attribution.observe(actual)
            if actual_scope['kind'] == 'handle-release':
                if actual_scope['opened_as'] != {'kind':'direct'}:
                    raise ValueError('observed direct release has an unproven opener')
            elif actual_scope['kind'] != 'direct':
                raise ValueError('observed direct operation has unproven attribution')
            op = event.get('op')
            fields = _FIELDS.get(op)
            if fields is None or actual.get('op') != op:
                raise ValueError('direct filesystem operation differs or is unsupported')
            if any(field not in event or field not in actual or event[field] != actual[field] for field in fields):
                raise ValueError('direct filesystem request or result differs')
            if op == 'write':
                digest = event.get('request_sha256')
                if (not isinstance(digest, str) or len(digest) != 64
                        or any(c not in '0123456789abcdef' for c in digest)):
                    raise ValueError('direct filesystem write digest is invalid')
            if op in ('open', 'create'):
                left, right = event.get('handle'), actual.get('handle')
                if (type(left) is not int or left <= 0 or type(right) is not int or right <= 0
                        or left in handles or right in used_observed):
                    raise ValueError('direct filesystem open handle is invalid')
                handles[left] = right; used_observed.add(right)
            elif op in _HANDLE_OPS:
                left, right = event.get('handle'), actual.get('handle')
                if not (op == 'truncate' and left is None and right is None):
                    if left not in handles or handles[left] != right:
                        raise ValueError('direct filesystem handle differs')
                    if op == 'release':
                        handles.pop(left); used_observed.remove(right)
            if op in _PAYLOAD_OPS and _digest_range(expected_data, event) != _digest_range(observed_data, actual):
                raise ValueError('direct filesystem operation payload differs')
            count += 1
        else:
            raise ValueError('direct filesystem prefix has no tool boundary')
        if handles or expected_attribution.handles or observed_attribution.handles:
            raise ValueError('direct filesystem prefix has open handles')
        if next(observed, None) is not None:
            raise ValueError('direct filesystem replay has additional operations')
    return count
