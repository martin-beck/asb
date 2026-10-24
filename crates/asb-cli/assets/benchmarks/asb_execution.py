"""Agent/provider/benchmark-neutral execution inside an owned Docker workspace.

No host mounts, host networking or privileged container mode. A stopped shell
gate reports both namespace and daemon-host PIDs before the caller attaches
collectors and resumes execution. Docker-daemon PIDs are deliberately not
claimed to be local-host PIDs: the caller must establish that separately.
"""
import json
import math
import re
import threading
import time


class ExecutionError(RuntimeError):
    pass


def principal_observation(metadata):
    """Optional owner-authored evidence, never a Docker/carrier exit inference."""
    exact = metadata.get("process_termination")
    diagnostic = metadata.get("process_termination_diagnostic")
    valid = exact is None or (isinstance(exact, dict) and (
        (set(exact) == {"kind", "code"} and exact.get("kind") == "code"
         and type(exact.get("code")) is int and -(2**31) <= exact["code"] < 2**31)
        or (set(exact) == {"kind", "signal"} and exact.get("kind") == "signal"
            and type(exact.get("signal")) is int and 1 <= exact["signal"] <= 127)))
    if (not valid or diagnostic not in [None, "malformed", "unavailable"]
            or diagnostic is not None and exact is not None):
        return {"process_termination": None, "process_termination_diagnostic": "malformed"}
    result = {"process_termination": exact}
    if diagnostic is not None:
        result["process_termination_diagnostic"] = diagnostic
    return result


GATE = 'printf "__ASB_GATE__:%s\\n" "$$"; kill -STOP "$$"; exec "$@"'
STOPPED = 'while IFS=: read -r key value; do case "$key" in State) case "$value" in *T*) exit 0;; esac;; esac; done < "/proc/$1/status"; exit 1'
MAX_ARGUMENT_BYTES = 1024 * 1024


class WorkloadFrames:
    """Bounded framing for a namespace-owned supervisor, never agent stdout metadata.

    Ordinary bytes remain ordinary output; control frames require the current
    controller token and gate PID. The principal exit boundary precedes private
    result parsing and is distinct from the lifetime of workload services.
    """
    def __init__(self, pid, owner, ended):
        self.pid, self.owner, self.ended = pid, owner, ended
        self.pending = bytearray()
        self.result = None
        self.ready = False

    def feed(self, chunk):
        self.pending.extend(chunk)
        output = bytearray()
        while b"\n" in self.pending:
            line, _, remaining = self.pending.partition(b"\n")
            self.pending = bytearray(remaining)
            if line.startswith((b"__ASB_AGENT_ENDED__:", b"__ASB_WORKLOAD_READY__:")):
                if len(line) > 256:
                    raise ExecutionError("workload supervisor frame exceeds its bound")
                parts = line.split(b":")
                if (len(parts) < 3 or parts[1] != str(self.pid).encode()
                        or parts[2] != self.owner["token"].encode()):
                    raise ExecutionError("workload supervisor frame does not match its owner")
                if parts[0] == b"__ASB_AGENT_ENDED__":
                    if (self.result is not None or len(parts) != 6
                            or not all(part.isdigit() for part in parts[3:])
                            or not 0 <= int(parts[3]) <= 255 or not 0 <= int(parts[4]) <= 86410000
                            or parts[5] not in [b"0", b"1"]):
                        raise ExecutionError("invalid or repeated workload execution boundary")
                    self.result = {"exit_code":int(parts[3]), "elapsed_seconds":int(parts[4]) / 1000,
                                   "timed_out":parts[5] == b"1"}
                    if self.ended is not None:
                        self.ended(dict(self.result))
                else:
                    if (self.result is None or self.ready or len(parts) != 4
                            or parts[3] != str(self.result["exit_code"]).encode()):
                        raise ExecutionError("workload readiness preceded or contradicted execution completion")
                    self.ready = True
                    # Output after readiness belongs to retained services, not
                    # the now-frozen client observation.
                    self.pending.clear()
                    break
            else:
                output.extend(line + b"\n")
        if len(self.pending) > 512:
            if self.pending.startswith(b"__ASB_"):
                raise ExecutionError("workload supervisor frame exceeds its bound")
            output.extend(self.pending[:-512])
            del self.pending[:-512]
        return bytes(output)


class DockerExecTransport:
    """Owned demux transport, distinct from the execution's measured target.

    Alternative workload launchers implement this iterable/target/exit_status/
    close contract. They must settle partial allocations if construction fails;
    closing a transport alone never proves its client process tree quiescent.
    """
    def __init__(self, api, container, command, environment, workdir):
        self.api, self.container = api,container
        self.exec_id = api.exec_create(container.id,cmd=command,environment=environment,
            workdir=workdir,stdout=True,stderr=True)["Id"]
        self.stream = api.exec_start(self.exec_id,stream=True,demux=True)
        self.closed, self.close_error = False,None

    def __iter__(self):
        return self

    def __next__(self):
        return next(self.stream)

    def target(self, namespace_pid):
        inspection = self.api.exec_inspect(self.exec_id)
        daemon_pid = inspection.get("Pid")
        if inspection.get("Running") is not True or type(daemon_pid) is not int or daemon_pid <= 1:
            raise ExecutionError("Docker did not identify the running execution target")
        return {"container_id":self.container.id,"exec_id":self.exec_id,
            "daemon_pid":daemon_pid,"namespace_pid":namespace_pid,"pid_scope":"docker-daemon-host"}

    def exit_status(self):
        inspection = self.api.exec_inspect(self.exec_id)
        status = inspection.get("ExitCode")
        if type(status) is not int or not 0 <= status <= 255 or inspection.get("Running") is not False:
            raise ExecutionError("Docker returned no final execution status")
        return status

    def close(self):
        if self.close_error is not None:
            raise ExecutionError("execution transport closure remains unconfirmed") from self.close_error
        if self.closed:
            return
        self.closed = True
        try:
            try:
                self.stream.close()
            finally:
                response = getattr(self.stream,"_response",None)
                if response is not None:
                    response.close()
        except BaseException as error:
            self.close_error = error
            raise


def execution_target(transport, container, namespace_pid):
    """Validate routing identity without guessing a custom launcher's agent PID."""
    target = transport.target(namespace_pid)
    if (not isinstance(target,dict) or target.get("container_id") != container.id
            or target.get("exec_id") != transport.exec_id or target.get("namespace_pid") != namespace_pid
            or target.get("pid_scope") not in ["docker-daemon-host","container-namespace"]
            or (target["pid_scope"] == "docker-daemon-host" and
                (type(target.get("daemon_pid")) is not int or target["daemon_pid"] <= 1))
            or (target["pid_scope"] == "container-namespace" and target.get("daemon_pid") is not None)):
        raise ExecutionError("execution transport returned a contradictory target identity")
    return target


class ManagedDockerWorkspace:
    @classmethod
    def attach(cls, client, container, attempt, workdir):
        """Use an official harness-created container with verified ASB ownership.

        Its owning workload session remains responsible for Compose/sidecar
        teardown; generic command execution never enumerates host resources.
        """
        labels = container.attrs.get("Config", {}).get("Labels", {}) or {}
        if labels.get("asb.owner") != "asb" or labels.get("asb.attempt") != attempt:
            raise ExecutionError("cannot attach to a task environment outside this attempt")
        workspace = cls(client, container.image, attempt, workdir)
        host = container.attrs.get("HostConfig", {})
        if host.get("PidMode") not in [None, "", "private"]:
            raise ExecutionError("task execution requires a private container PID namespace")
        workspace.limits = {"memory_bytes": host.get("Memory") or None,
                            "cpus": (host.get("NanoCpus") or 0) / 10**9 or None,
                            "pids": host.get("PidsLimit") if (host.get("PidsLimit") or 0) > 0 else None}
        workspace.container, workspace.cleanup_status = container, "pending"
        return workspace

    def __init__(self, client, image, attempt, workdir, memory_bytes=4 * 1024**3,
                 cpus=2, pids=512, journal=None):
        if not re.fullmatch(r"asb-[0-9a-f]{32}", attempt):
            raise ExecutionError("workspace requires a unique ASB attempt identity")
        if not isinstance(workdir, str) or not workdir.startswith("/") or "\x00" in workdir:
            raise ExecutionError("workspace working directory must be absolute")
        if not 128 * 1024**2 <= memory_bytes <= 1024**4 or not 0 < cpus <= 1024 or not 16 <= pids <= 65536:
            raise ExecutionError("invalid container resource limits")
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", image.id):
            raise ExecutionError("workspace image must resolve to a content identity")
        self.client, self.image, self.attempt, self.workdir = client, image, attempt, workdir
        self.limits = {"memory_bytes": memory_bytes, "cpus": cpus, "pids": pids}
        self.container = None
        self.journal = journal
        self.active = threading.Lock()
        self.cleanup_status = "not_created"
        self.held_executions = []
        self.image_identity = {"image_id": image.id, "repository_digests": image.attrs.get("RepoDigests", []),
                               "architecture": image.attrs.get("Architecture"), "os": image.attrs.get("Os")}

    def __enter__(self):
        if self.container is not None:
            raise ExecutionError("workspace has already been created")
        labels = {"asb.owner": "asb", "asb.attempt": self.attempt}
        if self.journal is not None:
            labels.update(self.journal.declare("containers", self.attempt))
        creation = dict(
            image=self.image.id, name=self.attempt, command=["/bin/sh", "-c", "exec tail -f /dev/null"],
            detach=True, working_dir=self.workdir,
            labels=labels,
            mem_limit=self.limits["memory_bytes"], nano_cpus=int(self.limits["cpus"] * 10**9),
            pids_limit=self.limits["pids"],
        )
        if self.journal is not None:
            self.journal.direct_container(creation)
        self.container = self.client.containers.create(**creation)
        self.cleanup_status = "pending"
        try:
            self.container.start()
            return self
        except BaseException:
            self.close()
            raise

    def execute(self, argv, environment=None, timeout=900, output_limit=256 * 1024, before_resume=None,
                on_output=None, cancelled=None, gate="shell", preparation_timeout=None,
                workload_owner=None, on_execution_end=None, transport_factory=None, on_preparation_failure=None):
        if self.container is None:
            raise ExecutionError("workspace is not running")
        if not self.active.acquire(blocking=False):
            raise ExecutionError("one command is already running in this task workspace")
        try:
            return self._execute(argv, environment or {}, timeout, output_limit, before_resume, on_output, cancelled, gate, preparation_timeout,
                                 workload_owner, on_execution_end, transport_factory, on_preparation_failure)
        finally:
            self.active.release()

    def _execute(self, argv, environment, timeout, output_limit, before_resume, on_output, cancelled, gate, preparation_timeout,
                 workload_owner, on_execution_end, transport_factory, on_preparation_failure):
        if (not isinstance(argv, list) or not argv or len(argv) > 10000
                or any(not isinstance(value, str) or "\x00" in value for value in argv)
                or not isinstance(environment, dict)
                or any(not isinstance(key, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key)
                       or not isinstance(value, str) or "\x00" in value for key, value in environment.items())):
            raise ExecutionError("invalid execution arguments or environment")
        if sum(len(value.encode()) for value in argv) + sum(len(key) + len(value.encode()) for key, value in environment.items()) > MAX_ARGUMENT_BYTES:
            raise ExecutionError("execution arguments and environment exceed 1 MiB")
        if not 0 < timeout <= 86400 or not 1 <= output_limit <= 16 * 1024**2:
            raise ExecutionError("invalid execution timeout or output limit")
        preparation_timeout = min(timeout, 60) if preparation_timeout is None else preparation_timeout
        if type(preparation_timeout) not in [int,float] or not math.isfinite(preparation_timeout) or not 0 < preparation_timeout <= 1800:
            raise ExecutionError("invalid execution preparation timeout")
        if cancelled is not None and cancelled.is_set():
            raise ExecutionError("execution was cancelled before starting")
        if gate not in ["shell", "application"]:
            raise ExecutionError("unknown execution gate protocol")
        if transport_factory is not None and not callable(transport_factory):
            raise ExecutionError("execution transport factory is not callable")
        if workload_owner is not None:
            if (gate != "application" or self.held_executions or not isinstance(workload_owner, dict)
                    or set(workload_owner) != {"controller_namespace", "guest_namespace", "token"}
                    or any(type(workload_owner[key]) is not int or workload_owner[key] <= 1
                           for key in ["controller_namespace", "guest_namespace"])
                    or workload_owner["guest_namespace"] == workload_owner["controller_namespace"]
                    or not isinstance(workload_owner["token"], str)
                    or re.fullmatch(r"[0-9a-f]{32}", workload_owner["token"]) is None):
                raise ExecutionError("invalid owned workload execution lifetime")
        container, api = self.container, self.client.api
        done, expired, interrupted = threading.Event(), threading.Event(), threading.Event()
        reset_needed = threading.Event()
        watchdog_error = []
        started = time.monotonic()
        execution_started = None
        def preparation_failed():
            if execution_started is None and on_preparation_failure is not None:
                try:on_preparation_failure()
                except BaseException:pass
        phase_deadline, phase_lock = [started + preparation_timeout], threading.Lock()

        def deadline():
            while not done.wait(min(0.05, timeout)):
                with phase_lock:
                    if cancelled is not None and cancelled.is_set():
                        interrupted.set()
                        break
                    if time.monotonic() >= phase_deadline[0]:
                        expired.set()
                        break
            else:
                return
            reset_needed.set()
            preparation_failed()
            try:
                # Killing this owned container terminates the entire tree,
                # including daemonized helpers, without host-wide signals.
                container.kill()
            except Exception as error:
                watchdog_error.append(type(error).__name__)

        watchdog = threading.Thread(target=deadline, daemon=True)
        watchdog.start()
        stream = None
        result = None
        try:
            command = argv if gate == "application" else ["/bin/sh", "-c", GATE, "asb-gate", *argv]
            stream = (transport_factory or DockerExecTransport)(api,container,command,environment,self.workdir)
            ready = bytearray()
            # Docker's multiplexed stream may deliver stderr before the gate's
            # already-written stdout readiness frame. Installer diagnostics are
            # ordinary agent output, not proof that the gate itself failed.
            # Retain them with the eventual stderr result after the gate is
            # authenticated and stopped.
            gate_stderr = bytearray()
            for out, err in stream:
                if err:
                    room = output_limit - len(gate_stderr)
                    if len(err) > room:
                        raise ExecutionError("container execution gate stderr exceeded its output limit before readiness")
                    gate_stderr.extend(err)
                ready.extend(out or b"")
                if len(ready) > 64:
                    raise ExecutionError("container execution gate returned an invalid readiness frame")
                if ready.endswith(b"\n"):
                    break
            match = re.fullmatch(rb"__ASB_GATE__:(\d+)\n", ready)
            if match is None or expired.is_set() or interrupted.is_set():
                # Before readiness this is the only diagnostic emitted by the
                # private guest driver. Preserve a bounded, single-line tail
                # so a failed pre-gate preparation can be repaired rather than
                # being reported as an indistinguishable timeout.
                detail = " ".join(gate_stderr.decode("utf-8", "replace").split())[-4096:]
                message = "container execution gate did not become ready before its deadline"
                if detail:
                    message += f": {detail}"
                raise ExecutionError(message)
            namespace_pid = int(match.group(1))
            if namespace_pid <= 1:
                raise ExecutionError("container gate did not identify a distinct agent process")
            # The readiness bytes precede SIGSTOP by a few instructions. Do not
            # attach or resume until the process is actually stopped.
            for _ in range(100):
                stopped = container.exec_run(["/bin/sh", "-c", STOPPED, "asb-status", str(namespace_pid)])
                if stopped.exit_code == 0:
                    break
                if expired.is_set() or interrupted.is_set():
                    raise ExecutionError("container gate expired while waiting for SIGSTOP")
                time.sleep(0.01)
            else:
                raise ExecutionError("container gate never reached its stopped state")
            target = execution_target(stream,container,namespace_pid)
            if before_resume is not None:
                before_resume(target)
            with phase_lock:
                if (expired.is_set() or interrupted.is_set() or time.monotonic() >= phase_deadline[0]
                        or (cancelled is not None and cancelled.is_set())):
                    raise ExecutionError("container execution preparation was cancelled or exceeded its deadline")
                execution_started = time.monotonic()
                # The guest enforces the exact principal budget. Leave bounded
                # time for private artifact parsing/publication after it exits.
                phase_deadline[0] = execution_started + timeout + (15 if workload_owner else 0)
            # Minimal official task images may have only the POSIX shell's
            # kill builtin, not a standalone /bin/kill executable.
            resumed = container.exec_run(["/bin/sh", "-c", 'kill -CONT "$1"', "asb-resume", str(namespace_pid)])
            if resumed.exit_code != 0:
                raise ExecutionError("container execution gate could not be resumed")
            stdout, stderr = bytearray(), gate_stderr
            truncated = False
            frames = WorkloadFrames(namespace_pid, workload_owner, on_execution_end) if workload_owner else None
            for out, err in stream:
                if frames is not None and out:
                    out = frames.feed(out)
                for name, destination, chunk in [("stdout", stdout, out), ("stderr", stderr, err)]:
                    if chunk:
                        if on_output is not None:
                            for start in range(0, len(chunk), 32 * 1024):
                                on_output(name, chunk[start:start + 32 * 1024])
                        room = max(0, output_limit - len(destination))
                        truncated |= len(chunk) > room
                        destination.extend(chunk[:room])
                if frames is not None and frames.ready:
                    if expired.is_set() or interrupted.is_set() or (cancelled is not None and cancelled.is_set()):
                        raise ExecutionError("retained workload handoff was cancelled")
                    metadata = container.exec_run(["cat", "/opt/asb-private/attempt/agent-result.json"])
                    if metadata.exit_code != 0 or len(metadata.output) > 1024 * 1024:
                        raise ExecutionError("retained workload has no bounded private result")
                    try:
                        metadata = json.loads(metadata.output)
                    except (ValueError, TypeError):
                        raise ExecutionError("retained workload returned malformed result metadata") from None
                    if (not isinstance(metadata, dict) or metadata.get("workload_owner") != workload_owner
                            or metadata.get("tree_quiesced") is not False
                            or (metadata.get("exit_code") if type(metadata.get("exit_code")) is int
                                else 1 if metadata.get("exit_code") is None else -1) != frames.result["exit_code"]
                            or type(metadata.get("elapsed_ms")) is not int
                            or metadata["elapsed_ms"] != round(frames.result["elapsed_seconds"] * 1000)
                            or metadata.get("timed_out") is not frames.result["timed_out"]):
                        raise ExecutionError("retained workload metadata contradicts its supervisor")
                    release_parent = getattr(stream,"release_workload_parent",None)
                    if release_parent is not None:
                        release_parent()
                    result = dict(frames.result, stdout=bytes(stdout), stderr=bytes(stderr), cancelled=False,
                                  output_truncated=truncated, preparation_seconds=execution_started-started,
                                  target=target, workload_processes_retained=True, metadata=metadata)
                    done.set()
                    watchdog.join(timeout=125)
                    if watchdog.is_alive() or watchdog_error or expired.is_set() or interrupted.is_set():
                        raise ExecutionError("retained workload handoff exceeded its execution lifetime")
                    self._hold_execution(stream)
                    stream = None
                    break
            if result is not None:
                return result
            if frames is not None and frames.pending:
                chunk = bytes(frames.pending)
                room = max(0, output_limit - len(stdout))
                truncated |= len(chunk) > room
                stdout.extend(chunk[:room])
                if on_output is not None:
                    on_output("stdout", chunk)
            status = stream.exit_status()
            if type(status) is not int or not 0 <= status <= 255:
                raise ExecutionError("execution transport returned no valid child exit status")
            result = {"exit_code": status, "stdout": bytes(stdout), "stderr": bytes(stderr),
                      "timed_out": expired.is_set(), "cancelled": interrupted.is_set(), "output_truncated": truncated,
                      "elapsed_seconds": time.monotonic() - execution_started,
                      "preparation_seconds": execution_started - started, "target": target}
        except BaseException:
            # A failed collector callback, malformed gate or protocol failure
            # must not leave a paused/live agent behind. Preserve its filesystem
            # by restarting only PID 1 after the complete old tree is gone.
            preparation_failed()
            reset_needed.set()
            try:
                container.kill()
            except Exception:
                pass
            raise
        finally:
            done.set()
            watchdog.join(timeout=125)
            try:
                if stream is not None:
                    self._close_stream(stream)
            except BaseException:
                # Closing bridge IO is not proof that its actual client (or
                # launcher worker) settled. Quiesce the owned namespace too.
                reset_needed.set()
                try:
                    container.kill()
                except Exception:
                    pass
                raise
            finally:
                if reset_needed.is_set() and not watchdog.is_alive() and not watchdog_error:
                    container.wait(timeout=125)
                    container.start()
        if watchdog.is_alive() or watchdog_error:
            raise ExecutionError("Docker could not stop the timed-out execution tree")
        return result

    @staticmethod
    def _close_stream(stream):
        try:
            stream.close()
        finally:
            response = getattr(stream, "_response", None)
            if response is not None:
                response.close()

    def _hold_execution(self, stream):
        errors = []
        def drain():
            try:
                for _chunk in stream:
                    pass
            except Exception:
                # Namespace teardown closes Docker's attached stream. Grading
                # and cleanup status come from the owning SDK, not this drain.
                pass
            finally:
                try:
                    self._close_stream(stream)
                except BaseException as error:
                    errors.append(type(error).__name__)
        worker = threading.Thread(target=drain, daemon=True)
        self.held_executions.append((stream, worker, errors))
        worker.start()

    def join_held_executions(self):
        """Call after the owning SDK stops/removes its namespace, never before grading."""
        for stream, worker, errors in self.held_executions:
            worker.join(timeout=2)
            if worker.is_alive():
                self._close_stream(stream)
                worker.join(timeout=2)
            if worker.is_alive():
                raise ExecutionError("owned workload output transport did not stop after namespace teardown")
            if errors:
                raise ExecutionError("owned workload transport closure remains unconfirmed after namespace teardown")
        self.held_executions.clear()

    def quiesce_unconfirmed_agent(self):
        """Stop every process in this owned PID namespace, retaining its files.

        A failed/old guest supervisor cannot publish a trustworthy completion
        acknowledgement. Container exit must be observed before restarting its
        original entry point and considering any partial submission for grading.
        """
        if not self.active.acquire(blocking=False):
            raise ExecutionError("cannot stop an unconfirmed agent during another task operation")
        try:
            container = self.container
            if container is None:
                raise ExecutionError("unconfirmed agent workspace is no longer available")
            container.reload()
            labels = container.attrs.get("Config", {}).get("Labels", {}) or {}
            if (labels.get("asb.owner") != "asb" or labels.get("asb.attempt") != self.attempt
                    or container.attrs.get("HostConfig", {}).get("PidMode") not in [None, "", "private"]):
                raise ExecutionError("refused process-tree reset outside this owned PID namespace")
            if container.attrs.get("State", {}).get("Running", True):
                container.kill()
            container.wait(timeout=125)
            container.start()
        except ExecutionError:
            raise
        except Exception as error:
            raise ExecutionError("could not confirm stopped agent tree before submission capture") from error
        finally:
            self.active.release()

    def close(self):
        if self.container is None:
            return
        labels = self.container.attrs.get("Config", {}).get("Labels", {}) or {}
        if labels.get("asb.owner") != "asb" or labels.get("asb.attempt") != self.attempt:
            self.cleanup_status = "ownership_mismatch"
            raise ExecutionError("refused cleanup of a container outside this attempt")
        try:
            self.container.remove(force=True)
            self.join_held_executions()
            self.cleanup_status = "removed"
            self.container = None
        except Exception:
            self.cleanup_status = "error"
            raise

    def __exit__(self, *_exception):
        self.close()
