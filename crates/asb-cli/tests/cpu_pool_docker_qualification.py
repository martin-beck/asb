"""Explicit Linux/Docker CPU-pool qualification; never run in ordinary CI.

Requires two allowed host CPUs, local Docker access, system-wide perf permission,
and ASB_TEST_IMAGE naming a locally installed native image with Python 3. The
test owns only uniquely named containers and a private temporary output tree.
"""
import concurrent.futures
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / "assets/benchmarks"))
import asb_execution as execution
import docker


def main():
    cpus = sorted(os.sched_getaffinity(0))[:2]
    if len(cpus) != 2:
        raise RuntimeError("qualification requires at least two allowed logical CPUs")
    image_name = os.environ.get("ASB_TEST_IMAGE")
    if not image_name:
        raise RuntimeError("set ASB_TEST_IMAGE to a locally installed native image with Python 3")
    client = docker.from_env()
    image = client.images.get(image_name)
    root = Path(os.environ.get("ASB_TEST_TEMP_ROOT", ".")).resolve()
    token = uuid.uuid4().hex[:10]
    workspaces, recorders = [], []
    independent = None
    try:
        with tempfile.TemporaryDirectory(prefix="asb-cpu-qualification-", dir=root) as directory:
            for cpu in cpus:
                workspace = execution.ManagedDockerWorkspace(
                    client, image, "asb-" + uuid.uuid4().hex, "/", cpu_set=[cpu], offline=True)
                workspace.__enter__()
                workspaces.append(workspace)
            independent = client.containers.create(
                image.id, name="asb-independent-" + token,
                command=["/bin/sh", "-c", "exec sleep 30"], detach=True, network_mode="none")
            independent.start()
            independent_status = independent.exec_run(
                ["/bin/sh", "-c", "grep Cpus_allowed_list /proc/self/status"]).output.decode().strip()
            for cpu in cpus:
                path = Path(directory) / f"cpu-{cpu}.data"
                process = subprocess.Popen(
                    ["perf", "record", "-e", "cpu-clock", "-g", "-F", "199", "-a", "-C", str(cpu),
                     "-o", str(path), "--", "sleep", "5"], stdout=subprocess.DEVNULL,
                    stderr=subprocess.PIPE, text=True)
                recorders.append((cpu, path, process))
            time.sleep(1)

            def burn(container, name, cpu):
                code = ("import ctypes,os,time; ctypes.CDLL(None).prctl(15, %r, 0, 0, 0); "
                        "os.sched_setaffinity(0,{%d}); end=time.monotonic()+2; x=0\n"
                        "while time.monotonic()<end: x+=1") % (name.encode(), cpu)
                result = container.exec_run(["python3", "-c", code])
                if result.exit_code:
                    raise RuntimeError(result.output.decode(errors="replace"))

            with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
                futures = [pool.submit(burn, workspace.container, f"managed{index}", cpu)
                           for index, (workspace, cpu) in enumerate(zip(workspaces, cpus))]
                futures.append(pool.submit(burn, independent, "independent0", cpus[0]))
                for future in futures:
                    future.result()
            results = {}
            for index, (cpu, path, process) in enumerate(recorders):
                _, stderr = process.communicate(timeout=20)
                if process.returncode:
                    raise RuntimeError(stderr)
                decoded = subprocess.run(["perf", "script", "-i", str(path), "-F", "comm,cpu"],
                                         capture_output=True, text=True, check=True).stdout
                lines = [line.split() for line in decoded.splitlines() if line.strip()]
                names = {line[0] for line in lines}
                if (f"managed{index}" not in names or f"managed{1-index}" in names
                        or (index == 0 and "independent0" not in names)):
                    raise AssertionError(f"unexpected sampled processes on CPU {cpu}: {sorted(names)}")
                if any(int(line[-1].strip("[]")) != cpu for line in lines):
                    raise AssertionError(f"recording contained a sample outside CPU {cpu}")
                results[str(cpu)] = {"sample_names": sorted(names), "samples": len(lines)}
            print(json.dumps({"managed_cpu_sets": cpus, "independent_affinity": independent_status,
                              "recordings": results}, sort_keys=True))
    finally:
        for _, _, process in recorders:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        if independent is not None:
            independent.remove(force=True)
        for workspace in workspaces:
            workspace.close()


if __name__ == "__main__":
    main()
