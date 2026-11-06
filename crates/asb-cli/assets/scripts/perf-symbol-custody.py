"""Collect mapped ELF files while a benchmark process tree is alive.

Private ASB helper: callers must supply an already-created user-owned cache
home and supervise this process until its verified root exits. It never executes
mapped files. The cache consumer runs only after dropping elevated privileges.
"""
import os
import json
import pathlib
import re
import selectors
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
MAX_GUEST_ROWS_BYTES = 40 * 1024 * 1024


class GuestCaptureSchedule:
    """Bound retry work without starving other live mappings."""
    def __init__(self, limit=MAX_IMAGES):
        self.limit = limit
        self.settled = set()
        self.pending = {}

    def eligibility(self, observation, now):
        if observation in self.settled:
            return 'settled'
        retry = self.pending.get(observation)
        if retry is not None:
            return 'ready' if now >= retry[1] else 'waiting'
        return 'full' if len(self.settled) + len(self.pending) >= self.limit else 'ready'

    def success(self, observation):
        self.pending.pop(observation, None)
        self.settled.add(observation)

    def failure(self, observation, now):
        attempts = self.pending.get(observation, (0, 0))[0] + 1
        if attempts >= 3:
            self.success(observation)
        else:
            self.pending[observation] = (attempts, now + 1.0)


def namespace_pid(pid, start):
    current = identity(pid)
    if current is None or current[1] != start:
        return None
    try:
        status = pathlib.Path(f'/proc/{pid}/status').read_text()
        values = next(line for line in status.splitlines() if line.startswith('NSpid:')).split()[1:]
        numbers = [int(value) for value in values]
        current = identity(pid)
        if len(numbers) < 2 or numbers[0] != pid or numbers[-1] <= 1 or not current or current[1] != start:
            return None
        return numbers[-1]
    except (OSError, ValueError, StopIteration):
        return None


def guest_command(container, script, arguments, output, limit, timeout):
    """Run a bounded guest helper, discarding partial output on failure."""
    if not re.fullmatch(r'[0-9a-f]{64}', container) or limit <= 0:
        return False
    process = None
    try:
        with open(script, 'rb') as source:
            process = subprocess.Popen(
                ['docker', 'exec', '-i', '--user', '0', '--workdir', '/', container,
                 'python3', '-I', '-', *arguments],
                stdin=source, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            )
            deadline = time.monotonic() + timeout
            count = 0
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        return False
                    if not selector.select(remaining):
                        return False
                    block = os.read(process.stdout.fileno(), min(1024 * 1024, limit + 1 - count))
                    if not block:
                        break
                    count += len(block)
                    if count > limit:
                        return False
                    output.write(block)
            return process.wait(timeout=max(0.01, deadline - time.monotonic())) == 0
    except (OSError, subprocess.SubprocessError):
        return False
    finally:
        if process is not None:
            if process.poll() is None:
                process.kill()
            process.wait()
            if process.stdout:
                process.stdout.close()


def guest_scan(container, script, roots, cache_home):
    if len(roots) > 1024:
        return None
    try:
        with tempfile.TemporaryFile(dir=cache_home) as output:
            if not guest_command(container, script, ['scan-roots', json.dumps(roots)],
                                 output, MAX_GUEST_ROWS_BYTES, 5):
                return None
            output.seek(0)
            result = json.load(output)
        if (not isinstance(result, dict) or result.get('schema_version') != 1
                or not isinstance(result.get('members'), list)
                or not isinstance(result.get('mappings'), list)
                or type(result.get('inaccessible')) is not int or result['inaccessible'] < 0
                or len(result['members']) > 1024 or len(result['mappings']) > 8192):
            return None
        return result
    except (OSError, ValueError, TypeError):
        return None


def capture_guest_noid(container, script, row, cache_home, max_bytes):
    """Stage a live mapping and return it only if it has no build ID."""
    try:
        pid, start = row['pid'], row['start']
        path, major, minor, inode = row['path'], row['major'], row['minor'], row['inode']
        stamp = (row['size'], row['mtime_ns'], row['ctime_ns'])
        if (any(type(value) is not int or value < 0 for value in (pid, start, major, minor, inode))
                or any(type(value) is not int for value in stamp) or stamp[0] <= 0
                or pid <= 1 or start <= 0 or inode <= 0 or not isinstance(path, str)
                or not path.startswith('/') or any(part in ('', '.', '..') for part in path[1:].split('/'))
                or len(os.fsencode(path)) > 4096 or max_bytes <= 0):
            return None
        descriptor, name = tempfile.mkstemp(prefix='.asb-guest-image-', dir=cache_home)
        try:
            with os.fdopen(descriptor, 'w+b') as output:
                arguments = ['copy', str(pid), str(start), path, str(major), str(minor), str(inode),
                             *(str(value) for value in stamp)]
                limit = min(MAX_IMAGE_BYTES, max_bytes)
                if not guest_command(container, script, arguments, output, limit, 8):
                    return None
                output.flush()
                info = os.fstat(output.fileno())
                if not 0 < info.st_size <= limit or os.pread(output.fileno(), 4, 0) != b'\x7fELF':
                    return None
                buildid = elf_buildid(output.fileno())
                if buildid is not None:
                    return (0, False)
                root = pathlib.Path(cache_home) / 'symfs'
                destination = root.joinpath(*path[1:].split('/'))
                destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                if destination.exists() or destination.is_symlink():
                    return None
                os.fsync(output.fileno())
                os.link(name, destination)
                return (info.st_size, True)
        finally:
            os.unlink(name)
    except (KeyError, TypeError, OSError, ValueError):
        return None


def prepare_symfs(listing, cache_home, ambiguous):
    """Mirror known build IDs and retain available no-build-ID images."""
    if ambiguous:
        return False
    root = pathlib.Path(cache_home) / 'symfs'
    buildroot = pathlib.Path(cache_home) / '.debug' / '.build-id'
    if not root.is_dir():
        return False
    paths = {}
    missing_noid = 0
    missing_buildid = 0
    try:
        for line in listing.splitlines():
            fields = line.split(None, 1)
            if not fields:
                continue
            if len(fields) == 1:
                name = fields[0]
                if name.startswith('/tmp/perf-vdso'):
                    continue
                buildid = None
            else:
                buildid, name = fields
                if name.startswith('[') and name.endswith(']'):
                    continue
                if not re.fullmatch(r'(?:[0-9a-f]{2}){4,64}', buildid):
                    return False
            if (not name.startswith('/') or any(part in ('', '.', '..') for part in name[1:].split('/'))
                    or len(os.fsencode(name)) > 4096):
                return False
            if name in paths and paths[name] != buildid:
                return False
            paths[name] = buildid
            target = root.joinpath(*name[1:].split('/'))
            if buildid is None:
                if not target.is_file() or target.is_symlink():
                    missing_noid += 1
                continue
            source = buildroot / buildid[:2] / buildid[2:] / 'elf'
            try:
                image = source.resolve(strict=True)
            except FileNotFoundError:
                # A prior no-build-ID snapshot at the same path must never be
                # substituted for an unavailable recorded build ID.
                if target.exists() or target.is_symlink():
                    return False
                missing_buildid += 1
                continue
            with image.open('rb') as source:
                if elf_buildid(source.fileno()) != buildid:
                    return False
            target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            if target.exists() or target.is_symlink():
                if target.is_symlink() or not os.path.samefile(image, target):
                    return False
            else:
                os.link(image, target)
            debug = (buildroot / buildid[:2] / buildid[2:] / 'debug')
            if debug.is_file():
                target_debug = root / 'usr/lib/debug/.build-id' / buildid[:2] / (buildid[2:] + '.debug')
                target_debug.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                if not target_debug.exists():
                    os.link(debug.resolve(strict=True), target_debug)
        marker = pathlib.Path(cache_home) / 'symfs-ready'
        with marker.open('x') as output:
            json.dump({'schema_version': 1, 'missing_no_build_id_images': missing_noid,
                       'missing_build_id_images': missing_buildid}, output)
        return True
    except (OSError, ValueError, TypeError):
        return False
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


def container_for(pid, start):
    """Identify the local Docker cgroup without trusting a caller-supplied name."""
    current = identity(pid)
    if current is None or current[1] != start:
        return None
    try:
        lines = pathlib.Path(f'/proc/{pid}/cgroup').read_text().splitlines()
        ids = set()
        for line in lines:
            components = line.split(':', 2)[-1].split('/')
            for index, component in enumerate(components):
                match = re.fullmatch(r'docker-([0-9a-f]{64})\.scope', component)
                if match is None and index and components[index - 1] == 'docker':
                    match = re.fullmatch(r'([0-9a-f]{64})', component)
                if match:
                    ids.add(match.group(1))
        current = identity(pid)
        return next(iter(ids)) if len(ids) == 1 and current and current[1] == start else None
    except OSError:
        return None


def cache_container_file(container, path, expected_buildid, cache_home, perf, uid, gid, max_bytes, timeout=20):
    """Copy one bounded image via the owned container, then verify its build ID."""
    if (not re.fullmatch(r'[0-9a-f]{64}', container) or not path.startswith('/')
            or '..' in path.split('/') or len(path) > 4096 or max_bytes <= 0):
        return False
    limit = min(MAX_IMAGE_BYTES, max_bytes)
    try:
        with tempfile.TemporaryFile(dir=cache_home) as image:
            result = subprocess.run(
                ['docker', 'exec', '--user', '0', '--workdir', '/', container,
                 'head', '-c', str(limit + 1), '--', path],
                stdin=subprocess.DEVNULL, stdout=image, stderr=subprocess.DEVNULL,
                timeout=timeout, check=False,
            )
            info = os.fstat(image.fileno())
            if result.returncode != 0 or not 0 < info.st_size <= limit:
                return False
            return cache_fd(image.fileno(), (info.st_dev, info.st_ino), cache_home,
                            perf, uid, gid, limit, expected_buildid)
    except (OSError, subprocess.SubprocessError):
        return False


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


def settle_recorded_images(root_fd, data, cache_home, perf, symbol_root, uid, gid, remaining, slots=MAX_IMAGES, container=None, ambiguous=()):
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
    export_deadline = time.monotonic() + 20
    for line in listing.splitlines():
        fields = line.split(None, 1)
        if len(fields) != 2 or fields[0] not in wanted or not fields[1].startswith('/'):
            continue
        if remaining <= 0 or recovered >= slots or time.monotonic() >= export_deadline:
            break
        copied = (cache_pinned_file(root_fd, fields[1], fields[0], cache_home, perf, uid, gid, remaining)
                  if root_fd is not None else
                  cache_container_file(container, fields[1], fields[0], cache_home, perf, uid, gid, remaining,
                                       max(0.01, export_deadline - time.monotonic())))
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
    symfs_ready = prepare_symfs(listing, cache_home, ambiguous)
    missing_noid = None
    missing_buildid = None
    if symfs_ready:
        try:
            with open(pathlib.Path(cache_home) / 'symfs-ready', encoding='utf-8') as marker:
                manifest = json.load(marker)
                missing_noid = manifest['missing_no_build_id_images']
                missing_buildid = manifest['missing_build_id_images']
        except (OSError, ValueError, KeyError):
            symfs_ready = False
            (pathlib.Path(cache_home) / 'symfs-ready').unlink(missing_ok=True)
    return {'recovered_recorded_images': recovered, 'recovered_bytes': initial_remaining - remaining,
            'missing_recorded_images': len(final['missing']),
            'missing_debug_images': len(final['missing_debug']), 'unverified_entries': final['unverified'],
            'symfs_ready': symfs_ready, 'symfs_missing_no_build_id_images': missing_noid,
            'symfs_missing_build_id_images': missing_buildid}


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
    guest_script = sys.argv[12] if len(sys.argv) > 12 else None
    root_fd = None
    container = None
    seen = set()
    members = [(root, start)]
    size = 0
    skipped = 0
    guest_roots = []
    guest_schedule = GuestCaptureSchedule()
    guest_paths = {}
    guest_ambiguous = set()
    guest_captured_paths = set()
    guest_noid = 0
    guest_failures = 0
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
            except OSError as error:
                container = container_for(root, start)
                if container is None:
                    print('Cannot access the benchmark process root for perf symbol capture: '
                          f'{error}. No verified local Docker container was found.', file=sys.stderr)
                    return 2
                guest_pid = namespace_pid(root, start)
                if guest_script and guest_pid:
                    guest_roots = [(guest_pid, start)]
            after = identity(root)
            if after is None or after[1] != start:
                if root_fd is not None:
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
        if container and guest_script and guest_roots:
            observed = guest_scan(container, guest_script, guest_roots, cache_home)
            if observed is None:
                guest_failures += 1
            else:
                guest_roots = observed['members']
                guest_failures += observed.get('inaccessible', 0)
                for row in observed['mappings']:
                    if not isinstance(row, dict):
                        guest_failures += 1
                        continue
                    path = row.get('path')
                    key = (row.get('major'), row.get('minor'), row.get('inode'),
                           row.get('size'), row.get('mtime_ns'), row.get('ctime_ns'))
                    if not isinstance(path, str) or not all(type(value) is int for value in key):
                        guest_failures += 1
                        continue
                    if path not in guest_paths and len(guest_paths) >= MAX_IMAGES:
                        guest_failures += 1
                        continue
                    previous = guest_paths.setdefault(path, key)
                    if previous != key:
                        guest_ambiguous.add(path)
                        if path in guest_captured_paths:
                            guest_captured_paths.remove(path)
                            guest_noid -= 1
                        if path.startswith('/') and all(part not in ('', '.', '..') for part in path[1:].split('/')):
                            try:
                                (pathlib.Path(cache_home) / 'symfs').joinpath(*path[1:].split('/')).unlink(missing_ok=True)
                            except OSError:
                                guest_failures += 1
                        continue
                    observation = (path, key)
                    if path in guest_ambiguous:
                        continue
                    eligibility = guest_schedule.eligibility(observation, time.monotonic())
                    if eligibility == 'full':
                        guest_failures += 1
                        continue
                    if eligibility != 'ready':
                        continue
                    if row.get('has_build_id') is True:
                        # The settled perf listing recovers these images via
                        # their build IDs; prioritize no-ID files while live.
                        guest_schedule.success(observation)
                        continue
                    copied = capture_guest_noid(container, guest_script, row, cache_home, MAX_TOTAL_BYTES - size)
                    if copied is None:
                        guest_failures += 1
                        guest_schedule.failure(observation, time.monotonic())
                    else:
                        guest_schedule.success(observation)
                        size += copied[0]
                        guest_noid += int(copied[1])
                        if copied[1]:
                            guest_captured_paths.add(path)
                    # Bound collector work per scan; short-lived mappings can
                    # be missed and are explicitly not claimed as complete.
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
    if recorder and (root_fd is not None or container is not None):
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            current = identity(recorder[0])
            if current is None or current[1] != recorder[1]:
                recorded = settle_recorded_images(root_fd, data, cache_home, perf, symbol_root, uid, gid, MAX_TOTAL_BYTES - size, MAX_IMAGES - len(seen), container, guest_ambiguous)
                if recorded is not None:
                    size += recorded['recovered_bytes']
                break
            time.sleep(0.05)
        if root_fd is not None:
            os.close(root_fd)
    print(json.dumps({'schema_version': 1, 'scope': 'sampled-live-mappings-and-recorded-build-ids',
                      'retained_images': len(seen) + (recorded['recovered_recorded_images'] if recorded else 0), 'retained_bytes': size,
                      'skipped_observations': skipped, 'interval_ms': 500,
                      'recorded_images': recorded,
                      'guest_no_build_id_images': guest_noid,
                      'guest_ambiguous_paths': len(guest_ambiguous),
                      'guest_capture_failures': guest_failures,
                      'limitation': 'deleted or otherwise inaccessible images cannot be recovered'}, sort_keys=True), flush=True)
    return 2 if recorder and recorded is None else 0


if __name__ == '__main__':
    sys.exit(main())
