"""Private task protocol tests; no Docker, agent or provider calls."""
import base64
import io
import json
import os
from pathlib import Path
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / "assets/benchmarks"))
from asb_task_server import Event, Operation, serve
import asb_task_server as subject


class Task:
    def __init__(self):
        self.invocation = {"prompt": "synthetic issue", "workspace": "/task"}
        self.workspace = SimpleNamespace(cleanup_status="pending", execute=self.execute)
        self.commands = []
    def __enter__(self):
        return self
    def __exit__(self, *args):
        self.workspace.cleanup_status = "removed"
    def execute(self, argv, environment, **kwargs):
        self.commands.append((argv, environment))
        target = {"exec_id": "owned-exec", "daemon_pid": 9001}
        kwargs["before_resume"](target)
        kwargs["on_release_accepted"](target)
        kwargs["on_output"]("stdout", b"live\x00stdout")
        return {"exit_code": 7, "stdout": b"live\x00stdout", "stderr": b"warning", "timed_out": False}
    def capture_submission(self):
        return {"patch_bytes": 123, "artifact": "candidate.patch"}
    def evaluate(self, timeout):
        assert timeout == 60
        return {"resolved": False, "outcome": "unresolved"}


class ProtocolTests(unittest.TestCase):
    def test_unexpected_start_failure_logs_bounded_frames_without_exception_text(self):
        class BrokenTask(Task):
            def __enter__(self):
                raise RuntimeError("private-credential-and-response")
        log, output = io.StringIO(), io.StringIO()
        with patch.object(subject.sys, "stderr", log):
            self.assertEqual(serve(BrokenTask(), io.StringIO(""), output), 1)
        diagnostic = json.loads(log.getvalue())
        self.assertEqual(diagnostic["error_class"], "RuntimeError")
        self.assertTrue(any(frame["function"] == "__enter__" for frame in diagnostic["frames"]))
        self.assertLessEqual(len(diagnostic["frames"]), 16)
        self.assertNotIn("private-credential-and-response", log.getvalue() + output.getvalue())
        class BrokenLog:
            def write(self, _text):
                raise OSError("disk full")
        output = io.StringIO()
        with patch.object(subject.sys, "stderr", BrokenLog()):
            self.assertEqual(serve(BrokenTask(), io.StringIO(""), output), 1)
        self.assertIn('"error_class": "RuntimeError"', output.getvalue())

    def test_protocol_deadline_counts_actual_monotonic_delay_not_requested_poll_intervals(self):
        clock = SimpleNamespace(now=0.0, monotonic=lambda: clock.now)
        class DelayedQueue:
            calls = 0
            def get(self, timeout):
                self.calls += 1
                clock.now += 1.0  # Scheduling delay much longer than a 50ms poll.
                raise subject.queue.Empty
        requests = DelayedQueue()
        with patch.object(subject, "time", clock, create=True), \
             patch.object(subject.queue, "Queue", return_value=requests), \
             patch.object(subject.threading, "Thread"):
            output, task = io.StringIO(), Task()
            self.assertEqual(serve(task, io.StringIO(""), output, idle_timeout=0.1), 1)
        self.assertEqual(requests.calls, 1, "protocol deadline restarted after actual wall time elapsed")
        self.assertTrue(task.interrupted)

    def test_late_queued_close_cannot_authorize_normal_teardown_after_deadline(self):
        clock = SimpleNamespace(now=0.0, monotonic=lambda: clock.now)
        class LateQueue:
            def get(self, timeout):
                clock.now += 1.0
                return {"op": "close"}
        with patch.object(subject, "time", clock), \
             patch.object(subject.queue, "Queue", return_value=LateQueue()), \
             patch.object(subject.threading, "Thread"):
            output, task = io.StringIO(), Task()
            self.assertEqual(serve(task, io.StringIO(""), output, idle_timeout=0.1), 1)
        self.assertTrue(task.interrupted)
        self.assertIn("protocol deadline", json.loads(output.getvalue().splitlines()[-2])["reason"])

    def test_typed_identifiers_preserve_wire_and_reject_unknown_operation_types(self):
        self.assertEqual([item.value for item in Operation],
                         ["execute", "agent", "resume", "capture", "evaluate", "close", "cancel", "initial-material"])
        self.assertEqual(json.loads(json.dumps(list(Event))),
                         ["ready", "paused", "release_accepted", "output", "execution_ended", "completed",
                          "submission", "evaluated", "error", "closed", "initial-material", "preparation_failed"])
        for unknown in [None, 1, [], {}, "private-unknown-operation"]:
            with self.subTest(operation=unknown):
                status, events, task = self.run_protocol([{"op": unknown}, {"op": "close"}])
                self.assertEqual(status, 0)
                self.assertEqual(task.commands, [])
                self.assertEqual([event["event"] for event in events], ["ready", "error", "closed"])
                self.assertEqual(events[1], {"event": "error", "error_class": "ExecutionError",
                                            "reason": "unknown task protocol operation"})
                self.assertNotIn("private-unknown-operation", json.dumps(events))
    def test_shared_launch_attach_wait_exceeds_legacy_idle_and_sixty_second_caps(self):
        import queue
        now, waiting = [0.0], [False]
        class VirtualQueue(queue.Queue):
            def get(self, **options):
                if waiting[0] and now[0] < 120:
                    now[0] += 40
                    raise queue.Empty
                result = super().get(**options)
                if waiting[0]:waiting[0] = False
                return result
        def execute(_task,_request,attach,*_args,**options):
            budget = options["preparation_budget"]
            self.assertEqual(budget.deadline,1800)
            waiting[0] = True
            attach(dict(exec_id="owned-exec"))
            self.assertEqual(now[0],120)
            options["on_release_accepted"](dict(exec_id="owned-exec"))
            return dict(stdout=b"",stderr=b"",exit_code=0)
        with patch("asb_task_server.queue.Queue",VirtualQueue), \
                patch("asb_task_server.time.monotonic",side_effect=lambda:now[0]), \
                patch("asb_guest.execute_agent",side_effect=execute):
            status,events,_ = self.run_protocol([dict(op="agent",preparation_seconds=1800),
                dict(op="resume",exec_id="owned-exec"),dict(op="close")])
        self.assertEqual(status,0)
        self.assertEqual([event["event"] for event in events],["ready","paused","release_accepted","completed","closed"])

    def test_material_capture_is_explicit_offline_and_never_starts_execution(self):
        class MaterialTask(Task):
            calls = 0
            def initial_material(self):
                self.calls += 1
                return {"schema_version":1,"scope":"prepared-workload-material-v1","sha256":"a"*64}
        task = MaterialTask()
        self.run_protocol([{"op":"close"}],task)
        self.assertEqual(task.calls,0)
        status,events,_ = self.run_protocol([{"op":"initial-material"},{"op":"close"}],task)
        self.assertEqual(status,0)
        self.assertEqual(task.calls,1)
        self.assertEqual(task.commands,[])
        self.assertEqual([event["event"] for event in events],["ready","initial-material","closed"])
        self.assertEqual(events[1]["material"]["sha256"],"a"*64)
        _,events,_ = self.run_protocol([{"op":"initial-material"},{"op":"close"}])
        self.assertIn("has not declared prepared material coverage",events[1]["reason"])

    def run_protocol(self, requests, task=None):
        task = task or Task()
        output = io.StringIO()
        # Keep stdin genuinely open like the Rust controller; EOF must always
        # cancel even if a graceful close request was previously queued.
        read_fd, write_fd = os.pipe()
        source = os.fdopen(read_fd, "r")
        try:
            os.write(write_fd, "".join(json.dumps(request) + "\n" for request in requests).encode())
            status = serve(task, source, output, idle_timeout=2)
        finally:
            os.close(write_fd)
            source.close()
        return status, [json.loads(line) for line in output.getvalue().splitlines()], task

    def test_material_runtime_is_forwarded_privately_without_agent_dispatch(self):
        class MaterialTask(Task):
            def initial_material(self, binary=None):
                self.binary = binary
                return {"schema_version":1,"scope":"prepared-workload-material-v1","sha256":"a"*64}
        task = MaterialTask()
        status, events, _ = self.run_protocol([{"op":"initial-material","binary":"/private/fixture-helper"},
                                              {"op":"close"}], task)
        self.assertEqual(status,0)
        self.assertEqual(task.binary,"/private/fixture-helper")
        self.assertEqual(task.commands,[])
        self.assertNotIn("/private/fixture-helper",json.dumps(events))

    def test_selected_command_and_credentials_stay_in_private_request_not_events(self):
        status, events, task = self.run_protocol([
            {"op": "execute", "argv": ["selected-agent", "literal ; argument"], "environment": {"API_KEY": "fixture-key"}},
            {"op": "resume", "exec_id": "owned-exec"}, {"op": "capture"}, {"op": "evaluate", "timeout": 60}, {"op": "close"},
        ])
        self.assertEqual(status, 0)
        self.assertEqual([event["event"] for event in events], ["ready", "paused", "release_accepted", "output", "completed", "submission", "evaluated", "closed"])
        self.assertEqual(task.commands, [(["selected-agent", "literal ; argument"], {"API_KEY": "fixture-key"})])
        self.assertNotIn("fixture-key", json.dumps(events))
        self.assertEqual(base64.b64decode(events[3]["data"]), b"live\x00stdout")
        self.assertEqual(base64.b64decode(events[4]["result"]["stderr"]), b"warning")
        self.assertEqual(events[4]["result"]["exit_code"], 7)
        self.assertEqual(events[-1]["cleanup"], "removed")

    def test_wrong_resume_identity_is_rejected_and_cleanup_remains_available(self):
        status, events, _ = self.run_protocol([
            {"op": "execute", "argv": ["selected-agent"]},
            {"op": "resume", "exec_id": "other-execution"}, {"op": "close"},
        ])
        self.assertEqual(status, 0)
        self.assertNotIn("completed", [event["event"] for event in events])
        self.assertEqual(events[2]["event"], "error")
        self.assertEqual(events[-1]["cleanup"], "removed")

    def test_eof_or_malformed_request_closes_task_without_executing_agent(self):
        for source in ["", "not-json\n", "[]\n", "x" * (1024 * 1024 + 1)]:
            task, output = Task(), io.StringIO()
            self.assertEqual(serve(task, io.StringIO(source), output), 1)
            self.assertEqual(task.commands, [])
            self.assertEqual(task.workspace.cleanup_status, "removed")
            self.assertEqual(json.loads(output.getvalue().splitlines()[-1])["event"], "closed")

    def test_unknown_operation_does_not_echo_arbitrary_input(self):
        status, events, _ = self.run_protocol([{"op": "sensitive-input-not-for-output"}, {"op": "close"}])
        self.assertEqual(status, 0)
        self.assertNotIn("sensitive-input-not-for-output", json.dumps(events))
        self.assertEqual(events[1]["reason"], "unknown task protocol operation")

    def test_only_explicit_close_authorizes_normal_teardown(self):
        class PreservingTask(Task):
            def __exit__(self, *args):
                self.workspace.cleanup_status = "stopped-preserved" if self.interrupted else "removed"
        _, events, task = self.run_protocol([{"op": "close"}], PreservingTask())
        self.assertFalse(task.interrupted)
        self.assertEqual(events[-1]["cleanup"], "removed")
        for content in ["", "not-json\n", '{"op":"cancel"}\n']:
            task, output = PreservingTask(), io.StringIO()
            self.assertEqual(serve(task, io.StringIO(content), output), 1)
            self.assertTrue(task.interrupted)
            self.assertEqual(json.loads(output.getvalue().splitlines()[-1])["cleanup"], "stopped-preserved")
            self.assertFalse(task.commands)

    def test_failed_ready_publication_preserves_instead_of_authorizing_deletion(self):
        class BrokenDestination(io.StringIO):
            def write(self, _text):
                raise BrokenPipeError("fixture consumer exited")
        task = Task()
        status = serve(task, io.StringIO(""), BrokenDestination())
        self.assertEqual(status, 1)
        self.assertTrue(task.interrupted)
        self.assertFalse(task.commands)

    def test_disconnect_while_waiting_for_attachment_acknowledges_cleanup(self):
        read_fd, write_fd = os.pipe()
        source, task = os.fdopen(read_fd, "r"), Task()
        open_writer = [write_fd]
        class DisconnectAtGate(io.StringIO):
            def write(self, text):
                result = super().write(text)
                if json.loads(text)["event"] == "paused":
                    os.close(open_writer.pop())
                return result
        output = DisconnectAtGate()
        try:
            os.write(write_fd, b'{"op":"execute","argv":["selected-agent"]}\n')
            status = serve(task, source, output, idle_timeout=2)
        finally:
            if open_writer:
                os.close(open_writer.pop())
            source.close()
        events = [json.loads(line) for line in output.getvalue().splitlines()]
        self.assertEqual(status, 0)
        self.assertEqual([event["event"] for event in events], ["ready", "paused", "error", "closed"])
        self.assertEqual(events[-1]["cleanup"], "removed")
        self.assertEqual(task.workspace.cleanup_status, "removed")


if __name__ == "__main__":
    unittest.main()
