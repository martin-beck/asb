"""Provider-free real-executable tests of the private guest TCP relay."""
import json
import os
from pathlib import Path
import select
import socket
import socketserver
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
import tomllib

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / "assets/benchmarks"))
from asb_relay import HostRelay, ForwardingCancellation, forward_request, loopback_target, project_toml
from asb_execution import ExecutionError


class RelayTests(unittest.TestCase):
    def start(self, count=2):
        process = subprocess.Popen([os.environ["ASB_TEST_HOST_ASB"], "__asb-relay", str(count)],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, bufsize=0)
        self.addCleanup(self.cleanup, process)
        self.process = process
        kind, channel, payload = self.frame()
        self.assertEqual((kind, channel, len(payload)), (5, 0, 2 * count))
        return struct.unpack("!" + "H" * count, payload)

    @staticmethod
    def cleanup(process):
        if process.stdin and not process.stdin.closed:
            process.stdin.close()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)
        for stream in (process.stdout, process.stderr):
            if stream:
                stream.close()

    def exact(self, count):
        data, deadline = bytearray(), time.monotonic() + 5
        while len(data) < count:
            remaining = deadline - time.monotonic()
            self.assertGreater(remaining, 0, "relay frame timed out")
            ready, _, _ = select.select([self.process.stdout], [], [], remaining)
            self.assertTrue(ready, "relay did not publish a frame")
            chunk = os.read(self.process.stdout.fileno(), count - len(data))
            self.assertTrue(chunk, "relay unexpectedly closed stdout")
            data.extend(chunk)
        return bytes(data)

    def frame(self):
        kind, channel, size = struct.unpack("!BII", self.exact(9))
        self.assertLessEqual(size, 65536)
        return kind, channel, self.exact(size)

    def send(self, kind, channel, payload=b""):
        data = struct.pack("!BII", kind, channel, len(payload)) + payload
        while data:
            written = os.write(self.process.stdin.fileno(), data)
            data = data[written:]

    def connect(self, port, target):
        client = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.addCleanup(client.close)
        kind, channel, payload = self.frame()
        self.assertEqual((kind, payload), (1, struct.pack("!I", target)))
        return client, channel

    def test_concurrent_targets_preserve_binary_bytes_and_independent_half_closes(self):
        ports = self.start()
        first, first_id = self.connect(ports[0], 0)
        second, second_id = self.connect(ports[1], 1)
        self.assertNotEqual(first_id, second_id)
        # Includes NUL/non-UTF8 bytes and HTTP-looking content: no HTTP parser,
        # token transformation or buffering-until-response is permitted here.
        payloads = {first_id: bytes(range(256)) * 128,
                    second_id: b"GET /stream HTTP/1.1\r\n\r\n\x00\xff"}
        first.sendall(payloads[first_id])
        second.sendall(payloads[second_id])
        observed = {first_id: bytearray(), second_id: bytearray()}
        while any(len(observed[key]) < len(value) for key, value in payloads.items()):
            kind, channel, data = self.frame()
            self.assertEqual(kind, 2)
            observed[channel].extend(data)
        self.assertEqual(observed, payloads)
        first.shutdown(socket.SHUT_WR)
        self.assertEqual(self.frame(), (3, first_id, b""))
        self.send(2, first_id, b"response after request EOF")
        self.send(3, first_id)
        received = bytearray()
        while chunk := first.recv(1024):
            received.extend(chunk)
        self.assertEqual(received, b"response after request EOF")
        self.send(2, second_id, b"still independent")
        self.assertEqual(second.recv(1024), b"still independent")
        self.send(4, first_id)
        self.send(4, second_id)
        # Closing a channel is idempotent; stale late data cannot reopen it.
        self.send(4, first_id)
        self.send(2, first_id, b"ignored after close")
        self.process.stdin.close()
        self.assertEqual(self.process.wait(timeout=3), 0)

    def test_transport_disconnect_closes_active_sockets_without_other_process_changes(self):
        ports = self.start(1)
        client, _ = self.connect(ports[0], 0)
        self.process.stdin.close()
        self.assertEqual(self.process.wait(timeout=3), 0)
        self.assertEqual(client.recv(1), b"")
        with self.assertRaises(OSError):
            socket.create_connection(("127.0.0.1", ports[0]), timeout=1)

    def test_streaming_many_frames_does_not_require_a_complete_response_buffer(self):
        port, = self.start(1)
        client, channel = self.connect(port, 0)
        payload = bytes(range(256)) * 4096
        received = bytearray()
        errors = []
        def receive():
            try:
                while chunk := client.recv(65536):
                    received.extend(chunk)
            except OSError as error:
                errors.append(error)
        reader = threading.Thread(target=receive, daemon=True)
        reader.start()
        for offset in range(0, len(payload), 65536):
            self.send(2, channel, payload[offset:offset + 65536])
        self.send(3, channel)
        reader.join(timeout=5)
        self.assertFalse(reader.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(received, payload)

    def test_channel_capacity_is_bounded_and_transport_eof_drains_all_connections(self):
        port, = self.start(1)
        clients = [self.connect(port, 0)[0] for _ in range(128)]
        overflow = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.addCleanup(overflow.close)
        self.assertEqual(overflow.recv(1), b"")
        self.process.stdin.close()
        self.assertEqual(self.process.wait(timeout=5), 0)
        self.assertTrue(all(client.recv(1) == b"" for client in clients))

    def test_invalid_direction_and_oversized_frames_fail_without_payload_allocation(self):
        for kind, channel, length, body in [(1, 1, 0, b""), (2, 0, 0, b""),
                                             (3, 1, 1, b"x"), (2, 1, 65537, b"")]:
            with self.subTest(kind=kind, channel=channel, length=length):
                self.start(1)
                self.process.stdin.write(struct.pack("!BII", kind, channel, length) + body)
                self.process.stdin.flush()
                self.assertNotEqual(self.process.wait(timeout=3), 0)
                self.cleanup(self.process)

    def test_target_count_is_bounded(self):
        for count in ("0", "65", "-1", "not-a-number"):
            result = subprocess.run([os.environ["ASB_TEST_HOST_ASB"], "__asb-relay", count],
                                    capture_output=True, timeout=3)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, b"")

    def test_listener_arguments_cannot_expose_non_loopback_interfaces(self):
        for address in ("0.0.0.0", "::", "192.0.2.1", "localhost", "not-an-address"):
            result = subprocess.run([os.environ["ASB_TEST_HOST_ASB"], "__asb-relay", "1", address],
                                    capture_output=True, timeout=3)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(result.stdout, b"")


class Echo(socketserver.BaseRequestHandler):
    def handle(self):
        while data := self.request.recv(65536):
            self.request.sendall(data)
        self.request.sendall(b"after-half-close")


class GzipJsonProvider(socketserver.StreamRequestHandler):
    """Tiny provider double that keeps the wire body and header distinct."""

    def handle(self):
        import gzip

        while self.rfile.readline() not in (b"", b"\r\n"):
            pass
        body = gzip.compress(b'{"choices":[{"message":{"content":"ok"}}]}')
        self.wfile.write(
            b"HTTP/1.1 200 OK\r\n"
            b"content-type: application/json\r\n"
            b"content-encoding: gzip\r\n"
            + f"content-length: {len(body)}\r\n\r\n".encode()
            + body
        )


class TapeHeaderTests(unittest.TestCase):
    def setUp(self):
        self.upstream = socketserver.ThreadingTCPServer(("127.0.0.1", 0), GzipJsonProvider)
        self.upstream.daemon_threads = True
        self.thread = threading.Thread(target=self.upstream.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.upstream.server_close)
        self.addCleanup(self.upstream.shutdown)

    def test_recorded_decoded_response_drops_stale_content_encoding(self):
        script = Path(__file__).parents[1] / "assets/scripts/llmtape.mjs"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            ready, cassette = root / "ready.json", root / "cassette.jsonl"
            environment = {**os.environ, "ASB_TAPE_READY_FILE": str(ready), "NO_PROXY": "*", "no_proxy": "*"}
            for name in ("http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"):
                environment.pop(name, None)
            process = subprocess.Popen(
                ["node", str(script), "record", "--port", "0", "--upstream",
                 f"http://127.0.0.1:{self.upstream.server_address[1]}", "--out", str(cassette)],
                env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            self.addCleanup(lambda: process.poll() is None and process.terminate())
            deadline = time.monotonic() + 5
            while not ready.exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            self.assertTrue(ready.exists(), f"tape listener did not become ready (exit={process.poll()})")
            port = json.loads(ready.read_text())["port"]
            with socket.create_connection(("127.0.0.1", port), timeout=5) as client:
                client.sendall(
                    b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\n"
                    b"Connection: close\r\nContent-Length: 2\r\n\r\n{}"
                )
                response = bytearray()
                client.settimeout(0.2)
                while True:
                    try:
                        chunk = client.recv(65536)
                    except TimeoutError:
                        break
                    if not chunk:
                        break
                    response.extend(chunk)
            # The production owner performs graceful draining.  This isolated
            # wire test has no owner/capture receipt to close, so stop the
            # private listener directly once its response has been observed.
            process.kill()
            process.wait(timeout=5)
            process.stdout.close()
            process.stderr.close()
            headers, body = bytes(response).split(b"\r\n\r\n", 1)
            self.assertNotIn(b"content-encoding:", headers.lower())
            self.assertIn(b'"content":"ok"', body)
            recorded = json.loads(cassette.read_text())
            self.assertNotIn("content-encoding", {key.lower() for key in recorded["response"]["headers"]})


class ForwardingTests(unittest.TestCase):
    def setUp(self):
        self.server = socketserver.ThreadingTCPServer(("127.0.0.1", 0), Echo)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)

    def start(self, targets):
        relay = HostRelay(targets, [os.environ["ASB_TEST_HOST_ASB"], "__asb-relay"])
        self.addCleanup(relay.close)
        return relay

    def test_host_and_guest_relay_forward_binary_streams_and_half_closes(self):
        destination = self.server.server_address
        relay = self.start([destination, ("localhost", destination[1])])
        payload = bytes(range(256)) * 2048
        def roundtrip(port):
            with socket.create_connection(("127.0.0.1", port), timeout=5) as client:
                incoming, errors = bytearray(), []
                def reader():
                    try:
                        while chunk := client.recv(65536):
                            incoming.extend(chunk)
                    except OSError as error:
                        errors.append(error)
                thread = threading.Thread(target=reader, daemon=True)
                thread.start()
                client.sendall(payload)
                client.shutdown(socket.SHUT_WR)
                thread.join(timeout=5)
                self.assertFalse(thread.is_alive())
                self.assertEqual(errors, [])
                self.assertEqual(incoming, payload + b"after-half-close")
        # Repeated connections must be released, not accumulate active slots.
        for _ in range(3):
            for port in relay.ports:
                roundtrip(port)
        relay.check()
        relay.close()
        self.assertIsNotNone(relay.process.poll())
        self.assertFalse(relay.reader.is_alive())
        self.assertTrue(all(not worker.is_alive() for worker in relay.workers))

    def test_rewrites_all_local_proxies_and_endpoint_without_mutating_host_configuration(self):
        original = {
            "project_configuration": 'tools_dir="tools"\n[proxy]\nhttps="http://localhost:3128"\n'
                'socks="socks5h://[::1]:1080"\nno_proxy="localhost,127.0.0.1"\n',
            "request": {"connection": {"base_url": "https://localhost:443/v1?name=%2F"}},
            "environment": {"http_proxy": "http://user:fixture-pass@localhost:3128/path",
                            "HTTPS_PROXY": "http://192.0.2.1:8080", "NO_PROXY": "*"},
        }
        observed = []
        class FakeRelay:
            ports = (10001, 10002, 10003)
        def start(targets):
            observed.extend(targets)
            return FakeRelay()
        changed = forward_request(original, start)
        self.assertEqual(observed, [("localhost", 443), ("localhost", 3128), ("::1", 1080)])
        self.assertEqual(changed["request"]["connection"]["base_url"], "https://localhost:10001/v1?name=%2F")
        self.assertEqual(changed["environment"]["http_proxy"], "http://user:fixture-pass@localhost:10002/path")
        self.assertEqual(changed["environment"]["HTTPS_PROXY"], "http://192.0.2.1:8080")
        self.assertEqual(changed["environment"]["NO_PROXY"], "*")
        config = tomllib.loads(changed["project_configuration"])
        self.assertEqual(config["proxy"]["https"], "http://localhost:10002")
        self.assertEqual(config["proxy"]["socks"], "socks5h://[::1]:10003")
        self.assertEqual(config["proxy"]["no_proxy"], "localhost,127.0.0.1")
        self.assertEqual(original["request"]["connection"]["base_url"], "https://localhost:443/v1?name=%2F")
        self.assertIn("localhost:3128", original["environment"]["http_proxy"])

    def test_remote_urls_do_not_start_a_relay_and_project_serialization_round_trips(self):
        original = {"project_configuration": "tools_dir='tools'\n", "request": {
            "connection": {"base_url": "https://provider.example/v1"}}}
        def unexpected(_):
            self.fail("no relay for remote endpoints")
        self.assertEqual(forward_request(original, unexpected)["request"], original["request"])
        self.assertEqual(forward_request(dict(original, request={"connection": None}), unexpected)["request"],
                         {"connection": None})
        document = {"tools_dir": "tools/é\nquoted\"", "cgroup_parent": None,
                    "proxy": {"no_proxy": "*", "http": "http://localhost:80"}}
        self.assertEqual(tomllib.loads(project_toml(document)),
                         {key: value for key, value in document.items() if value is not None})

    def test_destinations_are_explicit_and_loopback_only(self):
        for target in [("192.0.2.1", 80), ("provider.example", 443), ("0.0.0.0", 80)]:
            with self.assertRaises(ExecutionError):
                self.start([target])
        self.assertIsNone(loopback_target("https://provider.example/v1"))
        self.assertEqual(loopback_target("https://localhost/v1"), ("localhost", 443))
        self.assertEqual(loopback_target("http://127.0.0.2:123/v1"), ("127.0.0.2", 123))
        with self.assertRaises(ExecutionError):
            loopback_target("http://localhost:99999")
        with self.assertRaises(ExecutionError):
            loopback_target("http://localhost:0")

    def test_transport_loss_interrupts_owned_execution_without_mutating_caller_cancellation(self):
        from types import SimpleNamespace
        upstream, lost = threading.Event(), threading.Event()
        cancelled = ForwardingCancellation(upstream, [SimpleNamespace(stop=lost)])
        self.assertFalse(cancelled.is_set())
        lost.set()
        self.assertTrue(cancelled.is_set())
        self.assertFalse(upstream.is_set())
        self.assertTrue(ForwardingCancellation(lambda: True, []).is_set())

    def test_malformed_startup_frames_close_the_owned_exec_process(self):
        for header in [(5, 1, 2), (5, 0, 65537), (1, 0, 2)]:
            children = []
            original = subprocess.Popen
            def start(*args, **kwargs):
                process = original(*args, **kwargs)
                children.append(process)
                return process
            script = ("import os,struct,time; os.write(1,struct.pack('!BII',"
                      + ",".join(str(value) for value in header) + ")+b'xx'); time.sleep(60)")
            with self.subTest(header=header), patch("asb_relay.subprocess.Popen", side_effect=start):
                with self.assertRaises(ExecutionError):
                    HostRelay([("127.0.0.1", 12345)], [sys.executable, "-c", script])
                self.assertEqual(len(children), 1)
                self.assertIsNotNone(children[0].poll())

    def test_disconnect_keeps_bounded_child_diagnostics_in_an_attempt_artifact(self):
        # Use a private temporary directory; the diagnostic must be useful
        # after a controller failure without exposing an unbounded stderr pipe.
        import tempfile
        with tempfile.TemporaryDirectory() as directory:
            artifact = Path(directory) / "endpoint-relay-0.json"
            script = ("import os,struct,sys; "
                      "os.write(1,struct.pack('!BII',5,0,2)+struct.pack('!H',12345)); "
                      "sys.stderr.write('fixture relay diagnostic\\n'); sys.stderr.flush()")
            relay = HostRelay([("127.0.0.1", 12345)], [sys.executable, "-c", script],
                              diagnostic_path=artifact)
            relay.reader.join(timeout=3)
            self.assertFalse(relay.reader.is_alive())
            with self.assertRaisesRegex(ExecutionError, "control stream closed"):
                relay.check()
            relay.close()
            diagnostic = json.loads(artifact.read_text())
            self.assertEqual(diagnostic["reason"], "relay control stream closed")
            self.assertEqual(diagnostic["stderr_tail"], "fixture relay diagnostic")
            self.assertLessEqual(artifact.stat().st_size, 12 * 1024)

    def test_cancellation_closes_host_connections_and_owned_relay(self):
        cancelled = threading.Event()
        relay = HostRelay([self.server.server_address],
                          [os.environ["ASB_TEST_HOST_ASB"], "__asb-relay"], cancelled)
        self.addCleanup(relay.close)
        with socket.create_connection(("127.0.0.1", relay.ports[0]), timeout=5) as client:
            client.sendall(b"before cancellation")
            self.assertEqual(client.recv(1024), b"before cancellation")
            cancelled.set()
            relay.reader.join(timeout=3)
            self.assertFalse(relay.reader.is_alive())
            with self.assertRaises(ExecutionError):
                relay.check()
        relay.close()
        self.assertIsNotNone(relay.process.poll())

    def test_ipv6_only_localhost_upstream_and_guest_listener_preserve_address_family(self):
        class IPv6Server(socketserver.ThreadingTCPServer):
            address_family = socket.AF_INET6
            daemon_threads = True
        try:
            server = IPv6Server(("::1", 0), Echo)
        except OSError:
            self.skipTest("host has no IPv6 loopback")
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        relay = self.start([("::1", server.server_address[1]),
                            ("localhost", server.server_address[1])])
        for host, port in [("::1", relay.ports[0]), ("127.0.0.1", relay.ports[1])]:
            with socket.create_connection((host, port), timeout=5) as client:
                client.sendall(b"IPv6 remains loopback")
                self.assertEqual(client.recv(1024), b"IPv6 remains loopback")

    def test_non_default_ipv4_loopback_address_is_preserved(self):
        try:
            server = socketserver.ThreadingTCPServer(("127.0.0.2", 0), Echo)
        except OSError:
            self.skipTest("host cannot bind a non-default IPv4 loopback address")
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        relay = self.start([server.server_address])
        with socket.create_connection(("127.0.0.2", relay.ports[0]), timeout=5) as client:
            client.sendall(b"non-default loopback")
            self.assertEqual(client.recv(1024), b"non-default loopback")


if __name__ == "__main__":
    unittest.main()
