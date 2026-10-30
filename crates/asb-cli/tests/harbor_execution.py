"""Offline Harbor adapter contracts; no installed SDK, daemon or provider."""
import asyncio
import json
from pathlib import Path
import sys
import tempfile
from types import ModuleType, SimpleNamespace
import unittest
from unittest.mock import Mock, patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / "assets/benchmarks"))
import asb_harbor as subject
from asb_execution import ExecutionError


class HarborTests(unittest.TestCase):
    def test_signal_interruption_drains_sdk_operation_before_cleanup(self):
        task = subject.HarborTask(None,"catalog","instance",Path("unused"))
        self.addCleanup(task.loop.close)
        events = []
        async def operation():
            events.append("started")
            try:
                await asyncio.sleep(3600)
            finally:
                events.append("cancelled")
        real_run = task.loop.run_until_complete
        first = True
        def interrupt_once(pending):
            nonlocal first
            if first:
                first = False
                real_run(asyncio.sleep(0))
                raise InterruptedError("fixture termination signal")
            return real_run(pending)
        with patch.object(task.loop,"run_until_complete",side_effect=interrupt_once):
            with self.assertRaisesRegex(InterruptedError,"fixture termination signal"):
                task.wait(operation(),60)
        self.assertEqual(events,["started","cancelled"])
        self.assertFalse(asyncio.all_tasks(task.loop))

    def test_cleanup_is_project_scoped_and_reports_failure_truthfully(self):
        for fail in [False,True]:
            with self.subTest(fail=fail), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                task = subject.HarborTask(Mock(),"catalog","instance",root)
                task.workspace = SimpleNamespace(container=object(),cleanup_status="pending",join_held_executions=Mock())
                calls = []
                class Environment:
                    async def prepare_logs_for_host(self):
                        calls.append("logs")
                    async def _run_docker_compose_command(self,arguments):
                        calls.append(arguments)
                        if fail:
                            raise ExecutionError("owned project down failed")
                task.environment = Environment()
                task.client.containers.list.return_value = []
                if fail:
                    with self.assertRaisesRegex(ExecutionError,"owned project down failed"):
                        task.__exit__(None,None,None)
                else:
                    task.__exit__(None,None,None)
                    self.assertIsNone(task.workspace.container)
                expected = "cleanup_failed" if fail else "removed"
                self.assertEqual(task.workspace.cleanup_status,expected)
                self.assertEqual(json.loads((root / "task-cleanup.json").read_text())["status"],expected)
                self.assertEqual(calls,["logs",["down","--timeout","2","--volumes","--remove-orphans"]])
                self.assertTrue(task.loop.is_closed())

    def test_failed_start_removes_empty_owned_compose_project(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            task = subject.HarborTask(Mock(), "catalog", "instance", root)
            task.interrupted = True
            calls = []
            class Environment:
                async def _run_docker_compose_command(self, arguments):
                    calls.append(arguments)
            task.environment = Environment()
            task.journal = Mock()
            task.client.containers.list.return_value = []

            task.__exit__(RuntimeError, RuntimeError("startup failed"), None)

            self.assertEqual(calls, [["down", "--timeout", "2", "--volumes", "--remove-orphans"]])
            task.journal.cleanup_images.assert_called_once_with(task.client)
            task.journal.preserve.assert_not_called()
            self.assertEqual(json.loads((root / "task-cleanup.json").read_text())["status"], "removed-before-agent")
            self.assertTrue(task.loop.is_closed())

    def test_unavailable_prebuilt_image_uses_original_source_with_one_shared_budget(self):
        with tempfile.TemporaryDirectory() as directory:
            public = Path(directory)
            (public / "Dockerfile").write_text("FROM official")
            calls = []
            class Environment:
                _use_prebuilt = True
                async def start(self, force_build):
                    calls.append(("start",force_build))
                    if not force_build:
                        raise subject.ImageUnavailable("fixture unavailable image")
                async def _run_docker_compose_command(self, arguments):
                    calls.append(("compose",arguments))
            loop = asyncio.new_event_loop()
            self.addCleanup(loop.close)
            budgets = []
            def wait(operation,budget):
                budgets.append(budget)
                return loop.run_until_complete(operation)
            # Do not replace asyncio's shared monotonic clock in this test.
            with patch.object(subject,"time",SimpleNamespace(monotonic=Mock(side_effect=[10,12]))):
                result = subject.start_environment(Environment(),public,wait,600)
            self.assertEqual(result,"original-source-build-fallback")
            self.assertEqual(budgets,[600,120,598])
            self.assertEqual(calls,[("start",False),("compose",["down","--timeout","2","--volumes","--remove-orphans"]),("start",True)])
            (public / "Dockerfile").unlink()
            with self.assertRaises(subject.ImageUnavailable):
                subject.start_environment(Environment(),public,wait,600)

    def test_scoped_compose_preserves_task_resources_services_and_internal_ports(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            environment = root / "environment"
            environment.mkdir()
            config = {"services":{"main":{"image":"official", "mem_limit":"2g", "cpus":1,
                "ports":["1234:1234"],"expose":["1234"],"volumes":[{"type":"bind","source":str(root / "logs"),"target":"/logs"}]},
                "service":{"image":"other","depends_on":["main"]}},"networks":{"default":{}},"volumes":{"data":{}}}
            result = subject.scoped_configuration(config,"asb-fixture",root,environment)
            self.assertEqual(result["services"]["main"]["ports"], [])
            self.assertEqual(result["services"]["main"]["expose"], ["1234"])
            self.assertEqual(result["services"]["main"]["mem_limit"], "2g")
            self.assertEqual(result["services"]["main"]["cpus"], 1)
            self.assertEqual(result["services"]["service"]["depends_on"], ["main"])
            self.assertEqual(result["services"]["service"]["labels"]["asb.attempt"], "asb-fixture")
            self.assertEqual(result["networks"]["default"]["name"], "asb-fixture-default")
            self.assertEqual(result["volumes"]["data"]["name"], "asb-fixture-data")

    def test_host_exposure_and_private_build_contexts_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            environment = root / "environment"
            environment.mkdir()
            variants = [{"privileged":True}, {"network_mode":"host"}, {"pid":"host"}, {"ipc":"host"},
                {"devices":["/dev/example"]}, {"build":{"context":"/private"}},
                {"build":{"context":str(environment),"network":"host"}},
                {"volumes":[{"type":"bind","source":"/home","target":"/host"}]}]
            for service in variants:
                with self.subTest(service=service), self.assertRaises(ExecutionError):
                    subject.scoped_configuration({"services":{"main":service}},"asb-fixture",root,environment)
            for category in ["networks","volumes"]:
                with self.assertRaises(ExecutionError):
                    subject.scoped_configuration({"services":{"main":{}},category:{"shared":{"external":True}}},
                                                 "asb-fixture",root,environment)

    def test_original_verifier_rewards_are_task_bound_and_stale_rewards_are_removed(self):
        modules = {name:ModuleType(name) for name in ["harbor","harbor.verifier","harbor.verifier.verifier"]}
        with tempfile.TemporaryDirectory() as directory, patch.dict(sys.modules,modules):
            root = Path(directory)
            task = subject.HarborTask(Mock(),"catalog","instance",root)
            self.addCleanup(task.loop.close)
            task.workspace = SimpleNamespace(container=Mock())
            task.original_networks = ["original"]
            task.capture_submission = Mock()
            task.catalog = {"workload":"terminal-bench-2.1","dataset":{"revision":"pinned"},"sdk":{"version":"pinned"}}
            # Synthetic identity injection checks metadata plumbing, not support
            # or qualification for an additional Harbor dataset.
            identity = {"workload":"fixture-profile", "task_id":task.task_id,
                        "attempt_id":task.attempt, "dataset":task.catalog["dataset"]}
            task.invocation = {"identity":identity}
            task.task = SimpleNamespace(config=SimpleNamespace(verifier=SimpleNamespace(timeout_sec=60,user="verifier")))
            task.environment = SimpleNamespace(default_user=None)
            verifier_dir = root/"official/verifier"
            verifier_dir.mkdir(parents=True)
            task.paths = SimpleNamespace(reward_text_path=verifier_dir/"reward.txt",reward_json_path=verifier_dir/"reward.json")
            for rewards, expected in [({"reward":1},"resolved"), ({"reward":0},"unresolved"), ({},"evaluation_error"),
                ({"reward":float("nan")},"evaluation_error"), ({"reward":0.5},"evaluation_error"),
                ({"reward":True},"evaluation_error"), ({"other":1},"evaluation_error")]:
                # These SDK seams represent separate first-verifier attempts,
                # not permission to repeat tests against one mutated task.
                (root/"evaluation-started.json").unlink(missing_ok=True)
                (root/"verifier-return.json").unlink(missing_ok=True)
                with self.subTest(rewards=rewards), patch.object(subject,"restore_task_networks") as restore:
                    task.paths.reward_text_path.write_text("stale reward")
                    task.paths.reward_json_path.write_text('{"reward":1}')
                    async def verify():
                        self.assertFalse(task.paths.reward_text_path.exists())
                        self.assertFalse(task.paths.reward_json_path.exists())
                        self.assertEqual(task.environment.default_user,"verifier")
                        task.paths.reward_json_path.write_text(json.dumps(rewards))
                        return SimpleNamespace(rewards=rewards)
                    verifier = Mock(return_value=SimpleNamespace(verify=verify))
                    modules["harbor.verifier.verifier"].Verifier = verifier
                    result = task.evaluate()
                    self.assertEqual(result["outcome"],expected)
                    self.assertEqual(result["task_id"],"instance")
                    self.assertEqual(result["dataset"],task.catalog["dataset"])
                    self.assertEqual(result["workload"],identity["workload"])
                    self.assertEqual(json.loads((root/"evaluation-started.json").read_bytes())["task"],identity)
                    verifier.assert_called_once_with(task.task,task.paths,task.environment)
                    restore.assert_called_once_with(task.client,task.workspace.container,["original"])


if __name__ == "__main__":
    unittest.main()
