"""Install and execute ASB-selected clients in an owned task environment.

No provider/agent registry lives here. Selection, package recipes, endpoint
configuration and output interpretation remain in the same Rust adapters used
by native execution. Host homes, venvs and project directories are not mounted.
"""
from contextlib import ExitStack
import hashlib
import json
import math
from pathlib import Path
import posixpath
import re
import tarfile
import tempfile
import time
import threading

from asb_execution import ExecutionError, principal_observation
from asb_runtime import bundle
from asb_staging import upload, stage_runtime, root_execute, prepare_guest_roots
from asb_relay import HostRelay, ForwardingCancellation, forward_request


class PreparationBudget:
    """One bounded monotonic deadline shared by all launch preparation stages."""
    def __init__(self, seconds):
        if type(seconds) not in [int,float] or not math.isfinite(seconds) or not 0 < seconds <= 1800:
            raise ExecutionError("invalid shared launch preparation budget")
        self.deadline, self.active = time.monotonic()+seconds, True
        self.on_failure, self.failed, self.lock = None, False, threading.Lock()

    def remaining(self):
        remaining = self.deadline-time.monotonic()
        if remaining <= 0:
            raise ExecutionError("shared launch preparation deadline exceeded")
        return remaining

    def expired(self):
        return self.active and time.monotonic() >= self.deadline

    def fail(self):
        with self.lock:
            notify = self.active and not self.failed
            self.failed = True
        if notify and self.on_failure is not None:
            try:self.on_failure()
            except BaseException:pass  # Notification must not skip settlement.


def stage_public_cache(container, project, temporary_dir=None, owner=None):
    cache = project / ".asb-cache/npm/_cacache"
    if not cache.is_dir():
        return
    paths = list(cache.rglob("*"))
    if (any(path.is_symlink() for path in paths)
            or len(paths) > 100000 or sum(path.stat().st_size for path in paths if path.is_file()) > 512 * 1024**2):
        return  # Do not copy unsafe/unbounded caches; normal install can download.
    for entry in paths:
        if entry.is_file() and "index-v5" in entry.parts:
            for line in entry.read_text().splitlines():
                if "\t" in line:
                    record = json.loads(line.split("\t", 1)[1])
                    metadata = record.get("metadata") or {}
                    if {key.lower() for key in metadata.get("reqHeaders", {})} & {"authorization", "proxy-authorization", "cookie"}:
                        return  # Never stage a credential-bearing registry cache.
    with tempfile.TemporaryFile(dir=temporary_dir) as archive:
        with tarfile.open(fileobj=archive, mode="w") as tar:
            def ownership(entry):
                if owner is not None:
                    entry.uid,entry.gid = owner
                    entry.uname,entry.gname = "",""
                return entry
            tar.add(cache, arcname="npm/_cacache", recursive=True,filter=ownership)
        archive.seek(0)
        if not container.put_archive("/opt/asb-project/.asb-cache", archive):
            raise ExecutionError("could not stage public package cache")


def stage_bootstrap(task, plan, tools, owner=None):
    """Consume declared installer assets; never infer dependencies from names."""
    if (not isinstance(plan,dict) or not {"runtime_tools","assets"}.issubset(plan)
            or set(plan)-{"runtime_tools","assets","public_package_cache","npm_package_tree"}
            or type(plan.get("public_package_cache",False)) is not bool):
        raise ExecutionError("selected tool installation bootstrap plan is missing or invalid")
    runtimes,assets = plan["runtime_tools"],plan["assets"]
    if (not isinstance(runtimes,list) or len(runtimes) > 16
            or any(not isinstance(name,str) or not re.fullmatch(r"[a-z0-9][a-z0-9_.-]{0,79}",name) for name in runtimes)
            or len(set(runtimes)) != len(runtimes) or not isinstance(assets,list) or len(assets) > 64):
        raise ExecutionError("selected tool installation bootstrap plan exceeds its limits")
    staged,targets,total = [],set(),0
    for asset in assets:
        if not isinstance(asset,dict) or set(asset) != {"source","target"}:
            raise ExecutionError("invalid declared tool bootstrap asset")
        if not isinstance(asset["source"],str) or not isinstance(asset["target"],str):
            raise ExecutionError("invalid declared tool bootstrap path")
        source,target = Path(asset["source"]),Path(asset["target"])
        if (not source.is_absolute() or source.is_symlink() or not source.is_file()
                or not source.resolve().is_relative_to(tools.resolve())
                or target.is_absolute() or not target.parts or ".." in target.parts
                or any(not re.fullmatch(r"[A-Za-z0-9+_.-]{1,256}",part) for part in target.parts)
                or len(str(target)) > 1024 or str(target) in targets):
            raise ExecutionError("tool bootstrap asset escapes its project cache or guest destination")
        size = source.stat().st_size
        total += size
        if size > 256 * 1024**2 or total > 512 * 1024**2:
            raise ExecutionError("selected tool bootstrap assets exceed 512 MiB")
        targets.add(str(target))
        with source.open("rb") as incoming:
            content = incoming.read(256 * 1024**2 + 1)
        if len(content) != size:
            raise ExecutionError("tool bootstrap asset changed while it was being staged")
        staged.append((str(target),content,0o600))
    root = "/opt/asb-project/.asb-tools"
    for target in sorted({str(Path(name).parent) for name,_,_ in staged}):
        task.checked(["mkdir","-p",root+"/"+target])
    if staged:
        upload(task.workspace.container,root,staged,temporary_dir=task.artifacts,owner=owner)
    receipt = {"schema_version":1,"runtime_tools":runtimes,
               "assets":[{"target":name,"bytes":len(content),"sha256":hashlib.sha256(content).hexdigest()}
                         for name,content,_ in staged]}
    path = task.artifacts / "bootstrap.json"
    path.write_text(json.dumps(receipt,sort_keys=True)+"\n")
    path.chmod(0o600)
    return runtimes


def stage_npm_package_tree(task, tree, tools, selected, owner=None):
    """Stage a declared, bounded npm tree without exposing a host mount.

    The Rust installation adapter owns archive construction.  This consumer
    verifies the narrow wire contract before Docker extracts the archive into
    the selected guest tool root.  No client name or package name is inferred.
    """
    if (not isinstance(tree,dict) or set(tree) != {"source","target","executable"}
            or not all(isinstance(tree[key],str) for key in tree)):
        raise ExecutionError("npm package-tree bootstrap is invalid")
    source,target,executable = Path(tree["source"]),tree["target"],tree["executable"]
    if (target != selected or not re.fullmatch(r"[a-z0-9][a-z0-9_.-]{0,79}",target)
            or not re.fullmatch(r"[A-Za-z0-9+_.-]{1,256}",executable)
            or not source.is_absolute() or source.is_symlink() or not source.is_file()
            or not source.resolve().is_relative_to(tools.resolve())):
        raise ExecutionError("npm package-tree bootstrap escapes its selected guest tool")
    if source.stat().st_size > 512 * 1024**2:
        raise ExecutionError("npm package-tree bootstrap exceeds 512 MiB")
    members,total = [],0
    try:
        with tarfile.open(source,mode="r:") as archive:
            members = archive.getmembers()
            if not 1 <= len(members) <= 100000:
                raise ExecutionError("npm package-tree bootstrap has an invalid member count")
            for member in members:
                name = member.name
                parts = Path(name).parts
                if (not name or name.startswith("/") or ".." in parts
                        or parts[0] != "node_modules"
                        or not (member.isdir() or member.isfile() or member.issym() or member.islnk())):
                    raise ExecutionError("npm package-tree bootstrap contains an unsafe member")
                if member.isfile():
                    total += member.size
                    if member.size > 256 * 1024**2 or total > 512 * 1024**2:
                        raise ExecutionError("npm package-tree bootstrap exceeds its unpacked bound")
                if member.issym() or member.islnk():
                    link = member.linkname
                    resolved = posixpath.normpath(posixpath.join(
                        posixpath.dirname(name) if member.issym() else "",link))
                    parts = Path(resolved).parts
                    if (not link or Path(link).is_absolute() or resolved in {".",".."}
                            or resolved.startswith("../") or not parts or parts[0] != "node_modules"):
                        raise ExecutionError("npm package-tree bootstrap contains an escaping link")
    except (OSError,tarfile.TarError) as error:
        raise ExecutionError("npm package-tree bootstrap cannot be inspected") from error
    with source.open("rb") as incoming:
        destination = "/opt/asb-project/.asb-tools/"+target
        task.checked(["mkdir","-p",destination])
        if not task.workspace.container.put_archive(destination,incoming):
            raise ExecutionError("could not stage npm package-tree bootstrap")
    launcher = "/opt/asb-project/.asb-tools/"+target+"/node_modules/.bin/"+executable
    task.checked(["test","-x",launcher])
    receipt = {"schema_version":1,"target":target,"executable":executable,
               "archive_bytes":source.stat().st_size,"unpacked_bytes":total,
               "members":len(members)}
    path = task.artifacts / "npm-package-tree.json"
    path.write_text(json.dumps(receipt,sort_keys=True)+"\n")
    path.chmod(0o600)


def prepare_agent(task, request, cancelled, resources=None, relays=None, preparation_budget=None):
    """Install the exact selected client independently of provider execution."""
    guest = request.get("request")
    if not isinstance(guest, dict):
        raise ExecutionError("private agent execution request is missing")
    tool = guest.get("config", {}).get("agent", "")
    if not re.fullmatch(r"[a-z0-9][a-z0-9_.-]{0,79}", tool):
        raise ExecutionError("selected agent tool identity is invalid")
    project = Path(request["project_root"]).absolute()
    tools = Path(request["tools_root"]).absolute()
    if not (project / ".asb/project.toml").is_file():
        raise ExecutionError("host benchmark project is not initialized")
    container = task.workspace.container
    check_budget = preparation_budget.remaining if preparation_budget else lambda:None
    check_budget()
    files,receipt = bundle(request["binary"],getattr(task.workspace,"image_identity",{}).get("architecture"))
    check_budget()
    runtime_receipt = task.artifacts / "runtime.json"
    runtime_receipt.write_text(json.dumps(receipt,sort_keys=True)+"\n")
    runtime_receipt.chmod(0o600)
    deadline = preparation_budget.deadline if preparation_budget else time.monotonic()+900
    root_checked = lambda arguments:root_execute(task,arguments,deadline=deadline,cancelled=cancelled)
    owner = stage_runtime(task,request["binary"],files,task.checked,root_checked,deadline=deadline,cancelled=cancelled)
    task_identity,ownership = prepare_guest_roots(task,owner,task.checked,root_checked)
    task.checked(["mkdir", "-m", "700", "/opt/asb-project/.asb", "/opt/asb-project/.asb-tools", "/opt/asb-project/.asb-cache"])
    check_budget()
    configuration = request.get("project_configuration", "tools_dir='.asb-tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n")
    if not isinstance(configuration, str) or len(configuration.encode()) > 64 * 1024:
        raise ExecutionError("guest project configuration exceeds its limit")
    if relays is None:
        relays = []
    def start_relay(targets):
        if resources is None:
            raise ExecutionError("local endpoint forwarding needs an owned execution scope")
        from asb_docker import docker_command
        relay = HostRelay(targets, [*docker_command(), "exec", "-i", container.id,
                                   "/opt/asb-runtime/asb", "__asb-relay"], cancelled,
                          task.artifacts / f"endpoint-relay-{len(relays)}.json")
        resources.callback(relay.close)
        relays.append(relay)
        return relay
    request = forward_request(dict(request, project_configuration=configuration), start_relay)
    configuration = request["project_configuration"]
    guest = request["request"]
    guest = dict(guest,task_identity=task_identity,owned_instrumentation=ownership)
    upload(container, "/opt/asb-project/.asb", [("project.toml", configuration.encode(), 0o600)],temporary_dir=task.artifacts,owner=owner)
    runtimes = stage_bootstrap(task,request.get("bootstrap"),tools,owner=owner)
    package_tree = request["bootstrap"].get("npm_package_tree")
    if package_tree is not None:
        stage_npm_package_tree(task,package_tree,tools,tool,owner=owner)
    check_budget()
    if request["bootstrap"].get("public_package_cache",False):
        stage_public_cache(container, project,temporary_dir=task.artifacts,owner=owner)
        check_budget()
    environment = request.get("environment", {})
    for selected in [*runtimes, *([] if package_tree is not None else [tool])]:
        result = task.workspace.execute(["/bin/sh", "-c", 'cd /opt/asb-project && exec /opt/asb-runtime/asb tool install "$1"', "asb", selected],
                                        environment=environment, timeout=min(900,preparation_budget.remaining()) if preparation_budget else 900, output_limit=256 * 1024,
                                        cancelled=ForwardingCancellation(cancelled, relays),
                                        on_preparation_failure=preparation_budget.fail if preparation_budget else None)
        check_budget()
        for relay in relays:
            relay.check()
        for stream in ["stdout", "stderr"]:
            path = task.artifacts / f"install-{selected}.{stream}"
            path.write_bytes(result[stream])
            path.chmod(0o600)
        if result["exit_code"] != 0 or result["timed_out"] or result["cancelled"]:
            # The nested CLI puts command diagnostics in its project log.
            # Export a bounded tail before the owned environment is removed.
            detail = container.exec_run(["tail", "-c", "262144", "/opt/asb-project/.asb/install.log"])
            if detail.exit_code == 0:
                path = task.artifacts / f"install-{selected}.log"
                path.write_bytes(detail.output)
                path.chmod(0o600)
            raise ExecutionError("selected agent runtime installation failed; inspect private install artifacts")
    return guest,environment


def execute_agent(task, request, before_resume, on_output, cancelled, on_ended=None, preparation_budget=None, on_preparation_failure=None):
    # The relay spans package installation AND agent execution and is closed
    # on success, cancellation and every preparation/execution exception.
    seconds = request.get("preparation_seconds")
    if preparation_budget is None and seconds is not None:
        preparation_budget = PreparationBudget(seconds)
    if preparation_budget is not None:
        preparation_budget.on_failure = on_preparation_failure
    upstream = cancelled
    if preparation_budget is not None:
        cancelled = ForwardingCancellation(lambda: preparation_budget.expired() or
            (upstream.is_set() if upstream is not None else False), [])
    with ExitStack() as resources:
        try:
            return _execute_agent(task, request, before_resume, on_output, cancelled, resources, [], on_ended, preparation_budget)
        except BaseException:
            if preparation_budget is not None:preparation_budget.fail()
            raise


def _execute_agent(task, request, before_resume, on_output, cancelled, resources, relays, on_ended, preparation_budget):
    guest,environment = prepare_agent(task,request,cancelled,resources,relays,preparation_budget)
    if preparation_budget is not None:
        preparation_budget.remaining()
        request = dict(request, preparation_deadline=preparation_budget.deadline,
                       on_preparation_failure=preparation_budget.fail)
    # Only final client execution uses a workload-owned launcher. Install and
    # bootstrap commands remain ordinary managed Docker executions.
    prepare_transport = getattr(task,"prepare_agent_transport",None)
    transport = prepare_transport(request) if prepare_transport is not None else None
    if preparation_budget is not None:preparation_budget.remaining()
    if transport is not None:
        guest = dict(guest,inherit_working_directory=transport.inherit_working_directory)
    container = task.workspace.container
    network = request.get("network")
    if network:
        # Change only the task's owned container, never host network policy.
        container.reload()
        container.client.networks.get(network).connect(container)
        for previous in list(container.attrs["NetworkSettings"]["Networks"]):
            if previous != network:
                container.client.networks.get(previous).disconnect(container)
    guest = dict(guest)
    lifetime = guest.get("process_lifetime", "quiesce")
    lifetime_options = {}
    if lifetime == "through-evaluation":
        import os
        import uuid
        container.reload()
        labels = container.attrs.get("Config", {}).get("Labels", {}) or {}
        if (labels.get("asb.owner") != "asb" or labels.get("asb.attempt") != task.workspace.attempt
                or container.attrs.get("HostConfig", {}).get("PidMode") not in [None, "", "private"]):
            raise ExecutionError("retained services require this attempt's private PID namespace")
        probe = container.exec_run(["/opt/asb-runtime/asb", "__asb-namespace"])
        if probe.exit_code != 0 or len(probe.output) > 32 or not probe.output.strip().isdigit():
            raise ExecutionError("cannot identify owned workload PID namespace")
        controller_namespace = os.stat("/proc/self/ns/pid").st_ino
        guest_namespace = int(probe.output)
        if guest_namespace <= 1 or guest_namespace == controller_namespace:
            raise ExecutionError("workload services cannot outlive their client in the controller namespace")
        guest["workload_owner"] = {"controller_namespace":controller_namespace,
                                   "guest_namespace":guest_namespace, "token":uuid.uuid4().hex}
        lifetime_options["workload_owner"] = guest["workload_owner"]
    elif lifetime != "quiesce":
        raise ExecutionError("unknown workload process lifetime")
    guest["timeout_seconds"] = request.get("timeout", getattr(task, "agent_timeout", 900))
    guest.update(project_root="/opt/asb-project", runtime_dir="/opt/asb-private", run_dir="/opt/asb-private/attempt")
    environment = dict(environment, ASB_GUEST_AGENT_REQUEST=json.dumps(guest))
    ended = False
    def finish_execution(result):
        nonlocal ended
        if not ended:
            ended = True
            if on_ended is not None:
                on_ended(result)
    if lifetime_options:
        lifetime_options["on_execution_end"] = finish_execution
    def attach(target):
        if request.get("measure_target", False):
            # The task execution lock is already held while the gate is
            # stopped. This read-only control probe must not recursively start
            # another managed workspace execution.
            probe = container.exec_run(["/opt/asb-runtime/asb", "__asb-target", str(target["namespace_pid"])])
            if probe.exit_code != 0 or len(probe.output) > 65536:
                raise ExecutionError("cannot read isolated measurement identity")
            try:
                target = dict(target, process_identity=json.loads(probe.output))
            except ValueError:
                raise ExecutionError("isolated process did not return valid measurement identity") from None
        if before_resume is not None:
            before_resume(target)
        if preparation_budget is not None:
            preparation_budget.remaining()
            preparation_budget.active = False
    result = task.workspace.execute(["/opt/asb-runtime/asb", "__asb-agent"], environment=environment,
                                    timeout=request.get("timeout", getattr(task, "agent_timeout", 900)), output_limit=256 * 1024,
                                    gate="application", before_resume=attach,
                                    preparation_timeout=preparation_budget.remaining() if preparation_budget else 90,
                                    on_preparation_failure=preparation_budget.fail if preparation_budget else None,
                                    on_output=on_output, cancelled=ForwardingCancellation(cancelled, relays),
                                    transport_factory=transport,
                                    **lifetime_options)
    finish_execution(result)
    for relay in relays:
        relay.check()
    observation = {key: result.get(key) for key in ["exit_code", "timed_out", "cancelled",
                   "output_truncated", "elapsed_seconds", "preparation_seconds", "target"]}
    try:
        if not result.get("workload_processes_retained"):
            metadata = task.checked(["cat", "/opt/asb-private/attempt/agent-result.json"], output_limit=1024 * 1024)
            result["metadata"] = json.loads(metadata)
        if not isinstance(result["metadata"], dict):
            raise ExecutionError("guest result is not a structured record")
    except (ExecutionError, ValueError):
        # Keep partial work eligible for evaluation after timeout/client death.
        # Unknown usage is never invented from text or an expected minimum.
        result["metadata"] = {"schema_version": 1, "exit_code": result["exit_code"],
                              "final_response": None, "token_usage": {}, "steps": None}
    if result.get("workload_processes_retained"):
        if (not lifetime_options or result["metadata"].get("workload_owner") != guest["workload_owner"]
                or result["metadata"].get("tree_quiesced") is not False):
            raise ExecutionError("workload retained services without confirmed ownership")
        observation["agent_tree_cleanup"] = "delegated-owned-workload"
    elif result["metadata"].get("tree_quiesced") is not True:
        task.workspace.quiesce_unconfirmed_agent()
        result["metadata"]["tree_quiesced"] = True
        result["metadata"]["workload_owner"] = None
        observation["agent_tree_cleanup"] = "owned-container-reset"
    else:
        observation["agent_tree_cleanup"] = "supervisor-confirmed"
    # Metadata is available only after the existing quiescence/owner boundary.
    # A reset or carrier code alone never supplies an exact principal fact.
    exact = principal_observation(result["metadata"])
    result["metadata"].update(exact)
    result.update(exact)
    observation.update(exact)
    journal = task.artifacts / "execution.json"
    journal.write_text(json.dumps(observation))
    journal.chmod(0o600)
    return result
