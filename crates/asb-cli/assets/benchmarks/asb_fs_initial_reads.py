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


def restore_initial_reads(evidence, workspace, scratch):
    """Apply completely evidenced initial reads before the agent starts.

    This experimental preparation verifies every segment and destination
    before writing. It does not establish inode, ctime, or unobserved-file
    equivalence, and a later I/O failure can leave partially applied files.
    """
    from contextlib import ExitStack
    from pathlib import Path
    import tempfile

    evidence, workspace, scratch = map(Path, (evidence, workspace, scratch))
    with (evidence / 'events.jsonl').open(encoding='utf-8') as stream:
        plans = plan_initial_reads(stream)
    prepared = []
    with ExitStack() as resources:
        payload = resources.enter_context((evidence / 'payload.bin').open('rb'))
        for name, plan in sorted(plans.items()):
            relative = Path(name.lstrip('/'))
            if not relative.parts or '..' in relative.parts:
                raise ValueError('initial read path is not a workspace file')
            path = workspace / relative
            current = resources.enter_context(path.open('rb'))
            before = os.fstat(current.fileno())
            if (not stat.S_ISREG(before.st_mode) or before.st_nlink != 1
                    or before.st_size != plan['size'] or before.st_mode != plan['mode']):
                raise ValueError('initial read destination metadata differs')
            staged = resources.enter_context(tempfile.TemporaryFile(dir=scratch))
            covered = 0
            for offset, length, source_offset in plan['segments']:
                if offset > covered:
                    raise ValueError('initial read plan has a gap')
                payload.seek(source_offset)
                pos = offset
                remaining = length
                while remaining:
                    chunk = payload.read(min(65536, remaining))
                    if not chunk:
                        raise ValueError('initial read payload is truncated')
                    overlap = min(len(chunk), max(0, covered - pos))
                    if overlap:
                        staged.seek(pos)
                        if staged.read(overlap) != chunk[:overlap]:
                            raise ValueError('overlapping initial reads differ')
                    if overlap < len(chunk):
                        staged.seek(pos + overlap)
                        if staged.write(chunk[overlap:]) != len(chunk) - overlap:
                            raise OSError('short initial read staging write')
                    pos += len(chunk)
                    remaining -= len(chunk)
                    covered = max(covered, pos)
            if covered != plan['size']:
                raise ValueError('initial read plan is incomplete')
            current.seek(0); staged.seek(0)
            differs = False
            while True:
                recorded = staged.read(65536)
                actual = current.read(len(recorded)) if recorded else b''
                if actual != recorded:
                    differs = True
                if not recorded:
                    break
            writer = resources.enter_context(path.open('r+b')) if differs else None
            if writer is not None:
                current_stat = os.fstat(writer.fileno())
                if (current_stat.st_dev, current_stat.st_ino, current_stat.st_size,
                    current_stat.st_mtime_ns, current_stat.st_ctime_ns) != (
                        before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns):
                    raise ValueError('initial read destination changed during preparation')
            prepared.append((writer, staged, before))
        for writer, staged, before in prepared:
            if writer is None:
                continue
            staged.seek(0); writer.seek(0)
            while chunk := staged.read(65536):
                if writer.write(chunk) != len(chunk):
                    raise OSError('short initial read destination write')
            writer.flush()
            os.fsync(writer.fileno())
            os.utime(writer.fileno(), ns=(before.st_atime_ns, before.st_mtime_ns))
    return len(plans)
