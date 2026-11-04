# AgentSysPerf collector integration

## Purpose

This document defines an optional integration between ASB and
[AgentSysPerf](https://github.com/intel/agentsysperf).  ASB remains the owner
of agent/provider selection, execution, workload evaluation, recording and
result publication.  AgentSysPerf contributes a project-local analysis runtime
and measurement/analyzer vocabulary; it does not replace ASB's workload
adapters, process gate, grader, cassette, or report source of truth.

The integration must answer two different questions without conflating them:

* **agent performance:** metrics attributable to the selected agent process
  tree during ASB's execution interval; and
* **host interference:** optional node/cgroup observations that are explicitly
  labelled as host context and are never used to rank agents.

The latter is useful for capacity studies.  It is not a substitute for the
former.  In particular, AgentSysPerf's documented `perf stat -a` default is
not an acceptable source for an ASB agent-performance metric.

## Non-negotiable measurement contract

Every `agentsysperf:*` collector result carries this envelope before its
metrics can enter comparison:

```json
{
  "schema_version": 1,
  "collector": "agentsysperf:l3-perf",
  "scope": "agent-process-tree",
  "target": {"kind": "cgroup-v2", "identity": "opaque"},
  "execution_interval": {"start_ms": 0, "end_ms": 1234},
  "platform": {"architecture": "aarch64", "vendor": "arm"},
  "capabilities": {"requested": ["instructions"], "measured": ["instructions"]},
  "quality": {"complete": true, "scope_verified": true, "multiplexed": false},
  "metrics": {},
  "series": {},
  "artifacts": []
}
```

The real target identity is retained only in private ASB evidence.  Public
JSON never contains a PID, cgroup path, command line, prompt, credential, or
unbounded process output.

ASB accepts an agent-performance metric only when all of the following hold:

1. the target is the ASB process gate's verified agent tree or its exclusive
   delegated cgroup-v2 leaf;
2. every sample is clipped to the observed agent execution interval;
3. the producer reports complete capture and a verified target identity;
4. a counter is reported only when the running kernel exposes and accepts it;
5. shared-resource telemetry declares its scope (`core`, `socket`, or `host`)
   and is not relabelled as per-agent data.

Otherwise the result is an `unavailable` or `diagnostic` artifact.  It may
explain a run, but it cannot produce a comparative score, bar chart, or a
positive/negative bottleneck verdict.  A collector failure never changes an
independently established workload grade.

## Product shape

`asb tool install agentsysperf` installs a pinned, project-local Python
environment and ASB bridge entry point.  It must not modify the user's Python,
PATH, kernel settings, or global pip state.  `asb setup tui` treats it like
every other collector dependency: selecting a collector which needs it offers
project-local installation, shows progress, and keeps the collector unchecked
until a post-install capability probe succeeds.

The bridge is deliberately small.  It imports only the published AgentSysPerf
protocol/analyzer surface pinned by the tool manifest.  ASB-owned Rust code
continues to create and own process/cgroup targets, lifecycle boundaries,
capture custody, and report publication.  This avoids handing AgentSysPerf a
host-wide observation capability or letting it start a second benchmark
harness around an ASB run.

When an `agentsysperf:l1-resource` export has complete prerequisite evidence,
ASB invokes the project-local `agentsysperf analyze` command against those
records after the benchmark ends.  This invocation has no provider, agent, or
workload arguments; it is an offline post-processing step.  Its stdout and
stderr are retained only under the run's private measurement directory, each
limited to 1 MiB by the child process, and it is stopped after 10 seconds.
An unavailable, malformed, oversized, or failed analysis is recorded as an
`unavailable` analysis artifact and never changes the benchmark grade.

The initial selectable collector family is:

| Selection | Source | Result | Scope |
|---|---|---|---|
| `agentsysperf:l1-resource` | AgentSysPerf L1 vocabulary + ASB sampler | CPU/RSS/thread observations for ASB lifecycle spans | agent tree |
| `agentsysperf:l3-perf` | AgentSysPerf L3 vocabulary + ASB-owned `perf` | generic counters, IPC, cache/branch rates | agent tree/cgroup |
| `agentsysperf:analysis` | AgentSysPerf analyzers | bounded evidence-backed bottleneck findings | offline post-run |
| `agentsysperf:scaling` | AgentSysPerf scaling analyzer | sweep-only throughput/latency knee findings | offline post-sweep |
| `agentsysperf:intel`, `:amd`, `:armv8` | platform capability catalogs | vendor-specific optional telemetry | explicitly declared |

`analysis` and `scaling` consume validated normalized records; they never
launch an agent, a provider, or a workload.  A one-run analysis cannot claim a
concurrency knee.  The scaling collector requires a sweep with at least three
distinct observed concurrency points and compatible selected task populations.

### Scaling evidence contract

Select `agentsysperf:scaling` in `asb setup --measure`, or select **sweep
scaling** in the TUI's **AgentSysPerf modes** pane.  ASB installs the same
pinned project-local AgentSysPerf runtime as resource analysis and automatically
selects its process-tree prerequisites.  The selection is persisted in each
run's `agentsysperf.json`; it is not inferred later from a report name.

At comparison time ASB emits an AgentSysPerf scaling SVG only when it finds at
least three distinct configured-concurrency points for the same agent,
provider, model, workload and trial.  Every point must be one full, sealed ASB
execution cohort with observed AgentSysPerf records and resolved successful
work.  Partial cohorts, failed/unresolved slots, mixed trials, and repeated
cohorts at the same point are omitted.  The report shows p95 end-to-end latency
and a conservative aggregate service rate (completed work divided by the sum
of observed elapsed times).  It does **not** invent wall-clock throughput from
cohort membership, because admission provenance alone does not prove that
members overlapped.

## Platform telemetry

The catalog is capability-driven, not vendor-name-driven.  ASB detects the
architecture/vendor from sanitized system facts and probes `perf list` plus
the relevant kernel PMU devices.  It records the detected capability manifest
as an artifact.  A selection remains available only when its manifest is
supported; an unsupported backend is shown in the TUI with an explanation.

All platforms receive the portable baseline where available: task-clock,
cycles, instructions, branches, branch-misses, cache references/misses,
context switches, migrations and page faults.  These are named as observed
counters, not scaled estimates.

* **Intel:** generic baseline plus only kernel-advertised Topdown, RAPL,
  PerfSpect/PCM-compatible capabilities.  Socket power, uncore bandwidth and
  cache occupancy are socket observations, never attributed to an individual
  agent unless a dedicated isolated experiment proves that attribution.
* **AMD:** generic baseline plus only kernel-advertised AMD PMU/IBS, L3, energy
  and NUMA capabilities.  There are no copied Intel raw event encodings and no
  fabricated Zen-specific event aliases.
* **Armv8 (priority):** generic architectural PMU baseline plus dynamically
  discovered Arm PMU aliases (for example cache-refill, branch-mispredict and
  frontend/backend stall events), optional SPE, and kernel-exposed energy or
  thermal sources.  Neither a particular Arm core nor an SPE device is assumed.
  Events are selected by the host's advertised names/format and are omitted if
  unavailable.  Arm socket/package energy is host or socket context until
  isolation evidence establishes more.

Vendor-specific metrics are versioned by a `platform_manifest` containing the
sanitized CPU identifier, kernel PMU name, event alias, unit, scope, and
availability reason.  A missing metric is omitted, not recorded as zero.

## ASB-to-AgentSysPerf mapping

ASB phases map to AgentSysPerf phase labels only where ASB observes an actual
boundary:

| ASB evidence | AgentSysPerf phase | Meaning |
|---|---|---|
| provider/proxy preparation | `admit` | setup/control-plane time; not agent execution |
| agent-owned repository/tool inspection | `retrieve` | emitted only when an adapter supplies bounded evidence |
| private LLM request interval | `reason` | request duration, never prompt or response text |
| local tool/MCP interval | `act` | tool duration and status, never arguments/output |
| workload evaluation/publication | `commit` | separately labelled post-execution work |

No phase is guessed from residual wall time.  Existing lifecycle components
remain available as coarse spans; an absent nested span yields no phase metric.

## PR series and merge order

Each PR is independently reviewable, offline-testable, and safe to merge before
the next one.  All source-dependent tests use local fixture bridges and fake
`perf` output.  Live-provider runs are qualifications, never CI.

1. **Design and contracts.** Add this document, the public JSON envelope, the
   collector IDs, scope/quality taxonomy, and fixture goldens.  No new runtime
   dependency or changed benchmark behavior.
2. **Project-local AgentSysPerf tool.** Add `agentsysperf` to ASB's tool
   registry, pinned to an exact upstream revision and dependency lock; install
   it into `.asb-tools/agentsysperf`, expose a version/capability probe, and
   wire dependency/install progress through CLI and TUI.  Test installation
   with a local wheel/index fixture, not the network.
3. **Bridge and L1 import.** Add a private JSON-lines bridge protocol and the
   `agentsysperf:l1-resource` collector.  It converts ASB-owned, verified
   process-tree observations into AgentSysPerf-compatible records.  Test
   rejection of malformed output, stale PID, foreign process, incorrect
   interval, and source replacement.
4. **Scoped L3 and platform manifest.** Add `agentsysperf:l3-perf`, requiring
   the existing process gate/cgroup target.  Add generic, Intel, AMD and Armv8
   capability discovery with fixture catalogs.  Test that no unsupported raw
   event is passed to `perf`, and that system-wide `-a` is rejected for an
   agent-scoped collection.
5. **Vendor telemetry backends.** Add the selectable Intel, AMD and Armv8
   extensions.  Implement only event groups proven by their platform manifest;
   use generic counters as fallback.  Add x86 Intel, x86 AMD and aarch64/Arm
   fixture matrices plus an opt-in real-host qualification per available
   platform.  Armv8 is a required release gate for this series, not a later
   parity item.
6. **Analytics and report contribution.** Run AgentSysPerf-compatible
   CPU/cache/memory/breakdown analysis after validated collection.  Persist
   structured findings with their input metric IDs and coverage, render an ASB
   report section/SVGs, and suppress verdicts whose prerequisite data is absent.
7. **Scaling and bottleneck diagnosis.** Run the scaling analyzer only after
   compatible ASB sweep rows are sealed.  Add throughput/latency-vs-concurrency
   charts, confidence/coverage labels, and no-rerun guarantees.  Validate mixed
   providers, missing replicas, replay/live separation, and incompatible task
   populations.
8. **End-to-end qualification and release integration.** Run five bounded live
   OpenRouter `deepseek/deepseek-v4.1-flash` ASB executions after the preceding
   PRs are merged into an integration branch.  At least one run exercises L1,
   one generic L3, one platform backend, one analysis report, and one
   three-point sweep/scaling report.  Preserve private credentials, retain
   result paths/commit hashes/capability manifests, and repeat no failed agent
   merely to repair a collector.  ARMv8 live qualification runs on an actual
   aarch64 host; an x86 host cannot prove Arm telemetry.

PRs 1--7 are merge prerequisites for PR 8.  A GitHub outage may delay push or
merge, but not local commits, offline tests, or the final qualification plan.

## Acceptance criteria

The series is complete only when a fresh project can install the bridge with
`asb tool install agentsysperf` or the setup TUI, select supported collectors,
run a live benchmark, and receive ASB-native JSON/HTML evidence with exact
scope and capability provenance.  The same project must be able to run a
compatible sweep and receive a bounded, coverage-labelled scaling/bottleneck
report.  Intel, AMD and Armv8 unavailable paths must be discoverable and
non-blocking; supported paths must be validated on their actual architecture.
