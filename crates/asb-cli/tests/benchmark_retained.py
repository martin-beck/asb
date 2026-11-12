"""Provider-free retained-artifact checks for the five original benchmarks."""
import hashlib
import json
import os
import shutil
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from test_environment import coverage_env
from unittest.mock import patch

sys.dont_write_bytecode = True
SOURCE = Path(__file__).parents[1] / "assets/benchmarks"
sys.path.insert(0,str(SOURCE))
import asb_retained as retained
import asb_swebench as swe
import asb_terminal_bench_1 as terminal
import asb_harbor as harbor


class Fixture:
    def __init__(self,workload,resolved):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.sdk = ({"revision":swe.HARNESS_REVISION,"version":"5.0.2"} if workload.startswith("swe-") else
                    {"package":"harbor","version":"0.24.0"} if workload.endswith("2.1") else
                    {"package":"terminal-bench","version":"0.2.18"})
        identity = {"workload":workload,"task_id":"case-1","attempt_id":"asb-"+"a"*32,
                    "dataset":{"source":"fixture/"+workload,"revision":"b"*40,"split":"test"}}
        baseline = ({"kind":"upstream-task-definition","base_commit":"b"*40} if workload.startswith("swe-") else
                    {"kind":"upstream-environment-definition","source_revision":"b"*40,"definition_sha256":"e"*64})
        image = {"image_id":"sha256:"+"c"*64}
        self.request = {"reference":{**identity,"artifacts":str(self.root)},
                        "summary":{"task":identity,"workload_baseline":baseline,
                                   "workload_success":resolved,"workload_evaluation":"upstream-harness"}}
        self.write("task.json",{"identity":identity,"baseline":baseline,"image":image,"sdk":self.sdk})
        submission = {"task":identity,"image":image}
        self.write("task-cleanup.json",{"attempt_id":identity["attempt_id"],"status":"removed"})
        self.write("resources.json",{"schema_version":1,"attempt":identity["attempt_id"],"daemon":"fixture-daemon","token":"d"*32,"resources":[]})
        (self.root / "resources.lock").touch()
        result = {"schema_version":1,**{key:identity[key] for key in ["workload","task_id","dataset"]},
                  "resolved":resolved,"outcome":"resolved" if resolved else "unresolved","sdk":self.sdk}
        if workload.startswith("swe-"):
            self.module = swe
            body = b"diff --git a/task b/task\nfixture\n"
            (self.root / "candidate.patch").write_bytes(body)
            digest = hashlib.sha256(body).hexdigest()
            submission.update(patch_sha256=digest,patch_bytes=len(body),base_commit="b"*40)
            evaluator_image = {**image,"declared_image":image["image_id"]}
            result.update(patch_sha256=digest,image=evaluator_image,harness_revision=swe.HARNESS_REVISION,official_report="report.json",official_summary="summary.json")
            result["official_test_application"] = swe.TEST_APPLICATION_RECEIPT
            self.write("evaluation-fixture/dataset.json",[{"instance_id":"case-1","base_commit":"b"*40,"image":image["image_id"]}])
            self.write("evaluation-fixture/evaluation.json",result)
            self.write("evaluation-fixture/submission.jsonl",{"instance_id":"case-1","model_patch":body.decode()})
            self.write("evaluation-fixture/report.json",{"case-1":{"resolved":resolved,"patch_successfully_applied":True}})
            self.write("evaluation-fixture/summary.json",{"total_instances":1,"submitted_ids":["case-1"],"completed_ids":["case-1"],"resolved_ids":["case-1"] if resolved else []})
            self.write("evaluation-fixture/sdk-identity.json",self.sdk)
            self.write("evaluation-fixture/image-identity.json",evaluator_image)
            self.write("evaluation-fixture/official-test-application.json",{
                "schema_version":1,"harness_revision":swe.HARNESS_REVISION,
                "task_id":"case-1","official_test_patch_applied":True})
        elif workload.endswith("1.0"):
            self.module = terminal
            result.update(test_results={"test_case":"passed" if resolved else "failed"},failure_mode="none",parse_failure="none")
            self.write("evaluation.json",result)
            path = self.root / "official/case-1" / identity["attempt_id"] / "panes/post-test.txt"
            path.parent.mkdir(parents=True)
            path.write_text("retained verifier output")
        else:
            self.module = harbor
            result["rewards"]={"reward":1 if resolved else 0}
            self.write("evaluation.json",result)
            path=self.root/"official/verifier/reward.txt"
            path.parent.mkdir(parents=True)
            path.write_text("1" if resolved else "0")
        self.write("submission.json",submission)

    def write(self,path,value):
        path=self.root/path
        path.parent.mkdir(parents=True,exist_ok=True)
        path.write_text(json.dumps(value))

    def verify(self):
        with patch.object(swe,"sdk_identity",return_value=self.sdk):
            self.module.verify_retained(retained.RetainedOutcome(self.request))

    def preserve(self):
        attempt = self.request["summary"]["task"]["attempt_id"]
        declarations = [{"kind":"containers","name":attempt},{"kind":"images","name":"asb-workload:"+attempt}]
        self.write("resources.json",{"schema_version":1,"attempt":attempt,"daemon":"fixture-daemon","token":"d"*32,"resources":declarations})
        self.write("task-cleanup.json",{"attempt_id":attempt,"status":"stopped-preserved"})
        self.write("interruption.json",{"schema_version":1,"attempt":attempt,"scope":"preserved-incomplete-attempt-not-grade-not-retry",
            "resources":[dict(declarations[0],status="stopped-preserved"),dict(declarations[1],status="preserved",image_id="sha256:"+"c"*64)]})

    def inspect(self):
        value = retained.RetainedOutcome({"reference":self.request["reference"]},inspect=True)
        with patch.object(swe,"sdk_identity",return_value=self.sdk):
            self.module.inspect_retained(value)
        return value.inspected_grade()


class RetainedTests(unittest.TestCase):
    def test_environment_grades_require_their_actual_environment_baseline_kind_and_revision(self):
        for workload in ["terminal-bench-1.0","terminal-bench-2.1"]:
            for mutation in ["kind","revision","hash"]:
                fixture = Fixture(workload,True)
                try:
                    task = json.loads((fixture.root/"task.json").read_bytes())
                    if mutation == "kind":task["baseline"] = {"kind":"upstream-task-definition","base_commit":"b"*40}
                    if mutation == "revision":task["baseline"]["source_revision"] = "c"*40
                    if mutation == "hash":task["baseline"]["definition_sha256"] = "wrong"
                    fixture.write("task.json",task)
                    with self.assertRaisesRegex(retained.RetainedError,"environment baseline"):
                        fixture.inspect()
                finally:fixture.temporary.cleanup()

    @unittest.skipUnless(os.environ.get("ASB_TEST_HOST_ASB"), "requires compiled ASB CLI")
    def test_public_cli_inspects_every_adapter_from_its_owning_project_without_writing_attempts(self):
        binary = os.environ["ASB_TEST_HOST_ASB"]
        for workload, tool in [("swe-bench","swebench"),("swe-bench-lite","swebench"),("swe-bench-verified","swebench"),
                                ("terminal-bench-1.0","terminal-bench"),("terminal-bench-2.1","harbor")]:
            with self.subTest(workload=workload), tempfile.TemporaryDirectory() as directory:
                root = Path(directory) / "project"
                (root / ".asb").mkdir(parents=True)
                (root / ".asb/project.toml").write_text("tools_dir='custom-tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n")
                metadata = Path(directory) / "sdk-metadata/swebench-5.0.2.dist-info"
                metadata.mkdir(parents=True)
                (metadata / "METADATA").write_text("Name: swebench\nVersion: 5.0.2\n")
                (metadata / "direct_url.json").write_text(json.dumps({"vcs_info":{"commit_id":swe.HARNESS_REVISION}}))
                python = root / "custom-tools" / tool / "venv/bin/python"
                python.parent.mkdir(parents=True)
                python.symlink_to(sys.executable)
                old_helper = python.parents[2] / "asb_retained.py"
                old_helper.write_text("raise RuntimeError('installed helper must not be replaced or used')\n")
                installed_bytes = old_helper.read_bytes()
                fixture = Fixture(workload,False)
                try:
                    fixture.preserve()
                    artifacts = root / ".asb/attempts/a-b-c"
                    shutil.copytree(fixture.root,artifacts)
                    reference = {"schema_version":1,"preparation_id":"a-b-c","workload":workload,"task_id":"case-1",
                                 "dataset":fixture.request["reference"]["dataset"],"artifacts":str(artifacts)}
                    path = Path(directory) / "external-reference.json"
                    path.write_text(json.dumps(reference))
                    path.chmod(0o600)
                    environment=coverage_env({"PATH":os.environ.get("PATH",""),"HOME":directory,
                                 "PYTHONPATH":str(metadata.parent),"PYTHONDONTWRITEBYTECODE":"1"})
                    before={str(item.relative_to(artifacts)):item.read_bytes() for item in artifacts.rglob("*") if item.is_file()}
                    command=[binary,"--project-dir",str(root),"workload","recover",str(path),"--inspect-grade"]
                    output=subprocess.run(command+["--json"],env=environment,cwd=directory,capture_output=True,timeout=15)
                    self.assertEqual(output.returncode,0,output.stderr.decode())
                    report=json.loads(output.stdout)
                    self.assertIs(report["workload_success"],False)
                    self.assertIsNone(report["exit_code"])
                    self.assertEqual(report["task"]["workload"],workload)
                    human=subprocess.run(command,env=environment,cwd=directory,capture_output=True,timeout=15)
                    self.assertEqual(human.returncode,0,human.stderr.decode())
                    self.assertIn(b"unresolved",human.stdout)
                    self.assertIn(b"No agent or evaluator ran",human.stdout)
                    self.assertIn(b"No sweep outcome was imported",human.stdout)
                    self.assertEqual(old_helper.read_bytes(),installed_bytes)
                    self.assertEqual(list((root/".asb").glob("grade-inspection-*")),[])
                    import fcntl
                    with (artifacts/"resources.lock").open("r+") as owner:
                        fcntl.flock(owner,fcntl.LOCK_EX|fcntl.LOCK_NB)
                        active=subprocess.run(command+["--json"],env=environment,cwd=directory,capture_output=True,timeout=15)
                        self.assertNotEqual(active.returncode,0)
                        self.assertFalse(active.stdout)
                        self.assertIn(b"active owner",active.stderr)
                    self.assertEqual(list((root/".asb").glob("grade-inspection-*")),[])
                    after={str(item.relative_to(artifacts)):item.read_bytes() for item in artifacts.rglob("*") if item.is_file()}
                    self.assertEqual(before,after)
                    # Missing grading evidence cannot silently become zero or a retry.
                    result=artifacts/("evaluation-fixture/evaluation.json" if workload.startswith("swe-") else "evaluation.json")
                    result.unlink()
                    missing=subprocess.run(command+["--json"],env=environment,cwd=directory,capture_output=True,timeout=15)
                    self.assertNotEqual(missing.returncode,0)
                    self.assertFalse(missing.stdout)
                    self.assertEqual(old_helper.read_bytes(),installed_bytes)
                    self.assertEqual(list((root/".asb").glob("grade-inspection-*")),[])
                finally:fixture.temporary.cleanup()

    def test_all_original_inspections_preserve_grades_and_do_not_invent_execution_metrics(self):
        for workload in ["swe-bench","swe-bench-lite","swe-bench-verified","terminal-bench-1.0","terminal-bench-2.1"]:
            for resolved in [False,True]:
                for preserved in [False,True]:
                    with self.subTest(workload=workload,resolved=resolved,preserved=preserved):
                        fixture = Fixture(workload,resolved)
                        try:
                            if preserved:fixture.preserve()
                            before={str(path.relative_to(fixture.root)):path.read_bytes() for path in fixture.root.rglob("*") if path.is_file()}
                            report=fixture.inspect()
                            self.assertEqual(report["workload_success"],resolved)
                            self.assertEqual(report["task"],fixture.request["summary"]["task"])
                            self.assertEqual(report["scope"],"retained-official-grade-not-execution-recovery")
                            self.assertEqual(report["recorded_cleanup"],"stopped-preserved" if preserved else "removed")
                            for field in ["process_success","exit_code","elapsed_ms","token_usage","steps"]:
                                self.assertIn(field,report)
                                self.assertIsNone(report[field])
                            after={str(path.relative_to(fixture.root)):path.read_bytes() for path in fixture.root.rglob("*") if path.is_file()}
                            self.assertEqual(before,after)
                            if preserved:
                                with self.assertRaises(retained.RetainedError):fixture.verify()
                        finally:fixture.temporary.cleanup()

    def test_interrupted_errors_changed_submissions_and_ambiguous_grades_cannot_be_inspected(self):
        for workload in ["swe-bench-lite","terminal-bench-1.0","terminal-bench-2.1"]:
            fixture=Fixture(workload,False)
            try:
                fixture.preserve()
                result_path="evaluation-fixture/evaluation.json" if workload.startswith("swe-") else "evaluation.json"
                original=json.loads((fixture.root/result_path).read_bytes())
                fixture.write(result_path,{**original,"outcome":"evaluation_error"})
                with self.assertRaises(retained.RetainedError):fixture.inspect()
                fixture.write(result_path,{**original,"resolved":True,"outcome":"resolved"})
                with self.assertRaises(retained.RetainedError):fixture.inspect()
                fixture.write(result_path,original)
                interruption=json.loads((fixture.root/"interruption.json").read_bytes())
                for changes in [{"resources":[]},{"attempt":"another"}]:
                    fixture.write("interruption.json",{**interruption,**changes})
                    with self.assertRaises(retained.RetainedError):fixture.inspect()
                fixture.write("interruption.json",interruption)
                if workload.startswith("swe-"):
                    (fixture.root/"evaluation-other").mkdir()
                    with self.assertRaises(retained.RetainedError):fixture.inspect()
                    (fixture.root/"evaluation-other").rmdir()
                    (fixture.root/"candidate.patch").write_bytes(b"changed candidate")
                    with self.assertRaises(retained.RetainedError):fixture.inspect()
            finally:fixture.temporary.cleanup()
    def test_all_five_original_adapter_contracts_preserve_true_and_false_grades(self):
        for workload in ["swe-bench","swe-bench-lite","swe-bench-verified","terminal-bench-1.0","terminal-bench-2.1"]:
            for resolved in [False,True]:
                with self.subTest(workload=workload,resolved=resolved):
                    fixture=Fixture(workload,resolved)
                    try:
                        before={str(path.relative_to(fixture.root)):path.read_bytes() for path in fixture.root.rglob("*") if path.is_file()}
                        fixture.verify()
                        after={str(path.relative_to(fixture.root)):path.read_bytes() for path in fixture.root.rglob("*") if path.is_file()}
                        self.assertEqual(before,after,"validation must not rewrite any retained artifact")
                        fixture.request["summary"]["workload_success"]=not resolved
                        with self.assertRaises(retained.RetainedError):
                            fixture.verify()
                    finally:
                        fixture.temporary.cleanup()

    def test_missing_grade_cleanup_wrong_identity_and_changed_patch_are_not_completion(self):
        fixture=Fixture("swe-bench",True)
        self.addCleanup(fixture.temporary.cleanup)
        fixture.request["summary"]["workload_success"]=None
        with self.assertRaises(retained.RetainedError):fixture.verify()
        fixture.request["summary"]["workload_success"]=True
        fixture.write("task-cleanup.json",{"attempt_id":"other","status":"removed"})
        with self.assertRaises(retained.RetainedError):fixture.verify()
        fixture.write("task-cleanup.json",{"attempt_id":fixture.request["summary"]["task"]["attempt_id"],"status":"removed"})
        (fixture.root/"candidate.patch").write_text("changed candidate")
        with self.assertRaises(retained.RetainedError):fixture.verify()

    def test_symlink_escape_is_rejected_and_reward_precedence_matches_the_sdk(self):
        fixture=Fixture("terminal-bench-2.1",True)
        self.addCleanup(fixture.temporary.cleanup)
        with self.assertRaises(retained.RetainedError):retained.RetainedOutcome(fixture.request).child("../outside")
        fixture.write("official/verifier/reward.json",{"reward":1})
        fixture.verify()
        (fixture.root/"official/verifier/reward.json").unlink()
        path=fixture.root/"official/verifier/reward.txt"
        path.unlink()
        path.symlink_to(fixture.root/"submission.json")
        with self.assertRaises(retained.RetainedError):fixture.verify()

    def test_empty_swebench_submission_can_only_retain_an_unresolved_grade(self):
        fixture=Fixture("swe-bench-lite",False)
        self.addCleanup(fixture.temporary.cleanup)
        (fixture.root/"candidate.patch").write_bytes(b"")
        submission=json.loads((fixture.root/"submission.json").read_bytes())
        submission.update(patch_sha256=hashlib.sha256(b"").hexdigest(),patch_bytes=0)
        fixture.write("submission.json",submission)
        result=json.loads((fixture.root/"evaluation-fixture/evaluation.json").read_bytes())
        result.update(patch_sha256=submission["patch_sha256"],reason="empty_submission")
        fixture.write("evaluation-fixture/evaluation.json",result)
        fixture.verify()
        fixture.request["summary"]["workload_success"]=True
        with self.assertRaises(retained.RetainedError):fixture.verify()

    def test_cli_is_bounded_read_only_and_refuses_an_active_owner(self):
        import fcntl
        fixture=Fixture("terminal-bench-1.0",True)
        self.addCleanup(fixture.temporary.cleanup)
        (fixture.root/"asb_fixture_retained.py").write_text("def verify_retained(value): value.grade(value.object('evaluation.json'))\n")
        command=[sys.executable,str(SOURCE/"asb_retained.py"),"--module","asb_fixture_retained"]
        environment={"PYTHONPATH":str(fixture.root),"PYTHONDONTWRITEBYTECODE":"1"}
        def invoke(body):
            return subprocess.run(command,input=body,capture_output=True,env=environment,timeout=5)
        request=(json.dumps(fixture.request)+"\n").encode()
        result=invoke(request)
        self.assertEqual(result.returncode,0,result.stderr.decode())
        self.assertEqual(json.loads(result.stdout)["scope"],"retained-artifact-consistency-not-historical-attestation")
        with (fixture.root/"resources.lock").open("r+") as lock:
            fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
            self.assertNotEqual(invoke(request).returncode,0)
        self.assertNotEqual(invoke(b"{"+b" "*(1024*1024)+b"\n").returncode,0)

    def test_owned_build_images_require_their_own_cleanup_evidence(self):
        for workload in ["swe-bench","swe-bench-lite","swe-bench-verified","terminal-bench-1.0","terminal-bench-2.1"]:
            fixture = Fixture(workload,True)
            try:
                attempt = fixture.request["summary"]["task"]["attempt_id"]
                tag = "asb-workload:" + attempt
                fixture.write("resources.json",{"attempt":attempt,"resources":[{"kind":"images","name":tag}]})
                with self.assertRaises(FileNotFoundError):fixture.verify()
                cleanup = {"schema_version":1,"attempt":attempt,"scope":"owned-build-tags-only",
                    "resources":[{"kind":"images","name":tag,"status":"removed","image_id":"sha256:"+"c"*64}]}
                fixture.write("image-cleanup.json",cleanup)
                fixture.verify()
                for value in ["preserved","unknown"]:
                    cleanup["resources"][0]["status"] = value
                    fixture.write("image-cleanup.json",cleanup)
                    with self.assertRaises(retained.RetainedError):fixture.verify()
                cleanup["resources"][0]["status"] = "removed"
                cleanup["resources"][0]["name"] = "another-tag"
                fixture.write("image-cleanup.json",cleanup)
                with self.assertRaises(retained.RetainedError):fixture.verify()
            finally:
                fixture.temporary.cleanup()


if __name__ == "__main__":unittest.main()
