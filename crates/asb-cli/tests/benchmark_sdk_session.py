"""Provider-free SDK launch protocol fixtures; not an installed-SDK proof."""
import json
import io
from pathlib import Path
import shlex
import subprocess
import sys
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

sys.dont_write_bytecode = True
sys.path.insert(0,str(Path(__file__).parents[1]/"assets/benchmarks"))
from benchmark_execution import Container as BaseContainer, API
from asb_execution import ExecutionError, ManagedDockerWorkspace
from asb_material import MaterialError
from asb_terminal_session import SDKSessionLauncher, _publish_receipt, _receive_release


class Container(BaseContainer):
    def __init__(self):
        super().__init__()
        self.attrs = {"Config":{"User":"worker","Labels":{"asb.owner":"asb","asb.attempt":"asb-"+"a"*32}}}
        self.image = SimpleNamespace(id="sha256:"+"f"*64,attrs={"Os":"linux","Architecture":"amd64"})
        self.frames,self.archives,self.nonce = {},[],None
        self.ancestry_valid = False
        self.candidates = {"Titles":["PID"],"Processes":[["222"],["9001"]]}
    def exec_run(self,argv,user=None):
        result = SimpleNamespace(exit_code=0,output=b"")
        if argv[0] == "id":result.output = b"1001\n" if argv[1] == "-u" else b"1002\n"
        elif "asb-session" in argv:self.nonce = argv[-1].split("asb-session-")[-1]
        elif "asb-session-descendant" in argv:
            result.exit_code = 0 if self.ancestry_valid else 1
        elif "asb-session-receipt" in argv:
            name = argv[-1].rsplit("/",1)[1]
            fields = {"started":"42","parent":"9","exit":"42 7","returned":"9 0","released":"42"}
            result.output = self.frames.get(name,(self.nonce+" "+fields[name]+"\n").encode())
        elif argv[0] == "/opt/asb-runtime/asb":
            result.output = json.dumps({"boot_id":"01234567-89ab-cdef-0123-456789abcdef",
                "namespace_pid":42,"start_time_ticks":123}).encode()
        elif argv[0] != "timeout":return super().exec_run(argv)
        return result
    def put_archive(self,destination,source):
        with tarfile.open(fileobj=source,mode="r") as archive:
            for entry in archive.getmembers():
                self.archives.append((destination,entry.uid,entry.gid,entry.mode,archive.extractfile(entry).read()))
        return True
    def top(self,ps_args):
        assert ps_args == "-eo pid"
        return self.candidates


class SDKSessionTests(unittest.TestCase):
    def test_original_session_keeps_absolute_shared_deadline_after_slow_preparation(self):
        from asb_terminal_session import SessionTransport
        now = [100.0]
        with patch("asb_terminal_session.time.monotonic",side_effect=lambda:now[0]):
            launcher = SDKSessionLauncher(None,"fixture",None,preparation_deadline=1900)
            now[0] = 1100
            transport = SessionTransport(launcher,None,None)
            self.assertEqual(transport.deadline,1900)
            self.assertGreater(transport.deadline-now[0],90)
            for invalid in [True,float("nan"),float("inf"),1100,3000]:
                with self.subTest(invalid=invalid),self.assertRaises(ExecutionError):
                    SDKSessionLauncher(None,"fixture",None,preparation_deadline=invalid)

    def test_missing_or_dead_original_shell_cannot_mark_verifier_started(self):
        import asb_terminal_bench_1 as terminal
        for session in [None,SimpleNamespace(is_session_alive=lambda:False)]:
            with self.subTest(session=session),tempfile.TemporaryDirectory() as root:
                task = terminal.TerminalBenchTask(None,"catalog","task",root)
                task.invocation = {"identity":{"task_id":"task"}}
                task.handler = SimpleNamespace(task=SimpleNamespace(run_tests_in_same_shell=True))
                task.agent_session = session
                with patch.dict(sys.modules,{"terminal_bench.agents.failure_mode":SimpleNamespace(FailureMode=object())}), \
                        patch.object(terminal,"record_grading_start") as started:
                    with self.assertRaisesRegex(ExecutionError,"original same-shell agent session"):
                        task.grade_environment()
                started.assert_not_called()

    def test_task_cleanup_settles_launcher_even_when_other_finalizers_fail(self):
        import asb_terminal_bench_1 as terminal
        from types import ModuleType
        package,subpackage,module = [ModuleType(name) for name in
            ["terminal_bench","terminal_bench.terminal","terminal_bench.terminal.terminal"]]
        package.__path__,subpackage.__path__ = [],[]
        package.terminal,subpackage.terminal = subpackage,module
        for fault in ["held","journal","record","serve-held"]:
            with self.subTest(fault=fault),tempfile.TemporaryDirectory() as root:
                task = terminal.TerminalBenchTask(None,"catalog","task",root)
                task.interrupted = True
                order = []
                def finalizer(name,failed=False):
                    def close():
                        order.append(name)
                        if failed:raise ExecutionError(name+" failure")
                    return Mock(side_effect=close)
                task.workspace = SimpleNamespace(join_held_executions=finalizer("held",fault in ["held","serve-held"]))
                task.journal = SimpleNamespace(preserve=Mock(),close=finalizer("journal",fault == "journal"))
                task.agent_launcher = SimpleNamespace(close=finalizer("launcher"))
                with patch.dict(sys.modules,{"docker":ModuleType("docker"),"terminal_bench":package,
                        "terminal_bench.terminal":subpackage,"terminal_bench.terminal.terminal":module}), \
                        patch.object(terminal,"write_json",side_effect=ExecutionError("record failure") if fault == "record" else None):
                    if fault == "serve-held":
                        from asb_task_server import serve
                        task.invocation = {}
                        output = io.StringIO()
                        with patch.object(terminal.TerminalBenchTask,"__enter__",return_value=task):
                            self.assertEqual(serve(task,io.StringIO(""),output,idle_timeout=.1),1)
                        closed = [json.loads(line) for line in output.getvalue().splitlines()
                            if json.loads(line).get("event") == "closed"]
                        self.assertEqual(closed,[{"event":"closed","cleanup":"cleanup_failed"}])
                    else:
                        with self.assertRaises(ExecutionError):task.__exit__()
                task.agent_launcher.close.assert_called_once_with()
                task.workspace.join_held_executions.assert_called_once_with()
                task.journal.close.assert_called_once_with()
                self.assertEqual(task.cleanup_status,"cleanup_failed")
                self.assertEqual(task.workspace.cleanup_status,"cleanup_failed")
                self.assertEqual(order,["held","launcher","journal"],"owner lock must span transport settlement attempts")

    def test_release_reader_bounds_raw_bytes_and_requires_exact_nonce_newline(self):
        nonce = "a"*32
        for frame in [nonce.encode()+b"\n",nonce.encode(),b"b"*32+b"\n",
                nonce.encode()+b"\nextra",b"x"*100000,nonce.encode()+b"\0\n",b"\0"*100000]:
            with self.subTest(frame=frame[:40]),tempfile.TemporaryDirectory() as root:
                (Path(root)/"release").write_bytes(frame)
                result = subprocess.run(["/bin/bash","-c",
                    "asb_child_pid=42; "+_receive_release(root,nonce)],capture_output=True,timeout=3)
                valid = frame == nonce.encode()+b"\n"
                self.assertEqual(result.returncode == 0,valid)
                self.assertLessEqual((Path(root)/"release-input.tmp").stat().st_size,34)
                if valid:
                    self.assertEqual((Path(root)/"released").read_bytes(),nonce.encode()+b" 42\n")
                else:self.assertFalse((Path(root)/"released").exists())

    def test_only_true_same_shell_tasks_lazily_own_the_original_sdk_launcher(self):
        from asb_terminal_bench_1 import TerminalBenchTask
        for same_shell in [False,True]:
            with self.subTest(same_shell=same_shell):
                task = TerminalBenchTask(None,"catalog","task","artifacts")
                created = []
                session = SimpleNamespace()
                def create(name,*,as_configured_user):
                    self.assertEqual((name,as_configured_user),("agent",True))
                    created.append(name)
                    return session
                task.handler = SimpleNamespace(task=SimpleNamespace(run_tests_in_same_shell=same_shell))
                task.terminal = SimpleNamespace(create_session=create)
                self.assertIsNone(task.agent_session)
                launcher = task.prepare_agent_transport({"binary":"/frozen/asb","measure_target":True})
                if same_shell:
                    self.assertIs(launcher,task.agent_launcher)
                    self.assertIs(launcher.session,session)
                    self.assertTrue(launcher.inherit_working_directory)
                    self.assertTrue(launcher.require_host_target)
                    with self.assertRaisesRegex(ExecutionError,"already initialized"):
                        task.prepare_agent_transport({"binary":"/frozen/asb"})
                    launcher.close()
                    self.assertEqual(created,["agent"])
                else:
                    self.assertIsNone(launcher)
                    self.assertEqual(created,[])

    def test_receipts_publish_complete_private_bytes_without_overwriting(self):
        with tempfile.TemporaryDirectory() as root:
            for name,variables,expected in [
                    ("parent",("asb_parent",),b"nonce 9\n"),
                    ("started",("asb_child_pid",),b"nonce 42\n"),
                    ("exit",("asb_child_pid","asb_child_status"),b"nonce 42 7\n"),
                    ("returned",("asb_parent",),b"nonce 9\n")]:
                with self.subTest(receipt=name):
                    final = Path(root)/name
                    temporary = Path(root)/(name+".tmp")
                    # Intercept exactly the publication boundary: final must
                    # still be absent and the staged file already complete.
                    script = ('asb_parent=9; asb_child_pid=42; asb_child_status=7; '
                        'ln() { test "$1" = -T && test "$2" = -- && '
                        'test ! -e "$4" && test "$(cat "$3")" = '+
                        shlex.quote(expected.decode().strip())+
                        ' && command ln "$@"; }; '+_publish_receipt(root,name,"nonce",*variables))
                    subprocess.run(["/bin/sh","-c",script],check=True,capture_output=True)
                    self.assertEqual(final.read_bytes(),expected)
                    self.assertEqual(final.stat().st_mode & 0o777,0o600)
                    # A second publication cannot replace either staged or
                    # published bytes, including an existing symlink target.
                    failed = subprocess.run(["/bin/sh","-c",script],capture_output=True)
                    self.assertNotEqual(failed.returncode,0)
                    self.assertEqual(final.read_bytes(),expected)
                    temporary.unlink()
                    sentinel = Path(root)/(name+"-sentinel")
                    sentinel.write_bytes(b"preserve")
                    temporary.symlink_to(sentinel)
                    failed = subprocess.run(["/bin/sh","-c",script],capture_output=True)
                    self.assertNotEqual(failed.returncode,0)
                    self.assertEqual(sentinel.read_bytes(),b"preserve")
                    self.assertEqual(final.read_bytes(),expected)

    def test_receipt_final_target_is_exact_and_never_followed(self):
        for kind in ["file","directory","dangling-symlink","directory-symlink"]:
            with self.subTest(kind=kind),tempfile.TemporaryDirectory() as root:
                final = Path(root)/"receipt"
                destination = Path(root)/"destination"
                if kind == "file":final.write_bytes(b"preserve")
                elif kind == "directory":final.mkdir()
                elif kind == "dangling-symlink":final.symlink_to(destination)
                else:
                    destination.mkdir()
                    final.symlink_to(destination,target_is_directory=True)
                before = final.lstat()
                result = subprocess.run(["/bin/sh","-c",
                    "asb_pid=42; "+_publish_receipt(root,"receipt","nonce","asb_pid")],capture_output=True)
                self.assertNotEqual(result.returncode,0)
                self.assertEqual(final.lstat().st_ino,before.st_ino)
                if kind == "file":self.assertEqual(final.read_bytes(),b"preserve")
                if kind == "directory":self.assertEqual(list(final.iterdir()),[])
                if kind == "directory-symlink":self.assertEqual(list(destination.iterdir()),[])
                if kind == "dangling-symlink":self.assertFalse(destination.exists())

    def fixture(self,require_host=True):
        temporary = tempfile.TemporaryDirectory();self.addCleanup(temporary.cleanup)
        container = Container();api = API(container)
        original = api.exec_inspect
        def inspect(exec_id):
            self.assertTrue(api.completed,"bridge inspection must not identify the client")
            return original(exec_id)
        api.exec_inspect = inspect
        client = SimpleNamespace(api=api)
        workspace = ManagedDockerWorkspace.attach(client,container,"asb-"+"a"*32,"/task")
        keys = []
        session = SimpleNamespace(container=container,_user="worker",is_session_alive=lambda:True,
            send_keys=lambda values,**kwargs:keys.append(values))
        launcher = SDKSessionLauncher(session,"/frozen/asb",Path(temporary.name),require_host_target=require_host)
        return workspace,container,launcher,keys

    def resolve(self,command,payload,**kwargs):
        self.assertEqual(command,["/frozen/asb","__asb-target","--resolve"])
        value = json.loads(payload)
        self.assertEqual(value.pop("candidates"),[222,9001])
        return json.dumps(dict(value,daemon_pid=222,pid_scope="docker-daemon-host")).encode()

    def test_private_launcher_preserves_user_and_never_types_credentials_into_sdk(self):
        workspace,container,launcher,keys = self.fixture()
        with patch("asb_material_exec.run",side_effect=self.resolve):
            result = workspace.execute(["agent","literal ; $value"],
                {"FIXTURE_KEY":"private-key-value"},transport_factory=launcher)
        self.assertEqual(result["exit_code"],7)
        self.assertEqual(result["target"]["daemon_pid"],222)
        self.assertEqual((result["stdout"],result["stderr"]),(b"stdout",b"stderr"))
        self.assertNotIn("private-key-value",str(keys))
        self.assertEqual(len(container.archives),1)
        _,uid,gid,mode,script = container.archives[0]
        self.assertEqual((uid,gid,mode),(1001,1002,0o600))
        self.assertIn(b"private-key-value",script)
        self.assertIn(b'wait -n "$asb_child_pid" "$asb_release_pid"',script)
        self.assertIn(b'if wait "$asb_child_pid"',script)
        self.assertTrue(script.endswith(b"exit 0\n"))
        self.assertTrue(launcher.transports[0].closed)
        launcher.close();workspace.close()

    def test_host_resolution_failure_is_explicit_and_never_uses_bridge_pid(self):
        for require_host in [False,True]:
            workspace,container,launcher,_ = self.fixture(require_host)
            with patch("asb_material_exec.run",side_effect=MaterialError("remote host")):
                if require_host:
                    with self.assertRaisesRegex(ExecutionError,"no verified local host PID"):
                        workspace.execute(["agent"],transport_factory=launcher)
                    self.assertFalse(container.resumed)
                else:
                    result = workspace.execute(["agent"],transport_factory=launcher)
                    self.assertEqual(result["target"]["pid_scope"],"container-namespace")
                    self.assertIsNone(result["target"]["daemon_pid"])
            launcher.close();workspace.close()

    def test_malformed_or_contradictory_resolver_output_never_resumes(self):
        for output in [b"not-json",b"null",b"[]",
                b'{"container_id":"foreign","exec_id":"bridge","namespace_pid":42,"daemon_pid":222,"pid_scope":"docker-daemon-host"}']:
            with self.subTest(output=output):
                workspace,container,launcher,_ = self.fixture()
                with patch("asb_material_exec.run",return_value=output),self.assertRaises(ExecutionError):
                    workspace.execute(["agent"],transport_factory=launcher)
                self.assertFalse(container.resumed)
                self.assertIn("kill",container.events)
                launcher.close();workspace.close()

    def test_parent_release_is_single_use_and_requires_original_parent_and_launcher_success(self):
        for name,data in [(None,None),("released",b"{nonce} 43\n"),
                ("released",b"wrong 42\n"),("returned",b"{nonce} 9 1\n")]:
            with self.subTest(name=name,data=data):
                workspace,container,launcher,_ = self.fixture()
                transport = launcher(workspace.client.api,container,["agent"],{},"/task")
                with patch("asb_material_exec.run",side_effect=self.resolve):transport.target(42)
                if name:
                    container.frames[name] = data.replace(b"{nonce}",container.nonce.encode())
                    with self.assertRaises(ExecutionError):transport.release_workload_parent()
                else:transport.release_workload_parent()
                with self.assertRaisesRegex(ExecutionError,"already attempted"):transport.release_workload_parent()
                launcher.close();workspace.close()

    def test_bad_nonce_pid_or_parent_identity_cannot_admit_or_complete(self):
        for name,data in [("started",b"wrong 42\n"),("started",b"x"*129),
                ("parent",b"{nonce} 42\n"),("exit",b"{nonce} 43 7\n"),
                ("exit",b"{nonce} 42 256\n"),("returned",b"{nonce} 10 0\n"),
                ("returned",b"{nonce} 9 1\n")]:
            with self.subTest(name=name,data=data):
                workspace,container,launcher,_ = self.fixture()
                run = container.exec_run
                def read(argv,user=None):
                    result = run(argv,user)
                    if "asb-session-receipt" in argv and argv[-1].endswith("/"+name):
                        result.output = data.replace(b"{nonce}",container.nonce.encode())
                    return result
                container.exec_run = read
                with patch("asb_material_exec.run",side_effect=self.resolve),self.assertRaises(ExecutionError):
                    workspace.execute(["agent"],transport_factory=launcher)
                if name in ["started","parent"]:self.assertFalse(container.resumed)
                self.assertIn("kill",container.events)
                launcher.close();workspace.close()

    def test_descendant_principal_uses_launcher_receipts_for_release(self):
        workspace,container,launcher,_ = self.fixture()
        transport = launcher(workspace.client.api,container,["agent"],{},"/task")
        container.frames["started"] = (container.nonce+" 43\n").encode()
        container.frames["released"] = (container.nonce+" 43\n").encode()
        with patch("asb_material_exec.run",side_effect=self.resolve):
            with self.assertRaisesRegex(ExecutionError,"not a descendant"):
                transport.target(42)
            container.ancestry_valid = True
            target = transport.target(42)
        self.assertEqual(target["namespace_pid"],42)
        transport.release_workload_parent()
        launcher.close();workspace.close()

    def test_partial_failed_sdk_launch_remains_owned_and_does_not_resume(self):
        workspace,container,launcher,_ = self.fixture()
        def fail(*args,**kwargs):raise ValueError("SDK launch failed")
        launcher.session.send_keys = fail
        with self.assertRaisesRegex(ExecutionError,"SDK launch worker failed"):
            workspace.execute(["agent"],transport_factory=launcher)
        self.assertFalse(container.resumed)
        self.assertEqual(len(launcher.transports),1)
        with self.assertRaises(ExecutionError):launcher.close()
        workspace.close()

    def test_failed_factory_construction_settles_registered_transport(self):
        workspace,container,launcher,_ = self.fixture()
        with patch("asb_terminal_session.SessionTransport.start",side_effect=ExecutionError("start failed")):
            with self.assertRaisesRegex(ExecutionError,"start failed"):
                workspace.execute(["agent"],transport_factory=launcher)
        self.assertFalse(container.resumed)
        self.assertEqual(len(launcher.transports),1)
        self.assertTrue(launcher.transports[0].closed)
        launcher.close();workspace.close()

    def test_bridge_construction_failure_cannot_claim_launched_child_settled(self):
        workspace,container,launcher,_ = self.fixture()
        with patch("asb_terminal_session.DockerExecTransport",side_effect=ExecutionError("bridge failed")):
            with self.assertRaisesRegex(ExecutionError,"settlement remains unconfirmed"):
                workspace.execute(["agent"],transport_factory=launcher)
        self.assertFalse(container.resumed)
        self.assertIn("kill",container.events)
        self.assertEqual(len(launcher.transports),1)
        with self.assertRaisesRegex(ExecutionError,"settlement remains unconfirmed"):launcher.close()
        workspace.close()

    def test_upload_rejects_invalid_private_owners_before_staging(self):
        from asb_guest import upload
        container = Container()
        for owner in [(True,1),(-1,1),(1,4294967296),(1,),[1,2],("1",2)]:
            with self.subTest(owner=owner),self.assertRaisesRegex(ExecutionError,"invalid private guest file owner"):
                upload(container,"/tmp",[("private",b"fixture",0o600)],owner=owner)
        self.assertEqual(container.archives,[])


if __name__ == "__main__":unittest.main()
