# Benchmark planning and measurement policy

ASB resolves backend, host platform, task input modalities, requested policies,
collector targets, phase caps, repetition identity and declared admission
reservations before starting tools, providers or workload environments. Setup's
CLI/TUI publishers use the same resolver. Unsupported requirements fail before
execution; no remote backend is advertised. Current Docker execution requires a
same-architecture Linux x86_64 or aarch64 ASB runtime. Image/runtime validation
and local target ownership checks remain mandatory at execution.

Existing TOML files retain their defaults. Optional generic configuration:

```toml
[planning]
backend = "native"
required_policies = ["owned-process"]

[planning.budgets]
execution_seconds = 900

[planning.reservation]
cpu_millis = 2000
memory_mib = 8192
disk_mib = 16384

[planning.repetition]
index = 2
total = 5
```

Set `execution_seconds = 1800` for a 30-minute agent execution budget. Guest
preparation and collector attachment have separate deadlines; grading and
teardown happen after the agent budget. The guest enforces the agent deadline,
while the outer transport allows a bounded settlement period afterward.

Repetition identifies a distinct selected configuration. It does not create
extra executions: select each repeated configuration explicitly. Its replicas
are still controlled by `concurrency`; sweep `jobs` bounds concurrent distinct
configurations and `max_active_attempts` bounds their summed replica counts.

For automatic fresh execution use `asb run CONFIG --repeat N`. This creates N
sequential batches of the configured concurrent replicas and derives each batch's
planning repetition index/total. A nondefault manual `planning.repetition`
cannot be combined with `--repeat`; it fails before campaign publication.
Every execution retains the resolved policy, effective controller caps and
persisted task policy. Batch indices share a protocol fingerprint, while a
different repeat total, replica concurrency or shared admission policy separates
the cohort. The manually labelled and automatically orchestrated protocols are
also distinct even when their numeric repetition identities happen to match.

The fixed batch-admission policy is recorded before allocation in the campaign
plan and in every execution identity. Its material rendezvous has a 1800-second
deadline from gate construction; the separate 1800-second launch rendezvous arms
once at the first post-material slot registration, covering preparation and
readiness of all clients. Both caps come from the same typed policy used to
construct the gates. These shared admission deadlines are not configurable
per-agent bootstrap caps, do not restart for late siblings and do not claim
hard-real-time interruption of every SDK call. The separate
`agent_preparation_seconds` option remains unsupported and its effective value
remains unknown; execution caps remain independent and begin at actual launch.

For resource admission, a sweep manifest can additionally contain:

```toml
configs = ["one.toml", "two.toml"]

[resources]
cpu_millis = 8000
memory_mib = 32768
disk_mib = 65536
```

Every configuration then needs an explicit per-replica reservation. ASB admits
its whole replica group only when its summed CPU/memory/disk requests and count
slots fit; FIFO permits stay held through preparation, grading and cleanup.
Overflow, unknown costs, empty budgeted dimensions and individually impossible
groups fail before preserving old sweep results. These are operator-declared
host scheduling reservations, not measurements or newly enforced task limits.
The adapters retain ownership of Docker/Compose limits. SWE's declared admission
envelope includes its 8 GiB source build (execution containers use 4 GiB).
Harbor requirements are read from explicit numeric fields in pinned task TOML;
missing SDK defaults remain unknown. Original Terminal-Bench YAML/Compose
requirements are not guessed. Known module requirements reject under-reservation.

New sweep entries contain a resolved `policy`, and the sweep records its resource
capacity. Continuation compares those operational choices, task requirements,
collector selection, host platform, repetition and existing module identity.
Historical plans lacking a policy remain readable for comparison but cannot
silently resume as a newly planned experiment. Runtime dispatch revalidates the
saved policy before per-attempt startup and instantiates only the frozen adapter
set. Collector dependency expansion is preserved without enabling optional
collectors merely because an automatic reporting helper exists.

Standalone and sweep attempts publish private create-new policy records under
`.asb/execution-plans/` before provider/collector startup, then publish an
additional record with actual task identity and effective controller caps before
the agent executes. Run logs, JSONL and completion summaries retain the policy,
effective caps and record reference even without measurement collection.
Instructions, credentials and attachment payloads never enter policy records.
Comparison JSON preserves recorded `planning` and `effective_budgets` objects
separately from observations. A matching original outcome/log can supply them
when measurement records omit them; absent metadata stays unknown. Module
revisions, reservations, repetition indices and timeout caps are not measurement
metrics or charts. Conflicting recorded policy objects fail comparison.

The runner computes `execution_plan_fingerprint` itself from validated resolved
operator choices, pinned module/default-rule identity and replica concurrency;
run callers cannot supply an opaque identity. The versioned SHA-256 cohort
fingerprint includes repetition total/policy but excludes its occurrence index,
task/attempt IDs, output paths and execution-cohort IDs. Repeated occurrences of
the same protocol can therefore aggregate, while explicit cap overrides, collector choices,
host/backend policy or replica concurrency form separate comparison groups.
The same fingerprint enters logs, completion outcomes, optional measurements and
the existing metric/population/resolution grouping paths. Historical records
without a fingerprint remain a separate unknown cohort.

Task-specific upstream requirements and effective caps remain per-attempt
provenance, not cohort dimensions: two tasks honoring different pinned timeouts
and resource defaults still belong to the same benchmark protocol. Effective
execution caps must equal an explicit override or the controller's separately
recorded ready-invocation default. Where the pinned catalog exposes that task
default, it must agree with the ready value too. Native and official-controller
defaults for other phases come from the same constants used by execution.
Inconsistent actual caps are rejected instead of being silently normalized.

`execution_seconds` caps the native process or guest driver. Official task
execution defaults are taken from the controller's ready invocation, including
fractional seconds, and recorded separately from requested overrides.
`environment_preparation_seconds` caps the controller-ready phase only; it is
supported for official Docker sessions. `evaluation_seconds` caps the official
controller's evaluation request/wait, with its existing transport allowance.
An upstream verifier can retain a tighter deadline. These are controller caps,
not measured durations. Cleanup and transport have separate existing allowances.
Native preparation/evaluation overrides are rejected. A blanket
`preparation_seconds` is not accepted; `agent_preparation_seconds` is explicitly
unsupported because local bootstrap and guest installation do not yet share a
configurable end-to-end deadline.

SWE-bench source-image builds default to 7200 seconds. For unusually slow
native builds, `ASB_SWEBENCH_BUILD_TIMEOUT_SECONDS` sets a limit from 1 to
86400 seconds; the effective limit is recorded in the source-image receipt.
Set `planning.budgets.environment_preparation_seconds` high enough to cover
that build and its setup and cleanup. Cohort material admission has its own
deadline, so this setting does not extend concurrent admission waits.

Collector descriptors distinguish agent-process, whole-container, host-context
and run-observation targets and execution versus run scope. Declarations do not
prove host permission or successful collection: existing capability probes and
verified PID/generation/cgroup attachment still decide that. Unmeasured Docker
runs cannot require the measurement-only `verified-local-target` policy.
The CLI measurement-directory override participates in negotiation even when
configuration disables measurements. Unavailable results remain unavailable.

Task invocation now carries structured instructions and bounded attachment
references (relative artifact, MIME type, byte length and SHA-256). Registered
agents declare input modalities. Current transports deliver instructions only
and reject all attachment references, including `text/plain`. A declared MIME
type is not validated payload delivery. Adding an attachment transport requires
file/size/hash validation and actual delivery before accepting references;
introducing an attachment type does not qualify multimodal execution or
authorize dropping attachments.

## Remaining qualification and extensions

This change supplies explicit repeat identity, not automatic repeat generation
or statistical aggregation. End-to-end agent-preparation deadlines, original
Terminal-Bench YAML/Compose requirement extraction, SDK-default requirement
resolution and payload transport for additional input modalities remain separate
implementation work. None is advertised as currently supported. Real installed
SDK/Docker runtime qualification remains opt-in; provider-free tests cover
admission, policy rejection, protocol budgets, input validation, frozen collector
selection and persistence without changing host privileges.
