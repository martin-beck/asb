"""ASB's pinned SWE-bench materializer; no agent/provider-specific behavior.

Evaluator-only rows stay outside agent workspaces. The public catalog contains
task identity and short descriptions, never the gold patch or test program.
The official SDK is imported only for operations that need it, so protocol
tests can run entirely offline using Python's standard library.
"""

import argparse
import contextlib
import fcntl
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid


class CatalogError(ValueError):
    pass


MAX_METADATA_BYTES = 4 * 1024 * 1024
MAX_CATALOG_BYTES = 16 * 1024 * 1024
MAX_PATCH_BYTES = 64 * 1024 * 1024
HARNESS_REVISION = "02e7a74ffd0b707aab73d203fe87bdc7c76afc8e"


def install_session_cancel_handlers():
    """Begin cancellation once; later terminal signals must not interrupt cleanup."""
    def interrupted(_signal, _frame):
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        raise InterruptedError("ASB task session was cancelled")
    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGTERM, interrupted)


def sdk_identity(distribution=None):
    """Record actual installed code identity rather than claiming the recipe pin."""
    distribution = distribution or importlib.metadata.distribution("swebench")
    try:
        direct = json.loads(distribution.read_text("direct_url.json") or "{}")
        revision = direct.get("vcs_info", {}).get("commit_id")
    except (TypeError, ValueError):
        revision = None
    if revision != HARNESS_REVISION:
        raise CatalogError("installed SWE-bench SDK does not match its pinned source revision; run `asb tool install swebench`")
    return {"revision": revision, "version": distribution.version}


def read_limited(path, limit):
    with Path(path).open("rb") as source:
        content = source.read(limit + 1)
    if len(content) > limit:
        raise CatalogError("installed task metadata exceeds its bounded read limit")
    return content


def encoded(value):
    return (json.dumps(value, ensure_ascii=False, sort_keys=True) + "\n").encode()


def atomic_write(path, content, mode=0o600):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=".asb-write-", dir=path.parent)
    try:
        os.fchmod(descriptor, mode)
        with os.fdopen(descriptor, "wb") as output:
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def build_catalog(rows, workload, source, revision, split, destination, validate=None):
    """Validate all rows before publishing a catalog; preserve previous revisions."""
    if not re.fullmatch(r"[0-9a-fA-F]{40,64}", revision):
        raise CatalogError("dataset revision must be a resolved commit, not a floating name")
    destination = Path(destination)
    private = destination / "evaluation-data" / revision
    if private.parent.is_symlink() or private.is_symlink():
        raise CatalogError("private evaluator data directory must not be a symlink")
    private.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(private.parent, 0o700)
    os.chmod(private, 0o700)
    entries, seen = [], set()
    for row in rows:
        task_id = row.get("instance_id")
        if not isinstance(task_id, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,255}", task_id):
            raise CatalogError("dataset row has no valid bounded instance_id")
        if task_id in seen:
            raise CatalogError(f"duplicate dataset instance: {task_id}")
        seen.add(task_id)
        for field in ("repo", "problem_statement", "base_commit"):
            if not isinstance(row.get(field), str) or not row[field].strip():
                raise CatalogError(f"instance {task_id} has no nonempty {field}")
        if not re.fullmatch(r"[0-9a-fA-F]{40}", row["base_commit"]):
            raise CatalogError(f"instance {task_id} has an invalid base_commit")
        agent_instruction(row)
        if validate is not None:
            try:
                validate(row)
            except (KeyError, TypeError, ValueError) as error:
                detail = type(error).__name__
                if isinstance(error, KeyError) and error.args and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,63}", str(error.args[0])):
                    detail += ": missing field " + str(error.args[0])
                raise CatalogError(f"instance {task_id} is incompatible with the official evaluator ({detail})") from error
        payload = encoded(row)
        if len(payload) > MAX_METADATA_BYTES:
            raise CatalogError(f"instance {task_id} exceeds the 4 MiB metadata limit")
        filename = hashlib.sha256(task_id.encode()).hexdigest() + ".json"
        metadata = private / filename
        if metadata.is_symlink():
            raise CatalogError(f"pinned metadata for {task_id} must not be a symlink")
        if metadata.exists() and read_limited(metadata, MAX_METADATA_BYTES) != payload:
            raise CatalogError(f"pinned metadata changed for instance {task_id}; retained old evidence")
        if not metadata.exists():
            atomic_write(metadata, payload)
        description = row["problem_statement"].strip().splitlines()[0][:180]
        entries.append({
            "id": task_id,
            "description": description,
            "repo": row["repo"],
            "base_commit": row["base_commit"],
            "metadata": str(metadata.relative_to(destination)),
            "metadata_sha256": hashlib.sha256(payload).hexdigest(),
        })
        if len(entries) > 10000:
            raise CatalogError("task catalog exceeds the 10000-instance safety limit")
    if not entries:
        raise CatalogError("selected dataset split contains no tasks")
    entries.sort(key=lambda entry: entry["id"])
    catalog = {
        "schema_version": 1,
        "workload": workload,
        "dataset": {"source": source, "revision": revision, "split": split},
        "tasks": entries,
    }
    content = encoded(catalog)
    immutable = destination / f"catalog-{revision}-{split}.json"
    if len(content) > MAX_CATALOG_BYTES:
        raise CatalogError("task catalog exceeds the 16 MiB limit")
    if immutable.exists() and read_limited(immutable, MAX_CATALOG_BYTES) != content:
        raise CatalogError("pinned catalog changed; retained the previous catalog")
    if not immutable.exists():
        atomic_write(immutable, content)
    atomic_write(destination / "catalog.json", content)
    return catalog


def read_task(catalog_path, task_id):
    """Bind private evaluator metadata to one installed catalog instance."""
    catalog_path = Path(catalog_path)
    catalog = json.loads(read_limited(catalog_path, MAX_CATALOG_BYTES))
    if catalog.get("schema_version") != 1:
        raise CatalogError("unsupported task catalog schema")
    selected = [entry for entry in catalog["tasks"] if entry["id"] == task_id]
    if len(selected) != 1:
        raise CatalogError("task must occur exactly once in the installed catalog")
    entry = selected[0]
    return catalog, read_metadata(catalog_path, task_id, entry)


def read_metadata(catalog_path, task_id, entry):
    catalog_path = Path(catalog_path)
    relative = Path(entry["metadata"])
    root = catalog_path.parent.resolve()
    path = root / relative
    if relative.is_absolute() or ".." in relative.parts or path.is_symlink() or not path.resolve().is_relative_to(root):
        raise CatalogError("task metadata must stay inside its installed catalog")
    payload = read_limited(path, MAX_METADATA_BYTES)
    if hashlib.sha256(payload).hexdigest() != entry["metadata_sha256"]:
        raise CatalogError("task metadata checksum changed; reinstall the pinned dataset")
    row = json.loads(payload)
    if (row["instance_id"], row["repo"], row["base_commit"]) != (task_id, entry["repo"], entry["base_commit"]):
        raise CatalogError("task metadata does not match its catalog identity")
    return row


def verify_catalog(catalog_path):
    catalog = json.loads(read_limited(catalog_path, MAX_CATALOG_BYTES))
    tasks = catalog.get("tasks")
    if not isinstance(tasks, list) or not tasks or len(tasks) > 10000:
        raise CatalogError("installed catalog has no valid task population")
    ids = [task["id"] for task in tasks]
    if len(ids) != len(set(ids)):
        raise CatalogError("installed catalog contains duplicate tasks")
    for entry in tasks:
        read_metadata(catalog_path, entry["id"], entry)
    return {"status": "verified", "task_count": len(ids)}


def agent_instruction(row):
    """Explicit allowlist: never send the gold patch or hidden tests to agents."""
    prompt = (
        f"Resolve this issue in repository {row['repo']} at base commit {row['base_commit']}.\n"
        "Make the necessary changes in the task repository and run relevant tests.\n\n"
        + row["problem_statement"]
    )
    if len(prompt.encode()) > 64 * 1024:
        raise CatalogError("task instruction exceeds the 64 KiB agent input limit")
    return prompt


def run_harness(arguments, directory, log, timeout, cancelled=None, on_cancel=None):
    """One isolated evaluator process; timeout terminates its own process group."""
    environment = os.environ.copy()
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    environment["LITELLM_LOCAL_MODEL_COST_MAP"] = "True"
    with subprocess.Popen(arguments, cwd=directory, env=environment, stdin=subprocess.DEVNULL,
                          stdout=log, stderr=log, start_new_session=True) as process:
        try:
            deadline = time.monotonic() + timeout
            while True:
                if cancelled is not None and cancelled.is_set():
                    raise InterruptedError("ASB evaluator controller disconnected")
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(arguments, timeout)
                try:
                    return process.wait(timeout=min(0.1, remaining))
                except subprocess.TimeoutExpired:
                    pass
        except (subprocess.TimeoutExpired, InterruptedError):
            # Give the controller's finally block time to remove only this
            # attempt's container before terminating a non-responsive helper.
            try:
                if on_cancel is not None:
                    on_cancel()
            finally:
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                except ProcessLookupError:
                    process.wait()
            raise


def evaluate_submission(catalog_path, task_id, patch_path, artifacts, timeout, executor=run_harness, image_identity=None):
    """Grade a frozen submission using upstream, never agent exit or source heuristics.

    Every invocation gets a new run ID and a new directory. The official SDK's
    cached result for an earlier patch can therefore never satisfy this attempt.
    The executor seam is only for provider-free protocol tests.
    """
    catalog, row = read_task(catalog_path, task_id)
    evaluation_row = dict(row)
    if image_identity is not None:
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", image_identity.get("image_id", "")):
            raise CatalogError("task environment has no immutable evaluator image identity")
        # Evaluate against the exact image used for the attempt even when an
        # upstream mutable tag has changed since preparation.
        evaluation_row["image"] = image_identity["image_id"]
    if not isinstance(timeout, int) or not 1 <= timeout <= 86400:
        raise CatalogError("evaluation timeout must be between 1 and 86400 seconds")
    patch = read_limited(patch_path, MAX_PATCH_BYTES)
    try:
        patch_text = patch.decode("utf-8")
    except UnicodeDecodeError as error:
        raise CatalogError("submission patch must be UTF-8") from error
    artifacts = Path(artifacts).absolute()
    artifacts.mkdir(mode=0o700, parents=True, exist_ok=False)
    run_id = "asb-" + uuid.uuid4().hex
    result = {
        "schema_version": 1, "workload": catalog["workload"], "task_id": task_id,
        "dataset": catalog["dataset"], "run_id": run_id,
        "patch_sha256": hashlib.sha256(patch).hexdigest(),
        "harness_revision": HARNESS_REVISION,
        "resolved": False, "outcome": "evaluation_error", "reason": None,
        "source_image": row.get("image"),
    }
    atomic_write(artifacts / "submission.jsonl", encoded({
        "instance_id": task_id, "model_name_or_path": "asb",
        "model_patch": patch_text,
    }))
    # The evaluator reads this exact frozen row, not a floating upstream dataset.
    atomic_write(artifacts / "dataset.json", encoded([evaluation_row]))
    if not patch_text.strip():
        result.update(outcome="unresolved", reason="empty_submission")
    else:
        arguments = [sys.executable, str(Path(__file__).absolute()), "harness",
                     "--dataset_name", str(artifacts / "dataset.json"), "--split", catalog["dataset"]["split"],
                     "--instance_ids", task_id, "--predictions_path", str(artifacts / "submission.jsonl"),
                     "--max_workers", "1", "--open_file_limit", "4096",
                     "--timeout", str(timeout), "--run_id", run_id]
        with (artifacts / "harness.log").open("xb") as log:
            try:
                status = executor(arguments, artifacts, log, timeout + 120)
                result["harness_exit_code"] = status
                report_dir = artifacts / "logs/evaluation" / run_id
                summary_path = report_dir / "results.json"
                instance_path = report_dir / "asb" / task_id / "report.json"
                if not summary_path.is_file() or not instance_path.is_file():
                    result["reason"] = "official_report_missing"
                else:
                    summary = json.loads(read_limited(summary_path, MAX_METADATA_BYTES))
                    report = json.loads(read_limited(instance_path, MAX_METADATA_BYTES))
                    if (summary.get("total_instances") != 1
                            or summary.get("submitted_ids") != [task_id]
                            or summary.get("completed_ids") != [task_id]
                            or set(report) != {task_id}
                            or not isinstance(report[task_id].get("resolved"), bool)
                            or summary.get("resolved_ids") != ([task_id] if report[task_id]["resolved"] else [])):
                        raise CatalogError("official report does not match the submitted task")
                    resolved = report[task_id]["resolved"]
                    if resolved and report[task_id].get("patch_successfully_applied") is not True:
                        raise CatalogError("official report claims resolution without an applied patch")
                    image = json.loads(read_limited(artifacts / "image-identity.json", MAX_METADATA_BYTES))
                    sdk = json.loads(read_limited(artifacts / "sdk-identity.json", MAX_METADATA_BYTES))
                    if (image.get("declared_image") != evaluation_row.get("image")
                            or not re.fullmatch(r"sha256:[0-9a-f]{64}", image.get("image_id", ""))
                            or (image_identity is not None and image.get("image_id") != image_identity["image_id"])
                            or sdk.get("revision") != HARNESS_REVISION):
                        raise CatalogError("official evaluator image or SDK identity does not match the attempt")
                    result.update(resolved=resolved, outcome="resolved" if resolved else "unresolved",
                                  reason=None if resolved else "official_tests_not_resolved",
                                  image=image, sdk=sdk,
                                  official_summary=str(summary_path.relative_to(artifacts)),
                                  official_report=str(instance_path.relative_to(artifacts)))
            except subprocess.TimeoutExpired:
                result.update(outcome="evaluation_timeout", reason="official_harness_deadline_exceeded")
            except InterruptedError:
                result.update(outcome="evaluation_cancelled", reason="controller_disconnected")
            except (CatalogError, KeyError, TypeError, ValueError, OSError):
                result.update(outcome="evaluation_error", reason="invalid_official_report")
    atomic_write(artifacts / "evaluation.json", encoded(result))
    return result


def install_catalog(arguments):
    sdk_identity()
    from huggingface_hub import HfApi
    from datasets import load_dataset
    from swebench.harness.utils import make_test_spec

    # Public benchmark data needs no token, even if the enclosing user has one.
    info = HfApi(token=False).dataset_info(
        repo_id=arguments.dataset, revision=arguments.revision, timeout=30
    )
    rows = load_dataset(
        arguments.dataset,
        revision=info.sha,
        split=arguments.split,
        token=False,
        cache_dir=str(arguments.cache),
    )
    catalog = build_catalog(
        rows, arguments.workload, arguments.dataset, info.sha, arguments.split,
        arguments.output, validate=make_test_spec,
    )
    return {
        "status": "installed", "dataset": catalog["dataset"],
        "task_count": len(catalog["tasks"]), "manifest": "catalog.json",
    }


def remove_evaluation_container(client, task_id, run_id, expected_labels=None):
    if (not re.fullmatch(r"asb-[0-9a-f]{32}", run_id)
            or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,255}", task_id)):
        raise CatalogError("refused cleanup without an ASB evaluator identity")
    name = f"sweb.eval.{task_id.lower()}.{run_id}"
    cleanup = {"container": name, "status": "absent"}
    try:
        container = client.containers.get(name)
        labels = container.attrs.get("Config", {}).get("Labels", {}) or {}
        required = {"asb.owner":"asb", "asb.attempt":run_id, **(expected_labels or {})}
        if container.name.lstrip("/") != name or any(labels.get(key) != value for key, value in required.items()):
            raise CatalogError("refused cleanup of a different evaluator container")
        container.remove(force=True)
        cleanup["status"] = "removed"
    except Exception as error:
        if getattr(error, "status_code", None) != 404:
            cleanup.update(status="error", error_class=type(error).__name__)
    return cleanup


def retained_evaluation_prefix(retained):
    from asb_retained import require
    selected = retained.selected_evaluation()
    if selected is not None:
        return selected
    directories = []
    for index, path in enumerate(retained.root.iterdir()):
        require(index < 256, "retained attempt has excessive top-level artifacts")
        if path.name.startswith("evaluation-"):
            directories.append(path.name)
    require(len(directories) == 1, "retained SWE-bench evaluator population is ambiguous")
    return directories[0] + "/"


def reconstruct_evaluation(retained, source, catalog, row, cancelled, owner_lock=None):
    """Normalize already-completed official reports, never run tests again.

    Keep every original byte (including a failed/missing wrapper) unchanged.
    The derived evaluator binds those source bytes and is selected only after
    its normal verifier accepts the complete frozen-input/report population.
    """
    from asb_retained import RetainedOutcome, require
    from asb_resources import open_directory, private_file
    prefix = source+"/"
    descriptor, lock = open_directory(retained.child(source)),None
    try:
        if owner_lock is None:
            lock = private_file(descriptor,"resources.lock",os.O_RDWR)
            try:fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
            except BlockingIOError:require(False,"official evaluator still has an active owner; no tests or reconstruction were started")
        else:
            expected = os.stat("resources.lock",dir_fd=descriptor,follow_symlinks=False)
            held = os.fstat(owner_lock)
            require((held.st_dev,held.st_ino) == (expected.st_dev,expected.st_ino),
                    "reconstruction does not hold the original evaluator exclusion lock")
        journal = retained.object(prefix+"resources.json")
        run_id = journal.get("attempt", "")
        require(journal.get("schema_version") == 1 and re.fullmatch(r"asb-[a-f0-9]{32}",str(run_id))
                and journal.get("daemon") == retained.object("resources.json").get("daemon")
                and re.fullmatch(r"[a-f0-9]{32}",str(journal.get("token",""))),
                "raw official evaluator ownership does not match this attempt")
        name = f"sweb.eval.{retained.identity['task_id'].lower()}.{run_id}"
        require(journal.get("resources") == [{"kind":"containers","name":name}],
                "raw evaluator resource declaration does not match its task")
        cleanup = retained.object(prefix+"cleanup.json")
        require(cleanup.get("container") == name and cleanup.get("status") in ["removed","absent"],
                "raw official evaluator cleanup is not confirmed; no evaluator was rerun")
        old = retained.object(prefix+"evaluation.json") if retained.child(prefix+"evaluation.json").exists() else None
        if old is not None:
            require(all(old.get(key) == retained.identity[key] for key in ["workload","task_id","dataset"])
                    and old.get("run_id") == run_id and old.get("harness_revision") == HARNESS_REVISION
                    and old.get("patch_sha256") == retained.submission["patch_sha256"],
                    "lost wrapper metadata differs from its frozen attempt")
        relative_report = (old or {}).get("official_report") or f"logs/evaluation/{run_id}/asb/{retained.identity['task_id']}/report.json"
        relative_summary = (old or {}).get("official_summary") or f"logs/evaluation/{run_id}/results.json"
        probe = RetainedOutcome({"reference":retained.reference},inspect=True)
        report = probe.object(prefix+relative_report)
        require(set(report) == {retained.identity["task_id"]}
                and type(report[retained.identity["task_id"]].get("resolved")) is bool,
                "raw official report has no completed task outcome")
        resolved = report[retained.identity["task_id"]]["resolved"]
        result = {"schema_version":1,"workload":catalog["workload"],"task_id":retained.identity["task_id"],
            "dataset":catalog["dataset"],"run_id":run_id,"patch_sha256":retained.submission["patch_sha256"],
            "harness_revision":HARNESS_REVISION,"resolved":resolved,"outcome":"resolved" if resolved else "unresolved",
            "reason":None if resolved else "official_tests_not_resolved", "source_image":row.get("image"),
            "image":probe.object(prefix+"image-identity.json"),"sdk":probe.object(prefix+"sdk-identity.json"),
            "official_report":relative_report,"official_summary":relative_summary}
        # Full frozen evaluator row equality binds the selected pinned test data,
        # not merely an instance/base shared by several original populations.
        expected_row = dict(row,image=retained.task["image"]["image_id"])
        require(json.loads(probe.bytes(prefix+"dataset.json")) == [expected_row],
                "raw evaluator input differs from the installed pinned task")
        probe.select_grade(result)
        verify_retained(probe,prefix=prefix,result=result)
        files = ["dataset.json","submission.jsonl","sdk-identity.json","image-identity.json",relative_report,relative_summary,
                 "resources.json","cleanup.json"]
        require(len(set(files)) == len(files),"raw evaluator artifacts overlap")
        inputs = {}
        bodies = {}
        for relative in files:
            body = probe.bytes(prefix+relative)
            bodies[relative] = body
            inputs[prefix+relative] = {"sha256":hashlib.sha256(body).hexdigest(),"bytes":len(body)}
        wrapper = prefix+"evaluation.json"
        if old is None:inputs[wrapper] = None
        else:
            body = retained.bytes(wrapper)
            inputs[wrapper] = {"sha256":hashlib.sha256(body).hexdigest(),"bytes":len(body)}
        require(not cancelled.is_set(),"raw outcome reconstruction cancelled before publication")
        directory = "recovered-evaluation-"+uuid.uuid4().hex
        staging = ".outcome-reconstruction-"+uuid.uuid4().hex
        destination = retained.child(staging)
        destination.mkdir(mode=0o700)
        for relative,body in bodies.items():
            # RetainedOutcome validates the entire original relative path.
            path = retained.child(staging+"/"+relative)
            path.parent.mkdir(mode=0o700,parents=True,exist_ok=True)
            with path.open("xb") as output:
                os.fchmod(output.fileno(),0o600)
                output.write(body)
                output.flush()
                os.fsync(output.fileno())
        result["reconstruction"] = {"schema_version":1,"scope":"existing-official-reports-not-test-retry",
                                    "source":source,"inputs":inputs}
        atomic_write(destination/"evaluation.json",encoded(result))
        preview = RetainedOutcome({"reference":retained.reference},inspect=True)
        preview.select_grade(result)
        verify_retained(preview,prefix=staging+"/")
        require(not cancelled.is_set(),"raw outcome reconstruction cancelled before publication")
        os.rename(destination,retained.child(directory))
        retained.select_grade(result)
        verify_retained(retained,prefix=directory+"/")
        require(not cancelled.is_set(),"raw outcome reconstruction cancelled before selection")
        retained.publish_selection(directory)
    finally:
        if lock is not None:os.close(lock)
        os.close(descriptor)


def grade_retained(retained, catalog_path, cancelled, _excluded=None):
    """Grade the frozen patch, not a new client attempt or restarted workspace."""
    from asb_retained import RetainedOutcome, require
    require(retained.task.get("sdk") == sdk_identity(), "retained SWE-bench SDK identity differs")
    catalog, row = read_task(catalog_path, retained.identity["task_id"])
    require(catalog.get("workload") == retained.identity["workload"]
            and catalog.get("dataset") == retained.identity["dataset"]
            and row.get("base_commit") == retained.task["baseline"].get("base_commit"),
            "retained submission differs from its installed grading catalog")
    patch = retained.bytes("candidate.patch",MAX_PATCH_BYTES)
    require(retained.submission.get("patch_sha256") == hashlib.sha256(patch).hexdigest()
            and retained.submission.get("patch_bytes") == len(patch)
            and retained.submission.get("base_commit") == row["base_commit"],
            "retained candidate patch or base commit changed")
    if retained.child("selected-evaluation.json").exists():
        inspect_retained(retained)
        return
    candidates = []
    completed = []
    raw = []
    # A preliminary absence scan must not freeze an old failed wrapper into
    # the final evidence reader: the independently owned grader may complete
    # while it is being excluded. Only the post-exclusion scan decides retry.
    probe = RetainedOutcome({"reference":retained.reference},inspect=True)
    for index,path in enumerate(retained.root.iterdir()):
        require(index < 256, "retained attempt has excessive top-level artifacts")
        if path.name.startswith(("evaluation-","recovered-evaluation-")):
            require(re.fullmatch(r"(?:evaluation|recovered-evaluation)-[a-f0-9]{32}",path.name),
                    "retained evaluator directory identity is malformed")
            require(not path.is_symlink() and path.is_dir(), "retained evaluator is not a real directory")
            candidates.append(path)
            result_path = retained.child(path.name+"/evaluation.json")
            result = {}
            if result_path.exists():
                result = probe.object(path.name+"/evaluation.json")
                if result.get("outcome") in ["resolved","unresolved"]:
                    require(type(result.get("resolved")) is bool, "completed retained grade is malformed")
                    completed.append(path.name)
            else:
                require(retained.child(path.name+"/resources.json").is_file(),
                        "unfinished evaluator has no durable ownership journal; inspect it before grading again")
            if path.name not in completed:
                # A lost wrapper result does not imply the official tests never
                # finished. Preserve that potential grade for reconstruction,
                # rather than rerunning a flaky failure until it passes.
                reports = [result["official_report"]] if isinstance(result.get("official_report"),str) else []
                run_id = result.get("run_id")
                if not run_id and retained.child(path.name+"/resources.json").exists():
                    run_id = probe.object(path.name+"/resources.json").get("attempt")
                if isinstance(run_id,str) and re.fullmatch(r"asb-[a-f0-9]{32}",run_id):
                    reports.append("logs/evaluation/"+run_id+"/asb/"+retained.identity["task_id"]+"/report.json")
                    reports.append("logs/evaluation/"+run_id+"/results.json")
                if any(retained.child(path.name+"/"+relative).exists() for relative in reports):raw.append(path.name)
    if _excluded is not None:
        require({path.name for path in candidates} == _excluded[2]
                and all(path.name in _excluded[1] for path in candidates if (path/"resources.json").exists()),
                "old evaluator ownership population changed during exclusion; no evaluator was started")
    require(len(raw) <= 1,"multiple raw official grades cannot be replaced or guessed")
    if raw and len(completed) == 1:
        receipt = retained.object(completed[0]+"/evaluation.json").get("reconstruction",{})
        # A crash after derived publication but before selection can be
        # reconciled through its complete source binding, not another test run.
        if isinstance(receipt,dict) and receipt.get("source") == raw[0]:raw = []
    require(not (raw and completed),"multiple completed or raw official grades cannot be replaced or guessed")
    if raw:
        reconstruct_evaluation(retained,raw[0],catalog,row,cancelled,
            owner_lock=None if _excluded is None else _excluded[1].get(raw[0]))
        return
    require(len(completed) <= 1, "multiple completed retained evaluations cannot be replaced or guessed")
    if completed:
        # Validate every byte of the existing grade before selecting it. A bad
        # existing grade is an error, never permission to rerun until it passes.
        # Selection is only needed with old failed directories alongside it;
        # validate through the normal verifier with an explicit prefix first.
        retained.select_grade(retained.object(completed[0]+"/evaluation.json"))
        verify_retained(retained, prefix=completed[0]+"/")
        if len(candidates) != 1 or completed[0].startswith("recovered-"):
            retained.publish_selection(completed[0])
        return
    require(not cancelled.is_set(), "retained grading was cancelled before evaluator launch")
    # Retired evaluator journals are independently locked/validated. An active
    # grader must finish or be quiesced; a root lock alone is not sufficient.
    from asb_docker import DockerSession
    from asb_resources import quiesce
    if _excluded is None:
        from asb_resources import open_directory, private_file
        with DockerSession() as client, contextlib.ExitStack() as exclusions:
            for path in candidates:
                if (path/"resources.json").exists():
                    quiesce(client,path)
            owners = {}
            for path in candidates:
                if not (path/"resources.json").exists():continue
                descriptor = open_directory(path)
                exclusions.callback(os.close,descriptor)
                lock = private_file(descriptor,"resources.lock",os.O_RDWR)
                exclusions.callback(os.close,lock)
                try:fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
                except BlockingIOError:require(False,"old evaluator still has an active owner; no evaluator was started")
                owners[path.name] = lock
            # Re-read all normalized/raw outcomes while every old evaluator is
            # excluded. A grade published during quiescence is reused, never
            # replaced by another evaluation (including an unresolved grade).
            return grade_retained(retained,catalog_path,cancelled,
                _excluded=(client,owners,{path.name for path in candidates}))
    with contextlib.nullcontext(_excluded[0]) as client:
        image = retained.task["image"]
        require(re.fullmatch(r"sha256:[0-9a-f]{64}",image.get("image_id","")),
                "retained task has no immutable evaluator image")
        # Lookup only the pinned image; no pull, rebuild, workspace start or
        # client execution can silently replace the retained submission.
        require(client.images.get(image["image_id"]).id == image["image_id"],
                "retained evaluator image is unavailable")
        directory = "recovered-evaluation-"+uuid.uuid4().hex
        def executor(arguments, destination, log, deadline):
            run_id = arguments[arguments.index("--run_id")+1]
            def cancel():
                cleanup = remove_evaluation_container(client,retained.identity["task_id"],run_id)
                atomic_write(destination/"parent-cleanup.json",encoded(cleanup))
                require(cleanup["status"] != "error", "could not stop the retained submission evaluator")
            return run_harness(arguments,destination,log,deadline,cancelled=cancelled,on_cancel=cancel)
        result = evaluate_submission(catalog_path,retained.identity["task_id"],retained.child("candidate.patch"),
            retained.child(directory),1800,executor=executor,image_identity=image)
        require(type(result.get("resolved")) is bool
                and result.get("outcome") in ["resolved","unresolved"],
                "official retained-submission evaluation did not complete; original work and grader artifacts were preserved")
        retained.select_grade(result)
        verify_retained(retained,prefix=directory+"/")
        retained.publish_selection(directory)


def inspect_retained(retained):
    retained.select_grade(retained.object(retained_evaluation_prefix(retained) + "evaluation.json"))
    verify_retained(retained)


def verify_retained(retained, prefix=None, result=None):
    """Validate a closed SWE-bench record without executing tests or an agent."""
    from asb_retained import require
    require(retained.task.get("sdk") == sdk_identity(), "retained SWE-bench SDK identity differs")
    patch = retained.bytes("candidate.patch", MAX_PATCH_BYTES)
    digest = hashlib.sha256(patch).hexdigest()
    require(retained.submission.get("patch_sha256") == digest
            and retained.submission.get("patch_bytes") == len(patch)
            and retained.submission.get("base_commit") == retained.task["baseline"].get("base_commit"),
            "retained SWE-bench submission hash or base commit differs")
    prefix = retained_evaluation_prefix(retained) if prefix is None else prefix
    result = retained.object(prefix + "evaluation.json") if result is None else result
    require(result.get("schema_version") == 1
            and result.get("harness_revision") == HARNESS_REVISION
            and all(result.get(key) == retained.identity.get(key) for key in ["workload","task_id","dataset"]),
            "retained SWE-bench evaluator identity differs")
    require(result.get("patch_sha256") == digest, "retained evaluator patch hash differs")
    if not patch.strip():
        require(result.get("resolved") is False and result.get("outcome") == "unresolved"
                and result.get("reason") == "empty_submission" and retained.summary["workload_success"] is False
                and "reconstruction" not in result,
                "retained empty submission has an invalid grade")
        return
    retained.grade(result)
    rows = json.loads(retained.bytes(prefix + "dataset.json"))
    require(isinstance(rows,list) and len(rows) == 1
            and rows[0].get("instance_id") == retained.identity["task_id"]
            and rows[0].get("base_commit") == retained.submission["base_commit"]
            and rows[0].get("image") == result["image"].get("declared_image"),
            "retained evaluator dataset row differs")
    submitted = retained.object(prefix + "submission.jsonl")
    require(submitted.get("instance_id") == retained.identity["task_id"]
            and submitted.get("model_patch") == patch.decode("utf-8"), "retained evaluator submission differs")
    require(all(isinstance(result.get(key),str) and result[key] for key in ["official_report","official_summary"]),
            "retained official report paths are malformed")
    report = retained.object(prefix + result["official_report"])
    summary = retained.object(prefix + result["official_summary"])
    task = retained.identity["task_id"]
    require(set(report) == {task} and type(report[task].get("resolved")) is bool and report[task].get("resolved") == result["resolved"]
            and type(summary.get("total_instances")) is int and summary.get("total_instances") == 1 and summary.get("submitted_ids") == [task]
            and summary.get("completed_ids") == [task]
            and summary.get("resolved_ids") == ([task] if result["resolved"] else [])
            and (not result["resolved"] or report[task].get("patch_successfully_applied") is True),
            "retained official SWE-bench reports disagree")
    require(result.get("sdk") == retained.object(prefix + "sdk-identity.json")
            and result.get("sdk") == retained.task.get("sdk")
            and result.get("image") == retained.object(prefix + "image-identity.json")
            and result["image"].get("image_id") == retained.task["image"].get("image_id"),
            "retained official evaluator image or SDK differs")
    if "reconstruction" in result:
        receipt = result["reconstruction"]
        require(isinstance(receipt,dict) and receipt.get("schema_version") == 1
                and receipt.get("scope") == "existing-official-reports-not-test-retry"
                and re.fullmatch(r"(?:evaluation|recovered-evaluation)-[a-f0-9]{32}",str(receipt.get("source","")))
                and receipt["source"]+"/" != prefix and isinstance(receipt.get("inputs"),dict)
                and 1 <= len(receipt["inputs"]) <= 16,"reconstructed evaluator provenance is malformed")
        expected_sources = {receipt["source"]+"/"+relative for relative in [
            "dataset.json","submission.jsonl","sdk-identity.json","image-identity.json",result["official_report"],
            result["official_summary"],"resources.json","cleanup.json","evaluation.json"]}
        require(set(receipt["inputs"]) == expected_sources,"reconstructed evaluator source population is incomplete")
        for relative,expected in receipt["inputs"].items():
            require(isinstance(relative,str) and relative.startswith(receipt["source"]+"/"),
                    "reconstructed evaluator source escapes its original directory")
            if expected is None:
                require(relative == receipt["source"]+"/evaluation.json" and not retained.child(relative).exists(),
                        "missing original wrapper was replaced after reconstruction")
            else:
                body = retained.bytes(relative)
                require(expected == {"sha256":hashlib.sha256(body).hexdigest(),"bytes":len(body)},
                        "original official evaluator artifacts changed after reconstruction")
                if relative != receipt["source"]+"/evaluation.json":
                    copied = retained.bytes(prefix+relative.removeprefix(receipt["source"]+"/"))
                    require(expected == {"sha256":hashlib.sha256(copied).hexdigest(),"bytes":len(copied)},
                            "reconstructed evaluator copy differs from its original artifact")
        journal = retained.object(receipt["source"]+"/resources.json")
        name = f"sweb.eval.{task.lower()}.{result.get('run_id')}"
        require(journal.get("schema_version") == 1 and re.fullmatch(r"asb-[a-f0-9]{32}",str(result.get("run_id","")))
                and journal.get("attempt") == result["run_id"]
                and journal.get("daemon") == retained.object("resources.json").get("daemon")
                and re.fullmatch(r"[a-f0-9]{32}",str(journal.get("token","")))
                and journal.get("resources") == [{"kind":"containers","name":name}],
                "reconstructed evaluator resource identity differs")
        cleanup = retained.object(receipt["source"]+"/cleanup.json")
        require(cleanup.get("container") == name and cleanup.get("status") in ["removed","absent"],
                "reconstructed evaluator cleanup is not confirmed")


def attempt_report(reporter):
    """Keep official grading/reporting, omit its optional host-wide inventory.

    Listing every daemon image races unrelated build-tag cleanup. Owned
    resource cleanup/provenance is recorded separately by this adapter.
    """
    def report(predictions,full_dataset,run_id,client=None):
        return reporter(predictions,full_dataset,run_id,client=None)
    return report


def official_harness(arguments):
    """Run the pinned SDK with scoped Docker transport and no root Python."""
    from asb_docker import DockerSession
    from asb_resources import ResourceJournal, RecoveryError
    from swebench.harness import run_evaluation
    import docker.errors
    identity = sdk_identity()
    atomic_write(Path.cwd() / "sdk-identity.json", encoded(identity))
    if (not re.fullmatch(r"asb-[0-9a-f]{32}", arguments.run_id)
            or len(arguments.instance_ids) != 1
            or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,255}", arguments.instance_ids[0])):
        raise CatalogError("official evaluation needs one bounded task and a fresh ASB run identity")
    cancelled, finished = threading.Event(), threading.Event()
    def interrupted(_signal, _frame):
        # Raising inside ThreadPoolExecutor.__exit__ waits for the evaluator
        # thread before cleanup. A separate caretaker removes this exact
        # attempt first, which unblocks that thread and allows prompt exit.
        cancelled.set()
    signal.signal(signal.SIGTERM, interrupted)
    with DockerSession() as client:
        owned_name = f"sweb.eval.{arguments.instance_ids[0].lower()}.{arguments.run_id}"
        journal = ResourceJournal(client, Path.cwd(), arguments.run_id)
        class ScopedContainers:
            def get(self, name):
                if name != owned_name:
                    raise RecoveryError("evaluator requested a container outside this attempt")
                container = client.containers.get(name)
                labels = container.attrs.get("Config", {}).get("Labels", {}) or {}
                if any(labels.get(key) != value for key, value in journal.labels.items()):
                    raise RecoveryError("evaluator container belongs to a different owner")
                return container
            def create(self, *args, **kwargs):
                if kwargs.get("name") != owned_name:
                    raise RecoveryError("evaluator cannot change its declared container identity")
                kwargs.setdefault("labels", {}).update(journal.declare("containers", owned_name))
                return client.containers.create(*args, **kwargs)
        class ScopedClient:
            containers = ScopedContainers()
            def __getattr__(self, name):
                return getattr(client, name)
        original_create = run_evaluation.create_container
        def create_container(test_spec, docker_client, run_id, logger):
            if cancelled.is_set():
                raise InterruptedError("ASB evaluation was cancelled before container creation")
            # Evaluation must use the same native image as task execution; never
            # let the upstream harness launch an architecture-mismatched image.
            from asb_execution import require_native_image
            require_native_image(client, client.images.get(test_spec.image))
            container = original_create(test_spec, ScopedClient(), run_id, logger)
            try:
                if cancelled.is_set():
                    raise InterruptedError("ASB evaluation was cancelled during container creation")
                image = container.image
                atomic_write(Path.cwd() / "image-identity.json", encoded({
                    "declared_image": test_spec.image,
                    "image_id": image.id,
                    "repository_digests": image.attrs.get("RepoDigests", []),
                    "architecture": image.attrs.get("Architecture"),
                    "os": image.attrs.get("Os"),
                }))
                return container
            except BaseException:
                # Creation succeeded but provenance capture failed; do not
                # leak that newly owned container or accept an unbound score.
                try:
                    container.remove(force=True)
                except docker.errors.NotFound:
                    pass
                raise
        def cleanup_container(_client, container, logger):
            if container is None:
                return
            if container.name.lstrip("/") != owned_name:
                raise CatalogError("refused cleanup of a container outside the current attempt")
            try:
                container.remove(force=True)
                logger.info("Removed ASB attempt container through scoped Docker transport")
            except docker.errors.NotFound:
                pass
        def caretaker():
            while not finished.is_set():
                if cancelled.wait(0.05):
                    cleanup = remove_evaluation_container(client, arguments.instance_ids[0], arguments.run_id, journal.labels)
                    if cleanup["status"] == "removed":
                        return
                    time.sleep(0.05)
        care = threading.Thread(target=caretaker, daemon=True)
        care.start()
        try:
            run_evaluation._docker_client = lambda: client
            run_evaluation.create_container = create_container
            # This SDK revision shells out to an unprivileged Docker CLI for
            # cleanup. Route its equivalent operation through the same scoped
            # client as creation, avoiding permission failures and leftovers.
            run_evaluation.cleanup_container = cleanup_container
            run_evaluation.make_run_report = attempt_report(run_evaluation.make_run_report)
            run_evaluation.main(
                dataset_name=str(arguments.dataset_name), split=arguments.split,
                instance_ids=arguments.instance_ids, predictions_path=str(arguments.predictions_path),
                max_workers=arguments.max_workers, open_file_limit=arguments.open_file_limit,
                run_id=arguments.run_id, timeout=arguments.timeout, rewrite_reports=False, modal=False,
            )
            if cancelled.is_set():
                raise InterruptedError("ASB evaluation was cancelled")
        finally:
            # Upstream cleans normal outcomes. This also covers controller
            # exceptions and graceful cancellation; never enumerate/remove
            # unrelated containers or prune host images.
            finished.set()
            care.join(timeout=2)
            try:
                cleanup = remove_evaluation_container(client, arguments.instance_ids[0], arguments.run_id, journal.labels)
                atomic_write(Path.cwd() / "cleanup.json", encoded(cleanup))
            finally:
                journal.close()
    return {"status": "evaluated", "run_id": arguments.run_id}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="operation", required=True)
    catalog = commands.add_parser("catalog")
    catalog.add_argument("--workload", required=True)
    catalog.add_argument("--dataset", required=True)
    catalog.add_argument("--revision", default="main")
    catalog.add_argument("--split", default="test")
    catalog.add_argument("--output", type=Path, required=True)
    catalog.add_argument("--cache", type=Path, required=True)
    commands.add_parser("identity")
    verify = commands.add_parser("verify")
    verify.add_argument("--catalog", type=Path, required=True)
    session = commands.add_parser("session")
    session.add_argument("--catalog", type=Path, required=True)
    session.add_argument("--task", required=True)
    session.add_argument("--artifacts", type=Path, required=True)
    evaluate = commands.add_parser("evaluate")
    evaluate.add_argument("--catalog", type=Path, required=True)
    evaluate.add_argument("--task", required=True)
    evaluate.add_argument("--patch", type=Path, required=True)
    evaluate.add_argument("--artifacts", type=Path, required=True)
    evaluate.add_argument("--timeout", type=int, default=1800)
    harness = commands.add_parser("harness")
    harness.add_argument("--dataset_name", type=Path, required=True)
    harness.add_argument("--split", required=True)
    harness.add_argument("--instance_ids", nargs="+", required=True)
    harness.add_argument("--predictions_path", type=Path, required=True)
    harness.add_argument("--max_workers", type=int, required=True)
    harness.add_argument("--open_file_limit", type=int, required=True)
    harness.add_argument("--run_id", required=True)
    harness.add_argument("--timeout", type=int, required=True)
    arguments = parser.parse_args(argv)
    if arguments.operation == "session":
        from asb_docker import DockerProbeError, DockerSession
        from asb_swebench_task import SweBenchTask
        from asb_task_server import serve
        install_session_cancel_handlers()
        with contextlib.ExitStack() as stack:
            try:
                client = stack.enter_context(DockerSession())
            except DockerProbeError as error:
                # Only session startup gets this disposition: no task was created.
                print(json.dumps({"event": "error", "error_class": "ExecutionError", "reason": str(error)}), flush=True)
                print(json.dumps({"event": "closed", "cleanup": "not_created"}), flush=True)
                return 1
            return serve(SweBenchTask(client, arguments.catalog, arguments.task, arguments.artifacts,
                                      tool_mode=os.environ.get("ASB_WORKLOAD_TOOL_MODE", "none"),
                                      snapshot=os.environ.get("ASB_EXPERIMENTAL_SNAPSHOT") == "1"),
                         sys.stdin, sys.stdout)
    try:
        if arguments.operation == "catalog":
            result = install_catalog(arguments)
        elif arguments.operation == "identity":
            result = {"status": "ready", "sdk": sdk_identity()}
        elif arguments.operation == "verify":
            result = verify_catalog(arguments.catalog)
        elif arguments.operation == "evaluate":
            result = evaluate_submission(arguments.catalog, arguments.task, arguments.patch,
                                         arguments.artifacts, arguments.timeout)
        else:
            result = official_harness(arguments)
    except Exception as error:
        # Do not echo arbitrary upstream response bodies or credential-bearing
        # URLs. Detailed library diagnostics stay in the installer's private log.
        result = {"status": "error", "error_class": type(error).__name__}
        if isinstance(error, CatalogError):
            result["reason"] = str(error)
        if isinstance(error, ImportError):
            result["missing_module"] = getattr(error, "name", None)
        response = getattr(error, "response", None)
        if response is not None:
            result["http_status"] = getattr(response, "status_code", None)
        print(json.dumps(result))
        return 1
    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
