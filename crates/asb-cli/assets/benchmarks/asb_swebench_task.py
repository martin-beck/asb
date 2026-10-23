"""SWE-bench-owned preparation and submission capture, using generic execution."""
import hashlib
import re
import time
from pathlib import Path
import uuid

from asb_execution import ExecutionError, ManagedDockerWorkspace
from asb_swebench import (CatalogError, MAX_PATCH_BYTES, agent_instruction,
                          atomic_write, encoded, evaluate_submission, read_task,
                          remove_evaluation_container, run_harness, sdk_identity)
from asb_swebench_sources import build_image, resolve_image
from asb_resources import ResourceJournal


class SweBenchTask:
    def __init__(self, client, catalog_path, task_id, artifacts):
        self.client, self.catalog_path, self.task_id = client, Path(catalog_path), task_id
        self.artifacts = Path(artifacts).absolute()
        self.workspace = None
        self.invocation = None
        self.row = None
        self.catalog = None
        self.cancelled = None
        self.interrupted = False
        self.attempt = "asb-" + uuid.uuid4().hex
        self.source_image_tag = None
        self.journal = None

    def __enter__(self):
        from swebench.harness.utils import make_test_spec
        from swebench.image_builder.constants import CONTAINER_WORKDIR
        sdk = sdk_identity()
        self.catalog, self.row = read_task(self.catalog_path, self.task_id)
        spec = make_test_spec(self.row)
        self.artifacts.mkdir(mode=0o700, parents=True, exist_ok=False)
        try:
            self.journal = ResourceJournal(self.client, self.artifacts, self.attempt)
            image, source = resolve_image(self.client, spec.image, lambda: build_image(
                self.client, self.row, self.catalog["dataset"], self.artifacts, self.attempt,
                cancelled=self.cancelled,journal=self.journal))
            self.source_image_tag = source.get("tag")
            if self.source_image_tag is not None:
                atomic_write(self.artifacts / "source-image.json", encoded(source))
            self.workspace = ManagedDockerWorkspace(self.client, image, self.attempt, CONTAINER_WORKDIR,
                                                    journal=self.journal)
            self.workspace.__enter__()
            base = self.row["base_commit"]
            self.checked(["git", "cat-file", "-e", base + "^{commit}"])
            # This is an ASB-owned container, never the user's host checkout.
            self.checked(["git", "reset", "--hard", base], environment={
                "GIT_COMMITTER_NAME":"ASB workload preparation",
                "GIT_COMMITTER_EMAIL":"asb-workload@localhost"})
            self.checked(["git", "clean", "-fd"])
            observed = self.checked(["git", "rev-parse", "HEAD"]).strip().decode("ascii")
            if observed != base:
                raise CatalogError("prepared repository does not match the official base commit")
            if self.checked(["git", "status", "--porcelain", "--untracked-files=all"]).strip():
                raise CatalogError("prepared task repository is not clean")
            self.invocation = {
                "identity": {"workload": self.catalog["workload"], "task_id": self.task_id,
                             "attempt_id": self.attempt, "dataset": self.catalog["dataset"]},
                "prompt": agent_instruction(self.row), "workspace": str(CONTAINER_WORKDIR),
                "baseline": {"kind": "upstream-task-definition", "base_commit": observed},
                "image": self.workspace.image_identity, "sdk": sdk,
                "resources": self.workspace.limits,
                "environment_source": source,
            }
            # No gold patch/test script is copied into the task environment.
            atomic_write(self.artifacts / "task.json", encoded(self.invocation))
            return self
        except BaseException:
            self.interrupted = True
            self.__exit__(None, None, None)
            raise

    def checked(self, argv, output_limit=256 * 1024, environment=None):
        result = self.workspace.execute(argv, environment=environment, timeout=60, output_limit=output_limit, cancelled=self.cancelled)
        if result["timed_out"] or result["cancelled"] or result["exit_code"] != 0 or result["output_truncated"]:
            # Command output can contain agent-controlled data. Keep diagnostics
            # structured and do not echo it into human/measurement records.
            raise ExecutionError(f"task repository operation {argv[0]} {argv[1]} failed (exit {result['exit_code']}, timeout {result['timed_out']}, truncated {result['output_truncated']})")
        return result["stdout"]

    def capture_submission(self):
        if self.invocation is None:
            raise ExecutionError("task must be prepared before capturing a submission")
        untracked = self.checked(["git", "ls-files", "--others", "--exclude-standard", "-z"], MAX_PATCH_BYTES)
        paths = [path.decode("utf-8") for path in untracked.split(b"\x00") if path]
        if len(paths) > 10000 or sum(len(path.encode()) for path in paths) > 512 * 1024:
            raise ExecutionError("submission contains too many untracked paths")
        if paths:
            self.checked(["git", "add", "--intent-to-add", "--", *paths])
        patch = self.checked(["git", "-c", "core.fsmonitor=false", "diff", "--binary", "--no-ext-diff",
                              "--no-textconv", self.row["base_commit"], "--"], MAX_PATCH_BYTES)
        path = self.artifacts / "candidate.patch"
        atomic_write(path, patch)
        submission = {"task": self.invocation["identity"], "base_commit": self.row["base_commit"],
                      "image": self.workspace.image_identity, "patch_sha256": hashlib.sha256(patch).hexdigest(),
                      "patch_bytes": len(patch), "artifact": "candidate.patch"}
        atomic_write(self.artifacts / "submission.json", encoded(submission))
        return submission

    def initial_material(self, binary=None):
        from asb_material import canonical_digest
        if self.invocation is None:
            raise ExecutionError("official task must be prepared before material capture")
        deadline = time.monotonic() + 420
        def checked(arguments, limit=256*1024):
            if time.monotonic() >= deadline or (self.cancelled is not None and self.cancelled.is_set()):
                raise ExecutionError("prepared material capture was cancelled or exceeded its deadline")
            return self.checked(arguments, limit)
        # Index timestamps and reflogs are SDK reset artifacts. Bind the actual
        # index entries/flags and clean base separately; retain config, hooks,
        # attributes and all other Git/worktree bytes in the archive identity.
        def git_state():
            prefix = ["git", "-c", "core.fsmonitor=false"]
            observed = checked(prefix + ["rev-parse", "HEAD"]).strip().decode("ascii")
            if observed != self.row["base_commit"] or checked(prefix + ["status", "--porcelain", "--untracked-files=all"]).strip():
                raise ExecutionError("prepared repository changed before material capture")
            return {"base_commit":observed,
                "index_sha256":hashlib.sha256(checked(prefix + ["ls-files", "--stage", "-z"], 4*1024**2)).hexdigest(),
                "flags_sha256":hashlib.sha256(checked(prefix + ["ls-files", "-v", "-z"], 4*1024**2)).hexdigest(),
                # Ref/index, commit, action and actor remain bound. Only raw
                # timestamp/timezone fields are omitted; SDK reset actors are
                # explicitly stable rather than inherited from container names.
                "reflog_sha256":hashlib.sha256(checked(prefix + ["reflog", "show", "--all",
                    "--format=%gD%x00%H%x00%gs%x00%gn%x00%ge%x00"], 4*1024**2)).hexdigest(),
                "local_config_sha256":hashlib.sha256(checked(prefix + ["config", "--null", "--local", "--list"], 4*1024**2)).hexdigest()}
        before = git_state()
        if git_state() != before:
            raise ExecutionError("prepared repository changed during material capture")
        image_id = self.workspace.image_identity.get("image_id")
        if not isinstance(image_id, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", image_id):
            raise ExecutionError("prepared task image has no immutable identity")
        # A SWE-bench workspace is a new private container from this immutable
        # image, reset to the pinned commit above. The previous generic path
        # streamed and hashed its whole source tree merely to prove those same
        # facts. Bind the image, exact Git state and workload definition instead.
        return {"schema_version":1,"scope":"prepared-workload-material-v1","sha256":canonical_digest({
            "scope":"swe-bench-clean-git-worktree-and-image-v1",
            "baseline":self.invocation["baseline"],
            "dataset":self.invocation["identity"]["dataset"],
            "sdk":self.invocation["sdk"],"image_id":image_id,"git":before},
            "asb-prepared-workload-material-v1")}

    def __exit__(self, *_exception):
        self.interrupted = self.interrupted or bool(_exception and _exception[0] is not None)
        if self.interrupted and self.journal is not None:
            status = "preservation_failed"
            try:
                self.journal.preserve(self.client)
                status = "stopped-preserved"
            finally:
                self.cleanup_status = status
                if self.workspace is not None:
                    self.workspace.cleanup_status = status
                try:
                    atomic_write(self.artifacts / "task-cleanup.json", encoded({
                        "attempt_id": self.attempt, "status": status,
                    }))
                finally:
                    self.journal.close()
            return
        try:
            if self.workspace is not None:
                try:
                    self.workspace.close()
                finally:
                    atomic_write(self.artifacts / "task-cleanup.json", encoded({
                        "attempt_id": self.attempt, "status": self.workspace.cleanup_status,
                    }))
        finally:
            try:
                if self.journal is not None:
                    images = self.journal.cleanup_images(self.client)
                else:
                    images = []
                if self.source_image_tag is not None:
                    status = next(item["status"] for item in images if item["name"] == self.source_image_tag)
                    atomic_write(self.artifacts / "source-image-cleanup.json", encoded({
                        "attempt_id": self.attempt, "tag": self.source_image_tag, "status": status,
                    }))
                    self.source_image_tag = None
            finally:
                if self.journal is not None:
                    self.journal.close()

    def evaluate(self, timeout=1800):
        submission = self.capture_submission()
        relative = "evaluation-" + uuid.uuid4().hex
        def executor(arguments, directory, log, deadline):
            run_id = arguments[arguments.index("--run_id") + 1]
            def cancel():
                cleanup = remove_evaluation_container(self.client, self.task_id, run_id)
                atomic_write(directory / "parent-cleanup.json", encoded(cleanup))
                if cleanup["status"] == "error":
                    raise ExecutionError("could not stop the owned evaluator container")
            return run_harness(arguments, directory, log, deadline,
                               cancelled=self.cancelled, on_cancel=cancel)
        result = evaluate_submission(
            self.catalog_path, self.task_id, self.artifacts / submission["artifact"],
            self.artifacts / relative, timeout, image_identity=self.workspace.image_identity,
            executor=executor,
        )
        result["artifact"] = relative + "/evaluation.json"
        return result
