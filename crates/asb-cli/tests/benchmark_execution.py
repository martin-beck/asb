"""Provider-free managed execution tests using a deterministic Docker API fixture."""
import importlib.util
import json
from pathlib import Path
import sys
import threading
import time
from types import SimpleNamespace
import unittest

sys.dont_write_bytecode = True
SOURCE = Path(__file__).parents[1] / "assets/benchmarks/asb_execution.py"
SPEC = importlib.util.spec_from_file_location("asb_execution", SOURCE)
RUNTIME = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNTIME)


class Stream:
    def __init__(self, api):
        self.api, self.index = api, 0
        self.closed = False
        self._response = SimpleNamespace(closed=False)
        self._response.close = lambda: setattr(self._response, "closed", True)
    def __iter__(self):
        return self
    def __next__(self):
        api = self.api
        if self.index == 0:
            self.index += 1
            return api.ready, None
        if not api.container.resumed:
            raise AssertionError("read agent output before resume")
        if api.hang:
            if not api.container.killed.wait(2):
                raise AssertionError("deadline did not stop its owned container")
        position = self.index - 1
        if position < len(api.output) and not api.hang:
            self.index += 1
            return api.output[position]
        api.completed = True
        raise StopIteration
    def close(self):
        self.closed = True


class Container:
    def __init__(self):
        self.id = "owned-container"
        self.attrs = {}
        self.events = []
        self.resumed = False
        self.killed = threading.Event()
    def start(self):
        self.events.append("start")
    def wait(self, timeout):
        assert timeout == 125
        self.events.append("wait")
    def reload(self):
        pass
    def exec_run(self, argv):
        if "asb-resume" in argv:
            self.events.append("resume")
            self.resumed = True
        else:
            self.events.append("stopped")
        return SimpleNamespace(exit_code=0, output=b"")
    def kill(self):
        self.events.append("kill")
        self.killed.set()
    def remove(self, force):
        assert force is True
        self.events.append("remove")


class API:
    def __init__(self, container):
        self.container = container
        self.ready = b"__ASB_GATE__:42\n"
        self.output = [(b"stdout", None), (None, b"stderr")]
        self.hang = False
        self.status = 0
        self.pid = 9001
        self.calls = []
        self.completed = False
    def exec_create(self, container_id, **kwargs):
        assert container_id == self.container.id
        self.completed = False
        self.container.resumed = False
        self.container.killed.clear()
        self.calls.append(kwargs)
        return {"Id": "exec-1"}
    def exec_start(self, exec_id, stream, demux):
        assert exec_id == "exec-1" and stream and demux
        self.stream = Stream(self)
        return self.stream
    def exec_inspect(self, exec_id):
        assert exec_id == "exec-1"
        return {"Pid": self.pid, "Running": not self.completed,
                "ExitCode": (137 if self.hang else self.status) if self.completed else None}


class ExecutionTests(unittest.TestCase):
    def test_optional_principal_facts_never_infer_signal_from_carrier(self):
        for code in [-(2**31), -1, 0, 137, 143, 2**31-1]:
            exact = {"kind":"code", "code":code}
            self.assertEqual(RUNTIME.principal_observation({"exit_code":1,"process_termination":exact}), {"process_termination":exact})
        for signal in [1,9,15,127]:
            exact = {"kind":"signal", "signal":signal}
            self.assertEqual(RUNTIME.principal_observation({"exit_code":143,"process_termination":exact}), {"process_termination":exact})
        for metadata in [{}, {"exit_code":137}, {"exit_code":143,"timed_out":True}]:
            self.assertEqual(RUNTIME.principal_observation(metadata), {"process_termination":None})
        for exact in [{"kind":"signal","signal":0}, {"kind":"signal","signal":128},
                      {"kind":"code","code":True}, {"kind":"code","code":2**31},
                      {"kind":"signal","signal":9,"code":137}, {"kind":"future"}]:
            self.assertEqual(RUNTIME.principal_observation({"process_termination":exact}),
                             {"process_termination":None,"process_termination_diagnostic":"malformed"})

    def owner(self):
        return {"controller_namespace":100, "guest_namespace":200, "token":"e" * 32}

    def frames(self, status=0, timed_out=0):
        return (f"\n__ASB_AGENT_ENDED__:42:{'e' * 32}:{status}:123:{timed_out}\n"
                f"\n__ASB_WORKLOAD_READY__:42:{'e' * 32}:{status}\n").encode()

    def test_workload_frames_keep_split_output_and_publish_one_exact_boundary(self):
        ended = []
        frames = RUNTIME.WorkloadFrames(42, self.owner(), ended.append)
        received = bytearray()
        for byte in b"agent output without newline" + self.frames(status=7, timed_out=1):
            received.extend(frames.feed(bytes([byte])))
        self.assertTrue(frames.ready)
        self.assertEqual(bytes(received), b"agent output without newline\n\n")
        self.assertEqual(ended, [{"exit_code":7, "elapsed_seconds":.123, "timed_out":True}])
        self.assertNotIn(b"__ASB_", received)

    def test_workload_frames_reject_wrong_owner_order_status_and_repeated_end(self):
        valid = self.frames()
        ended, ready = valid.strip().split(b"\n\n")
        for content in [valid.replace(b":42:", b":43:"), valid.replace(b"e" * 32, b"a" * 32),
                        ready + b"\n", ended + b"\n" + ended + b"\n",
                        valid.replace(b":0:123:0", b":256:123:0"),
                        valid.replace(b":0:123:0", b":0:123:2"),
                        ended + b"\n" + ready.replace(b":0", b":7") + b"\n"]:
            with self.subTest(content=content), self.assertRaises(RUNTIME.ExecutionError):
                RUNTIME.WorkloadFrames(42, self.owner(), None).feed(content)

    def test_workload_output_remains_bounded_without_linebreaks(self):
        frames = RUNTIME.WorkloadFrames(42, self.owner(), None)
        output = frames.feed(b"x" * 100000)
        self.assertEqual(len(frames.pending), 512)
        self.assertEqual(len(output), 100000-512)

    def retained_workspace(self, metadata_override=None):
        workspace, container, api, _ = self.workspace()
        metadata = dict(workload_owner=self.owner(), tree_quiesced=False, exit_code=0,
                        elapsed_ms=123, timed_out=False)
        metadata.update(metadata_override or {})
        original_exec = container.exec_run
        def execute(argv):
            if argv == ["cat", "/opt/asb-private/attempt/agent-result.json"]:
                return SimpleNamespace(exit_code=0, output=json.dumps(metadata).encode())
            return original_exec(argv)
        container.exec_run = execute
        class RetainedStream(Stream):
            def __next__(stream):
                if stream.index > len(api.output):
                    if not container.killed.wait(3):
                        raise AssertionError("SDK did not stop its owned namespace")
                    raise StopIteration
                return super().__next__()
        def start(*_args, **_kwargs):
            api.stream = RetainedStream(api)
            return api.stream
        api.exec_start = start
        api.output = [(b"agent output", None), (self.frames()[:40], None), (self.frames()[40:], None)]
        remove = container.remove
        def stop(force):
            remove(force)
            container.killed.set()
        container.remove = stop
        return workspace, container, api

    def test_services_and_their_output_transport_live_through_grading_until_sdk_cleanup(self):
        workspace, container, api = self.retained_workspace()
        ended = []
        with workspace:
            result = workspace.execute(["asb", "__asb-agent"], gate="application",
                workload_owner=self.owner(), on_execution_end=ended.append)
            self.assertTrue(result["workload_processes_retained"])
            self.assertEqual(result["exit_code"], 0)
            self.assertEqual(result["elapsed_seconds"], .123)
            self.assertEqual(result["metadata"]["workload_owner"], self.owner())
            self.assertEqual(len(ended), 1)
            self.assertFalse(api.stream.closed)
            self.assertIsInstance(workspace.held_executions[0][0],RUNTIME.DockerExecTransport)
            self.assertTrue(workspace.held_executions[0][1].is_alive())
            self.assertTrue(workspace.active.acquire(blocking=False))
            workspace.active.release()
            self.assertNotIn("kill", container.events)
            # The SDK may now run its official verifier while services remain.
            container.events.append("official-grade")
        self.assertTrue(api.stream.closed)
        self.assertTrue(api.stream._response.closed)
        self.assertEqual(workspace.held_executions, [])
        self.assertLess(container.events.index("official-grade"), container.events.index("remove"))

    def test_contradictory_retained_metadata_resets_owned_tree_and_cannot_grade(self):
        for changed in [{"tree_quiesced":True}, {"workload_owner":None}, {"exit_code":7},
                        {"elapsed_ms":124}, {"timed_out":True}]:
            with self.subTest(changed=changed):
                workspace, container, api = self.retained_workspace(changed)
                with workspace:
                    with self.assertRaises(RUNTIME.ExecutionError):
                        workspace.execute(["asb"], gate="application", workload_owner=self.owner())
                    self.assertIn("kill", container.events)
                    self.assertEqual(workspace.held_executions, [])
                    self.assertTrue(api.stream.closed)

    def test_artifact_publication_status_never_substitutes_for_retained_owner_evidence(self):
        # Consumer/grader-admission seam with the production managed transport;
        # not a real SDK verifier. A pending ancillary status is not quiescence.
        for publication in ["missing-first", "pending-final", "settlement-failed"]:
            with self.subTest(publication=publication):
                workspace, container, api = self.retained_workspace({
                    "artifact_error": "scratch settlement pending" if publication == "pending-final"
                        else "scratch settlement failed"})
                if publication == "missing-first":
                    previous = container.exec_run
                    def missing(argv):
                        if argv == ["cat", "/opt/asb-private/attempt/agent-result.json"]:
                            return SimpleNamespace(exit_code=1, output=b"missing")
                        return previous(argv)
                    container.exec_run = missing
                grades = []
                with workspace:
                    try:
                        result = workspace.execute(["asb", "__asb-agent"], gate="application",
                            workload_owner=self.owner())
                    except RUNTIME.ExecutionError:
                        self.assertEqual(publication, "missing-first")
                        self.assertIn("kill", container.events)
                    else:
                        self.assertNotEqual(publication, "missing-first")
                        self.assertTrue(result["workload_processes_retained"])
                        self.assertEqual(result["metadata"]["workload_owner"], self.owner())
                        self.assertFalse(result["metadata"]["tree_quiesced"])
                        grades.append("one admitted grade")
                self.assertEqual(len(grades), 0 if publication == "missing-first" else 1)
                self.assertEqual(container.events.count("remove"), 1)
                self.assertTrue(api.stream.closed)
                self.assertEqual(workspace.held_executions, [])

    def test_workload_parent_release_follows_validated_metadata_and_preserves_transport_ownership(self):
        for fault in [None,"metadata","release"]:
            with self.subTest(fault=fault):
                workspace,container,api = self.retained_workspace({"tree_quiesced":True} if fault == "metadata" else None)
                released = []
                class Transport(RUNTIME.DockerExecTransport):
                    def release_workload_parent(self):
                        released.append(True)
                        if fault == "release":raise RUNTIME.ExecutionError("parent return unconfirmed")
                with workspace:
                    if fault is None:
                        result = workspace.execute(["asb"],gate="application",workload_owner=self.owner(),transport_factory=Transport)
                        self.assertTrue(result["workload_processes_retained"])
                        self.assertFalse(result["metadata"]["tree_quiesced"])
                        self.assertFalse(api.stream.closed)
                        self.assertEqual(len(workspace.held_executions),1)
                    else:
                        with self.assertRaises(RUNTIME.ExecutionError):
                            workspace.execute(["asb"],gate="application",workload_owner=self.owner(),transport_factory=Transport)
                        self.assertIn("kill",container.events)
                        self.assertEqual(workspace.held_executions,[])
                self.assertEqual(released,[] if fault == "metadata" else [True])

    def test_invalid_retention_never_launches_an_execution(self):
        for owner in [{}, dict(self.owner(), guest_namespace=100), dict(self.owner(), token="bad")]:
            workspace, _, api, _ = self.workspace()
            with workspace, self.assertRaises(RUNTIME.ExecutionError):
                workspace.execute(["asb"], gate="application", workload_owner=owner)
            self.assertEqual(api.calls, [])

    def test_collector_attachment_has_its_own_budget_and_is_not_agent_elapsed_time(self):
        workspace, container, _, _ = self.workspace()
        with workspace:
            result = workspace.execute(["agent"], timeout=.02, preparation_timeout=.3,
                                       before_resume=lambda _: time.sleep(.06))
            self.assertGreaterEqual(result["preparation_seconds"], .06)
            self.assertLess(result["elapsed_seconds"], .05)
            self.assertFalse(result["timed_out"])
            self.assertNotIn("kill", container.events)

    def test_expired_or_cancelled_preparation_never_resumes_the_owned_gate(self):
        for cancel in (False, True):
            workspace, container, _, _ = self.workspace()
            cancelled = threading.Event()
            with workspace:
                def attach(_):
                    if cancel:
                        cancelled.set()
                    else:
                        time.sleep(.05)
                with self.assertRaises(RUNTIME.ExecutionError):
                    workspace.execute(["agent"], timeout=1, preparation_timeout=.02,
                                      before_resume=attach, cancelled=cancelled)
                self.assertFalse(container.resumed)
                self.assertIn("kill", container.events)

    def workspace(self):
        container = Container()
        api = API(container)
        creation = []
        def create(**kwargs):
            creation.append(kwargs)
            container.attrs = {"Config": {"Labels": kwargs["labels"]}}
            return container
        client = SimpleNamespace(api=api, containers=SimpleNamespace(create=create))
        image = SimpleNamespace(id="sha256:" + "f" * 64, attrs={"RepoDigests": [], "Os": "linux", "Architecture": "amd64"})
        workspace = RUNTIME.ManagedDockerWorkspace(client, image, "asb-" + "a" * 32, "/task")
        return workspace, container, api, creation

    def test_gate_stops_before_attachment_and_preserves_argv_env_and_output(self):
        workspace, container, api, creation = self.workspace()
        with workspace:
            def attach(target):
                self.assertFalse(container.resumed)
                self.assertEqual(container.events[-1], "stopped")
                self.assertEqual(target["daemon_pid"], 9001)
                self.assertEqual(target["namespace_pid"], 42)
                self.assertEqual(target["pid_scope"], "docker-daemon-host")
                container.events.append("attached")
            result = workspace.execute(["agent", "literal ; $value"], {"FIXTURE_KEY": "fixture-only"}, before_resume=attach)
            self.assertEqual(result["stdout"], b"stdout")
            self.assertEqual(result["stderr"], b"stderr")
            self.assertEqual(result["exit_code"], 0)
            self.assertFalse(result["timed_out"])
            self.assertEqual(api.calls[0]["cmd"][-2:], ["agent", "literal ; $value"])
            self.assertEqual(api.calls[0]["environment"], {"FIXTURE_KEY": "fixture-only"})
            self.assertLess(container.events.index("attached"), container.events.index("resume"))
            self.assertNotIn("volumes", creation[0])
            self.assertNotIn("network_mode", creation[0])
            self.assertNotIn("privileged", creation[0])
            self.assertGreater(creation[0]["pids_limit"], 0)
        self.assertEqual(container.events[-1], "remove")
        self.assertTrue(api.stream.closed)
        self.assertTrue(api.stream._response.closed)
        workspace.close()
        self.assertEqual(container.events.count("remove"), 1)

    def test_gate_accepts_reordered_installer_stderr_before_readiness(self):
        workspace, _, api, _ = self.workspace()
        class StderrFirst(Stream):
            def __init__(self, api):
                super().__init__(api)
                self.diagnostic_sent = False
            def __next__(self):
                if not self.diagnostic_sent:
                    self.diagnostic_sent = True
                    return None, b"installer diagnostic\\n"
                return super().__next__()
        def start(*_args, **_kwargs):
            api.stream = StderrFirst(api)
            return api.stream
        api.exec_start = start
        with workspace:
            result = workspace.execute(["agent"])
        self.assertEqual(result["stderr"], b"installer diagnostic\\nstderr")
        self.assertEqual(result["stdout"], b"stdout")

    def test_gate_failure_preserves_bounded_preparation_diagnostic(self):
        workspace, _, api, _ = self.workspace()
        class FailedBeforeReady(Stream):
            def __next__(self):
                if self.index == 0:
                    self.index += 1
                    return None, b"ASB guest agent could not execute: fixture preparation failed\\n"
                raise StopIteration
        def start(*_args, **_kwargs):
            api.stream = FailedBeforeReady(api)
            return api.stream
        api.exec_start = start
        with workspace:
            with self.assertRaisesRegex(RUNTIME.ExecutionError,
                                        "fixture preparation failed"):
                workspace.execute(["agent"])

    def test_application_gate_preserves_self_contained_adapter_driver(self):
        workspace, _container, api, _ = self.workspace()
        with workspace:
            self.assertEqual(workspace.execute(["/opt/asb/asb", "__asb-agent"], gate="application")["exit_code"], 0)
            self.assertEqual(api.calls[0]["cmd"], ["/opt/asb/asb", "__asb-agent"])

    def alternate_transport(self, api, targets=None, status=7):
        transports = []
        class Transport(RUNTIME.DockerExecTransport):
            def __init__(self,*args):
                super().__init__(*args)
                self.close_calls = 0
                transports.append(self)
            def target(self,namespace_pid):
                target = {"container_id":self.container.id,"exec_id":self.exec_id,
                    "namespace_pid":namespace_pid,"daemon_pid":222,"pid_scope":"docker-daemon-host"}
                target.update(targets or {})
                return target
            def exit_status(self):
                return status
            def close(self):
                self.close_calls += 1
                super().close()
        def forbidden_inspection(*args):
            raise AssertionError("generic execution must not identify the bridge as the agent")
        api.exec_inspect = forbidden_inspection
        return Transport,transports

    def test_alternate_transport_supplies_actual_target_and_child_status_not_bridge(self):
        workspace, container, api, _ = self.workspace()
        factory,transports = self.alternate_transport(api)
        with workspace:
            observed = []
            result = workspace.execute(["agent"],transport_factory=factory,before_resume=observed.append)
            self.assertEqual(observed[0]["daemon_pid"],222)
            self.assertNotEqual(observed[0]["daemon_pid"],api.pid)
            self.assertEqual(result["exit_code"],7)
            self.assertEqual((result["stdout"],result["stderr"]),(b"stdout",b"stderr"))
            self.assertEqual(transports[0].close_calls,1)
            self.assertTrue(api.stream.closed)
            self.assertTrue(api.stream._response.closed)

    def test_namespace_only_transport_never_invents_a_host_pid_or_resumes_refused_collectors(self):
        for measured in [False,True]:
            workspace, container, api, _ = self.workspace()
            factory,transports = self.alternate_transport(api,
                {"daemon_pid":None,"pid_scope":"container-namespace"})
            def attach(target):
                self.assertIsNone(target["daemon_pid"])
                if measured:raise RUNTIME.ExecutionError("host collectors need a verified local process")
            with workspace:
                if measured:
                    with self.assertRaisesRegex(RUNTIME.ExecutionError,"verified local process"):
                        workspace.execute(["agent"],transport_factory=factory,before_resume=attach)
                    self.assertFalse(container.resumed)
                    self.assertIn("kill",container.events)
                else:
                    self.assertEqual(workspace.execute(["agent"],transport_factory=factory,
                        before_resume=attach)["exit_code"],7)
                    self.assertTrue(container.resumed)
                self.assertEqual(transports[0].close_calls,1)

    def test_transport_cannot_change_gate_container_or_launch_identity(self):
        for changed in [{"namespace_pid":43},{"container_id":"another-container"},
                {"exec_id":"another-exec"},{"pid_scope":"unknown"},{"daemon_pid":None},
                {"pid_scope":"container-namespace"}]:
            with self.subTest(changed=changed):
                workspace, container, api, _ = self.workspace()
                factory,transports = self.alternate_transport(api,changed)
                with workspace,self.assertRaisesRegex(RUNTIME.ExecutionError,"contradictory target"):
                    workspace.execute(["agent"],transport_factory=factory)
                self.assertFalse(container.resumed)
                self.assertIn("kill",container.events)
                self.assertEqual(transports[0].close_calls,1)

    def test_transport_start_is_inside_shared_preparation_deadline_and_cancellation(self):
        for cancel in [False,True]:
            workspace,container,api,_ = self.workspace()
            cancelled = threading.Event()
            def start(*args):
                if cancel:cancelled.set()
                self.assertTrue(container.killed.wait(2),"shared watchdog must cover transport construction")
                raise RUNTIME.ExecutionError("transport start interrupted")
            with workspace,self.assertRaisesRegex(RUNTIME.ExecutionError,"start interrupted"):
                workspace.execute(["agent"],transport_factory=start,cancelled=cancelled,
                    timeout=60,preparation_timeout=.02)
            self.assertFalse(container.resumed)
            self.assertEqual(container.events[-4:],["kill","wait","start","remove"])

    def test_preparation_failure_notifies_peers_before_namespace_kill(self):
        for fault in ["transport","attach"]:
            workspace,container,api,_ = self.workspace()
            order = []
            original_kill = container.kill
            def kill():
                order.append("kill")
                original_kill()
            container.kill = kill
            def fail(*_args):
                raise RUNTIME.ExecutionError("preparation failed")
            with workspace,self.assertRaisesRegex(RUNTIME.ExecutionError,"preparation failed"):
                workspace.execute(["agent"],
                    transport_factory=fail if fault == "transport" else None,
                    before_resume=fail if fault == "attach" else None,
                    on_preparation_failure=lambda:order.append("notify"))
            self.assertEqual(order[0],"notify")
            self.assertFalse(container.resumed)

    def test_alternate_transport_stream_uses_shared_execution_deadline_and_cancel(self):
        for cancel in [False,True]:
            workspace,container,api,_ = self.workspace()
            factory,transports = self.alternate_transport(api)
            api.hang = True
            cancelled = threading.Event()
            def attach(_):
                if cancel:cancelled.set()
            with workspace:
                if cancel:
                    with self.assertRaises(RUNTIME.ExecutionError):
                        workspace.execute(["agent"],transport_factory=factory,cancelled=cancelled,
                            before_resume=attach)
                    self.assertFalse(container.resumed)
                else:
                    result = workspace.execute(["agent"],transport_factory=factory,timeout=.02,
                        preparation_timeout=1)
                    self.assertTrue(result["timed_out"])
                self.assertIn("kill",container.events)
                self.assertEqual(transports[0].close_calls,1)

    def test_transport_exit_status_must_describe_a_completed_child(self):
        for status in [None,True,-1,256,"0"]:
            with self.subTest(status=status):
                workspace, container, api, _ = self.workspace()
                factory,transports = self.alternate_transport(api,status=status)
                with workspace,self.assertRaisesRegex(RUNTIME.ExecutionError,"valid child exit status"):
                    workspace.execute(["agent"],transport_factory=factory)
                self.assertIn("kill",container.events)
                self.assertEqual(transports[0].close_calls,1)

    def test_transport_close_failure_quiesces_namespace_instead_of_claiming_completion(self):
        workspace,container,api,_ = self.workspace()
        class FailedClose(RUNTIME.DockerExecTransport):
            def close(self):
                super().close()
                raise RUNTIME.ExecutionError("launcher worker did not settle")
        with workspace,self.assertRaisesRegex(RUNTIME.ExecutionError,"launcher worker"):
            workspace.execute(["agent"],transport_factory=FailedClose)
        self.assertIn("kill",container.events)

    def test_default_transport_close_is_idempotent_but_never_forgets_failure(self):
        class Cancelled(BaseException):pass
        for error_type in [None,ValueError,Cancelled]:
            workspace,container,api,_ = self.workspace()
            transport = RUNTIME.DockerExecTransport(api,container,["agent"],{},"/task")
            closes = []
            def close():
                closes.append("close")
                if error_type:raise error_type("fixture close failure")
            api.stream.close = close
            if error_type:
                with self.assertRaisesRegex(error_type,"fixture close failure"):transport.close()
                with self.assertRaisesRegex(RUNTIME.ExecutionError,"remains unconfirmed"):transport.close()
            else:
                transport.close();transport.close()
            self.assertEqual(closes,["close"])
            self.assertTrue(api.stream._response.closed)

    def test_default_transport_needs_explicit_boolean_running_evidence(self):
        for final in [False,True]:
            for malformed in [None,0,1,"false","true", "missing"]:
                with self.subTest(final=final,malformed=malformed):
                    workspace,container,api,_ = self.workspace()
                    inspect = api.exec_inspect
                    def changed(exec_id):
                        value = inspect(exec_id)
                        if api.completed == final:
                            value.pop("Running")
                            if malformed != "missing":value["Running"] = malformed
                        return value
                    api.exec_inspect = changed
                    with workspace,self.assertRaises(RUNTIME.ExecutionError):
                        workspace.execute(["agent"])
                    if not final:self.assertFalse(container.resumed)
                    self.assertIn("kill",container.events)

    def test_retained_transport_close_failure_is_not_lost_in_background_drain(self):
        class Cancelled(BaseException):pass
        for error_type in [RUNTIME.ExecutionError,Cancelled]:
            workspace,container,api = self.retained_workspace()
            class FailedClose(RUNTIME.DockerExecTransport):
                def close(self):
                    super().close()
                    raise error_type("retained launcher did not settle")
            workspace.__enter__()
            workspace.execute(["agent"],gate="application",transport_factory=FailedClose,
                workload_owner=self.owner())
            with self.assertRaisesRegex(RUNTIME.ExecutionError,"transport closure remains unconfirmed"):
                workspace.close()
            self.assertEqual(workspace.cleanup_status,"error")

    def test_nonzero_exit_preserves_workspace_and_output_is_bounded(self):
        workspace, container, api, _ = self.workspace()
        api.status = 7
        with workspace:
            result = workspace.execute(["agent"], output_limit=3)
            self.assertEqual(result["exit_code"], 7)
            self.assertEqual(result["stdout"], b"std")
            self.assertEqual(result["stderr"], b"std")
            self.assertTrue(result["output_truncated"])
            self.assertNotIn("kill", container.events)

    def test_deadline_stops_owned_tree_and_restarts_workspace_for_submission_capture(self):
        workspace, container, api, _ = self.workspace()
        api.hang = True
        with workspace:
            result = workspace.execute(["agent"], timeout=0.02)
            self.assertEqual(result["exit_code"], 137)
            self.assertTrue(result["timed_out"])
            self.assertEqual(container.events[-3:], ["kill", "wait", "start"])
            api.hang = False
            self.assertEqual(workspace.execute(["git", "diff"])["exit_code"], 0)

    def test_controller_disconnect_cancels_active_execution_without_waiting_for_deadline(self):
        workspace, container, api, _ = self.workspace()
        cancelled = threading.Event()
        api.hang = True
        trigger = threading.Timer(0.03, cancelled.set)
        with workspace:
            trigger.start()
            try:
                result = workspace.execute(["agent"], timeout=60, cancelled=cancelled)
            finally:
                trigger.join(timeout=1)
            self.assertTrue(result["cancelled"])
            self.assertFalse(result["timed_out"])
            self.assertLess(result["elapsed_seconds"], 2)
            self.assertEqual(container.events[-3:], ["kill", "wait", "start"])

    def test_callback_failure_cannot_resume_agent_or_leave_paused_process(self):
        workspace, container, api, _ = self.workspace()
        with workspace:
            def attach(_target):
                raise ValueError("synthetic collector failure")
            with self.assertRaisesRegex(ValueError, "synthetic collector"):
                workspace.execute(["agent"], before_resume=attach)
            self.assertNotIn("resume", container.events)
            self.assertEqual(container.events[-3:], ["kill", "wait", "start"])
            self.assertTrue(api.stream.closed)
            self.assertEqual(workspace.execute(["retry"])["exit_code"], 0)

    def test_invalid_gate_or_target_is_not_resumed(self):
        for field, value in [("ready", b"unrelated output\n"), ("pid", 0)]:
            workspace, container, api, _ = self.workspace()
            setattr(api, field, value)
            with workspace:
                with self.assertRaises(RUNTIME.ExecutionError):
                    workspace.execute(["agent"])
                self.assertNotIn("resume", container.events)
                self.assertIn("kill", container.events)

    def test_invalid_arguments_are_rejected_before_any_exec_is_created(self):
        workspace, _container, api, _ = self.workspace()
        with workspace:
            for argv, env in [([], {}), (["bad\x00"], {}), (["agent"], {"bad-key": "v"}), (["agent"], {None: "v"})]:
                with self.assertRaises(RUNTIME.ExecutionError):
                    workspace.execute(argv, env)
            self.assertEqual(api.calls, [])

    def test_cleanup_refuses_a_different_attempts_container(self):
        workspace, container, _api, _ = self.workspace()
        workspace.__enter__()
        container.attrs["Config"]["Labels"]["asb.attempt"] = "someone-else"
        with self.assertRaisesRegex(RUNTIME.ExecutionError, "outside this attempt"):
            workspace.close()
        self.assertNotIn("remove", container.events)

    def test_unconfirmed_agent_is_reset_only_after_observed_container_exit(self):
        workspace, container, _api, _ = self.workspace()
        with workspace:
            workspace.quiesce_unconfirmed_agent()
            self.assertEqual(container.events[-3:], ["kill", "wait", "start"])
            def failed_wait(timeout):
                raise RuntimeError("synthetic stop failure")
            container.wait = failed_wait
            with self.assertRaisesRegex(RUNTIME.ExecutionError, "confirm stopped"):
                workspace.quiesce_unconfirmed_agent()
            self.assertEqual(container.events[-1], "kill")

    def test_unconfirmed_agent_reset_refuses_other_owners_or_shared_pid_namespaces(self):
        for change in ["owner", "pid"]:
            workspace, container, _api, _ = self.workspace()
            workspace.__enter__()
            if change == "owner":
                container.attrs["Config"]["Labels"]["asb.attempt"] = "another-attempt"
            else:
                container.attrs["HostConfig"] = {"PidMode":"host"}
            with self.assertRaisesRegex(RUNTIME.ExecutionError, "owned PID namespace"):
                workspace.quiesce_unconfirmed_agent()
            self.assertNotIn("kill", container.events)

    def test_attach_refuses_shared_container_pid_namespaces(self):
        workspace, container, _api, creation = self.workspace()
        workspace.__enter__()
        container.image = workspace.image
        for mode in ["host", "container:some-other-container"]:
            container.attrs["HostConfig"] = {"PidMode":mode}
            with self.assertRaisesRegex(RUNTIME.ExecutionError, "private container PID namespace"):
                RUNTIME.ManagedDockerWorkspace.attach(workspace.client, container, workspace.attempt, "/task")
        workspace.close()


if __name__ == "__main__":
    unittest.main()
