"""Bounded same-architecture ASB runtime bundles for owned Linux task images.

Static ELF executables are copied directly. Dynamically linked executables
bring their own matching loader/libraries; no guest system library is replaced
and LD_LIBRARY_PATH is never inherited by benchmark clients.
"""
import hashlib
import os
from pathlib import Path
import re
import shlex
import struct
import stat
import subprocess
import selectors
import threading
import time

from asb_execution import ExecutionError

MAX_FILE = 256 * 1024**2
MAX_BUNDLE = 384 * 1024**2
ROOT = "/opt/asb-runtime"
_PROBE_CUSTODY = None
_PROBE_LOCK = threading.Lock()

class _ProbeCustody:
    """One pre-established direct-child fallback; never owns a numeric PGID."""
    def __init__(self):
        self.ready = threading.Event()
        self.child = None
        self.transferred = False
        self.starting = True
        self.active = threading.Lock()
        def reap():
            self.ready.wait()
            if self.child is not None:
                while True:
                    try:
                        if self.child.poll() is None:self.child.kill()
                        self.child.wait()
                        return
                    except OSError:
                        # Retain this exact Popen owner on uncertain settlement;
                        # never replace it with a numeric PID/group or new work.
                        time.sleep(.05)
        self.worker = threading.Thread(target=reap,daemon=True,name="asb-runtime-probe-reaper")

    def start(self):
        self.worker.start()  # Fallible before Popen; no helper exists on failure.
        self.starting = False

    def begin(self, deadline):
        if self.starting:
            raise ExecutionError("runtime probe cleanup custody unavailable before spawn")
        if not self.active.acquire(timeout=max(0,deadline-time.monotonic())):
            raise ExecutionError("runtime probe admission deadline exceeded")
        if self.transferred:
            self.active.release()
            raise ExecutionError("runtime probe cleanup remains unconfirmed")

    def finish(self, child):
        try:
            if child is not None:
                try:
                    if child.poll() is None:child.kill()
                    child.wait(timeout=1)
                except BaseException:
                    # Includes main-thread unwind during cleanup itself.
                    self.child,self.transferred = child,True
                    self.ready.set()
                    raise
            return True
        except (OSError,subprocess.TimeoutExpired):
            return False
        finally:
            self.active.release()
        # No pipe worker exists. Never join a keeper that may own a stuck child.


def bounded_probe(arguments, *, environment=None, deadline=None):
    """Bound local probe IO; enclosing Linux discovery owns descendants."""
    deadline = min(time.monotonic()+15,deadline) if deadline is not None else time.monotonic()+15
    if time.monotonic() >= deadline:raise ExecutionError("runtime probe deadline expired")
    # Preserve the previously healthy non-Unix path without claiming its
    # communicate/capture has the Unix during-read memory bound.
    if os.name != "posix":
        result = subprocess.run(arguments,env=environment,stdin=subprocess.DEVNULL,
            capture_output=True,timeout=max(.001,deadline-time.monotonic()))
        if result.returncode or len(result.stdout)>1024**2 or len(result.stderr)>65536:
            raise ExecutionError("runtime probe failed or exceeded its output bound")
        return result.stdout
    global _PROBE_CUSTODY
    with _PROBE_LOCK:
        if _PROBE_CUSTODY is None:
            # Publish ownership before fallible start: interrupted startup may
            # leave one idle worker, but can never admit another keeper/child.
            _PROBE_CUSTODY = _ProbeCustody()
            try:_PROBE_CUSTODY.start()
            except (OSError,RuntimeError) as error:
                raise ExecutionError("runtime probe cleanup custody unavailable before spawn") from error
        custody = _PROBE_CUSTODY
    custody.begin(deadline)
    child = None
    primary = ""
    try:
        child = subprocess.Popen(arguments,env=environment,stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,stderr=subprocess.PIPE,bufsize=0)
        streams,buffers = [child.stdout,child.stderr],[bytearray(),bytearray()]
        with selectors.DefaultSelector() as selector:
            for index,stream in enumerate(streams):
                os.set_blocking(stream.fileno(),False)
                selector.register(stream,selectors.EVENT_READ,index)
            while selector.get_map() or child.poll() is None:
                remaining = deadline-time.monotonic()
                if remaining <= 0:raise ExecutionError("runtime probe deadline exceeded")
                for key,_ in selector.select(min(.02,remaining)):
                    index = key.data
                    limit = 1024**2 if index==0 else 65536
                    for _ in range(4):
                        if time.monotonic() >= deadline:break
                        try:chunk = os.read(key.fileobj.fileno(),min(16384,limit-len(buffers[index])+1))
                        except InterruptedError:continue
                        except BlockingIOError:break
                        if not chunk:
                            selector.unregister(key.fileobj)
                            break
                        if len(buffers[index])+len(chunk)>limit:
                            raise ExecutionError("runtime probe output exceeded its bound")
                        buffers[index].extend(chunk)
            if child.returncode:raise ExecutionError("runtime probe failed")
            return bytes(buffers[0])
    except ExecutionError as error:
        primary = str(error)+"; "
        raise
    except (OSError,ValueError) as error:
        primary = "runtime probe transport failed; "
        raise ExecutionError("runtime probe transport failed") from error
    finally:
        # Closing raw FileIO does not acquire a blocked buffered-reader lock.
        close_failed = False
        try:
            if child is not None:
                for stream in [child.stdout,child.stderr]:
                    try:
                        if stream is not None:stream.close()
                    except OSError:close_failed = True
        finally:
            confirmed = custody.finish(child)
            if not confirmed:
                raise ExecutionError(primary+"runtime probe cleanup unconfirmed; direct-child custody retained")
        if close_failed:raise ExecutionError(primary+"runtime probe pipe cleanup failed")


def _elf_metadata(source, length):
    """Read bounded ELF64 headers, never the payload or debug/section tables."""
    def read_at(offset, size):
        if offset < 0 or size < 0 or offset + size > length:
            raise ExecutionError("ASB runtime has invalid ELF program headers")
        source.seek(offset)
        data = source.read(size)
        if len(data) != size:
            raise ExecutionError("ASB runtime executable changed during bounded inspection")
        return data
    data = read_at(0, min(64, length))
    if data[:7] != b"\x7fELF\x02\x01\x01" or len(data) < 64:
        raise ExecutionError("ASB runtime must be a Linux little-endian ELF64 executable")
    if struct.unpack_from("<H",data,16)[0] not in [2,3] or struct.unpack_from("<I",data,20)[0] != 1:
        raise ExecutionError("ASB runtime has an invalid ELF executable type or version")
    machine = struct.unpack_from("<H",data,18)[0]
    if machine not in [62,183]:
        raise ExecutionError("ASB runtime supports Linux x86_64 and aarch64 task images")
    offset = struct.unpack_from("<Q",data,32)[0]
    size,count = struct.unpack_from("<HH",data,54)
    if (struct.unpack_from("<H",data,52)[0] != 64 or size < 56 or not 1 <= count <= 2048
            or offset < 64 or offset + size * count > length):
        raise ExecutionError("ASB runtime has invalid ELF program headers")
    interpreter = None
    for index in range(count):
        # A large declared stride is a seek, never an allocation/read length.
        header = read_at(offset + size * index, 56)
        if struct.unpack_from("<I",header,0)[0] == 3:
            start = struct.unpack_from("<Q",header,8)[0]
            width = struct.unpack_from("<Q",header,32)[0]
            if interpreter is not None or not 2 <= width <= 4096 or start + width > length:
                raise ExecutionError("ASB runtime has an invalid ELF interpreter")
            raw = read_at(start, width)
            if raw[-1:] != b"\0" or b"\0" in raw[:-1]:
                raise ExecutionError("ASB runtime has an invalid ELF interpreter")
            try:
                interpreter = raw[:-1].decode("ascii")
            except UnicodeError as error:
                raise ExecutionError("ASB runtime has an invalid ELF interpreter") from error
            if not interpreter.startswith("/"):
                raise ExecutionError("ASB runtime has an invalid ELF interpreter")
    return machine, interpreter


def _inspect_elf(path, payload):
    path = Path(path)
    missing = ("ASB runtime executable is missing or exceeds 256 MiB" if payload else
               "ASB runtime metadata executable is missing or not regular")
    if not path.is_file():
        raise ExecutionError(missing)
    descriptor = os.open(path,os.O_RDONLY|getattr(os,"O_NONBLOCK",0))
    # Metadata seeks must not amplify fixed header reads through userspace
    # readahead. Keep the existing complete-payload reader's buffering policy.
    with os.fdopen(descriptor,"rb",buffering=-1 if payload else 0) as source:
        before = os.fstat(source.fileno())
        if not stat.S_ISREG(before.st_mode) or (payload and before.st_size > MAX_FILE):
            raise ExecutionError(missing)
        def unchanged():
            after = os.fstat(source.fileno())
            return (before.st_dev,before.st_ino,before.st_size,before.st_mtime_ns,before.st_ctime_ns)==(after.st_dev,after.st_ino,after.st_size,after.st_mtime_ns,after.st_ctime_ns)
        data = source.read(MAX_FILE+1) if payload else None
        if payload and (len(data)>MAX_FILE or len(data)!=before.st_size or not unchanged()):
            raise ExecutionError("ASB runtime executable changed during bounded inspection")
        machine, interpreter = _elf_metadata(source, before.st_size)
        if not unchanged():
            raise ExecutionError("ASB runtime executable changed during bounded inspection")
    return data,machine,interpreter


def elf(path):
    """Read a bounded complete ELF payload for guest staging."""
    return _inspect_elf(path, True)


def elf_identity(path):
    """Inspect a regular executable's headers without admitting a guest payload.

    Debug information can exceed the staging cap. Metadata inspection reads at
    most 64 + 2048*56 + 4096 bytes through one pinned descriptor; target code is
    never executed and section/debug content is not loaded. Full content input
    fingerprints and guest payload limits remain separate obligations.
    """
    _, machine, interpreter = _inspect_elf(path, False)
    return machine, interpreter


def library_paths(text):
    """Parse loader --list output without executing shell commands or ldd."""
    if text.strip() == "statically linked":
        return {}
    result = {}
    first = True
    anonymous_main = False
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        # A loader may list the already inspected main object with no name.
        # Its address is metadata, never a library identity or inferred path.
        if first and re.fullmatch(r"\(0x[0-9a-fA-F]{1,16}\)",line):
            anonymous_main = True
            first = False
            continue
        first = False
        if line.startswith("linux-vdso") or line.startswith("linux-gate"):
            continue
        mapped = re.fullmatch(r"([A-Za-z0-9+_.-]+) => (/[^\n]+?) \(0x[0-9a-fA-F]+\)",line)
        direct = re.fullmatch(r"(/[^\n]+?) \(0x[0-9a-fA-F]+\)",line)
        if mapped:
            name,path = mapped.groups()
        elif direct:
            path = direct.group(1)
            name = Path(path).name
        else:
            raise ExecutionError("ASB loader could not resolve a complete runtime dependency list")
        if not re.fullmatch(r"[A-Za-z0-9+_.-]{1,128}",name) or name in [".",".."]:
            raise ExecutionError("ASB loader returned an invalid library name")
        if name in result and result[name] != path:
            raise ExecutionError("ASB loader returned conflicting library identities")
        result[name] = path
        if len(result) > 64:
            raise ExecutionError("ASB runtime exceeds its dependency limit")
    if anonymous_main and not result:
        raise ExecutionError("ASB loader returned an anonymous main mapping without resolved dependencies")
    return result


def resolve_dependencies(interpreter,path,environment=None,*,deadline=None):
    try:
        observed = bounded_probe([interpreter,"--list",str(Path(path).absolute())],
            environment={"PATH":os.defpath,"LC_ALL":"C"} if environment is None else environment,deadline=deadline)
    except (OSError,subprocess.TimeoutExpired) as error:
        raise ExecutionError("cannot inspect the running ASB executable's dynamic libraries") from error
    try:return library_paths(observed.decode("utf-8"))
    except UnicodeError as error:raise ExecutionError("ASB loader returned invalid runtime metadata") from error


def bundle(path, architecture=None):
    data,machine,interpreter = elf(path)
    expected = {62:"amd64",183:"arm64"}[machine]
    if architecture is not None and architecture != expected:
        raise ExecutionError("ASB runtime architecture does not match the selected task image")
    files = [("asb",data,0o755)]
    kind = "static"
    if interpreter:
        _,loader_machine,_ = elf(interpreter)
        if loader_machine != machine:
            raise ExecutionError("ASB loader architecture does not match its executable")
        dependencies = resolve_dependencies(interpreter,path)
        loader_name = Path(interpreter).name
        if not re.fullmatch(r"[A-Za-z0-9+_.-]{1,128}",loader_name):
            raise ExecutionError("ASB runtime loader has an invalid file name")
        dependencies[loader_name] = interpreter
        # Older glibc builds may load DNS/files NSS plugins lazily. Include
        # only these matching standard plugins, never host NSS configuration.
        if "libc.so.6" in dependencies:
            directory = Path(dependencies["libc.so.6"]).parent
            for name in ["libnss_dns.so.2","libnss_files.so.2"]:
                candidate = directory / name
                if candidate.is_file():
                    dependencies.setdefault(name,str(candidate))
                    for dependency,location in resolve_dependencies(interpreter,candidate).items():
                        if dependency in dependencies and Path(dependencies[dependency]).resolve() != Path(location).resolve():
                            raise ExecutionError("ASB NSS plugin requires conflicting runtime libraries")
                        dependencies[dependency] = location
        if len(dependencies) > 64:
            raise ExecutionError("ASB runtime exceeds its dependency limit")
        files = [("bin/asb",data,0o755)]
        for name,library in sorted(dependencies.items()):
            content,library_machine,_ = elf(library)
            if library_machine != machine:
                raise ExecutionError("ASB runtime contains an incompatible library architecture")
            files.append(("lib/"+name,content,0o755))
            if sum(len(content) for _,content,_ in files) > MAX_BUNDLE:
                raise ExecutionError("ASB runtime bundle exceeds 384 MiB")
        launcher = ("#!/bin/sh\nexport ASB_RUNTIME_EXECUTABLE="+shlex.quote(ROOT+"/asb")+"\n"
            "exec "+shlex.quote(ROOT+"/lib/"+loader_name)+" --library-path "+shlex.quote(ROOT+"/lib")
            +" "+shlex.quote(ROOT+"/bin/asb")+' "$@"\n')
        files.append(("asb",launcher.encode(),0o755))
        kind = "bundled-loader"
    receipt = {"schema_version":1,"kind":kind,"architecture":expected,
               "files":[{"name":name,"sha256":hashlib.sha256(content).hexdigest(),"bytes":len(content)}
                        for name,content,_ in files]}
    return files,receipt


def bundle_tool(path, name, architecture=None):
    """Bundle an auxiliary executable and its own loader under a private prefix."""
    if not isinstance(name,str) or not re.fullmatch(r'[a-z0-9][a-z0-9_.-]{0,79}',name):
        raise ExecutionError('auxiliary runtime name is invalid')
    files,receipt = bundle(path,architecture)
    prefix = 'tools/'+name
    tool_root = ROOT+'/'+prefix
    output = []
    for filename,content,mode in files:
        if filename == 'asb':
            filename = name
            if receipt['kind'] == 'bundled-loader':
                launcher = content.decode('utf-8')
                launcher = '\n'.join(line for line in launcher.split('\n')
                                     if not line.startswith('export ASB_RUNTIME_EXECUTABLE='))
                content = launcher.replace(ROOT,tool_root).replace('/bin/asb','/bin/'+name).encode('utf-8')
        elif filename == 'bin/asb':
            filename = 'bin/'+name
        output.append((prefix+'/'+filename,content,mode))
    receipt = dict(receipt,files=[{'name':filename,'sha256':hashlib.sha256(content).hexdigest(),
                                   'bytes':len(content)} for filename,content,_ in output])
    return output,receipt
