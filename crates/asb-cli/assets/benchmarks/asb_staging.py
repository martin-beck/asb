"""Owned ASB staging, shared by prepared-material capture and guest bootstrap.

Elevation creates fresh fixed prefixes; callers may also use a privileged
read-only material observer inside the owned container. Client commands keep
the container's configured user and environment.
"""
import hashlib
import io
import json
import re
import tarfile
import tempfile
import uuid

from asb_execution import ExecutionError

RUNTIME = "/opt/asb-runtime"
PREFIXES = (RUNTIME, "/opt/asb-project", "/opt/asb-private")


class StreamedInput:
    """Verify a file-backed input while the shared tar staging code reads it."""
    def __init__(self, source, size, sha256):
        self.source, self.size, self.sha256 = source, size, sha256
        self.digest, self.count = hashlib.sha256(), 0

    def read(self, size=-1):
        data = self.source.read(size)
        self.count += len(data)
        self.digest.update(data)
        return data

    def verify(self):
        if (self.count != self.size or self.digest.hexdigest() != self.sha256
                or self.source.read(1)):
            raise ExecutionError("sealed replay input changed during guest staging")


def upload(container, destination, files, temporary_dir=None, owner=None):
    if owner is not None and (not isinstance(owner,tuple) or len(owner) != 2
            or any(type(value) is not int or not 0 <= value <= 4294967295 for value in owner)):
        raise ExecutionError("invalid private guest file owner")
    with tempfile.TemporaryFile(dir=temporary_dir) as archive:
        with tarfile.open(fileobj=archive, mode="w") as tar:
            for name, content, mode in files:
                entry = tarfile.TarInfo(name)
                entry.size, entry.mode = (content.size if isinstance(content,StreamedInput) else len(content)), mode
                if owner is not None:
                    entry.uid,entry.gid = owner
                try:
                    tar.addfile(entry, content if isinstance(content,StreamedInput) else io.BytesIO(content))
                except OSError:
                    if isinstance(content,StreamedInput):
                        raise ExecutionError("sealed replay input could not be streamed for guest staging") from None
                    raise
                if isinstance(content,StreamedInput):
                    content.verify()
        archive.seek(0)
        if not container.put_archive(destination, archive):
            raise ExecutionError("could not stage the task's private runtime")


def configured_owner(task, checked):
    container = task.workspace.container
    user = container.attrs.get("Config",{}).get("User","")
    cached = getattr(task,"_staging_owner",None)
    if cached is not None:
        if cached[:2] != (container.id,user):
            raise ExecutionError("configured staging user or container changed")
        return cached[2]
    values = []
    for option in ["-u","-g"]:
        value = checked(["id",option])
        if not isinstance(value,bytes) or not re.fullmatch(rb"[0-9]{1,10}\n",value):
            raise ExecutionError("cannot identify the configured task user")
        number = int(value)
        if number > 4294967295:
            raise ExecutionError("configured task user exceeds numeric identity bounds")
        values.append(number)
    owner = tuple(values)
    task._staging_owner = (container.id,user,owner)
    return owner


def create_prefix(root_checked, path, owner):
    if path not in PREFIXES:
        raise ExecutionError("staging may create only fixed ASB instrumentation prefixes")
    # No recursive chown, existing destination, symlink ancestor or broad /opt
    # mutation is authorized. A failed partial creation remains for inspection.
    root_checked(["/bin/sh","-c",
        'test -d /opt && test ! -L /opt && test ! -e "$1" && test ! -L "$1" && '
        'mkdir -m 700 -- "$1" && chown -- "$2:$3" "$1"',
        "asb-owned-prefix",path,str(owner[0]),str(owner[1])])


def root_execute(task, arguments, *, deadline, cancelled=None):
    from asb_docker import docker_command
    from asb_material import MaterialError
    from asb_material_exec import run
    try:
        return run([*docker_command(),"exec","--user","0","-i",task.workspace.container.id,*arguments],
                   deadline=deadline,cancelled=cancelled)
    except MaterialError as error:
        raise ExecutionError(str(error)) from None


def expected_runtime(files, owner):
    expected = {name:dict(kind="file",mode=mode,bytes=len(content),
        sha256=hashlib.sha256(content).hexdigest(),uid=owner[0],gid=owner[1])
        for name,content,mode in files}
    expected.update({name:dict(kind="5",mode=0o700,uid=owner[0],gid=owner[1]) for name in ["","bin","lib"]})
    return expected


def prefix_identity(path, owner, checked):
    if path not in PREFIXES:
        raise ExecutionError("unsupported ASB instrumentation prefix")
    raw = checked([RUNTIME+"/asb","__asb-staging",path])
    if not isinstance(raw,bytes) or len(raw) > 4096:
        raise ExecutionError("instrumentation prefix returned an invalid identity")
    try:
        identity = json.loads(raw)
    except (ValueError,UnicodeError):
        raise ExecutionError("instrumentation prefix returned an invalid identity") from None
    numbers = ["parent_device","parent_inode","device","inode","uid","gid","mode"]
    if (not isinstance(identity,dict) or set(identity) != {"schema_version","scope","path",*numbers}
            or type(identity["schema_version"]) is not int or identity["schema_version"] != 1
            or identity["scope"] != "owned-asb-instrumentation-prefix-v1" or identity["path"] != path
            or any(type(identity[key]) is not int or not 0 <= identity[key] <= 2**64-1 for key in numbers)
            or (identity["uid"],identity["gid"]) != owner or identity["mode"] != 0o700):
        raise ExecutionError("instrumentation prefix does not belong to the configured task user")
    return identity


def prepare_guest_roots(task, owner, checked, root_checked):
    identity = getattr(task,"invocation",{}).get("identity",{})
    attempt = identity.get("attempt_id")
    if not isinstance(attempt,str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,128}",attempt):
        raise ExecutionError("guest staging requires the allocated task identity")
    create_prefix(root_checked,"/opt/asb-project",owner)
    create_prefix(root_checked,"/opt/asb-private",owner)
    receipt = dict(attempt=attempt,nonce=uuid.uuid4().hex,
        prefixes=[prefix_identity(path,owner,checked) for path in PREFIXES])
    upload(task.workspace.container,"/opt/asb-private",[(".asb-owner.json",
        json.dumps(receipt,sort_keys=True).encode(),0o600)],temporary_dir=task.artifacts,owner=owner)
    evidence = task.artifacts/"staging.json"
    evidence.write_text(json.dumps(receipt,sort_keys=True)+"\n")
    evidence.chmod(0o600)
    return identity,receipt


def verify_runtime(task, binary, checked, *, deadline, cancelled=None):
    from asb_material import MaterialError, tar_digest
    state = getattr(task,"_material_runtime",None)
    container = task.workspace.container
    owner = configured_owner(task,checked)
    if (state is None or state.get("container") != container.id or state.get("binary") != str(binary)
            or state.get("attempt") != getattr(task,"attempt",None) or state.get("owner") != owner):
        raise ExecutionError("prepared material helper ownership changed")
    checked(["/bin/sh","-c","test -d /opt && test ! -L /opt && test -d /opt/asb-runtime && test ! -L /opt/asb-runtime"])
    chunks,metadata = container.get_archive(RUNTIME,chunk_size=65536,encode_stream=False)
    try:
        if metadata.get("name") != "asb-runtime" or metadata.get("linkTarget"):
            raise ExecutionError("prepared material helper root was substituted")
        digest = tar_digest(chunks,"asb-runtime",deadline=deadline,cancelled=cancelled,
            expected_root_kind=b"5",expected_entries=state["expected"],max_entries=128,
            max_bytes=384*1024**2,max_wire=385*1024**2)
    except MaterialError as error:
        raise ExecutionError(str(error)) from None
    finally:
        chunks.close()
    if state["verified"] is not None and state["verified"] != digest:
        raise ExecutionError("prepared material helper metadata changed")
    state["verified"] = digest
    identity = prefix_identity(RUNTIME,owner,checked)
    if state.get("identity") is not None and state["identity"] != identity:
        raise ExecutionError("owned runtime directory identity changed")
    state["identity"] = identity
    checked(["/bin/sh","-c","test -d /opt && test ! -L /opt && test -d /opt/asb-runtime && test ! -L /opt/asb-runtime"])
    return owner


def stage_runtime(task, binary, files, checked, root_checked, *, deadline, cancelled=None):
    owner = configured_owner(task,checked)
    state = getattr(task,"_material_runtime",None)
    expected = expected_runtime(files,owner)
    if state is None:
        create_prefix(root_checked,RUNTIME,owner)
        checked(["mkdir","-m","700",RUNTIME+"/bin",RUNTIME+"/lib"])
        upload(task.workspace.container,RUNTIME,files,temporary_dir=task.artifacts,owner=owner)
        task._material_runtime = dict(container=task.workspace.container.id,binary=str(binary),
            attempt=getattr(task,"attempt",None),owner=owner,expected=expected,verified=None,identity=None)
    else:
        verify_runtime(task,binary,checked,deadline=deadline,cancelled=cancelled)
        if state.get("expected") != expected:
            raise ExecutionError("prepared runtime differs from the selected ASB executable")
        return owner
    verify_runtime(task,binary,checked,deadline=deadline,cancelled=cancelled)
    return owner
