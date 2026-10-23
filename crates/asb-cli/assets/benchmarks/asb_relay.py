"""Owned raw TCP forwarding for host-loopback endpoints in task containers.

Only the already-selected endpoint/proxy destinations are forwarded. No host
listener, host networking, provider dispatch or credential discovery is used.
"""
import copy
import ipaddress
import json
import os
import select
import socket
import struct
import subprocess
import threading
import time
import tomllib
from urllib.parse import urlsplit, urlunsplit

from asb_execution import ExecutionError

OPEN, DATA, EOF, CLOSE, READY = range(1, 6)
MAX_FRAME, MAX_CHANNELS, MAX_DIAGNOSTIC = 65536, 128, 8192
PROXY_NAMES = ("http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY",
               "all_proxy", "ALL_PROXY", "socks_proxy", "SOCKS_PROXY")


class ForwardingCancellation:
    """Interrupt an install/client promptly if its private transport is lost."""
    def __init__(self, upstream, relays):
        self.upstream, self.relays = upstream, relays

    def is_set(self):
        upstream = (self.upstream() if callable(self.upstream) else
                    self.upstream.is_set() if self.upstream is not None else False)
        return upstream or any(relay.stop.is_set() for relay in self.relays)


def loopback_target(url):
    """Return only explicit loopback destinations; never resolve arbitrary DNS."""
    try:
        parts = urlsplit(url)
        host = parts.hostname
        if not host:
            return None
        if host != "localhost":
            try:
                if not ipaddress.ip_address(host).is_loopback:
                    return None
            except ValueError:
                return None
        defaults = {"http": 80, "https": 443, "socks": 1080, "socks4": 1080,
                    "socks4a": 1080, "socks5": 1080, "socks5h": 1080}
        port = defaults.get(parts.scheme) if parts.port is None else parts.port
        if parts.scheme not in defaults or not port or not 1 <= port <= 65535:
            raise ValueError()
        return host, port
    except (TypeError, ValueError):
        raise ExecutionError("a local endpoint or proxy has an invalid URL") from None


def replace_port(url, port):
    parts = urlsplit(url)
    # Preserve scheme, TLS hostname, userinfo, escaped path/query and fragment.
    host = parts.hostname
    authority = ("[" + host + "]") if ":" in host else host
    if "@" in parts.netloc:
        authority = parts.netloc.rsplit("@", 1)[0] + "@" + authority
    return urlunsplit(parts._replace(netloc=authority + ":" + str(port)))


def project_toml(document):
    """Encode the string/bool/integer/tables in typed ProjectConfig, not URLs by substitution."""
    lines = []
    def table(values, prefix):
        if prefix:
            lines.append("[" + ".".join(json.dumps(key) for key in prefix) + "]")
        for key, value in values.items():
            if value is not None and not isinstance(value, dict):
                if not isinstance(value, (str, bool, int)):
                    raise ExecutionError("unsupported guest project configuration value")
                lines.append(json.dumps(key) + " = " + json.dumps(value, ensure_ascii=False))
        for key, value in values.items():
            if isinstance(value, dict):
                table(value, [*prefix, key])
    table(document, [])
    return "\n".join(lines) + "\n"


class HostRelay:
    """One owned exec process, bounded channels and buffers; no HTTP interpretation."""
    def __init__(self, targets, command, cancelled=lambda: False, diagnostic_path=None):
        if not 1 <= len(targets) <= 64 or any(
                loopback_target("http://" + ("[" + host + "]" if ":" in host else host)
                                + ":" + str(port)) != (host, port) for host, port in targets):
            raise ExecutionError("relay destinations must be bounded explicit loopback addresses")
        self.targets = targets
        self.cancelled = (cancelled if callable(cancelled) else
                          cancelled.is_set if cancelled is not None else lambda: False)
        self.stop, self.ready = threading.Event(), threading.Event()
        self.lock, self.write_lock = threading.Lock(), threading.Lock()
        self.channels, self.workers, self.ports, self.error = {}, [], None, None
        self.diagnostic_path, self.stderr_tail = diagnostic_path, bytearray()
        self.stderr_lock, self.stderr_done = threading.Lock(), threading.Event()
        self.cancelled_by_owner = False
        addresses = ["127.0.0.1" if host == "localhost" else host for host, _ in targets]
        self.process = subprocess.Popen([*command, str(len(targets)), *addresses],
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, bufsize=0, start_new_session=True)
        os.set_blocking(self.process.stdin.fileno(), False)
        self.stderr_reader = threading.Thread(target=self.drain_stderr, daemon=True)
        self.stderr_reader.start()
        self.reader = threading.Thread(target=self.run, daemon=True)
        self.reader.start()
        if not self.ready.wait(10) or self.error or self.ports is None:
            self.close()
            raise ExecutionError("private endpoint relay did not become ready")

    def drain_stderr(self):
        """Drain stderr continuously, retaining only a safe bounded tail."""
        try:
            while True:
                chunk = os.read(self.process.stderr.fileno(), 4096)
                if not chunk:
                    return
                with self.stderr_lock:
                    self.stderr_tail.extend(chunk)
                    if len(self.stderr_tail) > MAX_DIAGNOSTIC:
                        del self.stderr_tail[:-MAX_DIAGNOSTIC]
        except OSError:
            pass
        finally:
            self.stderr_done.set()

    def diagnostic(self):
        with self.stderr_lock:
            stderr = bytes(self.stderr_tail).decode("utf-8", "replace").strip()
        status = self.process.poll()
        return {"schema_version": 1, "component": "private-endpoint-relay",
                "reason": self.error, "cancelled_by_owner": self.cancelled_by_owner,
                "exit_code": status if isinstance(status, int) else None,
                "stderr_tail": stderr or None}

    def save_diagnostic(self):
        if self.diagnostic_path is None:
            return
        try:
            self.diagnostic_path.write_text(json.dumps(self.diagnostic(), sort_keys=True) + "\n")
            self.diagnostic_path.chmod(0o600)
        except OSError:
            # Diagnostics must not mask the execution's original result.
            pass

    def read(self, count):
        data = bytearray()
        while len(data) < count:
            if self.stop.is_set() or self.cancelled():
                raise EOFError()
            ready, _, _ = select.select([self.process.stdout], [], [], .1)
            if ready:
                chunk = os.read(self.process.stdout.fileno(), count - len(data))
                if not chunk:
                    raise EOFError()
                data.extend(chunk)
        return bytes(data)

    def send(self, kind, channel, payload=b""):
        data = struct.pack("!BII", kind, channel, len(payload)) + payload
        deadline = time.monotonic() + 5
        try:
            with self.write_lock:
                while data:
                    if self.stop.is_set() or self.cancelled() or time.monotonic() >= deadline:
                        raise EOFError()
                    _, ready, _ = select.select([], [self.process.stdin], [], .1)
                    if ready:
                        try:
                            written = os.write(self.process.stdin.fileno(), data)
                        except BlockingIOError:
                            continue
                        data = data[written:]
        except (OSError, EOFError):
            # A partial frame cannot be retried on a multiplexed stream: stop
            # the whole transport instead of corrupting another channel.
            self.error = "private endpoint relay write failed"
            self.stop.set()
            raise

    def disconnect(self, channel, notify=False):
        with self.lock:
            state = self.channels.pop(channel, None)
        if state:
            try:
                state["socket"].shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            state["socket"].close()
        if notify and not self.stop.is_set():
            self.send(CLOSE, channel)

    def upstream(self, channel, state):
        connection = state["socket"]
        try:
            while not self.stop.is_set() and not self.cancelled():
                try:
                    data = connection.recv(MAX_FRAME)
                except socket.timeout:
                    continue
                if not data:
                    self.send(EOF, channel)
                    with self.lock:
                        state["read_eof"] = True
                        complete = state["write_eof"]
                    if complete:
                        self.disconnect(channel, notify=True)
                    return
                self.send(DATA, channel, data)
        except (OSError, EOFError):
            try:
                self.disconnect(channel, notify=True)
            except (OSError, EOFError):
                pass

    def connect(self, channel, target):
        if not 0 <= target < len(self.targets):
            raise ValueError()
        self.workers = [worker for worker in self.workers if worker.is_alive()]
        with self.lock:
            if channel in self.channels:
                raise ValueError()
            if len(self.channels) >= MAX_CHANNELS or len(self.workers) >= MAX_CHANNELS:
                self.send(CLOSE, channel)
                return
        host, port = self.targets[target]
        # localhost is never sent to DNS: preserve support for IPv6-only servers.
        addresses = ["127.0.0.1", "::1"] if host == "localhost" else [host]
        connection = None
        for address in addresses:
            try:
                connection = socket.create_connection((address, port), timeout=2)
                break
            except OSError:
                continue
        if connection is None:
            self.send(CLOSE, channel)
            return
        connection.settimeout(2)
        state = {"socket": connection, "read_eof": False, "write_eof": False}
        with self.lock:
            self.channels[channel] = state
        worker = threading.Thread(target=self.upstream, args=(channel, state), daemon=True)
        self.workers.append(worker)
        worker.start()

    def run(self):
        try:
            while not self.stop.is_set():
                kind, channel, length = struct.unpack("!BII", self.read(9))
                if length > MAX_FRAME:
                    raise ValueError()
                payload = self.read(length)
                if self.ports is None:
                    if (kind, channel, length) != (READY, 0, len(self.targets) * 2):
                        raise ValueError()
                    self.ports = struct.unpack("!" + "H" * len(self.targets), payload)
                    if 0 in self.ports:
                        raise ValueError()
                    self.ready.set()
                    continue
                if channel == 0 or kind not in (OPEN, DATA, EOF, CLOSE):
                    raise ValueError()
                if kind == OPEN:
                    if length != 4:
                        raise ValueError()
                    self.connect(channel, struct.unpack("!I", payload)[0])
                elif kind in (EOF, CLOSE) and length:
                    raise ValueError()
                elif kind == CLOSE:
                    self.disconnect(channel)
                else:
                    with self.lock:
                        state = self.channels.get(channel)
                    if state:
                        try:
                            if kind == DATA:
                                state["socket"].sendall(payload)
                            else:
                                state["socket"].shutdown(socket.SHUT_WR)
                                with self.lock:
                                    state["write_eof"] = True
                                    complete = state["read_eof"]
                                if complete:
                                    self.disconnect(channel, notify=True)
                        except OSError:
                            self.disconnect(channel, notify=True)
        except (OSError, EOFError, ValueError) as error:
            if self.cancelled():
                # The owner cancellation is already reported by the workload
                # execution. Do not mislabel it as a relay transport failure.
                self.cancelled_by_owner = True
            elif not self.stop.is_set():
                if isinstance(error, ValueError):
                    self.error = "relay protocol rejected a frame"
                elif isinstance(error, OSError):
                    self.error = "relay control stream I/O failed"
                else:
                    self.error = "relay control stream closed"
        finally:
            self.stop.set()
            self.ready.set()
            with self.lock:
                channels = list(self.channels)
            for channel in channels:
                self.disconnect(channel)
            self.save_diagnostic()

    def check(self):
        if self.error:
            diagnostic = self.diagnostic()
            detail = diagnostic["reason"]
            if diagnostic["exit_code"] is not None:
                detail += f" (relay exit {diagnostic['exit_code']})"
            raise ExecutionError("private endpoint relay disconnected during the attempt: " + detail)
        if self.stop.is_set():
            if self.cancelled_by_owner:
                raise ExecutionError("private endpoint relay stopped because the attempt was cancelled")
            raise ExecutionError("private endpoint relay stopped unexpectedly during the attempt")

    def close(self):
        self.stop.set()
        with self.lock:
            channels = list(self.channels)
        for channel in channels:
            self.disconnect(channel)
        self.reader.join(timeout=5)
        if not self.reader.is_alive() and not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=2)
        for worker in self.workers:
            worker.join(timeout=1)
        self.stderr_reader.join(timeout=1)
        self.save_diagnostic()
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            if not stream.closed:
                stream.close()


def forward_request(request, start_relay):
    """Rewrite all local endpoint/proxy ports in a private copy before install."""
    request = copy.deepcopy(request)
    configuration = request.get("project_configuration", "")
    try:
        project = tomllib.loads(configuration)
    except (TypeError, ValueError):
        raise ExecutionError("invalid guest project configuration") from None
    connection = request.get("request", {}).get("connection") or {}
    environment = request.setdefault("environment", {})
    proxy = project.get("proxy", {})
    fields = [(connection, "base_url")]
    fields += [(environment, key) for key in PROXY_NAMES]
    fields += [(proxy, key) for key in ("http", "https", "socks")]
    targets, selections = [], []
    for owner, key in fields:
        value = owner.get(key)
        if value:
            target = loopback_target(value)
            if target:
                if target not in targets:
                    targets.append(target)
                selections.append((owner, key, targets.index(target)))
    if targets:
        relay = start_relay(targets)
        for owner, key, index in selections:
            owner[key] = replace_port(owner[key], relay.ports[index])
        request["project_configuration"] = project_toml(project)
    return request
