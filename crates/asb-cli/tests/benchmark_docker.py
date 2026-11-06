"""Socketpair transport tests: no Docker, network or elevated access."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import threading
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch

sys.dont_write_bytecode = True
SOURCE = Path(__file__).parents[1] / "assets/benchmarks/asb_docker.py"
SPEC = importlib.util.spec_from_file_location("asb_docker", SOURCE)
RUNTIME = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNTIME)


class ConnectionTests(unittest.TestCase):
    def test_docker_access_probe_allows_slow_success_and_reports_timeout(self):
        with patch.object(RUNTIME.shutil, "which", side_effect=lambda name: "/bin/docker" if name == "docker" else None), \
             patch.object(RUNTIME.subprocess, "run", return_value=SimpleNamespace(returncode=0)) as run:
            self.assertEqual(RUNTIME.docker_command(), ["/bin/docker"])
            self.assertEqual(run.call_args.kwargs["timeout"], 120.0)
        with patch.dict(RUNTIME.os.environ, {"DOCKER_HOST": "unix:///run/docker.sock"}), \
             patch.object(RUNTIME.shutil, "which", side_effect=lambda name: "/bin/docker" if name == "docker" else None), \
             patch.object(RUNTIME.subprocess, "run", side_effect=subprocess.TimeoutExpired("docker", 120)):
            with self.assertRaisesRegex(RUNTIME.DockerAccessError, "probe timed out"):
                RUNTIME.docker_command()
        with patch.dict(RUNTIME.os.environ, {"DOCKER_HOST": "unix:///run/docker.sock"}), \
             patch.object(RUNTIME.shutil, "which", side_effect=lambda name: "/bin/docker" if name == "docker" else None), \
             patch.object(RUNTIME.subprocess, "run", return_value=SimpleNamespace(returncode=1)):
            with self.assertRaisesRegex(RUNTIME.DockerAccessError, "permission was denied"):
                RUNTIME.docker_command()
        with patch.dict(RUNTIME.os.environ, {"DOCKER_HOST": "unix:///run/docker.sock"}), \
             patch.object(RUNTIME.shutil, "which", side_effect=lambda name: "/bin/" + name), \
             patch.object(RUNTIME.subprocess, "run", side_effect=[SimpleNamespace(returncode=1), SimpleNamespace(returncode=0)]):
            self.assertEqual(RUNTIME.docker_command(), ["/bin/sudo", "-n", "--", "/bin/docker", "--host", "unix:///run/docker.sock"])

    def test_docker_negotiates_on_unversioned_endpoint_before_ping(self):
        try:
            import docker
        except ImportError:
            self.skipTest("Docker SDK is not installed")
        requests = []
        def get(client, url, **_kwargs):
            requests.append(url)
            response = SimpleNamespace(status_code=200, json=lambda: {"ApiVersion": "1.52"})
            response.raise_for_status = lambda: None
            return response
        transport = SimpleNamespace(close=lambda: None)
        with patch.object(RUNTIME, "StdioConnections", return_value=transport), \
             patch.object(docker.APIClient, "_get", get), \
             patch.object(docker.APIClient, "ping", return_value=True):
            with RUNTIME.DockerSession() as client:
                self.assertEqual(client.api._version, "1.52")
                self.assertEqual(client.api.timeout, 600.0)
                self.assertEqual(client.api.get_adapter("http+docker://localhost").timeout, 600.0)
        self.assertEqual(requests, ["http+docker://localhost/version"])

    def test_blocked_inflight_spawn_cannot_extend_close_budget_or_lose_its_eventual_child(self):
        transport = RUNTIME.StdioConnections([sys.executable,"-c","import sys;sys.stdin.buffer.read()"])
        unrelated = subprocess.Popen([sys.executable,"-c","import time;time.sleep(30)"])
        entered, release = threading.Event(), threading.Event()
        original = RUNTIME.subprocess.Popen
        errors = []
        def controlled_spawn(*args,**kwargs):
            entered.set()
            if not release.wait(2):raise RuntimeError("controlled spawn barrier deadline")
            return original(*args,**kwargs)
        def connect():
            try:
                connection = transport.connect(1)
                connection.close()
            except BaseException as error:errors.append(type(error).__name__)
        worker = threading.Thread(target=connect)
        try:
            with patch.object(RUNTIME.subprocess,"Popen",side_effect=controlled_spawn), patch.object(RUNTIME,"_CLOSE_SECONDS",.1):
                worker.start()
                try:
                    self.assertTrue(entered.wait(2))
                    started = time.monotonic()
                    with self.assertRaisesRegex(RUNTIME.DockerAccessError,"startup lock.*unconfirmed"):
                        transport.close()
                    elapsed = time.monotonic()-started
                    requested = transport._close_requested.is_set()
                    with self.assertRaisesRegex(RUNTIME.DockerAccessError,"already been closed"):
                        transport.connect(1)
                finally:
                    release.set()
                    worker.join(timeout=4)
            self.assertFalse(worker.is_alive())
            self.assertEqual(len(transport.children),1)
            child = transport.children[0]
            # Parent endpoints close when in-flight connect observes request;
            # the real child then sees EOF and exits without any invented reap.
            child.wait(timeout=3)
            retained = transport._close_record.remaining is transport.children
            sentinel_survived = unrelated.poll() is None
            unrelated.kill()
            unrelated.wait(timeout=3)
            with patch.object(RUNTIME.os,"killpg",side_effect=AssertionError("signal after actual reap")) as signal:
                with self.assertRaisesRegex(RUNTIME.DockerAccessError,"unconfirmed"):
                    transport.close()
                signal.assert_not_called()
            self.assertTrue(requested and retained and sentinel_survived)
            self.assertLess(elapsed,.7)
            self.assertEqual(errors,["DockerAccessError"])
        finally:
            release.set()
            if worker.ident is not None:worker.join(timeout=4)
            if not worker.is_alive():
                for child in transport.children:
                    if child.poll() is None:child.kill()
                    child.wait(timeout=3)
            if unrelated.poll() is None:unrelated.kill()
            unrelated.wait(timeout=3)

    def test_concurrent_close_has_one_cleanup_owner_and_never_claims_pending_settlement(self):
        transport = RUNTIME.StdioConnections([sys.executable,"-c","import time;time.sleep(30)"])
        unrelated = subprocess.Popen([sys.executable,"-c","import time;time.sleep(30)"])
        connection = None
        entered, release = threading.Event(), threading.Event()
        calls, errors = [], []
        original = RUNTIME.os.killpg
        worker = None
        try:
            connection = transport.connect(1)
            child = transport.children[0]
            def held_signal(pid, sig):
                if pid == child.pid:
                    calls.append(threading.get_ident())
                    entered.set()
                    if not release.wait(1):
                        raise RuntimeError("owned close barrier deadline")
                return original(pid, sig)
            def close_owner():
                try:transport.close()
                except BaseException as error:errors.append(type(error).__name__)
            with patch.object(RUNTIME.os,"killpg",side_effect=held_signal):
                worker = threading.Thread(target=close_owner)
                worker.start()
                try:
                    self.assertTrue(entered.wait(2))
                    before = len(calls)
                    with self.assertRaisesRegex(RUNTIME.DockerAccessError,"in progress.*unconfirmed"):
                        transport.close()
                    self.assertEqual(len(calls),before)
                finally:
                    release.set()
                    worker.join(timeout=4)
            self.assertFalse(worker.is_alive())
            self.assertEqual(errors,[])
            self.assertTrue(calls)
            self.assertEqual(set(calls),{worker.ident})
            self.assertIsNotNone(child.poll())
            self.assertEqual(transport.children,[])
            self.assertIsNone(unrelated.poll())
            transport.close()  # Completed normal close stays idempotent.
        finally:
            release.set()
            if worker is not None:worker.join(timeout=4)
            if connection is not None:connection.close()
            # The sole cleanup worker has completed before any fallback signal.
            if worker is None or not worker.is_alive():
                for child in transport.children:
                    if child.poll() is None:child.kill()
                    child.wait(timeout=3)
            unrelated.kill()
            unrelated.wait(timeout=3)

    def test_unknown_close_is_sticky_and_cannot_restart_cleanup_or_signal_after_reap(self):
        transport = RUNTIME.StdioConnections([sys.executable,"-c","import time;time.sleep(30)"])
        connection = transport.connect(1)
        child = transport.children[0]
        original_poll = child.poll
        try:
            with patch.object(child,"poll",return_value=None), patch.object(RUNTIME.os,"killpg",side_effect=PermissionError):
                with self.assertRaisesRegex(RUNTIME.DockerAccessError,"unconfirmed"):
                    transport.close()
            # Restore the real wait owner and actually reap before repeated
            # close: a cached PID/PGID must NEVER be signalled by that repeat.
            if original_poll() is None:child.kill()
            child.wait(timeout=3)
            with patch.object(RUNTIME.os,"killpg",side_effect=AssertionError("cached group signal after reap")) as signal:
                started = time.monotonic()
                with self.assertRaisesRegex(RUNTIME.DockerAccessError,"unconfirmed"):
                    transport.close()
                self.assertLess(time.monotonic()-started,.2)
                signal.assert_not_called()
        finally:
            connection.close()
            if original_poll() is None:child.kill()
            child.wait(timeout=3)

    def test_archive_policy_preserves_quiet_tty_stream_semantics(self):
        class SDK:
            def _stream_raw_result(self,response,chunk_size,decode):
                response.timeout = None  # Upstream's deliberate TTY behavior.
                return "upstream-stream"
            def _raise_for_status(self,response):
                if response.failed:raise RuntimeError("fixture HTTP failure")
        class Client(RUNTIME.ArchiveStreamPolicy,SDK):pass
        for path,archive in [("/v1.52/containers/fixture/archive?path=/",True),
                             ("/v1.52/containers/fixture/logs",False),
                             ("/v1.52/containers/fixture/attach?path=/archive",False),
                             ("/v1.52/images/get",False)]:
            response = SimpleNamespace(url="http+docker://localhost"+path,timeout=120,failed=False,
                closed=False,iter_content=lambda *_:iter([b"fixture"]))
            response.close = lambda:setattr(response,"closed",True)
            stream = Client()._stream_raw_result(response,512,False)
            if archive:
                self.assertEqual(response.timeout,120)
                self.assertEqual(list(stream),[b"fixture"])
                self.assertTrue(response.closed)
            else:
                self.assertEqual(stream,"upstream-stream")
                self.assertIsNone(response.timeout)
                self.assertFalse(response.closed)
        response.failed = True
        response.url = "http+docker://localhost/v1.52/containers/fixture/archive"
        with self.assertRaisesRegex(RuntimeError,"HTTP failure"):
            Client()._stream_raw_result(response)
        self.assertTrue(response.closed)

    def test_failed_stream_construction_and_client_close_release_owned_resources(self):
        for invalid in [False,True]:
            response = SimpleNamespace(closed=False)
            def content(*_):
                if invalid:raise RuntimeError("fixture construction failed")
                return 42  # Not an iterable.
            response.iter_content = content
            response.close = lambda:setattr(response,"closed",True)
            with self.assertRaises((TypeError,RuntimeError)):
                RUNTIME.ClosingResponseStream(response,512,False)
            self.assertTrue(response.closed)
        session = RUNTIME.DockerSession()
        def fail():raise RuntimeError("fixture client close failed")
        transport = SimpleNamespace(closed=False)
        transport.close = lambda:setattr(transport,"closed",True)
        session.client,session.connections = SimpleNamespace(close=fail),transport
        with self.assertRaisesRegex(RuntimeError,"client close failed"):
            session.__exit__(None,None,None)
        self.assertTrue(transport.closed)
        self.assertIsNone(session.client)
        self.assertIsNone(session.connections)
        session.__exit__(None,None,None)

    def test_raw_stream_releases_response_before_first_read_after_partial_and_on_error(self):
        class Response:
            def __init__(self, error=False):
                self.closed,self.reads,self.error = 0,0,error
            def iter_content(self, size, decode):
                self.reads += 1
                self.arguments = (size,decode)
                yield b"first"
                if self.error:raise TimeoutError("fixture read deadline")
                yield b"second"
            def close(self):self.closed += 1
        for mode in ["unread","partial","complete","error"]:
            response = Response(error=mode == "error")
            stream = RUNTIME.ClosingResponseStream(response,512,False)
            if mode == "partial":self.assertEqual(next(stream),b"first")
            if mode == "complete":self.assertEqual(list(stream),[b"first",b"second"])
            if mode == "error":
                self.assertEqual(next(stream),b"first")
                with self.assertRaises(TimeoutError):next(stream)
            stream.close()
            stream.close()
            self.assertEqual(response.closed,1)
            self.assertEqual(response.reads,0 if mode == "unread" else 1)
            self.assertEqual(list(stream),[])
            if mode != "unread":self.assertEqual(response.arguments,(512,False))

    def test_duplex_transport_retains_binary_payload_and_cleans_only_owned_children(self):
        command = [sys.executable, "-c", "import os; data=os.read(0, 1024); os.write(1, data[::-1])"]
        transport = RUNTIME.StdioConnections(command)
        connection = transport.connect(3)
        connection.sendall(b"HTTP/1.1\r\n\x00payload\xff")
        self.assertEqual(connection.recv(1024), b"HTTP/1.1\r\n\x00payload\xff"[::-1])
        connection.close()
        children = list(transport.children)
        transport.close()
        self.assertTrue(all(child.poll() is not None for child in children))
        transport.close()
        with self.assertRaises(RUNTIME.DockerAccessError):
            transport.connect(1)

    def test_closing_connections_terminates_their_live_cli_children_not_unrelated_processes(self):
        unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        transport = RUNTIME.StdioConnections([sys.executable, "-c", "import time; time.sleep(60)"])
        connection = transport.connect(1)
        child = transport.children[0]
        try:
            self.assertIsNone(child.poll())
            transport.close()
            self.assertIsNotNone(child.poll())
            self.assertIsNone(unrelated.poll())
        finally:
            connection.close()
            unrelated.terminate()
            unrelated.wait(timeout=3)

    def test_failed_start_closes_socketpair_and_leaves_no_child(self):
        transport = RUNTIME.StdioConnections(["/does-not-exist/asb-test"])
        with self.assertRaises(FileNotFoundError):
            transport.connect(1)
        self.assertEqual(transport.children, [])
        transport.close()


if __name__ == "__main__":
    unittest.main()
