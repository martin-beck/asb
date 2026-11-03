"""Local feasibility probe only; not an ASB replay backend."""
import errno, hashlib, json, os, re, socket, stat, sys, threading, time
from collections import OrderedDict
from pathlib import Path
from fuse import FUSE, FuseOSError, Operations, fuse_get_context

backing, mount, evidence = map(Path, sys.argv[1:4])


class PayloadStore:
    """Reuse recently seen exact payloads without changing event ordering."""
    def __init__(self, path, capacity=32768):
        self.file = open(path, 'a+b', buffering=0)
        self.recent = OrderedDict()
        self.capacity = capacity

    def append(self, data):
        key = (len(data), hashlib.sha256(data).digest())
        offset = self.recent.get(key)
        if offset is not None:
            self.file.seek(offset)
            if self.file.read(len(data)) == data:
                self.recent.move_to_end(key)
                return offset
        self.file.seek(0, os.SEEK_END)
        offset = self.file.tell()
        if self.file.write(data) != len(data):
            raise RuntimeError('short payload write')
        if self.capacity:
            self.recent[key] = offset
            self.recent.move_to_end(key)
            if len(self.recent) > self.capacity:
                self.recent.popitem(last=False)
        return offset

# Bounded positive cache: a process start tick prevents stale PID reuse from
# inheriting a creator thread. Missing observations are never guessed.
_creator_cache = OrderedDict()
_creator_lock = threading.Lock()
_CREATOR_CACHE_LIMIT = 8192
_MAX_PARENT_THREADS = 4096


def proc_status(pid):
    fields = {}
    for line in Path(f'/proc/{pid}/status').read_text().splitlines():
        key, separator, value = line.partition(':')
        if separator and key in ('Tgid', 'PPid'):
            fields[key] = int(value.strip())
    parts = Path(f'/proc/{pid}/stat').read_text().rpartition(')')[2].split()
    return fields['Tgid'], fields['PPid'], int(parts[19])


def creator_thread(child, parent, child_start):
    """Find the live parent thread that created a child process, if observable."""
    key = (child, parent, child_start)
    with _creator_lock:
        cached = _creator_cache.get(key)
        if cached is not None:
            _creator_cache.move_to_end(key)
            return dict(cached)
    try:
        threads = list(Path(f'/proc/{parent}/task').iterdir())
        if len(threads) > _MAX_PARENT_THREADS:
            return None
        for task in threads:
            if not task.name.isdigit():
                continue
            try:
                before_group, _, before_start = proc_status(int(task.name))
                if str(child) not in (task / 'children').read_text().split():
                    continue
                group, _, thread_start = proc_status(int(task.name))
                actual_child, actual_parent, actual_start = proc_status(child)
                if (before_group != parent or group != parent or before_start != thread_start
                        or actual_child != child or actual_parent != parent
                        or actual_start != child_start):
                    return None
                result = {'tid': int(task.name), 'start_ticks': thread_start}
                with _creator_lock:
                    _creator_cache[key] = result
                    _creator_cache.move_to_end(key)
                    if len(_creator_cache) > _CREATOR_CACHE_LIMIT:
                        _creator_cache.popitem(last=False)
                return dict(result)
            except (OSError, KeyError, ValueError, IndexError):
                continue
    except OSError:
        pass
    return None


def marker_origin(pid, tid):
    """Attach generation evidence without assuming the marker's thread lives."""
    origin = {'pid': pid, 'tid': tid, 'process_start_ticks': None, 'thread_start_ticks': None}
    try:
        group, _, thread_start = proc_status(tid)
        leader, _, process_start = proc_status(pid)
        if group == pid and leader == pid:
            origin.update(process_start_ticks=process_start, thread_start_ticks=thread_start)
    except (OSError, KeyError, ValueError, IndexError):
        pass
    return origin


def caller_identity(tid):
    """Best-effort bounded process and creating-thread ancestry."""
    try:
        tgid, ppid, started = proc_status(tid)
        ancestry = []
        current = tgid
        root = int(os.environ['ASB_PROBE_ROOT_TGID'])
        thread_complete = True
        for _ in range(64):
            group, parent, start = proc_status(current)
            entry = {'tgid': group, 'start_ticks': start}
            if group != root and parent > 0 and parent != group:
                creator = creator_thread(group, parent, start)
                entry['creator_thread'] = creator
                thread_complete &= creator is not None
            ancestry.append(entry)
            if group == root or parent <= 0 or parent == group:
                break
            current = parent
        complete = bool(ancestry and ancestry[-1]['tgid'] == root)
        return {'tid': tid, 'tgid': tgid, 'ppid': ppid,
                'start_ticks': started, 'ancestry': ancestry,
                'ancestry_complete': complete,
                'thread_ancestry_complete': complete and thread_complete}
    except (OSError, KeyError, ValueError, IndexError):
        return {'tid': tid, 'tgid': None, 'ppid': None, 'start_ticks': None,
                'ancestry': [], 'ancestry_complete': False, 'thread_ancestry_complete': False}

class Proxy(Operations):
    use_ns = True
    def __init__(self):
        self.record_lock = threading.RLock()
        self.sequence = 0
        self.handle_lock = threading.Lock()
        self.next_handle = 0
        self.handles = {}
        self.inode_locks = {}
        self.log = open(evidence / 'events.jsonl', 'a', encoding='utf-8', buffering=1)
        self.payload = PayloadStore(evidence / 'payload.bin')
        self.metadata_playback = None
        metadata_source = os.environ.get('ASB_PROBE_METADATA_REPLAY')
        if metadata_source:
            from asb_fs_metadata_windows import ObservationWindow
            source = Path(metadata_source)
            def recorded_events():
                with (source / 'events.jsonl').open(encoding='utf-8') as stream:
                    for line in stream:
                        if not line.endswith('\n'):
                            raise ValueError('recorded filesystem event stream is truncated')
                        event = json.loads(line)
                        yield event
                        if event.get('op') == 'marker':
                            return
            self.metadata_playback = ObservationWindow(recorded_events(), source / 'payload.bin')
        self.attribution = None
        if os.environ.get('ASB_PROBE_COMPACT_TOOL_READS') == '1':
            from asb_fs_attribution import Attribution
            self.attribution = Attribution()
        control_fd = os.environ.get('ASB_PROBE_CONTROL_FD')
        if control_fd is not None:
            control = socket.socket(fileno=int(control_fd))
            threading.Thread(target=self.control_loop, args=(control,), daemon=True).start()
        control_path = os.environ.get('ASB_PROBE_CONTROL_SOCKET')
        if control_path:
            self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.listener.bind(control_path)
            os.chmod(control_path, 0o666)
            self.listener.listen(32)
            threading.Thread(target=self.accept_control, daemon=True).start()
    def accept_control(self):
        while True:
            try:
                control, _ = self.listener.accept()
            except OSError:
                return
            threading.Thread(target=self.control_loop, args=(control,), daemon=True).start()
    def control_loop(self, control):
        with control, control.makefile('rwb', buffering=0) as stream:
            for line in stream:
                text = line.decode('ascii').strip()
                match = re.fullmatch(r'(tool-(?:begin|end):[A-Za-z0-9_-]{1,80})(?::([0-9]+):([0-9]+))?', text)
                if match is None:
                    raise RuntimeError('invalid fixture marker')
                marker, pid, tid = match.groups()
                origin = marker_origin(int(pid), int(tid)) if pid is not None else None
                sequence = self.record('marker', None, marker=marker, origin=origin, is_marker=True)
                stream.write(f'{sequence}\n'.encode('ascii'))
    def path(self, path): return backing / path.lstrip('/')
    def register(self, fd):
        with self.handle_lock:
            self.next_handle += 1
            self.handles[fd] = self.next_handle
            return self.next_handle
    def handle(self, fi):
        with self.handle_lock: return self.handles[fi.fh]
    def lock_for_stat(self, st):
        with self.handle_lock: return self.inode_locks.setdefault((st.st_dev, st.st_ino), threading.RLock())
    def inode_lock(self, fi):
        return self.lock_for_stat(os.fstat(fi.fh))
    def record(self, op, path, data=None, is_marker=False, **detail):
        with self.record_lock:
            playback_boundary = None
            if self.metadata_playback is not None and not op.startswith('observe-'):
                playback_boundary = self.metadata_playback.advance()
                if playback_boundary is None or playback_boundary.get('op') != op or playback_boundary.get('path') != path:
                    raise RuntimeError('recorded direct filesystem operation differs')
            self.sequence += 1
            event = {'sequence': self.sequence, 'op': op, 'path': path, 'caller': None if is_marker or os.environ.get('ASB_PROBE_BENCHMARK_NO_IDENTITY') == '1' else caller_identity(fuse_get_context()[2]), **detail}
            scope = None
            if self.attribution is not None:
                try:
                    scope = self.attribution.observe(event)
                except (TypeError, ValueError, KeyError):
                    # Fall back to complete payloads if attribution fails; do
                    # not alter the caller's filesystem operation.
                    self.attribution = None
            if data is not None:
                if op == 'read' and scope is not None and scope.get('kind') == 'tool':
                    event.update(payload_sha256=hashlib.sha256(data).hexdigest(), bytes=len(data))
                else:
                    position = self.payload.append(data)
                    event.update(object='payload.bin', payload_offset=position, bytes=len(data))
            self.log.write(json.dumps(event, separators=(',', ':')) + '\n')
            if playback_boundary is not None:
                self.metadata_playback.operation_applied()
            return self.sequence
    def observe_metadata(self, op, path, operation, **detail):
        """Return recorded results in the opt-in startup replay prototype."""
        if self.metadata_playback is None and os.environ.get('ASB_PROBE_METADATA_CAPTURE') != '1':
            return operation()
        with self.record_lock:
            try:
                result = (self.metadata_playback.observe('observe-' + op, path, **detail)
                          if self.metadata_playback is not None else operation())
            except OSError as error:
                self.record('observe-' + op, path, error=error.errno, **detail)
                raise
            except RuntimeError:
                if self.metadata_playback is not None:
                    self.record('observe-unmatched', path, requested_op=op, **detail)
                raise
            if isinstance(result, bytes):
                self.record('observe-' + op, path, result, **detail)
            else:
                self.record('observe-' + op, path, result=result, **detail)
            return result

    def getattr(self, path, fh=None):
        def lookup():
            st = os.lstat(self.path(path))
            attrs = {key: getattr(st, key) for key in ('st_gid','st_ino','st_mode','st_nlink','st_size','st_uid','st_blocks','st_blksize')}
            attrs.update(st_atime=st.st_atime_ns, st_mtime=st.st_mtime_ns, st_ctime=st.st_ctime_ns)
            return attrs
        try:
            return self.observe_metadata('getattr', path, lookup)
        except OSError as error:
            raise FuseOSError(error.errno)
    def readdir(self, path, fh):
        return self.observe_metadata('readdir', path, lambda: ['.', '..', *os.listdir(self.path(path))])
    def open(self, path, fi):
        try:
            fi.fh = os.open(self.path(path), fi.flags)
            handle = self.register(fi.fh)
            with self.inode_lock(fi):
                st = os.fstat(fi.fh)
                self.record('open', path, handle=handle, flags=fi.flags,
                            initial_size=st.st_size, initial_mode=st.st_mode,
                            initial_nlink=st.st_nlink)
            return 0
        except OSError as error: raise FuseOSError(error.errno)
    def create(self, path, mode, fi=None):
        try:
            fd = os.open(self.path(path), fi.flags | os.O_CREAT, mode)
            if os.geteuid() == 0:
                uid, gid, _ = fuse_get_context()
                try:
                    os.fchown(fd, uid, gid)
                except OSError:
                    os.close(fd)
                    raise
            handle = self.register(fd)
            self.record('create', path, mode=mode, flags=fi.flags, handle=handle)
            fi.fh = fd
            return 0
        except OSError as error: raise FuseOSError(error.errno)
    def read(self, path, size, offset, fh):
        with self.inode_lock(fh):
            data = os.pread(fh.fh, size, offset)
            self.record('read', path, data, offset=offset, handle=self.handle(fh), requested_size=size)
            return data
    def write(self, path, data, offset, fh):
        with self.inode_lock(fh):
            written = os.pwrite(fh.fh, data, offset)
            if os.environ.get('ASB_PROBE_HOLD_WRITE') == '1' and data.startswith(b'changed'):
                (evidence / 'write-applied').touch()
                deadline = time.monotonic() + 5
                while not (evidence / 'write-release').exists() and time.monotonic() < deadline:
                    time.sleep(0.005)
            self.record('write', path, data[:written], offset=offset, handle=self.handle(fh),
                        requested_size=len(data), request_sha256=hashlib.sha256(data).hexdigest())
            return written
    def truncate(self, path, length, fh=None):
        lock = self.lock_for_stat(os.stat(self.path(path))) if fh is None else self.inode_lock(fh)
        if os.environ.get('ASB_PROBE_HOLD_TRUNCATE_LOOKUP') == '1' and path == '/victim':
            (evidence / 'truncate-lookup').touch()
            deadline = time.monotonic() + 5
            while not (evidence / 'truncate-release').exists() and time.monotonic() < deadline:
                time.sleep(0.005)
        with lock:
            if fh is None: os.truncate(self.path(path), length)
            else: os.ftruncate(fh.fh, length)
            self.record('truncate', path, size=length, handle=self.handle(fh) if fh is not None else None)
    def chmod(self, path, mode):
        with self.lock_for_stat(os.stat(self.path(path))):
            os.chmod(self.path(path), mode)
            self.record('chmod', path, mode=mode)
    def utimens(self, path, times=None):
        with self.lock_for_stat(os.stat(self.path(path))):
            if times is None: os.utime(self.path(path), None)
            else: os.utime(self.path(path), ns=times)
            st = os.stat(self.path(path))
            self.record('utimens', path, atime_ns=st.st_atime_ns, mtime_ns=st.st_mtime_ns)
    def getxattr(self, path, name, position=0):
        return self.observe_metadata('getxattr', path,
                                     lambda: os.getxattr(self.path(path), name, follow_symlinks=False), name=name, position=position)
    def listxattr(self, path):
        return self.observe_metadata('listxattr', path,
                                     lambda: os.listxattr(self.path(path), follow_symlinks=False))
    def setxattr(self, path, name, value, options, position=0):
        with self.lock_for_stat(os.lstat(self.path(path))):
            os.setxattr(self.path(path), name, value, options, follow_symlinks=False)
            self.record('setxattr', path, value, name=name, options=options)
    def removexattr(self, path, name):
        with self.lock_for_stat(os.lstat(self.path(path))):
            os.removexattr(self.path(path), name, follow_symlinks=False)
            self.record('removexattr', path, name=name)
    def rename(self, old, new):
        os.rename(self.path(old), self.path(new)); self.record('rename', old, destination=new)
    def link(self, target, source):
        os.link(self.path(source), self.path(target)); self.record('link', source, destination=target)
    def symlink(self, target, source):
        os.symlink(source, self.path(target)); self.record('symlink', target, destination=source)
    def readlink(self, path):
        return self.observe_metadata('readlink', path, lambda: os.readlink(self.path(path)))
    def unlink(self, path): os.unlink(self.path(path)); self.record('unlink', path)
    def mkdir(self, path, mode):
        os.mkdir(self.path(path), mode)
        if os.geteuid() == 0:
            uid, gid, _ = fuse_get_context()
            os.chown(self.path(path), uid, gid)
        self.record('mkdir', path, mode=mode)
    def rmdir(self, path): os.rmdir(self.path(path)); self.record('rmdir', path)
    def flush(self, path, fh): os.fsync(fh.fh)
    def fsync(self, path, datasync, fh): os.fsync(fh.fh)
    def release(self, path, fh):
        with self.handle_lock:
            handle = self.handles.pop(fh.fh)
            os.close(fh.fh)
            self.record('release', path, handle=handle)
FUSE(Proxy(), str(mount), foreground=True, nothreads=False, raw_fi=True, use_ino=True,
     **({'allow_other': True} if os.environ.get('ASB_PROBE_ALLOW_OTHER') == '1' else {}),
     **({'nonempty': True} if os.environ.get('ASB_PROBE_NONEMPTY') == '1' else {}))
