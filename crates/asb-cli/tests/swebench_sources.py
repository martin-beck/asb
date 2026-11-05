"""Original source input/build contracts; no SDK, Docker, network or provider."""
import hashlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
from types import SimpleNamespace
from types import ModuleType
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / "assets/benchmarks"))
import asb_swebench_sources as sources

ATTEMPT = "asb-" + "a" * 32
DATASET = {"source": "official/fixture", "revision": "b" * 40, "split": "test"}


def fixture():
    row = {"instance_id": "owner__repo-1", "repo": "owner/repo", "base_commit": "c" * 40,
           "image": "upstream/fixture:latest", "problem_statement": "Synthetic issue",
           "patch": "SECRET GOLD", "test_patch": "SECRET TEST", "eval_script": "SECRET GRADER",
           "FAIL_TO_PASS": '["initial_failure"]', "PASS_TO_PASS": ["existing_test"]}
    metadata = {key: row[key] for key in ("instance_id", "repo", "base_commit", "image")}
    metadata.update(datasets=[DATASET["source"]], split=DATASET["split"])
    files = {"task.yaml": json.dumps(metadata).encode(), "Dockerfile": b"FROM scratch\n",
             "gold.patch": row["patch"].encode(), "test.patch": row["test_patch"].encode(),
             "eval.sh": row["eval_script"].encode(), "problem_statement.md": row["problem_statement"].encode(),
             "tests.json": json.dumps({"FAIL_TO_PASS": ["initial_failure"], "PASS_TO_PASS": ["existing_test"]}).encode()}
    return row, files


def archive(root, files, extra=()):
    path = root / "source.tar.gz"
    prefix = "swe-bench-tasks-" + sources.SOURCE_REVISION + "/tasks/owner__repo-1/"
    with tarfile.open(path, "w:gz") as output:
        for name, data in files.items():
            member = tarfile.TarInfo(prefix + name)
            member.size = len(data)
            output.addfile(member, io.BytesIO(data))
        for member in extra:
            output.addfile(member, io.BytesIO(b""))
    return path, hashlib.sha256(path.read_bytes()).hexdigest()


class SourceContracts(unittest.TestCase):
    def test_local_and_remote_prebuilt_images_are_preferred_and_only_pull_failures_build(self):
        class DockerFailure(Exception):
            pass
        class ImageNotFound(DockerFailure):
            pass
        class NetworkFailure(Exception):
            pass
        docker, errors = ModuleType("docker"), ModuleType("docker.errors")
        requests, exceptions = ModuleType("requests"), ModuleType("requests.exceptions")
        docker.errors, requests.exceptions = errors, exceptions
        errors.DockerException, errors.ImageNotFound = DockerFailure, ImageNotFound
        exceptions.RequestException = NetworkFailure
        with patch.dict(sys.modules, {"docker": docker, "docker.errors": errors,
                                      "requests": requests, "requests.exceptions": exceptions}):
            for mode in ["local", "pull", "registry-failure", "network-failure", "daemon-failure", "programming-error"]:
                calls = []
                def get(_name):
                    calls.append("get")
                    if mode == "local":
                        return "local-image"
                    if mode == "daemon-failure":
                        raise DockerFailure("daemon unavailable")
                    raise ImageNotFound()
                def pull(_name):
                    calls.append("pull")
                    if mode == "registry-failure":
                        raise DockerFailure()
                    if mode == "network-failure":
                        raise NetworkFailure()
                    if mode == "programming-error":
                        raise ValueError("do not hide a programming error")
                    return "pulled-image"
                def build():
                    calls.append("build")
                    return "built-image", {"source": "original-source"}
                client = SimpleNamespace(images=SimpleNamespace(get=get, pull=pull))
                with self.subTest(mode=mode):
                    if mode in ["daemon-failure", "programming-error"]:
                        with self.assertRaises((DockerFailure, ValueError)):
                            sources.resolve_image(client, "upstream/task", build)
                        self.assertNotIn("build", calls)
                    else:
                        image, receipt = sources.resolve_image(client, "upstream/task", build)
                        expected = "local-image" if mode == "local" else "pulled-image" if mode == "pull" else "built-image"
                        self.assertEqual(image, expected)
                        self.assertEqual(calls, ["get"] if mode == "local" else ["get", "pull"] if mode == "pull" else ["get", "pull", "build"])
                        self.assertTrue(receipt["source"])

    def test_pinned_archive_reads_only_selected_sources_and_never_extracts(self):
        row, files = fixture()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path, digest = archive(root, files)
            selected = sources.read_sources(path, {row["instance_id"]}, digest)
            self.assertEqual(selected, {row["instance_id"]: files})
            self.assertEqual(sources.validate_task(row, selected[row["instance_id"]], DATASET, json.loads), files["Dockerfile"])
            self.assertEqual(list(root.iterdir()), [path])
            with self.assertRaisesRegex(sources.CatalogError, "checksum"):
                sources.read_sources(path)
            with self.assertRaisesRegex(sources.CatalogError, "every selected"):
                sources.read_sources(path, {"missing"}, digest)
            linked = root / "linked"
            linked.symlink_to(path)
            with self.assertRaisesRegex(sources.CatalogError, "unsafe"):
                sources.read_sources(linked, expected_sha256=digest)

    def test_unsafe_archives_and_size_limits_fail_before_publishing_any_context(self):
        _, files = fixture()
        for name, kind in [("../escape", tarfile.REGTYPE), ("/outside", tarfile.REGTYPE),
                           ("swe-bench-tasks-" + sources.SOURCE_REVISION + "/link", tarfile.SYMTYPE)]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                member = tarfile.TarInfo(name)
                member.type = kind
                path, digest = archive(root, files, [member])
                with self.assertRaisesRegex(sources.CatalogError, "unsafe"):
                    sources.read_sources(path, expected_sha256=digest)
        with tempfile.TemporaryDirectory() as temporary:
            path, digest = archive(Path(temporary), files)
            for constant in ["MAX_ARCHIVE_BYTES", "MAX_EXPANDED_BYTES"]:
                with patch.object(sources, constant, 1), self.assertRaises(sources.CatalogError):
                    sources.read_sources(path, expected_sha256=digest)

    def test_exact_dataset_source_identity_and_evaluation_payloads_are_required(self):
        row, files = fixture()
        for key in ("base_commit", "repo", "image", "patch", "test_patch", "eval_script",
                    "problem_statement", "FAIL_TO_PASS", "PASS_TO_PASS"):
            changed = dict(row, **{key: ["different"] if key.endswith("TO_PASS") else "different"})
            with self.subTest(key=key), self.assertRaisesRegex(sources.CatalogError, "disagree"):
                sources.validate_task(changed, files, DATASET, json.loads)
        with self.assertRaisesRegex(sources.CatalogError, "dataset split"):
            sources.validate_task(row, files, dict(DATASET, split="dev"), json.loads)
        with self.assertRaisesRegex(sources.CatalogError, "incomplete"):
            sources.validate_task(row, {}, DATASET, json.loads)
        for instruction in [b"COPY gold.patch /solution", b"ADD https://example.invalid/file /file"]:
            with self.assertRaisesRegex(sources.CatalogError, "additional build context"):
                sources.validate_task(row, dict(files, Dockerfile=instruction), DATASET, json.loads)

    def build_fixture(self, root, code=0, error=None, journaled=False):
        row, files = fixture()
        path, digest = archive(root, files)
        artifacts = root / "attempt"
        artifacts.mkdir(mode=0o700)
        removed, calls = [], []
        image = SimpleNamespace(id="sha256:" + "d" * 64, attrs={"Config": {"Labels": {
            "asb.owner": "asb", "asb.attempt": ATTEMPT}}})
        client = SimpleNamespace(info=lambda:{"ID":"fixture-daemon"},images=SimpleNamespace(get=lambda _name: image,
            remove=lambda name, **options: removed.append((name, options))))
        journal = None
        if journaled:
            from asb_resources import ResourceJournal
            journal = ResourceJournal(client,artifacts,ATTEMPT)
            self.addCleanup(journal.close)
            image.attrs["Config"]["Labels"].update(journal.labels)
            image.attrs["RepoTags"] = ["asb-swebench:" + ATTEMPT]
        client.journal = journal
        cancelled = object()
        def execute(arguments, directory, log, timeout, **options):
            calls.append(arguments)
            self.assertEqual(directory, artifacts)
            self.assertEqual(timeout, sources.SOURCE_BUILD_TIMEOUT_SECONDS)
            self.assertGreater(timeout, 600)
            self.assertIs(options["cancelled"], cancelled)
            if journaled:
                declared = json.loads((artifacts / "resources.json").read_bytes())["resources"]
                self.assertIn({"kind":"images","name":"asb-swebench:" + ATTEMPT},declared)
                self.assertIn("asb.recovery=" + journal.record["token"],arguments)
            context = artifacts / "source-build"
            self.assertEqual([p.name for p in context.iterdir()], ["Dockerfile"])
            self.assertEqual((context / "Dockerfile").read_bytes(), files["Dockerfile"])
            self.assertNotIn("SECRET", (context / "Dockerfile").read_text())
            if error is not None:
                raise error
            return code
        invoke = lambda: sources.build_image(client, row, DATASET, artifacts, ATTEMPT,
            cancelled=cancelled, archive=path, command=["scoped-docker"], executor=execute,
            yaml_loader=json.loads, expected_sha256=digest,journal=journal)
        return invoke, artifacts, client, removed, calls

    def test_failed_buildx_diagnosis_distinguishes_unavailable_from_working(self):
        with patch.object(sources.subprocess, "run", return_value=SimpleNamespace(returncode=0)) as run:
            self.assertFalse(sources.buildx_unavailable(["docker"]))
            self.assertEqual(run.call_args.args[0], ["docker", "buildx", "version"])
        for outcome in [SimpleNamespace(returncode=1), FileNotFoundError(), subprocess.TimeoutExpired("docker", 60)]:
            with self.subTest(outcome=outcome), patch.object(sources.subprocess, "run") as run:
                if isinstance(outcome, Exception):
                    run.side_effect = outcome
                else:
                    run.return_value = outcome
                self.assertTrue(sources.buildx_unavailable(["docker"]))

    def test_failed_build_reports_actionable_missing_buildx(self):
        with tempfile.TemporaryDirectory() as temporary:
            invoke, _, _, _, _ = self.build_fixture(Path(temporary), code=1)
            with patch.object(sources, "buildx_unavailable", return_value=True):
                with self.assertRaisesRegex(sources.CatalogError, "install the Docker Buildx CLI plugin"):
                    invoke()

    def test_other_failed_build_retains_original_diagnostic(self):
        with tempfile.TemporaryDirectory() as temporary:
            invoke, _, _, _, _ = self.build_fixture(Path(temporary), code=7)
            with patch.object(sources, "buildx_unavailable", return_value=False):
                with self.assertRaisesRegex(sources.CatalogError, r"official source image build failed \(exit 7\)"):
                    invoke()

    def test_build_context_contains_only_original_dockerfile_and_records_provenance(self):
        with tempfile.TemporaryDirectory() as temporary:
            invoke, artifacts, client, removed, calls = self.build_fixture(Path(temporary))
            image, receipt = invoke()
            self.assertEqual(receipt["image_id"], image.id)
            self.assertEqual(receipt["revision"], sources.SOURCE_REVISION)
            self.assertEqual(receipt["status"], "built")
            self.assertEqual(json.loads((artifacts / "source-image.json").read_bytes()), receipt)
            self.assertEqual(calls[0][:4], ["scoped-docker", "buildx", "build", "--builder"])
            self.assertIn("asb.attempt=" + ATTEMPT, calls[0])
            self.assertIn("memory=8g", calls[0])
            self.assertIn("cpu-quota=200000", calls[0])
            self.assertEqual(removed, [])
            self.assertEqual(sources.remove_owned_image(client, receipt["tag"], ATTEMPT), "removed")
            self.assertEqual(removed, [(receipt["tag"], {"noprune": True})])

    def test_failed_timed_out_and_cancelled_builds_remove_only_their_owned_tag(self):
        for code, error in [(7, None), (0, InterruptedError("cancelled")),
                            (0, subprocess.TimeoutExpired("fixture", 1))]:
            with self.subTest(code=code, error=error), tempfile.TemporaryDirectory() as temporary:
                invoke, artifacts, _, removed, _ = self.build_fixture(Path(temporary), code, error)
                with self.assertRaises((sources.CatalogError, InterruptedError, subprocess.TimeoutExpired)):
                    invoke()
                receipt = json.loads((artifacts / "source-image.json").read_bytes())
                self.assertEqual(receipt["status"], "failed")
                self.assertEqual(receipt["cleanup"], "removed")
                self.assertEqual(receipt["failure_kind"], "timeout" if isinstance(error, subprocess.TimeoutExpired)
                                 else "cancelled" if error is not None else "build_failed")
                self.assertEqual(removed, [("asb-swebench:" + ATTEMPT, {"noprune": True})])

    def test_cleanup_never_removes_upstream_images_or_other_attempts(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, _, client, removed, _ = self.build_fixture(Path(temporary))
            for tag, attempt in [("upstream/fixture:latest", ATTEMPT), ("asb-swebench:" + ATTEMPT, "other")]:
                with self.assertRaises(sources.CatalogError):
                    sources.remove_owned_image(client, tag, attempt)
            client.images.get = lambda _name: SimpleNamespace(attrs={"Config": {"Labels": {}}})
            with self.assertRaisesRegex(sources.CatalogError, "different owner"):
                sources.remove_owned_image(client, "asb-swebench:" + ATTEMPT, ATTEMPT)
            self.assertEqual(removed, [])

    def test_journaled_source_output_is_declared_before_build_and_bound_to_its_owner(self):
        with tempfile.TemporaryDirectory() as temporary:
            invoke,artifacts,client,removed,calls = self.build_fixture(Path(temporary),journaled=True)
            image,receipt = invoke()
            self.assertEqual(receipt["image_id"],image.id)
            self.assertIn("asb.recovery=" + client.journal.record["token"],calls[0])
            results = client.journal.cleanup_images(client)
            self.assertEqual(results[0]["status"],"removed")
            self.assertEqual(results[0]["image_id"],image.id)
            self.assertEqual(removed,[(receipt["tag"],{"noprune":True})])
            self.assertEqual(json.loads((artifacts / "image-cleanup.json").read_bytes())["attempt"],ATTEMPT)

    def test_replaced_source_tag_is_not_used_or_removed_even_if_attempt_label_matches(self):
        from asb_resources import RecoveryError
        with tempfile.TemporaryDirectory() as temporary:
            invoke,artifacts,client,removed,_ = self.build_fixture(Path(temporary),journaled=True)
            client.images.get("unused").attrs["Config"]["Labels"]["asb.recovery"] = "other"
            with self.assertRaisesRegex(RecoveryError,"different owner"):
                invoke()
            self.assertEqual(removed,[])
            receipt = json.loads((artifacts / "source-image.json").read_bytes())
            self.assertEqual(receipt["status"],"failed")
            self.assertEqual(receipt["cleanup"],"error:CatalogError")


if __name__ == "__main__":
    unittest.main()
