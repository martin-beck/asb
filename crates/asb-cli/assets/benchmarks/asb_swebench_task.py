"""SWE-bench-owned preparation and submission capture, using generic execution."""
import hashlib
import os
import re
import time
from contextlib import contextmanager, nullcontext
from pathlib import Path
import uuid

from asb_execution import ExecutionError, ManagedDockerWorkspace, _parse_cpu_list
from asb_swebench import (CatalogError, MAX_PATCH_BYTES, agent_instruction,
                          atomic_write, encoded, evaluate_submission, read_task,
                          remove_evaluation_container, run_harness, sdk_identity)
from asb_swebench_sources import build_image, resolve_image, material_cache_tag
from asb_resources import ResourceJournal

SUBMISSION_GIT_DIR = "/opt/asb-swebench-baseline.git"
SUBMISSION_WORKTREE = "/testbed"
# Repository setup has its own bounds; neither is an agent execution budget.
# Repository checks run through the same host-wide Docker gate as every
# concurrent replay replica.  Their own command runtime remains bounded at
# ten minutes, but queued gate admission must fit inside the workload's
# documented SWE-bench environment-preparation ceiling.  Otherwise a healthy
# command can finish with exit 0 after its local gate watchdog has already
# expired under a large cohort.
_REPOSITORY_GATE_PREPARATION_SECONDS = 7200
_REPOSITORY_COMMAND_SECONDS = 600
_PREPARED_BASELINE_RAW_CACHE_VERSION = "swebench-prepared-baseline-v1"
_PREPARED_BASELINE_MATERIAL_CACHE_VERSION = "swebench-prepared-baseline-v2"
_PREPARED_BASELINE_LOCK_SECONDS = 600
_AGENT_ENVIRONMENT_PROBE_ATTEMPTS = 3


def _validated_prepared_baseline_inputs(source_image_id, task_id, base_commit, material_sha256):
    if (not isinstance(source_image_id, str)
            or not re.fullmatch(r"sha256:[0-9a-f]{64}", source_image_id)
            or not isinstance(task_id, str) or not re.fullmatch(r"[A-Za-z0-9_.-]{1,200}", task_id)
            or not isinstance(base_commit, str) or not re.fullmatch(r"[0-9a-f]{40,64}", base_commit)
            or material_sha256 is not None and (not isinstance(material_sha256, str)
                                                or not re.fullmatch(r"[0-9a-f]{64}", material_sha256))):
        raise ExecutionError("prepared baseline cache requires validated image, task and base identities")


def prepared_baseline_cache_tag(source_image_id, task_id, base_commit, material_sha256=None):
    """Address one replay-only, clean task baseline without host-specific names.

    A verified retained replay material is content-addressed, whereas a Docker
    image ID is daemon-local.  Prefer that material identity so a fresh daemon
    or an equivalent imported image can reuse the same clean baseline.
    """
    _validated_prepared_baseline_inputs(source_image_id, task_id, base_commit, material_sha256)
    identity = ("material-v1\0" + material_sha256) if material_sha256 is not None else source_image_id
    digest = hashlib.sha256((identity + "\0" + task_id + "\0" + base_commit).encode("ascii")).hexdigest()
    return "asb-swebench-baseline:" + digest


def prepared_baseline_labels(source_image, task_id, base_commit, material_sha256=None):
    """Bind a retained baseline to an exact image or verified replay material."""
    image_id = getattr(source_image, "id", None)
    tag = prepared_baseline_cache_tag(image_id, task_id, base_commit, material_sha256)
    labels = {"asb.owner": "asb",
              "asb.cache": (_PREPARED_BASELINE_MATERIAL_CACHE_VERSION if material_sha256 is not None
                            else _PREPARED_BASELINE_RAW_CACHE_VERSION),
              "asb.cache-key": tag.rsplit(":", 1)[1],
              "asb.task-id": task_id, "asb.base-commit": base_commit}
    if material_sha256 is not None:
        labels["asb.material-sha256"] = material_sha256
    else:
        labels["asb.source-image"] = image_id
    return tag, labels


def verified_replay_material(source, expected_material):
    """Return the cassette-bound material only after offline cache authentication."""
    if expected_material is None:
        return None
    if (not isinstance(source, dict)
            or source.get("source") != "verified-local-material-cache"
            or source.get("cache_material_sha256") != expected_material
            or not isinstance(expected_material, str)
            or not re.fullmatch(r"[0-9a-f]{64}", expected_material)):
        raise CatalogError("offline replay did not resolve the expected verified material identity")
    return expected_material


def _validated_prepared_baseline(image, source_image, task_id, base_commit, material_sha256=None):
    tag, expected = prepared_baseline_labels(source_image, task_id, base_commit, material_sha256)
    attrs = getattr(image, "attrs", {})
    labels = attrs.get("Config", {}).get("Labels", {}) or {}
    source_attrs = getattr(source_image, "attrs", {})
    if (not isinstance(getattr(image, "id", None), str)
            or not re.fullmatch(r"sha256:[0-9a-f]{64}", image.id)
            or any(labels.get(key) != value for key, value in expected.items())
            or attrs.get("Os") != "linux"
            or attrs.get("Architecture") != source_attrs.get("Architecture")
            or source_attrs.get("Os") != "linux"):
        raise ExecutionError("prepared baseline cache has invalid identity, ownership or architecture")
    return tag


def load_prepared_baseline(client, source_image, task_id, base_commit, material_sha256=None):
    """Return an authenticated retained baseline, or None if it has not been made."""
    import docker.errors
    tag, _labels = prepared_baseline_labels(source_image, task_id, base_commit, material_sha256)
    try:
        image = client.images.get(tag)
    except docker.errors.ImageNotFound:
        return None
    _validated_prepared_baseline(image, source_image, task_id, base_commit, material_sha256)
    return image


def retain_prepared_baseline(client, container, source_image, task_id, base_commit, material_sha256=None):
    """Commit exactly one verified clean task container for future offline replays."""
    tag, labels = prepared_baseline_labels(source_image, task_id, base_commit, material_sha256)
    repository, version = tag.rsplit(":", 1)
    created = container.commit(repository=repository, tag=version, conf={"Labels": labels})
    image = client.images.get(tag)
    if image.id != created.id:
        raise ExecutionError("prepared baseline cache identity changed while it was published")
    _validated_prepared_baseline(image, source_image, task_id, base_commit, material_sha256)
    return image


@contextmanager
def prepared_baseline_lock(catalog_path, tag, cancelled=None):
    """Serialize the one-time replay preparation without serializing execution."""
    try:
        import fcntl
    except ImportError:
        # Docker-backed SWE-bench is not supported on native Windows. Preserve
        # correctness there even though concurrent first use may duplicate work.
        yield
        return
    key = tag.rsplit(":", 1)[1]
    directory = Path(catalog_path).parent / ".asb-prepared-baseline-locks"
    directory.mkdir(mode=0o700, exist_ok=True)
    path = directory / (key + ".lock")
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT, 0o600)
    started = time.monotonic()
    try:
        while True:
            if cancelled is not None and cancelled.is_set():
                raise InterruptedError("prepared baseline cache wait was cancelled")
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() - started >= _PREPARED_BASELINE_LOCK_SECONDS:
                    raise ExecutionError("prepared baseline cache wait exceeded its deadline")
                time.sleep(0.1)
        yield
    finally:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_UN)
        finally:
            os.close(descriptor)


def prepared_baseline_lock_if_missing(catalog_path, tag, baseline, cancelled=None):
    """Lock only a replay cache miss; cached baselines are safe to open in parallel."""
    if baseline is None:
        return prepared_baseline_lock(catalog_path, tag, cancelled)
    return nullcontext()


def snapshot_storage_path(artifacts, attempt, tool_mode):
    """Allocate optional replay scratch and retain its location with the attempt."""
    configured = os.environ.get('ASB_EXPERIMENTAL_SNAPSHOT_STORAGE_ROOT') if tool_mode == 'replay' else None
    if configured is None:
        storage = artifacts/'snapshot-storage'
    else:
        root = Path(configured)
        if not root.is_absolute() or not root.is_dir():
            raise ExecutionError('experimental snapshot storage root must be an existing absolute directory')
        storage = root/attempt
    storage.mkdir(mode=0o700)
    if configured is not None:
        atomic_write(artifacts/'snapshot-storage-location.json',encoded({
            'schema_version':1,'attempt':attempt,'path':str(storage),
            'scope':'experimental-retained-replay-scratch',
        }))
    return storage


def reflog_digest(entries):
    # Git merges refs by timestamp. Ref-local positions are explicit in %gD,
    # so canonicalize the cross-ref order without discarding any entry.
    return hashlib.sha256(b"".join(sorted(entries.splitlines(keepends=True)))).hexdigest()


def source_image_material(image, image_id, source, owner_labels):
    """Bind executable image content while excluding verified attempt labels."""
    if (not isinstance(source, dict) or source.get("status") != "built"
            or source.get("image_id") != image_id or getattr(image, "id", None) != image_id
            or not isinstance(owner_labels, dict)
            or set(owner_labels) != {"asb.owner", "asb.attempt", "asb.recovery"}):
        raise ExecutionError("source image identity or ownership is unavailable")
    attrs = image.attrs
    config = attrs.get("Config")
    rootfs = attrs.get("RootFS")
    if not isinstance(config, dict) or not isinstance(rootfs, dict):
        raise ExecutionError("source image runtime configuration is unavailable")
    labels = config.get("Labels")
    layers = rootfs.get("Layers")
    if (not isinstance(labels, dict) or any(labels.get(key) != value for key,value in owner_labels.items())
            or rootfs.get("Type") != "layers" or not isinstance(layers, list) or not layers
            or any(not isinstance(item, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", item) for item in layers)
            or not isinstance(attrs.get("Architecture"), str) or attrs.get("Os") != "linux"):
        raise ExecutionError("source image content or ownership does not match its receipt")
    config = dict(config)
    config["Labels"] = {key:value for key,value in labels.items() if key not in owner_labels}
    return {"kind":"owned-source-image-content-v1", "architecture":attrs["Architecture"],
            "os":attrs["Os"], "variant":attrs.get("Variant"), "os_version":attrs.get("OsVersion"),
            "os_features":attrs.get("OsFeatures"), "rootfs":rootfs, "config":config}


def retain_source_image(client, source_image, image_id, image, material_sha256):
    import docker.errors
    tag = material_cache_tag(material_sha256)
    try:
        existing = client.images.get(tag)
    except docker.errors.ImageNotFound:
        existing = None
    if existing is not None and existing.id != image_id:
        existing_labels = (existing.attrs.get("Config", {}).get("Labels", {}) or {})
        existing_owner = {key: existing_labels.get(key) for key in ("asb.owner", "asb.attempt", "asb.recovery")}
        if (existing_owner["asb.owner"] != "asb"
                or not re.fullmatch(r"asb-[0-9a-f]{32}", existing_owner["asb.attempt"] or "")
                or not re.fullmatch(r"[0-9a-f]{32}", existing_owner["asb.recovery"] or "")
                or source_image_material(existing, existing.id,
                    {"status":"built", "image_id":existing.id}, existing_owner) != image):
            raise ExecutionError("source image material cache tag already identifies another image")
    if existing is None:
        repository, version = tag.rsplit(":", 1)
        if not source_image.tag(repository, tag=version):
            raise ExecutionError("cannot retain source image for offline replay")
        if client.images.get(tag).id != image_id:
            raise ExecutionError("retained source image identity changed")

class SweBenchTask:
    def __init__(self, client, catalog_path, task_id, artifacts, tool_mode="none", snapshot=False):
        if tool_mode not in ("none", "record", "replay"):
            raise ValueError("unsupported workload tool mode")
        self.tool_mode = tool_mode
        self.snapshot = snapshot and tool_mode != "none"
        requested_cpus = os.environ.get("ASB_CPU_ALLOCATION")
        self.cpu_set = None if requested_cpus is None else sorted(_parse_cpu_list(requested_cpus))
        self.client, self.catalog_path, self.task_id = client, Path(catalog_path), task_id
        self.artifacts = Path(artifacts).absolute()
        self.workspace = None
        self.invocation = None
        self.row = None
        self.catalog = None
        self.cancelled = None
        self.interrupted = False
        self.preparing_repository = False
        self.attempt = "asb-" + uuid.uuid4().hex
        self.source_image_tag = None
        self.material_source_image = None
        self.material_source_image_id = None
        self.verified_replay_material = None
        self.journal = None

    def submission_git_dir(self):
        return '/asb-snapshot-storage/baseline.git' if self.snapshot else SUBMISSION_GIT_DIR

    def __enter__(self):
        from swebench.harness.utils import make_test_spec
        from swebench.image_builder.constants import CONTAINER_WORKDIR
        sdk = sdk_identity()
        self.catalog, self.row = read_task(self.catalog_path, self.task_id)
        spec = make_test_spec(self.row)
        self.artifacts.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.preparing_repository = True
        try:
            self.journal = ResourceJournal(self.client, self.artifacts, self.attempt)
            expected_material = os.environ.get("ASB_EXPECTED_MATERIAL_SHA256")
            image, source = resolve_image(self.client, spec.image, lambda: build_image(
                self.client, self.row, self.catalog["dataset"], self.artifacts, self.attempt,
                cancelled=self.cancelled,journal=self.journal), offline=self.tool_mode == "replay",
                material_sha256=expected_material)
            self.source_image_tag = source.get("tag")
            if self.source_image_tag is not None:
                atomic_write(self.artifacts / "source-image.json", encoded(source))
            snapshot_storage = None
            if self.snapshot:
                snapshot_storage = snapshot_storage_path(self.artifacts,self.attempt,self.tool_mode)
            base = self.row["base_commit"]
            material_identity = (verified_replay_material(source, expected_material)
                                 if self.tool_mode == "replay" else None)
            self.verified_replay_material = material_identity
            cache_tag, _cache_labels = prepared_baseline_labels(
                image, self.task_id, base, material_identity)
            # A verified replay cache hit is immutable and can be opened by all
            # cohort peers concurrently. Serialize only the first creator.
            baseline = (
                load_prepared_baseline(self.client, image, self.task_id, base, material_identity)
                if self.tool_mode == "replay" else None
            )
            cache_lock = (
                prepared_baseline_lock_if_missing(
                    self.catalog_path, cache_tag, baseline, self.cancelled)
                if self.tool_mode == "replay" else nullcontext()
            )
            with cache_lock:
                # Another peer may have created the baseline while this peer
                # waited for the miss lock. Re-read under the lock before
                # deciding whether this workspace must create it.
                if baseline is None and self.tool_mode == "replay":
                    baseline = load_prepared_baseline(
                        self.client, image, self.task_id, base, material_identity)
                workspace_image = baseline or image
                self.workspace = ManagedDockerWorkspace(self.client, workspace_image, self.attempt, CONTAINER_WORKDIR,
                                                        journal=self.journal, snapshot=self.snapshot,
                                                        snapshot_storage=snapshot_storage, offline=self.tool_mode == "replay",
                                                        cpu_set=self.cpu_set)
                self.workspace.__enter__()
                self.checked(["git", "cat-file", "-e", base + "^{commit}"])
                if baseline is None:
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
                if baseline is None:
                    self.prepare_base_history(str(CONTAINER_WORKDIR), '/opt/'+self.attempt+'-base-history.git', base)
                    # Keep an independent copy of the verified base Git metadata for
                    # submission generation. Agents may modify or delete their own
                    # .git directory; that must not destroy the original comparison.
                    self.checked(["cp", "-a", f"{CONTAINER_WORKDIR}/.git", self.submission_git_dir()])
                    if self.tool_mode == "replay":
                        retain_prepared_baseline(self.client, self.workspace.container, image, self.task_id, base,
                                                 material_identity)
                elif self.snapshot:
                    # Snapshot storage is per attempt, so only its independent
                    # submission copy is recreated from the cached clean tree.
                    self.checked(["cp", "-a", f"{CONTAINER_WORKDIR}/.git", self.submission_git_dir()])
                self.checked(["git", f"--git-dir={self.submission_git_dir()}",
                              f"--work-tree={CONTAINER_WORKDIR}", "cat-file", "-e", base + "^{commit}"])
            # The cache image is only a transport for the verified clean tree.
            # Replay material must remain bound to the source image that made
            # the original recording, not the Docker commit used to share it.
            self.material_source_image = image
            self.material_source_image_id = image.id
            self.invocation = {
                "identity": {"workload": self.catalog["workload"], "task_id": self.task_id,
                             "attempt_id": self.attempt, "dataset": self.catalog["dataset"]},
                "prompt": agent_instruction(self.row), "workspace": str(CONTAINER_WORKDIR),
                "baseline": {"kind": "upstream-task-definition", "base_commit": observed},
                "image": self.workspace.image_identity, "sdk": sdk,
                "resources": self.workspace.limits,
                "environment_source": source,
                "prepared_baseline": {"status": "reused" if baseline is not None else "created",
                                      "tag": cache_tag} if self.tool_mode == "replay" else None,
            }
            # No gold patch/test script is copied into the task environment.
            atomic_write(self.artifacts / "task.json", encoded(self.invocation))
            return self
        except BaseException:
            self.interrupted = True
            self.__exit__(None, None, None)
            raise
        finally:
            self.preparing_repository = False

    def checked(self, argv, output_limit=256 * 1024, environment=None, timeout=None, description=None,
                preparation_budget=None):
        if timeout is None:
            # Staging and submission helpers run outside repository setup but
            # still precede or follow the agent's separately timed execution.
            # Their Docker transport can be slow on emulated architectures.
            timeout = _REPOSITORY_COMMAND_SECONDS
        options = {"environment": environment, "timeout": timeout,
                   "output_limit": output_limit, "cancelled": self.cancelled}
        if preparation_budget is None:
            preparation_budget = getattr(self, "_asb_preparation_budget", None)
        if preparation_budget is not None:
            # Every helper before the stopped target shares the cohort-wide
            # startup deadline, while retaining its own command timeout.
            options["preparation_timeout"] = preparation_budget.remaining()
        elif self.preparing_repository:
            options["preparation_timeout"] = _REPOSITORY_GATE_PREPARATION_SECONDS
        result = self.workspace.execute(argv, **options)
        if result["output_truncated"] and not result["timed_out"] and not result["cancelled"] and result["exit_code"] == 0:
            label = description or "repository operation"
            raise ExecutionError(f"{label} output exceeded the {output_limit} byte limit; the attempt was preserved")
        if result["timed_out"] or result["cancelled"] or result["exit_code"] != 0 or result["output_truncated"]:
            # Command output can contain agent-controlled data. Keep diagnostics
            # structured and do not echo it into human/measurement records.
            raise ExecutionError(f"task repository operation {argv[0]} {argv[1]} failed (exit {result['exit_code']}, timeout {result['timed_out']}, truncated {result['output_truncated']})")
        return result["stdout"]

    def agent_environment(self, environment, preparation_budget=None):
        """Activate the SDK's task environment for the final client only."""
        from swebench.image_builder.constants import CONTAINER_ENV_NAME
        if not re.fullmatch(r"[A-Za-z0-9_.-]+", CONTAINER_ENV_NAME):
            raise ExecutionError("invalid SWE-bench environment name")
        def remaining():
            return min(60, preparation_budget.remaining()) if preparation_budget else 60
        def probe(argv, **options):
            """Retry bounded read-only environment probes under Docker pressure."""
            for attempt in range(_AGENT_ENVIRONMENT_PROBE_ATTEMPTS):
                try:
                    return self.checked(argv, **options)
                except Exception as error:
                    if attempt + 1 == _AGENT_ENVIRONMENT_PROBE_ATTEMPTS:
                        if not isinstance(error, ExecutionError):
                            raise ExecutionError("SWE-bench task environment probe failed") from None
                        raise
                    if preparation_budget is not None:
                        preparation_budget.remaining()
                    time.sleep(0.1 * (attempt + 1))
        # Only PATH is passed to the probe: provider credentials and HOME must
        # never enter probe output or be overwritten by conda activation.
        probe_environment = {"PATH": environment["PATH"]} if "PATH" in environment else None
        output = probe(["/bin/bash", "-c",
            'source /opt/miniconda3/bin/activate "$1" >/dev/null && '
            'printf "%s\\0%s\\0%s\\0%s\\0" "$PATH" "$CONDA_PREFIX" "$CONDA_DEFAULT_ENV" "$CONDA_SHLVL"',
            "asb-swe-env", CONTAINER_ENV_NAME], environment=probe_environment, timeout=remaining(),
                              preparation_budget=preparation_budget)
        try:
            fields = output.decode("utf-8").split("\0")
            if len(fields) != 5 or fields[-1] or not all(fields[:4]):
                raise ValueError()
            path, prefix, name, level = fields[:4]
            if not prefix.startswith("/") or name != CONTAINER_ENV_NAME or not level.isdigit():
                raise ValueError()
        except (UnicodeDecodeError, ValueError):
            raise ExecutionError("SWE-bench task environment activation returned invalid metadata") from None
        timestamp = probe(["git", "show", "-s", "--format=%ct", self.row["base_commit"]],
                                 timeout=remaining(), preparation_budget=preparation_budget).strip()
        if not re.fullmatch(rb"[0-9]{1,12}", timestamp):
            raise ExecutionError("SWE-bench base commit timestamp is invalid")
        result = dict(environment, PATH=path, CONDA_PREFIX=prefix,
                      CONDA_DEFAULT_ENV=name, CONDA_SHLVL=level)
        date = "@" + timestamp.decode("ascii") + " +0000"
        result.setdefault("GIT_AUTHOR_DATE", date)
        result.setdefault("GIT_COMMITTER_DATE", date)
        # Git otherwise chooses abbreviations based on the current object set.
        # Full object IDs retain identity even when a tool clones a repository
        # whose remote has gained objects between recording and replay. Honor
        # an explicitly supplied command-scope Git configuration unchanged.
        if not any(key == "GIT_CONFIG_COUNT" or key.startswith("GIT_CONFIG_KEY_")
                   or key.startswith("GIT_CONFIG_VALUE_") for key in result):
            result.update(GIT_CONFIG_COUNT="1", GIT_CONFIG_KEY_0="core.abbrev",
                          GIT_CONFIG_VALUE_0="40")
        return result

    def prepare_base_history(self, worktree, clean_git, base):
        """Retain the base and its ancestors, excluding later image history."""
        original = worktree + '/.git'
        old_git = clean_git + '.unfiltered'
        committer = {'GIT_COMMITTER_NAME':'ASB workload preparation',
                     'GIT_COMMITTER_EMAIL':'asb-workload@localhost'}
        self.checked(['mkdir', clean_git])
        self.checked(['git', 'init', '--bare', clean_git])
        self.checked(['git', '--git-dir='+clean_git, 'fetch', '--no-tags',
                      '--no-recurse-submodules', '--no-write-fetch-head', worktree, base], timeout=7200)
        self.checked(['git', '--git-dir='+clean_git, 'cat-file', '-e', base+'^{commit}'])
        self.checked(['git', '--git-dir='+clean_git, 'config', 'core.bare', 'false'])
        self.checked(['git', '--git-dir='+clean_git, 'update-ref', 'refs/heads/asb-base', base],
                     environment=committer)
        self.checked(['git', '--git-dir='+clean_git, 'symbolic-ref', 'HEAD', 'refs/heads/asb-base'],
                     environment=committer)
        self.checked(['mv', original, old_git])
        self.checked(['mv', clean_git, original])
        self.checked(['git', '-C', worktree, 'reset', '--mixed', base],
                     environment=committer, timeout=300)
        # The disposable image's unrelated refs and unreachable objects must
        # not remain available to the benchmark agent through this copy.
        self.checked(['rm', '-rf', old_git], timeout=300)

    def capture_submission(self):
        if self.invocation is None:
            raise ExecutionError("task must be prepared before capturing a submission")
        git = ["git", f"--git-dir={self.submission_git_dir()}", f"--work-tree={SUBMISSION_WORKTREE}"]
        untracked = self.checked([*git, "ls-files", "--others", "--exclude-standard", "-z"], MAX_PATCH_BYTES)
        paths = [path.decode("utf-8") for path in untracked.split(b"\x00") if path]
        if len(paths) > 10000 or sum(len(path.encode()) for path in paths) > 512 * 1024:
            raise ExecutionError("submission contains too many untracked paths")
        if paths:
            self.checked([*git, "add", "--intent-to-add", "--", *paths])
        patch = self.checked([*git, "-c", "core.fsmonitor=false", "diff", "--binary", "--no-ext-diff",
                              "--no-textconv", self.row["base_commit"], "--"], MAX_PATCH_BYTES,
                             description="submission patch generation")
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
        # Material capture is also exercised with a minimally constructed task
        # in the offline contract tests.  Only a replay-entered task has this
        # optional cache identity; direct material capture must retain the
        # canonical full-identity path rather than assuming __init__ ran.
        if getattr(self, "verified_replay_material", None) is not None:
            # __enter__ already proved this writable workspace is clean at the
            # official base commit. The offline source tag and the prepared
            # baseline label are both bound to this cassette material, so
            # repeating full Git/reflog scans once per concurrent replica adds
            # no new identity coverage and can dominate ARM setup time.
            return {"schema_version": 1, "scope": "prepared-workload-material-v1",
                    "sha256": self.verified_replay_material}
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
                "reflog_sha256":reflog_digest(checked(prefix + ["reflog", "show", "--all",
                    "--format=%gD%x00%H%x00%gs%x00%gn%x00%ge%x00"], 4*1024**2)),
                "local_config_sha256":hashlib.sha256(checked(prefix + ["config", "--null", "--local", "--list"], 4*1024**2)).hexdigest()}
        before = git_state()
        if git_state() != before:
            raise ExecutionError("prepared repository changed during material capture")
        image_id = self.workspace.image_identity.get("image_id")
        if not isinstance(image_id, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", image_id):
            raise ExecutionError("prepared task image has no immutable identity")
        # Attempt ownership labels change a source-built image ID even when its
        # ordered rootfs layers and effective runtime configuration are equal.
        # Normalize only those verified labels; prebuilt images stay bound to
        # their original immutable ID. Actual image IDs remain in task evidence.
        source = self.invocation.get("environment_source")
        if isinstance(source, dict) and source.get("status") == "built":
            material_image = getattr(self, "material_source_image", None) or self.workspace.image
            material_image_id = getattr(self, "material_source_image_id", None) or image_id
            image = source_image_material(material_image, material_image_id, source,
                                          source.get("source_owner_labels", self.journal.labels))
            scope = "swe-bench-clean-git-worktree-and-source-image-v2"
            image_key = "image_material"
        else:
            image = getattr(self, "material_source_image_id", None) or image_id
            scope = "swe-bench-clean-git-worktree-and-image-v1"
            image_key = "image_id"
        common = {"baseline":self.invocation["baseline"],
                  "dataset":self.invocation["identity"]["dataset"],
                  "sdk":self.invocation["sdk"], "git":before}
        material = {"schema_version":1,"scope":"prepared-workload-material-v1",
                    "sha256":canonical_digest({"scope":scope, **common, image_key:image},
                                               "asb-prepared-workload-material-v1")}
        if image_key == "image_material":
            material["legacy_sha256"] = canonical_digest({
                "scope":"swe-bench-clean-git-worktree-and-image-v1", **common, "image_id":image_id},
                "asb-prepared-workload-material-v1")
            if os.environ.get("ASB_RETAIN_RECORDED_MATERIAL") == "1":
                retain_source_image(self.client, self.workspace.image, image_id, image, material["sha256"])
        return material

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
