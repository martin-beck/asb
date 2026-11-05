"""Pinned original SWE-bench Dockerfiles, never agent-visible gold/test inputs."""
import argparse
import hashlib
import io
import json
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


def build_image(client, row, dataset, artifacts, attempt, cancelled=None,
                archive=None, command=None, executor=run_harness, yaml_loader=None,
                expected_sha256=SOURCE_SHA256, journal=None):
    if not re.fullmatch(r"asb-[0-9a-f]{32}", attempt):
        raise CatalogError("source builds require a fresh ASB attempt identity")
    archive = archive or Path(__file__).parent / SOURCE_ARCHIVE
    files = read_sources(archive, {row["instance_id"]}, expected_sha256)[row["instance_id"]]
    dockerfile = validate_task(row, files, dataset, yaml_loader)
    artifacts = Path(artifacts)
    context = artifacts / "source-build"
    context.mkdir(mode=0o700, parents=False, exist_ok=False)
    atomic_write(context / "Dockerfile", dockerfile)
    if command is None:
        from asb_docker import docker_command
        command = docker_command()
    tag = "asb-swebench:" + attempt
    labels = journal.declare("images",tag) if journal is not None else {"asb.owner":"asb","asb.attempt":attempt}
    receipt = {"source": "https://github.com/SWE-bench/swe-bench-tasks",
               "revision": SOURCE_REVISION, "archive_sha256": expected_sha256,
               "dockerfile_sha256": hashlib.sha256(dockerfile).hexdigest(),
               "task_id": row["instance_id"], "tag": tag, "status": "building",
               "limits": {"timeout_seconds": SOURCE_BUILD_TIMEOUT_SECONDS, "memory_bytes": 8 * 1024**3, "cpus": 2}}
    path = artifacts / "source-image.json"
    atomic_write(path, encoded(receipt))
    arguments = command + ["buildx", "build", "--builder", "default", "--load",
        "--resource", "memory=8g", "--resource", "cpu-quota=200000", "--resource", "cpu-period=100000",
        "--tag", tag]
    for key,value in labels.items():
        arguments += ["--label",key + "=" + value]
    arguments += [
        "--file", str(context / "Dockerfile"), "--progress=plain", str(context)]
    try:
        with (artifacts / "source-build.log").open("xb") as log:
            code = executor(arguments, artifacts, log, SOURCE_BUILD_TIMEOUT_SECONDS, cancelled=cancelled)
        if code:
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


def resolve_image(client, name, build):
    import docker.errors
    import requests.exceptions
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
