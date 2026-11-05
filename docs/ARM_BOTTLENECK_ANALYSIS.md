# Arm telemetry and bottleneck analysis delivery

This series extends ASB's existing execution and collector lifecycle. Analysis
consumes observations after execution settlement and never launches an agent,
provider, grader or second benchmark harness. Raw captures remain authoritative;
derived findings include input references, rule versions, scope and limitations.

## Planned pull requests

1. Observation and analysis foundation: typed units, scopes and availability;
   a stateless rule registry; component-accounting observations and diagnostic
   findings published with measured attempts. Preserve existing components.
2. Arm PMU and platform adapters: bounded sysfs discovery, explicit PMU event
   groups, shared perf collection planning, interval counts and actual running
   fractions. Keep heterogeneous CPU domains separate. Add selectable adapters
   to CLI/TUI discovery and setup using the existing registry.
3. Linux kernel evidence adapters: cgroup CPU/memory/I/O/PSI, scheduler and
   synchronization observations, process-generation and namespace attribution.
   Reuse existing CPU-accounting, BPF and Perfetto capture rather than starting
   duplicate collectors. Disclose unavailable classification and capture loss.
4. Hardware analysis: CPU-specific Arm Topdown formulas and optional scoped SPE decoding.
   HiSilicon/other discovered uncore events remain capability metadata only:
   shared counters cannot isolate agent activity and must not become agent measurements.
   Unsupported CPUs/events produce explicit unavailable observations; no guessed
   encodings, simulated hardware counts or per-agent uncore apportionment.
5. Cohort and reporting integration: actual overlap and scaling, phase alignment,
   comparison-time offline regeneration, findings/coverage HTML, SVG and optional
   columnar exports. AgentSysPerf uses the same normalized evidence.
6. Qualification: deterministic offline fixtures and actual concurrent local
   workloads on node26, including collector loss, process churn and unrelated
   noise. Compare enabled/disabled instrumentation; retain receipts and overhead.

## Output and semantics

The observation contract uses explicit units and integer raw counters. Time
intervals identify their clock; sampled, accumulated thread time and elapsed
wall time are distinct. Missing data never becomes zero. Event definitions,
PMU/CPU identity, time-enabled/running, sample loss, source artifact and metric
policy revision belong to provenance, not graphable scalar metrics.

Diagnostics distinguish observed resource symptoms, supported explanations and
controlled-experiment confirmation. No uncalibrated confidence percentage or
claimed critical path is inferred from aggregate counters. In particular,
sleeping is not automatically storage/network wait, IPC is not a portable
efficiency score, and high system CPU alone does not prove a kernel defect.

Execution identity joins observations to the actual admitted replica and cohort.
Admission does not prove overlap: scaling analysis uses observed execution bounds.
Agents, tool descendants, container totals, shared services and host context
retain separate scopes. PID reuse, incomplete namespaces and capture loss cannot
silently widen scope. All collectors attach before release of the existing gate.
Heavy processing and grading remain behind the execution settlement barrier.

Agent activity excludes ASB, collector processes, graders, unrelated services and
other benchmark replicas. The perf producer validates the selected live process
generation and rejects ASB itself or an ancestor whose inherited tree includes
ASB before starting capture. Arm counters use that same producer, selected PID
and descendant inheritance, never system-wide or CPU-wide attachment. Platform
inventory is host capability metadata, not a measurement of system processes.
Future SPE, pressure and BPF adapters must prove equivalent process-tree/cgroup
scope; unavailable isolation produces unavailable data, never a host-wide fallback.
An advertised SPE/uncore PMU alone does not establish scoped measurement support.

Native execution uses the actual stopped agent principal beneath its guardian
for every process-scoped collector. Guardian custody/cleanup remains separate;
the guardian and ASB controller are excluded from the inherited measurement
tree. The principal is bound to both process generations and parent links before
attachment, and resumes through the owned principal's pidfd after collectors
and any cohort rendezvous are ready. Native execution timing begins at that
release boundary rather than counting guardian/collector preparation as agent
execution. `measurement-target.json` records the selected principal and excluded
guardian/controller identities without command text or environment values.
CPU accounting may move only that verified stopped principal into its exclusive
leaf; container execution continues to preserve existing container membership.

## Qualification target

Read-only discovery on node26 found a Kunpeng-920 with Arm PMUv3, SPE and
HiSilicon DDR/L3/HHA PMUs. Cycles and instructions are readable through perf.
The generic cycles alias also expanded to an unsupported SMMU PMU, demonstrating
why the Arm adapter must select the discovered CPU PMU explicitly. This probe
does not yet qualify the proposed adapters or per-replica attribution.

## Completion ledger

PR #1128 delivered the component-observation schema, stateless rule registry and
initial diagnostic artifact, with all 68 measurement tests passing. It merged
after its required CI succeeded. CPU-specific and cross-collector bottleneck
analysis remains in the later steps above.

The `arm-platform` collector adds bounded advertised-capability discovery through
the ordinary collector registry. It writes `arm-platform.json` with CPU/SPE/uncore
domains, PMU type, CPU masks, event aliases and format descriptions. Select it in
the measurements list or setup measure selection. It does not start perf or claim
that advertised events have been successfully measured. Non-AArch64 hosts write
an explicit unavailable record. Discovery is bounded to 256 PMUs, 256 entries per
event/format directory, 512 bytes per value and 4096 shared event/format fields.
This slice merged as PR #1129 after its required CI passed.

Native discovery qualification on node26 passed the opt-in
`native_arm_inventory_matches_exposed_cpu_pmus` test, yielding 64 PMUs (one core
PMU with 48 aliases, one SPE PMU and 62 uncore PMUs). The AArch64 test binary
used its matching loader/libraries in a private directory because the host's
older glibc could not start the cross-built executable directly. No host library
was replaced. The retained inventory verifies capability discovery only; PMU
counter scope, SPE decoding and bottleneck attribution require later tests.

The `arm-pmu` slice requests one event group per discovered CPU PMU through the
existing `perf-stat` producer. Each group contains advertised cycles, retired
instructions and available L1D refill, L1D TLB refill, branch-misprediction and
backend-stall events (at most six). This replaces generic hardware aliases for
that session, avoiding duplicate AgentSysPerf counter requests and SMMU alias
expansion. Existing software counters and stack sampling remain available.
All consumers share one attempt-frozen platform inventory. On non-Arm hosts,
the Arm adapter reports unavailable without suppressing other perf consumers.

`arm-pmu-plan.json` records requested groups; `arm-pmu.json` projects validated
per-PMU quality from `perf-stat-quality.json`. Exact unsigned integer counts are
retained separately from floating-point graph values. Missing/unschedulable
counters remain absent, and overflow or malformed counters invalidate parsing.
Quality retains actual event runtime and rounded running percentage; no reverse
scaling or CPU utilization is inferred. CPU-specific ratios and Topdown analysis
remain later work. The complete six-event group was directly tested on node26
at 100 ms intervals with all six events scheduled at reported 100% running time;
this is hardware/event qualification, not yet full concurrent ASB qualification.
The grouped-counter slice merged as PR #1130 after required CI succeeded.

## Offline diagnostic integration

The next engine slice derives IPC, L1D/TLB refills and branch mispredictions per
1000 instructions, plus backend-stall cycles as a percentage. Each CPU PMU keeps
its own raw integer counts and definitions. Ratios require the same explicitly
enabled event group, equal observed runtimes, nonzero denominators, no missing
or unknown-quality rows and reported 100% running percentages. The percentages
are rounded by perf: eligibility is not proof of whole-run completeness. Missing
inputs retain an unavailable reason. These ratios do not implement a Neoverse
Topdown specification on unsupported CPUs or distinguish kernel/user execution.

Kernel diagnostics are stateless registered rules over existing collector data:
scoped, complete Perfetto thread states provide runnable-wait share of active
thread time; BPF profiles provide completed kernel-lock contention and futex
syscall durations. Lost/unscoped/unfinished scheduler evidence cannot produce a
ratio. Futex syscall duration includes wake operations and descheduling and must
not be labelled exact blocked-lock time. The rules do not add overlapping thread
waits to elapsed wall time or make causal performance claims.

Run finalization writes these findings to `bottlenecks.json`. Comparison recomputes
them offline from bounded retained source documents rather than accepting saved
conclusions. `comparison.json` includes the regenerated analysis; HTML details
show findings, units, limitations and unavailable reasons. Available derived
metrics use the existing SVG/interactive graph pipeline with explicit units.
Original result/analysis artifacts are preserved during regeneration. Existing
per-attempt identities and scope checks remain the source-loading boundary.

No series-wide completion is claimed until all six steps have merged, their
required CI checks pass, and retained Arm qualification verifies actual output,
scope, overhead, analysis and report behavior. Each PR records its own tests and
remaining delivery work. The CPU-specific methodology follows Arm Telemetry
Solution; hardware availability is discovered on the measured host.

## Agent-only kernel pressure

Select `kernel-pressure` in the ordinary collector list or setup measures tab.
It depends on CPU accounting and reuses only its ASB-created exclusive native
agent cgroup; it never uses `/proc/pressure`, the ASB cgroup or a whole-container
cgroup as a measurement fallback. Missing delegation yields an explicit
unavailable record, including for container execution without an exclusive leaf.
No extra tool installation or privileged host mutation is performed.

Capture begins once both agent execution-begin and gate attachment are observed,
in either order, and
stops at execution-end/failure before grading. The sampler reads bounded cgroup
fields every 100–1000 ms using the configured measurement interval. It retains
at most 4096 timestamped snapshots, downsampling older points when needed;
actual elapsed timestamps, counter-reset/read failures and a downsampling flag
remain in the result. Missing execution-end cannot publish valid measurements.

`kernel-pressure.json` contains raw initial/final unsigned counters, per-field
unavailable reasons, derived metrics and timestamped interval series for:

- CPU/memory/I/O PSI some/full occupancy and cumulative stall milliseconds;
- CPU bandwidth throttled duration and period counters;
- charged current/sampled-peak memory, memory.high/max and OOM/kill events;
- cgroup block read/write bytes, operation counts and interval rates.

An empty successfully read `io.stat` is a kernel-defined zero-device set; an
unreadable file is unavailable. Counter decreases or intermediate read failures
invalidate the affected metric rather than publishing partial totals as complete.
Sampler maxima are observed maxima, not guaranteed peaks. Memory charges may
exclude shared pages charged elsewhere. PSI some/full and different resource
stalls overlap; they cannot be added to thread time or interpreted as an additive
wall-time breakdown. Accumulated CPU throttling can exceed elapsed wall time.

The bounded `kernel-pressure-summary.json` supplies offline registered pressure,
throttling and memory-limit diagnostics. Comparison regenerates findings and
produces the normal JSON, SVG line/bar charts and interactive HTML, with bytes,
bytes/s, ms/s and percentage units. The summary contains observations rather
than a second graph metric set. Findings describe symptoms, not proven causes.

Linux must expose the corresponding controllers and enable PSI. On a kernel
built with `CONFIG_PSI_DEFAULT_DISABLED=y`, PSI requires `psi=1` in that
measurement environment's boot configuration. Node26 currently uses this
default-disabled configuration; its absent PSI is a tested unavailable case,
not permission to substitute global pressure data or change its boot settings.

The opt-in `tests/fixtures/kernel_pressure/delegated.py` qualification runs in a
disposable delegated systemd service. It separates the ASB/fixture/noise control
group from an empty agent parent, enables only available CPU/memory/I/O
controllers there, and launches the public ASB run command. Supply a frozen
executable and initialized synthetic-client project with a local tape and
`pressure.toml` at concurrency two. Pass the user's executable PATH to the
service when Node is user-installed. This fixture changes only its own transient
cgroup subtree and neither contacts a provider nor runs in ordinary CI.

Retained native qualification passed on x86_64 with PSI active: both synthetic
replicas graded successfully and each produced 18 populated timestamped series.
Observed agent memory maxima were about 49/38 MiB while an unrelated 256 MiB
allocation and CPU load remained in a sibling control group. Scope snapshots
exclude ASB, the fixture and the noise PIDs. Equivalent AArch64 qualification on
node26 passed both grades with 12 accounting series per replica, six explicit
unavailable PSI fields, and observed Arm core counters. JSON/HTML diagnostics
and 18 local time-series SVGs were checked. These are synthetic CPU/I/O client
qualifications, not a live-agent benchmark, full sampling-overhead study or
qualification of the still-planned SPE adapter.

## Optional process-scoped SPE

Select `arm-spe` explicitly; it is not part of the default collector set. It
depends on the platform inventory and the ordinary perf tool/capture adapter.
The separate SPE producer uses the dedicated SPE hardware, leaving the shared
core-PMU counters and ordinary stack sampling intact. All use the same selected
agent root and inherit its descendants. SPE requires the stopped agent launch
gate, one unambiguous advertised SPE domain and timestamp/load/store controls.
Unsupported hosts, domains or gate layouts are unavailable; there is no
system-wide fallback or guessed event encoding.

Default sampling period is 1,048,576 operations with a 256 MiB raw capture budget.
Adapter-owned selections can configure `arm-spe:period=65536:budget-mib=512`.
Period is bounded to 65,536–1,073,741,824; raw budget to 16–1024 MiB. Unknown,
duplicate or conflicting options are rejected. Capture uses bounded mmap
buffers, acknowledged activation and the existing process/capture guardians.
The maximum-output-size argument includes perf's required size suffix. A budget
stop without complete root/descendant exit metadata cannot produce valid metrics:
increase the period or budget and rerun. Existing captures are preserved.

`arm-spe.data` retains the raw recording. Validation checks bounded perf record
framing, explicit lost-event/sample records, AUX flags/payloads, fork ancestry,
unambiguous task lifetimes and recorded exits. The bounded offline built-in perf
decoder must exit successfully without diagnostics; no script extension,
provider or network symbol lookup is used. Every decoded sample must match a
recorded task generation, allowing only the decoder's disclosed sub-microsecond
timestamp rounding. This first decoder rejects any reused task ID, including
non-overlapping generations, rather than guessing a generation. Missing scope,
unknown events, ambiguous IDs, incomplete
captures or invalid decoding retain evidence without publishing numeric results.

`arm-spe-quality.json` retains the requested period, validated task metadata,
exact sampled counts, observed task identities, sample times and latency-weight
sum as an exact decimal string. `arm-spe.json` supplies sampled classifications,
known operation-latency weights in cycles and one-second interval series through
the normal report pipeline. Zero/unknown weights cannot become zero latency.
Series start at the first observed sample; absent intervals are not fabricated
zeroes, and the final bin can be partial. Capture includes the launch transition
inside the selected agent root; ASB controller, collectors and unrelated tasks
are outside the inherited tree.

Offline analysis reports classifications per 1000 sampled memory operations
and known mean operation-latency cycles in a separate sampling family. These
are not full cache-access counts, cache miss rates, retired instructions, DRAM
service latency or an architecture-independent efficiency score. Sampling bias
and unreported loss remain possible despite validated capture metadata.
The kernel/perf semantics follow the [Linux SPE documentation](https://github.com/torvalds/linux/blob/master/tools/perf/Documentation/perf-arm-spe.txt).


An offline two-replica probe on a Kunpeng-920 ARMv8 host exercised real SPE,
core PMU and cgroup collection with a synthetic Pi-compatible Node client and
bundled bug-fix grading. Both replicas produced populated validated SPE series
and passed grading. A sibling CPU/memory noise process, the ASB controller and
its guardians were excluded by the observed agent cgroup membership and the
validated task ancestry. This qualifies the exercised native launch/collection
path; it is not a live Pi/provider or external-workload qualification. The
host had PSI disabled, so unavailable PSI remained explicit. Retained raw
record diagnostics included perf's optional `discard` format lookup warning;
raw AUX loss flags were zero and decoding had no diagnostics. Neither outcome
proves absence of sampling bias or unreported hardware loss.

AgentSysPerf exports retain only agent-scoped source observations. Its former
host-context CPU/load/memory sampler is removed: diagnostic context is not a
reason to capture unrelated system activity. Static CPU/PMU capability inventory
remains separate from runtime measurements. Missing agent evidence is explicitly
unavailable, without falling back to host `/proc/stat`, load averages or memory
availability.


## Shared normalized counter input

`perf-stat-plan.json` records the generation-bound agent root, excluded ASB
controller and guardian, explicit event groups, inheritance and absence of
system-wide collection. Generic and hybrid core counters retain exact `u64`
counts in `perf-stat-quality.json`, just like the Arm counters. An adapter-owned,
bounded `perf list --no-desc` probe discovers advertised aliases; it never
selects uncore events or guesses a raw encoding. Hybrid core and atom groups
are requested independently.

The shared `bottlenecks.json` counter groups retain raw integers separately
from counters with complete, reported 100% scheduling and identical integral
runtime. IPC requires compatible cycles and instructions and a positive cycle
denominator. Missing optional events stay missing; failure, missing scope,
unknown quality and incompatible runtime cannot become a ratio. Rounded perf
percentages do not prove absence of unreported loss, and these are observed
unscaled counts rather than whole-run estimates or utilization.

All adapters finalize before normalized analysis is published. AgentSysPerf
then consumes the current in-memory result, preserving it as
`agentsysperf-normalized-input.json`; publication failure cannot select an old
artifact or raw scalar fallback. Both qualified agent CPU components are
required for total CPU time. Its compatibility L3 record supports one PMU
domain; multiple domains remain separate in normalized JSON and are not
combined into a misleading single record. Comparison regenerates the same
normalization offline and exposes group quality and provenance in HTML.


## Observed native agent lifetime windows

Measured native launches now publish `execution-window.json`. A bounded observer
checks only the already verified principal's process generation, at most once
per 100 ms in the existing capture loop. It does not start another collector
process, depend on terminal progress, inspect other tasks or widen scope.
The `CLOCK_MONOTONIC` domain is identified by a digest of boot and observer time
namespace identities. Domain changes, failed execution observation, unsupported
hosts and fewer than two live observations leave the interval unavailable.
This is distinct from SPE/perf timestamps; no implicit clock conversion exists.

The stored start is after the first successful live observation, and the end
is before the last. Both observation uncertainties and unsampled launch/exit
tails are excluded. A principal that has exited cannot extend the interval;
remaining descendants are not guessed. The interval proves a conservative
principal lifetime, including sleeping/stopped time, not CPU residency or
continuous useful work. An analysis can compare these bounds only within an
identical clock domain. Dispatch admission and per-session elapsed times remain
insufficient to prove overlap. Container/direct launches have no fallback to
an ASB controller or container management process.

The clock policy follows the Linux manuals for
[CLOCK_MONOTONIC](https://man7.org/linux/man-pages/man2/clock_gettime.2.html) and
[time namespaces](https://man7.org/linux/man-pages/man7/time_namespaces.7.html).


## Cohort overlap and phase completion bounds

`asb compare` now regenerates `cohort-analysis.json`, includes the same data in
`comparison.json`, and renders an HTML table plus up to sixteen lifetime plots.
It checks actual replica indices and admitted membership, workload/agent/model/
provider compatibility, distinct principal generations, matching target receipts
and a common boot/time-namespace clock. Missing members or incompatible evidence
make a cohort explicitly unavailable. Sparse admitted memberships remain sparse;
selected concurrency is never substituted for observed concurrency.

Each verified principal interior lifetime contributes to a lower bound on peak
live principals, time with multiple principals alive, and all-principal overlap.
Sleeping and stopped time count as alive; these are not CPU-residency claims.
A second pair of monotonic observations encloses the measured phase: before
releasing the stopped principal, and immediately after its owned process tree
has exited, before pipe draining. The earliest release to latest observed exit
is an upper bound on the cohort's agent-phase span. Completed agent phases
divided by that span is therefore a lower-bound phase completion rate, not
resolved-task throughput, end-to-end latency, provider service rate or a speedup.
Neither missing overlap nor missing clocks is turned into a zero measurement.
A real zero overlap is reported only when comparable validated intervals show it.

All computation is offline over retained source artifacts. Previously saved
cohort conclusions are ignored; source results are preserved. The timeline SVGs
show the verified interior as a solid bar and enclosing phase as a dashed line,
with numbered time axes in seconds and the ordinary report enlargement controls.
The cohort table always describes full admitted membership; plot filtering does
not recompute a different cohort. Historical receipts without enclosing phase
bounds remain unavailable for this combined analysis, rather than being backfilled.


When at least two actual admitted widths have observed cohorts in the same
configuration-name and clock-domain group, comparison also emits descriptive
phase-rate scatter plots and a `scaling` record. Every repeated cohort remains
its own point. The graph uses actual admitted width, not configured concurrency
or a guessed CPU utilization, and never computes a speedup from two lower bounds.
Identical names/boot do not establish identical inputs, budgets, instruments,
host contention or provider behavior; the report discloses these limits rather
than labeling the points a controlled scalability experiment.
## CPU-specific methodology registry

Arm platform inventory also records bounded per-online-CPU MIDR identities.
This is capability metadata, not system/process activity. A shared static CPU
methodology registry matches every CPU advertised by each core PMU domain;
it never selects a processor from the first CPU, hostname or PMU name alone.
Missing identities are unavailable and unregistered/mixed domains unsupported.
The generic measured event ratios remain independent of methodology support.

The first registered method is Neoverse N1 Cycle Accounting, identified by
implementer 0x41 and part 0xd0c. It reports frontend and backend stalled cycles
independently as percentages of matching observed CPU cycles. These are not
four additive slot categories, elapsed-time fractions or proof of a bottleneck.
On a matched N1 domain the shared producer adds an advertised `stall_frontend`
event to the existing six-event group, preserving the existing events. N1's
six programmable counters plus dedicated cycle counter can represent that
seven-event group. Missing/unavailable/multiplexed counters still cannot produce
ratios; inconsistent N1 stalled-cycle counts above matched cycles are unavailable.
No event encoding is copied from another processor or invented from a name.

The registered formulas and semantics follow Arm's
[Neoverse N1 Core Telemetry Specification, 108070_0200_02](https://documentation-service.arm.com/static/66f7154a7243ed1626433643)
and [Neoverse N1 PMU Guide, Issue 2.0](https://documentation-service.arm.com/static/66ace6ee0469d5197d40c93e).
Kunpeng and other unregistered processors explicitly have no N1 methodology;
their scoped generic counter and SPE results remain available where validated.
This first profile does not implement Neoverse N2/V1/V2/V3 slot formulas, apply
silicon errata workarounds, or claim native N1 hardware qualification from a
Kunpeng run. Deterministic fixtures check the N1 formulas and domain rejection.


Comparison JSON preserves the full captured Arm platform inventory separately
from scalar measurements. HTML exposes that inventory as escaped capability
metadata beside the CPU methodology status, including per-CPU identities and
PMU definitions. Those static fields never become agent activity graphs.
Ratios whose reported event runtimes cannot be compared as exactly representable
integral nanoseconds remain unavailable; large/fractional f64 runtimes cannot
silently establish group coherence after rounding.

## Memory scope independent of launcher discovery

Tree memory sampling uses the live generation-bound agent root below its ASB
controller. Resolving a script interpreter or proving an inherited PATH is not
required to observe that tree. Worker census still requires the exact selected
entry point and stays unavailable when that identity cannot be resolved.
ASB itself, its ancestors and the owned guardian cannot become memory roots;
changing roots after attachment invalidates that capture rather than mixing data.

The stopped native principal initially retains its ASB parent's image before
executing the application. RSS sampling waits until a non-ASB image is visible
and excludes the known parent/controller images from descendant observations.
Missing image access or short application lifetimes remain unavailable. This
preserves sampled lower-bound semantics instead of publishing ASB's inherited
pre-exec memory as an agent peak. Namespace export/import keeps bounded aggregate
values and its explicit availability/completeness flags.
