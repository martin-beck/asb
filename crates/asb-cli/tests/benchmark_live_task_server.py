"""Real task bridge lifecycle tests with synthetic commands, never live providers."""
import base64
import io
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import unittest
import uuid

sys.dont_write_bytecode = True
SOURCE = Path(__file__).parents[1] / "assets/benchmarks"
sys.path.insert(0, str(SOURCE))


class Controller:
    def __init__(self, root, artifacts):
        self.events = queue.Queue()
        self.stderr = (artifacts.parent / (artifacts.name + ".stderr")).open("wb")
        self.process = subprocess.Popen([
            sys.executable, "-u", str(SOURCE / "asb_swebench.py"), "session",
            "--catalog", str(root / "workloads/swe-bench-lite/catalog.json"),
            "--task", "astropy__astropy-12907", "--artifacts", str(artifacts),
        ], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr, text=True,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})
        def read():
            try:
                for line in self.process.stdout:
                    self.events.put(json.loads(line))
            finally:
                self.events.put(None)
        self.reader = threading.Thread(target=read, daemon=True)
        self.reader.start()
    def send(self, **request):
        self.process.stdin.write(json.dumps(request) + "\n")
        self.process.stdin.flush()
    def event(self, timeout=30):
        value = self.events.get(timeout=timeout)
        if value is None:
            raise AssertionError("task bridge exited before expected event")
        return value
    def close(self):
        if not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)
        self.reader.join(timeout=5)
        self.process.stdout.close()
        self.stderr.close()


@unittest.skipUnless(os.environ.get("ASB_TEST_SWECATALOG_ROOT"), "needs explicit upstream Docker qualification")
class BridgeQualification(unittest.TestCase):
    def test_direct_harness_sigterm_removes_grader_before_joining_worker(self):
        from asb_docker import DockerSession
        from asb_swebench import atomic_write, encoded, read_task
        import docker.errors
        root = Path(os.environ["ASB_TEST_SWECATALOG_ROOT"]).absolute()
        artifacts = Path(tempfile.mkdtemp(prefix="official-harness-signal-", dir=root))
        _, row = read_task(root / "workloads/swe-bench-lite/catalog.json", "astropy__astropy-12907")
        run_id = "asb-" + uuid.uuid4().hex
        name = "sweb.eval.astropy__astropy-12907." + run_id
        atomic_write(artifacts / "dataset.json", encoded([row]))
        atomic_write(artifacts / "prediction.jsonl", encoded({
            "instance_id": row["instance_id"], "model_name_or_path": "asb",
            "model_patch": "diff --git a/asb-fixture.txt b/asb-fixture.txt\nnew file mode 100644\n--- /dev/null\n+++ b/asb-fixture.txt\n@@ -0,0 +1 @@\n+fixture\n",
        }))
        with DockerSession() as client, (artifacts / "harness.log").open("wb") as log:
            process = subprocess.Popen([sys.executable, str(SOURCE / "asb_swebench.py"), "harness",
                "--dataset_name", str(artifacts / "dataset.json"), "--split", "test",
                "--instance_ids", row["instance_id"], "--predictions_path", str(artifacts / "prediction.jsonl"),
                "--max_workers", "1", "--open_file_limit", "4096", "--run_id", run_id, "--timeout", "300"],
                cwd=artifacts, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})
            try:
                deadline = time.monotonic() + 30
                while True:
                    self.assertLess(time.monotonic(), deadline, "official grader never started")
                    try:
                        if client.containers.get(name).status == "running":
                            break
                    except docker.errors.NotFound:
                        pass
                    time.sleep(0.1)
                started = time.monotonic()
                process.terminate()
                self.assertNotEqual(process.wait(timeout=15), 0)
                self.assertLess(time.monotonic() - started, 15)
                with self.assertRaises(docker.errors.NotFound):
                    client.containers.get(name)
                self.assertIn(json.loads((artifacts / "cleanup.json").read_bytes())["status"], ["removed", "absent"])
                print(f"Direct harness SIGTERM cleanup passed: {artifacts}", flush=True)
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=10)
                try:
                    client.containers.get(name).remove(force=True)
                except docker.errors.NotFound:
                    pass

    def test_normal_results_remove_resources_and_interruptions_preserve_work(self):
        from asb_docker import DockerSession
        from asb_swebench import read_task
        import docker.errors
        root = Path(os.environ["ASB_TEST_SWECATALOG_ROOT"]).absolute()
        artifacts = Path(tempfile.mkdtemp(prefix="official-task-bridge-", dir=root))
        print(f"Retained task bridge qualification: {artifacts}", flush=True)
        modes = os.environ.get("ASB_TEST_TASK_BRIDGE_MODES", "normal,oracle,eof,sigterm,evaluation-eof").split(",")
        self.assertTrue(modes and set(modes) <= {"normal", "oracle", "eof", "sigterm", "evaluation-eof"})
        with DockerSession() as client:
            for mode in modes:
                with self.subTest(mode=mode):
                    controller = Controller(root, artifacts / mode)
                    cancelled_grader = None
                    try:
                        ready = controller.event()
                        self.assertEqual(ready["event"], "ready", ready)
                        if mode == "sigterm":
                            controller.process.terminate()
                        else:
                            if mode == "eof":
                                command = "printf 'running\\n'; sleep 60"
                            elif mode == "oracle":
                                # Qualification fixture only: no agent is being
                                # credited with producing the reference patch.
                                command = "git apply /tmp/asb-qualification-oracle.patch; printf 'result\\n'; exit 7"
                            else:
                                command = "printf 'rpc\\n' > asb-rpc.txt; printf 'result\\n'; exit 7"
                            controller.send(op="execute", argv=["/bin/sh", "-c", command],
                                            environment={"ASB_FIXTURE_KEY": "fixture-never-echo"}, timeout=900)
                            paused = controller.event()
                            self.assertEqual(paused["event"], "paused", paused)
                            container_id = paused["target"]["container_id"]
                            self.assertEqual(client.containers.get(container_id).exec_run(
                                ["/bin/sh", "-c", "test ! -e /testbed/asb-rpc.txt"]
                            ).exit_code, 0)
                            if mode == "oracle":
                                _, row = read_task(root / "workloads/swe-bench-lite/catalog.json", "astropy__astropy-12907")
                                payload = row["patch"].encode()
                                archive = io.BytesIO()
                                with tarfile.open(fileobj=archive, mode="w") as tar:
                                    entry = tarfile.TarInfo("asb-qualification-oracle.patch")
                                    entry.size = len(payload)
                                    entry.mode = 0o600
                                    tar.addfile(entry, io.BytesIO(payload))
                                self.assertTrue(client.containers.get(container_id).put_archive("/tmp", archive.getvalue()))
                            controller.send(op="resume", exec_id=paused["target"]["exec_id"])
                            released = controller.event()
                            self.assertEqual(released["event"], "release_accepted", released)
                            controller.send(op="activate", exec_id=paused["target"]["exec_id"])
                            activated = controller.event()
                            self.assertEqual(activated["event"], "activation_accepted", activated)
                            output = controller.event()
                            self.assertEqual(output["event"], "output", output)
                            self.assertIn(base64.b64decode(output["data"]), [b"running\n", b"result\n"])
                            if mode == "eof":
                                started = time.monotonic()
                                controller.process.stdin.close()
                            else:
                                completed = controller.event()
                                self.assertEqual(completed["event"], "completed", completed)
                                self.assertEqual(completed["result"]["exit_code"], 7)
                                controller.send(op="capture")
                                captured = controller.event()
                                self.assertEqual(captured["event"], "submission", captured)
                                self.assertGreater(captured["submission"]["patch_bytes"], 0)
                                patch = (artifacts / mode / "candidate.patch").read_text()
                                self.assertIn("diff --git", patch)
                                if mode == "normal":
                                    self.assertIn("asb-rpc.txt", patch)
                                controller.send(op="evaluate", timeout=300)
                                if mode == "evaluation-eof":
                                    deadline = time.monotonic() + 30
                                    while time.monotonic() < deadline:
                                        runs = list((artifacts / mode).glob("evaluation-*/logs/evaluation/asb-*"))
                                        if runs:
                                            name = "sweb.eval.astropy__astropy-12907." + runs[0].name
                                            try:
                                                graded = client.containers.get(name)
                                                if graded.status == "running":
                                                    cancelled_grader = name
                                                    break
                                            except docker.errors.NotFound:
                                                pass
                                        time.sleep(0.1)
                                    self.assertIsNotNone(cancelled_grader, "official evaluator never started")
                                    started = time.monotonic()
                                    controller.process.stdin.close()
                                else:
                                    evaluated = controller.event(timeout=420)
                                    self.assertEqual(evaluated["event"], "evaluated", evaluated)
                                    self.assertEqual(evaluated["result"]["outcome"], "resolved" if mode == "oracle" else "unresolved", evaluated)
                                    self.assertEqual(evaluated["result"]["reason"], None if mode == "oracle" else "official_tests_not_resolved")
                                    self.assertEqual(evaluated["result"]["image"]["declared_image"],
                                                     ready["invocation"]["image"]["image_id"])
                                    controller.send(op="close")
                        seen = []
                        while True:
                            event = controller.event(timeout=20)
                            seen.append(event)
                            if event["event"] == "closed":
                                self.assertEqual(event["cleanup"], "removed" if mode in ["normal", "oracle"] else "stopped-preserved", event)
                                break
                        controller.process.wait(timeout=10)
                        if mode in ["normal", "oracle"]:
                            self.assertEqual(controller.process.returncode, 0)
                        elif mode == "eof":
                            self.assertLess(time.monotonic() - started, 15)
                        elif mode == "evaluation-eof":
                            self.assertLess(time.monotonic() - started, 30)
                            with self.assertRaises(docker.errors.NotFound):
                                client.containers.get(cancelled_grader)
                        self.assertNotIn("fixture-never-echo", json.dumps(seen))
                        cleanup = json.loads((artifacts / mode / "task-cleanup.json").read_bytes())
                        self.assertEqual(cleanup["status"], "removed" if mode in ["normal", "oracle"] else "stopped-preserved")
                        attempt = ready["invocation"]["identity"]["attempt_id"]
                        if mode in ["normal", "oracle"]:
                            with self.assertRaises(docker.errors.NotFound):
                                client.containers.get(attempt)
                        else:
                            retained = client.containers.get(attempt)
                            retained.reload()
                            self.assertIs(retained.attrs["State"]["Running"], False)
                            self.assertTrue((artifacts / mode / "interruption.json").is_file())
                            if mode == "evaluation-eof":
                                self.assertGreater((artifacts / mode / "candidate.patch").stat().st_size, 0)
                        print(f"{mode}: task bridge and owned container stopped", flush=True)
                    finally:
                        controller.close()
                        from benchmark_live_interrupted import cleanup_fixture
                        cleanup_fixture(client, artifacts / mode)


if __name__ == "__main__":
    unittest.main()
