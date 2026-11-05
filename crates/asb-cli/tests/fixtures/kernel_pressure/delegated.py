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
args = parser.parse_args()
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
for path in reports:
    result = json.loads(path.read_text())
    if result["status"] not in {"completed", "partial"} or not result["series"] or not result["target"]:
        raise SystemExit("Agent pressure did not publish observed scoped series: " + str(path))
    if result["target"]["pid"] not in observed:
        raise SystemExit("Pressure target was absent from the observed agent leaves")
    if receipt["psi_enabled"] and "cpu.some_stall_pct" not in result["metrics"]:
        raise SystemExit("Enabled PSI did not produce agent CPU pressure observations")
print("Verified agent leaves exclude ASB, fixture and unrelated CPU/memory load")
