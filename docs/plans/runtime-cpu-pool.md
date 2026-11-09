# Runtime CPU pool and CPU-wide perf implementation plan

The CPU-pool request crosses configuration, setup, scheduling, workload and
collector boundaries. Planning alone is not enforcement. Do not enable the
configuration or report the feature complete until all of the following have
been implemented and qualified.

1. Parse ordered logical CPU lists and inclusive ranges with bounded expansion,
   reject duplicates and invalid capacity, and test target present/online/allowed
   set decisions independently from host inspection. Keep portable no-affinity
   execution as the default.
2. Add a versioned runtime configuration and setup wizard step with None
   (portable) and CPU pool, editable pool/group size, allocation and capacity
   display, and preservation when reloading benchmark and sweep configurations.
   Reject over-capacity selections before installation or workload preparation.
   Capture optional setup CPU description for advisory comparison only.
3. At actual run target admission, obtain logical present/online CPUs and the
   process's effective cpuset; validate the entire selection before side effects.
   Warn and ask before continuing on advisory topology mismatch, without
   requiring a machine identifier. Preserve the pre-run plan's requested and
   observed data, and reject failures to apply or verify affinity.
4. Assign each active replica a deterministic disjoint allocation; reserve CPU
   groups across independently launched ASB campaigns with bounded lifetime
   ownership and explicit contention behavior. Integrate affinity before work
   starts in the native agent launch, llmtape/proxy lifecycle and managed
   workload container creation. Verify effective process and container settings,
   preserve inherited child behavior, and explicitly disclose independent
   Docker-daemon containers which ASB cannot control. Never claim process-tree
   inheritance constrains them. Preserve cancellation and cleanup ownership.
5. Add opt-in per-instance `perf record -a -C <group>` spanning the intended
   execution interval. Keep process-tree perf and its stat counters as the
   defaults; label perf record and perf stat scopes separately in all retained
   evidence and reports. Warn that CPU-wide recording can include unrelated
   system, kernel and collector work. Non-overlapping ASB allocations are not
   exclusive host CPU reservations. Preserve existing symbol quality reporting.
6. Test parsing and both requested allocation examples, duplicate/overflow and
   capacity failures, wizard default/navigation/reload, target topology warning
   and failure cases, launch-before-work affinity and verification, persistent
   evidence, cancellation and cross-campaign contention. Qualify on Linux with
   concurrent CPU-burning agents, a proxy, descendants and managed and
   independently created Docker containers. Compare effective CPU sets and
   CPU-wide capture scope, including explicit unverified-container disclosures.
   Confirm existing native unconfigured and process-tree behavior remains intact.

The draft provides target admission, topology confirmation, cooperative leases,
replica dispatch, verified launch boundaries, managed SWE-bench and Compose
placement, CPU-wide perf recording, separate process-tree stat evidence and a
report disclosure. An isolated guard-disabled checkout passed two-replica
offline replay with observed agent, descendant and llmtape affinity, real
CPU-wide perf samples, and real flamegraph generation. The explicit Docker
fixture also observed disjoint managed workloads and independent-container and
host noise in CPU-wide captures. An approximate offline two-replica SWE-bench
run verified managed and proxy placement and task-container cleanup; its
unresolved task is not a benchmark-correctness result. A real x86-to-aarch64
target change, disallowed target CPU, sweep replay and failure-time lease
release were exercised. These tests do not exhaust all replay and cleanup
paths. Run and sweep now admit valid runtime settings; final review and exact
head CI are still required. A lease only coordinates campaigns using the same
lock directory and is not crash recovery or an operating-system CPU reservation.
