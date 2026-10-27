"""Offline bootstrap contracts; no Docker daemon, installed client or provider."""
import io
import hashlib
import json
from pathlib import Path
import struct
import sys
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / "assets/benchmarks"))
from asb_execution import ExecutionError
from asb_guest import (execute_agent, export_agent_artifacts, stage_npm_package_tree,
                       stage_public_cache, stage_bootstrap, PreparationBudget)
from asb_runtime import elf as inspect_elf


def elf(interpreter=False):
    data = bytearray(120)
    data[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<H",data,16,2)
    struct.pack_into("<H",data,18,62)
    struct.pack_into("<I",data,20,1)
    struct.pack_into("<Q", data, 32, 64)
    struct.pack_into("<HHH", data, 52, 64, 56, 1)
    struct.pack_into("<I", data, 64, 3 if interpreter else 1)
    return data


class Container:
    def __init__(self):
        self.files = []
        self.id = "guest-bootstrap-fixture"
        self.attrs = {"Config":{"User":""}}

    def put_archive(self, destination, source):
        if isinstance(source, bytes):
            source = io.BytesIO(source)
        with tarfile.open(fileobj=source, mode="r") as archive:
            self.files.extend((destination, item.name, archive.extractfile(item).read() if item.isfile() else None)
                              for item in archive.getmembers())
        return True


class ArchiveContainer:
    def __init__(self, name, payload):
        self.name, self.payload = name, payload

    def get_archive(self, source, chunk_size=65536, encode_stream=False):
        if source.rsplit("/",1)[-1] != self.name:
            raise AssertionError("unexpected guest artifact source")
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive,mode="w") as outgoing:
            member = tarfile.TarInfo(self.name)
            member.size = len(self.payload)
            outgoing.addfile(member,io.BytesIO(self.payload))
        class Chunks:
            def __init__(self, bytes):self.stream=io.BytesIO(bytes)
            def __iter__(self):
                while data := self.stream.read(65536):yield data
            def close(self):self.stream.close()
        return Chunks(archive.getvalue()), {"name":self.name,"linkTarget":""}


class GuestBootstrapTests(unittest.TestCase):
    def setUp(self):
        # These fixtures exercise bootstrap selection, not the separately
        # tested ownership transport or actual Docker filesystem staging.
        for name, options in [
            ("asb_staging.configured_owner",dict(return_value=(0,0))),
            ("asb_staging.verify_runtime",dict(return_value=(0,0))),
            ("asb_guest.root_execute",dict(return_value=b"")),
            ("asb_guest.prepare_guest_roots",dict(return_value=({},None))),
        ]:
            mock = patch(name,**options)
            mock.start()
            self.addCleanup(mock.stop)

    def test_guest_artifact_export_copies_only_receipt_bound_archive_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            payload = b"sealed MCP relay transcript"
            digest = hashlib.sha256(payload).hexdigest()
            task = SimpleNamespace(workspace=SimpleNamespace(container=ArchiveContainer("artifact-0",payload)))
            export_agent_artifacts(task,directory,[{"kind":"mcp-relay-filesystem","filename":"artifact-0",
                "bytes":len(payload),"sha256":digest}])
            exported = Path(directory) / "artifact-0"
            self.assertEqual(exported.read_bytes(),payload)
            self.assertEqual(exported.stat().st_mode & 0o777,0o600)

    def test_guest_artifact_export_rejects_changed_or_oversized_receipts(self):
        with tempfile.TemporaryDirectory() as directory:
            payload = b"x"
            task = SimpleNamespace(workspace=SimpleNamespace(container=ArchiveContainer("artifact-0",payload)))
            for receipt in [
                {"kind":"mcp-relay-filesystem","filename":"artifact-0","bytes":1,"sha256":"a"*64},
                {"kind":"mcp-relay-filesystem","filename":"../artifact-0","bytes":1,"sha256":"a"*64},
                {"kind":"mcp-relay-filesystem","filename":"artifact-0","bytes":16*1024*1024+1,"sha256":"a"*64},
            ]:
                with self.subTest(receipt=receipt), self.assertRaises(ExecutionError):
                    export_agent_artifacts(task,directory,[receipt])
                self.assertFalse(any(path.name.endswith(".part") for path in Path(directory).iterdir()))
    def test_preparation_budget_is_bounded_anchored_and_not_restarted(self):
        for invalid in [True, False, 0, -1, 1801, float("nan"), float("inf"), "900"]:
            with self.subTest(invalid=invalid), self.assertRaises(ExecutionError):
                PreparationBudget(invalid)
        now = [100.0]
        with patch("asb_guest.time.monotonic", side_effect=lambda:now[0]):
            budget = PreparationBudget(1800)
            now[0] += 1000
            self.assertEqual(budget.remaining(),800)
            now[0] += 801
            with self.assertRaisesRegex(ExecutionError,"deadline exceeded"):budget.remaining()
            self.assertTrue(budget.expired())

    def test_slow_bootstrap_and_original_session_share_budget_without_extending_agent_runtime(self):
        now, calls, deadlines = [100.0], [], []
        with tempfile.TemporaryDirectory() as directory, patch("asb_guest.time.monotonic", side_effect=lambda:now[0]):
            root = Path(directory)
            (root/".asb").mkdir()
            (root/".asb/project.toml").write_text("tools_dir='tools'\n")
            binary = root/"asb"
            binary.write_bytes(elf())
            def execute(argv, **options):
                calls.append((argv,options))
                if argv[-1] == "__asb-agent":
                    self.assertEqual(options["preparation_timeout"],1000)
                    self.assertEqual(options["timeout"],12)
                    now[0] += 200  # Beyond both former 60/90-second waits.
                    options["before_resume"]({"namespace_pid":42})
                    now[0] += 2000
                    self.assertFalse(options["cancelled"].is_set(), "preparation budget must not become an agent runtime limit")
                else:
                    now[0] += 400
                return dict(stdout=b"",stderr=b"",exit_code=0,timed_out=False,cancelled=False)
            def transport(request):
                deadlines.append(request["preparation_deadline"])
                return SimpleNamespace(inherit_working_directory=True)
            task = SimpleNamespace(artifacts=root,agent_timeout=12,prepare_agent_transport=transport,
                workspace=SimpleNamespace(container=Container(),execute=execute),
                checked=lambda argv,**_:json.dumps(dict(schema_version=1,tree_quiesced=True,token_usage={})).encode() if argv[0] == "cat" else b"")
            request = dict(binary=str(binary),project_root=str(root),tools_root=str(root/"tools"),preparation_seconds=1800,
                bootstrap=dict(runtime_tools=["node"],assets=[]),request=dict(config=dict(agent="pi")))
            execute_agent(task,request,None,None,None)
            self.assertEqual([call[0][-1] for call in calls],["node","pi","__asb-agent"])
            self.assertEqual(deadlines,[1900])

    def test_expired_bootstrap_notifies_before_relay_teardown_and_never_launches_client(self):
        now, order = [0.0], []
        def expired(_task,_request,_cancelled,resources,_relays,budget):
            resources.callback(lambda:order.append("relay-close"))
            now[0] = 1900
            return {},{}
        task = SimpleNamespace(prepare_agent_transport=lambda _:self.fail("expired bootstrap reached client preparation"))
        with patch("asb_guest.time.monotonic",side_effect=lambda:now[0]), patch("asb_guest.prepare_agent",side_effect=expired):
            with self.assertRaisesRegex(ExecutionError,"deadline exceeded"):
                execute_agent(task,dict(preparation_seconds=1800),None,None,None,
                    on_preparation_failure=lambda:order.append("notify-peers"))
        self.assertEqual(order,["notify-peers","relay-close"])

    def test_owned_service_handoff_uses_guest_metadata_without_reset_or_duplicate_end(self):
        import os
        with tempfile.TemporaryDirectory() as directory:
            controller = os.stat("/proc/self/ns/pid").st_ino
            container = SimpleNamespace(
                attrs={"Config":{"Labels":{"asb.owner":"asb", "asb.attempt":"owned"}}, "HostConfig":{"PidMode":"private"}},
                reload=lambda:None,
                exec_run=lambda argv:SimpleNamespace(exit_code=0, output=str(controller+1).encode()))
            calls, ends = [], []
            def execute(_argv, **options):
                request = json.loads(options["environment"]["ASB_GUEST_AGENT_REQUEST"])
                self.assertEqual(request["workload_owner"], options["workload_owner"])
                self.assertNotEqual(request["workload_owner"]["guest_namespace"], controller)
                result = {"stdout":b"", "stderr":b"", "exit_code":0, "elapsed_seconds":.123,
                    "workload_processes_retained":True, "metadata":{
                        "workload_owner":request["workload_owner"], "tree_quiesced":False}}
                options["on_execution_end"](dict(result))
                calls.append(options)
                return result
            def forbidden(*_args, **_kwargs):
                raise AssertionError("confirmed service handoff must not reset the container or reread unbound metadata")
            task = SimpleNamespace(artifacts=Path(directory), agent_timeout=5, checked=forbidden,
                workspace=SimpleNamespace(attempt="owned", container=container, execute=execute, quiesce_unconfirmed_agent=forbidden))
            with patch("asb_guest.prepare_agent", return_value=({"process_lifetime":"through-evaluation"}, {})):
                result = execute_agent(task, {}, None, None, None, on_ended=ends.append)
            self.assertTrue(result["workload_processes_retained"])
            self.assertEqual(len(calls), 1)
            self.assertEqual(len(ends), 1)
            self.assertEqual(json.loads((task.artifacts / "execution.json").read_text())["agent_tree_cleanup"],
                             "delegated-owned-workload")

    def test_shared_or_unowned_namespace_cannot_retain_services(self):
        import os
        for mode, owner, namespace in [("host", "asb", 99), ("private", "foreign", 99),
                                        ("private", "asb", os.stat("/proc/self/ns/pid").st_ino)]:
            with self.subTest(mode=mode, owner=owner), tempfile.TemporaryDirectory() as directory:
                container = SimpleNamespace(
                    attrs={"Config":{"Labels":{"asb.owner":owner,"asb.attempt":"owned"}}, "HostConfig":{"PidMode":mode}},
                    reload=lambda:None,
                    exec_run=lambda argv:SimpleNamespace(exit_code=0, output=str(namespace).encode()))
                def forbidden(*_args, **_kwargs):
                    raise AssertionError("invalid lifetime launched a client")
                task = SimpleNamespace(artifacts=Path(directory), workspace=SimpleNamespace(
                    attempt="owned", container=container, execute=forbidden))
                with patch("asb_guest.prepare_agent", return_value=({"process_lifetime":"through-evaluation"}, {})):
                    with self.assertRaises(ExecutionError):
                        execute_agent(task, {}, None, None, None)
                self.assertFalse((task.artifacts / "execution.json").exists())

    def test_unconfirmed_tree_cannot_publish_a_result_when_container_reset_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def unavailable(_argv, **_kwargs):
                raise ExecutionError("guest result is missing")
            def failed_reset():
                raise ExecutionError("could not confirm stopped agent tree")
            task = SimpleNamespace(artifacts=root, agent_timeout=15, checked=unavailable,
                workspace=SimpleNamespace(container=Container(), quiesce_unconfirmed_agent=failed_reset,
                    execute=lambda *_args, **_kwargs: {"stdout":b"", "stderr":b"", "exit_code":2}))
            with patch("asb_guest.prepare_agent", return_value=({}, {})):
                with self.assertRaisesRegex(ExecutionError, "confirm stopped"):
                    execute_agent(task, {}, None, None, None)
            self.assertFalse((root / "execution.json").exists())

    def test_missing_first_result_keeps_nonretained_workspace_eligible_after_confirmed_reset(self):
        # Production guest fallback with a fake task's consuming grader seam;
        # no Docker daemon, original SDK verifier or live provider.
        with tempfile.TemporaryDirectory() as directory:
            calls = []
            def unavailable(_argv, **_kwargs):
                raise ExecutionError("first result publication unavailable")
            def execute(*_args, **_kwargs):
                calls.append("client")
                return {"stdout":b"", "stderr":b"", "exit_code":1}
            task = SimpleNamespace(artifacts=Path(directory), agent_timeout=15,
                checked=unavailable, workspace=SimpleNamespace(container=Container(),
                    execute=execute, quiesce_unconfirmed_agent=lambda:calls.append("confirmed-reset")),
                evaluate=lambda:calls.append("grade"))
            with patch("asb_guest.prepare_agent", return_value=({}, {})):
                result = execute_agent(task, {}, None, None, None)
            self.assertTrue(result["metadata"]["tree_quiesced"])
            self.assertIsNone(result["metadata"]["final_response"])
            self.assertEqual(result["metadata"]["token_usage"], {})
            task.evaluate()
            self.assertEqual(calls, ["client", "confirmed-reset", "grade"])

    def test_local_forwarding_precedes_install_and_is_closed_on_install_failure_or_success(self):
        for failure in (False, True):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / ".asb").mkdir()
                (root / ".asb/project.toml").write_text("tools_dir='tools'\n")
                binary = root / "asb"
                binary.write_bytes(elf())
                container = Container()
                container.id = "owned-container-fixture"
                container.exec_run = lambda _: SimpleNamespace(exit_code=1)
                calls, resources = [], []
                class Relay:
                    ports = (10001, 10002)
                    closed = False
                    def __init__(self, targets, command, cancellation, diagnostic_path):
                        self.targets = targets
                        self.cancellation = cancellation
                        self.diagnostic_path = diagnostic_path
                        resources.append(self)
                    def close(self):
                        self.closed = True
                    def check(self):
                        if self.closed:
                            raise ExecutionError("relay closed")
                def execute(argv, **options):
                    self.assertFalse(resources[0].closed)
                    self.assertEqual(options["environment"]["HTTP_PROXY"], "http://localhost:10002")
                    calls.append((argv, options))
                    return {"stdout":b"", "stderr":b"", "exit_code":1 if failure else 0,
                            "timed_out":False, "cancelled":False}
                def checked(argv, **_options):
                    return json.dumps({"schema_version":1,"exit_code":0,"final_response":"done",
                                       "token_usage":{},"steps":None,"tree_quiesced":True}).encode() if argv[0] == "cat" else b""
                task = SimpleNamespace(workspace=SimpleNamespace(container=container, execute=execute),
                                       artifacts=root, checked=checked)
                request = {"binary":str(binary),"project_root":str(root),"tools_root":str(root/"tools"),
                    "project_configuration":"tools_dir='tools'\n[proxy]\nhttp='http://localhost:3128'\n",
                    "environment":{"HTTP_PROXY":"http://localhost:3128"},
                    "bootstrap":{"runtime_tools":[],"assets":[]},
                    "request":{"config":{"agent":"pi"},"connection":{"base_url":"http://localhost:8000/v1"}}}
                with patch("asb_guest.HostRelay", Relay), patch("asb_docker.docker_command", return_value=["docker-fixture"]):
                    if failure:
                        with self.assertRaises(ExecutionError):
                            execute_agent(task,request,None,None,None)
                    else:
                        execute_agent(task,request,None,None,None)
                self.assertTrue(resources[0].closed)
                self.assertEqual(resources[0].targets, [("localhost",8000),("localhost",3128)])
                self.assertIsNone(resources[0].cancellation)
                self.assertEqual(resources[0].diagnostic_path, root / "endpoint-relay-0.json")
                project = next(data for destination,name,data in container.files
                               if destination == "/opt/asb-project/.asb" and name == "project.toml")
                self.assertIn(b"http://localhost:10002",project)
                if not failure:
                    guest = json.loads(calls[-1][1]["environment"]["ASB_GUEST_AGENT_REQUEST"])
                    self.assertEqual(guest["connection"]["base_url"],"http://localhost:10001/v1")

    def test_declared_assets_and_runtime_order_do_not_depend_on_agent_names(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root/"tools"
            source = tools/"example/cache/package.tar.gz"
            source.parent.mkdir(parents=True)
            source.write_bytes(b"public pinned distribution fixture")
            container = Container()
            calls = []
            task = SimpleNamespace(workspace=SimpleNamespace(container=container),artifacts=root,
                                   checked=lambda argv:calls.append(argv))
            plan = {"runtime_tools":["example-runtime"],"assets":[{"source":str(source),"target":"example/cache/package.tar.gz"}]}
            self.assertEqual(stage_bootstrap(task,plan,tools),["example-runtime"])
            self.assertEqual(container.files,[("/opt/asb-project/.asb-tools","example/cache/package.tar.gz",source.read_bytes())])
            receipt = json.loads((root/"bootstrap.json").read_text())
            self.assertNotIn(str(root),json.dumps(receipt))
            self.assertEqual(receipt["assets"][0]["bytes"],len(source.read_bytes()))

    def test_bootstrap_rejects_escape_symlinks_duplicate_targets_and_invalid_dependencies(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root/"tools"
            tools.mkdir()
            source = tools/"package"
            source.write_bytes(b"fixture")
            link = tools/"link"
            link.symlink_to(source)
            external = root/"private"
            external.write_bytes(b"must-not-be-staged")
            assets = [{"source":str(source),"target":"../escape"},{"source":str(source),"target":"/escape"},
                      {"source":str(external),"target":"package"},{"source":str(link),"target":"package"}]
            for asset in assets:
                container = Container()
                task = SimpleNamespace(workspace=SimpleNamespace(container=container),artifacts=root,checked=lambda _:None)
                with self.subTest(asset=asset),self.assertRaises(ExecutionError):
                    stage_bootstrap(task,{"runtime_tools":[],"assets":[asset]},tools)
                self.assertEqual(container.files,[])
            valid = {"source":str(source),"target":"package"}
            for plan in [{"runtime_tools":[],"assets":[valid,valid]},
                         {"runtime_tools":["runtime","runtime"],"assets":[]},
                         {"runtime_tools":["../bad"],"assets":[]},
                         {"runtime_tools":[],"assets":[],"public_package_cache":"not-a-boolean"}, {}]:
                with self.assertRaises(ExecutionError):
                    stage_bootstrap(task,plan,tools)

    def test_runtime_parser_accepts_static_elf_and_rejects_malformed_files(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "asb"
            binary.write_bytes(elf())
            self.assertEqual(inspect_elf(binary),(elf(),62,None))
            for invalid in [elf(True), b"#!/bin/sh\n", elf()[:100]]:
                binary.write_bytes(invalid)
                with self.assertRaises(ExecutionError):
                    inspect_elf(binary)

    def test_registry_cache_with_credentials_is_never_staged(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cache = root / ".asb-cache/npm/_cacache/index-v5"
            cache.mkdir(parents=True)
            (cache / "entry").write_text('hash\t' + json.dumps({"metadata":{"reqHeaders":{"Authorization":"private-fixture"}}}))
            container = Container()
            stage_public_cache(container, root)
            self.assertEqual(container.files, [])
            (cache / "entry").write_text('hash\t' + json.dumps({"metadata":{"reqHeaders":{}}}))
            stage_public_cache(container, root)
            self.assertTrue(any(name == "npm/_cacache/index-v5/entry" for _, name, _ in container.files))

    def test_npm_package_tree_is_bounded_selected_and_never_a_host_mount(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / "tools"
            archive = tools / "cline/cache/guest-node-modules.tar"
            archive.parent.mkdir(parents=True)
            with tarfile.open(archive, "w") as output:
                directory_member = tarfile.TarInfo("node_modules/.bin")
                directory_member.type = tarfile.DIRTYPE
                output.addfile(directory_member)
                payload = b"#!/bin/sh\nexit 0\n"
                target = tarfile.TarInfo("node_modules/@cline/cli-linux-x64/bin/cline")
                target.mode = 0o755
                target.size = len(payload)
                output.addfile(target, io.BytesIO(payload))
                launcher = tarfile.TarInfo("node_modules/.bin/cline")
                launcher.type = tarfile.SYMTYPE
                launcher.linkname = "../@cline/cli-linux-x64/bin/cline"
                output.addfile(launcher)
                postinstall = tarfile.TarInfo("node_modules/cline/bin/.cline")
                postinstall.type = tarfile.LNKTYPE
                postinstall.linkname = "node_modules/@cline/cli-linux-x64/bin/cline"
                output.addfile(postinstall)
            container = Container()
            checked = []
            task = SimpleNamespace(workspace=SimpleNamespace(container=container), artifacts=root,
                                   checked=lambda argv: checked.append(argv))
            tree = {"source":str(archive), "target":"cline", "executable":"cline"}
            stage_npm_package_tree(task, tree, tools, "cline")
            self.assertEqual(checked, [
                ["mkdir", "-p", "/opt/asb-project/.asb-tools/cline"],
                ["test", "-x", "/opt/asb-project/.asb-tools/cline/node_modules/.bin/cline"],
            ])
            self.assertTrue(any(destination == "/opt/asb-project/.asb-tools/cline"
                                and name == "node_modules/.bin/cline"
                                for destination,name,_ in container.files))
            receipt = json.loads((root / "npm-package-tree.json").read_text())
            self.assertEqual(receipt["target"], "cline")
            self.assertNotIn(str(tools), json.dumps(receipt))
            with self.assertRaises(ExecutionError):
                stage_npm_package_tree(task, tree, tools, "other-agent")

    def test_bootstrap_uses_exact_selected_tool_and_private_request_not_argv(self):
        for timed_out, measured, confirmed in [(False, False, True), (True, False, False), (False, True, True), (False, False, False)]:
            with self.subTest(timed_out=timed_out, measured=measured), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / ".asb").mkdir()
                (root / ".asb/project.toml").write_text("tools_dir='tools'\n")
                binary = root / "asb"
                binary.write_bytes(elf())
                container = Container()
                calls = []
                probes, attached = [], []
                def probe(argv):
                    probes.append(argv)
                    return SimpleNamespace(exit_code=0, output=b'{"namespace_pid":42,"start_time_ticks":123,"boot_id":"fixture"}')
                container.exec_run = probe

                def execute(argv, **options):
                    calls.append((argv, options))
                    if argv[-1] == "__asb-agent":
                        options["before_resume"]({"namespace_pid":42})
                    return {"stdout":b"out", "stderr":b"err", "exit_code":124 if timed_out and argv[-1] == "__asb-agent" else 0,
                            "timed_out":timed_out and argv[-1] == "__asb-agent", "cancelled":False}

                def checked(argv, **_options):
                    if argv[0] == "cat":
                        if timed_out:
                            raise ExecutionError("no completed metadata")
                        return json.dumps({"schema_version":1,"exit_code":0,"final_response":"done", "token_usage":{},"steps":None,
                                           "tree_quiesced":confirmed}).encode()
                    return b""

                resets = []
                task = SimpleNamespace(workspace=SimpleNamespace(container=container, execute=execute,
                                       quiesce_unconfirmed_agent=lambda:resets.append("stopped")),
                                       artifacts=root, checked=checked, agent_timeout=1234)
                request = {"binary":str(binary), "project_root":str(root), "tools_root":str(root / "tools"),
                           "measure_target":measured,
                           "bootstrap":{"runtime_tools":["node"],"assets":[]},
                           "request":{"config":{"agent":"pi"},"connection":{"credential":"private-fixture"}}}
                ended = []
                result = execute_agent(task, request, attached.append, lambda *_: None, None,
                                       on_ended=lambda result: ended.append(result["exit_code"]))
                self.assertEqual(resets, [] if confirmed else ["stopped"])
                self.assertTrue(result["metadata"]["tree_quiesced"])
                journal = json.loads((root / "execution.json").read_text())
                self.assertEqual(journal["agent_tree_cleanup"], "supervisor-confirmed" if confirmed else "owned-container-reset")
                self.assertEqual([call[0][-1] for call in calls], ["node", "pi", "__asb-agent"])
                self.assertNotIn("private-fixture", repr([call[0] for call in calls]))
                private = calls[-1][1]["environment"]["ASB_GUEST_AGENT_REQUEST"]
                self.assertEqual(json.loads(private)["connection"]["credential"], "private-fixture")
                self.assertEqual(json.loads(private)["timeout_seconds"], 1234)
                self.assertEqual(calls[-1][1]["timeout"], 1234)
                self.assertEqual(calls[-1][1]["gate"], "application")
                self.assertEqual(calls[-1][1]["preparation_timeout"], 90)
                self.assertEqual(ended, [124 if timed_out else 0])
                self.assertEqual(len(probes), int(measured))
                self.assertEqual(len(attached), 1)
                if measured:
                    self.assertEqual(probes[0], ["/opt/asb-runtime/asb", "__asb-target", "42"])
                    self.assertEqual(attached[0]["process_identity"]["start_time_ticks"], 123)
                self.assertIsNone(result["metadata"]["token_usage"].get("total_tokens"))
                self.assertEqual(result["metadata"]["exit_code"], 124 if timed_out else 0)


if __name__ == "__main__":
    unittest.main()
