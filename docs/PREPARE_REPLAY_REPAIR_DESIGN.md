# Repair design: prepare and replay without external network access

## Goal and current state

`asb sweep SWEEP.toml --prepare-replay` prepares replayable recordings. The
recording stage may use external services and acquire project-local inputs;
every verification replay and later `--replay` must work without contacting
an external host. Preserve ASB's existing agent, tool, and workload behavior:
record/replay is not a security boundary and must not arbitrarily restrict
agent capabilities. The result is reproducibility, not a claim of protection
against a malicious process.

At the time of this design, PR #1246 implements sampled preparation and PR
#1293 adds partial replay-offline safeguards. They are unmerged and must be
integrated against their actual current heads. In particular:

* A source-built SWE-bench image has an attempt-owned tag which is removed at
  cleanup. Replay cannot pull or rebuild that image offline.
* A task-keyed mutable cache is insufficient: the same task can produce two
  different source images, and an old cassette must still use its own image.
* Proxy environment variables are advisory; they do not show that native
  processes, package managers, DNS, a Docker daemon, or other workload helpers
  stayed local.
* A passing local-provider fixture shows only its exercised paths, not every
  workload, agent, transport, or network operation.

## User-visible contract

1. `asb sweep SWEEP.toml --prepare-replay` records each selected candidate,
   prepares its replay inputs and actually verifies it with replay. It retains
   a successful candidate only when the cassette, required inputs, replay
   consumption and cleanup are complete. It may replace unsuccessful sampled
   candidates using the existing bounded replacement pool. It must not report
   success after a partial or interrupted publication.
2. The feature must also handle ordinary fixed-trial sweep entries, including
   a sweep with no sampled entries. Preserve the current sampling semantics for
   `--trial-count`, mixed workloads, and `--continue` where applicable; reject
   unsupported combinations with an actionable message before doing work.
3. `asb sweep SWEEP.toml --replay` consumes the published selection and its
   cassette-bound inputs. It does not enumerate remote models, install or
   download dependencies, pull/build images, record replacements, or contact
   external services. Apply the same rule to the verification replay inside
   `--prepare-replay` and to direct `asb run --replay`. Explicit LLM-only
   replay retains its documented separate semantics.
4. A missing or incompatible local input fails before agent execution, names
   the input and affected cassette, and gives the user the actual command to
   prepare or restore it. Never tell the user to rerun an already completed
   benchmark as an automatic repair. JSON output remains machine-readable.

## Cassette-bound workload handoff

Add a versioned, workload-owned replay-input descriptor to each relevant
recorded track or its immutable manifest. It describes the selected workload,
trial and dataset revision, source definition and architecture, content-based
material identity, and immutable image ID when an image is required. Store no
credentials, prompts or evaluator secrets in the descriptor. The descriptor
must be covered by the cassette's existing integrity and compatibility checks.
Extend those checks explicitly; do not use an unchecked sidecar or a mutable
`task -> image` tag as authority.

The workload adapter owns preparing and resolving its replay inputs; the
runner transports an opaque descriptor and does not branch on workload names.
Add the descriptor to `PreparationContext` or a similarly typed pre-launch
handoff: the selected cassette and track must be resolved and validated before
`workloads::prepare_task`, not first discovered after the guest starts. A
recording must capture the descriptor while its original image is still owned
and available, before attempt cleanup and before the cassette is sealed. The
final cassette digest cannot be included in its own contents: publish a
separate receipt linking the finalized cassette digest to the descriptor and
local retained asset after sealing.

For SWE-bench, retain the exact source-built image by immutable ID before
removing the attempt-owned tag. Use an ASB-owned, collision-resistant local
reference and an atomic receipt in project-owned storage, bound to the
cassette/track and image ID. Validate image ID, architecture and the existing
source-image material identity before replay. Do not retag upstream images or
replace another image or receipt. A local tag is a retention reference, not the
identity. On record failure, remove only resources owned by that attempt;
retain evidence when cleanup is uncertain. On replay, use the recorded local
image and fail clearly if missing or different; never invoke pull or build.

Keep prebuilt image IDs bound in the same way. Support multiple cassettes of
one task with different image IDs, concurrent preparations, multiple replay
replicas and reuse of the same image by several cassettes. Serialize updates
with a project-scoped lock, write receipts atomically, and use explicit
references when deciding whether an ASB-owned image is no longer needed.
Deleting one cassette must not invalidate another; never prune unrelated
Docker objects. Image export/import is a separate explicit operation, not an
implicit replay download. If an old cassette lacks the descriptor, support it
only when an existing compatibility path can prove the exact local image;
otherwise give a migration/prepare diagnostic without changing its evidence.

## Network and dependency behavior

Make the execution phase explicit in the run and sweep pipeline: online
record/preparation, offline verification replay, and offline ordinary replay.
Pass this typed state through workload preflight, guest bootstrap, installer,
provider/proxy setup, collectors and subprocess creation. In replay, resolve
verified local caches only. Missing archives, portable runtimes, workload
assets and collector dependencies cause actionable errors before the run;
no implicit package-manager or installer fallback may reach a network.
Resolve Docker endpoints before probing; use only a validated local Unix socket
or numeric loopback endpoint, pin that endpoint for all probes and transport,
and ensure the daemon does not pull/build while handling replay. For isolated
workload containers, start replay guests without a routed network and forward
only required ASB-owned local endpoints over the existing owned transport.
For native paths retain the existing local-proxy behavior, audit direct socket,
DNS and helper calls, and repair any actual external egress discovered.

Do not describe proxy settings, a container setting, or a successful offline
fixture as a universal network security boundary. Any OS-level test isolation
must be scoped to the test process tree, allow required loopback and local
Docker transport, leave unrelated processes and host network configuration
alone, and report unsupported hosts rather than weakening a test silently.
No credentials or live providers are used in automated tests.

## Publication, recovery and concurrency

Use a sweep-owned preparation lock and per-candidate immutable directories.
Record an explicit stage and durable receipt for acquired inputs, complete
recording, sealed cassette, local image retention and successful verification.
Publish the complete winner mapping atomically only after all required
candidates pass. A retry validates existing completed evidence and continues
from the first incomplete stage; it does not silently repeat completed live
requests or overwrite a valid earlier cassette. Recovery must detect stale,
missing, mismatched, partial and concurrent publications. Keep failed evidence
and print a clear repair action. Input seals must include resolved asset
identities but exclude mutable output directories.

## Implementation sequence

1. Reconcile PRs #1246 and #1293 with current main. Keep the sampled-selection
   and offline-safeguard changes distinct until their integration tests pass.
2. Define and test the versioned opaque workload-input contract and cassette
   binding. Wire selected track inputs into workload preparation before any
   guest, installer or provider starts; keep generic code adapter-agnostic.
3. Implement SWE-bench exact-image retention, resolution and owned cleanup;
   cover both upstream images and x86-64/ARM64 source-build fallback. Add
   equivalent adapter-owned preparation for other workloads with replay-time
   external dependencies, or fail before execution with a precise diagnostic.
4. Finish offline checks for host tools, guest bootstrap, workloads, collectors,
   Docker and proxies; share this path between direct replay, sweep replay and
   preparation verification. Keep online recording functional.
5. Integrate fixed and sampled sweep preparation, recovery and publication;
   verify that concurrent or repeated preparations cannot replace a previously
   bound image or publish an incomplete selection.
6. Run the tests below, independently review the final current PR heads, repair
   failures, merge through PR, and verify post-merge CI on the actual main SHA.

## Required verification

* Unit tests for descriptor schema/versioning, digest binding, legacy behavior,
  exact-image mismatch, missing image, changed task/dataset/architecture,
  concurrent retention, interrupted writes, idempotent recovery and cleanup.
  Include two cassettes for the same task with different built images and
  deletion of one while the other remains replayable.
* Offline fixtures for cached and missing agent, workload, runtime and
  collector assets; assert missing inputs produce specific errors and no
  installer, package manager, registry, Docker pull or build is invoked.
* Full CLI tests for fixed, sampled and mixed sweeps: a local synthetic provider
  serves recording; replay consumes the sealed recording after that provider is
  stopped; test replacement, failure, retries, parallel replicas and JSON output.
  Verify ordinary direct and sweep replay as well as verification replay inside
  preparation. Check that completed recordings are not repeated on recovery.
* Observe IPv4, IPv6, DNS, UDP and TCP attempts from the ASB test process tree,
  including agent and helper descendants. Check Docker-daemon image operations
  separately because the daemon is outside that tree. Use a scoped test
  namespace or a kernel trace filtered to an owned test cgroup; account for
  tracing loss and do not rely on `strace` where it conflicts with ASB's native
  process gate. Permit only loopback/local transport during replay. Exercise a
  controlled external-address attempt without contacting a real external host.
* On an opt-in disposable Docker host, record an official fixture using a
  source-built image, remove the attempt tag, stop the fixture provider, and
  verify replay uses the recorded retained image without network or rebuild.
  Repeat with two different image IDs for the same task and with parallel
  replicas. Keep external downloads out of ordinary CI.

Acceptance requires the explicit workflow to pass on current PR heads, all
required CI checks to finish successfully, both merge commits to be ancestors
of main, and post-merge CI for the resulting main commit to pass. Clearly
report any platform or workload not covered; do not infer universal coverage
from one synthetic fixture.
