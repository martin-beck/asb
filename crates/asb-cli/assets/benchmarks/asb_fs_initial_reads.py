"""Experimental planning for fully observed pre-mutation file contents.

This produces evidence plans only; it does not modify a replay workspace.
"""
import json
import os
import stat


_MUTATIONS = {'create', 'write', 'truncate', 'chmod', 'utimens', 'setxattr',
              'removexattr', 'rename', 'link', 'symlink', 'unlink', 'mkdir', 'rmdir'}


def plan_initial_reads(events):
    """Return complete pre-mutation reads whose initial size was recorded.

    Stop at the first filesystem mutation anywhere. Reject hardlinked files,
    inconsistent metadata, malformed segments and files with gaps. The caller
    must still verify overlapping data and the destination before applying any
    plan; a plan alone does not prove that the entire workspace is equivalent.
    """
    handles = {}
    candidates = {}
    previous = 0
    for line in events:
        if not line.endswith('\n'):
            raise ValueError('truncated filesystem event stream')
        event = json.loads(line)
        sequence = event.get('sequence')
        if type(sequence) is not int or sequence != previous + 1:
            raise ValueError('filesystem event sequence has a gap')
        previous = sequence
        op = event.get('op')
        if op in _MUTATIONS:
            break
        if op == 'open':
            flags = event.get('flags')
            if type(flags) is not int or flags & os.O_ACCMODE != os.O_RDONLY or flags & (os.O_TRUNC | os.O_CREAT):
                break
            handle, path = event.get('handle'), event.get('path')
            size, mode, links = event.get('initial_size'), event.get('initial_mode'), event.get('initial_nlink')
            if type(handle) is not int or handle in handles:
                raise ValueError('filesystem open handle is invalid')
            if (not isinstance(path, str)
                    or not path.startswith('/') or type(size) is not int or size < 0
                    or type(mode) is not int or type(links) is not int):
                # Older journals have no initial metadata; they cannot provide
                # a complete initial-state proof.
                handles[handle] = None
                continue
            if not stat.S_ISREG(mode) or links != 1:
                handles[handle] = None
                continue
            entry = candidates.setdefault(path, {'size': size, 'mode': mode, 'segments': [], 'valid': True})
            if entry['size'] != size or entry['mode'] != mode:
                entry['valid'] = False
            handles[handle] = path
        elif op == 'read':
            handle = event.get('handle')
            if type(handle) is not int or handle not in handles:
                raise ValueError('filesystem read handle is invalid')
            path = handles[handle]
            if path is None:
                existing = candidates.get(event.get('path'))
                if existing is not None:
                    existing['valid'] = False
                continue
            entry = candidates[path]
            offset, length, payload_offset = event.get('offset'), event.get('bytes'), event.get('payload_offset')
            if (event.get('path') != path or event.get('object') != 'payload.bin'
                    or type(offset) is not int or offset < 0 or type(length) is not int or length < 0
                    or type(payload_offset) is not int or payload_offset < 0 or offset + length > entry['size']):
                entry['valid'] = False
                continue
            if length:
                entry['segments'].append((offset, length, payload_offset))
        elif op == 'release':
            handle = event.get('handle')
            if type(handle) is not int or handle not in handles:
                raise ValueError('filesystem release handle is invalid')
            handles.pop(handle)
        elif op != 'marker':
            # An unknown operation may have changed state; never assume it did not.
            break
    result = {}
    for path, entry in candidates.items():
        if not entry['valid']:
            continue
        segments = sorted(entry['segments'])
        end = 0
        for offset, length, _ in segments:
            if offset > end:
                break
            end = max(end, offset + length)
        if end == entry['size']:
            result[path] = {'size': entry['size'], 'mode': entry['mode'], 'segments': segments}
    return result
