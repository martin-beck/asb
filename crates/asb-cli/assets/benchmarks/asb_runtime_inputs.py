"""Discover declared runtime files without invoking benchmark entry points."""
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import stat
import struct
import subprocess
import sys
import time

import asb_runtime


class InputError(ValueError):
    pass


def absolute(path):
    path = Path(path)
    return path if path.is_absolute() else Path.cwd() / path


class Discovery:
    def __init__(self, timeout=50):
        self.paths = set()
        self.programs = set()
        self.native_scanned = set()
        # Exact existing json.dumps response bytes, including print's newline.
        self.response_bytes = len(json.dumps({"schema_version":1,"paths":[]}))+1
        self.deadline = time.monotonic() + timeout

    def check(self):
        if time.monotonic() > self.deadline or len(self.paths)>100000 or len(self.programs)>4096:
            raise InputError("runtime discovery resource bound exceeded")

    def file(self, path):
        self.check()
        path = absolute(path)
        if "\0" in str(path):
            raise InputError("invalid input path")
        text = str(path)
        if text not in self.paths:
            size = 2  # JSON quotes; count escaping without allocating its copy.
            for character in text:
                value = ord(character)
                size += (2 if character in '\\"\b\f\n\r\t' else
                         6 if value<32 or 127<=value<=65535 else
                         12 if value>65535 else 1)
                if self.response_bytes+size+(2 if self.paths else 0)>4*1024**2:
                    raise InputError("runtime response names exceeded their serialized byte bound")
            if len(self.paths)>=100000:
                raise InputError("runtime path population exceeded")
            self.response_bytes += size+(2 if self.paths else 0)
        self.paths.add(text)
        return path

    def program(self, path, search=None, depth=0):
        self.check()
        if depth>16:
            raise InputError("interpreter depth exceeded")
        search = os.environ.get("PATH", os.defpath) if search is None else search
        path = os.fspath(path)
        if "/" not in path:
            path = shutil.which(path, path=search)
        if not path:
            raise InputError("required runtime executable is unavailable")
        path = self.file(path)
        if not path.is_file() or not os.access(path, os.X_OK):
            raise InputError("runtime program is not executable")
        identity = (str(path), search)
        if identity in self.programs:
            return path
        self.programs.add(identity)
        descriptor = os.open(path,os.O_RDONLY|getattr(os,"O_NONBLOCK",0))
        with os.fdopen(descriptor,"rb") as source:
            if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
                raise InputError("runtime entry changed to a non-regular file")
            header = source.read(4096)
        if header.startswith(b"\x7fELF"):
            machine, interpreter = asb_runtime.elf_identity(path)
            if interpreter:
                if asb_runtime.elf_identity(interpreter)[0]!=machine:
                    raise InputError("runtime loader has a different architecture")
                self.file(interpreter)
                environment = {"PATH":os.defpath,"LC_ALL":"C"}
                for name in ["LD_LIBRARY_PATH","LD_PRELOAD"]:
                    if name in os.environ:
                        environment[name] = os.environ[name]
                libraries = asb_runtime.resolve_dependencies(interpreter,path,environment,deadline=self.deadline)
                for library in libraries.values():
                    self.file(library)
            return path
        if not header.startswith(b"#!"):
            raise InputError("unsupported runtime entry point format")
        line = header.split(b"\n",1)[0][2:]
        words = shlex.split(line.decode("utf-8"))
        if not words:
            raise InputError("missing interpreter")
        self.program(words[0], search, depth+1)
        if Path(words[0]).name == "env":
            command = words[1:]
            if command and command[0] == "-S":
                command = command[1:]
            if not command or command[0].startswith("-") or "=" in command[0]:
                raise InputError("unsupported env interpreter options")
            self.program(command[0],search,depth+1)
        return path

    def python(self, path, site=True):
        path = self.program(path)
        # Isolated mode ignores cwd/user-site/PYTHONPATH for the probe itself.
        # The caller separately declares inherited PYTHONPATH directories.
        script = "import json,sys,sysconfig; print(json.dumps({'paths':sys.path,'stdlib':sysconfig.get_path('stdlib'),'platstdlib':sysconfig.get_path('platstdlib'),'executable':sys.executable}))"
        arguments = [path,"-I","-B"] + ([] if site else ["-S"]) + ["-c",script]
        result = asb_runtime.bounded_probe(arguments,deadline=self.deadline)
        info = json.loads(result)
        paths = info["paths"]+[info["stdlib"], info["platstdlib"]]
        if len(paths)>512:
            raise InputError("Python runtime path population exceeded")
        self.program(info["executable"])
        _,loader = asb_runtime.elf_identity(info["executable"])
        for item in paths:
            if not isinstance(item,str) or not item or not Path(item).is_absolute():
                raise InputError("Python runtime path is not absolute")
            if item in [info["stdlib"],info["platstdlib"]] and Path(item).is_dir():
                # A base stdlib can contain global packages excluded by the
                # selected venv. Bind its children without sweeping unrelated
                # host site-packages into this interpreter's identity.
                for child in sorted(Path(item).iterdir()):
                    if child.name not in ["site-packages","dist-packages"]:
                        self.file(child)
                        self.python_extensions(child,loader)
            else:
                self.file(item)
                self.python_extensions(Path(item),loader)
        for item in os.environ.get("PYTHONPATH", "").split(os.pathsep):
            if item:
                self.file(item)

    def python_extensions(self,root,loader):
        if not loader or root in self.native_scanned:
            return
        self.native_scanned.add(root)
        candidates = []
        if root.is_dir():
            for directory,children,files in os.walk(root,followlinks=False):
                self.check()
                children.sort()
                for name in files:
                    if ".so" in name:
                        candidates.append(Path(directory)/name)
                        if len(candidates)>4096:
                            raise InputError("Python native dependency population exceeded")
        elif ".so" in root.name:
            candidates.append(root)
        environment = {"PATH":os.defpath,"LC_ALL":"C"}
        for name in ["LD_LIBRARY_PATH","LD_PRELOAD"]:
            if name in os.environ:
                environment[name] = os.environ[name]
        for path in sorted(candidates):
            self.file(path)
            # Only extension entry points have PyInit exports. Audited wheels'
            # dependent DSOs can require the importing module's inherited RPATH
            # and must not be inspected as standalone loader roots.
            if not python_extension(path):
                continue
            asb_runtime.elf_identity(path)
            for dependency in asb_runtime.resolve_dependencies(loader,path,environment,deadline=self.deadline).values():
                self.file(dependency)

    def process(self, pid):
        if type(pid) is not int or pid<=0 or sys.platform!="linux":
            raise InputError("unsupported process runtime identity")
        executable = Path(f"/proc/{pid}/exe")
        _,interpreter = asb_runtime.elf_identity(executable)
        if interpreter:
            self.file(interpreter)
            for library in asb_runtime.resolve_dependencies(interpreter,executable,deadline=self.deadline).values():
                self.file(library)
        # Bind libraries actually mapped in the running ASB, not a newer file
        # installed under an old library's pathname during a package upgrade.
        with Path(f"/proc/{pid}/maps").open("r",encoding="utf-8") as source:
            text = source.read(4*1024**2+1)
        if len(text)>4*1024**2:
            raise InputError("process mapping population exceeded")
        target = executable.stat()
        for line in text.splitlines():
            fields = line.split(None,5)
            if len(fields)<6 or not fields[5].startswith("/"):
                continue
            address, permissions, offset, device, inode, name = fields
            if "x" not in permissions:
                continue
            if int(inode)==target.st_ino and os.makedev(*[int(value,16) for value in device.split(":")])==target.st_dev:
                continue
            if name.endswith(" (deleted)") or re.search(r"\\[0-7]{3}",name):
                raise InputError("mapped library cannot be bound to its original file")
            path = Path(name)
            info = path.stat()
            if info.st_ino!=int(inode) or info.st_dev!=os.makedev(*[int(value,16) for value in device.split(":")]):
                raise InputError("mapped runtime file was replaced")
            self.file(path)


def discover(request):
    if not isinstance(request,list) or len(request)>4096:
        raise InputError("invalid runtime declaration population")
    discovery = Discovery()
    for entry in request:
        if not isinstance(entry,dict):
            raise InputError("invalid runtime declaration")
        kind = entry.get("kind")
        if kind=="file" and set(entry)=={"kind","path"}:
            discovery.file(entry["path"])
        elif kind=="program" and set(entry)=={"kind","path","search"}:
            discovery.program(entry["path"],entry["search"])
        elif kind=="python" and set(entry)=={"kind","path","site"} and type(entry["site"]) is bool:
            discovery.python(entry["path"],entry["site"])
        elif kind=="process" and set(entry)=={"kind","pid"}:
            discovery.process(entry["pid"])
        else:
            raise InputError("unknown runtime declaration")
    return {"schema_version":1,"paths":sorted(discovery.paths)}


def python_extension(path):
    """Bounded ELF64 dynamic-symbol inspection, never dlopen/import a module."""
    with Path(path).open("rb") as source:
        size = os.fstat(source.fileno()).st_size
        header = source.read(64)
        if header[:7]!=b"\x7fELF\x02\x01\x01":
            return False
        if len(header)!=64:
            raise InputError("truncated native module header")
        offset = struct.unpack_from("<Q",header,40)[0]
        width,count = struct.unpack_from("<HH",header,58)
        if width!=64 or not 1<=count<=65535 or offset+width*count>size:
            raise InputError("invalid native module section bounds")
        source.seek(offset)
        sections = source.read(width*count)
        for index in range(count):
            section = index*width
            if struct.unpack_from("<I",sections,section+4)[0]!=11:
                continue
            start,length = struct.unpack_from("<QQ",sections,section+24)
            strings = struct.unpack_from("<I",sections,section+40)[0]
            entry_size = struct.unpack_from("<Q",sections,section+56)[0]
            if (strings>=count or entry_size!=24 or length>24*1024**2 or length%24
                    or start+length>size):
                raise InputError("invalid native module symbol bounds")
            text_start,text_length = struct.unpack_from("<QQ",sections,strings*width+24)
            if text_length>16*1024**2 or text_start+text_length>size:
                raise InputError("invalid native module string bounds")
            source.seek(text_start); text = source.read(text_length)
            source.seek(start); symbols = source.read(length)
            for at in range(0,len(symbols),24):
                name = struct.unpack_from("<I",symbols,at)[0]
                defined = struct.unpack_from("<H",symbols,at+6)[0]!=0
                if defined and text[name:name+7]==b"PyInit_":
                    return True
    return False


def main():
    try:
        request = sys.stdin.buffer.read(1024*1024+1)
        if len(request)>1024*1024:
            raise InputError("runtime request exceeded its limit")
        print(json.dumps(discover(json.loads(request))))
        return 0
    except Exception as error:
        # File paths, environment, private interpreter output and exception
        # messages never enter the public error response.
        # Preserve only a small stable code so the caller can distinguish an
        # actionable capacity/installation problem from an opaque helper
        # failure without exposing private paths or environment values.
        message = str(error)
        if "resource bound" in message or "population exceeded" in message:
            code = "resource_bound"
        elif "unavailable" in message or "not executable" in message:
            code = "required_runtime_unavailable"
        elif "unsupported" in message or "interpreter" in message:
            code = "runtime_entry_unsupported"
        else:
            code = "runtime_input_validation_failed"
        print(json.dumps({"schema_version":1,"error_code":code,"paths":[]}))
        return 0


if __name__=="__main__":
    raise SystemExit(main())
