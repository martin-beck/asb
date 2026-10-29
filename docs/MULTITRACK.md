# Multi-track recording and replay delivery

This document defines the implementation contract and delivery sequence. It is
not a claim that the CLI features below are available before their integration
and qualification PRs merge.

## Feature boundaries

1. Reusable cassette backend: versioned manifest, bounded selective ZIP reader,
   integrity and compatibility checks, deterministic selectors and selection plan.
2. Repeated execution: sequential batches, concurrent replicas, fresh workload
   material and isolated agent state, stable slot identities and complete results.
3. Recording publication: one writer per bundle, independent Deflate entries,
   durable per-track publication/recovery journal, retained incomplete attempts,
   create-new final publication and cleanup after confirmed success.
4. Run/replay integration: legacy JSONL/layout readers, private selected JSONL
   scratch before existing model checks, strict proxy validation, CLI and logs.
5. Sweep/report integration: persisted execution and replay selection plans,
   checkpoint/resume/reconciliation, scheduling budgets, every execution in
   comparison, honest population/attempt and live/replay labels.
6. Offline Ratatui track viewer: sort/filter/inspect, replay controls, terminal
   restoration, help/completions/quickstart and end-to-end qualification.

Each feature receives independent review, the repository-required gates, a
separate PR and a patch release after merging. Integration tests remain extensible
and provider-free, using fake clients and synthetic loopback providers. Tests of
real installed clients or original benchmark environments are explicit local
qualification, not implicit live-provider operations.

## Execution and selection

`--repeat N` means N sequential batches of the configured concurrent replicas.
Default repeat is one; explicit repeated execution uses newly allocated
workspaces even for one batch. The legacy non-repeated single-run workspace
behavior is preserved. Positive counts and their checked product are validated
before allocation. Track ID is `(batch - 1) * concurrency + replica`, one-based;
completion order never changes it. Batch/replica, workload attempt, dispatch
cohort and selected replay track are distinct identities.

An execution owns its workspace, agent runtime/session, log, measurements and
proxy progress. Workload adapters rebuild their own fixture/source material;
the runner never copies an already-mutated workspace or edits user files.
Canonical initial-content fingerprints exclude ephemeral attempt paths and are
checked across executions. Baseline provenance is retained separately: an
upstream definition is not a claim of measured initial failing tests.

Random replay selects whole complete nonempty traces uniformly with replacement
within a matching compatibility identity. Selection is precomputed in stable
configuration/batch/replica order, using a disclosed seed and versioned algorithm,
and saved before launch. Resume reuses that exact plan. Fixed or statistic
selection repeats the same selected trace for each execution; each execution
still receives a private proxy cursor. Numeric selection may explicitly select
an incomplete trace, but strict request matching/full consumption still apply.

Statistics selectors use complete nonempty tracks with known values. Tokens are
normalized total input plus output (cached input is a subset, not added again).
Duration is execution-only elapsed milliseconds, not preparation or evaluation.
Requests count recorded inference calls, excluding readiness probes. Steps use
the owning agent adapter's counter. Missing observations remain null. Sort by
metric then ID; min/max ties choose lowest ID, median chooses the lower middle.

## Bundle and failure contract

Version one is a ZIP with bounded `manifest.json` and independently Deflate-
compressed `tracks/ID.jsonl` entries. The manifest binds agent/provider/model,
workload/task/dataset identity, prompt and initial material fingerprints. These
are local consistency checks, not authentication or production trust chains.
Repeat/concurrency/measurement choices do not define replay compatibility.

Completeness, process outcome and workload outcome are independent. Failed
attempts may have complete selectable traces; process exit zero or nonempty
JSONL alone never proves completeness. Recording finalization must account for
in-flight requests and interrupted streams. Every attempted slot remains
inspectable, including empty or incomplete traces and unknown metrics.

ZIP finalization is not crash recovery. An owned partial directory contains
immutable completed track payloads and a durable journal; only one writer updates
it. The final archive is created privately and published without replacing an
existing destination. Interrupted publication retains the partial directory.
Ordinary execution failure still finalizes results. Uncompressed payloads are
removed only after successful publication; no unrelated paths are removed.

Readers bound archive size, entry count, manifest size, individual and aggregate
uncompressed size, reject unsupported/encrypted entries, duplicate/unexpected
names and unsafe file kinds, and verify selected payload length/checksum.
Manifest inspection does not decompress all tracks. Request/response inspection
loads only the selected entry and strips terminal control effects when displayed.
Corruption is an actionable data error, not a reason to introduce a signing or
key-management prerequisite.

## Private native-tool payloads (planned schema v2)

Some agents execute native tools without an interceptable MCP transport. Exact
offline replay for those agents requires the agent-visible native-tool request
and response bytes; a digest, a summary, or redacted output cannot reproduce a
later tool turn. Schema v2 may therefore add a manifest-bound, canonical
per-track native sidecar. This is a private replay payload, not public evidence
or a report artifact.

The sidecar contract is deliberately narrower than the provider track:

- The manifest declares its canonical entry name, exact byte count, SHA-256,
  event count, and positive completion state. Readers reject orphaned,
  duplicate, noncanonical, oversized, incomplete, substituted, or malformed
  sidecars before admitting a native replay.
- Native events are bounded and ordered: launch, stable session alias,
  poll/write, output, exit, and a declared workspace-effect digest. Replay
  rejects missing, extra, reordered, or changed events and never re-executes a
  recorded native operation.
- The recorder stores only credential-free tool protocol fields. It never
  stores credential values, inherited environment dumps, authorization headers,
  host-home paths, or unclassified opaque fields. A call that cannot be
  represented under this rule makes the native sidecar incomplete rather than
  silently retaining unsafe bytes.
- Filesystem locations are represented only by declared workspace-relative
  paths or stable private-root placeholders. Absolute host paths are neither
  published nor replay-significant.
- Native bytes are materialized in owner-only private custody, excluded from
  CLI logs, reports, viewer output, comparison data, receipts, and public
  evidence. Failed/incomplete partial custody is retained only for the owning
  user under the same access boundary; successful journal staging is removed
  only after verified archive publication. Deleting a published bundle deletes
  its sidecars together; ASB never scavenges user bundles automatically.
- Native replay is an explicit opt-in capability. Ordinary `--replay` does not
  authorize it; selection must require a complete sidecar and an adapter that
  proves its native executor is fully intercepted. Additive MCP support alone
  is insufficient.

This contract intentionally does not add encryption, signing, telemetry, or a
host key-management dependency. It keeps exact private replay bytes local and
bounded while preserving the existing public-evidence privacy boundary.

## Integration invariants and acceptance

- No archive or selection dispatch inside provider adapters or proxy protocol.
- Offline inspection starts no provider, agent, environment or credential setup.
- Replay re-executes real tools; matching material is not a machine snapshot or
  a guarantee of deterministic external state. Reports disclose replay mode.
- Shared batch preparation fails before launching that batch; individual failures
  are collected without suppressing later executions, with a final failure exit.
- Sweep receipts/reservations and comparison enumerate the saved execution slots,
  not just `1..=concurrency`. Duplicate attempts and overwritten slots are errors.
- Repeated attempts never become an official one-attempt-per-task score.
- Replay input bundles enter declared-input seals; recording outputs enter
  completion evidence. Publishing later tracks must not invalidate sealed
  earlier execution evidence: immutable track identities/payloads are the unit.
- Test batch counts/isolation/fresh material/user preservation, baseline mismatch,
  complete failed traces, seed scheduling independence and fixed selection.
- Test ZIP round trips/selective reads/checksums/limits, corrupt archives, disk
  failures, interrupted recovery and overwrite refusal; retain legacy replay tests.
- Test sweep aggregation/resume/reconciliation, null statistics/ties, viewer
  navigation and terminal restoration on normal/error/cancel paths.
- Run fmt, strict Clippy, tests, build, Rust docs, UML, packaging and hosted CI.

## Run integration seams

The run-owned cassette coordinator owns source discovery, frozen selection,
private JSONL materialization and the single recording journal. Neither provider
adapters nor the proxy protocol know about ZIP entries or track selectors.
Source discovery uses bounded file/content inspection, not a filename suffix;
legacy JSONL and sparse concurrent layouts remain readable. A bundle is opened
once, its complete selection plan is persisted before allocation, and only the
selected entries are decoded before existing model checks and proxy startup.
Each execution still owns a private replay cursor.

New bundled recording and replay use fresh campaign workspaces even when
`--repeat` is omitted (one batch). Plain runs and legacy JSONL replay without
explicit repetition retain their existing workspace behavior; explicit
`--repeat 1` is fresh. Thus `--record` deliberately changes its old workspace
behavior together with its output format. Explicit cassette filenames do not
select the storage format: a new recording is ZIP even with a `.jsonl` suffix,
and output must make that format clear. Tests must verify every default and
preservation of the original user workspace.

Proxy resource cleanup is not recording completeness. The recorder adapter must
provide optional capture-finalization evidence bound to its exact output bytes,
including accepted/completed/failed/in-flight counts and inference requests
excluding known probes. The embedded recorder stops admission, settles owned
requests and publishes a bounded atomic final receipt. Missing legacy-override
evidence remains unknown/incomplete, never guessed from nonempty JSONL or agent
exit. Chain evidence retains distinct recorder hops; it must not add duplicate
hop requests together. Complete failed attempts remain automatically selectable.

Compatibility binds replay-specific module/harness/scoring semantics alongside
the existing agent/provider/model/task/dataset/prompt/material identity. Do not
reuse execution-plan fingerprints: those deliberately bind repetition,
concurrency and measurement choices that may change during replay.

Recording destination checks precede tools/resources, but final no-clobber
publication remains authoritative. Journal intent is created only after actual
prepared compatibility is confirmed, never with a fabricated material digest.
Workers return bounded payload references and completion evidence to one writer,
which incrementally commits stable IDs without holding a lock through client
execution or grading. Per-attempt completion seals bind immutable committed
tracks, not a growing archive or staging paths removed by successful cleanup.
Ordinary execution failures still finalize settled results; interruption keeps
recoverable partial state, and explicit recovery never implicitly reruns slots.

## Sweep continuation integration

The original input owner classifies every original slot under all campaign
locks before publishing a continuation generation. Completed false grades are
reused; only positively untouched slots may be newly dispatched. Every
configuration manifest is published and rechecked before the final admission
marker and before returning any consuming worker capability. The parent retains
the input owner and every campaign lock through worker joins, receipt sealing,
input stability verification and comparison.

Fresh and continued execution share one actual batch dispatcher. A continued
batch retains original concurrency/replica/slot identities but gates only its
selected pending members. Retained peers produce no preparation/launch arrivals.
Replay aliases use saved private JSONL inputs and precomputed track choices,
never reopen the public source, and preserve nullable legacy selection facts.
Known-incomplete pending selections retain their warning and strict proxy checks.
New prepared/finished metadata is generation-scoped; original start/outcome
paths remain create-new and carry exact admission/manifest references.

Settlement validates inherited receipt bytes before proving the full population
and publishes only new receipts. Completed-only reuse does not mint a generation
or rewrite original input seals. Recording continuation additionally needs a
self-contained old-plus-new immutable archive generation and no-client recovery;
the replay/live dispatch does not authorize reopening an old recording journal.
See [the recording continuation contract](MULTITRACK-RECORDING-CONTINUATION.md)
for the remaining physical-generation, union and no-client recovery work.
Interrupted generation publication/activation remains recovery-only, not an
implicit retry or permission to pick a new generation.
