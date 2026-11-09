"""No dataset downloads, Docker calls or provider interactions in these tests."""

import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import subprocess
import threading
import time
import unittest
import sys
from types import SimpleNamespace


SOURCE = Path(__file__).parents[1] / "assets/benchmarks/asb_swebench.py"
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("asb_swebench_catalog", SOURCE)
CATALOG = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CATALOG)
REVISION = "a" * 40


def row(task_id="owner__repo-1"):
    return {
        "instance_id": task_id, "repo": "owner/repo", "base_commit": "b" * 40,
        "problem_statement": "Repair the synthetic failing fixture.\nMore context.",
        "patch": "SYNTHETIC_GOLD_MUST_NOT_ENTER_PUBLIC_CATALOG",
        "test_patch": "SYNTHETIC_TEST_PATCH_MUST_NOT_ENTER_PUBLIC_CATALOG",
        "image": "official/synthetic-task:latest",
    }


class CatalogTests(unittest.TestCase):
    def test_evaluator_start_checks_affinity_before_returning_to_harness(self):
        events = []
        container = SimpleNamespace(start=lambda: events.append('start'))
        CATALOG.verify_evaluator_start(container, '1,3', lambda obj, cpus: events.append((obj, cpus)))
        container.start()
        self.assertEqual(events, ['start', (container, '1,3')])

    def test_evaluator_affinity_failure_is_propagated(self):
        container = SimpleNamespace(start=lambda: None)
        def reject(_container, _cpus):
            raise ValueError('affinity mismatch')
        CATALOG.verify_evaluator_start(container, '1', reject)
        with self.assertRaisesRegex(ValueError, 'affinity mismatch'):
            container.start()

    def test_evaluator_default_does_not_change_start(self):
        container = SimpleNamespace(start=lambda: 'started')
        original = container.start
        CATALOG.verify_evaluator_start(container, None, lambda *_: self.fail('unexpected verification'))
        self.assertIs(container.start, original)

    def test_official_report_keeps_grading_arguments_but_never_inventories_the_host(self):
        class UnrelatedDaemon:
            def __getattr__(self,name):
                raise AssertionError("report tried to inspect unrelated daemon resources")
        for grade in [True,False,None]:
            calls = []
            predictions = {"fixture":{"resolved":grade}}
            dataset = [{"instance_id":"fixture"}]
            sentinel = object()
            def reporter(predictions,full_dataset,run_id,client):
                calls.append((predictions,full_dataset,run_id,client))
                return sentinel
            scoped = CATALOG.attempt_report(reporter)
            self.assertIs(scoped(predictions,dataset,"asb-fixture",UnrelatedDaemon()),sentinel)
            self.assertEqual(calls,[(predictions,dataset,"asb-fixture",None)])

    def test_installed_sdk_identity_comes_from_the_actual_package_receipt(self):
        for revision in [None, "wrong-source", CATALOG.HARNESS_REVISION]:
            receipt = json.dumps({"vcs_info": {"commit_id": revision}})
            distribution = SimpleNamespace(version="synthetic", read_text=lambda _name: receipt)
            if revision == CATALOG.HARNESS_REVISION:
                self.assertEqual(CATALOG.sdk_identity(distribution), {"version": "synthetic", "revision": revision})
            else:
                with self.assertRaisesRegex(CATALOG.CatalogError, "asb tool install swebench"):
                    CATALOG.sdk_identity(distribution)

    def build(self, rows, directory, revision=REVISION, validate=None):
        return CATALOG.build_catalog(rows, "suite", "official/source", revision,
                                     "test", directory, validate)

    def test_private_metadata_public_identity_and_immutable_hashes(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            catalog = self.build([row("owner__repo-2"), row()], directory)
            self.assertEqual([entry["id"] for entry in catalog["tasks"]],
                             ["owner__repo-1", "owner__repo-2"])
            self.assertEqual(catalog["dataset"]["revision"], REVISION)
            public = (directory / "catalog.json").read_text()
            self.assertNotIn("SYNTHETIC_GOLD", public)
            self.assertNotIn("SYNTHETIC_TEST_PATCH", public)
            for entry in catalog["tasks"]:
                metadata = directory / entry["metadata"]
                self.assertEqual(hashlib.sha256(metadata.read_bytes()).hexdigest(),
                                 entry["metadata_sha256"])
                self.assertEqual(metadata.stat().st_mode & 0o777, 0o600)
                self.assertEqual(metadata.parent.stat().st_mode & 0o777, 0o700)
                self.assertIn("patch", json.loads(metadata.read_bytes()))
            self.assertEqual(self.build([row(), row("owner__repo-2")], directory), catalog)

    def test_invalid_or_duplicate_input_cannot_publish_a_catalog(self):
        invalid = [[], [row(), row()], [row("../escape")], [dict(row(), base_commit="bad")]]
        for rows in invalid:
            with tempfile.TemporaryDirectory() as temporary:
                with self.assertRaises(CATALOG.CatalogError):
                    self.build(rows, temporary)
                self.assertFalse((Path(temporary) / "catalog.json").exists())
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(CATALOG.CatalogError):
                self.build([row()], temporary, revision="main")

    def test_changed_or_invalid_metadata_preserves_the_last_complete_catalog(self):
        with tempfile.TemporaryDirectory() as temporary:
            self.build([row()], temporary)
            original = (Path(temporary) / "catalog.json").read_bytes()
            with self.assertRaises(CATALOG.CatalogError):
                self.build([dict(row(), patch="different")], temporary)
            self.assertEqual((Path(temporary) / "catalog.json").read_bytes(), original)
            def reject(_row):
                raise ValueError("official SDK rejected the task schema")
            with self.assertRaises(ValueError):
                self.build([row("owner__repo-3")], temporary, validate=reject)
            self.assertEqual((Path(temporary) / "catalog.json").read_bytes(), original)

    def test_evaluator_directory_cannot_follow_a_symlink(self):
        with tempfile.TemporaryDirectory() as temporary, tempfile.TemporaryDirectory() as other:
            target = Path(other)
            before = target.stat().st_mode
            (Path(temporary) / "evaluation-data").symlink_to(target, target_is_directory=True)
            with self.assertRaises(CATALOG.CatalogError):
                self.build([row()], temporary)
            self.assertEqual(target.stat().st_mode, before)
            self.assertEqual(list(target.iterdir()), [])

    def test_task_selection_verifies_identity_and_only_supplies_public_agent_input(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            catalog = self.build([row()], directory)
            selected, metadata = CATALOG.read_task(directory / "catalog.json", "owner__repo-1")
            self.assertEqual(CATALOG.verify_catalog(directory / "catalog.json"), {"status": "verified", "task_count": 1})
            self.assertEqual(selected, catalog)
            instruction = CATALOG.agent_instruction(metadata)
            self.assertIn("Repair the synthetic failing fixture", instruction)
            self.assertNotIn("SYNTHETIC_GOLD", instruction)
            self.assertNotIn("SYNTHETIC_TEST_PATCH", instruction)
            with self.assertRaises(CATALOG.CatalogError):
                CATALOG.read_task(directory / "catalog.json", "unknown")
            (directory / catalog["tasks"][0]["metadata"]).write_text("{}")
            with self.assertRaisesRegex(CATALOG.CatalogError, "checksum changed"):
                CATALOG.read_task(directory / "catalog.json", "owner__repo-1")
            with self.assertRaisesRegex(CATALOG.CatalogError, "checksum changed"):
                CATALOG.verify_catalog(directory / "catalog.json")

    def test_catalog_cannot_reference_external_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            catalog = self.build([row()], directory)
            catalog["tasks"][0]["metadata"] = "../outside.json"
            (directory / "catalog.json").write_text(json.dumps(catalog))
            with self.assertRaisesRegex(CATALOG.CatalogError, "inside its installed catalog"):
                CATALOG.read_task(directory / "catalog.json", "owner__repo-1")

    def test_metadata_reads_and_agent_instructions_are_bounded(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            catalog = self.build([row()], directory)
            (directory / catalog["tasks"][0]["metadata"]).write_bytes(b"x" * (CATALOG.MAX_METADATA_BYTES + 1))
            with self.assertRaisesRegex(CATALOG.CatalogError, "bounded read limit"):
                CATALOG.read_task(directory / "catalog.json", "owner__repo-1")
        with self.assertRaisesRegex(CATALOG.CatalogError, "64 KiB"):
            CATALOG.agent_instruction(dict(row(), problem_statement="x" * 65536))


class EvaluationTests(unittest.TestCase):
    def test_controller_disconnect_stops_its_evaluator_subprocess_promptly(self):
        cancelled = threading.Event()
        timer = threading.Timer(0.05, cancelled.set)
        cleanup = []
        with tempfile.TemporaryDirectory() as temporary, (Path(temporary) / "log").open("wb") as log:
            started = time.monotonic()
            timer.start()
            try:
                with self.assertRaises(InterruptedError):
                    CATALOG.run_harness([sys.executable, "-c", "import time; time.sleep(60)"],
                                        temporary, log, 60, cancelled=cancelled,
                                        on_cancel=lambda: cleanup.append("owned evaluator removed"))
            finally:
                timer.cancel()
                timer.join(timeout=1)
            self.assertLess(time.monotonic() - started, 3)
            self.assertEqual(cleanup, ["owned evaluator removed"])

    def test_evaluator_cleanup_is_bound_to_the_exact_private_attempt(self):
        task_id, run_id = "owner__repo-1", "asb-" + "a" * 32
        expected = f"sweb.eval.{task_id}.{run_id}"
        removed, looked_up = [], []
        labels = {"asb.owner":"asb", "asb.attempt":run_id}
        container = SimpleNamespace(name=expected, attrs={"Config":{"Labels":labels}},
                                    remove=lambda **kwargs: removed.append(kwargs))
        def get(name):
            looked_up.append(name)
            return container
        client = SimpleNamespace(containers=SimpleNamespace(get=get))
        self.assertEqual(CATALOG.remove_evaluation_container(client, task_id, run_id)["status"], "removed")
        self.assertEqual(looked_up, [expected])
        self.assertEqual(removed, [{"force": True}])
        container.name = "someone-elses-container"
        self.assertEqual(CATALOG.remove_evaluation_container(client, task_id, run_id)["status"], "error")
        self.assertEqual(len(removed), 1)
        container.name = expected
        labels["asb.attempt"] = "different-owner"
        self.assertEqual(CATALOG.remove_evaluation_container(client, task_id, run_id)["status"], "error")
        self.assertEqual(len(removed), 1)
        with self.assertRaises(CATALOG.CatalogError):
            CATALOG.remove_evaluation_container(client, "../other", run_id)

    def evaluate(self, directory, executor, patch=b"synthetic patch", suffix="evaluation"):
        directory = Path(directory)
        if not (directory / "catalog.json").exists():
            CATALOG.build_catalog([row()], "suite", "official/source", REVISION, "test", directory)
        (directory / "submission.patch").write_bytes(patch)
        return CATALOG.evaluate_submission(directory / "catalog.json", "owner__repo-1",
                                           directory / "submission.patch", directory / suffix,
                                           60, executor)

    def executor(self, resolved, wrong_task=False, applied=True, expected_image=None):
        def execute(arguments, directory, log, timeout):
            self.assertEqual(timeout, 180)
            self.assertEqual(arguments[arguments.index("--max_workers") + 1], "1")
            dataset = json.loads((directory / "dataset.json").read_bytes())
            self.assertEqual(dataset, [dict(row(), image=expected_image or row()["image"])])
            predictions = json.loads((directory / "submission.jsonl").read_bytes())
            self.assertEqual(predictions["model_patch"], "synthetic patch")
            run_id = arguments[arguments.index("--run_id") + 1]
            task_id = "other__repo-2" if wrong_task else "owner__repo-1"
            report_dir = directory / "logs/evaluation" / run_id
            (report_dir / "asb/owner__repo-1").mkdir(parents=True)
            (report_dir / "results.json").write_text(json.dumps({
                "total_instances": 1, "submitted_ids": [task_id], "completed_ids": [task_id],
                "resolved_ids": [task_id] if resolved else [],
            }))
            (report_dir / "asb/owner__repo-1/report.json").write_text(json.dumps({
                task_id: {"resolved": resolved, "patch_successfully_applied": applied},
            }))
            (directory / "image-identity.json").write_text(json.dumps({
                "declared_image": dataset[0]["image"], "image_id": "sha256:" + "f" * 64,
                "repository_digests": ["official/synthetic-task@sha256:" + "f" * 64],
            }))
            (directory / "sdk-identity.json").write_text(json.dumps({
                "revision": CATALOG.HARNESS_REVISION, "version": "synthetic",
            }))
            return 0
        return execute

    def test_official_task_resolution_is_distinct_from_process_exit(self):
        for resolved in [False, True]:
            with tempfile.TemporaryDirectory() as temporary:
                result = self.evaluate(temporary, self.executor(resolved))
                self.assertEqual(result["resolved"], resolved)
                self.assertEqual(result["outcome"], "resolved" if resolved else "unresolved")
                self.assertEqual(result["patch_sha256"], hashlib.sha256(b"synthetic patch").hexdigest())
                self.assertEqual(result["dataset"]["revision"], REVISION)
                self.assertEqual(json.loads((Path(temporary) / "evaluation/evaluation.json").read_text()), result)

    def test_zero_exit_without_report_or_cross_task_report_never_scores(self):
        for executor, reason in [(lambda *args: 0, "official_report_missing"),
                                 (self.executor(True, wrong_task=True), "invalid_official_report"),
                                 (self.executor(True, applied=False), "invalid_official_report")]:
            with tempfile.TemporaryDirectory() as temporary:
                result = self.evaluate(temporary, executor)
                self.assertFalse(result["resolved"])
                self.assertEqual(result["outcome"], "evaluation_error")
                self.assertEqual(result["reason"], reason)

    def test_empty_submission_does_not_launch_the_harness_and_timeouts_are_explicit(self):
        def unexpected(*args):
            self.fail("empty submissions must not start Docker")
        with tempfile.TemporaryDirectory() as temporary:
            result = self.evaluate(temporary, unexpected, patch=b"\n")
            self.assertEqual(result["reason"], "empty_submission")
        def timeout(*args):
            raise subprocess.TimeoutExpired("synthetic", 180)
        with tempfile.TemporaryDirectory() as temporary:
            result = self.evaluate(temporary, timeout)
            self.assertEqual(result["outcome"], "evaluation_timeout")

    def test_changed_sdk_or_wrong_image_cannot_satisfy_official_provenance(self):
        for target in ["sdk-identity.json", "image-identity.json"]:
            def changed(arguments, directory, log, timeout):
                status = self.executor(True)(arguments, directory, log, timeout)
                (directory / target).write_text("{}")
                return status
            with tempfile.TemporaryDirectory() as temporary:
                result = self.evaluate(temporary, changed)
                self.assertFalse(result["resolved"])
                self.assertEqual(result["reason"], "invalid_official_report")

    def test_each_attempt_has_fresh_artifacts_and_cannot_reuse_cached_grades(self):
        with tempfile.TemporaryDirectory() as temporary:
            first = self.evaluate(temporary, self.executor(True))
            with self.assertRaises(FileExistsError):
                self.evaluate(temporary, self.executor(True))
            second = self.evaluate(temporary, self.executor(False), suffix="second")
            self.assertNotEqual(first["run_id"], second["run_id"])
            self.assertNotEqual(first["outcome"], second["outcome"])

    def test_evaluation_binds_to_actual_prepared_image_not_a_later_mutable_tag(self):
        for digest, expected in [("f", True), ("e", False)]:
            with tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                CATALOG.build_catalog([row()], "suite", "official/source", REVISION, "test", directory)
                (directory / "patch").write_text("synthetic patch")
                image = "sha256:" + digest * 64
                result = CATALOG.evaluate_submission(directory / "catalog.json", "owner__repo-1",
                    directory / "patch", directory / "evaluation", 60,
                    self.executor(True, expected_image=image), image_identity={"image_id": image})
                self.assertEqual(result["resolved"], expected)
                self.assertEqual(result["source_image"], row()["image"])


if __name__ == "__main__":
    unittest.main()
