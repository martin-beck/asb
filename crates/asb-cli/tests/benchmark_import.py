"""Grade-only sweep imports: no clients, providers, Docker or test runners."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from test_environment import coverage_env

sys.dont_write_bytecode = True
from benchmark_retained import Fixture, swe
from benchmark_retained import retained,terminal,harbor
from unittest.mock import patch

WORKLOADS = [("swe-bench", "swebench"), ("swe-bench-lite", "swebench"),
             ("swe-bench-verified", "swebench"), ("terminal-bench-1.0", "terminal-bench"),
             ("terminal-bench-2.1", "harbor")]


class SweepFixture:
    def __init__(self, resolved=False, concurrency=1):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.results = self.root / "sweep-results"
        self.checkpoint = self.results / ".checkpoint"
        (self.root / ".asb").mkdir()
        (self.root / ".asb/project.toml").write_text("tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n")
        metadata = self.root / "metadata/swebench-5.0.2.dist-info"
        metadata.mkdir(parents=True)
        (metadata / "METADATA").write_text("Name: swebench\nVersion: 5.0.2\n")
        (metadata / "direct_url.json").write_text(json.dumps({"vcs_info":{"commit_id":swe.HARNESS_REVISION}}))
        self.env = coverage_env({"PATH":os.environ.get("PATH", ""), "HOME":str(self.root),
                    "PYTHONPATH":str(metadata.parent), "PYTHONDONTWRITEBYTECODE":"1"})
        entries = []
        for index, (workload, tool) in enumerate(WORKLOADS):
            python = self.root / "tools" / tool / "venv/bin/python"
            python.parent.mkdir(parents=True, exist_ok=True)
            if not python.exists():
                python.symlink_to(sys.executable)
            reference = f"config-{index}.toml"
            configuration = {"agent":"pi", "workload":workload, "provider":"openrouter", "model":"fixture-model",
                             "concurrency":concurrency, "trial":"case-1", "workspace":"workspace", "log_file":"run.log", "measurements":None}
            text = "\n".join(f"{key}={json.dumps(value)}" for key, value in configuration.items() if value is not None)
            (self.root / reference).write_text(text)
            directory = self.results / f"{index:03}-config-{index}"
            directory.mkdir(parents=True)
            (directory / "benchmark.toml").write_text(text)
            entry = {"reference":reference, "configuration":configuration, "selection":None}
            for slot in range(1, concurrency+1):
                fixture = Fixture(workload, resolved)
                try:
                    fixture.preserve()
                    identity = fixture.request["summary"]["task"]
                    # Actual attempt identities are unique, independent of slots.
                    old_attempt = identity["attempt_id"]
                    attempt = "asb-" + f"{index * concurrency + slot:032x}"
                    for path in fixture.root.rglob("*"):
                        if path.is_file():
                            path.write_bytes(path.read_bytes().replace(old_attempt.encode(), attempt.encode()))
                    old_pane = fixture.root / "official/case-1" / old_attempt
                    if old_pane.exists():
                        old_pane.rename(old_pane.with_name(attempt))
                    preparation = f"a-{index}-{slot}"
                    artifacts = self.root / ".asb/attempts" / preparation
                    shutil.copytree(fixture.root, artifacts)
                    dataset = identity["dataset"]
                    entry["selection"] = {"workload":workload,"task_id":"case-1","dataset":dataset,
                                          "evaluation_kind":"upstream-harness","population_size":1}
                    replica = {"index":slot,"total":concurrency}
                    intent = {"schema_version":1,"scope":"replica-execution-intent","configuration":index,
                              "replica":replica,"input_digest":"f"*64,"intent_id":f"abc-{index}-{slot}","entry":entry}
                    allocation = {"schema_version":1,"scope":"allocated-workload-attempt","configuration":index,
                                  "replica":replica,"input_digest":intent["input_digest"],"intent_id":intent["intent_id"],
                                  "reference":{"schema_version":1,"preparation_id":preparation,"workload":workload,
                                               "task_id":"case-1","dataset":dataset,"artifacts":str(artifacts)}}
                    for kind, value in [("started",intent), ("allocated",allocation)]:
                        path = self.checkpoint / kind / f"{index:06}/{slot:03}.json"
                        path.parent.mkdir(parents=True, exist_ok=True)
                        path.write_text(json.dumps(value))
                finally:
                    fixture.temporary.cleanup()
            entries.append(entry)
        (self.root / "sweep.toml").write_text("configs="+json.dumps([entry["reference"] for entry in entries]))
        (self.results / "plan.json").write_text(json.dumps({"schema_version":1,"run_id":"abc-123","execution_mode":"live",
                                                          "respect_record_timings":False,"entries":entries}))

    def run(self, *arguments):
        return subprocess.run([os.environ["ASB_TEST_HOST_ASB"], *arguments], cwd=self.root, env=self.env,
                              capture_output=True, timeout=30)

    def imports(self):
        return sorted(self.results.glob("*/.asb-imported-grades/*.json"))


@unittest.skipUnless(os.environ.get("ASB_TEST_HOST_ASB"), "requires compiled ASB")
class GradeImportTests(unittest.TestCase):
    def test_closed_slot_reports_all_five_grades_without_using_partial_execution_measurements(self):
        # Synthetic reporting receipts only: actual creation and resume are
        # qualified separately against the checkpoint input snapshot.
        for resolved in [False, True]:
            with self.subTest(resolved=resolved):
                fixture = SweepFixture(resolved=resolved,concurrency=2)
                self.addCleanup(fixture.temporary.cleanup)
                imported = fixture.run("sweep","sweep.toml","--import-grades","--json")
                self.assertEqual(imported.returncode,0,imported.stderr.decode())
                for source in fixture.imports():
                    value = json.loads(source.read_bytes())
                    value["scope"] = "closed-slot-official-grade-only"
                    value["input_digest"] = "f"*64
                    value["roots"] = []
                    value["fingerprint"] = {"schema_version":1,"digest":"e"*64,"bytes":0,"nodes":0}
                    target = fixture.checkpoint / f"closed-slots/{value['configuration']:06}/{value['replica']['index']:03}.json"
                    target.parent.mkdir(parents=True,exist_ok=True)
                    target.write_text(json.dumps(value))
                    measurement = source.parent.parent / f"measurements-concurrency-{value['replica']['index']}"
                    measurement.mkdir()
                    (measurement / "run.json").write_text("unfinished execution record: never parse as final data")
                compared = fixture.run("compare","sweep.toml")
                self.assertEqual(compared.returncode,0,compared.stderr.decode())
                report = json.loads((fixture.root / "results/sweep-comparison/comparison.json").read_bytes())
                self.assertEqual(len(report["benchmarks"]),10)
                for row in report["benchmarks"]:
                    self.assertEqual(row["result_source"],"recovered-official-grade")
                    self.assertTrue(row["reconciled"])
                    self.assertEqual(row["metrics"]["success_rate"],int(resolved))
                    self.assertNotIn("elapsed_ms",row["metrics"])
                    self.assertNotIn("token_usage.total_tokens",row["metrics"])
                    self.assertIsNone(row["execution_cohort"])
                html = (fixture.root / "results/sweep-comparison/report.html").read_text()
                self.assertIn("partial execution artifacts preserved",html)
                # Losing the original plan cannot silently hide the closures.
                plan = fixture.results / "plan.json"
                plan.rename(fixture.root / "saved-plan.json")
                self.assertNotEqual(fixture.run("compare","sweep.toml").returncode,0)

    def test_terminal_sdk_return_reconstruction_imports_both_versions_and_binds_sources(self):
        fixture = SweepFixture()
        self.addCleanup(fixture.temporary.cleanup)
        sources = []
        for index,module in [(3,terminal),(4,harbor)]:
            artifacts = fixture.root/f".asb/attempts/a-{index}-1"
            source = artifacts/("recovered-evaluation-"+"e"*32)
            source.mkdir(mode=0o700)
            (artifacts/"official").rename(source/"official")
            (artifacts/"evaluation.json").rename(source/"evaluation.json")
            identity = json.loads((artifacts/"task.json").read_text())["identity"]
            (source/"evaluation-started.json").write_text(json.dumps({"schema_version":1,
                "scope":"official-verifier-started-not-a-grade","task":identity}))
            cleanup = {"schema_version":1,"scope":"retained-grading-resources-preserved","attempt":identity["attempt_id"],
                       "resources":json.loads((artifacts/"interruption.json").read_text())["resources"]}
            (source/"grading-cleanup.json").write_text(json.dumps(cleanup))
            files = ([f"official/case-1/{identity['attempt_id']}/panes/post-test.txt"] if index == 3 else
                     ["official/verifier/reward.json","official/verifier/reward.txt"])
            retained.record_verifier_return(source,identity,json.loads((source/"evaluation.json").read_text()),
                                             ["evaluation-started.json",*files])
            (source/"evaluation.json").rename(fixture.root/f"lost-wrapper-{index}.json")
            value = retained.RetainedOutcome({"reference":{"artifacts":str(artifacts),
                **{key:identity[key] for key in ["workload","task_id","dataset"]}}},inspect=True)
            value.grading = True
            with patch.object(module,"DockerSession",side_effect=AssertionError("reconstruction started Docker")):
                module.grade_retained(value,"unused",threading.Event())
            sources.append(source)
        imported = fixture.run("sweep","sweep.toml","--import-grades","--json")
        self.assertEqual(imported.returncode,0,imported.stderr.decode())
        compared = fixture.run("compare","sweep.toml")
        self.assertEqual(compared.returncode,0,compared.stderr.decode())
        before = {path:path.read_bytes() for path in fixture.imports()}
        (sources[1]/"verifier-return.json").write_text("{}")
        compared = fixture.run("compare","sweep.toml")
        self.assertNotEqual(compared.returncode,0)
        self.assertEqual(before,{path:path.read_bytes() for path in fixture.imports()})

    def test_reconstructed_swe_grades_import_and_compare_bind_original_raw_artifacts(self):
        fixture = SweepFixture()
        self.addCleanup(fixture.temporary.cleanup)
        originals = []
        for index in range(3):
            artifacts = fixture.root/f".asb/attempts/a-{index}-1"
            source = "evaluation-"+"e"*32
            original = artifacts/source
            (artifacts/"evaluation-fixture").rename(original)
            result = json.loads((original/"evaluation.json").read_text())
            result["run_id"] = "asb-"+"f"*32
            name = "sweb.eval.case-1."+result["run_id"]
            (original/"resources.json").write_text(json.dumps({"schema_version":1,"attempt":result["run_id"],
                "daemon":"fixture-daemon","token":"d"*32,"resources":[{"kind":"containers","name":name}]}))
            (original/"cleanup.json").write_text(json.dumps({"container":name,"status":"absent"}))
            old = dict(result,outcome="evaluation_error")
            (original/"evaluation.json").write_text(json.dumps(old))
            directory = "recovered-evaluation-"+"d"*32
            target = artifacts/directory
            shutil.copytree(original,target)
            inputs = {}
            for relative in ["dataset.json","submission.jsonl","sdk-identity.json","image-identity.json",
                             "official-test-application.json","report.json","summary.json","resources.json",
                             "cleanup.json","evaluation.json"]:
                body = (original/relative).read_bytes()
                inputs[source+"/"+relative] = {"sha256":hashlib.sha256(body).hexdigest(),"bytes":len(body)}
            result["reconstruction"] = {"schema_version":1,"scope":"existing-official-reports-not-test-retry",
                                        "source":source,"inputs":inputs}
            (target/"evaluation.json").write_text(json.dumps(result))
            identity = json.loads((artifacts/"task.json").read_text())["identity"]
            (artifacts/"selected-evaluation.json").write_text(json.dumps({"schema_version":1,
                "scope":"selected-retained-evaluator-not-agent-retry","attempt_id":identity["attempt_id"],"directory":directory}))
            originals.append(original)
        def flip_derived(index):
            target = originals[index].parent/("recovered-evaluation-"+"d"*32)
            before = {target/name:(target/name).read_bytes() for name in ["report.json","summary.json","evaluation.json"]}
            report = json.loads((target/"report.json").read_text())
            report["case-1"]["resolved"] = True
            (target/"report.json").write_text(json.dumps(report))
            summary = json.loads((target/"summary.json").read_text())
            summary["resolved_ids"] = ["case-1"]
            (target/"summary.json").write_text(json.dumps(summary))
            result = json.loads((target/"evaluation.json").read_text())
            result.update(resolved=True,outcome="resolved",reason=None)
            (target/"evaluation.json").write_text(json.dumps(result))
            return before
        source_bytes = {path:path.read_bytes() for directory in originals for path in directory.rglob("*") if path.is_file()}
        edited = flip_derived(2)
        rejected = fixture.run("sweep","sweep.toml","--import-grades","--json")
        self.assertNotEqual(rejected.returncode,0,"a consistent copied-grade flip must fail original-source validation")
        self.assertEqual(fixture.imports(),[],"late invalid copy cannot publish earlier imports")
        for path,body in edited.items():path.write_bytes(body)
        imported = fixture.run("sweep","sweep.toml","--import-grades","--json")
        self.assertEqual(imported.returncode,0,imported.stderr.decode())
        compared = fixture.run("compare","sweep.toml")
        self.assertEqual(compared.returncode,0,compared.stderr.decode())
        before = {path:path.read_bytes() for path in fixture.imports()}
        receipt = originals[0]/"official-test-application.json"
        receipt_bytes = receipt.read_bytes()
        receipt.unlink()
        self.assertNotEqual(fixture.run("compare","sweep.toml").returncode,0)
        self.assertEqual(before,{path:path.read_bytes() for path in fixture.imports()})
        receipt.write_bytes(receipt_bytes)
        for index in range(3):
            edited = flip_derived(index)
            self.assertNotEqual(fixture.run("compare","sweep.toml").returncode,0)
            self.assertEqual(before,{path:path.read_bytes() for path in fixture.imports()})
            self.assertEqual(source_bytes,{path:path.read_bytes() for path in source_bytes})
            for path,body in edited.items():path.write_bytes(body)
        (originals[0]/"report.json").write_text("{}")
        compared = fixture.run("compare","sweep.toml")
        self.assertNotEqual(compared.returncode,0)
        self.assertEqual(before,{path:path.read_bytes() for path in fixture.imports()})

    def test_both_recovered_terminal_grade_imports_require_preserved_resource_cleanup(self):
        fixture = SweepFixture()
        self.addCleanup(fixture.temporary.cleanup)
        for index in [3,4]:
            artifacts = fixture.root/f".asb/attempts/a-{index}-1"
            directory = "recovered-evaluation-"+"d"*32
            target = artifacts/directory
            target.mkdir(mode=0o700)
            (artifacts/"evaluation.json").rename(target/"evaluation.json")
            (artifacts/"official").rename(target/"official")
            identity = json.loads((artifacts/"task.json").read_text())["identity"]
            cleanup = {"schema_version":1,"scope":"retained-grading-resources-preserved","attempt":identity["attempt_id"],
                       "resources":json.loads((artifacts/"interruption.json").read_text())["resources"]}
            (target/"grading-cleanup.json").write_text(json.dumps(cleanup))
            (artifacts/"selected-evaluation.json").write_text(json.dumps({"schema_version":1,
                "scope":"selected-retained-evaluator-not-agent-retry","attempt_id":identity["attempt_id"],"directory":directory}))
        imported = fixture.run("sweep", "sweep.toml", "--import-grades", "--json")
        self.assertEqual(imported.returncode,0,imported.stderr.decode())
        compared = fixture.run("compare", "sweep.toml")
        self.assertEqual(compared.returncode,0,compared.stderr.decode())
        before = {path:path.read_bytes() for path in fixture.imports()}
        cleanup["resources"][0]["status"] = "running"
        (target/"grading-cleanup.json").write_text(json.dumps(cleanup))
        compared = fixture.run("compare", "sweep.toml")
        self.assertNotEqual(compared.returncode,0)
        self.assertEqual(before,{path:path.read_bytes() for path in fixture.imports()})

    def test_explicit_recovered_evaluation_imports_without_guessing_from_failed_predecessors(self):
        fixture = SweepFixture()
        self.addCleanup(fixture.temporary.cleanup)
        for index in range(3):
            artifacts = fixture.root / f".asb/attempts/a-{index}-1"
            directory = "recovered-evaluation-"+f"{index:032x}"
            (artifacts/"evaluation-fixture").rename(artifacts/directory)
            failed = artifacts/("evaluation-"+"e"*32)
            failed.mkdir()
            (failed/"evaluation.json").write_text('{"outcome":"evaluation_timeout"}')
            identity = json.loads((artifacts/"task.json").read_text())["identity"]
            (artifacts/"selected-evaluation.json").write_text(json.dumps({"schema_version":1,
                "scope":"selected-retained-evaluator-not-agent-retry","attempt_id":identity["attempt_id"],"directory":directory}))
        imported = fixture.run("sweep", "sweep.toml", "--import-grades", "--json")
        self.assertEqual(imported.returncode,0,imported.stderr.decode())
        self.assertEqual(json.loads(imported.stdout)["imported"],5)
        compared = fixture.run("compare", "sweep.toml")
        self.assertEqual(compared.returncode,0,compared.stderr.decode())
        for path in fixture.imports()[:3]:
            report = json.loads(path.read_text())["report"]
            self.assertIs(report["workload_success"],False)
            self.assertIn("selected-evaluation.json",report["evidence"])
            self.assertFalse(any(name.startswith("evaluation-") for name in report["evidence"]))

    def test_all_original_adapters_preserve_true_false_grades_unknown_metrics_and_slots(self):
        for resolved in [False, True]:
            with self.subTest(resolved=resolved):
                fixture = SweepFixture(resolved, concurrency=2)
                self.addCleanup(fixture.temporary.cleanup)
                output = fixture.run("sweep", "sweep.toml", "--import-grades", "--json")
                self.assertEqual(output.returncode, 0, output.stderr.decode())
                self.assertEqual(json.loads(output.stdout)["imported"], 10)
                before = {path:path.read_bytes() for path in fixture.imports()}
                again = fixture.run("sweep", "sweep.toml", "--import-grades", "--json")
                self.assertEqual(again.returncode, 0, again.stderr.decode())
                self.assertEqual(json.loads(again.stdout)["unchanged"], 10)
                self.assertEqual(before, {path:path.read_bytes() for path in fixture.imports()})
                compare = fixture.run("compare", "sweep.toml")
                self.assertEqual(compare.returncode, 0, compare.stderr.decode())
                report = json.loads((fixture.root / "results/sweep-comparison/comparison.json").read_text())
                self.assertEqual(len(report["benchmarks"]), 10)
                for row in report["benchmarks"]:
                    self.assertEqual(row["result_source"], "imported-official-grade")
                    self.assertTrue(row["evaluation_completed"])
                    self.assertEqual(row["metrics"]["success_rate"], int(resolved))
                    self.assertIn("unknown", row["status"])
                    self.assertIsNone(row["execution_cohort"])
                    for metric in ["elapsed_ms", "steps", "process_success", "token_usage.total_tokens"]:
                        self.assertNotIn(metric, row["metrics"])
                self.assertTrue(list((fixture.root / "results/sweep-comparison").glob("*.svg")))
                self.assertFalse((fixture.checkpoint / "replicas").exists())
                self.assertFalse((fixture.checkpoint / "completed").exists())
                resumed = fixture.run("sweep", "sweep.toml", "--resume")
                self.assertNotEqual(resumed.returncode, 0)
                self.assertEqual(before, {path:path.read_bytes() for path in fixture.imports()})

    def test_invalid_late_candidate_cannot_publish_earlier_grades(self):
        for mutation in ["grade", "intent", "slot", "selection", "measurement", "duplicate"]:
            with self.subTest(mutation=mutation):
                fixture = SweepFixture()
                self.addCleanup(fixture.temporary.cleanup)
                if mutation == "grade":
                    (fixture.root / ".asb/attempts/a-4-1/evaluation.json").unlink()
                elif mutation == "measurement":
                    path = fixture.results / "004-config-4/measurements/run.json"
                    path.parent.mkdir()
                    path.write_text("{}")
                elif mutation == "duplicate":
                    # Two same-task replicas may not share one actual attempt.
                    path = fixture.checkpoint / "allocated/000004/001.json"
                    value = json.loads(path.read_text())
                    artifact = fixture.root / ".asb/attempts/a-4-1"
                    original = "asb-"+f"{5:032x}"
                    duplicate = "asb-"+f"{1:032x}"
                    for item in artifact.rglob("*"):
                        if item.is_file():item.write_bytes(item.read_bytes().replace(original.encode(),duplicate.encode()))
                else:
                    path = fixture.checkpoint / "started/000004/001.json"
                    value = json.loads(path.read_text())
                    if mutation == "intent":value["intent_id"] = "changed"
                    if mutation == "slot":value["configuration"] = 0
                    if mutation == "selection":value["entry"]["configuration"]["agent"] = "wrong"
                    path.write_text(json.dumps(value))
                    if mutation == "slot":
                        allocation = fixture.checkpoint / "allocated/000004/001.json"
                        value = json.loads(allocation.read_text())
                        value["configuration"] = 0
                        allocation.write_text(json.dumps(value))
                output = fixture.run("sweep", "sweep.toml", "--import-grades")
                self.assertNotEqual(output.returncode, 0)
                self.assertFalse(fixture.imports())

    def test_compare_revalidates_retained_grade_and_original_binding(self):
        fixture = SweepFixture()
        self.addCleanup(fixture.temporary.cleanup)
        output = fixture.run("sweep", "sweep.toml", "--import-grades")
        self.assertEqual(output.returncode, 0, output.stderr.decode())
        path = fixture.root / ".asb/attempts/a-4-1/official/verifier/reward.txt"
        path.write_text("1")
        compared = fixture.run("compare", "sweep.toml")
        self.assertNotEqual(compared.returncode, 0)
        self.assertEqual(len(fixture.imports()), 5)

    def test_compare_rejects_even_matching_measurements_for_imported_slots(self):
        fixture = SweepFixture()
        self.addCleanup(fixture.temporary.cleanup)
        output = fixture.run("sweep", "sweep.toml", "--import-grades")
        self.assertEqual(output.returncode, 0, output.stderr.decode())
        record = json.loads(fixture.imports()[0].read_text())
        summary = dict(record["report"], schema_version=2, agent="pi", workload="swe-bench",
                       provider="openrouter", model="fixture-model")
        path = fixture.results / "000-config-0/measurements/run.json"
        path.parent.mkdir()
        path.write_text(json.dumps(summary))
        original = path.read_bytes()
        compared = fixture.run("compare", "sweep.toml")
        self.assertNotEqual(compared.returncode, 0)
        self.assertIn(b"same slot", compared.stderr)
        self.assertEqual(path.read_bytes(), original)

    def test_import_rejects_execution_flags_before_starting_any_work(self):
        fixture = SweepFixture()
        self.addCleanup(fixture.temporary.cleanup)
        for argument in ["--resume", "--record", "--replay", "--show-output"]:
            output = fixture.run("sweep", "sweep.toml", "--import-grades", argument)
            self.assertNotEqual(output.returncode, 0)
        output = fixture.run("sweep", "sweep.toml", "--import-grades", "--jobs", "2")
        self.assertNotEqual(output.returncode, 0)
        self.assertFalse(fixture.imports())

    def test_evidence_bytes_are_bound_even_when_task_grade_does_not_change(self):
        fixture = SweepFixture()
        self.addCleanup(fixture.temporary.cleanup)
        output = fixture.run("sweep", "sweep.toml", "--import-grades")
        self.assertEqual(output.returncode, 0, output.stderr.decode())
        path = fixture.root / ".asb/attempts/a-4-1/evaluation.json"
        # Whitespace does not change parsed grade, but does change its evidence.
        path.write_text(path.read_text()+"\n")
        output = fixture.run("compare", "sweep.toml")
        self.assertNotEqual(output.returncode, 0)
        self.assertIn(b"changed", output.stderr)

    def test_import_refuses_active_owner_and_preserves_existing_outcome(self):
        import fcntl
        fixture = SweepFixture()
        self.addCleanup(fixture.temporary.cleanup)
        with (fixture.root / ".asb/attempts/a-4-1/resources.lock").open("r+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            output = fixture.run("sweep", "sweep.toml", "--import-grades")
            self.assertNotEqual(output.returncode, 0)
            self.assertIn(b"active owner", output.stderr)
            self.assertFalse(fixture.imports())
        output = fixture.run("sweep", "sweep.toml", "--import-grades")
        self.assertEqual(output.returncode, 0, output.stderr.decode())
        original = {path:path.read_bytes() for path in fixture.imports()}
        directory = fixture.results / "004-config-4/.asb-completions"
        directory.mkdir()
        record = json.loads(original[fixture.results / "004-config-4/.asb-imported-grades/001.json"])
        summary = dict(record["report"], schema_version=2, process_success=True)
        path = directory / "existing.json"
        path.write_text(json.dumps({"schema_version":2,"scope":"task-outcome-not-yet-a-resume-receipt",
                                   "replica":{"index":1,"total":1},"summary":summary,
                                   "workload_cleanup_confirmed":True,"proxy_cleanup_confirmed":True}))
        existing_bytes = path.read_bytes()
        output = fixture.run("sweep", "sweep.toml", "--import-grades")
        self.assertNotEqual(output.returncode, 0)
        self.assertEqual(path.read_bytes(), existing_bytes)
        self.assertEqual(original, {path:path.read_bytes() for path in fixture.imports()})


if __name__ == "__main__":
    unittest.main()
