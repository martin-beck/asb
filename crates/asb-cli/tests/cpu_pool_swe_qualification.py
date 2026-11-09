"""Opt-in, approximate offline two-replica SWE-bench CPU-pool qualification.

Requires ASB_TEST_ASB pointing to a build with CPU pools enabled,
ASB_TEST_SWE_SEED pointing to a project with installed swebench, pi, node and
swe-bench catalog, and ASB_TEST_TEMP_ROOT on a filesystem with ample space.
The selected local task image and Docker daemon must already be available.
This checks placement and cleanup; it does not claim a resolved benchmark.
Set ASB_TEST_FLAMEGRAPH to a directory containing the real flamegraph and
stackcollapse-perf.pl executables to additionally qualify CPU-wide perf.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    binary = Path(os.environ["ASB_TEST_ASB"]).resolve(strict=True)
    seed = Path(os.environ["ASB_TEST_SWE_SEED"]).resolve(strict=True)
    temp_root = Path(os.environ["ASB_TEST_TEMP_ROOT"]).resolve(strict=True)
    trial = os.environ.get("ASB_TEST_SWE_TRIAL", "sympy__sympy-14774")
    cpus = sorted(os.sched_getaffinity(0))[:2]
    if len(cpus) != 2:
        raise RuntimeError("qualification requires two allowed logical CPUs")
    with tempfile.TemporaryDirectory(prefix="asb-cpu-swe-", dir=temp_root) as temporary:
        project = Path(temporary)
        (project / "home").mkdir()
        environment = {"HOME": str(project / "home"), "PATH": os.environ.get("PATH", ""),
                       "ASB_INIT_OFFLINE": "1"}
        with (project / "init.log").open("wb") as log:
            subprocess.run([binary, "init"], cwd=project, env=environment, stdin=subprocess.DEVNULL,
                           stdout=log, stderr=subprocess.STDOUT, check=True, timeout=300)
        for name in ("swebench", "pi", "node"):
            shutil.copytree(seed / ".asb-tools" / name, project / ".asb-tools" / name, symlinks=True)
        shutil.copytree(seed / "workloads/swe-bench", project / "workloads/swe-bench", symlinks=True)
        flamegraph = os.environ.get("ASB_TEST_FLAMEGRAPH")
        if flamegraph:
            perf = shutil.which("perf")
            if perf is None:
                raise RuntimeError("CPU-wide qualification requires an installed perf executable")
            perf_bin = project / ".asb-tools/perf/bin"
            perf_bin.mkdir(parents=True)
            shutil.copy2(perf, perf_bin / "perf")
            graph_bin = project / ".asb-tools/flamegraph/bin"
            graph_bin.mkdir(parents=True)
            for name in ("flamegraph", "stackcollapse-perf.pl"):
                shutil.copy2(Path(flamegraph) / name, graph_bin / name)
            environment["ASB_MEASURE_PERF"] = "1"
        runtime_scope = '[runtime]\nperf_record_scope="cpu-wide"\n' if flamegraph else ''
        measures = '[measurements]\nenabled=true\ndirectory="measurements"\ncollectors=["perf","flamegraph"]\n' if flamegraph else ''
        (project / "benchmark.toml").write_text(
            'agent="pi"\nworkload="swe-bench"\nprovider="openrouter"\nmodel="test-model"\n'
            f'trial={json.dumps(trial)}\nconcurrency=2\nworkspace="workspace"\nlog_file="run.log"\n'
            + runtime_scope +
            f'[runtime.cpu_pool]\ncpus="{cpus[0]},{cpus[1]}"\nnum_cpus_per_instance=1\n'
            '[planning.budgets]\nenvironment_preparation_seconds=600\nexecution_seconds=60\nevaluation_seconds=120\n'
            + measures)
        request = json.dumps({"model": "test-model", "input": "offline", "tools": [{"type": "function"}]})
        response = json.dumps({"id": "resp_fixture", "object": "response", "output": [], "status": "completed"})
        cassette = {"request": {"method": "POST", "url": "/v1/responses", "body": request},
                    "response": {"status": 200, "headers": {"content-type": "application/json"}, "body": response}}
        (project / "tape.jsonl").write_text(json.dumps(cassette) + "\n")
        command = [binary, "run", "benchmark.toml", "--llm-replay", "--cassette", "tape.jsonl",
                   "--ignore-provider-mismatch", "--ignore-provider-unconsumed", "--ignore-agent", "--ignore-evaluation"]
        with (project / "run-output.log").open("wb") as log:
            completed = subprocess.run(command, cwd=project, env=environment, stdin=subprocess.DEVNULL,
                                       stdout=log, stderr=subprocess.STDOUT, timeout=900)
        if completed.returncode:
            raise RuntimeError("qualification failed: " + (project / "run-output.log").read_text(errors="replace")[-4000:])
        observed = {}
        for path in (project / ".asb/execution-plans").glob("*.json"):
            plan = json.loads(path.read_text())
            runtime, task = plan.get("runtime", {}), plan.get("task") or {}
            if runtime.get("placement_status") != "managed_container_verified_before_execution":
                continue
            if task.get("task_id") != trial:
                raise AssertionError("managed placement evidence belongs to a different trial")
            allocation = runtime["effective_boundary_cpus"]
            if runtime.get("proxy_placement_status") != "verified_before_exec" or runtime.get("proxy_launch_cpu_affinities") != [allocation]:
                raise AssertionError("proxy placement was not verified for its managed replica")
            inspected = subprocess.run(["docker", "inspect", task["attempt_id"]], capture_output=True, text=True)
            if not inspected.returncode or "no such object" not in inspected.stderr.lower():
                raise AssertionError("owned task container still exists or Docker inspection failed")
            observed[allocation[0]] = task["attempt_id"]
        if set(observed) != set(cpus):
            raise AssertionError(f"unexpected managed allocations: {observed}")
        outcomes = [json.loads((project / f"run-concurrency-{index}.log").read_text()) for index in (1, 2)]
        if not all(outcome.get("approximate_replay") for outcome in outcomes):
            raise AssertionError("fixture must retain its approximate replay status")
        if flamegraph:
            root = project / "results/benchmark-runs/run-001"
            report = (project / "results/benchmark-comparison/report.html").read_text()
            for index, cpu in enumerate(cpus, 1):
                directory = root / f"measurements-concurrency-{index}"
                scope = json.loads((directory / "perf-record-scope.json").read_text())
                stat = json.loads((directory / "perf-stat-plan.json").read_text())
                graph = json.loads((directory / "flamegraph.json").read_text())
                if scope.get("scope") != "cpu-wide" or scope.get("logical_cpus") != [cpu] or stat.get("scope") != "agent-process-tree" or graph.get("status") != "completed":
                    raise AssertionError(f"invalid perf/flamegraph scope for replica {index}")
                if "<svg" not in (directory / "flamegraph.svg").read_text():
                    raise AssertionError("flamegraph SVG is unavailable")
                decoded = subprocess.run([perf, "script", "-i", str(directory / "perf.data"), "-F", "cpu"],
                                         capture_output=True, text=True, check=True, timeout=60).stdout
                sample_cpus = {int(line.strip().strip("[]")) for line in decoded.splitlines() if line.strip()}
                if sample_cpus != {cpu} or f"CPU-wide perf recording on logical CPUs {cpu}." not in report:
                    raise AssertionError(f"CPU-wide samples or report scope differ for replica {index}: {sample_cpus}")
        print(json.dumps({"trial": trial, "managed_allocations": observed,
                          "cpu_wide_flamegraphs": bool(flamegraph), "approximate_replay": True,
                          "benchmark_correctness": "not qualified"}, sort_keys=True))


if __name__ == "__main__":
    main()
