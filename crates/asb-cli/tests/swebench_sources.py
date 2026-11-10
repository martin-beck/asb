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
    def test_bundled_arm_scikit_lock_matches_recorded_digest(self):
        lock = (Path(sources.__file__).parent / sources.ARM_LEGACY_LOCK).read_bytes()
        self.assertEqual(hashlib.sha256(lock).hexdigest(), sources.ARM_LEGACY_LOCK_SHA256)
        self.assertTrue(lock.startswith(b'@EXPLICIT\n'))

    def test_locked_arm_scikit_adaptation_requires_exact_inputs(self):
        adapted = (b"RUN <<EOF_7724f09cce4e\n"
                   b"conda env create -f /root/environment.yml\nEOF_7724f09cce4e\n")
        lock = b"@EXPLICIT\nhttps://example.invalid/pkg.tar.bz2#1234\n"
        with patch.object(sources, 'ARM_LEGACY_DOCKERFILE_SHA256', hashlib.sha256(adapted).hexdigest()), \
             patch.object(sources, 'ARM_LEGACY_LOCK_SHA256', hashlib.sha256(lock).hexdigest()):
            transformed, digest = sources.locked_arm_dockerfile(sources.ARM_LEGACY_TASK, adapted, lock)
            self.assertEqual(digest, hashlib.sha256(lock).hexdigest())
            self.assertIn(b'conda create -y -n testbed --file', transformed)
            self.assertIn(b'COPY asb_swe_arm_scikit_legacy.lock', transformed)
            self.assertEqual(sources.locked_arm_dockerfile('other', adapted, b'bad'), (adapted, None))
            with self.assertRaisesRegex(sources.CatalogError, 'definition changed'):
                sources.locked_arm_dockerfile(sources.ARM_LEGACY_TASK, adapted + b'changed', lock)
            with self.assertRaisesRegex(sources.CatalogError, 'lock is missing or changed'):
                sources.locked_arm_dockerfile(sources.ARM_LEGACY_TASK, adapted, b'changed')
    def test_modern_arm_scikit_lock_and_exact_input_guards(self):
        lock = (Path(sources.__file__).parent / sources.ARM_MODERN_LOCK).read_bytes()
        self.assertTrue(lock.startswith(b"@EXPLICIT\n"))
        self.assertEqual(hashlib.sha256(lock).hexdigest(), sources.ARM_MODERN_LOCK_SHA256)
        adapted = (b"RUN <<EOF_b4c03f5616a4\n"
                   b"conda env create -f /root/environment.yml\nEOF_b4c03f5616a4\n")
        with patch.object(sources, "ARM_MODERN_DOCKERFILE_SHA256", hashlib.sha256(adapted).hexdigest()):
            result, digest = sources.locked_arm_dockerfile(sources.ARM_MODERN_TASK, adapted, lock)
            self.assertEqual(digest, sources.ARM_MODERN_LOCK_SHA256)
            self.assertIn(b"COPY asb_swe_arm_scikit_modern.lock", result)
            self.assertIn(b"conda create -y -n testbed --file", result)
            with self.assertRaisesRegex(sources.CatalogError, "definition changed"):
                sources.locked_arm_dockerfile(sources.ARM_MODERN_TASK, adapted + b"changed", lock)
            with self.assertRaisesRegex(sources.CatalogError, "lock is missing or changed"):
                sources.locked_arm_dockerfile(sources.ARM_MODERN_TASK, adapted, lock + b"changed")

    def test_source_image_retention_uses_material_identity_without_replacing_conflicts(self):
        from asb_swebench_task import retain_source_image
        from asb_execution import ExecutionError
        class ImageNotFound(Exception): pass
        docker, errors = ModuleType('docker'), ModuleType('docker.errors')
        docker.errors = errors
        errors.ImageNotFound = ImageNotFound
        images = {}
        identity = 'sha256:' + 'b' * 64
        image = SimpleNamespace(id=identity)
        def get(tag):
            if tag not in images: raise ImageNotFound()
            return images[tag]
        def tag(repository, tag):
            images[repository + ':' + tag] = image
            return True
        image.tag = tag
        client = SimpleNamespace(images=SimpleNamespace(get=get))
        with patch.dict(sys.modules, {'docker':docker,'docker.errors':errors}):
            retain_source_image(client, image, identity, {}, 'a' * 64)
            self.assertIs(images[sources.material_cache_tag('a' * 64)], image)
            retain_source_image(client, image, identity, {}, 'a' * 64)
            images[sources.material_cache_tag('a' * 64)] = SimpleNamespace(id='sha256:'+'c'*64, attrs={'Config':{'Labels':{}}})
            with self.assertRaises(ExecutionError):
                retain_source_image(client, image, identity, {}, 'a' * 64)

    def test_prepared_baseline_cache_binds_source_task_and_base_without_replacing_conflicts(self):
        from asb_swebench_task import (load_prepared_baseline, prepared_baseline_cache_tag,
                                       prepared_baseline_labels, retain_prepared_baseline)
        from asb_execution import ExecutionError
        class ImageNotFound(Exception): pass
        docker, errors = ModuleType('docker'), ModuleType('docker.errors')
        docker.errors, errors.ImageNotFound = errors, ImageNotFound
        source_id = 'sha256:' + 'a' * 64
        source = SimpleNamespace(id=source_id, attrs={'Architecture':'arm64', 'Os':'linux'})
        task, base = 'owner__repo-1', 'b' * 40
        tag, labels = prepared_baseline_labels(source, task, base)
        self.assertEqual(tag, prepared_baseline_cache_tag(source_id, task, base))
        self.assertNotEqual(tag, prepared_baseline_cache_tag(source_id, task, 'c' * 40))
        images = {}
        def get(name):
            if name not in images:
                raise ImageNotFound()
            return images[name]
        committed = SimpleNamespace(id='sha256:' + 'd' * 64,
                                    attrs={'Architecture':'arm64', 'Os':'linux',
                                           'Config':{'Labels':dict(labels)}})
        def commit(repository, tag, conf):
            self.assertEqual(repository + ':' + tag, prepared_baseline_cache_tag(source_id, task, base))
            self.assertEqual(conf, {'Labels':labels})
            images[repository + ':' + tag] = committed
            return committed
        client = SimpleNamespace(images=SimpleNamespace(get=get))
        with patch.dict(sys.modules, {'docker':docker, 'docker.errors':errors}):
            self.assertIsNone(load_prepared_baseline(client, source, task, base))
            self.assertIs(retain_prepared_baseline(client, SimpleNamespace(commit=commit), source, task, base), committed)
            self.assertIs(load_prepared_baseline(client, source, task, base), committed)
            committed.attrs['Config']['Labels']['asb.base-commit'] = 'wrong'
            with self.assertRaisesRegex(ExecutionError, 'identity, ownership or architecture'):
                load_prepared_baseline(client, source, task, base)

    def test_prepared_baseline_cache_reuses_verified_material_across_daemon_image_ids(self):
        from asb_swebench_task import (load_prepared_baseline, prepared_baseline_cache_tag,
                                       prepared_baseline_labels, retain_prepared_baseline)
        from asb_execution import ExecutionError
        class ImageNotFound(Exception): pass
        docker, errors = ModuleType('docker'), ModuleType('docker.errors')
        docker.errors, errors.ImageNotFound = errors, ImageNotFound
        material = 'c' * 64
        task, base = 'owner__repo-1', 'b' * 40
        source_a = SimpleNamespace(id='sha256:' + 'a' * 64,
                                   attrs={'Architecture':'arm64', 'Os':'linux'})
        source_b = SimpleNamespace(id='sha256:' + 'd' * 64,
                                   attrs={'Architecture':'arm64', 'Os':'linux'})
        tag, labels = prepared_baseline_labels(source_a, task, base, material)
        self.assertEqual(tag, prepared_baseline_cache_tag(source_a.id, task, base, material))
        self.assertEqual(tag, prepared_baseline_cache_tag(source_b.id, task, base, material))
        self.assertNotEqual(tag, prepared_baseline_cache_tag(source_b.id, task, base))
        for invalid in ('C' * 64, 'c' * 63, 'c' * 64 + '/other'):
            with self.assertRaisesRegex(ExecutionError, 'requires validated image'):
                prepared_baseline_cache_tag(source_a.id, task, base, invalid)
        self.assertEqual(labels['asb.cache'], 'swebench-prepared-baseline-v2')
        self.assertEqual(labels['asb.material-sha256'], material)
        self.assertNotIn('asb.source-image', labels)
        images = {}
        def get(name):
            if name not in images:
                raise ImageNotFound()
            return images[name]
        committed = SimpleNamespace(id='sha256:' + 'e' * 64,
                                    attrs={'Architecture':'arm64', 'Os':'linux',
                                           'Config':{'Labels':dict(labels)}})
        def commit(repository, tag, conf):
            images[repository + ':' + tag] = committed
            self.assertEqual(conf, {'Labels':labels})
            return committed
        client = SimpleNamespace(images=SimpleNamespace(get=get))
        with patch.dict(sys.modules, {'docker':docker, 'docker.errors':errors}):
            self.assertIs(retain_prepared_baseline(client, SimpleNamespace(commit=commit),
                                                    source_a, task, base, material), committed)
            self.assertIs(load_prepared_baseline(client, source_b, task, base, material), committed)
            committed.attrs['Config']['Labels']['asb.material-sha256'] = 'f' * 64
            with self.assertRaisesRegex(ExecutionError, 'identity, ownership or architecture'):
                load_prepared_baseline(client, source_b, task, base, material)
    def test_offline_material_cache_requires_owned_native_image(self):
        class ImageNotFound(Exception): pass
        docker, errors = ModuleType('docker'), ModuleType('docker.errors')
        requests, exceptions = ModuleType('requests'), ModuleType('requests.exceptions')
        docker.errors, requests.exceptions = errors, exceptions
        errors.DockerException, errors.ImageNotFound = Exception, ImageNotFound
        exceptions.RequestException = Exception
        digest = 'a' * 64
        labels = {'asb.owner':'asb', 'asb.attempt':'asb-'+'b'*32, 'asb.recovery':'c'*32}
        image = SimpleNamespace(id='sha256:'+'d'*64, attrs={'Architecture':'arm64','Os':'linux','Config':{'Labels':labels}})
        def get(name):
            if name == sources.material_cache_tag(digest): return image
            raise ImageNotFound()
        def forbidden(*_args): raise AssertionError('offline replay must not access network or build')
        client = SimpleNamespace(images=SimpleNamespace(get=get, pull=forbidden))
        with patch.dict(sys.modules, {'docker':docker,'docker.errors':errors,'requests':requests,'requests.exceptions':exceptions}):
            found, receipt = sources.resolve_image(client, 'swebench/sweb.eval.x86_64.fixture', forbidden, host_arch='aarch64', offline=True, material_sha256=digest)
            self.assertIs(found, image)
            self.assertEqual(receipt['source_owner_labels'], labels)
            labels['asb.owner'] = 'other'
            with self.assertRaisesRegex(sources.CatalogError, 'identity, ownership or architecture'):
                sources.resolve_image(client, 'swebench/sweb.eval.x86_64.fixture', forbidden, host_arch='aarch64', offline=True, material_sha256=digest)
            labels['asb.owner'] = 'asb'
            image.attrs['Architecture'] = 'amd64'
            with self.assertRaisesRegex(sources.CatalogError, 'identity, ownership or architecture'):
                sources.resolve_image(client, 'swebench/sweb.eval.x86_64.fixture', forbidden, host_arch='aarch64', offline=True, material_sha256=digest)
    def test_source_cache_tag_requires_a_canonical_material_digest(self):
        self.assertEqual(sources.material_cache_tag("a" * 64), "asb-swebench-material:" + "a" * 64)
        for value in (None, "", "a" * 63, "A" * 64, "g" * 64, "a" * 64 + "/other"):
            with self.subTest(value=value), self.assertRaises(sources.CatalogError):
                sources.material_cache_tag(value)
    def test_replay_never_pulls_or_builds_missing_task_image(self):
        class DockerFailure(Exception): pass
        class ImageNotFound(DockerFailure): pass
        docker, errors = ModuleType('docker'), ModuleType('docker.errors')
        requests, exceptions = ModuleType('requests'), ModuleType('requests.exceptions')
        docker.errors, requests.exceptions = errors, exceptions
        errors.DockerException, errors.ImageNotFound = DockerFailure, ImageNotFound
        exceptions.RequestException = type('NetworkFailure', (Exception,), {})
        calls = []
        def missing(name): calls.append(('get', name)); raise ImageNotFound()
        def forbidden(*args): raise AssertionError('offline replay attempted network or build')
        client = SimpleNamespace(images=SimpleNamespace(get=missing, pull=forbidden))
        with patch.dict(sys.modules, {'docker':docker, 'docker.errors':errors,
                'requests':requests, 'requests.exceptions':exceptions}):
            for arch in ('x86_64', 'aarch64'):
                with self.subTest(arch=arch), self.assertRaisesRegex(sources.CatalogError, 'prepare it before replay'):
                    sources.resolve_image(client, 'swebench/sweb.eval.x86_64.example', forbidden, host_arch=arch, offline=True)
        self.assertEqual(len(calls), 2)

    def test_arm64_uses_only_native_upstream_images_without_x86_fallback(self):
        class DockerFailure(Exception): pass
        class ImageNotFound(DockerFailure): pass
        class NetworkFailure(Exception): pass
        docker, errors = ModuleType("docker"), ModuleType("docker.errors")
        requests, exceptions = ModuleType("requests"), ModuleType("requests.exceptions")
        docker.errors, requests.exceptions = errors, exceptions
        errors.DockerException, errors.ImageNotFound = DockerFailure, ImageNotFound
        exceptions.RequestException = NetworkFailure
        expected = "swebench/sweb.eval.arm64.org_1776_repo-1:latest"
        original = "swebench/sweb.eval.x86_64.org_1776_repo-1:latest"
        image = SimpleNamespace(attrs={"Architecture":"arm64", "Os":"linux"})
        calls=[]
        def get(name):
            calls.append(("get",name)); raise ImageNotFound()
        def pull(name):
            calls.append(("pull",name)); return image
        client=SimpleNamespace(images=SimpleNamespace(get=get,pull=pull))
        def build(): raise AssertionError("must not build x86 on ARM")
        with patch.dict(sys.modules, {"docker":docker,"docker.errors":errors,
                                      "requests":requests,"requests.exceptions":exceptions}):
            found, receipt = sources.resolve_image(client, original, build, host_arch="aarch64")
            self.assertIs(found,image)
            self.assertEqual(calls,[("get",expected),("pull",expected)])
            self.assertEqual(receipt["architecture"],"arm64")
            image.attrs["Architecture"]="amd64"
            with self.assertRaisesRegex(sources.CatalogError,"not a native"):
                sources.resolve_image(client,original,build,host_arch="aarch64")
            client.images.pull=lambda name: (_ for _ in ()).throw(ImageNotFound())
            image.attrs["Architecture"]="arm64"
            found, receipt = sources.resolve_image(client, original, lambda: (image, {"source":"adapted"}), host_arch="aarch64")
            self.assertIs(found,image)
            self.assertEqual(receipt["fallback_reason"],"upstream_arm64_image_unavailable")
            client.images.pull=lambda name: (_ for _ in ()).throw(NetworkFailure())
            with self.assertRaisesRegex(sources.CatalogError,"registry access"):
                sources.resolve_image(client,original,build,host_arch="aarch64")

    def test_arm64_source_adaptation_preserves_versions_and_rejects_unknown_layouts(self):
        original = (b"FROM --platform=linux/amd64 ubuntu:jammy\n"
                    b"RUN wget Miniconda3-py311_23.11.0-2-Linux-x86_64.sh\n"
                    b"  - python=3.9.20=he870216_1\n"
                    b"  - ld_impl_linux-64=2.40=h123_0\n"
                    b"  - pip:\n      - mpmath==1.3.0\n")
        adapted = sources.arm64_dockerfile(original)
        self.assertIn(b"FROM --platform=linux/arm64", adapted)
        self.assertIn(b"Linux-aarch64.sh", adapted)
        self.assertIn(b"  - python=3.9.20\n", adapted)
        self.assertIn(b"  - ld_impl_linux-aarch64=2.40\n", adapted)
        self.assertIn(b"mpmath==1.3.0", adapted)
        for invalid in [b"FROM scratch", original + b"RUN x86_64-tool\n",
                        original.replace(b"Linux-x86_64.sh", b"Linux-unknown.sh")]:
            with self.assertRaises(sources.CatalogError):
                sources.arm64_dockerfile(invalid)

    def test_arm64_source_build_records_adaptation_and_requires_native_host(self):
        row, files = fixture()
        files["Dockerfile"] = (b"FROM --platform=linux/amd64 ubuntu:jammy\n"
                               b"RUN wget Miniconda3-py311_23.11.0-2-Linux-x86_64.sh\n"
                               b"  - python=3.9.20=he870216_1\n")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, digest = archive(root, files)
            image = SimpleNamespace(id="sha256:" + "d" * 64, attrs={"Architecture":"arm64", "Os":"linux"})
            client = SimpleNamespace(images=SimpleNamespace(get=lambda _: image))
            artifacts = root / "attempt"; artifacts.mkdir()
            calls=[]
            def execute(args, directory, log, timeout, **kwargs):
                calls.append(args)
                self.assertEqual(args[args.index("--platform")+1], "linux/arm64")
                self.assertIn(b"python=3.9.20\n", (artifacts/"source-build/Dockerfile").read_bytes())
                return 0
            with patch("asb_execution.require_native_docker_host", return_value="arm64"):
                actual, receipt = sources.build_image(client, row, DATASET, artifacts, ATTEMPT,
                    archive=source, command=["docker"], executor=execute, yaml_loader=json.loads,
                    expected_sha256=digest, host_arch="aarch64")
            self.assertIs(actual,image)
            self.assertEqual(receipt["adaptation"],"native-arm64-version-pinned-v1")
            self.assertEqual(receipt["dockerfile_sha256"],hashlib.sha256(files["Dockerfile"]).hexdigest())
            self.assertNotEqual(receipt["dockerfile_sha256"],receipt["build_dockerfile_sha256"])
            self.assertEqual(len(calls),1)
            other = root/"wrong-host"; other.mkdir()
            with patch("asb_execution.require_native_docker_host", return_value="amd64"):
                with self.assertRaisesRegex(sources.CatalogError,"native ARM64 Docker host"):
                    sources.build_image(client, row, DATASET, other, ATTEMPT, archive=source,
                        command=["docker"], executor=execute, yaml_loader=json.loads,
                        expected_sha256=digest, host_arch="aarch64")
            self.assertFalse((other/"source-build").exists())

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
                            sources.resolve_image(client, "upstream/task", build, host_arch="x86_64")
                        self.assertNotIn("build", calls)
                    else:
                        image, receipt = sources.resolve_image(client, "upstream/task", build, host_arch="x86_64")
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
            self.assertEqual(timeout, sources.source_build_timeout_seconds())
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
            yaml_loader=json.loads, expected_sha256=digest,journal=journal,host_arch="x86_64")
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

    def test_configured_build_memory_is_bounded_and_recorded(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict("os.environ", {"ASB_SWEBENCH_BUILD_MEMORY_GIB": "16"}):
            invoke, artifacts, _, _, calls = self.build_fixture(Path(temporary))
            _, receipt = invoke()
            self.assertIn("memory=16g", calls[0])
            self.assertEqual(receipt["limits"]["memory_bytes"], 16 * 1024**3)
            self.assertEqual(json.loads((artifacts / "source-image.json").read_bytes())["limits"], receipt["limits"])
        for value in ["0", "257", "01", "-1", "8g", "", "1;echo bad"]:
            with self.subTest(value=value), tempfile.TemporaryDirectory() as temporary, patch.dict("os.environ", {"ASB_SWEBENCH_BUILD_MEMORY_GIB": value}):
                invoke, artifacts, _, _, calls = self.build_fixture(Path(temporary))
                with self.assertRaisesRegex(sources.CatalogError, "ASB_SWEBENCH_BUILD_MEMORY_GIB"):
                    invoke()
                self.assertEqual(calls, [])
                self.assertFalse((artifacts / "source-build").exists())

    def test_source_build_timeout_is_bounded_and_recorded(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict("os.environ", {"ASB_SWEBENCH_BUILD_TIMEOUT_SECONDS": "21600"}):
            invoke, artifacts, _, _, _ = self.build_fixture(Path(temporary))
            _, receipt = invoke()
            self.assertEqual(receipt["limits"]["timeout_seconds"], 21600)
            self.assertEqual(json.loads((artifacts / "source-image.json").read_bytes())["limits"]["timeout_seconds"], 21600)
        for value in ["0", "86401", "01", "-1", "1h", "", "1;echo bad"]:
            with self.subTest(value=value), tempfile.TemporaryDirectory() as temporary, patch.dict("os.environ", {"ASB_SWEBENCH_BUILD_TIMEOUT_SECONDS": value}):
                invoke, artifacts, _, _, calls = self.build_fixture(Path(temporary))
                with self.assertRaisesRegex(sources.CatalogError, "ASB_SWEBENCH_BUILD_TIMEOUT_SECONDS"):
                    invoke()
                self.assertEqual(calls, [])
                self.assertFalse((artifacts / "source-build").exists())

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
