"""Install bounded, pinned LiveCodeBench trial catalogs into an ASB project."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import pickle
import sys
import tarfile
import tempfile
import urllib.request
import zlib

MAX_TASKS = 10_000
MAX_PROMPT = 64 * 1024
# Official code-generation rows retain complete private test populations.  A
# single one can be materially larger than an ordinary prompt, but it must
# still fit within a finite project-local installation budget.
# The pinned release-v6 population includes a 92,836,584-byte private test
# corpus. Keep enough headroom for its canonical JSON representation without
# making a row unbounded.
# Never retain a whole catalog row as a serialized byte string. Large official
# private tests are encoded directly into a project-local temporary file, with
# this per-row on-disk limit. `streaming=True` also prevents datasets from
# materializing the complete population in memory before installation.
MAX_METADATA = 128 * 1024 * 1024
MAX_METADATA_TOTAL = 16 * 1024 * 1024 * 1024
SOURCE_REVISION = "28fef95ea8c9f7a547c8329f2cd3d32b92c1fa24"
SOURCE_SHA256 = "56c7c42aee979def86b69547ecd6d5a877a0e4940af8923bf4325ca0defecbaf"
SOURCE_URL = "https://codeload.github.com/LiveCodeBench/LiveCodeBench/tar.gz/" + SOURCE_REVISION


def atomic_write(path, payload):
    path = Path(path)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=".asb-livecodebench-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as target:
            json.dump(payload, target, ensure_ascii=False, sort_keys=True, default=str)
            target.write("\n")
            target.flush()
            os.fsync(target.fileno())
        os.chmod(temporary, 0o600)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def bounded(value, field):
    if not isinstance(value, str) or not value.strip() or len(value) > MAX_PROMPT:
        raise ValueError(f"LiveCodeBench row has invalid {field}")
    return value.strip()


def prompt(scenario, row):
    if scenario in {"code-generation", "self-repair"}:
        task = bounded(row.get("question_content"), "question_content")
        starter = row.get("starter_code") or ""
        prefix = "Solve this LiveCodeBench programming task."
        if scenario == "self-repair":
            prefix = "Solve and self-review this LiveCodeBench programming task."
        return f"{prefix}\n\n{task}\n\nStarter code:\n{starter}\n"
    if scenario == "test-output-prediction":
        task = bounded(row.get("question_content"), "question_content")
        starter = row.get("starter_code") or ""
        test = bounded(row.get("test"), "test")
        return f"Predict the output for this LiveCodeBench test.\n\n{task}\n\nStarter code:\n{starter}\n\nTest:\n{test}\n"
    if scenario == "code-execution":
        code = bounded(row.get("code"), "code")
        input_value = bounded(row.get("input"), "input")
        return f"Predict the result of this LiveCodeBench code execution.\n\nCode:\n{code}\n\nInput:\n{input_value}\n"
    raise ValueError("unsupported LiveCodeBench scenario")


def task_id(scenario, release, row):
    if scenario in {"code-generation", "self-repair"}:
        value = str(row.get("question_id", ""))
        value = f"{release}.{value}"
    elif scenario == "test-output-prediction":
        value = f"{row.get('question_id', '')}.{row.get('test_id', '')}"
    else:
        value = str(row.get("id", ""))
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,255}", value):
        raise ValueError("LiveCodeBench row has invalid task identity")
    return value


def load(args):
    from datasets import load_dataset
    kwargs = {
        "split": "test",
        "revision": args.revision,
        "trust_remote_code": True,
        "streaming": True,
    }
    if args.release:
        kwargs["version_tag"] = args.release
    return load_dataset(args.dataset, **kwargs)


def metadata_to_disk(directory, row):
    """Encode one official row directly to disk and return its content digest."""
    if not isinstance(row, dict):
        raise ValueError("LiveCodeBench returned metadata that is not an object")
    directory = Path(directory)
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=".asb-livecodebench-metadata-", dir=directory)
    digest, size = hashlib.sha256(), 0
    try:
        with os.fdopen(descriptor, "wb") as target:
            encoder = json.JSONEncoder(ensure_ascii=False, sort_keys=True, default=str)
            # Stream the row directly: copying it with ``dict(row)`` can double
            # peak memory precisely when an upstream entry is unexpectedly large.
            for chunk in encoder.iterencode(row):
                encoded = chunk.encode("utf-8")
                size += len(encoded)
                if size > MAX_METADATA:
                    raise ValueError("LiveCodeBench task metadata exceeds 128 MiB")
                digest.update(encoded)
                target.write(encoded)
            target.write(b"\n")
            digest.update(b"\n")
            target.flush()
            os.fsync(target.fileno())
        os.chmod(temporary, 0o600)
        name = digest.hexdigest()
        destination = directory / f"{name}.json"
        if destination.exists():
            existing = hashlib.sha256()
            with open(destination, "rb") as source:
                while chunk := source.read(1024 * 1024):
                    existing.update(chunk)
            if existing.hexdigest() != name:
                raise ValueError("pinned LiveCodeBench task metadata changed")
            os.unlink(temporary)
        else:
            os.replace(temporary, destination)
        return name
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def install_source(cache):
    """Fetch the official runner once into the ASB-local cache, fail closed."""
    root = Path(cache) / "livecodebench-source" / SOURCE_REVISION
    marker = root / "lcb_runner" / "evaluation" / "compute_code_generation_metrics.py"
    if marker.is_file():
        return root
    archive = Path(cache) / "livecodebench-source" / f"{SOURCE_REVISION}.tar.gz"
    archive.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    if not archive.is_file() or hashlib.sha256(archive.read_bytes()).hexdigest() != SOURCE_SHA256:
        temporary = archive.with_suffix(".part")
        with urllib.request.urlopen(SOURCE_URL, timeout=60) as source, open(temporary, "wb") as target:
            while block := source.read(1024 * 1024):
                target.write(block)
        if hashlib.sha256(temporary.read_bytes()).hexdigest() != SOURCE_SHA256:
            temporary.unlink(missing_ok=True)
            raise ValueError("LiveCodeBench source archive digest differs from the pinned revision")
        os.replace(temporary, archive)
    temporary = root.with_name(root.name + ".part")
    if temporary.exists():
        raise ValueError("incomplete LiveCodeBench source extraction is present; preserve it and retry")
    temporary.mkdir(mode=0o700)
    try:
        with tarfile.open(archive, "r:gz") as incoming:
            members = incoming.getmembers()
            if not members or any(member.name.startswith("/") or ".." in Path(member.name).parts for member in members):
                raise ValueError("LiveCodeBench source archive has unsafe paths")
            incoming.extractall(temporary, members, filter="data")
        unpacked = temporary / f"LiveCodeBench-{SOURCE_REVISION}"
        if not (unpacked / "lcb_runner" / "evaluation" / "compute_code_generation_metrics.py").is_file():
            raise ValueError("LiveCodeBench source archive is incomplete")
        os.replace(unpacked, root)
    finally:
        if temporary.exists():
            import shutil
            shutil.rmtree(temporary)
    return root


def catalog(args):
    os.environ["HF_HOME"] = str(Path(args.cache).resolve())
    install_source(args.cache)
    dataset = load(args)
    tasks, seen, metadata_total = [], set(), 0
    for row in dataset:
        identifier = task_id(args.scenario, args.release, row)
        if identifier in seen:
            raise ValueError("LiveCodeBench dataset has duplicate task identities")
        seen.add(identifier)
        text = prompt(args.scenario, row)
        if len(text.encode()) > MAX_PROMPT:
            raise ValueError("LiveCodeBench task prompt exceeds 64 KiB")
        description = bounded(row.get("question_title") or row.get("contest_id") or identifier, "description")[:1024]
        metadata_root = Path(args.output) / "evaluation-data"
        digest = metadata_to_disk(metadata_root, row)
        metadata = f"evaluation-data/{digest}.json"
        metadata_total += (metadata_root / f"{digest}.json").stat().st_size
        if metadata_total > MAX_METADATA_TOTAL:
            raise ValueError("LiveCodeBench task metadata exceeds the 16 GiB catalog budget")
        tasks.append({"id": identifier, "description": description, "prompt": text,
                      "metadata": metadata, "metadata_sha256": digest})
        if len(tasks) > MAX_TASKS:
            raise ValueError("LiveCodeBench task population exceeds safety limit")
    if not tasks:
        raise ValueError("LiveCodeBench dataset is empty")
    tasks.sort(key=lambda item: item["id"])
    split = args.release or args.scenario
    atomic_write(Path(args.output) / "catalog.json", {
        "schema_version": 1,
        "workload": args.workload,
        "scenario": args.scenario,
        "dataset": {"source": args.dataset, "revision": args.revision, "split": split},
        "tasks": tasks,
    })


def extracted_code(response):
    """Accept the standard fenced answer form without model-family guessing."""
    fenced = re.search(r"```(?:python)?\s*\n(.*?)```", response, re.DOTALL | re.IGNORECASE)
    return fenced.group(1).strip() if fenced else response.strip()


def code_generation_tests(row):
    public = json.loads(row["public_test_cases"])
    try:
        private = json.loads(row["private_test_cases"])
    except (TypeError, json.JSONDecodeError):
        private = json.loads(pickle.loads(zlib.decompress(base64.b64decode(row["private_test_cases"]))))
    return public + private


def evaluate(args):
    catalog_path = Path(args.catalog)
    catalog_data = json.loads(catalog_path.read_text(encoding="utf-8"))
    tasks = [item for item in catalog_data.get("tasks", []) if item.get("id") == args.task]
    if len(tasks) != 1:
        raise ValueError("selected LiveCodeBench task is absent from the installed catalog")
    task = tasks[0]
    metadata = Path(task["metadata"])
    if metadata.is_absolute() or ".." in metadata.parts:
        raise ValueError("LiveCodeBench task metadata path is invalid")
    payload = (catalog_path.parent / metadata).read_bytes()
    if hashlib.sha256(payload).hexdigest() != task.get("metadata_sha256"):
        raise ValueError("LiveCodeBench task metadata checksum changed")
    row = json.loads(payload)
    response = Path(args.response).read_text(encoding="utf-8")
    source = Path(args.source)
    if not (source / "lcb_runner").is_dir():
        raise ValueError("pinned LiveCodeBench evaluator source is unavailable")
    sys.path.insert(0, str(source))
    scenario = catalog_data.get("scenario")
    if scenario in {"code-generation", "self-repair"}:
        from lcb_runner.evaluation.compute_code_generation_metrics import codegen_metrics
        tests = code_generation_tests(row)
        sample = {"input_output": json.dumps({
            "inputs": [item["input"] for item in tests],
            "outputs": [item["output"] for item in tests],
            "fn_name": json.loads(row["metadata"]).get("func_name"),
        })}
        result = codegen_metrics([sample], [[extracted_code(response)]], k_list=[1], num_process_evaluate=1, timeout=6, debug=False)
        passed = bool(result[1][0][0]) and all(result[1][0][0])
    elif scenario == "test-output-prediction":
        from lcb_runner.evaluation.compute_test_output_prediction_metrics import test_output_metrics
        test = json.loads(row["test"])[0]
        result = test_output_metrics([{"output": test["output"]}], [[response]])
        passed = bool(result[1][0][0][0])
    elif scenario == "code-execution":
        from lcb_runner.evaluation.compute_code_execution_metrics import code_execution_metrics
        result = code_execution_metrics([row], [[response]])
        passed = bool(result[1][0][0][0])
    else:
        raise ValueError("installed LiveCodeBench catalog has an unknown scenario")
    atomic_write(Path(args.result), {"schema_version": 1, "passed": passed})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("operation", choices=["catalog", "evaluate"])
    parser.add_argument("--workload")
    parser.add_argument("--scenario", choices=["code-generation", "self-repair", "test-output-prediction", "code-execution"])
    parser.add_argument("--dataset")
    parser.add_argument("--revision")
    parser.add_argument("--release")
    parser.add_argument("--output")
    parser.add_argument("--cache")
    parser.add_argument("--catalog")
    parser.add_argument("--task")
    parser.add_argument("--response")
    parser.add_argument("--source")
    parser.add_argument("--result")
    args = parser.parse_args()
    try:
        if args.operation == "catalog":
            if not all([args.workload, args.scenario, args.dataset, args.revision, args.output, args.cache]):
                raise ValueError("catalog requires workload, scenario, dataset, revision, output and cache")
            catalog(args)
        else:
            if not all([args.catalog, args.task, args.response, args.source, args.result]):
                raise ValueError("evaluate requires catalog, task, response, source and result")
            evaluate(args)
    except Exception as error:
        detail = str(error).replace("\n", " ").replace("\r", " ")[:512]
        print(f"LiveCodeBench catalog installation failed: {detail or type(error).__name__}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
