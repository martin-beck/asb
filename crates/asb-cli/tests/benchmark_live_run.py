"""Opt-in normal ASB run against an isolated synthetic provider and real grader."""
import json
import os
from pathlib import Path
import subprocess
import shlex
import shutil
import sys
import tempfile
import time
import unittest
import uuid
import fcntl

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / "assets/benchmarks"))


@unittest.skipUnless(os.environ.get("ASB_TEST_BENCHMARK_ASB"),
                     "requires explicit ASB builds and cached official dataset/tools/image")
class NormalRunQualification(unittest.TestCase):
    def test_selected_client_runs_and_is_officially_graded_through_normal_cli(self):
        from asb_docker import DockerSession
        from asb_execution import ManagedDockerWorkspace
        from asb_guest import upload
        root = Path(os.environ["ASB_TEST_SWECATALOG_ROOT"]).absolute()
        suite = os.environ.get("ASB_TEST_BENCHMARK_SUITE", "swe-bench-lite")
        self.assertIn(suite, ["swe-bench", "swe-bench-lite", "swe-bench-verified", "terminal-bench-1.0", "terminal-bench-2.1"])
        terminal = suite.startswith("terminal-bench-")
        instance = ("hello-world" if suite.endswith("1.0") else "cancel-async-tasks") if terminal else "astropy__astropy-12907"
        resolved = os.environ.get("ASB_TEST_BENCHMARK_RESOLVED") == "1"
        tape_mode = os.environ.get("ASB_TEST_BENCHMARK_TAPE_MODE")
        namespace_collectors = os.environ.get("ASB_TEST_NAMESPACE_COLLECTORS") == "1"
        extra_collectors = [name for name in os.environ.get("ASB_TEST_BENCHMARK_COLLECTORS", "").split(",") if name]
        collector_selection = (["agent-memory", "agent-workers", "cpu-accounting"] if namespace_collectors else []) + extra_collectors
        sweep = os.environ.get("ASB_TEST_BENCHMARK_SWEEP") == "1"
        measured = os.environ.get("ASB_TEST_BENCHMARK_MEASURED") == "1" or bool(collector_selection)
        self.assertIn(tape_mode, [None, "record", "replay"])
        binary = Path(os.environ["ASB_TEST_BENCHMARK_ASB"]).absolute()
        artifacts = Path(tempfile.mkdtemp(prefix="normal-cli-guest-", dir=root))
        print(f"Retained normal-run qualification: {artifacts}", flush=True)
        print(f"Population: {suite}; deterministic known-good repair: {resolved} (not an AI solution)", flush=True)
        cold = os.environ.get("ASB_TEST_COLD_BOOTSTRAP") == "1"
        if cold:
            # Opt-in qualification owns this dedicated project. Preserve its
            # public caches before exercising actual primary-source downloads.
            with (root/".asb-tools/.asb-guest-bootstrap.lock").open("a") as lock:
                fcntl.flock(lock,fcntl.LOCK_EX)
                for cache in [root/".asb-tools/node",root/".asb-cache/npm"]:
                    if cache.exists():
                        self.assertFalse(cache.is_symlink())
                        backup = cache.with_name(cache.name+".before-cold-"+uuid.uuid4().hex)
                        cache.rename(backup)
                        print(f"Preserved public cache: {backup}",flush=True)
                        if cache.name == "npm":
                            def restore_package_cache(cache=cache,backup=backup):
                                with (root/".asb-tools/.asb-guest-bootstrap.lock").open("a") as lock:
                                    fcntl.flock(lock,fcntl.LOCK_EX)
                                    if not cache.exists():
                                        backup.rename(cache)
                                        print(f"Restored prior public package cache: {cache}",flush=True)
                            self.addCleanup(restore_package_cache)
        network_name = "asb-fixture-" + uuid.uuid4().hex
        with DockerSession() as client:
            network = client.networks.create(network_name, internal=True, labels={"asb.owner": "asb"})
            try:
                image = client.images.get("swebench/sweb.eval.x86_64.astropy_1776_astropy-12907:latest")
                with ManagedDockerWorkspace(client, image, "asb-" + uuid.uuid4().hex, "/tmp") as provider:
                    upload(provider.container, "/tmp", [("provider.py", (Path(__file__).parent / "guest_provider_fixture.py").read_bytes(), 0o600)])
                    if resolved:
                        # An explicit oracle fixture exercises the agent's real
                        # shell tool and normal grading; no gold/test dataset
                        # fields are passed to the task container or prompt.
                        code = ("from pathlib import Path; p=Path('astropy/modeling/separable.py'); t=p.read_text(); "
                                "old='cright[-right.shape[0]:, -right.shape[1]:] = 1'; "
                                "assert t.count(old)==1; p.write_text(t.replace(old, 'cright[-right.shape[0]:, -right.shape[1]:] = right'))")
                        if suite == "terminal-bench-1.0":
                            command = "printf 'Hello, world!\\n' > hello.txt"
                        elif suite == "terminal-bench-2.1":
                            # Deliberately deterministic qualification code,
                            # not an AI-generated solution or benchmark score.
                            solution = ("import asyncio\nasync def run_tasks(tasks, max_concurrent):\n"
                                "    semaphore = asyncio.Semaphore(max_concurrent)\n"
                                "    async def run(task):\n        async with semaphore:\n            await task()\n"
                                "    async with asyncio.TaskGroup() as group:\n        for task in tasks:\n            group.create_task(run(task))\n")
                            command = "python3 -c " + shlex.quote("from pathlib import Path; Path('run.py').write_text(" + repr(solution) + ")")
                        else:
                            command = "python3 -c " + shlex.quote(code)
                        upload(provider.container, "/tmp", [("command.txt", command.encode(), 0o600)])
                    network.connect(provider.container)
                    client.networks.get("bridge").disconnect(provider.container)
                    provider.container.exec_run(["python3", "/tmp/provider.py", "/tmp", "0.0.0.0"], detach=True)
                    for _ in range(100):
                        port = provider.container.exec_run(["cat", "/tmp/port"])
                        if port.exit_code == 0:
                            break
                        time.sleep(0.05)
                    self.assertEqual(port.exit_code, 0)
                    provider.container.reload()
                    address = provider.container.attrs["NetworkSettings"]["Networks"][network_name]["IPAddress"]
                    base = f"http://{address}:{int(port.output.strip())}/v1"
                    host_provider = os.environ.get("ASB_TEST_HOST_PROVIDER") == "1"
                    if host_provider:
                        host_root = artifacts / "host-provider"
                        host_root.mkdir()
                        if resolved:
                            (host_root / "command.txt").write_text(command)
                        process = subprocess.Popen([sys.executable,
                            str(Path(__file__).parent / "guest_provider_fixture.py"), str(host_root)],
                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                        def stop_provider():
                            if process.poll() is None:
                                process.terminate()
                            try:
                                process.wait(timeout=3)
                            except subprocess.TimeoutExpired:
                                process.kill()
                                process.wait(timeout=3)
                        self.addCleanup(stop_provider)
                        for _ in range(100):
                            if (host_root / "port").is_file() or process.poll() is not None:
                                break
                            time.sleep(.05)
                        self.assertIsNone(process.poll())
                        base = f"http://127.0.0.1:{int((host_root / 'port').read_text())}/v1"
                    configuration = artifacts / "benchmark.toml"
                    setup = subprocess.run([binary,"setup","--agent","pi","--provider","openrouter",
                        "--model","fixture-model","--workload",suite,"--trial",instance,
                        "--workspace",str(artifacts / "workspace"),"--output",str(configuration)],
                        cwd=root,capture_output=True,timeout=30)
                    (artifacts / "setup.stdout").write_bytes(setup.stdout)
                    (artifacts / "setup.stderr").write_bytes(setup.stderr)
                    self.assertEqual(setup.returncode,0,setup.stderr.decode())
                    # Explicit per-qualification collection, not setup's host
                    # defaults. Keep all adapter-neutral setup identity fields.
                    selected = configuration.read_text().split("[measurements]",1)[0]
                    configuration.write_text("\n".join(
                        "log_file = " + json.dumps(str(artifacts / "run.log"))
                        if line.startswith("log_file = ") else line for line in selected.splitlines()) + "\n")
                    if collector_selection:
                        with configuration.open("a") as output:
                            output.write("[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=" + json.dumps(collector_selection) + "\n")
                    environment = {"PATH": os.environ["PATH"], "HOME": str(artifacts),
                                   "OPENROUTER_API_KEY": "fixture-token", "OPENROUTER_BASE_URL": base,
                                   "OPENROUTER_MODELS_URL": base + "/models", "NO_PROXY": "*", "no_proxy": "*",
                                   "ASB_BENCHMARK_NETWORK": network_name, "NPM_CONFIG_OFFLINE": "true"}
                    if namespace_collectors:
                        environment["ASB_MEASURE_INTERVAL_MS"] = "50"
                    if os.environ.get("ASB_TEST_GUEST_BINARY"):
                        environment["ASB_GUEST_BINARY"] = os.environ["ASB_TEST_GUEST_BINARY"]
                    if cold:
                        environment.pop("NPM_CONFIG_OFFLINE")
                    command_line = [binary, "run", configuration, "--json"]
                    measurement_root = artifacts / "measurements"
                    if measured:
                        command_line += ["--measurements", str(measurement_root)]
                    if tape_mode:
                        command_line += ["--" + tape_mode, "--cassette", os.environ["ASB_TEST_BENCHMARK_CASSETTE"]]
                    if sweep:
                        self.assertTrue(namespace_collectors, "sweep qualification needs configured collectors")
                        manifest = artifacts / "sweep.toml"
                        manifest.write_text("configs=['benchmark.toml']\n")
                        command_line = [binary, "sweep", manifest, "--json"]
                        measurement_root = artifacts / "sweep-results/000-benchmark/measurements"
                        if tape_mode:
                            cassette_root = artifacts / "cassettes"
                            cassette = cassette_root / f"000-benchmark/cassettes/benchmark-pi-{suite}-fixture-model.jsonl"
                            cassette.parent.mkdir(parents=True)
                            if tape_mode == "replay":
                                shutil.copyfile(os.environ["ASB_TEST_BENCHMARK_CASSETTE"], cassette)
                            command_line += ["--" + tape_mode, "--cassette-root", str(cassette_root)]
                    result = subprocess.run(command_line, cwd=root,
                                            env=environment, capture_output=True, timeout=240)
                    (artifacts / "asb.stdout").write_bytes(result.stdout)
                    (artifacts / "asb.stderr").write_bytes(result.stderr)
                    self.assertEqual(result.returncode, 0 if resolved else 2, result.stderr.decode())
                    record = json.loads(result.stdout)
                    self.assertEqual(record["process_success"], True, record)
                    self.assertEqual(record["workload_success"], resolved, record)
                    self.assertEqual(record["workload_evaluation"], "upstream-harness")
                    self.assertEqual(record["workload_baseline"]["kind"], "upstream-environment-definition" if terminal else "upstream-task-definition")
                    self.assertIsNone(record["workload_baseline_passed"])
                    self.assertEqual(record["success_rate"], 1.0 if resolved else 0.0)
                    if measured:
                        target = json.loads((measurement_root / "execution-target.json").read_text())
                        self.assertEqual(target["scope"], "verified-local-isolated-process-tree")
                        self.assertEqual(target["cgroup_membership"], "preserved")
                        self.assertNotEqual(target["host_pid"], process.pid if host_provider else os.getpid())
                        self.assertGreater(target["host_pid"], 1)
                        lifecycle = json.loads((measurement_root / "lifecycle.json").read_text())
                        events = [event["event"]["kind"] for event in lifecycle]
                        self.assertEqual([event for event in events if event.startswith("agent_")],
                            ["agent_spawn", "agent_execute_begin", "agent_started", "agent_execute_end", "agent_exited"])
                        boundaries = [(entry["event"]["kind"], entry["event"].get("phase")) for entry in lifecycle]
                        self.assertLess(boundaries.index(("phase_end", "agent_preparation")),
                                        boundaries.index(("agent_execute_begin", None)))
                        self.assertLess(boundaries.index(("agent_execute_end", None)),
                                        boundaries.index(("phase_begin", "workload_evaluation")))
                        accounting = json.loads((measurement_root / "cpu-accounting.json").read_text())
                        self.assertEqual(accounting["cleanup"], "not_created")
                        self.assertIsNone(accounting["remediation"])
                        if accounting["metrics"] is not None and "cpu_user_ms" in accounting["metrics"]:
                            self.assertGreater(accounting["metrics"]["cpu_user_ms"] + accounting["metrics"]["cpu_system_ms"], 0)
                        self.assertIn("not agent-only CPU", accounting["container_scope"])
                        self.assertIn("container_cpu_user_ms", accounting["metrics"])
                        if namespace_collectors:
                            components = json.loads((measurement_root / "components.json").read_text())
                            agent_timing = components["components"]["agent.execute"]
                            self.assertGreater(
                                agent_timing["cpu_user_ms"] + agent_timing["cpu_system_ms"],
                                0,
                                agent_timing,
                            )
                            self.assertEqual(
                                agent_timing["cpu_accounting_source"],
                                "proc-sampling-execution-namespace",
                            )
                            if Path("/proc/sys/kernel/task_delayacct").read_text().strip() == "1":
                                self.assertIsNotNone(agent_timing["io_wait_ms"], agent_timing)
                            for name in ["agent-memory", "agent-workers"]:
                                observation = json.loads((measurement_root / (name + ".json")).read_text())
                                self.assertEqual(observation["status"], "observed", observation)
                                self.assertEqual(observation["source"], "execution-namespace-linux-proc-sampling")
                                self.assertGreater(observation["samples"], 0)
                                if name == "agent-memory":
                                    self.assertGreater(observation["metrics"]["peak_rss_mib"], 0)
                                else:
                                    self.assertEqual(observation["metrics"]["spawned_worker_count"], 0)
                        if "perfetto" in extra_collectors:
                            trace_data = json.loads((measurement_root / "perfetto.json").read_text())
                            self.assertGreater(trace_data["metrics"]["perfetto.scheduler_events"], 0)
                            self.assertIn("<svg", (measurement_root / "perfetto-state-timeline.svg").read_text())
                    if sweep:
                        comparison = artifacts / "results/sweep-comparison"
                        seal = json.loads((artifacts / "sweep-results/.checkpoint/inputs.json").read_bytes())
                        self.assertEqual(seal["status"], "stable", seal)
                        names = [entry["name"] for entry in seal["roots"]]
                        self.assertEqual(any(name.startswith("proxy-execution-file-") for name in names), bool(tape_mode))
                        self.assertEqual(any(name.startswith("replay-source-") for name in names), tape_mode == "replay")
                        outcome_seal = json.loads((artifacts / "sweep-results/.checkpoint/completed/000000.json").read_bytes())
                        outputs = [entry for entry in outcome_seal["roots"] if entry["name"].startswith("proxy-output-")]
                        self.assertEqual(len(outputs), 1 if tape_mode == "record" else 0)
                        if tape_mode == "record":
                            self.assertEqual(Path(outputs[0]["path"]),cassette)
                            self.assertTrue(cassette.is_file())
                        coverage = json.loads((comparison / "comparison.json").read_text())
                        self.assertEqual(coverage["evaluation_coverage"]["eligible_selected_attempts"], 1)
                        self.assertEqual(coverage["evaluation_coverage"]["evaluated_attempts"], 1)
                        population = next(iter(coverage["population_coverage"].values()))
                        self.assertEqual(population["selected_tasks"], 1)
                        self.assertEqual(population["population_size"], len(json.loads((root / "workloads" / suite / "catalog.json").read_text())["tasks"]))
                        self.assertIsNone(population["full_population_resolution_rate"])
                        html = (comparison / "report.html").read_text()
                        for graph in ["metric-agent-memory.peak_rss_mib.svg", "metric-agent-workers.spawned_worker_count.svg",
                                      "metric-cpu-accounting.container_cpu_user_ms.svg", "metric-cpu-accounting.container_cpu_system_ms.svg"]:
                            self.assertIn("<svg", (comparison / graph).read_text())
                            self.assertIn(graph, html)
                    if tape_mode == "replay":
                        if host_provider:
                            self.assertFalse((host_root / "calls.json").exists(), "replay contacted the provider")
                            self.assertFalse((host_root / "requests.json").exists(), "replay contacted the provider catalog")
                        else:
                            self.assertNotEqual(provider.container.exec_run(["test", "-e", "/tmp/calls.json"]).exit_code, 0)
                            self.assertNotEqual(provider.container.exec_run(["test", "-e", "/tmp/requests.json"]).exit_code, 0)
                        calls = 2  # The deterministic recorded fixture has two calls.
                    else:
                        calls = json.loads((host_root / "calls.json").read_text() if host_provider else
                                           provider.container.exec_run(["cat", "/tmp/calls.json"]).output)["calls"]
                        self.assertGreaterEqual(calls, 2)
                    self.assertEqual(record["token_usage"]["total_tokens"], calls * 22)
                    identity = record["task"]
                    self.assertEqual(identity["task_id"], instance)
                    matching = [path.parent for path in (root / ".asb/attempts").glob("*/task.json")
                                if json.loads(path.read_text())["identity"] == identity]
                    self.assertEqual(len(matching), 1)
                    receipt = json.loads((matching[0]/"runtime.json").read_text())
                    bootstrap = json.loads((matching[0]/"bootstrap.json").read_text())
                    self.assertEqual(bootstrap["runtime_tools"],["node"])
                    self.assertEqual(len(bootstrap["assets"]),1)
                    if cold:
                        self.assertFalse((root/".asb-tools/node/runtime").exists(),
                                         "guest prefetch unexpectedly replaced or installed a host runtime")
                    if os.environ.get("ASB_TEST_EXPECT_RUNTIME_KIND"):
                        self.assertEqual(receipt["kind"],os.environ["ASB_TEST_EXPECT_RUNTIME_KIND"])
                    observation = json.loads((matching[0] / "execution.json").read_text())
                    self.assertEqual(observation["agent_tree_cleanup"],
                        "delegated-owned-workload" if terminal else "supervisor-confirmed")
                    self.assertLessEqual(abs(record["elapsed_ms"] - observation["elapsed_seconds"] * 1000), 1)
                    self.assertGreater(observation["preparation_seconds"], 0)
                    if terminal:
                        self.assertEqual(json.loads((matching[0] / "submission.json").read_text())["kind"], "in-place-environment")
                        grade = json.loads((matching[0] / "evaluation.json").read_text())
                        if suite == "terminal-bench-1.0":
                            self.assertTrue(grade["test_results"])
                        else:
                            self.assertEqual(grade["rewards"], {"reward":1 if resolved else 0})
                    else:
                        self.assertIn("cright" if resolved else "asb-real-agent.txt", (matching[0] / "candidate.patch").read_text())
                    self.assertEqual(json.loads((matching[0] / "task-cleanup.json").read_text())["status"], "removed")
                    self.assertEqual(client.containers.list(all=True,
                        filters={"label":"asb.attempt="+identity["attempt_id"]}), [],
                        "official grading must finish SDK-owned service/container teardown")
            finally:
                network.remove()


if __name__ == "__main__":
    unittest.main()
