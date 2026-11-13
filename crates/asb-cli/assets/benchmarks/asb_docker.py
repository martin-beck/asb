"""Scoped Docker SDK transport through the existing Docker CLI.

The SDK receives a private socketpair connected to `docker system dial-stdio`.
No host listener, daemon configuration, socket chmod, or group change is used.
If Docker already needs passwordless sudo, only the installed Docker binary
is elevated; the benchmark controller and dataset code remain unprivileged.
"""

import os
import ipaddress
import shutil
import signal
import socket
import subprocess
import threading
import time
from urllib.parse import urlsplit


class DockerAccessError(RuntimeError):
    pass


class DockerProbeError(DockerAccessError):
    """Access preflight failed before any transport was created."""


_API_TIMEOUT_SECONDS = 600.0
_ACCESS_PROBE_TIMEOUT_SECONDS = 120.0
_CLOSE_SECONDS = 3.0
_TERM_GRACE = 2.0
# Each SDK connection is a `docker system dial-stdio` child.  Keep a small
# reusable pool so a dense replay cohort cannot turn transient SDK concurrency
# into an unbounded population of Docker CLI processes.
_TRANSPORT_POOL_MAXSIZE = 2


class _CloseRecord:
    """One cleanup claim retains its owners even without a final state lock."""
    def __init__(self, started):
        self.started = started
        self.deadline = started + _CLOSE_SECONDS
        self.remaining = []
        self.error = None
        self.done = threading.Event()


def docker_command():
    executable = shutil.which("docker")
    if not executable:
        raise DockerProbeError("Docker CLI is missing; install Docker Engine and retry")
    environment = os.environ.copy()
    direct = [executable]
    if environment.get("ASB_WORKLOAD_TOOL_MODE") == "replay":
        # A Docker CLI probe can itself connect to a remote daemon. Resolve the
        # configured endpoint locally and pin it before the first probe.
        if environment.get("DOCKER_CONTEXT"):
            raise DockerProbeError("replay requires a local Docker endpoint; unset DOCKER_CONTEXT and set DOCKER_HOST to a local Unix socket")
        endpoint = environment.get("DOCKER_HOST")
        if not endpoint:
            try:
                endpoint = subprocess.check_output(
                    [executable, "context", "inspect", "--format", "{{.Endpoints.docker.Host}}"],
                    stdin=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    timeout=_ACCESS_PROBE_TIMEOUT_SECONDS, env=environment, text=True,
                ).strip()
            except (OSError, subprocess.SubprocessError) as error:
                raise DockerProbeError("cannot determine the local Docker endpoint for offline replay") from error
        try:
            parsed = urlsplit(endpoint)
        except ValueError as error:
            raise DockerProbeError("replay requires a valid local Docker endpoint") from error
        local = parsed.scheme == "unix" and bool(parsed.path) and not parsed.netloc
        if parsed.scheme in ("tcp", "http", "https") and parsed.hostname:
            try:
                local = ipaddress.ip_address(parsed.hostname).is_loopback
            except ValueError:
                # Do not resolve hostnames during replay: even a name spelled
                # localhost can be configured to resolve outside loopback.
                local = False
        if not local:
            raise DockerProbeError("replay requires a local Docker endpoint; prepare replay inputs before running offline")
        direct.extend(["--host", endpoint])
    timed_out = False
    # This is transport access, not an outbound provider request.
    def probe(command):
        nonlocal timed_out
        try:
            return subprocess.run(command + ["version", "--format", "{{.Server.Version}}"],
                                  stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                  stderr=subprocess.DEVNULL, timeout=_ACCESS_PROBE_TIMEOUT_SECONDS, env=environment).returncode == 0
        except subprocess.TimeoutExpired:
            timed_out = True
            return False
        except OSError:
            return False
    if probe(direct):
        return direct
    # Match the user's actual selected endpoint, not root's default context.
    try:
        endpoint = direct[2] if len(direct) == 3 else environment.get("DOCKER_HOST")
        if not endpoint:
            endpoint = subprocess.check_output(
                [executable, "context", "inspect", "--format", "{{.Endpoints.docker.Host}}"],
                stdin=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=_ACCESS_PROBE_TIMEOUT_SECONDS,
                env=environment, text=True,
            ).strip()
    except subprocess.TimeoutExpired:
        timed_out = True
        endpoint = None
    except (OSError, subprocess.SubprocessError):
        endpoint = None
    sudo = shutil.which("sudo")
    if sudo and endpoint and endpoint.startswith("unix://") and len(endpoint) <= 4096:
        elevated = [sudo, "-n", "--", executable, "--host", endpoint]
        if probe(elevated):
            return elevated
    if timed_out:
        raise DockerProbeError(
            "Docker access probe timed out; run `docker version` and check daemon responsiveness before retrying"
        )
    raise DockerProbeError(
        "Docker daemon is unavailable or permission was denied, and scoped passwordless "
        "Docker access is unavailable; run `docker info` and repair access before retrying"
    )


class StdioConnections:
    """Own only the transport children created by this controller."""
    def __init__(self, command):
        self.command = list(command)
        self.children = []
        self.lock = threading.Lock()
        self.closed = False
        self._close_requested = threading.Event()
        self._close_claim = threading.Lock()
        self._close_record = None

    def connect(self, timeout):
        if self._close_requested.is_set():
            raise DockerAccessError("Docker transport has already been closed")
        local, remote = socket.socketpair()
        local.settimeout(timeout)
        try:
            with self.lock:
                if self.closed or self._close_requested.is_set():
                    raise DockerAccessError("Docker transport has already been closed")
                # Reap completed reconnects eagerly instead of retaining an
                # unbounded history of Popen owners until session exit.
                self.children = [child for child in self.children if child.poll() is None]
                process = subprocess.Popen(
                    self.command, stdin=remote, stdout=remote, stderr=subprocess.DEVNULL,
                    start_new_session=True,
                )
                self.children.append(process)
                if self._close_requested.is_set():
                    # An in-flight spawn remains owned, but its connection is
                    # never admitted after close was requested while Popen held
                    # the lock. The caller closes local/remote endpoints below.
                    raise DockerAccessError("Docker transport closed during connection startup; exit is unconfirmed")
            return local
        except BaseException:
            local.close()
            raise
        finally:
            remote.close()

    def close(self):
        # The allowance starts BEFORE either ownership claim or the lock held
        # across Popen. A blocked concurrent spawn cannot extend this deadline.
        started = time.monotonic()
        self._close_requested.set()
        if not self._close_claim.acquire(blocking=False):
            record = self._close_record
            if record is not None and record.done.is_set():
                if record.error is not None:
                    raise DockerAccessError(record.error)
                return
            raise DockerAccessError("Docker transport cleanup is already in progress; settlement is unconfirmed")
        # This one-shot claim is intentionally never reacquired/retried. Later
        # closers may only observe its completed record, never poll/reap/signal.
        record = self._close_record = _CloseRecord(started)
        if not self.lock.acquire(timeout=max(0, record.deadline-time.monotonic())):
            # The in-flight connect still owns this list under the lock and
            # appends its eventual Child before observing close-request. Both
            # the session and this record retain the actual owner population.
            record.remaining = self.children
            record.error = "Docker transport startup lock did not settle before cleanup deadline; exit is unconfirmed"
            record.done.set()
            raise DockerAccessError(record.error)
        try:
            self.closed = True
            # Exactly one closer owns poll/reap/signal rights. Other callers
            # must not observe the detached empty list as settled, nor race a
            # cached PGID signal against this owner's actual child reap.
            record.remaining, self.children = self.children, []
        finally:
            self.lock.release()
        # One allowance for the entire population, not two seconds per child.
        # Python owns direct CLI children; the outer ManagedGate remains actual
        # descendant owner if signaling/observation is uncertain at this bound.
        deadline = record.deadline
        remaining = record.remaining
        term, force = set(), set()
        failed, interruption = False, None
        try:
            while remaining and time.monotonic() < deadline:
                for process in list(remaining):
                    if time.monotonic() >= deadline:
                        break
                    try:
                        if process.poll() is not None:
                            remaining.remove(process)
                            continue
                    except InterruptedError:
                        continue
                    except OSError:
                        failed = True
                        # An uncertain wait observation cannot authorize a
                        # cached numeric process-group signal.
                        continue
                    except BaseException as caught:
                        if interruption is None:
                            interruption = caught
                        continue
                    pending = force if time.monotonic()-started >= _TERM_GRACE else term
                    if process in pending:
                        continue
                    pending.add(process)
                    try:
                        # poll just confirmed the unreaped original leader;
                        # never signal this group after reaping its leader.
                        os.killpg(process.pid, signal.SIGKILL if pending is force else signal.SIGTERM)
                    except ProcessLookupError:
                        pass  # Only a subsequent actual reap proves exit.
                    except OSError:
                        failed = True  # Continue all independent siblings.
                    except BaseException as caught:
                        if interruption is None:
                            interruption = caught
                if remaining:
                    time.sleep(min(.01, max(0, deadline-time.monotonic())))
        except BaseException as caught:
            interruption = caught
        finally:
            # Retain exact pending owners; no Drop retry, unconditional wait,
            # per-child worker or new admission after unknown cleanup.
            # No second lock acquisition can bypass the deadline. Once the
            # initial claim detaches children, close-request excludes all new
            # writers; the exclusive record retains remaining owners even if
            # another observer holds the ordinary session lock indefinitely.
            self.children = remaining
            record.error = (
                "Docker transport exit is unconfirmed; owned controller supervision retains custody"
                if remaining else
                "Docker transport cleanup encountered an error; all owned transport children were reaped"
                if failed or interruption is not None else None)
            record.done.set()
        if interruption is not None:
            raise interruption
        if remaining:
            raise DockerAccessError("Docker transport exit is unconfirmed; owned controller supervision retains custody")
        if failed:
            raise DockerAccessError("Docker transport cleanup encountered an error; all owned transport children were reaped")


class ClosingResponseStream:
    """Own an HTTP response even when its consumer stops before the first read.

    Docker SDK's default raw stream disables socket timeouts and does not close
    the response on early generator close. Archive inspection needs both the
    configured read timeout and deterministic release of its owned connection.
    """
    def __init__(self, response, chunk_size, decode):
        self.response = response
        self.closed = False
        try:
            self.iterator = iter(response.iter_content(chunk_size, decode))
        except BaseException:
            self.closed = True
            response.close()
            raise

    def __iter__(self):
        return self

    def __next__(self):
        if self.closed:
            raise StopIteration
        try:
            return next(self.iterator)
        except BaseException:
            self.close()
            raise

    def close(self):
        if not self.closed:
            self.closed = True
            try:
                close = getattr(self.iterator, "close", None)
                if close is not None:
                    close()
            finally:
                self.response.close()


class ArchiveStreamPolicy:
    """Change archive reads only; quiet TTY/attach streams retain SDK semantics."""
    def _stream_raw_result(self, response, chunk_size=1, decode=True):
        path = urlsplit(response.url).path
        if not ("/containers/" in path and path.endswith("/archive")):
            return super()._stream_raw_result(response, chunk_size, decode)
        try:
            self._raise_for_status(response)
        except BaseException:
            response.close()
            raise
        return ClosingResponseStream(response, chunk_size, decode)


class DockerSession:
    def __init__(self):
        self.connections = None
        self.client = None

    def __enter__(self):
        import docker
        from docker.transport.unixconn import UnixHTTPAdapter, UnixHTTPConnection, UnixHTTPConnectionPool

        connections = StdioConnections(docker_command() + ["system", "dial-stdio"])
        self.connections = connections

        class BoundedAPIClient(ArchiveStreamPolicy, docker.APIClient):
            pass

        class BoundedDockerClient(docker.DockerClient):
            def __init__(self, *args, **kwargs):
                self.api = BoundedAPIClient(*args, **kwargs)

        class Connection(UnixHTTPConnection):
            def connect(self):
                self.sock = connections.connect(self.timeout)

        class Pool(UnixHTTPConnectionPool):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, **kwargs)
                # docker-py's Unix pool does not expose urllib3's `block`
                # constructor option. Set its inherited admission behavior
                # after construction so a saturated bounded pool waits for a
                # reusable transport rather than spawning another CLI tunnel.
                self.block = True

            def _new_conn(self):
                return Connection(self.base_url, "unused", self.timeout)

        class Adapter(UnixHTTPAdapter):
            def get_connection(self, url, proxies=None):
                with self.pools.lock:
                    # All requests through this adapter target the one scoped
                    # Unix socket.  Request paths differ for every Docker API
                    # operation, so using `url` here would create a separate
                    # bounded pool for each path and still admit unbounded CLI
                    # tunnel children over a long-lived controller session.
                    pool = self.pools.get("asb-scoped-unix-endpoint")
                    if pool is None:
                        pool = Pool(url, "unused", self.timeout,
                                    maxsize=_TRANSPORT_POOL_MAXSIZE)
                        self.pools["asb-scoped-unix-endpoint"] = pool
                    return pool

        try:
            # Explicit version suppresses constructor-time I/O before mounting
            # the transport. The unversioned /version request then negotiates
            # the actual daemon version, including newer minimum API versions.
            self.client = BoundedDockerClient(base_url="unix:///unused", version="1.41", timeout=_API_TIMEOUT_SECONDS)
            self.client.api.mount("http+docker://", Adapter("http+unix:///unused", timeout=_API_TIMEOUT_SECONDS))
            version = self.client.api.version(api_version=False)["ApiVersion"]
            self.client.api._version = version
            self.client.ping()
            return self.client
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def __exit__(self, *_exception):
        client,self.client = self.client,None
        connections,self.connections = self.connections,None
        try:
            if client is not None:
                client.close()
        finally:
            if connections is not None:
                connections.close()
