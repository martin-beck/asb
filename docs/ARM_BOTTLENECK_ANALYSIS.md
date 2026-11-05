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
4. Hardware analysis: CPU-specific Arm Topdown formulas, optional SPE decoding,
   HiSilicon/other discovered uncore events and shared host capture ownership.
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

## Qualification target

Read-only discovery on node26 found a Kunpeng-920 with Arm PMUv3, SPE and
HiSilicon DDR/L3/HHA PMUs. Cycles and instructions are readable through perf.
The generic cycles alias also expanded to an unsupported SMMU PMU, demonstrating
why the Arm adapter must select the discovered CPU PMU explicitly. This probe
does not yet qualify the proposed adapters or per-replica attribution.

## Completion ledger

No series-wide completion is claimed until all six steps have merged, their
required CI checks pass, and retained Arm qualification verifies actual output,
scope, overhead, analysis and report behavior. Each PR records its own tests and
remaining delivery work. The CPU-specific methodology follows Arm Telemetry
Solution; hardware availability is discovered on the measured host.
