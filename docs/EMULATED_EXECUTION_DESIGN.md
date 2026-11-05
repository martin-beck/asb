# Optional emulated benchmark execution

Native execution remains the default. An explicitly selected VM backend should
run the entire ASB invocation in a guest of the requested architecture, so that
the agent, workload, provider proxy, record/replay and collectors see the same
guest environment. The existing workload-specific execution adapters must not
need emulator branches. This document is a design, not an implemented feature.

A backend boundary should own guest provisioning, input and output transport,
invocation, termination and cleanup; the native backend should invoke the same
ASB entry point locally. A fixed versioned recipe should identify the guest OS,
architecture, image checksum, QEMU machine and CPU configuration, disk, memory,
virtual CPUs and required tools. A recipe is not ready until a clean guest has
been booted, provisioned and exercised on the relevant host architecture.

The guest needs its own architecture-matched ASB executable, project-local
agent and workload dependencies, container runtime and collector tools.
Provider credentials must be passed only to the guest for the invocation and
must never be embedded in a VM image, command line, log or result artifact.
Replay must preserve the existing offline guarantees. A selected collector
that cannot operate in the guest must produce an explicit unavailable result,
not a fabricated host measurement or a silent system-wide substitution.

Input preparation must handle configurations, trial sets, workspaces and replay
cassettes, including explicitly configured external paths. Output transport
must return logs, recordings, measurements, comparison data and exit status,
and preserve paths used by generated reports. Interrupted and failed transfers
must retain recoverable evidence and must not report success. Interactivity,
cancellation, signals and timeouts must reach the actual guest execution.

The resulting metadata must distinguish guest architecture, emulated CPU and
host architecture. Measurements inside software emulation are useful for
functional qualification, but cannot be interpreted as native hardware
performance. The execution backend and guest recipe should be independently
extensible for additional architectures and recipes.

Qualification requires deterministic tests of backend selection, guest command
construction, credentials and paths, termination and failed transport, plus a
real cross-architecture VM test covering one live run, record and offline replay,
all selected collectors and result/report retrieval. A separate sweep test must
verify concurrency and per-benchmark output isolation. Default native execution
must remain unchanged and tested.
