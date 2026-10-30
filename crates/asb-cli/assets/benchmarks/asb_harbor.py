"""Pinned Terminal-Bench 2.x sessions using Harbor's environment and verifier.

No Harbor agent or model is instantiated. ASB's shared guest driver executes
the selected client, while the pinned SDK owns environment setup and grading.
"""
import argparse
import asyncio
import contextlib
import hashlib
import json
import logging
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid

from asb_docker import DockerSession, docker_command
from asb_execution import ExecutionError, ManagedDockerWorkspace
from asb_resources import ResourceJournal, build_image_tag
from asb_terminal_catalog import verify, tree_manifest, read_json, SPECS
from asb_harness import (quiet_sdk, restore_task_networks, original_networks,
                         write_json, record_grading_start, network_options)


# An SDK package alone is not a workload contract. Additional profiles need
# pinned catalog provenance, task-shape and reward/retained-grading contracts.
HARBOR_PROFILES = frozenset({"terminal-bench-2.0", "terminal-bench-2.1"})


def harbor_profile(workload):
    if not isinstance(workload, str) or workload not in HARBOR_PROFILES:
        raise ExecutionError("Harbor workload has no qualified task and grading profile")
    return workload


def verify_catalog(path, workload=None):
    """Resolve identity before SDK/environment setup; never trust catalog labels alone."""
    if workload is None:
        workload = read_json(path).get("workload")
    workload = harbor_profile(workload)
    return verify(path, workload)


class ImageUnavailable(ExecutionError):
    pass


def start_environment(environment, public, wait, budget):
    """Use the official source only when its prebuilt image cannot be obtained."""
    started = time.monotonic()
    try:
        wait(environment.start(force_build=False), budget)
        return "prebuilt" if environment._use_prebuilt else "original-source-build"
    except ImageUnavailable:
        if not environment._use_prebuilt or not (public / "Dockerfile").is_file():
            raise
        wait(environment._run_docker_compose_command(
            ["down", "--timeout", "2", "--volumes", "--remove-orphans"]), 120)
        remaining = budget - (time.monotonic() - started)
        if remaining <= 0:
            raise ExecutionError("official task image preparation exhausted its build time budget")
        wait(environment.start(force_build=True), remaining)
        return "original-source-build-fallback"


def scoped_configuration(config, attempt, artifacts, environment):
    """Keep official service semantics without exposing host directories/ports."""
    services = config.get("services", {})
    if "main" not in services or len(services) > 16:
        raise ExecutionError("official Harbor task has an invalid service population")
    for name, service in services.items():
        if (service.get("privileged") or service.get("network_mode") == "host"
                or service.get("pid") == "host" or service.get("ipc") == "host" or service.get("devices")):
            raise ExecutionError("task requires a separately isolated host for privileged/device/host-namespace access")
        service["container_name"] = attempt if name == "main" else attempt + "-" + name
        service["ports"] = []
        service.setdefault("labels", {}).update({"asb.owner":"asb", "asb.attempt":attempt})
        service.setdefault("pids_limit", 512)
        for volume in service.get("volumes", []):
            if volume.get("type") == "bind":
                source = Path(volume["source"]).resolve()
                if not (source.is_relative_to(artifacts.resolve()) or source.is_relative_to(environment.resolve())):
                    raise ExecutionError("Harbor task bind mount would expose data outside this attempt")
        build = service.get("build")
        if isinstance(build, dict):
            if build.get("privileged") or build.get("network") == "host" or build.get("entitlements") or build.get("ssh"):
                raise ExecutionError("task build requires separately isolated host capabilities")
            if "context" in build and not Path(build["context"]).resolve().is_relative_to(environment.resolve()):
                raise ExecutionError("Harbor build context would expose private task or host data")
    for category in ["networks", "volumes"]:
        for name, resource in config.get(category, {}).items():
            if resource.get("external"):
                raise ExecutionError("Harbor task requires an isolated mapping for an external resource")
            resource["name"] = attempt + "-" + name
            resource.setdefault("labels", {}).update({"asb.owner":"asb", "asb.attempt":attempt})
    return config


def environment_class(command, attempt, artifacts, journal=None):
    from harbor.environments.base import ExecResult
    from harbor.environments.docker.docker import DockerEnvironment
    from harbor.environments.docker.runtime import ContainerRuntime

    class OwnedHarborEnvironment(DockerEnvironment):
        @property
        def _main_image_name(self):
            # Keep the SDK's OS validation/cache identity aligned with the
            # attempt-owned main build tag published by the Compose adapter.
            return build_image_tag(attempt,"main")

        @classmethod
        def runtime(cls):
            return ContainerRuntime(engine=tuple(command), compose=tuple(command + ["compose"]))

        async def _run_docker_compose_command(self, arguments, check=True, timeout_sec=None,
                                             stdin_data=None, on_output=None):
            # sudo drops arbitrary inherited env. Pass only SDK infrastructure
            # and explicitly declared task env through a private Compose file.
            environment = artifacts / ("harbor-env-" + uuid.uuid4().hex)
            descriptor = os.open(environment, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(descriptor, "w") as output:
                output.write("".join(key + "=" + json.dumps(str(value).replace("$", "$$")) + "\n"
                    for key, value in self._compose_env_vars(include_os_env=False).items()))
            prefix = command + ["compose", "--env-file", str(environment), "-p", attempt,
                                "--project-directory", str(self.environment_dir)]
            for path in self._docker_compose_paths:
                prefix += ["-f", str(path)]
            def run():
                def invoke(argv, input_data=None):
                    return subprocess.run(argv, input=input_data, capture_output=True,
                        timeout=timeout_sec or self.task_env_config.build_timeout_sec or 600)
                expanded = invoke(prefix + ["config", "--format", "json"])
                if expanded.returncode:
                    return expanded
                if len(expanded.stdout) > 4 * 1024**2:
                    raise ExecutionError("expanded Harbor Compose definition exceeds 4 MiB")
                config = scoped_configuration(json.loads(expanded.stdout), attempt, artifacts, self.environment_dir)
                if journal is not None:
                    journal.compose(config)
                private = artifacts / "harbor-compose.json"
                write_json(private, config)
                return invoke(command + ["compose", "-p", attempt, "-f", str(private), *arguments], stdin_data)
            pending = asyncio.create_task(asyncio.to_thread(run))
            try:
                result = await asyncio.shield(pending)
            except asyncio.CancelledError:
                # Cancelling an asyncio waiter must not leave a Compose up or
                # build running in a detached worker during project teardown.
                await pending
                raise
            stdout, stderr = result.stdout.decode(errors="replace"), result.stderr.decode(errors="replace")
            if check and result.returncode:
                log = artifacts / ("harbor-compose-" + arguments[0] + ".log")
                log.write_text(stdout + stderr)
                log.chmod(0o600)
                if arguments[0] in ["up", "pull"] and any(reason in (stdout + stderr).lower() for reason in [
                        "pull access denied", "manifest unknown", "failed to resolve reference",
                        "failed to authorize", "no matching manifest", "toomanyrequests"]):
                    raise ImageUnavailable("official prebuilt task image is unavailable; original source build is needed")
                raise ExecutionError("official Harbor Compose " + arguments[0] + " failed; inspect private task artifacts")
            if on_output is not None:
                await on_output(stdout, "stdout")
                if stderr:
                    await on_output(stderr, "stderr")
            return ExecResult(stdout=stdout, stderr=stderr, return_code=result.returncode)

    return OwnedHarborEnvironment


def inspect_retained(retained):
    prefix = retained.selected_evaluation() or ""
    retained.select_grade(retained.object(prefix+"evaluation.json"))
    verify_retained(retained)


def retained_command(arguments, containers):
    """Translate SDK transport operations to immutable, verified container IDs.

    No Compose discovery/start/build/down is allowed during retained grading.
    The SDK still owns test upload, shell construction and reward parsing.
    """
    if not isinstance(arguments,list) or not arguments or any(not isinstance(arg,str) or "\x00" in arg for arg in arguments):
        raise ExecutionError("invalid retained Harbor transport arguments")
    if arguments[0] == "exec":
        options, index = [], 1
        while index < len(arguments) and arguments[index].startswith("-"):
            flag = arguments[index]
            index += 1
            if flag in ["-T", "--no-TTY"]:continue
            if flag not in ["-w", "--workdir", "-e", "--env", "-u", "--user"] or index >= len(arguments):
                raise ExecutionError("unsupported retained Harbor exec option")
            options.extend([flag,arguments[index]])
            index += 1
        if index >= len(arguments)-1 or arguments[index] not in containers:
            raise ExecutionError("retained Harbor exec does not target a verified service")
        return ["exec", "-i", *options, containers[arguments[index]], *arguments[index+1:]]
    if arguments[0] == "cp" and len(arguments) == 3:
        endpoints = []
        remote = 0
        for endpoint in arguments[1:]:
            service, separator, path = endpoint.partition(":")
            if separator:
                if service not in containers or not path.startswith("/"):
                    raise ExecutionError("retained Harbor copy does not target a verified service")
                endpoints.append(containers[service]+":"+path)
                remote += 1
            else:
                if not Path(endpoint).is_absolute():
                    raise ExecutionError("retained Harbor copy needs an explicit SDK path")
                endpoints.append(endpoint)
        if remote == 1:return ["cp",*endpoints]
    raise ExecutionError("retained Harbor transport cannot create, replace or remove environments")


async def retained_output(process, stdin_data):
    async def read(stream):
        chunks, size = [],0
        while chunk := await stream.read(65536):
            size += len(chunk)
            if size > 1024 * 1024:
                raise ExecutionError("retained Harbor transport output exceeds 1 MiB")
            chunks.append(chunk)
        return b"".join(chunks)
    async def write():
        if stdin_data:process.stdin.write(stdin_data)
        try:
            await process.stdin.drain()
        except (BrokenPipeError,ConnectionResetError):
            pass  # The SDK command's exit status remains authoritative.
        finally:
            process.stdin.close()
    pending = [asyncio.create_task(operation) for operation in [write(),read(process.stdout),read(process.stderr),process.wait()]]
    try:
        _,stdout,stderr,_ = await asyncio.gather(*pending)
        return stdout,stderr
    finally:
        for task in pending:
            if not task.done():task.cancel()
        await asyncio.gather(*pending,return_exceptions=True)


def retained_environment_class(command, resumed):
    from harbor.environments.base import ExecResult
    from harbor.environments.docker.docker import DockerEnvironment
    from harbor.environments.docker.runtime import ContainerRuntime
    containers = {"main":resumed.main.id}
    for container in resumed.containers:
        service = (container.attrs.get("Config",{}).get("Labels",{}) or {}).get("com.docker.compose.service")
        if service and container.id != resumed.main.id:
            if service in containers:raise ExecutionError("retained Harbor service identity is ambiguous")
            containers[service] = container.id

    class RetainedHarborEnvironment(DockerEnvironment):
        @classmethod
        def runtime(cls):
            return ContainerRuntime(engine=tuple(command), compose=tuple(command+["compose"]))

        @property
        def capabilities(self):
            # The original log mounts point at the *old* attempt directory.
            # Download current verifier outputs to the new evaluation directory
            # instead of pretending it was mounted into the existing container.
            return super().capabilities.model_copy(update={"mounted":False,"stream":False})

        async def start(self,*_args,**_kwargs):
            raise ExecutionError("retained Harbor environments must not be recreated")

        async def stop(self,*_args,**_kwargs):
            raise ExecutionError("retained Harbor resources are preserved by their original owner")

        async def _run_docker_compose_command(self, arguments, check=True, timeout_sec=None,
                                             stdin_data=None, on_output=None):
            argv = retained_command(arguments,containers)
            process = await asyncio.create_subprocess_exec(*command,*argv,stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,stderr=asyncio.subprocess.PIPE)
            try:
                stdout,stderr = await asyncio.wait_for(retained_output(process,stdin_data),timeout=min(timeout_sec or 1800,1800))
            except BaseException:
                if process.returncode is None:
                    with contextlib.suppress(ProcessLookupError):process.kill()
                # Drain the killed process's remaining pipe buffers: wait()
                # alone can deadlock while asyncio has paused a full pipe.
                await asyncio.wait_for(process.communicate(),timeout=10)
                raise
            stdout,stderr = stdout.decode(errors="replace"),stderr.decode(errors="replace")
            if check and process.returncode:
                raise ExecutionError("retained original Harbor verifier transport failed")
            if on_output is not None:
                if stdout:await on_output(stdout,"stdout")
                if stderr:await on_output(stderr,"stderr")
            return ExecResult(stdout=stdout,stderr=stderr,return_code=process.returncode)

        async def download_dir(self,source_dir,target_dir):
            # Reuse the SDK's transfer format, but do not fall back to its
            # project-wide container-name discovery if Docker cp fails.
            Path(target_dir).mkdir(parents=True,exist_ok=True)
            await self._run_docker_compose_command(["cp","main:"+source_dir+"/.",str(target_dir)])

    return RetainedHarborEnvironment


@quiet_sdk
def grade_retained(retained, catalog_path, cancelled):
    from asb_retained import require
    workload = harbor_profile(retained.identity["workload"])
    catalog_path = Path(catalog_path)
    if retained.selected_evaluation() is not None:
        inspect_retained(retained)
        return
    completed,started = [],False
    if retained.child("evaluation.json").exists():
        result = retained.object("evaluation.json")
        if result.get("outcome") in ["resolved","unresolved"]:
            inspect_retained(retained)
            return
        started = True
    for index,path in enumerate(retained.root.iterdir()):
        require(index < 256,"retained attempt has excessive top-level artifacts")
        if path.name.startswith("recovered-evaluation-"):
            import re
            require(re.fullmatch(r"recovered-evaluation-[a-f0-9]{32}",path.name)
                    and not path.is_symlink() and path.is_dir(),"retained evaluator directory is malformed")
            prefix = path.name+"/"
            if retained.child(prefix+"evaluation.json").exists():
                result = retained.object(prefix+"evaluation.json")
                if result.get("outcome") in ["resolved","unresolved"]:completed.append(path.name)
            started = started or retained.child(prefix+"evaluation-started.json").exists()
    require(len(completed) <= 1,"multiple completed retained Harbor grades cannot be guessed")
    if completed:
        prefix = completed[0]+"/"
        retained.select_grade(retained.object(prefix+"evaluation.json"))
        verify_retained(retained,prefix)
        retained.publish_selection(completed[0])
        return
    from asb_retained import reconstruct_verifier_return
    if reconstruct_verifier_return(retained,verify_retained,cancelled):return
    require(not started and not retained.child("evaluation-started.json").exists()
            and not any(retained.child("official/verifier/"+name).exists() for name in ["reward.json","reward.txt","test-stdout.txt"]),
            "the retained Harbor verifier already started; reconstruct its original outcome instead of rerunning side-effecting tests")
    require(retained.submission.get("grading_state") == "captured-before-verifier"
            and retained.recorded_cleanup == "stopped-preserved","retained Harbor submission has no preserved pre-verifier capture")
    require(isinstance(retained.task.get("original_networks"),dict),"retained Harbor task has no original network topology")
    networks = network_options(retained.task["original_networks"])
    catalog = verify_catalog(catalog_path,workload)
    selected = next((item for item in catalog["tasks"] if item["id"] == retained.identity["task_id"]),None)
    require(selected is not None and catalog["dataset"] == retained.identity["dataset"]
            and catalog["sdk"] == retained.task.get("sdk"),"retained Harbor task differs from its installed grading catalog")
    definition = catalog_path.parent/selected["definition"]
    digest = hashlib.sha256(json.dumps(tree_manifest(definition),sort_keys=True).encode()).hexdigest()
    require(retained.task["baseline"] == {"kind":"upstream-environment-definition",
        "source_revision":catalog["dataset"]["revision"],"definition_sha256":digest},"retained Harbor task definition changed")
    from asb_terminal_catalog import runtime_identity
    from asb_resources import RetainedEnvironment
    from harbor.models.task.task import Task
    from harbor.models.task.config import TaskOS
    from harbor.models.trial.paths import TrialPaths
    runtime_identity(workload)
    task = Task(definition)
    require(not task.has_steps and task.config.environment.os == TaskOS.LINUX,"retained original Harbor task requires its single-step Linux definition")
    require(not cancelled.is_set(),"retained Harbor grading cancelled before environment restart")
    directory = "recovered-evaluation-"+uuid.uuid4().hex
    artifacts = retained.child(directory)
    artifacts.mkdir(mode=0o700)
    paths = TrialPaths(artifacts/"official")
    paths.mkdir()
    paths.chmod_dir()
    loop = asyncio.new_event_loop()
    try:
        with DockerSession() as client, RetainedEnvironment(client,retained,artifacts,cancelled,
                before_start=lambda main:restore_task_networks(client,main,networks)) as resumed:
            environment = retained_environment_class(docker_command(),resumed)(
                environment_dir=definition/"environment",environment_name=task.short_name,
                session_id=retained.identity["attempt_id"],trial_paths=paths,task_env_config=task.config.environment,
                logger=logging.getLogger("asb.retained-harbor"))
            grader = HarborTask.__new__(HarborTask)
            grader.artifacts,grader.task_id,grader.attempt = artifacts,retained.identity["task_id"],retained.identity["attempt_id"]
            grader.invocation,grader.catalog = {"identity":retained.identity},catalog
            grader.task,grader.paths,grader.environment,grader.loop = task,paths,environment,loop
            require(not cancelled.is_set(),"retained Harbor grading cancelled before verifier launch")
            result = grader.grade_environment()
            require(not cancelled.is_set(),"retained Harbor grading cancelled; no outcome was selected")
    finally:
        loop.close()
    retained.select_grade(result)
    verify_retained(retained,directory+"/")
    retained.publish_selection(directory)


def verify_retained(retained, prefix=None, result=None):
    from asb_retained import require
    workload = harbor_profile(retained.identity["workload"])
    retained.verify_environment_baseline()
    prefix = (retained.selected_evaluation() or "") if prefix is None else prefix
    result = retained.object(prefix+"evaluation.json") if result is None else result
    retained.grade(result)
    spec = SPECS[workload]
    require(retained.task.get("sdk") == {"package":spec["package"],"version":spec["version"]},
            "retained original Harbor SDK differs")
    rewards = result.get("rewards")
    require(isinstance(rewards,dict) and set(rewards) == {"reward"}
            and type(rewards["reward"]) in [int,float] and rewards["reward"] in [0,1]
            and result["resolved"] == (rewards["reward"] == 1), "retained original Harbor reward disagrees")
    encoded = retained.child(prefix+"official/verifier/reward.json")
    # Match the pinned SDK: JSON takes precedence when both outputs exist.
    reward = retained.object(prefix+"official/verifier/reward.json").get("reward") if encoded.is_file() else float(retained.bytes(prefix+"official/verifier/reward.txt").decode().strip())
    require(type(reward) in [int,float] and reward == rewards["reward"], "retained reward file differs from its report")
    if retained.child(prefix+"verifier-return.json").exists():
        from asb_retained import verifier_return
        observation = verifier_return(retained,prefix)
        require(set(observation["files"]) == {"evaluation-started.json","official/verifier/reward.json","official/verifier/reward.txt"}
                and observation["result"] == {key:value for key,value in result.items() if key != "reconstruction"},
                "original Harbor SDK return disagrees with its binary reward")
    if "reconstruction" in result:
        from asb_retained import verify_verifier_reconstruction
        verify_verifier_reconstruction(retained,prefix,result)
    elif prefix:retained.verify_grading_cleanup(prefix)


class HarborTask:
    def initial_material(self, binary=None):
        from asb_material import MaterialError, capture_task
        try:
            return capture_task(self, {"baseline":self.invocation["baseline"],
                "dataset":self.invocation["identity"]["dataset"],"sdk":self.invocation["sdk"]}, binary=binary)
        except MaterialError as error:
            raise ExecutionError(str(error)) from None
        except Exception as error:
            raise ExecutionError("prepared material transport failed: " + type(error).__name__) from None

    def __init__(self, client, catalog, task, artifacts, workload=None):
        self.workload = harbor_profile(workload) if workload is not None else None
        self.client, self.catalog_path, self.task_id = client, Path(catalog), task
        self.artifacts = Path(artifacts).absolute()
        self.attempt = "asb-" + uuid.uuid4().hex
        self.workspace, self.environment, self.invocation = None, None, None
        self.cancelled = None
        self.interrupted = False
        self.loop = asyncio.new_event_loop()
        self.journal = None

    def wait(self, operation, timeout):
        pending = self.loop.create_task(asyncio.wait_for(operation, timeout=timeout))
        try:
            return self.loop.run_until_complete(pending)
        except BaseException:
            if not pending.done():
                pending.cancel()
                # A synchronous termination signal must drain the SDK's
                # cancellation path before any Compose teardown starts.
                with contextlib.suppress(BaseException):
                    self.loop.run_until_complete(pending)
            raise

    @quiet_sdk
    def __enter__(self):
        self.catalog = verify_catalog(self.catalog_path, self.workload)
        self.workload = self.catalog["workload"]
        os.environ.setdefault("LITELLM_LOCAL_MODEL_COST_MAP", "True")
        from harbor.models.task.task import Task
        from harbor.models.trial.paths import TrialPaths
        from harbor.models.task.config import TaskOS
        selected = next((item for item in self.catalog["tasks"] if item["id"] == self.task_id), None)
        if selected is None:
            raise ExecutionError("selected Harbor profile task is not installed")
        self.definition = self.catalog_path.parent / selected["definition"]
        self.task = Task(self.definition)
        if self.task.has_steps or self.task.config.environment.os != TaskOS.LINUX:
            raise ExecutionError("this original dataset session expects its single-step Linux task definition")
        self.artifacts.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.journal = ResourceJournal(self.client, self.artifacts, self.attempt)
        self.paths = TrialPaths(self.artifacts / "official")
        self.paths.mkdir()
        self.paths.chmod_dir()
        # Only environment inputs go into build contexts. Reference solution,
        # instruction and verifier directories stay outside this copied tree.
        public = self.artifacts / "environment"
        shutil.copytree(self.task.paths.environment_dir, public)
        mounts = [{"type":"bind", "source":str(path), "target":"/logs/" + name}
                  for name, path in [("agent",self.paths.agent_dir), ("verifier",self.paths.verifier_dir),
                                     ("artifacts",self.paths.artifacts_dir)]]
        owned = environment_class(docker_command(), self.attempt, self.artifacts, self.journal)
        self.previous_tempdir = tempfile.tempdir
        sdk_temp = self.artifacts / "sdk-tmp"
        sdk_temp.mkdir(mode=0o700)
        # This controller is a separate process. Do not put the SDK's Compose
        # overlays on a potentially small host /tmp filesystem.
        tempfile.tempdir = str(sdk_temp)
        try:
            self.environment = owned(environment_dir=public, environment_name=self.task.short_name,
                session_id=self.attempt, trial_paths=self.paths, task_env_config=self.task.config.environment,
                mounts=mounts, logger=logging.getLogger("asb.original-harbor"))
            self.environment_source = start_environment(self.environment,public,self.wait,
                self.task.config.environment.build_timeout_sec + 120)
            containers = self.client.containers.list(filters={"label":["asb.attempt=" + self.attempt,
                "com.docker.compose.service=main"]})
            if len(containers) != 1:
                raise ExecutionError("official Harbor environment did not create exactly one owned main service")
            container = containers[0]
            self.original_networks = original_networks(container)
            self.workspace = ManagedDockerWorkspace.attach(self.client, container, self.attempt,
                self.task.config.environment.workdir or container.attrs["Config"].get("WorkingDir") or "/")
            self.agent_timeout = self.task.config.agent.timeout_sec
            definition_sha = hashlib.sha256(json.dumps(tree_manifest(self.definition), sort_keys=True).encode()).hexdigest()
            self.invocation = {"identity":{"workload":self.workload, "task_id":self.task_id,
                "attempt_id":self.attempt,"dataset":self.catalog["dataset"]}, "prompt":self.task.instruction,
                "workspace":self.workspace.workdir, "baseline":{"kind":"upstream-environment-definition",
                "source_revision":self.catalog["dataset"]["revision"], "definition_sha256":definition_sha},
                "sdk":self.catalog["sdk"], "image":self.workspace.image_identity,"resources":self.workspace.limits,
                "agent_timeout_seconds":self.agent_timeout,"environment_source":self.environment_source,
                "original_networks":self.original_networks}
            write_json(self.artifacts / "task.json", self.invocation)
            return self
        except BaseException:
            self.interrupted = True
            self.__exit__(None, None, None)
            raise

    def checked(self, argv, output_limit=256 * 1024):
        result = self.workspace.execute(argv, timeout=60, output_limit=output_limit, cancelled=self.cancelled)
        if result["exit_code"] != 0 or result["timed_out"] or result["cancelled"] or result["output_truncated"]:
            raise ExecutionError("task environment operation failed; inspect task artifacts")
        return result["stdout"]

    def capture_submission(self):
        result = {"task":self.invocation["identity"], "kind":"in-place-environment",
                  "grading_state":"captured-before-verifier",
                  "container_id":self.workspace.container.id,"image":self.workspace.image_identity}
        write_json(self.artifacts / "submission.json", result)
        return result

    @quiet_sdk
    def evaluate(self, timeout=1800):
        self.capture_submission()
        restore_task_networks(self.client, self.workspace.container, self.original_networks)
        return self.grade_environment(timeout)

    @quiet_sdk
    def grade_environment(self, timeout=1800):
        from harbor.verifier.verifier import Verifier
        identity = self.invocation["identity"] if self.invocation else {
            "workload":harbor_profile(self.catalog["workload"]),"task_id":self.task_id,"attempt_id":self.attempt,"dataset":self.catalog["dataset"]}
        record_grading_start(self.artifacts,identity)
        # Reward files are outputs of this verifier invocation, not a cache or
        # something left by a client before the official test script ran.
        for path in [self.paths.reward_text_path, self.paths.reward_json_path]:
            if path.is_symlink() or path.is_file():
                path.unlink()
        self.environment.default_user = self.task.config.verifier.user
        try:
            value = self.wait(Verifier(self.task, self.paths, self.environment).verify(),
                              min(timeout, self.task.config.verifier.timeout_sec))
            rewards = value.rewards
            if (not rewards or set(rewards) != {"reward"}
                    or type(rewards["reward"]) not in [int, float] or rewards["reward"] not in [0, 1]):
                raise ExecutionError("original Terminal-Bench verifier returned an invalid binary reward")
            resolved = rewards["reward"] == 1
            result = {"resolved":resolved, "outcome":"resolved" if resolved else "unresolved", "rewards":rewards}
        except Exception as error:
            # SDK exceptions may include command output; retain only the class
            # on the protocol. Original verifier artifacts remain private.
            result = {"resolved":False, "outcome":"evaluation_error", "error_class":type(error).__name__}
        result.update(schema_version=1,workload=identity["workload"],task_id=self.task_id,
                      dataset=self.catalog["dataset"],sdk=self.catalog["sdk"],artifact="evaluation.json")
        if result["outcome"] in ["resolved","unresolved"]:
            from asb_retained import record_verifier_return
            record_verifier_return(self.artifacts,identity,result,["evaluation-started.json",
                "official/verifier/reward.json","official/verifier/reward.txt"])
        write_json(self.artifacts / "evaluation.json", result)
        return result

    @quiet_sdk
    def __exit__(self, *_exception):
        self.interrupted = self.interrupted or bool(_exception and _exception[0] is not None)
        status = "not_created"
        try:
            if self.interrupted and self.journal is not None:
                # Compose can allocate a project network before failing to
                # create the first service.  There is no agent-visible state
                # to preserve until the main workspace and invocation exist;
                # keeping that empty network would eventually exhaust
                # Docker's default address pools across failed starts.
                if self.environment is not None and self.workspace is None and self.invocation is None:
                    status = "cleanup_failed"
                    self.wait(self.environment._run_docker_compose_command(
                        ["down", "--timeout", "2", "--volumes", "--remove-orphans"]), 120)
                    if self.client.containers.list(all=True, filters={"label":"com.docker.compose.project=" + self.attempt}):
                        raise ExecutionError("official Harbor startup containers could not be completely removed")
                    self.journal.cleanup_images(self.client)
                    status = "removed-before-agent"
                else:
                    status = "preservation_failed"
                    self.journal.preserve(self.client)
                    status = "stopped-preserved"
            elif self.environment is not None:
                status = "cleanup_failed"
                # Do not use SDK stop(delete=True): it removes shared local
                # build images. Remove only this unique Compose project.
                self.wait(self.environment.prepare_logs_for_host(), 60)
                self.wait(self.environment._run_docker_compose_command(
                    ["down", "--timeout", "2", "--volumes", "--remove-orphans"]), 120)
                if self.client.containers.list(all=True, filters={"label":"com.docker.compose.project=" + self.attempt}):
                    raise ExecutionError("official Harbor containers could not be completely removed")
                if self.journal is not None:
                    self.journal.cleanup_images(self.client)
                status = "removed"
                if self.workspace:
                    self.workspace.container = None
        finally:
            self.cleanup_status = status
            if hasattr(self,"previous_tempdir"):
                tempfile.tempdir = self.previous_tempdir
            if self.workspace:
                self.workspace.cleanup_status = status
            if self.artifacts.is_dir():
                write_json(self.artifacts / "task-cleanup.json", {"attempt_id":self.attempt,"status":status})
            self.loop.close()
            if self.journal is not None:
                self.journal.close()
            if self.workspace:
                self.workspace.join_held_executions()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--catalog", required=True, type=Path)
    parser.add_argument("--workload", choices=sorted(HARBOR_PROFILES),
                        help="explicit qualified profile; otherwise use the validated catalog identity")
    parser.add_argument("--task", required=True)
    parser.add_argument("--artifacts", required=True, type=Path)
    args = parser.parse_args()
    terminating = False
    def cancelled(_signal, _frame):
        nonlocal terminating
        if not terminating:
            terminating = True
            raise InterruptedError("ASB Harbor session was cancelled")
    signal.signal(signal.SIGTERM, cancelled)
    from asb_task_server import serve
    with DockerSession() as client:
        return serve(HarborTask(client,args.catalog,args.task,args.artifacts,args.workload),sys.stdin,sys.stdout)


if __name__ == "__main__":
    raise SystemExit(main())
