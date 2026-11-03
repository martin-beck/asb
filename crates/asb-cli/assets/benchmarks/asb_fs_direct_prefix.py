"""Validate direct startup effects before acknowledging a replay prefix.

Experimental evidence comparison only; not a mounted replay filesystem.
"""
import hashlib
import json
import os
import posixpath
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
    'observe-getattr': ('path',),
    'observe-readdir': ('path',),
    'observe-readlink': ('path',),
    'observe-getxattr': ('path', 'name', 'position'),
    'observe-listxattr': ('path',),
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
        directory_listings = {}
        def next_actual():
            while True:
                line = next(observed, None)
                if line is None:
                    return None, None
                if not line.endswith('\n'):
                    raise ValueError('observed direct filesystem event stream is truncated')
                actual = json.loads(line)
                scope = observed_attribution.observe(actual)
                if scope['kind'] == 'handle-release':
                    if scope['opened_as'] != {'kind':'direct'}:
                        raise ValueError('observed direct release has an unproven opener')
                elif scope['kind'] != 'direct':
                    raise ValueError('observed direct operation has unproven attribution')
                if actual.get('op') != 'observe-inferred-absent':
                    return actual, scope
                path = actual.get('path')
                if not isinstance(path, str) or not path.startswith('/'):
                    raise ValueError('inferred filesystem absence is invalid')
                parts = path.split('/')
                if len(parts) < 2 or any(part in ('', '.', '..') for part in parts[1:]):
                    raise ValueError('inferred filesystem absence is invalid')
                parent, name = posixpath.split(path); parent = parent or '/'
                listing = directory_listings.get(parent)
                if (actual.get('requested_op') != 'getattr' or actual.get('error') != 2
                        or actual.get('parent') != parent or listing is None or name in listing):
                    raise ValueError('inferred filesystem absence is unproven')
        pending = None
        while True:
            expected_line = pending if pending is not None else next(expected, None)
            pending = None
            if expected_line is None:
                raise ValueError('direct filesystem prefix has no tool boundary')
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
            actual, actual_scope = next_actual()
            if actual is None:
                raise ValueError('direct filesystem replay ended before the recorded prefix')
            op = event.get('op')
            # Parallel reads on already-open handles can complete in either
            # order. Never move them across another operation or boundary.
            if op == 'read' and actual.get('op') == 'read':
                expected_reads = [event]
                actual_reads = [actual]
                while True:
                    line = next(expected, None)
                    if line is None or not line.endswith('\n'):
                        raise ValueError('direct filesystem prefix is truncated')
                    candidate = json.loads(line)
                    if candidate.get('op') != 'read':
                        pending = line
                        break
                    scope = expected_attribution.observe(candidate)
                    if scope['kind'] != 'direct':
                        raise ValueError('direct filesystem prefix has unproven attribution')
                    expected_reads.append(candidate)
                    candidate_actual, candidate_scope = next_actual()
                    if candidate_actual is None:
                        raise ValueError('direct filesystem replay ended before the recorded prefix')
                    if candidate_scope['kind'] != 'direct':
                        raise ValueError('observed direct operation has unproven attribution')
                    actual_reads.append(candidate_actual)
                reverse_handles = {value: key for key, value in handles.items()}
                def read_key(item, data, mapping):
                    handle = item.get('handle')
                    if item.get('op') != 'read' or handle not in mapping:
                        raise ValueError('direct filesystem read group differs')
                    fields = _FIELDS['read']
                    if any(field not in item for field in fields):
                        raise ValueError('direct filesystem read request is incomplete')
                    return (mapping[handle],) + tuple(item[field] for field in fields) + (_digest_range(data, item),)
                from collections import Counter
                left = Counter(read_key(item, expected_data, {h: h for h in handles}) for item in expected_reads)
                right = Counter(read_key(item, observed_data, reverse_handles) for item in actual_reads)
                if left != right:
                    raise ValueError(f'direct filesystem read group differs at event {count + 1}')
                count += len(expected_reads)
                continue
            fields = _FIELDS.get(op)
            if fields is None or actual.get('op') != op:
                raise ValueError(f'direct filesystem operation differs or is unsupported at event {count + 1}')
            mismatch = next((field for field in fields
                             if field not in event or field not in actual or event[field] != actual[field]), None)
            if mismatch is not None:
                raise ValueError(f'direct filesystem request or result differs at event {count + 1} ({mismatch})')
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
            if op.startswith('observe-'):
                for item in (event, actual):
                    failure = 'error' in item
                    if failure and (type(item['error']) is not int or item['error'] <= 0 or 'result' in item or 'object' in item):
                        raise ValueError('direct filesystem observation is invalid')
                    if not failure and ((op == 'observe-getxattr' and 'result' in item)
                                        or (op != 'observe-getxattr' and 'result' not in item)):
                        raise ValueError('direct filesystem observation is invalid')
                for field in ('error', 'result'):
                    if (field in event) != (field in actual) or event.get(field) != actual.get(field):
                        raise ValueError(f'direct filesystem observation differs at event {count + 1}')
                if op == 'observe-getxattr' and 'error' not in event:
                    if _digest_range(expected_data, event) != _digest_range(observed_data, actual):
                        raise ValueError('direct filesystem observation payload differs')
            if op in _PAYLOAD_OPS and _digest_range(expected_data, event) != _digest_range(observed_data, actual):
                raise ValueError('direct filesystem operation payload differs')
            if op == 'observe-readdir' and 'error' not in event:
                listing = event.get('result')
                if (not isinstance(listing, list) or any(not isinstance(name, str) or not name or '/' in name for name in listing)
                        or len(listing) != len(set(listing)) or '.' not in listing or '..' not in listing):
                    raise ValueError('recorded filesystem directory listing is invalid')
                directory_listings[event['path']] = frozenset(listing)
            flags = event.get('flags')
            read_only_open = (op == 'open' and type(flags) is int and flags & os.O_ACCMODE == os.O_RDONLY
                              and not flags & (os.O_CREAT | os.O_TRUNC))
            if not op.startswith('observe-') and op not in ('read', 'release') and not read_only_open:
                directory_listings.clear()
            count += 1
        if handles or expected_attribution.handles or observed_attribution.handles:
            raise ValueError('direct filesystem prefix has open handles')
        if next_actual()[0] is not None:
            raise ValueError('direct filesystem replay has additional operations')
    return count


def validate_no_direct_tail(events, prefix_count):
    """Reject later direct or unclassified operations until live replay exists."""
    attribution = Attribution()
    for line in events:
        if not line.endswith('\n'):
            raise ValueError('filesystem event stream is truncated')
        event = json.loads(line)
        scope = attribution.observe(event)
        if event['sequence'] <= prefix_count:
            continue
        if scope['kind'] in ('unknown', 'ambiguous', 'shared-handle', 'direct'):
            raise ValueError('filesystem replay has direct or unclassified operations after startup')
        if scope['kind'] == 'handle-release' and scope['opened_as'].get('kind') != 'tool':
            raise ValueError('filesystem replay has an unproven handle release after startup')
    attribution.finish()
    if attribution.handles:
        raise ValueError('filesystem replay has unsettled handles')
