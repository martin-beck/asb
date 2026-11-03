"""Local feasibility probe only; not an ASB replay backend."""
import errno, json, os, socket, stat, sys, threading, time
from pathlib import Path
from fuse import FUSE, FuseOSError, Operations, fuse_get_context

backing, mount, evidence = map(Path, sys.argv[1:4])

def caller_identity(tid):
    """Best-effort bounded ancestry at callback time; not an authority boundary."""
    def status(pid):
        fields = {}
        for line in Path(f'/proc/{pid}/status').read_text().splitlines():
            key, separator, value = line.partition(':')
            if separator and key in ('Tgid', 'PPid'):
                fields[key] = int(value.strip())
        parts = Path(f'/proc/{pid}/stat').read_text().rpartition(')')[2].split()
        return fields['Tgid'], fields['PPid'], int(parts[19])
    try:
        tgid, ppid, started = status(tid)
        ancestry = []
        current = tgid
        root = int(os.environ['ASB_PROBE_ROOT_TGID'])
        for _ in range(64):
            group, parent, start = status(current)
            ancestry.append({'tgid': group, 'start_ticks': start})
            if group == root or parent <= 0 or parent == group:
                break
            current = parent
        return {'tid': tid, 'tgid': tgid, 'ppid': ppid,
                'start_ticks': started, 'ancestry': ancestry,
                'ancestry_complete': bool(ancestry and ancestry[-1]['tgid'] == root)}
    except (OSError, KeyError, ValueError, IndexError):
        return {'tid': tid, 'tgid': None, 'ppid': None, 'start_ticks': None, 'ancestry': [], 'ancestry_complete': False}

class Proxy(Operations):
    def __init__(self):
        self.record_lock = threading.Lock()
        self.sequence = 0
        self.handle_lock = threading.Lock()
        self.next_handle = 0
        self.handles = {}
        self.inode_locks = {}
        self.log = open(evidence / 'events.jsonl', 'a', encoding='utf-8', buffering=1)
        control_fd = os.environ.get('ASB_PROBE_CONTROL_FD')
        if control_fd is not None:
            self.control = socket.socket(fileno=int(control_fd))
            threading.Thread(target=self.control_loop, daemon=True).start()
        control_path = os.environ.get('ASB_PROBE_CONTROL_SOCKET')
        if control_path:
            self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            self.listener.bind(control_path)
            os.chmod(control_path, 0o666)
            self.listener.listen(1)
            threading.Thread(target=self.accept_control, daemon=True).start()
    def accept_control(self):
        self.control, _ = self.listener.accept()
        self.control_loop()
    def control_loop(self):
        with self.control.makefile('rwb', buffering=0) as stream:
            for line in stream:
                marker = line.decode('ascii').strip()
                if marker not in ('tool-begin:child', 'tool-end:child', 'tool-begin:hermes', 'tool-end:hermes', 'tool-begin:a', 'tool-end:a', 'tool-begin:b', 'tool-end:b', 'tool-begin:bg', 'tool-end:bg', 'tool-begin:after', 'tool-end:after', 'tool-begin:orphan', 'tool-end:orphan', 'tool-begin:one', 'tool-end:one', 'tool-begin:two', 'tool-end:two', 'tool-begin:write', 'tool-end:write', 'tool-begin:read', 'tool-end:read', 'tool-begin:patch', 'tool-end:patch'):
                    raise RuntimeError('invalid fixture marker')
                sequence = self.record('marker', None, marker=marker, is_marker=True)
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
            self.sequence += 1
            event = {'sequence': self.sequence, 'op': op, 'path': path, 'caller': None if is_marker else caller_identity(fuse_get_context()[2]), **detail}
            if data is not None:
                name = f'object-{self.sequence:08d}'
                with open(evidence / name, 'xb') as f: f.write(data)
                event.update(object=name, bytes=len(data))
            self.log.write(json.dumps(event, separators=(',', ':')) + '\n')
            return self.sequence
    def getattr(self, path, fh=None):
        try: st = os.lstat(self.path(path))
        except OSError as error: raise FuseOSError(error.errno)
        return {key: getattr(st, key) for key in ('st_atime','st_ctime','st_gid','st_mode','st_mtime','st_nlink','st_size','st_uid')}
    def readdir(self, path, fh): return ['.', '..', *os.listdir(self.path(path))]
    def open(self, path, fi):
        try:
            fi.fh = os.open(self.path(path), fi.flags)
            handle = self.register(fi.fh)
            self.record('open', path, handle=handle, flags=fi.flags)
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
            self.record('read', path, data, offset=offset, handle=self.handle(fh))
            return data
    def write(self, path, data, offset, fh):
        with self.inode_lock(fh):
            written = os.pwrite(fh.fh, data, offset)
            if os.environ.get('ASB_PROBE_HOLD_WRITE') == '1' and data.startswith(b'changed'):
                (evidence / 'write-applied').touch()
                deadline = time.monotonic() + 5
                while not (evidence / 'write-release').exists() and time.monotonic() < deadline:
                    time.sleep(0.005)
            self.record('write', path, data[:written], offset=offset, handle=self.handle(fh))
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
    def rename(self, old, new):
        os.rename(self.path(old), self.path(new)); self.record('rename', old, destination=new)
    def link(self, target, source):
        os.link(self.path(source), self.path(target)); self.record('link', source, destination=target)
    def symlink(self, target, source):
        os.symlink(source, self.path(target)); self.record('symlink', target, destination=source)
    def readlink(self, path): return os.readlink(self.path(path))
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
FUSE(Proxy(), str(mount), foreground=True, nothreads=False, raw_fi=True,
     **({'allow_other': True} if os.environ.get('ASB_PROBE_ALLOW_OTHER') == '1' else {}))
