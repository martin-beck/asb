"""Provider-free retained SWE submission grading and owner-loss contracts."""
import contextlib
import fcntl
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import MagicMock, patch

sys.dont_write_bytecode = True
from benchmark_retained import Fixture, retained, swe, SOURCE


class RetainedGradingTests(unittest.TestCase):
    def new(self, workload="swe-bench-lite", resolved=False, ungraded=True):
        fixture = Fixture(workload,resolved)
        self.addCleanup(fixture.temporary.cleanup)
        fixture.preserve()
        if ungraded:shutil.rmtree(fixture.root/"evaluation-fixture")
        else:(fixture.root/"evaluation-fixture").rename(fixture.root/("evaluation-"+"e"*32))
        value = retained.RetainedOutcome(fixture.request,inspect=True)
        value.grading = True
        return fixture,value

    def catalog(self, fixture):
        return ({"workload":fixture.request["reference"]["workload"],
                 "dataset":fixture.request["reference"]["dataset"]},
                {"instance_id":"case-1","base_commit":"b"*40,"image":"sha256:"+"c"*64})

    def raw(self,workload="swe-bench-lite",resolved=False,missing=False):
        fixture,value = self.new(workload,resolved,ungraded=False)
        directory = fixture.root/("evaluation-"+"e"*32)
        directory.chmod(0o700)
        result = json.loads((directory/"evaluation.json").read_text())
        run_id = "asb-"+"f"*32
        container = "sweb.eval.case-1."+run_id
        report = directory/f"logs/evaluation/{run_id}/asb/case-1/report.json"
        report.parent.mkdir(parents=True)
        (directory/"report.json").rename(report)
        summary = directory/f"logs/evaluation/{run_id}/results.json"
        (directory/"summary.json").rename(summary)
        result.update(outcome="evaluation_error",run_id=run_id,
            official_report=str(report.relative_to(directory)),official_summary=str(summary.relative_to(directory)))
        (directory/"evaluation.json").write_text(json.dumps(result))
        if missing:(directory/"evaluation.json").unlink()
        (directory/"resources.json").write_text(json.dumps({"schema_version":1,"attempt":run_id,
            "daemon":"fixture-daemon","token":"d"*32,"resources":[{"kind":"containers","name":container}]}))
        (directory/"cleanup.json").write_text(json.dumps({"container":container,"status":"absent"}))
        (directory/"resources.lock").touch()
        for path in directory.rglob("*"):
            if path.is_file():path.chmod(0o600)
        return fixture,value,directory

    def test_retained_unresolved_grade_requires_official_test_application_receipt(self):
        fixture,value = self.new(ungraded=False)
        directory = fixture.root/("evaluation-"+"e"*32)
        (directory/swe.TEST_APPLICATION_RECEIPT).write_text(json.dumps({
            "schema_version":1,"harness_revision":swe.HARNESS_REVISION,
            "task_id":"case-1","official_test_patch_applied":False}))
        with patch.object(swe,"sdk_identity",return_value=fixture.sdk), \
             patch.object(swe,"read_task",return_value=self.catalog(fixture)):
            with self.assertRaisesRegex(retained.RetainedError,"test patch was applied"):
                swe.grade_retained(value,"unused",threading.Event())

    def reconstruct(self,fixture,value):
        with patch.object(swe,"sdk_identity",return_value=fixture.sdk), \
             patch.object(swe,"read_task",return_value=self.catalog(fixture)), \
             patch.object(swe,"evaluate_submission") as evaluator, \
             patch("asb_docker.DockerSession") as docker:
            try:swe.grade_retained(value,"unused",threading.Event())
            finally:
                evaluator.assert_not_called()
                docker.assert_not_called()

    def evaluator(self, fixture, resolved):
        oracle = Fixture(fixture.request["reference"]["workload"],resolved)
        self.addCleanup(oracle.temporary.cleanup)
        def evaluate(_catalog, _task, _patch, destination, _timeout, **_options):
            shutil.copytree(oracle.root/"evaluation-fixture",destination)
            return json.loads((destination/"evaluation.json").read_bytes())
        return evaluate

    def grade(self, fixture, value, resolved, cancelled=None):
        client = MagicMock()
        image = "sha256:"+"c"*64
        client.images.get.return_value.id = image
        with patch.object(swe,"sdk_identity",return_value=fixture.sdk), \
             patch.object(swe,"read_task",return_value=self.catalog(fixture)), \
             patch("asb_docker.DockerSession",return_value=contextlib.nullcontext(client)), \
             patch.object(swe,"evaluate_submission",side_effect=self.evaluator(fixture,resolved)) as evaluator:
            swe.grade_retained(value,"unused",cancelled or threading.Event())
        return evaluator

    def test_all_three_populations_grade_frozen_submissions_without_agent_execution(self):
        for workload in ["swe-bench","swe-bench-lite","swe-bench-verified"]:
            for resolved in [False,True]:
                with self.subTest(workload=workload,resolved=resolved):
                    fixture,value = self.new(workload,resolved)
                    failed = fixture.root/("evaluation-"+"d"*32)
                    failed.mkdir()
                    (failed/"evaluation.json").write_text(json.dumps({"outcome":"evaluation_timeout"}))
                    original = {path:path.read_bytes() for path in fixture.root.rglob("*") if path.is_file()}
                    evaluator = self.grade(fixture,value,resolved)
                    evaluator.assert_called_once()
                    self.assertEqual({path:path.read_bytes() for path in original},original)
                    selected = json.loads((fixture.root/"selected-evaluation.json").read_bytes())
                    self.assertTrue(selected["directory"].startswith("recovered-evaluation-"))
                    with patch.object(swe,"sdk_identity",return_value=fixture.sdk):
                        report = fixture.inspect()
                    self.assertIs(report["workload_success"],resolved)
                    for field in ["process_success","elapsed_ms","token_usage","steps"]:
                        self.assertIsNone(report[field])
                    self.assertIn("selected-evaluation.json",report["evidence"])

    def test_completed_true_false_grades_are_never_rerun_or_replaced(self):
        for resolved in [False,True]:
            fixture,value = self.new(resolved=resolved,ungraded=False)
            before = {path:path.read_bytes() for path in fixture.root.rglob("*") if path.is_file()}
            with patch.object(swe,"sdk_identity",return_value=fixture.sdk), \
                 patch.object(swe,"read_task",return_value=self.catalog(fixture)), \
                 patch.object(swe,"evaluate_submission") as evaluator, \
                 patch("asb_docker.DockerSession") as docker:
                swe.grade_retained(value,"unused",threading.Event())
                evaluator.assert_not_called()
                docker.assert_not_called()
            self.assertEqual({path:path.read_bytes() for path in before},before)

    def test_corrupt_completed_grade_cannot_authorize_another_grader(self):
        fixture,value = self.new(ungraded=False)
        path = fixture.root/("evaluation-"+"e"*32)/"report.json"
        path.write_text("{}")
        with patch.object(swe,"sdk_identity",return_value=fixture.sdk), \
             patch.object(swe,"read_task",return_value=self.catalog(fixture)), \
             patch.object(swe,"evaluate_submission") as evaluator:
            with self.assertRaises(retained.RetainedError):swe.grade_retained(value,"unused",threading.Event())
            evaluator.assert_not_called()
        self.assertFalse((fixture.root/"selected-evaluation.json").exists())

    def test_all_populations_reconstruct_both_raw_grades_without_docker_or_evaluation(self):
        for workload in ["swe-bench","swe-bench-lite","swe-bench-verified"]:
            for resolved in [False,True]:
                for missing in [False,True]:
                    with self.subTest(workload=workload,resolved=resolved,missing=missing):
                        fixture,value,directory = self.raw(workload,resolved,missing)
                        before = {path:path.read_bytes() for path in directory.rglob("*") if path.is_file()}
                        self.reconstruct(fixture,value)
                        self.assertEqual({path:path.read_bytes() for path in before},before)
                        self.assertEqual(set(before),{path for path in directory.rglob("*") if path.is_file()})
                        self.assertEqual(value.inspected_grade()["workload_success"],resolved)
                        selected = value.selected_evaluation()
                        self.assertTrue(selected.startswith("recovered-evaluation-"))
                        self.assertIn("reconstruction",value.object(selected+"evaluation.json"))
                        with patch.object(swe,"sdk_identity",return_value=fixture.sdk):
                            inspected = fixture.inspect()
                        self.assertEqual(inspected["workload_success"],resolved)
                        self.assertIsNone(inspected["process_success"])
                        repeated = retained.RetainedOutcome(fixture.request,inspect=True)
                        repeated.grading = True
                        self.reconstruct(fixture,repeated)
                        self.assertEqual(before,{path:path.read_bytes() for path in before})

    def test_reconstructed_grade_cannot_hide_changed_sources_or_a_replaced_missing_wrapper(self):
        for mutation in ["report","wrapper","manifest"]:
            fixture,value,directory = self.raw(missing=True)
            self.reconstruct(fixture,value)
            if mutation == "report":
                path = next(directory.glob("logs/evaluation/*/asb/case-1/report.json"))
                path.write_text('{"case-1":{"resolved":true,"patch_successfully_applied":true}}')
            if mutation == "wrapper":(directory/"evaluation.json").write_text("{}")
            if mutation == "manifest":
                path = fixture.root/value.selected_evaluation()/"evaluation.json"
                result = json.loads(path.read_text())
                result["reconstruction"]["inputs"].pop(next(iter(result["reconstruction"]["inputs"])))
                path.write_text(json.dumps(result))
            with self.subTest(mutation=mutation),patch.object(swe,"sdk_identity",return_value=fixture.sdk):
                with self.assertRaises(retained.RetainedError):fixture.inspect()

    def test_reconstructed_copies_bind_every_original_input_even_for_semantically_identical_edits(self):
        for relative in ["dataset.json","submission.jsonl","sdk-identity.json","image-identity.json",
                         "resources.json","cleanup.json","report","summary"]:
            fixture,value,directory = self.raw(missing=True)
            self.reconstruct(fixture,value)
            result = value.object(value.selected_evaluation()+"evaluation.json")
            actual = result["official_report"] if relative == "report" else result["official_summary"] if relative == "summary" else relative
            copied = fixture.root/value.selected_evaluation()/actual
            source = directory/actual
            original = source.read_bytes()
            copied.write_bytes(copied.read_bytes()+b"\n")
            with self.subTest(artifact=relative),patch.object(swe,"sdk_identity",return_value=fixture.sdk):
                with self.assertRaisesRegex(retained.RetainedError,"copy differs"):
                    fixture.inspect()
            self.assertEqual(source.read_bytes(),original)

    def test_consistent_derived_grade_flip_cannot_replace_an_unchanged_official_false_grade(self):
        for workload in ["swe-bench","swe-bench-lite","swe-bench-verified"]:
            for missing in [False,True]:
                fixture,value,directory = self.raw(workload,False,missing)
                self.reconstruct(fixture,value)
                original = {path:path.read_bytes() for path in directory.rglob("*") if path.is_file()}
                target = fixture.root/value.selected_evaluation()
                result = json.loads((target/"evaluation.json").read_text())
                manifest = result["reconstruction"]
                report = target/result["official_report"]
                summary = target/result["official_summary"]
                report.write_text(json.dumps({"case-1":{"resolved":True,"patch_successfully_applied":True}}))
                body = json.loads(summary.read_text()); body["resolved_ids"] = ["case-1"]
                summary.write_text(json.dumps(body))
                result.update(resolved=True,outcome="resolved",reason=None)
                (target/"evaluation.json").write_text(json.dumps(result))
                with self.subTest(workload=workload,missing=missing),patch.object(swe,"sdk_identity",return_value=fixture.sdk):
                    with self.assertRaisesRegex(retained.RetainedError,"copy differs"):
                        fixture.inspect()
                self.assertEqual(original,{path:path.read_bytes() for path in original})
                self.assertEqual(manifest,json.loads((target/"evaluation.json").read_text())["reconstruction"])

    def test_old_grader_publication_during_quiescence_is_reused_under_exclusion_for_both_grades(self):
        for workload in ["swe-bench","swe-bench-lite","swe-bench-verified"]:
            for resolved in [False,True]:
                for publication in ["normalized","raw"]:
                    fixture,value,directory = self.raw(workload,resolved)
                    report = next(directory.glob("logs/evaluation/*/asb/case-1/report.json"))
                    summary = next(directory.glob("logs/evaluation/*/results.json"))
                    pending = {report:report.read_bytes(),summary:summary.read_bytes()}
                    for path in pending:path.unlink()
                    completed = json.loads((directory/"evaluation.json").read_text())
                    completed.update(outcome="resolved" if resolved else "unresolved",
                                     reason=None if resolved else "official_tests_not_resolved")
                    published = {}
                    def publish_old(_client,path):
                        self.assertEqual(path,directory)
                        for target,body in pending.items():target.write_bytes(body)
                        if publication == "normalized":
                            (directory/"evaluation.json").write_text(json.dumps(completed))
                        published.update({path:path.read_bytes() for path in directory.rglob("*") if path.is_file()})
                    with self.subTest(workload=workload,resolved=resolved,publication=publication), \
                         patch.object(swe,"sdk_identity",return_value=fixture.sdk), \
                         patch.object(swe,"read_task",return_value=self.catalog(fixture)), \
                         patch("asb_docker.DockerSession",return_value=contextlib.nullcontext(MagicMock())), \
                         patch("asb_resources.quiesce",side_effect=publish_old) as quiesce, \
                         patch.object(swe,"evaluate_submission") as evaluator:
                        swe.grade_retained(value,"unused",threading.Event())
                        quiesce.assert_called_once()
                        evaluator.assert_not_called()
                        self.assertEqual(value.inspected_grade()["workload_success"],resolved)
                        self.assertEqual(fixture.inspect()["workload_success"],resolved)
                    self.assertEqual(published,{path:path.read_bytes() for path in published})

    def test_old_grader_reacquiring_its_lock_after_quiescence_prevents_new_evaluation(self):
        fixture,value,directory = self.raw()
        for path in directory.glob("logs/evaluation/*/asb/case-1/report.json"):path.unlink()
        for path in directory.glob("logs/evaluation/*/results.json"):path.unlink()
        descriptor = os.open(directory/"resources.lock",os.O_RDWR)
        try:
            with patch.object(swe,"sdk_identity",return_value=fixture.sdk), \
                 patch.object(swe,"read_task",return_value=self.catalog(fixture)), \
                 patch("asb_docker.DockerSession",return_value=contextlib.nullcontext(MagicMock())), \
                 patch("asb_resources.quiesce",side_effect=lambda *_: fcntl.flock(descriptor,fcntl.LOCK_EX|fcntl.LOCK_NB)), \
                 patch.object(swe,"evaluate_submission") as evaluator:
                with self.assertRaisesRegex(retained.RetainedError,"active owner"):
                    swe.grade_retained(value,"unused",threading.Event())
                evaluator.assert_not_called()
        finally:os.close(descriptor)

    def test_reconstruction_publication_before_selection_is_reconciled_without_new_tests(self):
        fixture,value,directory = self.raw()
        self.reconstruct(fixture,value)
        (fixture.root/"selected-evaluation.json").unlink()
        repeated = retained.RetainedOutcome(fixture.request,inspect=True)
        repeated.grading = True
        self.reconstruct(fixture,repeated)
        self.assertIsNotNone(repeated.selected_evaluation())
        self.assertEqual(len(list(fixture.root.glob("recovered-evaluation-*"))),1)

    def test_partial_reconstruction_is_not_mistaken_for_another_official_grader(self):
        fixture,value,_directory = self.raw()
        with patch.object(swe,"atomic_write",side_effect=OSError("fixture publication failure")):
            with self.assertRaises(OSError):self.reconstruct(fixture,value)
        self.assertFalse((fixture.root/"selected-evaluation.json").exists())
        self.assertEqual(list(fixture.root.glob("recovered-evaluation-*")),[])
        self.assertEqual(len(list(fixture.root.glob(".outcome-reconstruction-*"))),1)
        repeated = retained.RetainedOutcome(fixture.request,inspect=True)
        repeated.grading = True
        self.reconstruct(fixture,repeated)
        self.assertIsNotNone(repeated.selected_evaluation())
        self.assertEqual(len(list(fixture.root.glob("recovered-evaluation-*"))),1)

    def test_raw_reconstruction_requires_inactive_owner_and_matching_complete_reports(self):
        from asb_resources import RecoveryError
        for mutation in ["active","journal","cleanup","summary","row","sdk","image","wrapper","partial"]:
            fixture,value,directory = self.raw()
            descriptor = None
            if mutation == "active":
                descriptor = os.open(directory/"resources.lock",os.O_RDWR)
                fcntl.flock(descriptor,fcntl.LOCK_EX|fcntl.LOCK_NB)
            if mutation in ["journal","cleanup"]:(directory/("resources.json" if mutation == "journal" else "cleanup.json")).write_text("{}")
            if mutation == "summary":next(directory.glob("logs/evaluation/*/results.json")).write_text("{}")
            if mutation == "row":(directory/"dataset.json").write_text("[]")
            if mutation == "sdk":(directory/"sdk-identity.json").write_text("{}")
            if mutation == "image":(directory/"image-identity.json").write_text("{}")
            if mutation == "wrapper":
                result = json.loads((directory/"evaluation.json").read_text())
                result["dataset"] = {}
                (directory/"evaluation.json").write_text(json.dumps(result))
            if mutation == "partial":next(directory.glob("logs/evaluation/*/asb/case-1/report.json")).unlink()
            try:
                with self.subTest(mutation=mutation),self.assertRaises((retained.RetainedError,RecoveryError,OSError)):
                    self.reconstruct(fixture,value)
            finally:
                if descriptor is not None:os.close(descriptor)
            self.assertFalse((fixture.root/"selected-evaluation.json").exists())
            self.assertEqual(list(fixture.root.glob("recovered-evaluation-*")),[])

    def test_failed_recovered_grading_preserves_artifacts_without_selecting_a_grade(self):
        fixture,value = self.new()
        def failed(*args,**kwargs):
            directory = args[3]
            directory.mkdir()
            (directory/"evaluation.json").write_text('{"outcome":"evaluation_error","resolved":false}')
            return {"outcome":"evaluation_error","resolved":False}
        client = MagicMock()
        client.images.get.return_value.id = "sha256:"+"c"*64
        with patch.object(swe,"sdk_identity",return_value=fixture.sdk), \
             patch.object(swe,"read_task",return_value=self.catalog(fixture)), \
             patch("asb_docker.DockerSession",return_value=contextlib.nullcontext(client)), \
             patch.object(swe,"evaluate_submission",side_effect=failed):
            with self.assertRaises(retained.RetainedError):swe.grade_retained(value,"unused",threading.Event())
        self.assertFalse((fixture.root/"selected-evaluation.json").exists())
        self.assertEqual(len(list(fixture.root.glob("recovered-evaluation-*/evaluation.json"))),1)
        self.assertTrue((fixture.root/"candidate.patch").is_file())

    def test_active_old_grader_and_missing_ownership_journal_are_not_ignored(self):
        for journal in [False,True]:
            fixture,value = self.new()
            failed = fixture.root/("evaluation-"+"d"*32)
            failed.mkdir()
            if journal:(failed/"resources.json").write_text("{}")
            with patch.object(swe,"sdk_identity",return_value=fixture.sdk), \
                 patch.object(swe,"read_task",return_value=self.catalog(fixture)), \
                 patch("asb_docker.DockerSession",return_value=contextlib.nullcontext(MagicMock())), \
                 patch("asb_resources.quiesce",side_effect=RuntimeError("active owner")), \
                 patch.object(swe,"evaluate_submission") as evaluator:
                with self.assertRaises((retained.RetainedError,RuntimeError)):
                    swe.grade_retained(value,"unused",threading.Event())
                evaluator.assert_not_called()

    def test_cancelled_or_changed_catalog_patch_and_selection_never_start_evaluation(self):
        for mutation in ["cancelled","catalog","patch","selection","sdk"]:
            fixture,value = self.new()
            event = threading.Event()
            catalog = self.catalog(fixture)
            if mutation == "cancelled":event.set()
            if mutation == "catalog":catalog[0]["dataset"] = {"wrong":True}
            if mutation == "patch":(fixture.root/"candidate.patch").write_text("changed")
            if mutation == "selection":(fixture.root/"selected-evaluation.json").write_text("{}")
            if mutation == "sdk":fixture.sdk = {"wrong":True}
            with patch.object(swe,"sdk_identity",return_value=fixture.sdk), \
                 patch.object(swe,"read_task",return_value=catalog), \
                 patch.object(swe,"evaluate_submission") as evaluator, \
                 patch("asb_docker.DockerSession") as docker:
                with self.assertRaises(retained.RetainedError):swe.grade_retained(value,"unused",event)
                evaluator.assert_not_called()
                docker.assert_not_called()

    def test_helper_cancels_its_grading_callback_on_eof_or_sigterm(self):
        for termination in ["eof","term","complete"]:
            fixture,_value = self.new()
            with tempfile.TemporaryDirectory() as directory:
                module = Path(directory)/"asb_grading_fixture.py"
                module.write_text("from pathlib import Path\nfrom asb_retained import RetainedError\n"
                    "def grade_retained(value,catalog,cancelled):\n"
                    " Path(catalog).write_text('started')\n"
                    " if Path(catalog).name=='complete':return\n"
                    " assert cancelled.wait(5),'owner loss not observed'\n"
                    " Path(catalog).write_text('cancelled')\n"
                    " raise RetainedError('grading cancelled')\n"
                    "def inspect_retained(value):\n"
                    " value.select_grade({'resolved':False,'outcome':'unresolved'})\n")
                marker = Path(directory)/("complete" if termination=="complete" else "marker")
                environment = dict(os.environ,PYTHONPATH=directory+os.pathsep+str(SOURCE),PYTHONDONTWRITEBYTECODE="1")
                process = subprocess.Popen([sys.executable,str(SOURCE/"asb_retained.py"),"--module","asb_grading_fixture","--grade"],
                    env=environment,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
                try:
                    process.stdin.write(json.dumps({"reference":fixture.request["reference"],"catalog":str(marker)}).encode()+b"\n")
                    process.stdin.flush()
                    deadline = time.monotonic()+5
                    while not marker.exists() and process.poll() is None and time.monotonic()<deadline:time.sleep(.01)
                    self.assertTrue(marker.exists())
                    if termination == "complete":
                        report = json.loads(process.stdout.readline())
                        self.assertIs(report["workload_success"],False)
                    if termination in ["eof","complete"]:
                        process.stdin.close()
                        process.stdin = None
                    else:process.terminate()
                    stdout,stderr = process.communicate(timeout=10)
                    if termination == "complete":
                        self.assertEqual(process.returncode,0,stderr.decode())
                        self.assertEqual(marker.read_text(),"started")
                    else:
                        self.assertEqual(process.returncode,1,stderr.decode())
                        self.assertEqual(marker.read_text(),"cancelled")
                        self.assertEqual(json.loads(stdout)["reason"],"grading cancelled")
                finally:
                    if process.poll() is None:process.kill()
                    process.wait(timeout=5)


if __name__ == "__main__":unittest.main()
