"""Bounded, keyed content identities. Never emits input data or secret hashes."""
import hashlib
import hmac
import json
import os
from pathlib import Path
import stat
import sys
import time

MAX_BYTES = 64 * 1024**3
# A fully provisioned project containing every supported local agent includes
# large Node/Python dependency trees. Keep sealing bounded, but allow that
# supported configuration to be admitted rather than silently losing its
# continuation checkpoint.
MAX_NODES = 2000000
MAX_REQUEST = 16 * 1024**2
MAX_ROOTS = 100000


class FingerprintError(ValueError):
    pass


def absolute_path(path):
    # Preserve '..': the kernel resolves it after any preceding symlink.
    # abspath/normpath can therefore name a different actual input file.
    path = Path(path)
    return path if path.is_absolute() else Path.cwd() / path


class Snapshot:
    def __init__(self, key, timeout=120, max_bytes=MAX_BYTES, max_nodes=MAX_NODES):
        if len(key) != 32:
            raise FingerprintError("invalid private fingerprint key")
        self.key, self.deadline = key, time.monotonic() + timeout
        self.max_bytes, self.max_nodes = max_bytes, max_nodes
        self.bytes = self.nodes = 0
        self.memo, self.observed, self.followed, self.active = {}, {}, {}, set()

    def tick(self):
        self.nodes += 1
        if self.nodes > self.max_nodes or time.monotonic() > self.deadline:
            raise FingerprintError("fingerprint resource bound exceeded")

    @staticmethod
    def signature(info):
        return (info.st_dev, info.st_ino, info.st_mode, info.st_size, info.st_mtime_ns, info.st_ctime_ns)

    def observe(self, path):
        try:
            info = path.lstat()
            value = self.signature(info)
        except FileNotFoundError:
            info, value = None, None
        old = self.observed.setdefault(path, value)
        if old != value:
            raise FingerprintError("input changed during fingerprinting")
        return info

    def path(self, path, depth=0, exclude=()):
        self.tick()
        if depth > 128:
            raise FingerprintError("input path depth exceeded")
        path = absolute_path(path)
        info = self.observe(path)
        if info is None:
            return hashlib.sha256(b"missing").digest()
        identity = (path, tuple(exclude))
        if identity in self.active:
            raise FingerprintError("input symlink cycle")
        if identity in self.memo:
            return self.memo[identity]
        self.active.add(identity)
        digest = hashlib.sha256()
        digest.update(str(stat.S_IMODE(info.st_mode)).encode() + b"\0")
        if stat.S_ISLNK(info.st_mode):
            target = os.readlink(path)
            digest.update(b"link\0" + os.fsencode(target) + b"\0")
            digest.update(self.path(path.parent / target, depth + 1, exclude))
        elif stat.S_ISREG(info.st_mode):
            digest.update(b"file\0")
            # A file may be replaced after lstat. Do not follow the new
            # symlink or block indefinitely opening a substituted FIFO.
            descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK | getattr(os, "O_NOFOLLOW", 0))
            with os.fdopen(descriptor, "rb") as source:
                if self.signature(os.fstat(source.fileno())) != self.signature(info):
                    raise FingerprintError("input was replaced before reading")
                while chunk := source.read(1024**2):
                    self.bytes += len(chunk)
                    if self.bytes > self.max_bytes:
                        raise FingerprintError("fingerprint byte bound exceeded")
                    self.tick()
                    digest.update(chunk)
                if self.signature(os.fstat(source.fileno())) != self.signature(info):
                    raise FingerprintError("input changed while reading")
        elif stat.S_ISDIR(info.st_mode):
            digest.update(b"directory\0")
            for child in sorted(path.iterdir(), key=lambda value: os.fsencode(value.name)):
                if child.name in exclude:
                    continue
                digest.update(os.fsencode(child.name) + b"\0")
                digest.update(self.path(child, depth + 1, exclude))
        else:
            raise FingerprintError("input is not a regular file, directory or symlink")
        self.active.remove(identity)
        value = digest.digest()
        self.memo[identity] = value
        return value

    def file_contents(self, path):
        """Read a kernel-backed file handle without trusting its pathname.

        /proc/PID/exe remains readable after executable replacement even when
        readlink() names an unlinked file. Its PID/path is not software identity.
        """
        self.tick()
        path = Path(path)
        before = path.stat()
        if not stat.S_ISREG(before.st_mode):
            raise FingerprintError("direct input is not a regular file")
        signature = self.signature(before)
        if self.followed.setdefault(path, signature) != signature:
            raise FingerprintError("direct input changed between reads")
        digest = hashlib.sha256(b"direct-file\0" + str(stat.S_IMODE(before.st_mode)).encode() + b"\0")
        descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
        with os.fdopen(descriptor, "rb") as source:
            if self.signature(os.fstat(source.fileno())) != self.signature(before):
                raise FingerprintError("direct file was replaced before reading")
            while chunk := source.read(1024**2):
                self.bytes += len(chunk)
                self.tick()
                if self.bytes > self.max_bytes:
                    raise FingerprintError("fingerprint byte bound exceeded")
                digest.update(chunk)
            if self.signature(os.fstat(source.fileno())) != self.signature(before):
                raise FingerprintError("direct file changed while reading")
        return digest.digest()

    def seal(self, roots, context, exclude=()):
        digest = hmac.new(self.key, digestmod=hashlib.sha256)
        digest.update(b"asb-content-snapshot-v1\0")
        encoded_context = json.dumps(context, sort_keys=True, separators=(",", ":"), ensure_ascii=True).encode()
        digest.update(len(encoded_context).to_bytes(8, "big"))
        digest.update(encoded_context)
        names = set()
        for entry in sorted(roots, key=lambda entry: entry["name"]):
            if (not {"name", "path"} <= set(entry) or set(entry) - {"name", "path", "bind_location", "direct_file"}
                    or not isinstance(entry["name"], str)
                    or not entry["name"] or "\0" in entry["name"] or entry["name"] in names):
                raise FingerprintError("invalid or duplicate input root name")
            names.add(entry["name"])
            digest.update(entry["name"].encode() + b"\0")
            # Physical roots matter for one-project continuation, not relocation.
            bind_location, direct_file = entry.get("bind_location", True), entry.get("direct_file", False)
            if type(bind_location) is not bool or type(direct_file) is not bool:
                raise FingerprintError("invalid input root semantics")
            if bind_location:
                digest.update(os.fsencode(absolute_path(entry["path"])) + b"\0")
            digest.update(self.file_contents(entry["path"]) if direct_file else self.path(entry["path"], exclude=exclude))
        for path, before in self.observed.items():
            try:
                after = self.signature(path.lstat())
            except FileNotFoundError:
                after = None
            if before != after:
                raise FingerprintError("input changed before snapshot completion")
        for path, before in self.followed.items():
            if self.signature(path.stat()) != before:
                raise FingerprintError("direct input changed before snapshot completion")
        return {"schema_version": 1, "digest": digest.hexdigest(), "bytes": self.bytes, "nodes": self.nodes}


def fingerprint(request, environment):
    if not isinstance(request, dict) or set(request) - {"key", "roots", "context", "environment", "exclude"}:
        raise FingerprintError("invalid fingerprint request")
    key = bytes.fromhex(request["key"])
    context = request.get("context")
    if request.get("environment", False):
        # Shell bookkeeping is not a benchmark input. All other values,
        # including credentials and endpoint/proxy overrides, remain only
        # inside the keyed digest and never enter the response or a log.
        values = {name: value for name, value in environment.items()
                  if name not in {"_", "PWD", "OLDPWD", "SHLVL"}}
        context = {"context": context, "environment": values}
    roots = request["roots"]
    if not isinstance(roots, list) or len(roots) > MAX_ROOTS:
        raise FingerprintError("invalid input root population")
    exclude = request.get("exclude", [])
    if not isinstance(exclude, list) or any(not isinstance(name, str) or "/" in name or "\\" in name for name in exclude):
        raise FingerprintError("invalid housekeeping exclusions")
    return Snapshot(key).seal(roots, context, tuple(exclude))


def main():
    try:
        content = sys.stdin.buffer.read(MAX_REQUEST + 1)
        if len(content) > MAX_REQUEST:
            raise FingerprintError("request size exceeded")
        response = fingerprint(json.loads(content), dict(os.environ))
        print(json.dumps(response))
        return 0
    except Exception as error:
        # Paths, context, key, contents and exception text may contain secrets.
        # Only the exception class can cross this private process boundary.
        print(json.dumps({"schema_version":1, "error_class":type(error).__name__}))
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
