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

## Pinned-source architecture audit

Architecture can be a property of an individual trial or of an optional nested
virtual machine; it cannot always be represented by one workload-wide value.
The following observations are from the source revisions pinned by ASB. They do
not prove every task in a family has the same requirement:

- Terminal-Bench 1.0 explicitly selects `linux/amd64` for
  `broken-networking`, `conda-env-conflict-resolution`, `get-bitcoin-nodes`,
  and `run-pdp11-code`. Other tasks in the same source are not thereby proven
  x86-only.
- SWE-bench Pro has 42 task Dockerfiles configuring an amd64 Chrome package
  repository. This identifies a dependency assumption in those source files,
  not a verified architecture requirement for its entire catalog.
- The upstream SWE-Lancer image-build script explicitly selects an x86_64
  base image. ASB currently stages the source catalog; it does not automatically
  invoke that upstream image-build script.
- AgentBench's optional Freebase Dockerfile downloads an x86_64 Virtuoso
  distribution. That is not evidence that every AgentBench task is x86-only.
- Terminal-Bench 2.0 and 2.1 include tasks that download an x86_64 Alpine ISO
  for an inner QEMU VM. An x86 guest image does not by itself prove that the
  outer benchmark host must be x86; emulation can be part of the task.

The ASB source scan did not reveal explicit architecture markers in the pinned
AgentDojo, Aider Polyglot or tau-bench sources inspected. Absence of a marker is
not proof of cross-architecture execution. Core-Bench and SWE-efficiency source
archives exceeded the audit download bound and were not inspected; data-only
catalogs and dynamically supplied workloads also remain unqualified. A future
trial-level architecture declaration needs source-backed metadata and must avoid
turning an inner QEMU guest's architecture into a false host requirement.

## Candidate x86 guest image

The node26 qualification uses the Ubuntu Noble daily image dated `20261017`:
`https://cloud-images.ubuntu.com/noble/20261017/noble-server-cloudimg-amd64.img`.
Its SHA-256 is `04e8f2461a16a44e78a370284471f29616cdd9baf1893df25807b71f985b6c94`.
This dated URL and digest identify the tested base image; a floating `current`
alias must not silently replace it. The qualification uses QEMU system TCG,
virtio storage and network, and 8 GiB guest memory. Docker Buildx must support
`build --resource`; the tested guest uses Buildx 0.36.1 and BuildKit 0.26.2.
The current manual qualification also installs packages inside the guest; their
versions and the full provisioning/transfer lifecycle are not yet pinned or
implemented as an ASB recipe. The candidate is not qualified by boot alone.
