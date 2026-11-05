#!/usr/bin/env python3
"""Opt-in offline native scope probe inside a disposable delegated service.

Run with systemd-run --wait --pipe --collect --uid=USER -p Delegate=yes
-p CPUAccounting=yes -p MemoryAccounting=yes -p IOAccounting=yes
--unit=asb-pressure-UNIQUE python3 THIS_FILE --asb FROZEN --project FIXTURE
--config benchmark.toml. The project must contain a synthetic Pi-compatible
client and completed local tape.jsonl. No provider credentials are used.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--asb", type=Path, required=True)
parser.add_argument("--project", type=Path, required=True)
parser.add_argument("--config", required=True)
parser.add_argument("--require-series", action="append", default=[],
                    help="also require populated metrics and time series for this collector")
parser.add_argument("--require-windows", action="store_true",
                    help="require same-domain verified agent lifetime overlap")
parser.add_argument("--require-cohort-analysis", action="store_true",
                    help="also validate regenerated overlap/rate evidence and report graphs")
args = parser.parse_args()
if args.require_cohort_analysis:
    args.require_windows = True
binary = args.asb.resolve(strict=True)
project = args.project.resolve(strict=True)
membership = Path("/proc/self/cgroup").read_text().strip().split("::", 1)[1]
parent = Path("/sys/fs/cgroup") / membership.lstrip("/")
if not parent.name.startswith("asb-pressure-") or not parent.name.endswith(".service"):
    raise SystemExit("Use a fresh disposable asb-pressure-*.service with delegation")
control = parent / "control"
agents = parent / "agents"
control.mkdir()
agents.mkdir()
(control / "cgroup.procs").write_text(str(os.getpid()))
controllers = set((parent / "cgroup.controllers").read_text().split())
selected = sorted(controllers.intersection({"cpu", "memory", "io"}))
(parent / "cgroup.subtree_control").write_text(" ".join("+" + x for x in selected))
(agents / "cgroup.subtree_control").write_text(" ".join("+" + x for x in selected))
observations = []
stopped = threading.Event()


def observe():
    while not stopped.wait(0.05):
        entries = {}
        for leaf in agents.iterdir():
            if leaf.is_dir():
                try:
                    entries[leaf.name] = [int(x) for x in (leaf / "cgroup.procs").read_text().split()]
                except FileNotFoundError:
                    pass
        observations.append({"monotonic_ns": time.monotonic_ns(), "leaves": entries})


# Unrelated memory/CPU load deliberately stays in the control sibling. Its
# allocation must never become part of an agent leaf's charged-memory gauge.
noise = subprocess.Popen([sys.executable, "-c", "import time; x=bytearray(256*1024*1024); end=time.monotonic()+90\nwhile time.monotonic()<end: sum(range(10000))"])
thread = threading.Thread(target=observe)
thread.start()
runner = None
status = None
try:
    env = dict(os.environ, ASB_CGROUP_PARENT=str(agents))
    runner = subprocess.Popen([str(binary), "run", args.config, "--llm-replay", "--cassette", "tape.jsonl"], cwd=project, env=env)
    status = runner.wait(timeout=120)
finally:
    if runner is not None and runner.poll() is None:
        runner.terminate()
        runner.wait(timeout=10)
    stopped.set()
    thread.join()
    noise.terminate()
    noise.wait(timeout=10)
    receipt = {"runner_pid": runner.pid if runner else None, "noise_pid": noise.pid,
               "fixture_pid": os.getpid(), "controllers": selected, "exit": status,
               "observations": observations, "psi_enabled": Path("/proc/pressure/cpu").exists()}
    with (project / "qualification-scope.json").open("x") as output:
        json.dump(receipt, output)
if status != 0:
    raise SystemExit("ASB qualification failed; retained logs and scope receipt")
observed = [p for row in observations for pids in row["leaves"].values() for p in pids]
if not observed or any(p in observed for p in [runner.pid, noise.pid, os.getpid()]):
    raise SystemExit("Agent cgroup scope was absent or included fixture/ASB/noise")
reports = list(project.glob("results/pressure-runs/run-*/**/kernel-pressure.json"))
if len(reports) != 2:
    raise SystemExit("Expected exactly two retained pressure result documents")
windows = []
for path in reports:
    result = json.loads(path.read_text())
    if result["status"] not in {"completed", "partial"} or not result["series"] or not result["target"]:
        raise SystemExit("Agent pressure did not publish observed scoped series: " + str(path))
    if result["target"]["pid"] not in observed:
        raise SystemExit("Pressure target was absent from the observed agent leaves")
    if receipt["psi_enabled"] and "cpu.some_stall_pct" not in result["metrics"]:
        raise SystemExit("Enabled PSI did not produce agent CPU pressure observations")
    target = json.loads((path.parent / "measurement-target.json").read_text())
    if target["source"] != "owned-stopped-native-principal" or target["root"]["pid"] != result["target"]["pid"]:
        raise SystemExit("Pressure root is not the verified actual agent principal")
    if target["excluded_guardian"]["pid"] in observed or target["excluded_controller_pid"] in observed:
        raise SystemExit("Agent cgroup included ASB guardian/controller activity")
    if args.require_windows:
        window = json.loads((path.parent / "execution-window.json").read_text())
        if (window["status"] != "observed" or window["root"] != target["root"]
                or window["clock"] != "CLOCK_MONOTONIC"
                or window["scope"] != "verified-native-agent-principal-lifetime"
                or not window["clock_domain"] or window["live_observations"] < 2
                or window["start_bound_ns"] >= window["end_bound_ns"]):
            raise SystemExit("Missing valid verified native lifetime window: " + str(path))
        windows.append(window)
    for collector in args.require_series:
        if not collector or not all(c.isascii() and (c.isalnum() or c == '-') for c in collector):
            raise SystemExit("Invalid collector identifier for series qualification")
        observed_collector = json.loads((path.parent / (collector + ".json")).read_text())
        if observed_collector["status"] != "completed" or not observed_collector["metrics"] or not observed_collector["series"]:
            raise SystemExit("Collector did not publish validated series: " + collector)
if args.require_windows:
    if len(windows) != 2 or len({w["clock_domain"] for w in windows}) != 1:
        raise SystemExit("Replica lifetime clocks are not comparable")
    overlap = min(w["end_bound_ns"] for w in windows) - max(w["start_bound_ns"] for w in windows)
    if overlap <= 0:
        raise SystemExit("No observed live principal overlap")
    print("Verified native agent lifetime overlap_ms=" + str(overlap / 1_000_000))
if args.require_cohort_analysis:
    report = project / "results/pressure-comparison"
    evidence = json.loads((report / "cohort-analysis.json").read_text())
    cohorts = evidence["cohorts"]
    if len(cohorts) != 1 or cohorts[0]["status"] != "observed":
        raise SystemExit("Comparison did not produce exactly one observed cohort")
    cohort = cohorts[0]
    if cohort["peak_live_principals_lower_bound"] != 2:
        raise SystemExit("Comparison did not observe two simultaneous live principals")
    expected_overlap = overlap / 1_000_000
    if abs(cohort["all_principals_overlap_lower_bound_ms"] - expected_overlap) > 1e-6:
        raise SystemExit("Comparison overlap does not match retained native observations")
    phase_span = max(w["exit_bound_ns"] for w in windows) - min(w["release_bound_ns"] for w in windows)
    if phase_span <= 0 or abs(cohort["completed_agent_phases_per_second_lower_bound"] - 2e9 / phase_span) > 1e-9:
        raise SystemExit("Comparison rate does not match the observed phase envelope")
    summary = json.loads((report / "comparison.json").read_text())
    if summary["cohort_analysis"] != evidence:
        raise SystemExit("Comparison JSON disagrees with separate cohort evidence")
    html = (report / "report.html").read_text()
    if (not (report / "cohort-lifetimes-0001.svg").is_file()
            or "cohort-lifetimes-0001.svg" not in html or "cohort-analysis.json" not in html):
        raise SystemExit("Cohort report is missing its graph or evidence link")
    print("Verified offline cohort overlap, phase-rate bounds, JSON and HTML/SVG")
print("Verified agent leaves exclude ASB, fixture and unrelated CPU/memory load")
