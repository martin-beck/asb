"""Opt-in original service-bearing task qualification, no live LLM provider.

The known upstream reference is deliberately supplied only to a synthetic
client fixture. This checks the real guest adapter/supervisor and original
verifier, not whether an AI agent can solve the task.
It uses the task-owned SDK launcher for this true same-shell declaration.
"""
import json
import hashlib
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / "assets/benchmarks"))


@unittest.skipUnless(os.environ.get("ASB_TEST_BENCHMARK_ASB") and os.environ.get("ASB_TEST_SWECATALOG_ROOT"),
                     "requires explicit original SDK, Docker and compiled guest opt-in")
class WorkloadServiceQualification(unittest.TestCase):
    def test_original_jupyter_service_survives_client_exit_and_official_grading(self):
        from asb_docker import DockerSession
        from asb_guest import execute_agent, upload
        from asb_runtime import bundle
        from asb_terminal_bench_1 import TerminalBenchTask
        import asb_execution,asb_guest,asb_terminal_bench_1,asb_terminal_session,asb_harness
        sources = [Path(module.__file__) for module in [asb_execution,asb_guest,asb_terminal_bench_1,asb_terminal_session,asb_harness]]
        source_hashes = {path.name:hashlib.sha256(path.read_bytes()).hexdigest() for path in sources}
        binary = Path(os.environ["ASB_TEST_BENCHMARK_ASB"])
        binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
        root = Path(os.environ["ASB_TEST_SWECATALOG_ROOT"]).absolute()
        artifacts = Path(tempfile.mkdtemp(prefix="original-service-lifetime-", dir=root))
        print(f"Retained original service-lifetime qualification: {artifacts}", flush=True)
        with DockerSession() as client:
            with TerminalBenchTask(client, root / "workloads/terminal-bench-1.0/catalog.json",
                                   "jupyter-notebook-server", artifacts / "task") as task:
                self.assertTrue(task.handler.task.run_tests_in_same_shell)
                task.checked(["mkdir", "-p", "/opt/asb-runtime/bin", "/opt/asb-runtime/lib",
                    "/opt/asb-project/.asb", "/opt/asb-project/.asb-tools/pi/bin", "/opt/asb-private"])
                files, _ = bundle(os.environ["ASB_TEST_BENCHMARK_ASB"], task.workspace.image_identity["architecture"])
                upload(task.workspace.container, "/opt/asb-runtime", files, temporary_dir=artifacts)
                upload(task.workspace.container, "/opt/asb-project/.asb", [("project.toml",
                    b"tools_dir='.asb-tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n", 0o600)], temporary_dir=artifacts)
                upload(task.workspace.container, "/opt/asb-private", [("fixture-solution.sh",
                    (task.definition / "solution.sh").read_bytes(), 0o700)], temporary_dir=artifacts)
                fixture = b'''#!/bin/sh
set -eu
test "$ASB_SDK_SERVICE_CONTEXT" = original-parent-context
/bin/bash /opt/asb-private/fixture-solution.sh
printf '%s\\n' '{"type":"result","result":"known service fixture prepared"}'
'''
                upload(task.workspace.container, "/opt/asb-project/.asb-tools/pi/bin",
                       [("pi", fixture, 0o755)], temporary_dir=artifacts)
                guest = {"process_lifetime":"through-evaluation", "config":{
                    "agent":"pi", "provider":"openrouter", "model":"fixture", "workload":"terminal-bench-1.0",
                    "workspace":task.workspace.workdir, "concurrency":1},
                    "connection":{"interface":"open_ai_chat", "authentication":"bearer",
                        "base_url":"http://127.0.0.1:1/v1", "credential":"unused-fixture"},
                    "prompt":"explicit known service-lifetime fixture, no AI solving claim"}
                ended = []
                parents = []
                def observe_parent():
                    task.agent_session.send_keys(["printf '%s|%s|%s|%s\\n' \"$$\" \"$(id -u)\" \"$PWD\" "
                        "\"$ASB_SDK_SERVICE_CONTEXT\" > /tmp/asb-lifetime-parent","Enter"],block=True,max_timeout_sec=10)
                    return task.checked(["cat","/tmp/asb-lifetime-parent"])
                create_session = task.prepare_agent_session
                def prepare_session():
                    create_session()
                    task.agent_session.send_keys(["export ASB_SDK_SERVICE_CONTEXT=original-parent-context","Enter"],
                        block=True,max_timeout_sec=10)
                    parents.append(observe_parent())
                exported = artifacts / "exported"
                exported.mkdir()
                with patch.object(task,"prepare_agent_session",side_effect=prepare_session), \
                        patch("asb_guest.prepare_agent", return_value=(guest, {"PATH":"/usr/local/bin:/usr/bin:/bin"})):
                    result = execute_agent(task, {"binary":os.environ["ASB_TEST_BENCHMARK_ASB"],
                        "artifact_output":str(exported)}, None, None, None, on_ended=ended.append)
                (artifacts / "driver.stdout").write_bytes(result["stdout"])
                (artifacts / "driver.stderr").write_bytes(result["stderr"])
                self.assertEqual(result["exit_code"], 0, result["stderr"].decode(errors="replace"))
                self.assertTrue(result["workload_processes_retained"])
                self.assertFalse(result["metadata"]["tree_quiesced"])
                self.assertEqual(len(ended), 1)
                self.assertTrue(task.workspace.held_executions[0][1].is_alive())
                self.assertEqual(len(parents),1)
                self.assertEqual(observe_parent(),parents[0],"original SDK parent must return before grading")
                task.checked(["python3", "-c", "import socket; s=socket.create_connection(('127.0.0.1',8888),5); s.close()"])
                observation = json.loads((task.artifacts / "execution.json").read_bytes())
                self.assertEqual(observation["agent_tree_cleanup"], "delegated-owned-workload")
                report = task.evaluate()
                self.assertTrue(report["resolved"], report)
                self.assertEqual(report["outcome"], "resolved")
                self.assertEqual(observe_parent(),parents[0],"official grading must use the original persistent shell")
                attempt = task.attempt
                workspace = task.workspace
            self.assertEqual(workspace.held_executions, [])
            self.assertEqual(json.loads((artifacts / "task/task-cleanup.json").read_text())["status"], "removed")
            self.assertEqual(client.containers.list(all=True, filters={"label":"asb.attempt=" + attempt}), [])
        self.assertEqual(source_hashes,{path.name:hashlib.sha256(path.read_bytes()).hexdigest() for path in sources})
        self.assertEqual(binary_hash,hashlib.sha256(binary.read_bytes()).hexdigest())
        (artifacts/"qualification.json").write_text(json.dumps({"scope":"original-sdk-same-shell-fixture-client-service-grading",
            "source_hashes":source_hashes,"binary_sha256":binary_hash,"parent":parents[0].decode().strip(),
            "official_resolved":True,"cleanup":"removed","coding_agent_started":False,"provider_called":False},sort_keys=True)+"\n")


if __name__ == "__main__":
    unittest.main()
