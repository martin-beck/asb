# Transparent agent record and replay plan

ASB needs to record a normal agent execution and replay its interactions with
the same observable results. Recording is not an authorization mechanism: it
must not disable agent tools, alter prompts to restrict behavior, deny paths or
network access, or substitute a reduced set of tools. Existing workload and
execution isolation remains owned by the workload, independently of replay.

The existing LLM proxy handles provider traffic. The MCP relay handles declared
MCP traffic. Neither alone captures subprocesses, direct filesystem operations,
or other operating-system interactions. The current adapter-local tool tapes
and optional final workspace capsules cover only some of those interactions;
they must not be described as complete native replay.

## Target behavior

- Normal runs and records expose the same agent capabilities and configuration.
  A replay substitutes recorded results without launching the recorded tools or
  contacting the recorded LLM and MCP backends.
- A recording binds each attempt to its initial environment, interaction order,
  private payloads, and resulting filesystem changes. A replay gets an isolated
  writable view for each replica; four replicas never share mutable replay state.
- Preserve intermediate effects at the point they become observable. A final
  patch alone cannot reproduce a command that writes a file subsequently read by
  another command, and a recorded tool response alone cannot restore that file.
- Model file creation, writes, reads, renames, deletion, directory listings,
  metadata, links, permissions, open descriptors, process trees and executable
  outcomes, including concurrent operations and memory-mapped file access.
  Record other system or network interactions the agent uses, or report their
  absence as incomplete evidence; never silently claim a full replay.
- Keep payloads in private project-owned recordings. Stream large payloads and
  effects to disk, deduplicate by content where useful, and avoid arbitrary
  file-size, entry-count, output-size and per-tool-time ceilings. The benchmark's
  configured execution timeout governs the agent process tree. Capacity and
  real I/O failures must be reported accurately, without changing the result an
  agent receives merely to make a recording fit.
- Preserve ordinary operation when recording is not requested. Missing replay
  support may be reported as incomplete, but must not reduce the capabilities of
  the live agent or be presented as a successful full replay.

## Architecture to implement

1. **Storage and streaming.** Introduce a versioned, private per-track object
   store and an ordered event index. Stream object writes, reads, hashing and
   archive transfer rather than collecting entire recordings or tool results in
   memory. Publish tracks atomically only after their objects and event index
   are durable. Retain old cassette readers; never promote a partial recording
   into a complete track. Preserve existing explicit MCP and LLM replay modes.
2. **Filesystem view.** Give the agent a transparent filesystem view whose
   backend records access and mutations and can reconstruct them for replay.
   A namespace and filesystem proxy, such as a FUSE-backed implementation, is
   a candidate, not a security boundary or an assumption that it works in every
   container. The backend must account for links, concurrent access, descriptor
   inheritance and mmap, and must be tested in the actual workload runner.
   Provide an appropriate container or VM when the runner needs different kernel
   facilities. Do not require a particular native host.
3. **Process and non-filesystem observations.** Filesystem interception alone
   does not observe an executable's arguments, exit status, pipe output, or all
   direct system calls. Add a transparent observation/replay mechanism for
   those interactions and for concurrent process lifetimes; select its concrete
   mechanism only after proving it preserves normal execution and cleanup. Do
   not rely on LD_PRELOAD, change the agent's tools, or present a final snapshot
   as equivalent to recorded intermediate operations. LLM and MCP traffic stay
   with their existing proxies.
4. **Adapter integration.** Agent-specific launch, tool or protocol handling
   belongs in that agent's adapter or an adapter extension. Shared orchestration
   carries opaque recorded data and provides reusable streaming and lifecycle
   primitives; it does not contain a growing set of agent-name branches. Avoid
   injecting serial-tool instructions, disabling ordinary tools or substituting
   artificial command limits to make replay deterministic.
5. **Causal scheduler.** Preserve dependencies and exact recorded results when
   concurrent requests arrive in a different order. Independently provable
   operations may commute; dependent operations wait for their recorded
   predecessors without inventing responses or imposing a separate short tool
   deadline. Missing, extra or incomplete interactions remain visible.
6. **User interface and evidence.** `--record` and `--replay` must describe
   which layers were actually captured and consumed. Keep existing explicit
   layer flags useful, and make full replay transparent to the agent. Report
   missing data, unsupported operations, cleanup failures and mismatches with an
   actionable explanation. Do not conflate matching a failed benchmark outcome
   with successfully resolving the workload.

## Verification

Use offline CI fixtures for concurrent tool calls, subprocess output and exit
status, all file operations above, large binary and text payloads, links,
failures, cancellation, interrupted publication and independent replicas.
Exercise the real container or VM path as well as a native fixture. No CI test
requires a live provider or a paid API key.

Qualify the actual adapters locally: record one selected SWE-bench task with
one replica per agent, then replay each recording offline with four independent
replicas. Check complete tape consumption, filesystem effects, official
workload outcomes, process cleanup, and measurements. Include all supported
agents, preserve evidence and report any capability that remains untested or
incomplete. A source workload that was unresolved may replay faithfully, but
it is not a successful task resolution.

## Current gaps

The local tool server still restricts some agent behavior, bounds certain reads,
outputs and effects, and cannot reproduce arbitrary filesystem and process
interactions. Hermes and OpenJiuwen native sidecars can replay responses without
restoring all intermediate filesystem effects. Existing final workspace
capsules do not solve that ordering problem. Host overlay/user-namespace
availability does not imply the same facilities are usable inside the normal
Docker guest. These gaps must be repaired and qualified before claiming full
native replay.

## Virtual-machine feasibility findings

A software-emulated VM experiment recorded a normal Pi invocation against a
local mock provider inside a filesystem extracted from a SWE-bench task image.
The agent used its native Bash tool to write a file; four offline replays had
matching consoles and independent disks containing that effect. A separate
workload-filesystem probe exercised Git, Python and HTTP with the same result.
These experiments did not run the official evaluator or a real provider and do
not establish agent or workload qualification.

The prototype required QEMU's documented block replay layer and network replay
filter. An earlier direct disk setup and a virtio-network configuration stalled
in some replay attempts; a different emulated network device succeeded in the
small test. Treat device selection, transport behavior, extraction fidelity,
performance, architecture coverage and failure cleanup as open engineering
questions, not portable guarantees. QEMU's record/replay configuration is
documented at <https://www.qemu.org/docs/master/system/replay.html>.

Whole-machine replay re-executes guest instructions, including subprocess
instructions. Consequently this experiment does not provide the required
interception-based playback that returns recorded tool results without launching
recorded tools. It also does not replace the existing LLM and MCP proxies.
Do not promote a successful VM trace into a claim of complete native-tool replay.

## Experimental snapshot mount visibility

The snapshot supervisor keeps its storage in its own container mount namespace.
The guest driver enters a private mount namespace and detaches the internal
storage mount before starting the agent. The task workspace remains available
through its separate bind mount, and the supervisor retains its original mount.
This avoids exposing snapshot copies to agent commands such as `find /`.
This is bookkeeping isolation, not a sandbox or a guarantee of native tool
replay; the tool interception and snapshot-ordering gaps above still apply.

Snapshot marker capture and replay do not impose a separate per-tool deadline.
An out-of-order marker waits for its recorded predecessor until the execution
ends or the service is invalidated. Shutdown wakes pending markers and closes
idle client connections before joining server threads. This does not establish
that overlapping filesystem mutations can be replayed correctly.

Snapshot workspace images are sparse and default to the storage space available
on the project filesystem at preparation time. This removes the previous fixed
4 GiB logical image size; actual writes can still fail if the filesystem fills
or cannot represent the image. An explicit size remains available for offline
fixtures. Preparation refuses backing filesystems that allocate a small sparse
probe eagerly, rather than filling the disk; an automatic VM-backed fallback
for those filesystems is not yet implemented. This sizing change does not
establish complete native replay.
