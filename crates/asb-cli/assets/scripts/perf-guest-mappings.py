"""Enumerate executable mappings under a generation-bound guest process tree.

Private collector helper: this produces observations only, never copies or
executes a mapped file. The caller must verify and retain any image separately.
"""
import json
import os
from pathlib import Path
import stat
import struct
import sys

MAX_PROCESSES = 16384
MAX_ROOTS = 1024
MAX_MAP_LINES = 65536
MAX_MAPPINGS = 8192
MAX_PATH_BYTES = 4096
MAX_IMAGE_BYTES = 512 * 1024 * 1024


def has_build_id(fd):
    """Return true only for a bounded, well-formed GNU build-ID note."""
    try:
        header = os.pread(fd, 64, 0)
        if len(header) < 52 or header[:4] != b'\x7fELF' or header[5] not in (1, 2):
            return False
        endian = '<' if header[5] == 1 else '>'
        if header[4] == 2 and len(header) >= 64:
            offset = struct.unpack_from(endian + 'Q', header, 32)[0]
            width, count = struct.unpack_from(endian + 'HH', header, 54)
            minimum, offset_field, size_field, number = 56, 8, 32, 'Q'
        elif header[4] == 1:
            offset = struct.unpack_from(endian + 'I', header, 28)[0]
            width, count = struct.unpack_from(endian + 'HH', header, 42)
            minimum, offset_field, size_field, number = 32, 4, 16, 'I'
        else:
            return False
        size = os.fstat(fd).st_size
        if width < minimum or width > 1024 or count > 512 or offset > size or count * width > size - offset:
            return False
        for index in range(count):
            segment = os.pread(fd, width, offset + index * width)
            if len(segment) != width or struct.unpack_from(endian + 'I', segment)[0] != 4:
                continue
            start = struct.unpack_from(endian + number, segment, offset_field)[0]
            length = struct.unpack_from(endian + number, segment, size_field)[0]
            if length > 1024 * 1024 or start > size or length > size - start:
                continue
            notes = os.pread(fd, length, start); cursor = 0
            while cursor + 12 <= len(notes):
                namesz, descsz, kind = struct.unpack_from(endian + 'III', notes, cursor)
                cursor += 12; name_end = cursor + namesz
                descriptor = (name_end + 3) & ~3; end = descriptor + descsz
                if end > len(notes):
                    break
                if kind == 3 and notes[cursor:name_end] == b'GNU\0' and 4 <= descsz <= 64:
                    return True
                cursor = (end + 3) & ~3
    except (OSError, ValueError, struct.error, OverflowError):
        pass
    return False


def identity(proc, pid):
    try:
        text = (proc / str(pid) / 'stat').read_text()
        if len(text) > 65536:
            return None
        fields = text.rsplit(')', 1)[1].split()
        if fields[0] in ('Z', 'X', 'x'):
            return None
        parent, start = int(fields[1]), int(fields[19])
        return (parent, start) if start > 0 else None
    except (OSError, ValueError, IndexError):
        return None


def scan(proc, roots, include_stamps=False):
    """Return a bounded tree and executable file mappings for current roots."""
    proc = Path(proc)
    if (not roots or len(roots) > MAX_ROOTS or any(
            not isinstance(root, (list, tuple)) or len(root) != 2
            or any(type(value) is not int or value <= 1 for value in root)
            for root in roots)):
        raise ValueError('invalid guest process roots')
    live = []
    for pid, start in roots:
        current = identity(proc, pid)
        if current and current[1] == start:
            live.append((pid, start))
    if not live:
        raise ValueError('guest process generations are no longer live')
    children = {}
    count = 0
    for entry in proc.iterdir():
        if not entry.name.isdecimal():
            continue
        count += 1
        if count > MAX_PROCESSES:
            raise ValueError('guest process count exceeds bound')
        pid = int(entry.name)
        current = identity(proc, pid)
        if current:
            children.setdefault(current[0], []).append((pid, current[1]))
    members = list(dict.fromkeys(live))
    seen = set(members)
    for pid, generation in members:
        parent_now = identity(proc, pid)
        if parent_now is None or parent_now[1] != generation:
            continue
        for child in children.get(pid, ()):
            child_now = identity(proc, child[0])
            if child not in seen and child_now == (pid, child[1]):
                members.append(child)
                seen.add(child)
        if len(members) > MAX_PROCESSES:
            raise ValueError('guest process tree exceeds bound')
    mappings = []
    inaccessible = 0
    for pid, start in members:
        try:
            with (proc / str(pid) / 'maps').open(encoding='utf-8', errors='surrogateescape') as source:
                for index, line in enumerate(source):
                    if index >= MAX_MAP_LINES:
                        raise ValueError('guest process maps exceed bound')
                    fields = line.rstrip('\n').split(None, 5)
                    if (len(fields) != 6 or 'x' not in fields[1] or not fields[5].startswith('/')
                            or fields[5].endswith(' (deleted)')):
                        continue
                    path = fields[5]
                    if len(os.fsencode(path)) > MAX_PATH_BYTES or '..' in path.split('/'):
                        raise ValueError('guest image path exceeds bound or contains traversal')
                    try:
                        major, minor = (int(value, 16) for value in fields[3].split(':', 1))
                        inode = int(fields[4])
                    except ValueError:
                        continue
                    if inode <= 0:
                        continue
                    row = {'pid': pid, 'start': start, 'path': path,
                           'major': major, 'minor': minor, 'inode': inode}
                    if include_stamps:
                        try:
                            fd = os.open(proc / str(pid) / 'root' / path.lstrip('/'),
                                         os.O_RDONLY | os.O_NONBLOCK | os.O_CLOEXEC)
                            try:
                                info = os.fstat(fd)
                                if ((os.major(info.st_dev), os.minor(info.st_dev), info.st_ino)
                                        != (major, minor, inode) or not stat.S_ISREG(info.st_mode)
                                        or os.pread(fd, 4, 0) != b'\x7fELF'):
                                    continue
                                row.update(size=info.st_size, mtime_ns=info.st_mtime_ns,
                                           ctime_ns=info.st_ctime_ns, has_build_id=has_build_id(fd))
                            finally:
                                os.close(fd)
                        except OSError:
                            inaccessible += 1
                            continue
                    mappings.append(row)
                    if len(mappings) > MAX_MAPPINGS:
                        raise ValueError('guest image mappings exceed bound')
        except OSError:
            inaccessible += 1
        current = identity(proc, pid)
        if current is None or current[1] != start:
            mappings = [entry for entry in mappings if entry['pid'] != pid or entry['start'] != start]
    return {'schema_version': 1, 'members': members, 'mappings': mappings, 'inaccessible': inaccessible}


def copy_image(proc, pid, start, path, major, minor, inode, output, limit=MAX_IMAGE_BYTES, expected_stamp=None):
    """Stream one stable mapped ELF; callers must discard bytes on any error."""
    proc = Path(proc)
    if (pid <= 1 or start <= 0 or not path.startswith('/') or '..' in path.split('/')
            or len(os.fsencode(path)) > MAX_PATH_BYTES or not 0 < limit <= MAX_IMAGE_BYTES):
        raise ValueError('invalid mapped image request')
    current = identity(proc, pid)
    if current is None or current[1] != start:
        raise ValueError('guest process generation changed')
    descriptor = os.open(proc / str(pid) / 'root' / path.lstrip('/'),
                         os.O_RDONLY | os.O_NONBLOCK | os.O_CLOEXEC)
    with os.fdopen(descriptor, 'rb') as source:
        before = os.fstat(source.fileno())
        if ((os.major(before.st_dev), os.minor(before.st_dev), before.st_ino) != (major, minor, inode)
                or not stat.S_ISREG(before.st_mode) or not 0 < before.st_size <= limit
                or (expected_stamp is not None and
                    (before.st_size, before.st_mtime_ns, before.st_ctime_ns) != expected_stamp)
                or source.read(4) != b'\x7fELF'):
            raise ValueError('guest image does not match the observed mapping')
        source.seek(0)
        remaining = before.st_size
        while remaining:
            block = source.read(min(1024 * 1024, remaining))
            if not block:
                raise ValueError('guest image was truncated during capture')
            view = memoryview(block)
            while view:
                written = output.write(view)
                if written is None or written <= 0:
                    raise ValueError('guest image output could not be written completely')
                view = view[written:]
            remaining -= len(block)
        after = os.fstat(source.fileno())
        current = identity(proc, pid)
        if ((before.st_size, before.st_mtime_ns, before.st_ctime_ns)
                != (after.st_size, after.st_mtime_ns, after.st_ctime_ns)
                or current is None or current[1] != start):
            raise ValueError('guest image or process changed during capture')
        return before.st_size


def main():
    if len(sys.argv) >= 2 and sys.argv[1] == 'copy':
        if len(sys.argv) not in (8, 11):
            raise ValueError('expected image process and mapping identity')
        pid, start = map(int, sys.argv[2:4])
        path = sys.argv[4]
        major, minor, inode = map(int, sys.argv[5:8])
        stamp = tuple(map(int, sys.argv[8:11])) if len(sys.argv) == 11 else None
        copy_image(Path('/proc'), pid, start, path, major, minor, inode, sys.stdout.buffer, expected_stamp=stamp)
        return
    if len(sys.argv) == 3 and sys.argv[1] == 'scan-roots':
        if len(os.fsencode(sys.argv[2])) > 65536:
            raise ValueError('guest roots exceed bound')
        roots = json.loads(sys.argv[2])
        if not isinstance(roots, list):
            raise ValueError('invalid guest process roots')
        result = scan(Path('/proc'), roots, include_stamps=True)
        print(json.dumps(result, ensure_ascii=True, separators=(',', ':')))
        return
    if len(sys.argv) != 4 or sys.argv[1] != 'scan':
        raise ValueError('expected scan or copy and process identity')
    pid, start = map(int, sys.argv[2:])
    if pid <= 1 or start <= 0:
        raise ValueError('invalid namespace PID or process start')
    result = scan(Path('/proc'), [(pid, start)])
    print(json.dumps(result, ensure_ascii=True, separators=(',', ':')))


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError) as error:
        print(f'guest perf mapping scan failed: {error}', file=sys.stderr)
        sys.exit(2)
