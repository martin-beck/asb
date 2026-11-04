"""Collect mapped ELF files while a benchmark process tree is alive.

Private ASB helper: callers must supply an already-created user-owned cache
home and supervise this process until its verified root exits. It never executes
mapped files. The cache consumer runs only after dropping elevated privileges.
"""
import os
import json
import pathlib
import re
import signal
import stat
import struct
import subprocess
import tempfile
import sys
import time

MAX_PROCESSES = 16384
MAX_MAP_LINES = 65536
MAX_IMAGES = 2048
MAX_IMAGE_BYTES = 512 * 1024 * 1024
MAX_TOTAL_BYTES = 2 * 1024 * 1024 * 1024


def identity(pid):
    try:
        value = pathlib.Path(f'/proc/{pid}/stat').read_text()
        fields = value.rsplit(')', 1)[1].split()
        if fields[0] in ('Z', 'X', 'x'):
            return None
        return int(fields[1]), int(fields[19])
    except (OSError, ValueError, IndexError):
        return None


def tree(roots):
    # Retain verified descendants even when an earlier parent exits and they
    # are reparented; never adopt a reused PID with a different start time.
    live = []
    for pid, expected_start in roots:
        current = identity(pid)
        if current is not None and current[1] == expected_start:
            live.append((pid, expected_start))
    if not live:
        return None
    children = {}
    count = 0
    for entry in pathlib.Path('/proc').iterdir():
        if not entry.name.isdecimal():
            continue
        count += 1
        if count > MAX_PROCESSES:
            return None
        pid = int(entry.name)
        record = identity(pid)
        if record is not None:
            children.setdefault(record[0], []).append((pid, record[1]))
    found = list(live)
    visited = set(found)
    for parent, _ in found:
        for child in children.get(parent, ()):
            if child not in visited:
                visited.add(child)
                found.append(child)
        if len(found) > MAX_PROCESSES:
            return None
    return found


def mappings(pid):
    try:
        with open(f'/proc/{pid}/maps', encoding='utf-8', errors='surrogateescape') as source:
            for index, line in enumerate(source):
                if index >= MAX_MAP_LINES:
                    break
                fields = line.rstrip('\n').split(None, 5)
                if len(fields) != 6 or 'x' not in fields[1] or not fields[5].startswith('/'):
                    continue
                if fields[5].endswith(' (deleted)'):
                    continue
                try:
                    major, minor = (int(value, 16) for value in fields[3].split(':', 1))
                    yield fields[5], (os.makedev(major, minor), int(fields[4]))
                except ValueError:
                    continue
    except OSError:
        return


def elf_buildid(fd):
    """Return a GNU build ID from bounded ELF program-header notes."""
    try:
        header = os.pread(fd, 64, 0)
        if len(header) < 52 or header[:4] != b'\x7fELF' or header[5] not in (1, 2):
            return None
        endian = '<' if header[5] == 1 else '>'
        if header[4] == 2 and len(header) >= 64:
            phoff = struct.unpack_from(endian + 'Q', header, 32)[0]
            entsize, count = struct.unpack_from(endian + 'HH', header, 54)
            minimum = 56
            fields = (8, 32, 'Q')
        elif header[4] == 1:
            phoff = struct.unpack_from(endian + 'I', header, 28)[0]
            entsize, count = struct.unpack_from(endian + 'HH', header, 42)
            minimum = 32
            fields = (4, 16, 'I')
        else:
            return None
        if entsize < minimum or entsize > 1024 or count > 512:
            return None
        size = os.fstat(fd).st_size
        if phoff > size or count * entsize > size - phoff:
            return None
        for index in range(count):
            ph = os.pread(fd, entsize, phoff + index * entsize)
            if len(ph) != entsize or struct.unpack_from(endian + 'I', ph)[0] != 4:
                continue
            offset = struct.unpack_from(endian + fields[2], ph, fields[0])[0]
            length = struct.unpack_from(endian + fields[2], ph, fields[1])[0]
            if length > 1024 * 1024 or offset > size or length > size - offset:
                continue
            notes = os.pread(fd, length, offset)
            cursor = 0
            while cursor + 12 <= len(notes):
                namesz, descsz, kind = struct.unpack_from(endian + 'III', notes, cursor)
                cursor += 12
                name_end = cursor + namesz
                descriptor = (name_end + 3) & ~3
                end = descriptor + descsz
                if end > len(notes):
                    break
                if kind == 3 and notes[cursor:name_end] == b'GNU\0' and 4 <= descsz <= 64:
                    return notes[descriptor:end].hex()
                cursor = (end + 3) & ~3
    except (OSError, ValueError, struct.error, OverflowError):
        return None
    return None


def elf_has_debug_info(fd):
    """Recognize bounded DWARF sections; a matching build ID alone is insufficient."""
    try:
        header = os.pread(fd, 64, 0)
        if len(header) < 52 or header[:4] != b'\x7fELF' or header[5] not in (1, 2):
            return False
        endian = '<' if header[5] == 1 else '>'
        if header[4] == 2 and len(header) >= 64:
            offset = struct.unpack_from(endian + 'Q', header, 40)[0]
            width, count, names_index = struct.unpack_from(endian + 'HHH', header, 58)
            minimum, offset_field, size_field, number = 64, 24, 32, 'Q'
        elif header[4] == 1:
            offset = struct.unpack_from(endian + 'I', header, 32)[0]
            width, count, names_index = struct.unpack_from(endian + 'HHH', header, 46)
            minimum, offset_field, size_field, number = 40, 16, 20, 'I'
        else:
            return False
        size = os.fstat(fd).st_size
        if width < minimum or width > 1024 or not 0 < count <= 8192 or names_index >= count:
            return False
        if offset > size or count * width > size - offset:
            return False
        entries = os.pread(fd, count * width, offset)
        table = entries[names_index * width:(names_index + 1) * width]
        names_offset = struct.unpack_from(endian + number, table, offset_field)[0]
        names_size = struct.unpack_from(endian + number, table, size_field)[0]
        if names_size > 8 * 1024 * 1024 or names_offset > size or names_size > size - names_offset:
            return False
        names = os.pread(fd, names_size, names_offset)
        for index in range(count):
            name = struct.unpack_from(endian + 'I', entries, index * width)[0]
            end = names.find(b'\0', name) if name < len(names) else -1
            if end >= 0 and names[name:end] in (b'.debug_info', b'.zdebug_info'):
                return True
    except (OSError, ValueError, struct.error, OverflowError):
        return False
    return False


def verify_buildids(text, cache_home):
    """Compare perf's bounded build-ID listing against retained ELF/debug files."""
    if len(text) > 4 * 1024 * 1024:
        raise ValueError('perf build-ID listing exceeds the supported bound')
    if not text.strip():
        raise ValueError('perf build-ID listing is empty')
    root = os.path.realpath(os.path.join(cache_home, '.debug'))
    missing = set()
    missing_debug = set()
    unverified = 0
    lines = text.splitlines()
    if len(lines) > 65536:
        raise ValueError('perf build-ID listing has too many rows')
    for line in lines:
        parts = line.split(None, 1)
        if not parts:
            continue
        if len(parts) == 1:
            unverified += 1
            continue
        buildid, name = parts
        if name.startswith('[') and name.endswith(']'):
            unverified += 1
            continue
        if not re.fullmatch(r'(?:[0-9a-f]{2}){4,64}', buildid):
            unverified += 1
            continue
        cache = os.path.join(root, '.build-id', buildid[:2], buildid[2:])
        try:
            resolved = os.path.realpath(cache)
            if os.path.commonpath((root, resolved)) != root:
                raise OSError('cache entry leaves its root')
            descriptor = os.open(os.path.join(resolved, 'elf'), os.O_RDONLY | os.O_NOFOLLOW)
            try:
                matches = elf_buildid(descriptor) == buildid
                embedded_debug = matches and elf_has_debug_info(descriptor)
            finally:
                os.close(descriptor)
            if not matches:
                raise OSError('cache ELF does not match build ID')
        except (OSError, ValueError):
            missing.add(buildid)
            continue
        if embedded_debug:
            continue
        try:
            descriptor = os.open(os.path.join(resolved, 'debug'), os.O_RDONLY | os.O_NOFOLLOW)
            try:
                matches = elf_buildid(descriptor) == buildid and elf_has_debug_info(descriptor)
            finally:
                os.close(descriptor)
            if not matches:
                raise OSError('cache debug file lacks matching DWARF information')
        except OSError:
            missing_debug.add(buildid)
    return {'missing': sorted(missing), 'missing_debug': sorted(missing_debug), 'unverified': unverified}


def cache_fd(fd, key, cache_home, perf, uid, gid, max_bytes=MAX_IMAGE_BYTES, expected_buildid=None):
    try:
        before = os.fstat(fd)
        if (before.st_dev, before.st_ino) != key or not stat.S_ISREG(before.st_mode):
            return False
        if not 0 < before.st_size <= min(MAX_IMAGE_BYTES, max_bytes) or os.pread(fd, 4, 0) != b'\x7fELF':
            return False
        buildid = elf_buildid(fd)
        if expected_buildid is not None and buildid != expected_buildid:
            return False
        # Copy to an exclusive, user-owned staging inode before publishing any
        # cache entry. A changing source must never leave a trusted cache file.
        stage, stage_path = tempfile.mkstemp(prefix='.asb-perf-image-', dir=cache_home)
        try:
            if os.geteuid() == 0:
                os.fchown(stage, uid, gid)
            offset = 0
            while offset < before.st_size:
                data = os.pread(fd, min(1024 * 1024, before.st_size - offset), offset)
                if not data:
                    return False
                view = memoryview(data)
                while view:
                    written = os.write(stage, view)
                    if written <= 0:
                        return False
                    view = view[written:]
                offset += len(data)
            after = os.fstat(fd)
            if (before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (after.st_size, after.st_mtime_ns, after.st_ctime_ns):
                return False
            os.fsync(stage)

            def drop_privileges():
                if os.geteuid() == 0:
                    os.setgroups([])
                    os.setgid(gid)
                    os.setuid(uid)
            env = {'HOME': cache_home, 'PATH': '/usr/bin:/bin', 'LC_ALL': 'C', 'DEBUGINFOD_URLS': ''}
            completed = subprocess.run(
                [perf, 'buildid-cache', '-a', f'/proc/self/fd/{stage}'],
                pass_fds=(stage,), env=env, stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                timeout=30, check=False, preexec_fn=drop_privileges,
            )
            return (before.st_size, buildid) if completed.returncode == 0 else False
        finally:
            os.close(stage)
            os.unlink(stage_path)
    except (OSError, subprocess.SubprocessError):
        return False


def cache_mapped_file(pid, start, path, key, cache_home, perf, uid, gid, max_bytes=MAX_IMAGE_BYTES, expected_buildid=None):
    current = identity(pid)
    if current is None or current[1] != start:
        return False
    try:
        fd = os.open(f'/proc/{pid}/root{path}', os.O_RDONLY | os.O_CLOEXEC)
    except OSError:
        return False
    try:
        current = identity(pid)
        if current is None or current[1] != start:
            return False
        return cache_fd(fd, key, cache_home, perf, uid, gid, max_bytes, expected_buildid)
    finally:
        os.close(fd)


def cache_pinned_file(root_fd, path, expected_buildid, cache_home, perf, uid, gid, max_bytes):
    if not path.startswith('/') or '..' in path.split('/') or len(path) > 4096:
        return False
    try:
        fd = os.open(path.lstrip('/'), os.O_RDONLY | os.O_CLOEXEC, dir_fd=root_fd)
    except OSError:
        return False
    try:
        current = os.fstat(fd)
        return cache_fd(fd, (current.st_dev, current.st_ino), cache_home, perf, uid, gid, max_bytes, expected_buildid)
    finally:
        os.close(fd)


def publish_host_debug(buildid, symbol_root, cache_home, uid, gid, max_bytes):
    """Attach an independently verified project debug file to a cached ELF."""
    if not re.fullmatch(r'(?:[0-9a-f]{2}){4,64}', buildid):
        return None
    source = pathlib.Path(symbol_root) / '.build-id' / buildid[:2] / (buildid[2:] + '.debug')
    root = os.path.realpath(os.path.join(cache_home, '.debug'))
    cache = os.path.realpath(os.path.join(root, '.build-id', buildid[:2], buildid[2:]))
    try:
        if os.path.commonpath((root, cache)) != root:
            return None
        image = os.open(os.path.join(cache, 'elf'), os.O_RDONLY | os.O_NOFOLLOW)
        try:
            if elf_buildid(image) != buildid:
                return None
        finally:
            os.close(image)
        fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW)
        try:
            before = os.fstat(fd)
            if (not stat.S_ISREG(before.st_mode) or not 0 < before.st_size <= max_bytes
                    or elf_buildid(fd) != buildid or not elf_has_debug_info(fd)):
                return None
            stage, name = tempfile.mkstemp(prefix='.asb-debug-', dir=cache)
            try:
                if os.geteuid() == 0:
                    os.fchown(stage, uid, gid)
                offset = 0
                while offset < before.st_size:
                    data = os.pread(fd, min(1024 * 1024, before.st_size - offset), offset)
                    if not data:
                        return None
                    view = memoryview(data)
                    while view:
                        count = os.write(stage, view)
                        if count <= 0:
                            return None
                        view = view[count:]
                    offset += len(data)
                after = os.fstat(fd)
                if (before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (after.st_size, after.st_mtime_ns, after.st_ctime_ns):
                    return None
                os.fsync(stage)
                try:
                    os.link(name, os.path.join(cache, 'debug'))
                except FileExistsError:
                    existing = os.open(os.path.join(cache, 'debug'), os.O_RDONLY | os.O_NOFOLLOW)
                    try:
                        if elf_buildid(existing) != buildid or not elf_has_debug_info(existing):
                            return None
                    finally:
                        os.close(existing)
                    return 0
                return before.st_size
            finally:
                os.close(stage)
                os.unlink(name)
        finally:
            os.close(fd)
    except (OSError, ValueError):
        return None


def settle_recorded_images(root_fd, data, cache_home, perf, symbol_root, uid, gid, remaining, slots=MAX_IMAGES):
    """Recover recorded images still present in the pinned guest filesystem."""
    initial_remaining = remaining
    def drop_privileges():
        if os.geteuid() == 0:
            os.setgroups([])
            os.setgid(gid)
            os.setuid(uid)
    environment = {'HOME': cache_home, 'PATH': '/usr/bin:/bin', 'LC_ALL': 'C', 'DEBUGINFOD_URLS': ''}
    with tempfile.TemporaryFile(dir=cache_home) as output:
        if os.geteuid() == 0:
            os.fchown(output.fileno(), uid, gid)
        try:
            result = subprocess.run([perf, 'buildid-list', '-i', data], stdin=subprocess.DEVNULL,
                                    stdout=output, stderr=subprocess.DEVNULL, timeout=15,
                                    env=environment, check=False, preexec_fn=drop_privileges)
            length = os.fstat(output.fileno()).st_size
            if result.returncode != 0 or not 0 < length <= 4 * 1024 * 1024:
                return None
            output.seek(0)
            listing = output.read().decode('utf-8', errors='replace')
            initial = verify_buildids(listing, cache_home)
        except (OSError, subprocess.SubprocessError, ValueError):
            return None
    wanted = set(initial['missing'])
    recovered = 0
    for line in listing.splitlines():
        fields = line.split(None, 1)
        if len(fields) != 2 or fields[0] not in wanted or not fields[1].startswith('/'):
            continue
        if remaining <= 0 or recovered >= slots:
            break
        copied = cache_pinned_file(root_fd, fields[1], fields[0], cache_home, perf, uid, gid, remaining)
        if copied:
            used, buildid = copied
            remaining -= used
            recovered += 1
            wanted.discard(buildid)
            if symbol_root and remaining > 0:
                retained = publish_host_debug(buildid, symbol_root, cache_home, uid, gid, remaining)
                if retained is not None:
                    remaining -= retained
    final = verify_buildids(listing, cache_home)
    return {'recovered_recorded_images': recovered, 'recovered_bytes': initial_remaining - remaining,
            'missing_recorded_images': len(final['missing']),
            'missing_debug_images': len(final['missing_debug']), 'unverified_entries': final['unverified']}


def main():
    if sys.argv[1:2] == ['--verify']:
        try:
            listing = sys.stdin.read(4 * 1024 * 1024 + 1)
            print(json.dumps(verify_buildids(listing, sys.argv[2]), sort_keys=True))
            return 0
        except (IndexError, ValueError, OSError):
            return 2
    root, start, uid, gid = map(int, sys.argv[1:5])
    cache_home, perf = sys.argv[5:7]
    symbol_root = sys.argv[7] if len(sys.argv) > 7 else None
    control = sys.argv[8] if len(sys.argv) > 8 else None
    recorder = (int(sys.argv[9]), int(sys.argv[10])) if len(sys.argv) > 11 else None
    data = sys.argv[11] if recorder else None
    root_fd = None
    seen = set()
    members = [(root, start)]
    size = 0
    skipped = 0
    stopping = False

    def stop(_signum, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    if control:
        if not control.startswith('fifo:') or control.count(',') != 1:
            return 2
        request, ack = control[5:].split(',', 1)
        current = identity(root)
        if current is None or current[1] != start:
            return 2
        if recorder:
            current_recorder = identity(recorder[0])
            if current_recorder is None or current_recorder[1] != recorder[1]:
                return 2
            try:
                root_fd = os.open(f'/proc/{root}/root', os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC)
            except OSError:
                return 2
            after = identity(root)
            if after is None or after[1] != start:
                os.close(root_fd)
                return 2
        with open(request, 'rb', buffering=0) as reader, open(ack, 'wb', buffering=0) as writer:
            if reader.read(7) != b'enable\n':
                return 2
            writer.write(b'ack\n')
    while not stopping:
        members = tree(members)
        if members is None:
            while recorder and not stopping:
                time.sleep(0.1)
            break
        for pid, generation in members:
            for path, key in mappings(pid):
                if key in seen or len(seen) >= MAX_IMAGES or size >= MAX_TOTAL_BYTES:
                    if key not in seen:
                        skipped += 1
                    continue
                try:
                    nbytes = os.stat(f'/proc/{pid}/root{path}').st_size
                except OSError:
                    skipped += 1
                    continue
                if not 0 < nbytes <= MAX_IMAGE_BYTES or size + nbytes > MAX_TOTAL_BYTES:
                    skipped += 1
                    continue
                copied = cache_mapped_file(pid, generation, path, key, cache_home, perf, uid, gid, MAX_TOTAL_BYTES - size)
                if copied:
                    seen.add(key)
                    used, buildid = copied
                    size += used
                    if buildid:
                        if symbol_root:
                            retained = publish_host_debug(buildid, symbol_root, cache_home, uid, gid, MAX_TOTAL_BYTES - size)
                            if retained is not None:
                                size += retained
                        debug = f'/usr/lib/debug/.build-id/{buildid[:2]}/{buildid[2:]}.debug'
                        try:
                            info = os.stat(f'/proc/{pid}/root{debug}')
                        except OSError:
                            continue
                        debug_key = (info.st_dev, info.st_ino)
                        if debug_key in seen or size >= MAX_TOTAL_BYTES:
                            continue
                        matched = cache_mapped_file(pid, generation, debug, debug_key, cache_home, perf, uid, gid, MAX_TOTAL_BYTES - size, buildid)
                        if matched:
                            seen.add(debug_key)
                            size += matched[0]
                else:
                    skipped += 1
        time.sleep(0.5)
    recorded = None
    if recorder and root_fd is not None:
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            current = identity(recorder[0])
            if current is None or current[1] != recorder[1]:
                recorded = settle_recorded_images(root_fd, data, cache_home, perf, symbol_root, uid, gid, MAX_TOTAL_BYTES - size, MAX_IMAGES - len(seen))
                if recorded is not None:
                    size += recorded['recovered_bytes']
                break
            time.sleep(0.05)
        os.close(root_fd)
    print(json.dumps({'schema_version': 1, 'scope': 'sampled-live-mappings-and-recorded-build-ids',
                      'retained_images': len(seen) + (recorded['recovered_recorded_images'] if recorded else 0), 'retained_bytes': size,
                      'skipped_observations': skipped, 'interval_ms': 500,
                      'recorded_images': recorded,
                      'limitation': 'deleted or otherwise inaccessible images cannot be recovered'}, sort_keys=True), flush=True)
    return 2 if recorder and recorded is None else 0


if __name__ == '__main__':
    sys.exit(main())
