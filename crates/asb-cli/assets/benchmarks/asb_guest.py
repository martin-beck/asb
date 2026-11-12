"""Install and execute ASB-selected clients in an owned task environment.

No provider/agent registry lives here. Selection, package recipes, endpoint
configuration and output interpretation remain in the same Rust adapters used
by native execution. Host homes, venvs and project directories are not mounted.
"""
from contextlib import ExitStack
import hashlib
import json
import math
import os
from pathlib import Path
import posixpath
import re
import tarfile
import tempfile
import time
import threading

from asb_execution import ExecutionError, principal_observation
from asb_runtime import bundle, bundle_tool
from asb_staging import upload, upload_files, stage_runtime, root_execute, prepare_guest_roots
from asb_relay import HostRelay, ForwardingCancellation, forward_request


MAX_EXPORTED_ARTIFACTS = 8
MAX_EXPORTED_ARTIFACT_BYTES = 16 * 1024 * 1024


def write_preparation_budget_artifact(task, name, value):
    artifacts = getattr(task, "artifacts", None)
    if not isinstance(artifacts, Path):
        return
    if name not in {"agent-preparation-budget.json", "agent-preparation-failure.json"}:
        raise ExecutionError("invalid preparation budget artifact name")
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":")).encode() + b"\n"
    descriptor = os.open(artifacts / name, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        written = 0
        while written < len(encoded):
            written += os.write(descriptor, encoded[written:])
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def preparation_failure_code(error):
    """Return an allowlisted cause code without persisting error text."""
    if not isinstance(error, ExecutionError):
        return None
    message = str(error)
    for prefix, code in [
        ("container execution gate did not become ready", "gate-not-ready"),
        ("container execution gate returned", "gate-invalid-readiness"),
        ("container gate did not identify", "gate-invalid-pid"),
        ("container execution preparation was cancelled", "gate-preparation-cancelled"),
        ("cannot safely coordinate concurrent Docker gate resumes", "gate-lock"),
        ("cannot open Docker gate-resume coordination lock", "gate-lock"),
        ("Docker gate-resume coordination lock", "gate-lock"),
        ("cannot coordinate container execution gate resume", "gate-lock"),
        ("workspace is not running", "workspace-not-running"),
        ("one command is already running in this task workspace", "workspace-busy"),
        ("container execution gate stderr exceeded", "gate-stderr-limit"),
        ("execution transport returned no valid child exit status", "gate-transport-status"),
        ("cannot read isolated measurement identity", "measurement-identity"),
        ("isolated process did not return valid measurement identity", "measurement-identity"),
        ("container execution gate could not be resumed", "gate-release"),
        ("application gate resume control", "gate-release"),
    ]:
        if message.startswith(prefix):
            return code
    return "execution-error-other"


def experimental_snapshot_roots(configured):
    if configured is None:
        return []
    try:
        roots = json.loads(configured)
    except (TypeError, ValueError) as error:
        raise ExecutionError('experimental snapshot roots must be a JSON array of guest paths') from error
    if (not isinstance(roots, list) or
            any(not isinstance(path, str) or not path.startswith('/') or '\x00' in path for path in roots)):
        raise ExecutionError('experimental snapshot roots must be a JSON array of guest paths')
    return roots


class _ArchiveChunks:
    """Read Docker's chunk iterator incrementally with a total wire bound."""
    def __init__(self, chunks, limit):
        self.chunks = iter(chunks)
        self.limit = limit
        self.total = 0
        self.buffer = bytearray()
        self.eof = False

    def read(self, size):
        if size < 0:
            raise ExecutionError("guest artifact archive requires bounded reads")
        while len(self.buffer) < size and not self.eof:
            try:
                chunk = next(self.chunks)
            except StopIteration:
                self.eof = True
                break
            if not isinstance(chunk, bytes):
                raise ExecutionError("guest artifact archive stream is invalid")
            self.total += len(chunk)
            if self.total > self.limit:
                raise ExecutionError("guest artifact archive exceeds its bound")
            self.buffer.extend(chunk)
        result = bytes(self.buffer[:size])
        del self.buffer[:size]
        return result


def export_agent_artifacts(task, output, receipts):
    """Copy sealed guest files to a controller-owned private directory.

    Receipts travel in the bounded controller JSON message; opaque bytes stay
    out of that message and cross Docker's archive channel exactly once.
    """
    if receipts is None:
        return
    if (not isinstance(output,str) or not os.path.isabs(output) or not isinstance(receipts,list)
            or len(receipts) > MAX_EXPORTED_ARTIFACTS):
        raise ExecutionError("guest artifact export request is invalid")
    destination = Path(output)
    if (not destination.is_dir() or destination.is_symlink()):
        raise ExecutionError("guest artifact export destination is unavailable")
    seen, total = set(), 0
    for receipt in receipts:
        if (not isinstance(receipt,dict) or set(receipt) != {"kind","filename","bytes","sha256"}
                or not isinstance(receipt["kind"],str) or not re.fullmatch(r"[a-z0-9-]{1,128}",receipt["kind"])
                or not isinstance(receipt["filename"],str) or not re.fullmatch(r"artifact-[0-7]",receipt["filename"])
                or type(receipt["bytes"]) is not int or not 0 <= receipt["bytes"] <= MAX_EXPORTED_ARTIFACT_BYTES
                or not isinstance(receipt["sha256"],str) or not re.fullmatch(r"[0-9a-f]{64}",receipt["sha256"])
                or receipt["filename"] in seen):
            raise ExecutionError("guest artifact receipt is invalid")
        seen.add(receipt["filename"])
        total += receipt["bytes"]
        if total > MAX_EXPORTED_ARTIFACT_BYTES:
            raise ExecutionError("guest artifact receipts exceed their aggregate bound")
        source = "/opt/asb-private/attempt/exports/" + receipt["filename"]
        chunks, metadata = task.workspace.container.get_archive(source,chunk_size=65536,encode_stream=False)
        temporary = destination / ("." + receipt["filename"] + ".part")
        final = destination / receipt["filename"]
        if temporary.exists() or final.exists():
            raise ExecutionError("guest artifact export destination already exists")
        try:
            if metadata.get("name") != receipt["filename"] or metadata.get("linkTarget"):
                raise ExecutionError("guest artifact archive identity is invalid")
            archive = _ArchiveChunks(chunks, MAX_EXPORTED_ARTIFACT_BYTES + 1024 * 1024)
            with tarfile.open(fileobj=archive,mode="r|") as incoming:
                member = incoming.next()
                if member is None:
                    raise ExecutionError("guest artifact archive has invalid entries")
                if (member.name != receipt["filename"] or not member.isfile()
                        or member.size != receipt["bytes"]):
                    raise ExecutionError("guest artifact archive payload is invalid")
                extracted = incoming.extractfile(member)
                if extracted is None:
                    raise ExecutionError("guest artifact archive payload is unavailable")
                digest, copied = hashlib.sha256(), 0
                fd = os.open(temporary,os.O_WRONLY|os.O_CREAT|os.O_EXCL|getattr(os,"O_NOFOLLOW",0),0o600)
                with os.fdopen(fd,"wb") as target:
                    while True:
                        block = extracted.read(65536)
                        if not block:
                            break
                        copied += len(block)
                        if copied > receipt["bytes"]:
                            raise ExecutionError("guest artifact archive payload exceeds its receipt")
                        digest.update(block)
                        target.write(block)
                    target.flush()
                    os.fsync(target.fileno())
                if copied != receipt["bytes"] or digest.hexdigest() != receipt["sha256"]:
                    raise ExecutionError("guest artifact export digest differs from its receipt")
                if incoming.next() is not None:
                    raise ExecutionError("guest artifact archive has invalid entries")
            while archive.read(65536):
                pass
            os.replace(temporary,final)
        finally:
            if temporary.exists():
                temporary.unlink()
            close = getattr(chunks,"close",None)
            if close is not None:
                close()


class PreparationBudget:
    """One bounded monotonic deadline shared by all launch preparation stages."""
    def __init__(self, seconds):
        if type(seconds) not in [int,float] or not math.isfinite(seconds) or not 0 < seconds <= 7200:
            raise ExecutionError("invalid shared launch preparation budget")
        self.deadline, self.active = time.monotonic()+seconds, True
        self.on_failure, self.failed, self.lock = None, False, threading.Lock()

    def remaining(self):
        remaining = self.deadline-time.monotonic()
        if remaining <= 0:
            raise ExecutionError("shared launch preparation deadline exceeded")
        return remaining

    def expired(self):
        return self.active and time.monotonic() >= self.deadline

    def fail(self):
        with self.lock:
            notify = self.active and not self.failed
            self.failed = True
        if notify and self.on_failure is not None:
            try:self.on_failure()
            except BaseException:pass  # Notification must not skip settlement.


def stage_public_cache(container, project, temporary_dir=None, owner=None):
    cache = project / ".asb-cache/npm/_cacache"
    if not cache.is_dir():
        return
    paths = list(cache.rglob("*"))
    if (any(path.is_symlink() for path in paths)
            or len(paths) > 100000 or sum(path.stat().st_size for path in paths if path.is_file()) > 512 * 1024**2):
        return  # Do not copy unsafe/unbounded caches; normal install can download.
    for entry in paths:
        if entry.is_file() and "index-v5" in entry.parts:
            for line in entry.read_text().splitlines():
                if "\t" in line:
                    record = json.loads(line.split("\t", 1)[1])
                    metadata = record.get("metadata") or {}
                    if {key.lower() for key in metadata.get("reqHeaders", {})} & {"authorization", "proxy-authorization", "cookie"}:
                        return  # Never stage a credential-bearing registry cache.
    with tempfile.TemporaryFile(dir=temporary_dir) as archive:
        with tarfile.open(fileobj=archive, mode="w") as tar:
            def ownership(entry):
                if owner is not None:
                    entry.uid,entry.gid = owner
                    entry.uname,entry.gname = "",""
                return entry
            tar.add(cache, arcname="npm/_cacache", recursive=True,filter=ownership)
        archive.seek(0)
        if not container.put_archive("/opt/asb-project/.asb-cache", archive):
            raise ExecutionError("could not stage public package cache")


def stage_bootstrap(task, plan, tools, owner=None):
    """Consume declared installer assets; never infer dependencies from names."""
    if (not isinstance(plan,dict) or not {"runtime_tools","assets"}.issubset(plan)
            or set(plan)-{"runtime_tools","assets","public_package_cache","npm_package_tree","python_package_tree"}
            or type(plan.get("public_package_cache",False)) is not bool):
        raise ExecutionError("selected tool installation bootstrap plan is missing or invalid")
    runtimes,assets = plan["runtime_tools"],plan["assets"]
    if (not isinstance(runtimes,list) or len(runtimes) > 16
            or any(not isinstance(name,str) or not re.fullmatch(r"[a-z0-9][a-z0-9_.-]{0,79}",name) for name in runtimes)
            or len(set(runtimes)) != len(runtimes) or not isinstance(assets,list) or len(assets) > 64):
        raise ExecutionError("selected tool installation bootstrap plan exceeds its limits")
    staged,targets,total,native,native_receipts,native_bundles = [],set(),0,[],[],[]
    for asset in assets:
        if (not isinstance(asset,dict) or not {"source","target"}.issubset(asset)
                or set(asset)-{"source","target","native_executable"}
                or type(asset.get('native_executable',False)) is not bool):
            raise ExecutionError("invalid declared tool bootstrap asset")
        if not isinstance(asset["source"],str) or not isinstance(asset["target"],str):
            raise ExecutionError("invalid declared tool bootstrap path")
        source,target = Path(asset["source"]),Path(asset["target"])
        if (not source.is_absolute() or source.is_symlink() or not source.is_file()
                or not source.resolve().is_relative_to(tools.resolve())
                or target.is_absolute() or not target.parts or ".." in target.parts
                or any(not re.fullmatch(r"[A-Za-z0-9+_.-]{1,256}",part) for part in target.parts)
                or len(str(target)) > 1024 or str(target) in targets):
            raise ExecutionError("tool bootstrap asset escapes its project cache or guest destination")
        size = source.stat().st_size
        total += size
        if size > 256 * 1024**2 or total > 512 * 1024**2:
            raise ExecutionError("selected tool bootstrap assets exceed 512 MiB")
        targets.add(str(target))
        if asset.get('native_executable',False):
            if len(target.parts) != 3 or target.parts[1] != 'bin':
                raise ExecutionError('native bootstrap executable target is invalid')
            tool,name=target.parts[0],target.parts[2]
            native_root=f'/opt/asb-project/.asb-tools/{tool}/native-runtime'
            architecture=getattr(task.workspace,'image_identity',{}).get('architecture')
            files,receipt=bundle_tool(source,name,architecture,root=native_root)
            bundle_size=sum(len(content) for _,content,_ in files)
            total += bundle_size-size
            if total > 512 * 1024**2:
                raise ExecutionError('selected tool bootstrap assets exceed 512 MiB')
            directories=set()
            for relative,_,_ in files:
                parent=Path(relative).parent
                while str(parent) not in ('.',''):
                    directories.add(native_root+'/'+parent.as_posix())
                    parent=parent.parent
            native_bundles.append((native_root,files,directories))
            launcher=(f'#!/bin/sh\nexec {native_root}/tools/{name}/{name} "$@"\n').encode()
            staged.append((str(target),launcher,0o755))
            native.append(tool)
            native_receipts.append({'target':str(target),'files':receipt['files']})
            continue
        with source.open("rb") as incoming:
            content = incoming.read(256 * 1024**2 + 1)
        if len(content) != size:
            raise ExecutionError("tool bootstrap asset changed while it was being staged")
        staged.append((str(target),content,0o600))
    root = "/opt/asb-project/.asb-tools"
    for native_root,files,directories in native_bundles:
        task.checked(['mkdir','-p',*sorted(directories,key=lambda value:(value.count('/'),value))])
        upload(task.workspace.container,native_root,files,temporary_dir=task.artifacts,owner=owner)
    for target in sorted({str(Path(name).parent) for name,_,_ in staged}):
        task.checked(["mkdir","-p",root+"/"+target])
    if staged:
        upload(task.workspace.container,root,staged,temporary_dir=task.artifacts,owner=owner)
    receipt = {"schema_version":1,"runtime_tools":runtimes,
               "native_executables":native_receipts,
               "assets":[{"target":name,"bytes":len(content),"sha256":hashlib.sha256(content).hexdigest()}
                         for name,content,_ in staged]}
    path = task.artifacts / "bootstrap.json"
    path.write_text(json.dumps(receipt,sort_keys=True)+"\n")
    path.chmod(0o600)
    return runtimes,native


def stage_private_inputs(task, inputs, owner=None):
    """Stage runner-sealed replay inputs only into the owned private mount."""
    if not isinstance(inputs,list) or len(inputs) > 16:
        raise ExecutionError("guest private replay inputs exceed their limit")
    staged,targets,total = [],set(),0
    for item in inputs:
        if not isinstance(item,dict) or set(item) != {"source","target","bytes","sha256"}:
            raise ExecutionError("guest private replay input is invalid")
        source,target = item["source"],item["target"]
        if (not isinstance(source,str) or not isinstance(target,str) or not isinstance(item["bytes"],int)
                or not isinstance(item["sha256"],str) or not Path(source).is_absolute()
                or Path(source).is_symlink() or not Path(source).is_file()
                or not re.fullmatch(r"[A-Za-z0-9+_.-]{1,256}",target)
                or len(item["sha256"]) != 64 or not re.fullmatch(r"[0-9a-f]{64}",item["sha256"])
                or target in targets or item["bytes"] < 0
                or item["bytes"] > (64 * 1024**3 if target == 'workspace-snapshots.tar' else 64 * 1024**2)):
            raise ExecutionError("guest private replay input is invalid")
        if target != 'workspace-snapshots.tar':
            total += item['bytes']
        if total > 64 * 1024**2:
            raise ExecutionError("sealed local-tool replay transcript changed before guest staging")
        targets.add(target)
        if target == 'workspace-snapshots.tar':
            storage = getattr(task.workspace, 'snapshot_storage', None)
            if storage is None or not Path(storage).is_dir():
                raise ExecutionError('snapshot replay requires owned project storage')
            destination = Path(storage)/target
            digest = hashlib.sha256()
            count = 0
            created = False
            try:
                with open(source, 'rb') as src, destination.open('xb') as dst:
                    created = True
                    while True:
                        chunk = src.read(1024*1024)
                        if not chunk:
                            break
                        count += len(chunk)
                        if count > item['bytes']:
                            raise ExecutionError('snapshot archive changed during guest staging')
                        digest.update(chunk)
                        dst.write(chunk)
                if count != item['bytes'] or digest.hexdigest() != item['sha256']:
                    raise ExecutionError('snapshot archive changed during guest staging')
            except BaseException:
                if created:
                    destination.unlink(missing_ok=True)
                raise
        else:
            staged.append((target,source,item['bytes'],item['sha256'],0o600))
    if staged:
        upload_files(task.workspace.container,"/opt/asb-private",staged,temporary_dir=task.artifacts,owner=owner)


def stage_auxiliary_tools(task, tools, owner):
    """Stage declared host or project-local tools under the guest-private prefix."""
    if not isinstance(tools,dict):
        raise ExecutionError('guest auxiliary tools must be a name-to-path mapping')
    paths = {}
    architecture = getattr(task.workspace,'image_identity',{}).get('architecture')
    for name, source in tools.items():
        files, _ = bundle_tool(source,name,architecture,root='/opt/asb-private')
        directories = set()
        for relative,_,_ in files:
            parent = Path(relative).parent
            while str(parent) not in ('.',''):
                directories.add('/opt/asb-private/'+parent.as_posix())
                parent = parent.parent
        if directories:
            task.checked(['mkdir','-p',*sorted(directories,key=lambda value:(value.count('/'),value))])
        upload(task.workspace.container,'/opt/asb-private',files,temporary_dir=task.artifacts,owner=owner)
        paths[name] = '/opt/asb-private/tools/'+name+'/'+name
    return paths


NPM_TREE_ARCHIVE_LIMIT = 4 * 1024**3
NPM_TREE_UNPACKED_LIMIT = 4 * 1024**3
NPM_TREE_MEMBER_LIMIT = 1024**3


def stage_npm_package_tree(task, tree, tools, selected, owner=None):
    """Stage a declared, bounded npm tree without exposing a host mount.

    The Rust installation adapter owns archive construction.  This consumer
    verifies the narrow wire contract before Docker extracts the archive into
    the selected guest tool root.  No client name or package name is inferred.
    """
    if (not isinstance(tree,dict) or set(tree) != {"source","target","executable"}
            or not all(isinstance(tree[key],str) for key in tree)):
        raise ExecutionError("npm package-tree bootstrap is invalid")
    source,target,executable = Path(tree["source"]),tree["target"],tree["executable"]
    if (target != selected or not re.fullmatch(r"[a-z0-9][a-z0-9_.-]{0,79}",target)
            or not re.fullmatch(r"[A-Za-z0-9+_.-]{1,256}",executable)
            or not source.is_absolute() or source.is_symlink() or not source.is_file()
            or not source.resolve().is_relative_to(tools.resolve())):
        raise ExecutionError("npm package-tree bootstrap escapes its selected guest tool")
    if source.stat().st_size > NPM_TREE_ARCHIVE_LIMIT:
        raise ExecutionError("npm package-tree bootstrap exceeds 4 GiB")
    members,total = [],0
    try:
        with tarfile.open(source,mode="r:") as archive:
            members = archive.getmembers()
            if not 1 <= len(members) <= 100000:
                raise ExecutionError("npm package-tree bootstrap has an invalid member count")
            for member in members:
                name = member.name
                parts = Path(name).parts
                if (not name or name.startswith("/") or ".." in parts
                        or parts[0] != "node_modules"
                        or not (member.isdir() or member.isfile() or member.issym() or member.islnk())):
                    raise ExecutionError("npm package-tree bootstrap contains an unsafe member")
                if member.isfile():
                    total += member.size
                    if member.size > NPM_TREE_MEMBER_LIMIT or total > NPM_TREE_UNPACKED_LIMIT:
                        raise ExecutionError("npm package-tree bootstrap exceeds its unpacked bound")
                if member.issym() or member.islnk():
                    link = member.linkname
                    resolved = posixpath.normpath(posixpath.join(
                        posixpath.dirname(name) if member.issym() else "",link))
                    parts = Path(resolved).parts
                    if (not link or Path(link).is_absolute() or resolved in {".",".."}
                            or resolved.startswith("../") or not parts or parts[0] != "node_modules"):
                        raise ExecutionError("npm package-tree bootstrap contains an escaping link")
    except (OSError,tarfile.TarError) as error:
        raise ExecutionError("npm package-tree bootstrap cannot be inspected") from error
    with source.open("rb") as incoming:
        destination = "/opt/asb-project/.asb-tools/"+target
        task.checked(["mkdir","-p",destination])
        if not task.workspace.container.put_archive(destination,incoming):
            raise ExecutionError("could not stage npm package-tree bootstrap")
    launcher = "/opt/asb-project/.asb-tools/"+target+"/node_modules/.bin/"+executable
    task.checked(["test","-x",launcher])
    receipt = {"schema_version":1,"target":target,"executable":executable,
               "archive_bytes":source.stat().st_size,"unpacked_bytes":total,
               "members":len(members)}
    path = task.artifacts / "npm-package-tree.json"
    path.write_text(json.dumps(receipt,sort_keys=True)+"\n")
    path.chmod(0o600)


def stage_python_package_tree(task, tree, tools, selected):
    """Stage an installed Python client and a portable matching interpreter."""
    keys = {"runtime", "packages", "runtime_name", "version", "architecture", "target", "executable"}
    if not isinstance(tree, dict) or set(tree) != keys or not all(isinstance(tree[key], str) for key in keys):
        raise ExecutionError("Python package-tree bootstrap is invalid")
    target, executable, version, runtime_name = (tree[key] for key in ("target", "executable", "version", "runtime_name"))
    if (target != selected or not re.fullmatch(r"[a-z0-9][a-z0-9_.-]{0,79}", target)
            or not re.fullmatch(r"[A-Za-z0-9+_.-]{1,256}", executable)
            or version not in ("3.12", "3.13")
            or tree["architecture"] not in ("amd64", "arm64")
            or getattr(task.workspace, "image_identity", {}).get("architecture") != tree["architecture"]
            or not re.fullmatch(r"[A-Za-z0-9_.-]{1,128}", runtime_name)):
        raise ExecutionError("Python package-tree identity is invalid")
    destination = "/opt/asb-project/.asb-tools/" + target
    stage = destination + "/python-guest"
    task.checked(["mkdir", "-p", stage])
    for key, allowed in (("runtime", {runtime_name}), ("packages", {"site-packages", "bin"})):
        source = Path(tree[key])
        if (not source.is_absolute() or source.is_symlink() or not source.is_file()
                or not source.resolve().is_relative_to((tools / target).resolve())
                or source.stat().st_size > 512 * 1024**2):
            raise ExecutionError("Python package-tree archive escapes its selected guest tool")
        try:
            with tarfile.open(source, mode="r:gz") as archive:
                members, total = archive.getmembers(), 0
                if not 1 <= len(members) <= 200000:
                    raise ExecutionError("Python package-tree member count is invalid")
                for member in members:
                    parts = Path(member.name).parts
                    if (not parts or member.name.startswith("/") or ".." in parts or parts[0] not in allowed
                            or not (member.isdir() or member.isfile() or member.issym() or member.islnk())):
                        raise ExecutionError("Python package-tree contains an unsafe member")
                    if member.isfile():
                        total += member.size
                        if member.size > 512 * 1024**2 or total > 2 * 1024**3:
                            raise ExecutionError("Python package-tree exceeds its unpacked bound")
                    if member.issym() or member.islnk():
                        link = member.linkname
                        resolved = posixpath.normpath(posixpath.join(posixpath.dirname(member.name) if member.issym() else "", link))
                        if (not link or link.startswith("/") or resolved.startswith("../") or
                                not Path(resolved).parts or Path(resolved).parts[0] not in allowed):
                            raise ExecutionError("Python package-tree contains an escaping link")
        except (OSError, tarfile.TarError) as error:
            raise ExecutionError("Python package-tree archive cannot be inspected") from error
        with source.open("rb") as incoming:
            if not task.workspace.container.put_archive(stage, incoming):
                raise ExecutionError("could not stage Python package-tree bootstrap")
    interpreter = stage + "/" + runtime_name + "/bin/python" + version
    venv = destination + "/venv"
    task.checked([interpreter, "-m", "venv", "--without-pip", venv])
    task.checked(["cp", "-a", stage + "/site-packages/.", venv + "/lib/python" + version + "/site-packages/"])
    task.checked(["cp", "-a", stage + "/bin/.", venv + "/bin/"])
    # Installed console scripts retain host shebangs. Rewrite only Python
    # shebangs and leave all other files untouched.
    rewrite = "import pathlib,sys\np=pathlib.Path(sys.argv[1]); py=sys.argv[2].encode()\nfor f in p.iterdir():\n if f.is_symlink() or not f.is_file(): continue\n with f.open('rb') as h: line=h.readline(4096)\n if line.startswith(b'#!') and line.endswith(b'\\n') and b'python' in line.lower():\n  if f.stat().st_size > 16*1024*1024: raise ValueError('Python launcher exceeds limit')\n  b=f.read_bytes(); f.write_bytes(b'#!'+py+b'\\n'+b[len(line):])\n"
    task.checked([venv + "/bin/python", "-c", rewrite, venv + "/bin", venv + "/bin/python"])
    task.checked(["test", "-x", venv + "/bin/" + executable])


def prepare_agent(task, request, cancelled, resources=None, relays=None, preparation_budget=None):
    """Install the exact selected client independently of provider execution."""
    guest = request.get("request")
    if not isinstance(guest, dict):
        raise ExecutionError("private agent execution request is missing")
    tool = guest.get("config", {}).get("agent", "")
    if not re.fullmatch(r"[a-z0-9][a-z0-9_.-]{0,79}", tool):
        raise ExecutionError("selected agent tool identity is invalid")
    project = Path(request["project_root"]).absolute()
    tools = Path(request["tools_root"]).absolute()
    if not (project / ".asb/project.toml").is_file():
        raise ExecutionError("host benchmark project is not initialized")
    container = task.workspace.container
    check_budget = preparation_budget.remaining if preparation_budget else lambda:None
    check_budget()
    files,receipt = bundle(request["binary"],getattr(task.workspace,"image_identity",{}).get("architecture"))
    check_budget()
    runtime_receipt = task.artifacts / "runtime.json"
    runtime_receipt.write_text(json.dumps(receipt,sort_keys=True)+"\n")
    runtime_receipt.chmod(0o600)
    deadline = preparation_budget.deadline if preparation_budget else time.monotonic()+900
    root_checked = lambda arguments:root_execute(task,arguments,deadline=deadline,cancelled=cancelled)
    owner = stage_runtime(task,request["binary"],files,task.checked,root_checked,deadline=deadline,cancelled=cancelled)
    task_identity,ownership = prepare_guest_roots(task,owner,task.checked,root_checked)
    task.checked(["mkdir", "-m", "700", "/opt/asb-project/.asb", "/opt/asb-project/.asb-tools", "/opt/asb-project/.asb-cache"])
    check_budget()
    configuration = request.get("project_configuration", "tools_dir='.asb-tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n")
    if not isinstance(configuration, str) or len(configuration.encode()) > 64 * 1024:
        raise ExecutionError("guest project configuration exceeds its limit")
    if relays is None:
        relays = []
    def start_relay(targets):
        if resources is None:
            raise ExecutionError("local endpoint forwarding needs an owned execution scope")
        from asb_docker import docker_command
        relay = HostRelay(targets, [*docker_command(), "exec", "-i", container.id,
                                   "/opt/asb-runtime/asb", "__asb-relay"], cancelled,
                          task.artifacts / f"endpoint-relay-{len(relays)}.json")
        resources.callback(relay.close)
        relays.append(relay)
        return relay
    request = forward_request(dict(request, project_configuration=configuration), start_relay)
    configuration = request["project_configuration"]
    guest = request["request"]
    guest = dict(guest,task_identity=task_identity,owned_instrumentation=ownership)
    upload(container, "/opt/asb-project/.asb", [("project.toml", configuration.encode(), 0o600)],temporary_dir=task.artifacts,owner=owner)
    stage_private_inputs(task,request.get("private_inputs",[]),owner=owner)
    runtimes, native_tools = stage_bootstrap(task,request.get("bootstrap"),tools,owner=owner)
    package_tree = request["bootstrap"].get("npm_package_tree")
    if package_tree is not None:
        stage_npm_package_tree(task,package_tree,tools,tool,owner=owner)
    python_tree = request["bootstrap"].get("python_package_tree")
    if python_tree is not None:
        stage_python_package_tree(task,python_tree,tools,tool)
    check_budget()
    if request["bootstrap"].get("public_package_cache",False):
        stage_public_cache(container, project,temporary_dir=task.artifacts,owner=owner)
        check_budget()
    environment = dict(request.get("environment", {}))
    auxiliary = stage_auxiliary_tools(task,request.get("auxiliary_tools",{}),owner)
    if auxiliary:
        environment['ASB_AUXILIARY_TOOLS'] = json.dumps(auxiliary,sort_keys=True,separators=(',',':'))
    for selected in [*runtimes, *([] if package_tree is not None or python_tree is not None or tool in native_tools else [tool])]:
        result = task.workspace.execute(["/bin/sh", "-c", 'cd /opt/asb-project && exec /opt/asb-runtime/asb tool install "$1"', "asb", selected],
                                        environment=environment, timeout=min(900,preparation_budget.remaining()) if preparation_budget else 900, output_limit=256 * 1024,
                                        # Docker exec startup participates in the same
                                        # cohort-wide preparation queue as the later
                                        # stopped application gate. Keep the installer
                                        # command itself bounded to 15 minutes, but do
                                        # not silently restore Workspace.execute's
                                        # shorter default startup deadline here.
                                        preparation_timeout=preparation_budget.remaining() if preparation_budget else None,
                                        cancelled=ForwardingCancellation(cancelled, relays),
                                        on_preparation_failure=preparation_budget.fail if preparation_budget else None)
        check_budget()
        for relay in relays:
            relay.check()
        for stream in ["stdout", "stderr"]:
            path = task.artifacts / f"install-{selected}.{stream}"
            path.write_bytes(result[stream])
            path.chmod(0o600)
        if result["exit_code"] != 0 or result["timed_out"] or result["cancelled"]:
            # The nested CLI puts command diagnostics in its project log.
            # Export a bounded tail before the owned environment is removed.
            detail = container.exec_run(["tail", "-c", "262144", "/opt/asb-project/.asb/install.log"])
            if detail.exit_code == 0:
                path = task.artifacts / f"install-{selected}.log"
                path.write_bytes(detail.output)
                path.chmod(0o600)
            raise ExecutionError("selected agent runtime installation failed; inspect private install artifacts")
    snapshot_requested = os.environ.get('ASB_EXPERIMENTAL_SNAPSHOT') == '1'
    if snapshot_requested and not getattr(task, 'snapshot', False):
        raise ExecutionError('snapshot workspace was requested but the workload did not provision snapshot storage and a guest mount')
    if getattr(task,'snapshot',False):
        if task.tool_mode not in ('record', 'replay'):
            raise ExecutionError('experimental snapshot requires tool recording or replay')
        if resources is None or not {'btrfs','mkfs.btrfs','rsync'}.issubset(auxiliary):
            raise ExecutionError('experimental snapshot preparation needs staged filesystem tools')
        from asb_docker import docker_command
        from asb_snapshot_control import settle_on_exit
        replay = task.tool_mode == 'replay'
        if replay and not any(item['target'] == 'workspace-snapshots.tar' for item in request.get('private_inputs', [])):
            raise ExecutionError('snapshot replay archive is missing from the selected cassette')
        if replay:
            # The staging step has verified these exact bytes while copying.
            guest['snapshot_archive_sha256'] = next(item['sha256'] for item in request['private_inputs'] if item['target'] == 'workspace-snapshots.tar')
        extra_roots = experimental_snapshot_roots(os.environ.get('ASB_EXPERIMENTAL_SNAPSHOT_ROOTS'))
        from asb_snapshot_backend import BTRFS, DockerSnapshotTransport, SnapshotRequest
        supervisor = BTRFS.start(SnapshotRequest(
            task=task, tools=auxiliary, owner=owner, transport=DockerSnapshotTransport(docker_command()),
            deadline=deadline, target=getattr(task.workspace, 'workdir', '/testbed'),
            replay=replay, extra_roots=tuple(extra_roots),
            independent_custody=os.environ.get('ASB_EXPERIMENTAL_SNAPSHOT_CUSTODY') == '1'))
        resources.push(lambda exc_type, exc, traceback: settle_on_exit(supervisor, exc_type, exc, traceback))
        environment['ASB_FS_REPLAY_SOCKET' if replay else 'ASB_FS_MARKER_SOCKET']='/opt/asb-private/snapshot.sock'
        guest['snapshot_private_mount'] = True
    return guest,environment


def propagate_guest_timeout(result, observation):
    """Carry a verified principal or transport deadline into both records."""
    if result.get("timed_out") is True or result["metadata"].get("timed_out") is True:
        result["timed_out"] = True
        observation["timed_out"] = True
        result["metadata"]["timed_out"] = True
        if not result["metadata"].get("execution_failure"):
            result["metadata"]["execution_failure"] = "agent execution exceeded its time budget"


def execute_agent(task, request, before_resume, on_output, cancelled, on_ended=None, preparation_budget=None, on_preparation_failure=None,
                  on_release_accepted=None):
    # The relay spans package installation AND agent execution and is closed
    # on success, cancellation and every preparation/execution exception.
    seconds = request.get("preparation_seconds")
    if preparation_budget is None and seconds is not None:
        preparation_budget = PreparationBudget(seconds)
    if preparation_budget is not None:
        preparation_budget.on_failure = on_preparation_failure
        try:
            write_preparation_budget_artifact(task, "agent-preparation-budget.json", {
                "schema_version": 1,
                "scope": "agent-preparation-budget-v1",
                "requested_seconds": seconds,
            })
        except FileExistsError:
            pass
    upstream = cancelled
    preparation_stage = ["agent-setup"]
    if preparation_budget is not None:
        cancelled = ForwardingCancellation(lambda: preparation_budget.expired() or
            (upstream.is_set() if upstream is not None else False), [])
    with ExitStack() as resources:
        absent = object()
        previous_budget = getattr(task, "_asb_preparation_budget", absent)
        if preparation_budget is not None:
            # Workload adapters may issue several private Docker helper
            # commands while preparing the final agent. Give all of those
            # commands the one cohort-wide startup budget.
            task._asb_preparation_budget = preparation_budget
        try:
            return _execute_agent(task, request, before_resume, on_output, cancelled, resources, [], on_ended, preparation_budget, on_release_accepted,
                                  preparation_stage)
        except BaseException as error:
            if preparation_budget is not None:
                remaining = max(0.0, preparation_budget.deadline - time.monotonic())
                if isinstance(error, ExecutionError):
                    failure_kind = "execution-error"
                elif isinstance(error, KeyboardInterrupt):
                    failure_kind = "interrupted"
                elif isinstance(error, SystemExit):
                    failure_kind = "system-exit"
                else:
                    failure_kind = "unexpected-error"
                write_preparation_budget_artifact(task, "agent-preparation-failure.json", {
                    "schema_version": 1,
                    "scope": "agent-preparation-budget-failure-v1",
                    "remaining_seconds": round(remaining, 3),
                    "expired": preparation_budget.expired(),
                    "upstream_cancelled": upstream.is_set() if upstream is not None else False,
                    "failure_kind": failure_kind,
                    "stage": preparation_stage[0],
                    "failure_code": preparation_failure_code(error),
                })
                preparation_budget.fail()
            raise
        finally:
            if previous_budget is absent:
                try:
                    delattr(task, "_asb_preparation_budget")
                except AttributeError:
                    pass
            else:
                task._asb_preparation_budget = previous_budget


def isolate_snapshot_replay_network(task):
    """Disconnect only this attempt's task container before native replay."""
    container = task.workspace.container
    container.reload()
    attrs = container.attrs
    labels = attrs.get('Config', {}).get('Labels', {}) or {}
    mode = attrs.get('HostConfig', {}).get('NetworkMode')
    if (labels.get('asb.owner') != 'asb' or labels.get('asb.attempt') != task.workspace.attempt
            or not isinstance(mode, str) or mode == 'host' or mode.startswith('container:')):
        raise ExecutionError('snapshot replay requires an owned, private container network')
    networks = attrs.get('NetworkSettings', {}).get('Networks')
    if not isinstance(networks, dict):
        raise ExecutionError('snapshot replay network state is unavailable')
    for name in list(networks):
        if name != 'none':
            container.client.networks.get(name).disconnect(container)
    container.reload()
    remaining = container.attrs.get('NetworkSettings', {}).get('Networks')
    if not isinstance(remaining, dict) or any(name != 'none' for name in remaining):
        raise ExecutionError('snapshot replay container still has a routed network')
    if any(not isinstance(value, dict) or value.get('IPAddress') or value.get('GlobalIPv6Address')
           for value in remaining.values()):
        raise ExecutionError('snapshot replay container still has a routed address')


def _execute_agent(task, request, before_resume, on_output, cancelled, resources, relays, on_ended, preparation_budget, on_release_accepted,
                   preparation_stage):
    if request.get("network") and getattr(task, 'tool_mode', None) == 'replay':
        raise ExecutionError('replay cannot attach an external task network')
    preparation_stage[0] = "guest-setup"
    guest,environment = prepare_agent(task,request,cancelled,resources,relays,preparation_budget)
    # The workload may select an already-prepared runtime for the client.
    # Do this after installation so its Python and tools remain independent.
    prepare_environment = getattr(task, "agent_environment", None)
    if prepare_environment is not None:
        preparation_stage[0] = "agent-environment"
        environment = prepare_environment(environment, preparation_budget)
    if preparation_budget is not None:
        preparation_budget.remaining()
        request = dict(request, preparation_deadline=preparation_budget.deadline,
                       on_preparation_failure=preparation_budget.fail)
    # Only final client execution uses a workload-owned launcher. Install and
    # bootstrap commands remain ordinary managed Docker executions.
    prepare_transport = getattr(task,"prepare_agent_transport",None)
    preparation_stage[0] = "agent-transport"
    transport = prepare_transport(request) if prepare_transport is not None else None
    if preparation_budget is not None:preparation_budget.remaining()
    if transport is not None:
        guest = dict(guest,inherit_working_directory=transport.inherit_working_directory)
    container = task.workspace.container
    network = request.get("network")
    if network:
        # Change only the task's owned container, never host network policy.
        container.reload()
        container.client.networks.get(network).connect(container)
        for previous in list(container.attrs["NetworkSettings"]["Networks"]):
            if previous != network:
                container.client.networks.get(previous).disconnect(container)
    if getattr(task, 'snapshot', False) and task.tool_mode == 'replay':
        # The provider relay listens on loopback inside the container and uses
        # docker exec streams; it does not require a routed container network.
        isolate_snapshot_replay_network(task)
    guest = dict(guest)
    lifetime = guest.get("process_lifetime", "quiesce")
    lifetime_options = {}
    if lifetime == "through-evaluation":
        import uuid
        container.reload()
        labels = container.attrs.get("Config", {}).get("Labels", {}) or {}
        if (labels.get("asb.owner") != "asb" or labels.get("asb.attempt") != task.workspace.attempt
                or container.attrs.get("HostConfig", {}).get("PidMode") not in [None, "", "private"]):
            raise ExecutionError("retained services require this attempt's private PID namespace")
        probe = container.exec_run(["/opt/asb-runtime/asb", "__asb-namespace"])
        if probe.exit_code != 0 or len(probe.output) > 32 or not probe.output.strip().isdigit():
            raise ExecutionError("cannot identify owned workload PID namespace")
        controller_namespace = os.stat("/proc/self/ns/pid").st_ino
        guest_namespace = int(probe.output)
        if guest_namespace <= 1 or guest_namespace == controller_namespace:
            raise ExecutionError("workload services cannot outlive their client in the controller namespace")
        guest["workload_owner"] = {"controller_namespace":controller_namespace,
                                   "guest_namespace":guest_namespace, "token":uuid.uuid4().hex}
        lifetime_options["workload_owner"] = guest["workload_owner"]
    elif lifetime != "quiesce":
        raise ExecutionError("unknown workload process lifetime")
    guest["timeout_seconds"] = request.get("timeout", getattr(task, "agent_timeout", 900))
    # A cohort dispatcher may need longer than the ordinary local execution
    # observation guard before it releases every stopped agent together.  This
    # is deliberately absent for ordinary runs and bounded before it crosses
    # the guest boundary, so it cannot turn an arbitrary client timeout into
    # an unbounded setup wait.
    shared_launch_seconds = request.get("preparation_seconds")
    if shared_launch_seconds is not None:
        if (isinstance(shared_launch_seconds, bool)
                or not isinstance(shared_launch_seconds, (int, float))
                or not math.isfinite(shared_launch_seconds)
                or not 0.001 <= shared_launch_seconds <= 7200):
            raise ExecutionError("shared launch window must be between 0.001 and 7200 seconds")
        guest["shared_launch_seconds"] = shared_launch_seconds
    guest.update(project_root="/opt/asb-project", runtime_dir="/opt/asb-private", run_dir="/opt/asb-private/attempt")
    environment = dict(environment, ASB_GUEST_AGENT_REQUEST=json.dumps(guest))
    ended = False
    def finish_execution(result):
        nonlocal ended
        if not ended:
            ended = True
            if on_ended is not None:
                on_ended(result)
    if lifetime_options:
        lifetime_options["on_execution_end"] = finish_execution
    def attach(target):
        if request.get("measure_target", False):
            # The task execution lock is already held while the gate is
            # stopped. This read-only control probe must not recursively start
            # another managed workspace execution.
            probe = container.exec_run(["/opt/asb-runtime/asb", "__asb-target", str(target["namespace_pid"])])
            if probe.exit_code != 0 or len(probe.output) > 65536:
                raise ExecutionError("cannot read isolated measurement identity")
            try:
                target = dict(target, process_identity=json.loads(probe.output))
            except ValueError:
                raise ExecutionError("isolated process did not return valid measurement identity") from None
        if before_resume is not None:
            before_resume(target)
        if preparation_budget is not None:
            preparation_budget.remaining()
            preparation_budget.active = False
    output_tails = {'stdout': bytearray(), 'stderr': bytearray()}
    def agent_output(stream, chunk):
        if stream in output_tails:
            tail = output_tails[stream]
            tail.extend(chunk)
            del tail[:-256 * 1024]
        if on_output is not None:
            on_output(stream, chunk)
    preparation_stage[0] = "agent-launch-gate"
    result = task.workspace.execute(["/opt/asb-runtime/asb", "__asb-agent"], environment=environment,
                                    timeout=request.get("timeout", getattr(task, "agent_timeout", 900)), output_limit=256 * 1024,
                                    gate="application", before_resume=attach,
                                    preparation_timeout=preparation_budget.remaining() if preparation_budget else 90,
                                    on_preparation_failure=preparation_budget.fail if preparation_budget else None,
                                    on_output=agent_output, cancelled=ForwardingCancellation(cancelled, relays),
                                    transport_factory=transport, on_release_accepted=on_release_accepted,
                                    **lifetime_options)
    finish_execution(result)
    # A timed-out/cancelled owned guest is torn down together with its private
    # relay.  Preserve the primary execution outcome for evaluation instead
    # of reporting that expected teardown as a relay-disconnect failure.
    if not result.get("timed_out") and not result.get("cancelled"):
        for relay in relays:
            relay.check()
    observation = {key: result.get(key) for key in ["exit_code", "timed_out", "cancelled",
                   "output_truncated", "elapsed_seconds", "preparation_seconds", "target"]}
    try:
        if not result.get("workload_processes_retained"):
            metadata = task.checked(["cat", "/opt/asb-private/attempt/agent-result.json"], output_limit=1024 * 1024)
            result["metadata"] = json.loads(metadata)
        if not isinstance(result["metadata"], dict):
            raise ExecutionError("guest result is not a structured record")
    except (ExecutionError, ValueError):
        # Keep partial work eligible for evaluation after timeout/client death.
        # Unknown usage is never invented from text or an expected minimum.
        # Preserve the end of the private guest stderr: the bounded transport
        # prefix may omit the actual failure after a verbose agent run.
        if output_tails['stderr']:
            try:
                path = task.artifacts / 'guest-error-stderr.log'
                descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                with os.fdopen(descriptor, 'wb') as log:
                    log.write(output_tails['stderr'])
                observation['guest_error_log'] = path.name
            except OSError:
                pass
        result["metadata"] = {"schema_version": 1, "exit_code": result["exit_code"],
                              "final_response": None, "token_usage": {}, "steps": None}
    # The guest owns the principal execution clock; the outer transport now
    # allows separate settlement time, so carry the guest's timeout fact out.
    propagate_guest_timeout(result, observation)
    export_agent_artifacts(task,request.get("artifact_output"),
                           result["metadata"].get("exported_artifacts"))
    if result.get("workload_processes_retained"):
        if (not lifetime_options or result["metadata"].get("workload_owner") != guest["workload_owner"]
                or result["metadata"].get("tree_quiesced") is not False):
            raise ExecutionError("workload retained services without confirmed ownership")
        observation["agent_tree_cleanup"] = "delegated-owned-workload"
    elif result["metadata"].get("tree_quiesced") is not True:
        task.workspace.quiesce_unconfirmed_agent()
        result["metadata"]["tree_quiesced"] = True
        result["metadata"]["workload_owner"] = None
        observation["agent_tree_cleanup"] = "owned-container-reset"
    else:
        observation["agent_tree_cleanup"] = "supervisor-confirmed"
    # Metadata is available only after the existing quiescence/owner boundary.
    # A reset or carrier code alone never supplies an exact principal fact.
    exact = principal_observation(result["metadata"])
    result["metadata"].update(exact)
    result.update(exact)
    observation.update(exact)
    # A failed agent can still produce valid metadata. Preserve the diagnostic
    # tails before the snapshot supervisor settles, since settlement may fail
    # and otherwise hide the original cause from the caller.
    if result['metadata'].get('exit_code') not in (None, 0) or result.get('exit_code') not in (None, 0):
        for stream, tail in output_tails.items():
            if not tail:
                continue
            path = task.artifacts / f'guest-error-{stream}.log'
            if stream == 'stderr' and observation.get('guest_error_log'):
                continue
            try:
                descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
                with os.fdopen(descriptor, 'wb') as log:
                    log.write(tail)
                observation[f'guest_error_{stream}_log'] = path.name
            except OSError:
                pass
    journal = task.artifacts / "execution.json"
    journal.write_text(json.dumps(observation))
    journal.chmod(0o600)
    return result
