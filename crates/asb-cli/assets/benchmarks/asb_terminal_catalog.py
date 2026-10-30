"""Pinned original Terminal-Bench datasets, validated by their own official SDKs.

Task definitions/verifiers/solutions remain host-side dataset inputs. The
public catalog exposes only instance identity and instructions, never the
solution or verifier source. No LLM provider or Docker daemon is needed here.
"""
import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path, PurePosixPath
import re
import sys
import tarfile
import uuid

SPECS = {
    "terminal-bench-1.0": {"source": "harbor-framework/terminal-bench-1",
        "revision": "91e10457b5410f16c44364da1a34cb6de8c488a5", "split": "terminal-bench-core@0.1.1",
        "count": 80, "package": "terminal-bench", "version": "0.2.18",
        "source_sha256": "a0f0995c25b87009d546b6ff670cc2ff900a1275e02068a860035d1c07c2f588"},
    "terminal-bench-2.1": {"source": "harbor-framework/terminal-bench-2-1",
        "revision": "7131e4375048a0e408a8fb404b5f499d726b695b", "split": "tasks",
        "count": 89, "package": "harbor", "version": "0.24.0",
        "source_sha256": "778363800cf869d98b2b2bffdfba79439671851050788e8130b83fcbb79cf1d4"},
    "terminal-bench-2.0": {"source": "harbor-framework/terminal-bench-2",
        "revision": "2fd12b88aafdd04a52c298e3940bcb189f9766d6", "split": "repository-root-tasks",
        "count": 89, "package": "harbor", "version": "0.24.0",
        "source_sha256": "f8e210f04e196de4e7d800116c50ed60f78287e6942314f7231bdec4fa902479"},
}
MAX_TREE = 512 * 1024**2
MAX_FILE = 128 * 1024**2
MAX_CATALOG = 16 * 1024**2


class CatalogError(ValueError):
    pass


def digest(path):
    hasher = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1024 * 1024):
            hasher.update(block)
    return hasher.hexdigest()


def read_json(path, limit=MAX_CATALOG):
    if path.is_symlink() or not path.is_file() or path.stat().st_size > limit:
        raise CatalogError("dataset metadata is not a bounded regular file")
    return json.loads(path.read_bytes())


def atomic_json(path, value):
    temporary = path.with_name(path.name + "." + uuid.uuid4().hex)
    temporary.write_text(json.dumps(value, sort_keys=True) + "\n")
    temporary.chmod(0o600)
    temporary.replace(path)


def tree_manifest(root):
    if root.is_symlink() or not root.is_dir():
        raise CatalogError("dataset source must be an owned regular directory")
    entries, size = [], 0
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise CatalogError("dataset definitions must not contain symlinks")
        if path.is_dir():
            continue
        if not path.is_file() or path.stat().st_size > MAX_FILE:
            raise CatalogError("dataset contains an invalid or oversized source file")
        size += path.stat().st_size
        if size > MAX_TREE or len(entries) >= 100000:
            raise CatalogError("dataset source exceeds its bounded file/size limit")
        entries.append({"path": path.relative_to(root).as_posix(), "sha256": digest(path),
                        "bytes": path.stat().st_size, "executable": bool(path.stat().st_mode & 0o111)})
    return entries


def checked_members(source):
    roots, names, members, total = set(), set(), [], 0
    for member in source:
        path = PurePosixPath(member.name)
        total += member.size
        if (path.is_absolute() or ".." in path.parts or not path.parts
                or not (member.isfile() or member.isdir()) or member.size > MAX_FILE
                or member.name in names or total > MAX_TREE or len(members) >= 100000):
            raise CatalogError("dataset archive contains unsafe paths, links or special files")
        names.add(member.name)
        roots.add(path.parts[0])
        members.append(member)
    if len(roots) != 1:
        raise CatalogError("dataset archive must have exactly one repository root")
    return members, next(iter(roots))


def extract(archive, destination):
    with tarfile.open(archive, mode="r:*") as source:
        members, root = checked_members(source)
        source.extractall(destination, members=members, filter="data")
    return destination / root


def catalog(workload, source_root):
    spec = SPECS[workload]
    observed = importlib.metadata.version(spec["package"])
    if observed != spec["version"]:
        raise CatalogError("installed evaluator SDK does not match the workload recipe")
    tasks = source_root / "tasks" if (source_root / "tasks").is_dir() else source_root
    if workload == "terminal-bench-1.0":
        ids = read_json(Path(__file__).with_name("terminal_bench_1_tasks.json"))
        from terminal_bench.handlers.trial_handler import Task
        def parse(path):
            task = Task.from_yaml(path / "task.yaml")
            return task.instruction, str(task.difficulty or "")
    else:
        ids = sorted(path.name for path in tasks.iterdir() if path.is_dir() and (path / "task.toml").is_file())
        from harbor.models.task.task import Task
        def parse(path):
            task = Task(path)
            return task.instruction, str(task.config.metadata.get("difficulty", ""))
    if len(ids) != spec["count"] or len(set(ids)) != len(ids):
        raise CatalogError("official task population does not match the pinned benchmark")
    result = []
    for task_id in ids:
        if not re.fullmatch(r"[A-Za-z0-9_.-]{1,256}", task_id):
            raise CatalogError("invalid or duplicate official task identity")
        instruction, difficulty = parse(tasks / task_id)
        if not isinstance(instruction, str) or not instruction.strip() or len(instruction.encode()) > 64 * 1024:
            raise CatalogError("official task instruction is empty or exceeds 64 KiB")
        definition = f"source/tasks/{task_id}" if tasks != source_root else f"source/{task_id}"
        result.append({"id": task_id, "description": f"{task_id} ({difficulty})"[:1024],
                       "instruction": instruction, "definition": definition})
    return {"schema_version": 1, "workload": workload,
            "dataset": {key: spec[key] for key in ["source", "revision", "split"]},
            "sdk": {"package": spec["package"], "version": observed}, "tasks": result}


def verify(path, workload):
    root = path.parent
    result = read_json(path)
    spec = SPECS[workload]
    if (result.get("schema_version") != 1 or result.get("workload") != workload
            or result.get("dataset") != {key: spec[key] for key in ["source", "revision", "split"]}
            or result.get("sdk") != {"package": spec["package"], "version": spec["version"]}
            or result.get("source_sha256") != spec["source_sha256"]):
        raise CatalogError("installed dataset provenance does not match the selected benchmark")
    manifest = read_json(root / "source-manifest.json")
    encoded = json.dumps(manifest, sort_keys=True).encode()
    if result.get("source_sha256") != hashlib.sha256(encoded).hexdigest():
        raise CatalogError("dataset source manifest does not match its installation receipt")
    if manifest != tree_manifest(root / "source"):
        raise CatalogError("installed dataset source changed; reinstall the workload to repair it")
    expected = catalog(workload, root / "source")
    if any(result.get(key) != value for key, value in expected.items()):
        raise CatalogError("installed task catalog no longer matches its official definitions")
    return result


def install(workload, archive, destination):
    if destination.is_symlink():
        raise CatalogError("dataset destination must not be a symlink")
    if destination.exists():
        try:
            return verify(destination / "catalog.json", workload)
        except (ValueError, OSError, KeyError):
            pass
    stage = destination.with_name(destination.name + ".stage-" + uuid.uuid4().hex)
    stage.mkdir(mode=0o700, parents=True)
    try:
        extracted = extract(archive, stage / "unpacked")
        extracted.rename(stage / "source")
        (stage / "unpacked").rmdir()
        result = catalog(workload, stage / "source")
        manifest = tree_manifest(stage / "source")
        result["source_sha256"] = hashlib.sha256(json.dumps(manifest, sort_keys=True).encode()).hexdigest()
        if result["source_sha256"] != SPECS[workload]["source_sha256"]:
            raise CatalogError("downloaded task source does not match the pinned original benchmark tree")
        result["archive_sha256"] = digest(archive)
        atomic_json(stage / "source-manifest.json", manifest)
        atomic_json(stage / "catalog.json", result)
        if destination.exists():
            destination.rename(destination.with_name(destination.name + ".asb-backup-" + uuid.uuid4().hex))
        stage.rename(destination)
        return result
    except BaseException:
        # Retain extraction/validation evidence without replacing a valid install.
        raise


def runtime_identity(workload):
    spec = SPECS[workload]
    if importlib.metadata.version(spec["package"]) != spec["version"]:
        raise CatalogError("installed evaluator SDK does not match the workload recipe")
    if importlib.metadata.version("docker") != "7.1.0":
        raise CatalogError("installed Docker Python SDK does not match the workload runtime recipe")
    importlib.import_module("docker")
    return {"status":"ready", "sdk":spec["package"], "version":spec["version"], "docker_version":"7.1.0"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("operation", choices=["install", "verify", "archive", "runtime"])
    parser.add_argument("--workload", required=True, choices=list(SPECS))
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--archive", type=Path)
    args = parser.parse_args()
    try:
        if args.operation == "runtime":
            print(json.dumps(runtime_identity(args.workload)))
            return 0
        if args.operation == "archive":
            if args.archive is None:
                raise CatalogError("archive validation needs a source archive")
            with tarfile.open(args.archive, "r:*") as source:
                checked_members(source)
            print(json.dumps({"status":"ready", "operation":"archive"}))
            return 0
        if args.operation == "install":
            if args.archive is None:
                raise CatalogError("dataset installation needs an upstream source archive")
            result = install(args.workload, args.archive, args.directory)
        else:
            result = verify(args.directory / "catalog.json", args.workload)
        print(json.dumps({"status": "ready", "workload": args.workload, "tasks": len(result["tasks"]), "dataset": result["dataset"]}))
    except Exception as error:
        response = {"status": "error", "error_class": type(error).__name__}
        if isinstance(error, CatalogError):
            response["reason"] = str(error)
        else:
            # This response is captured only in ASB's bounded private step
            # diagnostics. Keep enough SDK context to make missing modules or
            # incompatible evaluator versions actionable without emitting a
            # traceback into the TUI protocol.
            response["reason"] = str(error)[:1024]
        print(json.dumps(response))
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
