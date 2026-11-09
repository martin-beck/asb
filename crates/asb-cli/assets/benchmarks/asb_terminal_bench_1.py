"""Original Terminal-Bench environment/test/parser adapter, never a new grader.

The installed 0.2.18 SDK owns task parsing, Compose environment construction,
test-script injection, tmux execution and result parsing. ASB retains agent/
provider selection through the generic task controller and Rust adapters.
"""
import argparse
import hashlib
import importlib.metadata
import json
import logging
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import uuid

from asb_docker import DockerSession, docker_command
from asb_execution import (ExecutionError, ManagedDockerWorkspace, require_native_compose_service,
                           require_native_docker_host, requested_cpu_set, apply_compose_cpu_set,
                           verify_compose_cpu_set)
from asb_resources import ResourceJournal
from asb_terminal_catalog import verify, tree_manifest
from asb_harness import (quiet_sdk, restore_task_networks, original_networks,
                         write_json, record_grading_start, network_options)


def write_compose_environment(path, environment):
    """Only SDK task interpolation variables cross the elevated CLI boundary."""
    values = "".join(key + "=" + json.dumps(str(value).replace("$", "$$")) + "\n"
                     for key, value in environment.items() if key.startswith("T_BENCH_"))
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w") as output:
        output.write(values)


def copy_task_inputs(source, target, definition):
    """Do not put reference solutions or verifier scripts in a build/bind input."""
    private = {"tests", "solution.sh", "solution.yaml", "task.yaml", "run-tests.sh"}
    def exclude(directory, names):
        return private.intersection(names) if Path(directory).resolve() == definition.resolve() else set()
    if source.is_dir():
        shutil.copytree(source, target, dirs_exist_ok=True, ignore=exclude)
    else:
        if source.parent.resolve() == definition.resolve() and source.name in private:
            raise ExecutionError("task input would expose reference solution or verifier data")
        shutil.copy2(source, target)


def inspect_retained(retained):
    prefix = retained.selected_evaluation() or ""
    retained.select_grade(retained.object(prefix+"evaluation.json"))
    verify_retained(retained)


def verify_retained(retained, prefix=None, result=None):
    from asb_retained import require
    retained.verify_environment_baseline()
    prefix = (retained.selected_evaluation() or "") if prefix is None else prefix
    result = retained.object(prefix+"evaluation.json") if result is None else result
    retained.grade(result)
    require(retained.task.get("sdk") == {"package":"terminal-bench","version":"0.2.18"},
            "retained original Terminal-Bench SDK differs")
    tests = result.get("test_results")
    require(isinstance(tests,dict) and tests and len(tests) <= 10000
            and result.get("failure_mode") == "none" and result.get("parse_failure") == "none"
            and result["resolved"] == all(status == "passed" for status in tests.values()),
            "retained original Terminal-Bench parsed tests disagree")
    pane = prefix+"official/" + retained.identity["task_id"] + "/" + retained.identity["attempt_id"] + "/panes/post-test.txt"
    require(bool(retained.bytes(pane)), "retained original verifier pane is missing")
    if retained.child(prefix+"verifier-return.json").exists():
        from asb_retained import verifier_return
        observation = verifier_return(retained,prefix)
        require(set(observation["files"]) == {"evaluation-started.json",pane.removeprefix(prefix)}
                and observation["result"] == {key:value for key,value in result.items() if key != "reconstruction"},
                "original Terminal SDK return disagrees with its parsed result")
    if "reconstruction" in result:
        from asb_retained import verify_verifier_reconstruction
        verify_verifier_reconstruction(retained,prefix,result)
    elif prefix:
        retained.verify_grading_cleanup(prefix)


@quiet_sdk
def grade_retained(retained, catalog_path, cancelled):
    from asb_retained import require
    catalog_path = Path(catalog_path)
    if retained.selected_evaluation() is not None:
        inspect_retained(retained)
        return
    completed = []
    started = False
    for index,path in enumerate(retained.root.iterdir()):
        require(index < 256, "retained attempt has excessive top-level artifacts")
        if path.name == "evaluation.json":
            result = retained.object(path.name)
            if result.get("outcome") in ["resolved","unresolved"]:
                inspect_retained(retained)
                return
            started = True
        if path.name.startswith("recovered-evaluation-"):
            import re
            require(re.fullmatch(r"recovered-evaluation-[a-f0-9]{32}",path.name)
                    and not path.is_symlink() and path.is_dir(), "retained evaluator directory is malformed")
            result_path = retained.child(path.name+"/evaluation.json")
            if result_path.exists():
                result = retained.object(path.name+"/evaluation.json")
                if result.get("outcome") in ["resolved","unresolved"]:completed.append(path.name)
            started = started or retained.child(path.name+"/evaluation-started.json").exists()
    require(len(completed) <= 1, "multiple completed retained grades cannot be replaced or guessed")
    if completed:
        prefix = completed[0]+"/"
        retained.select_grade(retained.object(prefix+"evaluation.json"))
        verify_retained(retained,prefix)
        retained.publish_selection(completed[0])
        return
    from asb_retained import reconstruct_verifier_return
    if reconstruct_verifier_return(retained,verify_retained,cancelled):return
    pane = "official/"+retained.identity["task_id"]+"/"+retained.identity["attempt_id"]+"/panes/post-test.txt"
    require(not started and not retained.child("evaluation-started.json").exists() and not retained.child(pane).exists(),
            "the retained verifier already started; reconstruct its original outcome instead of rerunning side-effecting tests")
    require(retained.submission.get("grading_state") == "captured-before-verifier",
            "retained submission has no pre-verifier capture record; no tests or agent were rerun")
    require(retained.recorded_cleanup == "stopped-preserved", "retained environment was not preserved for grading")
    require(isinstance(retained.task.get("original_networks"),dict),
            "retained task has no original network topology; no environment or verifier was restarted")
    networks = network_options(retained.task.get("original_networks"))
    catalog = verify(catalog_path,"terminal-bench-1.0")
    selected = next((task for task in catalog["tasks"] if task["id"] == retained.identity["task_id"]),None)
    require(selected is not None and catalog["dataset"] == retained.identity["dataset"]
            and catalog["sdk"] == retained.task.get("sdk"), "retained task differs from its installed grading catalog")
    definition = Path(catalog_path).parent/selected["definition"]
    definition_sha = hashlib.sha256(json.dumps(tree_manifest(definition),sort_keys=True).encode()).hexdigest()
    require(retained.task["baseline"] == {"kind":"upstream-environment-definition",
            "source_revision":catalog["dataset"]["revision"],"definition_sha256":definition_sha},
            "retained original task definition changed")
    from asb_terminal_catalog import runtime_identity
    runtime_identity("terminal-bench-1.0")
    os.environ.setdefault("LITELLM_LOCAL_MODEL_COST_MAP","True")
    from terminal_bench.handlers.trial_handler import TrialHandler, TrialPaths
    from terminal_bench.harness.harness import Harness
    from terminal_bench.terminal.terminal import Terminal
    from terminal_bench.terminal.docker_compose_manager import DockerComposeManager
    from asb_resources import RetainedEnvironment
    handler = TrialHandler(retained.identity["attempt_id"],definition)
    require(handler.task_id == retained.identity["task_id"], "retained task definition ID differs")
    require(not handler.task.run_tests_in_same_shell,
            "this original task requires its lost agent shell; a new shell cannot truthfully grade the retained attempt")
    require(not cancelled.is_set(), "retained grading cancelled before environment restart")
    directory = "recovered-evaluation-"+uuid.uuid4().hex
    artifacts = retained.child(directory)
    artifacts.mkdir(mode=0o700)
    handler.trial_paths = TrialPaths(artifacts/"official",handler.task_id,handler.trial_name)
    handler.trial_paths.mkdir()
    with DockerSession() as client, RetainedEnvironment(client,retained,artifacts,cancelled,
            before_start=lambda main:restore_task_networks(client,main,networks)) as resumed:
        require(not cancelled.is_set(), "retained grading cancelled before verifier launch")
        terminal = Terminal.__new__(Terminal)
        terminal.container = resumed.main
        terminal._sessions = {}
        terminal._commands_path = handler.trial_paths.commands_path
        terminal._disable_recording = True
        terminal._compose_manager = DockerComposeManager.__new__(DockerComposeManager)
        terminal._compose_manager._client_container = resumed.main
        task = TerminalBenchTask.__new__(TerminalBenchTask)
        task.artifacts, task.task_id, task.attempt = artifacts,handler.task_id,handler.trial_name
        task.catalog, task.handler, task.terminal = catalog,handler,terminal
        task.test_session_name = "asb-grade-"+uuid.uuid4().hex
        task.evaluator = Harness.__new__(Harness)
        task.evaluator._global_test_timeout_sec = min(1800,handler.task.max_test_timeout_sec)
        task.evaluator._global_timeout_multiplier = 1.0
        task.evaluator._logger = logging.getLogger("asb.retained-terminal-bench")
        result = task.grade_environment()
        require(not cancelled.is_set(), "retained grading cancelled; no outcome was selected")
    retained.select_grade(result)
    verify_retained(retained,directory+"/")
    retained.publish_selection(directory)


class TerminalBenchTask:
    def initial_material(self, binary=None):
        from asb_material import MaterialError, capture_task
        try:
            return capture_task(self, {"baseline":self.invocation["baseline"],
                "dataset":self.invocation["identity"]["dataset"],"sdk":self.invocation["sdk"]}, binary=binary)
        except MaterialError as error:
            raise ExecutionError(str(error)) from None
        except Exception as error:
            raise ExecutionError("prepared material transport failed: " + type(error).__name__) from None

    def __init__(self, client, catalog, task, artifacts):
        self.cpu_set = requested_cpu_set()
        self.client, self.catalog_path, self.task_id = client, Path(catalog), task
        self.artifacts = Path(artifacts).absolute()
        self.attempt = "asb-" + uuid.uuid4().hex
        self.workspace, self.terminal, self.invocation = None, None, None
        self.agent_session = None
        self.agent_launcher = None
        self.cancelled = None
        self.interrupted = False
        self.journal = None

    def prepare_agent_session(self):
        """Create an SDK shell only when the task actually requires sharing it."""
        if self.handler.task.run_tests_in_same_shell:
            self.agent_session = self.terminal.create_session("agent", as_configured_user=True)

    def prepare_agent_transport(self, request):
        """Activate the original SDK shell after material capture/bootstrap."""
        if not self.handler.task.run_tests_in_same_shell:
            return None
        if self.agent_launcher is not None:
            raise ExecutionError("this task's original agent shell was already initialized")
        from asb_terminal_session import SDKSessionLauncher
        self.prepare_agent_session()
        self.agent_launcher = SDKSessionLauncher(self.agent_session,request["binary"],self.artifacts,
            require_host_target=request.get("measure_target",False), preparation_deadline=request.get("preparation_deadline"),
            on_preparation_failure=request.get("on_preparation_failure"))
        return self.agent_launcher

    @quiet_sdk
    def __enter__(self):
        require_native_docker_host(self.client)
        os.environ.setdefault("LITELLM_LOCAL_MODEL_COST_MAP", "True")
        import docker
        import terminal_bench.terminal.terminal as terminal_module
        from terminal_bench.terminal.docker_compose_manager import DockerComposeManager
        from terminal_bench.handlers.trial_handler import TrialHandler
        from terminal_bench.harness.harness import Harness
        self.catalog = verify(self.catalog_path, "terminal-bench-1.0")
        selected = next((task for task in self.catalog["tasks"] if task["id"] == self.task_id), None)
        if selected is None:
            raise ExecutionError("selected original Terminal-Bench task is not installed")
        self.definition = self.catalog_path.parent / selected["definition"]
        self.artifacts.mkdir(mode=0o700, parents=True, exist_ok=False)
        self.journal = ResourceJournal(self.client, self.artifacts, self.attempt)
        self.handler = TrialHandler(self.attempt, self.definition, self.artifacts / "official")
        self.agent_timeout = self.handler.task.max_agent_timeout_sec
        # Only these SDK test/parser operations are used. No SDK default agent,
        # provider, model, global run lock or publication path is initialized.
        self.evaluator = Harness.__new__(Harness)
        self.evaluator._global_test_timeout_sec = None
        self.evaluator._global_timeout_multiplier = 1.0
        self.evaluator._logger = logging.getLogger("asb.original-terminal-bench")
        command, attempt, artifacts = docker_command(), self.attempt, self.artifacts
        definition = self.definition
        journal = self.journal
        cpu_set = self.cpu_set

        class ScopedCompose(DockerComposeManager):
            def get_docker_compose_command(manager, arguments):
                path = getattr(manager, "asb_config", manager._docker_compose_path)
                environment = artifacts / "compose.env"
                if not environment.exists():
                    write_compose_environment(environment, manager.env)
                # sudo intentionally does not preserve arbitrary host env. Give
                # Compose only the SDK's declared task variables through its
                # own private env-file, never provider credentials or a shell.
                return command + ["compose", "--env-file", str(environment), "-p", attempt, "-f", str(path), *arguments]

            def _run_docker_compose_command(manager, arguments):
                try:
                    return subprocess.run(manager.get_docker_compose_command(arguments), env=manager.env,
                        stdin=subprocess.DEVNULL, capture_output=True, text=True, check=True,
                        timeout=900 if arguments[0] == "build" else 120)
                except subprocess.CalledProcessError as error:
                    log = artifacts / ("compose-" + arguments[0] + ".log")
                    log.write_text((error.stdout or "") + (error.stderr or ""))
                    log.chmod(0o600)
                    raise ExecutionError("official task Compose " + arguments[0] + " failed; inspect private Compose artifacts") from error

            def start(manager):
                expanded = manager._run_docker_compose_command(["config", "--format", "json"])
                config = json.loads(expanded.stdout)
                services = config.get("services", {})
                if not services or len(services) > 16:
                    raise ExecutionError("official task declares an invalid number of services")
                for name, service in services.items():
                    require_native_compose_service(service)
                    apply_compose_cpu_set(service, cpu_set)
                    if (service.get("privileged") or service.get("network_mode") == "host"
                            or service.get("pid") == "host" or service.get("ipc") == "host" or service.get("devices")):
                        raise ExecutionError("this task needs a separately isolated host for privileged/device/host-namespace access")
                    service["container_name"] = attempt if name == "client" else attempt + "-" + name
                    service["ports"] = []  # Internal service ports stay unchanged.
                    service.setdefault("labels", {}).update({"asb.owner": "asb", "asb.attempt": attempt})
                    service.setdefault("mem_limit", "4g")
                    service.setdefault("cpus", 2)
                    service.setdefault("pids_limit", 512)
                    build = service.get("build")
                    if isinstance(build, dict) and "context" in build:
                        context = Path(build["context"]).resolve()
                        if not context.is_relative_to(definition.resolve()):
                            raise ExecutionError("task needs an isolated mapping for a shared build context")
                        if build.get("privileged") or build.get("network") == "host" or build.get("entitlements") or build.get("ssh"):
                            raise ExecutionError("task build requires separately isolated host capabilities")
                        target = artifacts / "build-inputs" / hashlib.sha256(name.encode()).hexdigest()
                        target.parent.mkdir(parents=True, exist_ok=True)
                        copy_task_inputs(context, target, definition)
                        build["context"] = str(target)
                    for volume in service.get("volumes", []):
                        if volume.get("type") != "bind":
                            continue
                        source = Path(volume["source"]).resolve()
                        if source.is_relative_to(artifacts):
                            continue
                        if not source.is_relative_to(definition.resolve()):
                            raise ExecutionError("task bind mount would expose data outside the task definition")
                        target = artifacts / "inputs" / hashlib.sha256(str(source).encode()).hexdigest()
                        target.parent.mkdir(parents=True, exist_ok=True)
                        copy_task_inputs(source, target, definition)
                        volume["source"] = str(target)
                for name, network in config.get("networks", {}).items():
                    if network.get("external"):
                        raise ExecutionError("task requires an external network; an isolated task-local mapping is needed")
                    network["name"] = attempt + "-" + name
                    network.setdefault("labels", {}).update({"asb.owner": "asb", "asb.attempt": attempt})
                for name, volume in config.get("volumes", {}).items():
                    if volume.get("external"):
                        raise ExecutionError("task requires an external volume; an isolated task-local mapping is needed")
                    volume["name"] = attempt + "-" + name
                    volume.setdefault("labels", {}).update({"asb.owner": "asb", "asb.attempt": attempt})
                manager.asb_config = artifacts / "compose.json"
                journal.compose(config)
                write_json(manager.asb_config, config)
                return super().start()

        self.original_client, self.original_manager = docker.from_env, terminal_module.DockerComposeManager
        docker.from_env = lambda **_kwargs: self.client
        terminal_module.DockerComposeManager = ScopedCompose
        try:
            paths = self.handler.trial_paths
            image = "asb-terminal:" + self.catalog["source_sha256"][:24] + "-" + self.task_id
            self.terminal = terminal_module.Terminal(client_container_name=self.attempt, client_image_name=image,
                docker_image_name_prefix="asb-terminal-" + self.catalog["source_sha256"][:24],
                docker_compose_path=self.handler.task_paths.docker_compose_path, no_rebuild=False, cleanup=False,
                sessions_logs_path=paths.sessions_path, agent_logs_path=paths.agent_logging_dir,
                commands_path=paths.commands_path, disable_recording=self.handler.task.disable_asciinema)
            self.terminal.start()
            verify_compose_cpu_set(self.client, self.attempt, self.cpu_set)
            container = self.terminal.container
            self.original_networks = original_networks(container)
            self.workspace = ManagedDockerWorkspace.attach(self.client, container, self.attempt,
                container.attrs["Config"].get("WorkingDir") or "/", cpu_set=self.cpu_set)
            definition_sha = hashlib.sha256(json.dumps(tree_manifest(self.definition), sort_keys=True).encode()).hexdigest()
            self.invocation = {"identity":{"workload":"terminal-bench-1.0", "task_id":self.task_id,
                "attempt_id":self.attempt, "dataset":self.catalog["dataset"]}, "prompt":self.handler.instruction,
                "workspace":self.workspace.workdir, "baseline":{"kind":"upstream-environment-definition",
                "source_revision":self.catalog["dataset"]["revision"], "definition_sha256":definition_sha},
                "sdk":self.catalog["sdk"], "image":self.workspace.image_identity, "resources":self.workspace.limits,
                "agent_timeout_seconds":self.agent_timeout,"original_networks":self.original_networks}
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
                  "container_id":self.workspace.container.id, "image":self.workspace.image_identity}
        write_json(self.artifacts / "submission.json", result)
        return result

    @quiet_sdk
    def evaluate(self, timeout=1800):
        self.capture_submission()
        # A qualification-only agent network override must not change the
        # original verifier's service connectivity or dependency downloads.
        restore_task_networks(self.client, self.workspace.container, self.original_networks)
        return self.grade_environment(timeout)

    @quiet_sdk
    def grade_environment(self, timeout=1800):
        from terminal_bench.agents.failure_mode import FailureMode
        identity = self.invocation["identity"] if getattr(self,"invocation",None) else {
            "workload":"terminal-bench-1.0","task_id":self.task_id,"attempt_id":self.attempt,"dataset":self.catalog["dataset"]}
        if self.handler.task.run_tests_in_same_shell and (
                self.agent_session is None or not self.agent_session.is_session_alive()):
            raise ExecutionError("original same-shell agent session is missing; a new grader shell cannot replace it")
        record_grading_start(self.artifacts,identity)
        session = self.agent_session if self.handler.task.run_tests_in_same_shell else self.terminal.create_session(getattr(self,"test_session_name","tests"), as_configured_user=False)
        failure = self.evaluator._run_tests(self.terminal, session, self.handler)
        pane = session.capture_pane(capture_entire=True)
        self.handler.trial_paths.post_test_pane_path.write_text(pane)
        parsed, parse_failure = self.evaluator._parse_results(self.handler, pane)
        valid = failure == FailureMode.NONE and parse_failure == FailureMode.NONE and bool(parsed)
        resolved = valid and self.evaluator._is_resolved(parsed)
        result = {"schema_version":1,"workload":"terminal-bench-1.0","task_id":self.task_id,
            "dataset":self.catalog["dataset"], "sdk":self.catalog["sdk"], "resolved":resolved,
            "outcome":"resolved" if resolved else "unresolved" if valid else "evaluation_error",
            "test_results":{name:status.value for name,status in (parsed or {}).items()},
            "failure_mode":failure.value, "parse_failure":parse_failure.value,
            "artifact":"evaluation.json"}
        if valid:
            from asb_retained import record_verifier_return
            record_verifier_return(self.artifacts,identity,result,["evaluation-started.json",
                str(self.handler.trial_paths.post_test_pane_path.relative_to(self.artifacts))])
        write_json(self.artifacts / "evaluation.json", result)
        return result

    @quiet_sdk
    def __exit__(self, *_exception):
        self.interrupted = self.interrupted or bool(_exception and _exception[0] is not None)
        import docker
        import terminal_bench.terminal.terminal as terminal_module
        status = "not_created"
        try:
            if self.interrupted and self.journal is not None:
                status = "preservation_failed"
                self.journal.preserve(self.client)
                status = "stopped-preserved"
            elif self.terminal is not None:
                status = "cleanup_failed"
                for session in self.terminal._sessions.values():
                    try:
                        session.stop()
                    except Exception:
                        pass
                manager = self.terminal._compose_manager
                if getattr(manager, "asb_config", None) is not None:
                    manager._run_docker_compose_command(["down", "--timeout", "2", "--volumes", "--remove-orphans"])
                remaining = self.client.containers.list(all=True, filters={"label":"com.docker.compose.project=" + self.attempt})
                if remaining:
                    raise ExecutionError("official task containers could not be completely removed")
                if self.journal is not None:
                    self.journal.cleanup_images(self.client)
                status = "removed"
                if self.workspace:
                    self.workspace.container = None
        finally:
            namespace_status = status
            failures = []
            # Namespace removal alone does not settle launcher workers/bridges.
            # Keep the control acknowledgement unconfirmed until ALL owners join.
            self.cleanup_status = "cleanup_failed"
            if self.workspace:
                self.workspace.cleanup_status = "cleanup_failed"
            if hasattr(self, "original_client"):
                docker.from_env, terminal_module.DockerComposeManager = self.original_client, self.original_manager
            finalizers = [self.workspace.join_held_executions if self.workspace is not None else None,
                self.agent_launcher.close if self.agent_launcher is not None else None,
                self.journal.close if self.journal is not None else None]
            for finalize in finalizers:
                if finalize is not None:
                    try:finalize()
                    except BaseException as error:failures.append(type(error).__name__)
            if failures:status = "cleanup_failed"
            try:
                if self.artifacts.is_dir():
                    write_json(self.artifacts / "task-cleanup.json", {"attempt_id":self.attempt,
                        "status":status,"namespace_status":namespace_status})
            except BaseException as error:
                failures.append(type(error).__name__)
                status = "cleanup_failed"
            self.cleanup_status = status
            if self.workspace:self.workspace.cleanup_status = status
            if failures:
                raise ExecutionError("official task owner finalization remains unconfirmed: "+", ".join(failures))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--catalog", required=True, type=Path)
    parser.add_argument("--task", required=True)
    parser.add_argument("--artifacts", required=True, type=Path)
    args = parser.parse_args()
    def cancelled(_signal, _frame):
        raise InterruptedError("ASB original Terminal-Bench session was cancelled")
    signal.signal(signal.SIGTERM, cancelled)
    from asb_task_server import serve
    with DockerSession() as client:
        return serve(TerminalBenchTask(client, args.catalog, args.task, args.artifacts), sys.stdin, sys.stdout)


if __name__ == "__main__":
    raise SystemExit(main())
