"""Pinned original SWE-bench Dockerfiles, never agent-visible gold/test inputs."""
import argparse
import hashlib
import io
import json
import os
import platform
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile

from asb_swebench import (CatalogError, MAX_CATALOG_BYTES, atomic_write, encoded,
                          read_limited, read_metadata, run_harness)

SOURCE_REVISION = "3d07b464b7b311a0cbfb5ed5b2d8a3b96f84a33d"
SOURCE_SHA256 = "ff31aa1f8d2bb78f16559524264a99b868b7218ca82b390dc750bbc59538aa20"
SOURCE_ARCHIVE = "swe-bench-tasks-" + SOURCE_REVISION + ".tar.gz"
MAX_ARCHIVE_BYTES = 64 * 1024**2
MAX_EXPANDED_BYTES = 256 * 1024**2
# Source image preparation is separate from the agent execution deadline. The
# upstream environment can take substantially longer under CPU emulation.
SOURCE_BUILD_TIMEOUT_SECONDS = 7200
REQUIRED = {"task.yaml", "Dockerfile", "eval.sh", "gold.patch", "test.patch",
            "tests.json", "problem_statement.md"}
ARM_LEGACY_TASK = "scikit-learn__scikit-learn-12827"
ARM_LEGACY_DOCKERFILE_SHA256 = "93702f7ac38fe7aac29eac73032d9163391ce5336c3ab6f1ef250d26c5cde7c1"
ARM_LEGACY_LOCK_SHA256 = "ca9b4c87155d44f14ca8d5f59e5eac7f3ac49ec90f0c891689eb80edd35a606f"
ARM_LEGACY_LOCK = "asb_swe_arm_scikit_legacy.lock"
ARM_MODERN_TASK = "scikit-learn__scikit-learn-25232"
ARM_MODERN_DOCKERFILE_SHA256 = "d8536f86d39351dfaed7485307ad33f7d80c11ab3184a2c3e9269a63a3d29789"
ARM_MODERN_LOCK_SHA256 = "e3b0f7b307141e91cc5a54242d95b0f76c7f090f901b6611b50b0407ed4941d5"
ARM_MODERN_LOCK = "asb_swe_arm_scikit_modern.lock"


def locked_arm_dockerfile(task_id, adapted, lock):
    """Use a reviewed transitive lock only for the exact known task definition."""
    if task_id == ARM_LEGACY_TASK:
        name, docker_hash, lock_hash, marker = ARM_LEGACY_LOCK, ARM_LEGACY_DOCKERFILE_SHA256, ARM_LEGACY_LOCK_SHA256, b"RUN <<EOF_7724f09cce4e"
    elif task_id == ARM_MODERN_TASK:
        name, docker_hash, lock_hash, marker = ARM_MODERN_LOCK, ARM_MODERN_DOCKERFILE_SHA256, ARM_MODERN_LOCK_SHA256, b"RUN <<EOF_b4c03f5616a4"
    else:
        return adapted, None
    if hashlib.sha256(adapted).hexdigest() != docker_hash:
        raise CatalogError("known ARM64 scikit task definition changed; refusing its dependency lock")
    if hashlib.sha256(lock).hexdigest() != lock_hash:
        raise CatalogError("ARM64 scikit dependency lock is missing or changed; reinstall the workload")
    command = b"conda env create -f /root/environment.yml"
    if adapted.count(marker) != 1 or adapted.count(command) != 1:
        raise CatalogError("known ARM64 scikit task definition has an unexpected layout")
    adapted = adapted.replace(marker, b"COPY " + name.encode() + b" /root/asb-arm64-explicit.lock\n" + marker, 1)
    adapted = adapted.replace(command, b"conda create -y -n testbed --file /root/asb-arm64-explicit.lock", 1)
    return adapted, lock_hash


def material_cache_tag(material_sha256):
    """Address a retained source image by the cassette's prepared material."""
    if not isinstance(material_sha256, str) or not re.fullmatch(r"[0-9a-f]{64}", material_sha256):
        raise CatalogError("source image cache requires a valid material SHA-256")
    return "asb-swebench-material:" + material_sha256


def source_build_memory_gib():
    """Return the explicit, bounded source-build memory limit in GiB."""
    value = os.environ.get("ASB_SWEBENCH_BUILD_MEMORY_GIB", "8")
    if not re.fullmatch(r"[1-9][0-9]{0,2}", value) or not 1 <= int(value) <= 256:
        raise CatalogError("ASB_SWEBENCH_BUILD_MEMORY_GIB must be an integer from 1 to 256")
    return int(value)


def source_build_timeout_seconds():
    """Return the bounded source-build deadline; independent of agent runtime."""
    value = os.environ.get("ASB_SWEBENCH_BUILD_TIMEOUT_SECONDS", str(SOURCE_BUILD_TIMEOUT_SECONDS))
    if not re.fullmatch(r"[1-9][0-9]{0,5}", value) or not 1 <= int(value) <= 86400:
        raise CatalogError("ASB_SWEBENCH_BUILD_TIMEOUT_SECONDS must be an integer from 1 to 86400")
    return int(value)


def read_sources(path, tasks=None, expected_sha256=SOURCE_SHA256):
    """Authenticate first, then stream bounded members without tar extraction."""
    path = Path(path)
    if path.is_symlink() or not path.is_file() or path.stat().st_size > MAX_ARCHIVE_BYTES:
        raise CatalogError("official task source archive is missing, unsafe or exceeds 64 MiB; reinstall the workload")
    with path.open("rb") as source:
        payload = source.read(MAX_ARCHIVE_BYTES + 1)
    if len(payload) > MAX_ARCHIVE_BYTES:
        raise CatalogError("official task source archive exceeds 64 MiB")
    if hashlib.sha256(payload).hexdigest() != expected_sha256:
        raise CatalogError("official task source archive checksum does not match its pinned revision")
    root = "swe-bench-tasks-" + SOURCE_REVISION
    result, seen, expanded = {}, set(), 0
    with tarfile.open(fileobj=io.BytesIO(payload), mode="r|gz") as archive:
        for count, member in enumerate(archive, 1):
            parts = PurePosixPath(member.name).parts
            if (count > 100000 or not parts or parts[0] != root
                    or member.name.startswith("/") or ".." in parts
                    or not (member.isdir() or member.isfile()) or member.issparse()
                    or member.name in seen):
                raise CatalogError("official task source archive contains unsafe or excessive members")
            seen.add(member.name)
            expanded += member.size
            if member.size > 16 * 1024**2 or expanded > MAX_EXPANDED_BYTES:
                raise CatalogError("official task source archive exceeds expanded size limits")
            if (member.isfile() and len(parts) == 4 and parts[1] == "tasks"
                    and parts[3] in REQUIRED and (tasks is None or parts[2] in tasks)):
                result.setdefault(parts[2], {})[parts[3]] = archive.extractfile(member).read()
    if tasks is not None and set(result) != set(tasks):
        raise CatalogError("official task sources do not contain every selected instance")
    return result


def validate_task(row, files, dataset, yaml_loader=None):
    """The build definition must belong to the exact frozen dataset instance."""
    if not REQUIRED <= set(files):
        raise CatalogError("official task source definition is incomplete")
    if yaml_loader is None:
        from yaml import safe_load
        yaml_loader = safe_load
    metadata = yaml_loader(files["task.yaml"])
    for key in ("instance_id", "repo", "base_commit", "image", "version",
                "environment_setup_commit", "log_parser", "eval_type"):
        if row.get(key) != metadata.get(key):
            raise CatalogError(f"official source and frozen dataset disagree on {key}")
    if dataset["source"] not in metadata.get("datasets", []) or metadata.get("split") != dataset["split"]:
        raise CatalogError("official source does not belong to the selected dataset split")
    for key, name in (("patch", "gold.patch"), ("test_patch", "test.patch"),
                      ("eval_script", "eval.sh"), ("problem_statement", "problem_statement.md")):
        if row.get(key) != files[name].decode("utf-8"):
            raise CatalogError(f"official source and frozen dataset disagree on {key}")
    tests = json.loads(files["tests.json"])
    for key in ("FAIL_TO_PASS", "PASS_TO_PASS"):
        expected = row.get(key)
        if isinstance(expected, str):
            expected = json.loads(expected)
        if expected != tests.get(key):
            raise CatalogError(f"official source and frozen dataset disagree on {key}")
    dockerfile = files["Dockerfile"]
    # All Dockerfiles at this pinned original revision are self-contained.
    # Never send gold patches, evaluator scripts, task.yaml or host files as
    # build context. Future context-dependent sources need an explicit contract.
    if re.search(rb"(?im)^\s*(COPY|ADD)\b", dockerfile):
        raise CatalogError("original task Dockerfile unexpectedly requires additional build context")
    return dockerfile


def verify(path, catalog_path=None):
    sources = read_sources(path)
    if catalog_path is None:
        return {"status": "verified", "revision": SOURCE_REVISION, "archive_sha256": SOURCE_SHA256}
    catalog = json.loads(read_limited(catalog_path, MAX_CATALOG_BYTES))
    for entry in catalog["tasks"]:
        row = read_metadata(catalog_path, entry["id"], entry)
        validate_task(row, sources.get(entry["id"], {}), catalog["dataset"])
    return {"status": "verified", "revision": SOURCE_REVISION,
            "archive_sha256": SOURCE_SHA256, "task_count": len(catalog["tasks"])}


def remove_owned_image(client, tag, attempt, expected_labels=None):
    """Remove only this attempt's tag, never an upstream image or shared cache."""
    if tag != "asb-swebench:" + attempt or not re.fullmatch(r"asb-[0-9a-f]{32}", attempt):
        raise CatalogError("refused source-image cleanup without an ASB attempt identity")
    try:
        image = client.images.get(tag)
    except Exception as error:
        if getattr(error, "status_code", None) == 404:
            return "absent"
        raise
    labels = image.attrs.get("Config", {}).get("Labels", {}) or {}
    expected_labels = expected_labels or {"asb.owner":"asb","asb.attempt":attempt}
    if any(labels.get(key) != value for key,value in expected_labels.items()):
        raise CatalogError("refused source-image cleanup of a different owner")
    client.images.remove(tag, noprune=True)
    return "removed"


def buildx_unavailable(command):
    """Diagnose a failed build without assuming every build failure is Buildx."""
    try:
        result = subprocess.run(command + ["buildx", "version"], stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                timeout=60, check=False)
        return result.returncode != 0
    except (OSError, subprocess.TimeoutExpired):
        return True


def arm64_dockerfile(original):
    """Translate only the known pinned x86 task definition to a native ARM build.

    Keep package versions, but discard x86-specific Conda build identifiers.
    The resulting environment is an adaptation, not an upstream ARM image.
    """
    try:
        text = original.decode("utf-8")
    except UnicodeDecodeError as error:
        raise CatalogError("official task Dockerfile is not UTF-8") from error
    platform_x86 = "--platform=linux/amd64"
    conda_x86 = "Miniconda3-py311_23.11.0-2-Linux-x86_64.sh"
    if text.count(platform_x86) != 1 or text.count(conda_x86) != 1:
        raise CatalogError("official task definition has an unsupported ARM64 source layout")
    text = text.replace(platform_x86, "--platform=linux/arm64")
    text = text.replace(conda_x86, "Miniconda3-py311_23.11.0-2-Linux-aarch64.sh")
    text = text.replace("ld_impl_linux-64=", "ld_impl_linux-aarch64=")
    text = re.sub(r"(?m)^(  - [A-Za-z0-9_.-]+=[^=\s]+)=[^\s]+$", r"\1", text)
    if re.search(r"(?i)\b(?:amd64|x86_64|linux-64)\b", text):
        raise CatalogError("official task definition retains unsupported x86 dependencies")
    return text.encode("utf-8")


def build_image(client, row, dataset, artifacts, attempt, cancelled=None,
                archive=None, command=None, executor=run_harness, yaml_loader=None,
                expected_sha256=SOURCE_SHA256, journal=None, host_arch=None):
    memory_gib = source_build_memory_gib()
    build_timeout = source_build_timeout_seconds()
    if not re.fullmatch(r"asb-[0-9a-f]{32}", attempt):
        raise CatalogError("source builds require a fresh ASB attempt identity")
    archive = archive or Path(__file__).parent / SOURCE_ARCHIVE
    files = read_sources(archive, {row["instance_id"]}, expected_sha256)[row["instance_id"]]
    original_dockerfile = validate_task(row, files, dataset, yaml_loader)
    arch = host_arch or platform.machine()
    native_arm = arch in ("aarch64", "arm64")
    if native_arm:
        from asb_execution import require_native_docker_host
        if require_native_docker_host(client) != "arm64":
            raise CatalogError("ARM64 source image build requires a native ARM64 Docker host")
    dockerfile = arm64_dockerfile(original_dockerfile) if native_arm else original_dockerfile
    lock_digest = None
    lock = None
    lock_name = {ARM_LEGACY_TASK: ARM_LEGACY_LOCK, ARM_MODERN_TASK: ARM_MODERN_LOCK}.get(row["instance_id"]) if native_arm else None
    if lock_name:
        try:
            lock = (Path(__file__).parent / lock_name).read_bytes()
        except OSError as error:
            raise CatalogError("ARM64 scikit dependency lock is missing; reinstall the workload") from error
        dockerfile, lock_digest = locked_arm_dockerfile(row["instance_id"], dockerfile, lock)
    artifacts = Path(artifacts)
    context = artifacts / "source-build"
    context.mkdir(mode=0o700, parents=False, exist_ok=False)
    atomic_write(context / "Dockerfile", dockerfile)
    if lock is not None:
        atomic_write(context / lock_name, lock)
    if command is None:
        from asb_docker import docker_command
        command = docker_command()
    tag = "asb-swebench:" + attempt
    labels = journal.declare("images",tag) if journal is not None else {"asb.owner":"asb","asb.attempt":attempt}
    receipt = {"source": "https://github.com/SWE-bench/swe-bench-tasks",
               "revision": SOURCE_REVISION, "archive_sha256": expected_sha256,
               "dockerfile_sha256": hashlib.sha256(original_dockerfile).hexdigest(),
               "build_dockerfile_sha256": hashlib.sha256(dockerfile).hexdigest(),
               "architecture": "arm64" if native_arm else arch,
               "adaptation": ("native-arm64-scikit-transitive-lock-v1" if lock_digest else
                              "native-arm64-version-pinned-v1") if native_arm else None,
               "adaptation_lock_sha256": lock_digest,
               "task_id": row["instance_id"], "tag": tag, "status": "building",
               "limits": {"timeout_seconds": build_timeout, "memory_bytes": memory_gib * 1024**3, "cpus": 2}}
    path = artifacts / "source-image.json"
    atomic_write(path, encoded(receipt))
    arguments = command + ["buildx", "build", "--builder", "default", "--load",
        "--resource", f"memory={memory_gib}g", "--resource", "cpu-quota=200000", "--resource", "cpu-period=100000",
        "--tag", tag]
    if native_arm:
        arguments += ["--platform", "linux/arm64"]
    for key,value in labels.items():
        arguments += ["--label",key + "=" + value]
    arguments += [
        "--file", str(context / "Dockerfile"), "--progress=plain", str(context)]
    try:
        with (artifacts / "source-build.log").open("xb") as log:
            code = executor(arguments, artifacts, log, build_timeout, cancelled=cancelled)
        if code:
            if buildx_unavailable(command):
                raise CatalogError("Docker Buildx is unavailable or could not be verified for the Docker command used by ASB; install the Docker Buildx CLI plugin for that user, verify `docker buildx version`, and retry the benchmark. Inspect source-build.log for details.")
            raise CatalogError(f"official source image build failed (exit {code}); inspect source-build.log")
        if journal is not None:
            from asb_resources import owned_resource
            image = owned_resource(client,{"kind":"images","name":tag},labels)
            if image is None:
                raise CatalogError("completed source build did not publish its owned image")
        else:
            image = client.images.get(tag)
        receipt.update(status="built", image_id=image.id)
        atomic_write(path, encoded(receipt))
        return image, receipt
    except BaseException as error:
        receipt.update(status="failed", error_class=type(error).__name__,
                       failure_kind="timeout" if isinstance(error, subprocess.TimeoutExpired)
                       else "cancelled" if isinstance(error, InterruptedError) else "build_failed")
        try:
            receipt["cleanup"] = remove_owned_image(client, tag, attempt,expected_labels=labels)
        except Exception as error:
            receipt["cleanup"] = "error:" + type(error).__name__
        atomic_write(path, encoded(receipt))
        raise


def resolve_image(client, name, build, host_arch=None, offline=False, material_sha256=None):
    import docker.errors
    import requests.exceptions
    arch = host_arch or platform.machine()
    if offline:
        if arch in ("aarch64", "arm64"):
            prefix = "swebench/sweb.eval.x86_64."
            if not name.startswith(prefix):
                raise CatalogError("official SWE-bench image name does not support native ARM64 selection")
            name = "swebench/sweb.eval.arm64." + name[len(prefix):]
        image = None
        if material_sha256 is not None:
            tag = material_cache_tag(material_sha256)
            try:
                image = client.images.get(tag)
            except docker.errors.ImageNotFound:
                pass
        if image is not None:
            labels = image.attrs.get("Config", {}).get("Labels", {}) or {}
            owner = {key: labels.get(key) for key in ("asb.owner", "asb.attempt", "asb.recovery")}
            if (owner["asb.owner"] != "asb"
                    or not re.fullmatch(r"asb-[0-9a-f]{32}", owner["asb.attempt"] or "")
                    or not re.fullmatch(r"[0-9a-f]{32}", owner["asb.recovery"] or "")
                    or not re.fullmatch(r"sha256:[0-9a-f]{64}", getattr(image, "id", ""))
                    or image.attrs.get("Os") != "linux"
                    or image.attrs.get("Architecture") != ("arm64" if arch in ("arm64", "aarch64") else "amd64")):
                raise CatalogError("cached SWE-bench source image has invalid identity, ownership or architecture")
            return image, {"status":"built", "image_id":image.id, "cache_material_sha256":material_sha256,
                           "source_owner_labels":owner, "source":"verified-local-material-cache"}
        try:
            image = client.images.get(name)
        except docker.errors.ImageNotFound as error:
            raise CatalogError(f"SWE-bench replay image {name} is not installed locally; prepare it before replay") from error
        if arch in ("aarch64", "arm64") and (image.attrs.get("Architecture") != "arm64" or image.attrs.get("Os") != "linux"):
            raise CatalogError("upstream SWE-bench image is not a native Linux ARM64 image")
        return image, {"source": "prebuilt-local", "declared_image": name}
    if arch in ("aarch64", "arm64"):
        prefix = "swebench/sweb.eval.x86_64."
        if not name.startswith(prefix):
            raise CatalogError("official SWE-bench image name does not support native ARM64 selection")
        arm_name = "swebench/sweb.eval.arm64." + name[len(prefix):]
        try:
            try:
                image = client.images.get(arm_name)
                source = "prebuilt-local"
            except docker.errors.ImageNotFound:
                image = client.images.pull(arm_name)
                source = "prebuilt-pull"
        except docker.errors.ImageNotFound:
            image, receipt = build()
            if image.attrs.get("Architecture") != "arm64" or image.attrs.get("Os") != "linux":
                raise CatalogError("built SWE-bench image is not a native Linux ARM64 image")
            return image, dict(receipt, fallback_reason="upstream_arm64_image_unavailable")
        except (docker.errors.DockerException, requests.exceptions.RequestException) as error:
            raise CatalogError("cannot load the upstream ARM64 image; check registry access") from error
        if image.attrs.get("Architecture") != "arm64" or image.attrs.get("Os") != "linux":
            raise CatalogError("upstream SWE-bench image is not a native Linux ARM64 image")
        return image, {"source": source, "declared_image": arm_name, "architecture": "arm64"}
    try:
        return client.images.get(name), {"source": "prebuilt-local"}
    except docker.errors.ImageNotFound:
        try:
            return client.images.pull(name), {"source": "prebuilt-pull"}
        except (docker.errors.DockerException, requests.exceptions.RequestException) as error:
            image, receipt = build()
            receipt = dict(receipt, fallback_reason="prebuilt_pull_failed", pull_error_class=type(error).__name__)
            status = getattr(error, "status_code", None)
            if isinstance(status, int):
                receipt["pull_http_status"] = status
            return image, receipt


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--catalog", type=Path)
    arguments = parser.parse_args()
    print(json.dumps(verify(arguments.archive, arguments.catalog)))
