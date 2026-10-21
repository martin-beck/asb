# Architecture

The maintained overview below explains ASB's runtime boundaries. It can be
supplemented—not replaced—by generated Rust type, dataflow, and control-flow
diagrams. Generate them locally with `make uml`, `make flows`, or
`make interactive`; the derived files are intentionally not tracked.
Private implementation items are documented by `make rust-doc` under `target/doc/`.

ASB is deliberately split into a small CLI and a file-backed workload directory:

- `benchmark`: the configuration record and workload prompt catalogue;
- `config`: the `setup` command and TOML writer;
- `tui`: the Ratatui setup wizard. It composes reusable selection panes for
  agents, providers, models, workloads, measurements, and concurrency, with filtering and
  keyboard navigation. Its model pane uses the existing provider discovery
  path and falls back only when a catalog is unavailable, then writes test
  configurations and a sweep manifest;
- `install`: local copying of agent executables and workload prompt files;
- `providers`: a registry of `ProviderAdapter` implementations, each owning
  its provider's model and endpoint metadata, catalog policy, process preparation,
  and optional bridge lifecycle;
- `endpoint`: agent/provider-neutral selection of a shared API interface;
- `models`: bounded live model-catalog discovery using provider-specific
  endpoints and response shapes;
- `agents/adapters/`, `providers/adapters/`, `tools/`, and `workloads/adapters/`: owning adapters for
  target-specific command construction, setup, installation identity, source
  materialization, and result artifacts; the generic agent runner dispatches
  only through these registries;
- `project`: `.asb/project.toml` and shared project paths. `init` creates the
  file, copies the bundled workload prompts and fixture trees, and creates its
  configured `tools_dir`, `workloads_dir`, and `workspace_dir`;
  every command resolves relative paths from this project root;
- `runner`: configuration loading, bounded agent-output logging, and live agent
  process execution;
- `compare`: offline sweep-result loading, numeric aggregation, and dependency-free
  SVG bar/line chart generation;
- `measurements`: opt-in process-scoped runtime collection. It writes bounded JSON
  records for host facts, elapsed time, success rate, and normalized input,
  output, cached, and total token usage, and
  dispatches through the `CollectorAdapter` registry in `collectors/adapters/`;
  each registered collector has a concrete stateless type implementing that
  trait. Optional operations use trait defaults, while adapter modules own
  their metadata, lifecycle hooks, processing, and presentation. Mutable state
  belongs to each measurement session, so concurrent runs share only the
  stateless adapters;
  `agent-memory` and `agent-workers` share one per-session Linux proc sampler.
  The runner supplies the prepared executable and arguments. Native executables
  match by canonical executable path; scripts additionally match their interpreter
  and entry point, rather than counting every process using Node or Python.
  Direct interpreter launches require the exact script, module, or inline-code
  entry point; unsupported launch forms remain unavailable instead of counting
  every interpreter task. Launch arguments are not written to artifacts.
  Exact entry-point matching admits each PID/start-time lifetime once. Later
  samples retain that admission across client process-title changes, but still
  check the executable/interpreter and root ancestry before and after reading
  memory. New PID lifetimes require fresh entry-point admission; an unrelated
  process using the same interpreter cannot inherit another process's admission.
  Active admission is separate from the historical worker census: observed
  executable mismatches or disappearance revoke it and require fresh entry-point
  validation on re-entry. Title changes before initial admission can still leave
  the sampler unavailable; no initial executable-only fallback is guessed.
  New admissions also require Linux `PF_FORKNOEXEC` to be clear in the
  generation-checked proc stat read. A pending tool launch can otherwise expose
  its parent's agent executable and arguments before exec. Fork-without-exec
  workers are unclassified: passive entry-point inspection cannot distinguish
  them from pending tool children. This limitation applies to both RSS and
  worker-count lower bounds; no observation delay or guessed worker is added.
  This is sampled identity, not detection of an exec that preserves both the
  PID lifetime and identical interpreter executable between observations.
  Only matching descendants contribute to aggregate RSS or distinct PID/start-time
  worker totals; arbitrary tools and the ASB gate do not. Samples retain bounded
  aggregate state, never argv, prompts, or credentials. Lifecycle completion and
  session destruction stop and join the sampler. Compare renders all-agent SVG
  bars and dedicated HTML sections with units and sampling limitations;
  isolated workloads run these two samplers in their own execution namespace
  through optional collector-trait hooks, using the prepared command's PATH,
  working directory and entry point. Only bounded aggregate observations cross
  back to the host; argv, prompts and credentials do not. Proc samplers verify
  PID/start-time identity and ancestry throughout their lifetime, stop at the
  execution boundary and reject reused or reparented tasks;
  the mpstat/iostat result names use process-scoped `pidstat` sampling for the
  agent process and its child tasks, retaining bounded raw output and exposing
  unit-named CPU and I/O metrics averaged over process-tree interval totals
  under a machine-readable `metrics` object plus bounded `series` arrays.
  Process rows in the same observation are summed, not plotted as extra time
  points. `series_timestamps` uses monotonic observation-completion offsets;
  historical files fall back to pidstat clock offsets with midnight handling.
  Average and child-summary rows are excluded. CPU/I/O observations use the
  millisecond series timestamp contract (both monotonic pidstat markers and
  legacy clock offsets are converted from seconds before reporting).
  `perf stat` uses the same
  interval contract. Perf scalar counters sum the emitted interval deltas,
  rather than reporting only the final interval. Unsupported counters remain
  absent, not zero. Parsing streams the complete stopped perf-stat file before
  deleting it; the separate 256 KiB diagnostic prefix never supplies totals.
  Original per-counter timestamps preserve missing intervals, and hybrid CPU
  core/atom hardware counters are summed at the same observed timestamp.
  Perf's seconds are converted to the shared millisecond timestamp contract.
  Perf-stat runs with `--no-scale`: hardware metrics are explicitly named
  `perf.observed_cycles_count` and `perf.observed_instructions_count`, not
  estimated whole-run totals. Unavailable intervals or multiplexing may omit
  observations. `perf-stat-quality.json` retains each original PMU/event's
  observed value, known runtime, running-percentage range and unavailable or
  unknown-quality row counts. Missing observations remain null, not zero.
  Running percentages are not CPU utilization, and rounded values (including
  0.00%) are never inverted to reconstruct unscaled counts. Linux documents
  [separate hybrid-PMU scaling](https://raw.githubusercontent.com/torvalds/linux/v6.12/tools/perf/Documentation/intel-hybrid.txt),
  which makes naive sums of scaled core/atom estimates misleading.
  Limits of 256 MiB input, 16 KiB per line and 100,000 points per metric fail
  the collector without publishing partial counters. Malformed/nonfinite
  counters and inconsistent timestamps likewise fail rather than invent data.
  `ASB_MEASURE_INTERVAL_MS` accepts 100--60000 ms and
  defaults to 1000 ms; pidstat rounds sub-second requests to whole seconds,
  while perf supports millisecond periods. Comparisons emit line SVGs for
  series with at least two points and expose the same hover, zoom, pan, reset,
  and enlarge controls as bar charts. Built-in BPFTrace profiles remain
  bounded cumulative one-second snapshots and final aggregates (custom
  interval programs may add series), while Perfetto retains its timestamped
  trace. Process-attached collector processes stop at the agent-execution
  boundary (also on run failure), before grading. Report processing/export
  stays after workload evaluation; an optional processor failure becomes a
  failed collector record, never cancellation before official grading.
  Shutdown/processing overhead does not extend the recorded agent execution
  interval. Whole-run tracing can explicitly opt out through
  the collector trait. Perfetto does so to capture the complete typed lifecycle,
  and extracts timestamped scheduler-event and scheduler-duration
  series; those trace timestamps are used as line-chart x coordinates;
  concurrent Perfetto sessions share an explicitly started manual service per
  ASB process. Each service exclusively leases one of two project-specific,
  predelegated named tracefs instances and uses its matching marker file.
  Bare Tracebox autostart and its global ftrace reset are never used, including
  preflight. A dedicated owner thread outlives replica threads, reference counts
  preserve sibling captures, and final release stops/reaps the manual daemons.
  Linux parent-death signals are bound to that owner thread, not a transient
  replica. Both daemons inherit the instance lease, preventing reuse during
  asynchronous crash cleanup after ASB is killed. Startup verifies inherited
  lease identity. Unique service logs retain at most 256 KiB while draining excess.
  A persistent project-root admission lock excludes reset while service leases
  are established; it survives deletion of `.asb`. Independent processes wait
  at most fifteen seconds for startup admission, not for another capture to finish.
  Reset rejects active instance leases, including inherited crash-cleanup leases.
  Normal service destruction joins the owner and verifies every owned daemon
  is reaped before explicitly unlocking the persistent instance lease. This
  prevents incidental forked descriptor copies from retaining a finished
  service's lock. Uncertain cleanup does not explicitly unlock. Owner SIGKILL
  still skips destruction, so inherited daemon descriptors retain crash-time
  ownership until shutdown; the lease inode is never unlinked.
  The probes daemon enters an unprivileged private user/mount namespace via
  util-linux `unshare`. An internal native helper rechecks owner/death-signal
  and inherited-lease state, verifies the namespace transition, and binds only
  the owned instance's `trace` over the bootstrap `trace` pathname read-only.
  It execs Tracebox in-place; the host-wide trace snapshot stays inaccessible.
  This accommodates the pinned binary's primary-root open/close bootstrap check
  without granting host-wide trace read access or host-root capabilities.
  Startup also waits for `linux.ftrace` and `linux.process_stats` registration
  through bounded queries to the private service before admitting captures.
  Captures stream compressed data to the artifact every second instead of
  retaining only the tail of a ring. Fast Zstd level 1 and a 128 MiB service
  buffer per capture provide throughput headroom. The delegated instance requests
  at least 4 MiB per CPU, 50 ms draining and a half-full wakeup threshold;
  primary tracefs controls remain untouched. Each file is capped at 1 GiB and
  one hour. Post-processing verifies the startup marker, ordered terminal
  boundary and relevant service/kernel loss counters. A limit, missing boundary
  or reported loss marks the collector failed and suppresses scheduler metrics,
  series and timelines; the task's independent official grade is preserved.
  Only scheduler switch/wakeup and process-lifetime events consumed by the
  analysis are enabled; redundant `sched_stat_*` events are not collected.
  Scheduler-state output does not establish exact I/O-wait classification
  without the optional kernel blocked-reason event. These buffers are per
  capture/CPU, so operators must bound concurrency for available host memory.
  Init's explicit host-consent flow can create/repair only the selected owned
  instances and grant execute-only parent traversal when necessary; it never
  delegates parent write/list access. Ordinary capture uses no sudo. Upstream's
  primary watermark probe cannot write the non-delegated primary control file
  under normal permissions and falls back to periodic draining. No promise of
  zero primary read/probe accesses is made;
  the `system` record also captures bounded host-comparison metadata: CPU model,
  vendor and Arm implementer/part, package/core/thread topology, frequency
  governors and limits, NUMA node count, VM/container classification, compiler,
  runtime and selected collector versions, and relevant kernel settings. It
  records only sanitized values (never raw proc/sysfs/cgroup text, command
  paths, credentials, or provider data) and omits unavailable fields.
  Every measured run also writes `components.json`: lifecycle component wall
  times plus process-accounted user/system CPU time, block-I/O delay, and
  scheduler run-queue wait for the agent execution component.
  The `run.total` component measures elapsed wall time from measurement-session
  start through `RunFinishing`, including measured preparation, execution,
  evaluation and cleanup; it excludes pre-session collector preflight and
  subsequent collector/report processing. The execution-only `elapsed_ms` in
  `run.json` and the run summary remains separate (guest-observed for isolated
  workloads). A session without `RunFinishing` uses its current elapsed clock.
  Network wait is represented as `null` unless a collector can observe it
  directly; ASB never assigns residual wall time to a guessed category.
  An automatic `cpu-accounting` adapter attaches the paused Linux process gate
  to an exclusive cgroup-v2 leaf when writable delegation is available. All
  children inherit the group, retaining their user/system CPU counters after
  exit. Final cgroup counters replace the sampled CPU fields and set
  `cpu_accounting_source` to `cgroup-v2`; without delegation the existing
  process sampler remains available. `cpu-accounting.json` records the source,
  failure reason, and cleanup status. ASB creates no controllers, changes no
  limits, and removes only its own empty leaf; populated groups are retained
  and reported. A supplied `ASB_CGROUP_PARENT` must resolve beneath
  `/sys/fs/cgroup` to an existing cgroup-v2 directory. To obtain delegation on
  systemd Linux, run `systemd-run --user --scope -p Delegate=yes -- asb run
  CONFIG.toml` (or use `asb sweep SWEEP.toml`); `asb init` prints this advice.
  During init, ASB checks `kernel.perf_event_paranoid=-1` and
  `kernel.sched_schedstats=1`. If either is needed, it prints the exact
  `sudo sysctl -w ...` commands and requires an explicit `y` or `n` answer
  (Enter is not accepted as a default) before changing system-wide settings.
  After explicit approval it uses passwordless sudo when available, or lets
  sudo prompt for the user's password when it is not. Init separately lists
  every delegated-cgroup command (`mkdir`, `chown`, `chmod`, and, when needed,
  controller enablement) and asks for explicit approval before running them.
  `asb init --yes` (or `-y`) approves all listed setup groups without prompts.
  Declining, an invalid answer, or a failed command leaves the setting or
  delegation unchanged; init remains usable. It never edits sudoers or changes
  these settings silently.
  On a systemd cgroup-v2 host, the same best-effort init path creates a
  user-owned `asb.slice` beneath the current user slice, enables the CPU
  controller, and records its parent in `.asb/project.toml`; measured runs use
  that parent automatically. A non-systemd layout or failed setup is reported
  and falls back to process sampling. Init never changes resource limits or
  edits sudoers.
  `asb compare` also writes an execution-composition SVG with user CPU, system
  CPU, block-I/O delay, runnable scheduler delay, and explicitly-labelled
  remaining wall time. CPU categories are process-tree totals and may overlap
  elapsed wall time when children execute concurrently; this is accounting
  composition, not a fabricated single-thread state timeline.
  the Perfetto adapter validates tracefs/ftrace access and starts project-local
  Tracebox before preparation begins. It waits for recording readiness before
  emitting fixed, non-secret lifecycle tracepoints; after the agent is spawned,
  it scopes scheduler analysis to that PID and its descendants. Each concurrent
  run has a distinct trace identity. The project-local trace processor
  extracts stable aggregates,
  and ASB preserves the native `.perfetto-trace`, a bounded JSON record, one
  portable SVG per extracted metric, and an exact per-thread scheduler-state
  timeline SVG. The timeline uses trace timestamps and retains simultaneous
  child-thread states as separate lanes; it is distinct from the ordered
  aggregate execution-composition graph in `asb compare`;
  the perf adapter records both call stacks and stable `perf stat` counters
  (task clock, context switches, migrations, faults, cycles, instructions) for
  the complete process tree with explicit child inheritance; pidstat uses
  `-T ALL` for the same tree. Built-in BPFTrace profiles seed and maintain a
  process-tree map across forks. Custom BPFTrace programs are rejected unless
  they use the injected `@asb_tree` predicate, receive every currently-live
  tree PID, and inherit the same fork tracking; this fail-closed contract keeps
  arbitrary tracepoint programs from measuring unrelated host processes while
  still covering helpers created during the run;
  each adapter is isolated from the runner and can be extended without adding
  provider-specific fields to the benchmark configuration;
- `workloads/`: one UTF-8 `.txt` prompt per workload name. Built-in supporting
  files are stored under `.asb/fixtures/<name>/` and materialized into the run
  workspace without overwriting existing edits.

`WorkloadAdapter` creates a distinct mutable `WorkloadSession` for each
attempt. The session owns its task invocation, prepared workspace, initial
baseline evidence and evaluator. The generic runner consumes those values
without choosing a benchmark's grader. Task and attempt identities are stored
in run/measurement records and preserved by comparison; optional dataset
identities include source, revision and split. Generic contracts live in
`workloads/session.rs`, separate from concrete adapters. The ongoing
[full benchmark integration](FULL_BENCHMARK_INTEGRATION.md) extends this
boundary with actual upstream catalogs, submissions, environments and
evaluators; a session by itself is not proof of full benchmark support.
An adapter can also expose a read-only `PlanningCatalog`: dataset identity,
available task IDs and evaluator scope, without preparing an environment.
Sweep planning caches this once per installed workload and saves a bounded
`plan.json` before running anything. A `TaskSelection` is requested intent,
not an actual `TaskIdentity`, measured baseline, or evaluator result. Compare
uses that snapshot when preparation failed, validates source configuration
semantics and actual task/attempt/grader identity, and never invents attempts
for missing rows. Selection snapshots do not yet provide the complete
runtime/project/cassette fingerprints required by a resume protocol. A private
`.checkpoint/inputs.json` records before/after content stability for the running
ASB executable, saved plan, project configuration, installed tools/workloads,
project Python and file-valued environment overrides. Environment values enter
only a keyed digest, never the public response. Changed, excessive or
unsupported inputs cannot claim stability. This optional seal is explicitly
not a resume receipt. Adapter-owned runtime declarations now include selected
programs with their prepared PATH, Python standard/selected virtual-environment
libraries, native extension dependencies, and the invoking ASB's mapped native
libraries. Bounded local discovery inspects native loader dependencies and
script interpreters without running agent/provider entry points. Logical
commands are re-resolved at the end, so a new earlier PATH candidate or changed
native dependency cannot claim stability. Unsupported/excessive discovery
leaves this optional seal unavailable, never blocks a fresh execution, and
does not claim whole-host immutability or arbitrary application dlopen closure.
Record/replay dispatch asks the selected proxy adapters for their
execution files without starting them or materializing scratch scripts. The
default adapter binds its selected host Node executable or explicit proxy
override, preserving symlinks and PATH precedence. Replay additionally binds
the actual source cassette directories, including rebased/preserved history
paths. Record cassettes are outputs, not replay inputs. Interpreter and native
loader dependencies are added by shared runtime discovery, not inferred merely
from an executable's pathname.
After workload cleanup and measurement completion, the runner writes private
`.asb-completions/` outcome records beside the run log, including unresolved
outcomes, actual task identity, cleanup status and workload-owned evaluator
evidence paths. These records omit agent stdout/stderr. When one replica
finishes, a generic completion observer can independently bind its
one-based slot, selected population, original input digest, private outcome
record, workspace, log, native runtime state (if present), measurements and
external evaluator/proxy evidence in
`.checkpoint/replicas/<configuration>/<replica>.json`. These receipts publish
after grading, workload/proxy cleanup and measurement completion, before the
runner waits for the whole group. Sibling output paths do not enter that
replica's seal. Slot-to-workspace/log/measurement naming comes from the same
runner helper used to create replicas; the observer does not guess an agent's
paths. Per-configuration evidence publication is serialized without serializing
agent execution, and metadata publication failure never changes a fresh grade.
Schema-2 private outcomes declare the slot and execution-output paths; old
schema-1 whole-group outcomes retain their previous read compatibility.
After a configuration's
replicas finish, the checkpoint binds its complete result tree and all declared
evaluator roots into `.checkpoint/completed/<index>.json`. Every replica must
have a unique, matching task identity, an actual boolean evaluator outcome,
supported baseline evidence and confirmed workload/proxy cleanup. Proxy-owned
output files (including external recording paths) join the evidence roots;
an absent output from a failed attempt is bound as absent, not treated as a
request to retry a graded failure until it produces a recording. Replay validation failure is separate from
whether its proxy process stopped. Enabled measurement records
must agree with the outcome summaries. Both resolved and unresolved grades are
sealed; incomplete, malformed or mismatched groups are not. Publication cannot
replace an existing receipt, and verification rejects changed results, evidence,
selection or input identity. Temporary output spools live outside the per-task
result tree, so hashing excludes no evidence filenames. Neither an input-only
nor an outcome-only seal is sufficient to suppress execution or change a grade.
`asb sweep --resume` combines current declared-input validation with the original
selection/mode/scheduling and every completed group's evidence seal before
starting or skipping anything. It reuses complete groups without launching
clients, preserves failed grades and failure exits, and admits configurations
whose result directory does not exist. A pending initial seal from a stopped
owner is usable only when the current declared inputs still match exactly.
If a stopped owner missed the final group receipt but every selected replica
has independently sealed evidence, continuation verifies those slots and the
complete outcome population without creating new attempts or republishing a
group receipt during validation. Partial groups retain each validated grade and
can dispatch only slots with a positive, matching never-started reservation
and no attempt outputs. The owner publishes all reservations before launching
that configuration. Starting a replica publishes intent, consumes its reservation
and synchronizes the reservation directory before measurement/provider/workload
startup. A missing start file alone never proves a slot was unused. Missing,
changed or already-consumed reservations and unsealed graded outcomes reject
continuation before scheduling. Already-started unsealed slots still require
owned-resource recovery, which remains unfinished. A completion-evidence failure
never changes an already computed grade; an intent-write failure stops the
attempt before execution. Unavailable input fingerprints still do not block
fresh non-resume execution. Missing keys are never regenerated for continuation, and
invalid continuation never replaces historical input seals/results. JSONL
summaries carry `reused: true`; human output explicitly identifies reuse.
The generic runner preserves original replica indices while skipping cached
slots, including false grades and failed clients, and does not start those
slots' providers/proxies/environments again. Identical isolated configuration
files are not rewritten during continuation. Existing default replay cassettes
must still identify their original preserved hard links. Interrupted parallel
terminal spools are privately archived after validation, never overwritten or
treated as grading evidence.
`asb sweep SWEEP.toml --resume --reconcile-completed` explicitly repairs missing
completion receipts for already-graded, cleanup-confirmed schema-2 outcomes.
Original inputs, selection, start intent and workload allocation must still
match. The owning workload adapter verifies retained task/submission/grader
artifacts read-only under the resource owner's lock. All candidate outcomes
are validated before publication; false grades remain false. Repaired receipts
bind current artifacts and a reconciliation provenance marker, which JSONL and
comparison reports disclose. This is current local consistency validation,
not historical attestation. It never launches an agent, provider, environment
or evaluator, and cannot turn an ungraded interrupted attempt into a grade.
Sessions may execute through a workload-owned controller rather than the
native process path. The generic execution context carries the selected
configuration and negotiated connection, and the owning session returns
separate process output, adapter-parsed results and observed execution time.
The runner does not choose container images, dataset formats or graders.
The Linux guest driver and native process gate are exclusive child
supervisors. They retain double-forked helpers as subreapers and verify kernel
child ownership through pidfds. By default they stop/reap remaining helpers
after the principal client exits, and only then publish a completed result.
An adapter-owned `ProcessLifetime::ThroughEvaluation` instead keeps guest
services alive in its private workload PID namespace through official grading.
The SDK controller and guest bind that delegation to a distinct namespace and
per-execution token, confirming principal exit separately from a bounded private
result handoff. Execution accounting stops at principal exit, not evaluator
completion. Bounded output readers freeze client capture while continuing to
drain inherited service pipes; the idle subreaper reaps exited services without
signalling live ones. SDK stop/removal owns final service teardown and joins the
held Docker output transport. Errors before confirmed handoff retain the
container-reset/preservation path. Terminal-Bench adapters select this policy;
native and SWE-bench sessions remain quiescent. This service policy does not
claim preservation of arbitrary interactive shell state across guest execution.
A 500 ms TERM grace is
followed by KILL with a five-second teardown bound; the required kernel APIs
are probed before client launch. The shared multi-run process never takes this
ownership role. Guest output-consumer errors also trigger owned-child cleanup.
Native Linux execution uses the gate even when measurements are disabled.
Its owner is bound to the spawning process (not the spawning thread) with a
pidfd before client execution. A small child watcher pins both owner and gate;
it wakes a stopped gate on owner loss so the gate can quiesce its descendants.
The native resume path confirms the stopped state before sending CONT, and
cancellation uses TERM plus CONT with a bounded cleanup wait instead of
immediately killing the supervisor. All watchers and signals are scoped to
the attempt; no shared runner signal handler or process-group kill is used.
The native supervisor and its idle watcher are part of the measured process
tree; process-tree totals include that instrumentation overhead. The watcher
is created before cgroup attachment, so it remains outside the new accounting
leaf; the supervisor and subsequently launched client are inside it.
Container task measurements continue to target their guest driver separately.
If guest metadata confirms neither quiescence nor the declared namespace-owned
service handoff, the workload controller waits for
its owned private-PID-namespace container to stop and restarts its original
entry point before grading partial work. The filesystem is retained, the
client failure remains separate from the task grade, and the execution journal
distinguishes supervisor confirmation from a container reset. Failed reset
confirmation aborts evaluation. Host/shared container PID namespaces are not
valid task targets. This cooperative teardown is not abrupt-host-crash recovery.

The CLI is the stable user boundary. A benchmark configuration names an agent,
workload, provider, model, concurrency, workspace, and log file. `concurrency`
defaults to `1` and is bounded to `1..=64` before allocating replicas; values greater than one run isolated replicas of the same
agent/model/workload in parallel, with separate workspaces, logs, measurements,
and record cassettes. A replay may fan one cassette out to every replica, each
with independent progress and agent configuration. Provider credentials are never stored
in that file. The runner passes provider/model selection through environment
variables and gives the selected agent one bounded workload prompt.
Each agent declares the API interfaces its adapter can configure in preference
order, and each provider declares the interfaces it offers for the selected
model, including the interface's authentication style and API base address.
The endpoint negotiator chooses the first agent-preferred interface with a
compatible authentication style and obtains its connection details from that
specific provider offer. It does not
select providers based on agent identity or agents based on provider identity.
No intersection produces an actionable error rather than a silent fallback.
Each project-local cli2key session starts an independent bridge on a selected
free loopback port by default, avoiding the upstream shared default port during
concurrent replicas. Startup is serialized only through readiness; execution
remains concurrent. Explicit `CODEX_BRIDGE_PORT` remains a fixed-port override;
to intentionally share an existing bridge use `CLI2KEY_BASE_URL` and its key.
The upstream pinned CLI cannot bind port zero, so free-port selection is not an
atomic listener reservation against unrelated processes; a collision fails
startup, never authorizes using somebody else's listener.
The project-local cli2key session owns a small compatibility hop in front of the
bridge. It removes only Codex host metadata (`additional_tools` and reasoning
context) that the bridge API cannot accept, while forwarding prompts, function
tools, credentials, and streaming responses unchanged. External
`CLI2KEY_BASE_URL` endpoints are used directly and are not modified.
Agents, providers, and workloads use the same source layout: each has a
`mod.rs` for category-wide behavior and an `adapters/` directory containing
the adapter registry (`adapters/mod.rs`) and one module per implementation.
When an adapter needs multiple files, its helpers live beneath the matching
adapter name in that same directory. Provider-specific behavior belongs in
its provider's adapter module or its provider-owned helper directory;
register new adapters in the category's adapter registry. Catalog parsing and endpoint negotiation
operate on response shapes and API interfaces, not provider identities. A
source-level test catches common direct provider-ID comparisons outside provider
modules, but it is a heuristic, not a substitute for reviewing indirect dispatch.
Interface compatibility is distinct from whether a particular model can
successfully perform an agent's task. Provider model/endpoint declarations must
be maintained as the upstream providers change. OpenCode's public model-list
endpoint does not report the interface for each model, so preparation also
fetches OpenCode's public models.dev metadata and uses its per-model API URL and
SDK family. Metadata is read without provider credentials and discovered URLs
are restricted to the OpenCode host; `OPENCODE_ZEN_BASE_URL` remains an explicit
override. If live metadata is unavailable, ASB warns in human output and uses a
built-in snapshot. Every live run queries the selected provider's model catalog;
Anthropic, Gemini, and Fireworks continuation pages are followed with bounded size and page
counts, and cli2key is queried through the same bridge instance used for the
run. OpenRouter's public catalog can also be queried without credentials.
Other providers use their documented API-interface and address declarations
because model catalogs do not consistently enumerate wire interfaces. Explicit
model capability flags are used when available; a missing tool-support flag is
unknown, not a positive claim, and produces a warning before execution. These
documented declarations and the offline replay snapshot can become stale;
automatic freshness cannot be guaranteed when a provider does not publish that
metadata.

The catalog implementation follows the official [Anthropic Models API](https://platform.claude.com/docs/en/api/models/list),
[Gemini Models API](https://ai.google.dev/api/models),
[OpenAI Models API](https://platform.openai.com/docs/api-reference/models/list),
[OpenRouter Models API](https://openrouter.ai/docs/api/api-reference/models/get-models), and
[Ollama API](https://docs.ollama.com/api), [Together Models API](https://docs.together.ai/reference/models),
[Fireworks Models API](https://docs.fireworks.ai/api-reference/list-models), and
[Mistral Models API](https://docs.mistral.ai/api/endpoint/models); the OpenCode model-specific metadata
comes from its [models.dev catalog](https://github.com/anomalyco/models.dev).
Other provider adapters retain their documented URL and wire-interface defaults;
model availability still comes from their individual live model-list APIs.
For example, [DeepSeek's Anthropic-compatible API](https://api-docs.deepseek.com/guides/anthropic_api/)
is offered separately from its OpenAI-compatible API, with an independent
`DEEPSEEK_ANTHROPIC_BASE_URL` override.
Fireworks' model catalog is account-scoped: it queries the public `fireworks`
account by default. Set `FIREWORKS_ACCOUNT_ID` to a different account to list
its models; for a run, a full `accounts/<account>/models/<model>` identifier
also selects that account unless `FIREWORKS_ACCOUNT_ID` is set. Set
`FIREWORKS_MODELS_URL` to use an explicitly configured catalog
endpoint; `FIREWORKS_BASE_URL` independently controls inference. These do not
automatically combine public and private account catalogs. Fireworks advertises
documented chat, responses, and Anthropic-compatible interfaces, but a listed
model is not thereby guaranteed to support every interface.
Provider credentials are not stored in benchmark configuration. Known providers
document conventional credential and endpoint variable names; model discovery
and child launch use the environment first and then a bounded
`$HOME/.api_key_<provider>` file. Custom provider names can be represented in
configuration, but registered agents require a provider with declared endpoint
capabilities before a run can start.
Model discovery reads the selected credential only long enough to send a catalog
request; it sends it through curl's stdin configuration, never includes it in
arguments or output, and never stores it. Authenticated requests do not follow
redirects, which prevents a catalog redirect from forwarding a provider key.
For replay, ASB removes known provider and registered agent credential
environment variables and supplies dummy values where needed before launching
the agent. Replay is not a security sandbox:
agents may still access other environment variables, local files, and network.
OpenCode Zen model discovery uses its public `/zen/v1/models` catalog and does
not require `OPENCODE_API_KEY`; the key is still required for benchmark
execution.
Workloads are loaded from `workloads/` beside the configuration file. Set
`ASB_WORKLOAD_DIR` to use another directory; add a new `.txt` file to extend
the catalogue.

`run` (optionally with `--record` or `--replay`) appends one bounded JSON record per execution to
the configured `log_file`, including the captured agent output and error text.
Token usage is collected from generic/provider/agent environment variables,
structured agent JSON events (including JSON embedded in human-readable agent
output), and recorded cassette responses, then normalized without inferring
counts from text size. The parser accepts usage shapes emitted by every
supported agent family, including Claude Code, Goose, Cline, and Continue,
and maps provider aliases such as `prompt_tokens`, `inputTokens`,
`promptTokenCount`, `cacheRead`, and `total` to the stable input, output, cached,
and total fields. When input and output are present, total is canonicalized to
their sum because cached input is a subset of input. Qwen Code's adapter reads
the final cumulative result; OpenJiuwen's adapter consumes per-model-call
streaming usage, with a process-local compatibility shim for its pinned
client's partial streaming metadata. Cline uses its final aggregate, Pi and
OpenCode sum their completed per-turn or per-step records, and mini-SWE sums
the provider responses in its trajectory. A complete recording takes
precedence over agent output, because it also includes auxiliary provider
calls that an agent may omit from its own summary. Replay uses whole-cassette
totals only after successful proxy completion validation. Partial or rejected
replay falls back to actual agent-reported usage, or unknown when unavailable;
unconsumed recording responses are never charged to the replay attempt.
Without recording, token
counts are limited to the usage reported by the agent; for example, Goose and
OpenCode can omit a separate title-generation call, and some agents do not
report usage at all. Missing values are not estimated. Cassette accounting combines usage
snapshots within each response and sums completed calls; incomplete accounting
is not promoted to a whole-run total. Continue's adapter reads provider-reported
usage on individual session messages rather than its potentially estimated
aggregate; OpenHands reads per-call conversation metrics, not both individual
and accumulated counters. Both use separate per-run state directories, reject
ambiguous or incomplete state, and normalize cached input for the selected
wire interface. OpenHands may itself report a cache count of zero when its SDK
does not distinguish a missing cache field from zero. Those run-specific state
directories can contain agent session transcripts and remain beside the run
workspace. OpenHands and OpenJiuwen use
machine-readable output; mini-SWE writes a project-local trajectory that
is parsed for nested provider usage before it is removed. Their terminal output is intentionally limited to success
rate, elapsed time, and token usage; `--json` changes only the representation
of those metrics.
Pass `--show-output` to stream the agent's stdout and stderr live for interactive
debugging when `concurrency = 1`; concurrent replicas buffer each bounded stream
and emit it in replica order after completion so output cannot interleave
unreadably. The same output remains in the configured log.
ASB emits each identical provider warning once per run or sweep and groups
identical replica failures with their one-based replica numbers; distinct
failures remain separate. A sweep also groups identical failures across its
benchmark configurations. Per-replica result records and explicitly requested
agent output are not deduplicated. JSON result records remain per replica.
The same flag is accepted by `asb sweep` and is forwarded to each underlying
`run`, so sweeps do not maintain a second output path.
An agent is not considered successful solely because its process exits with
status zero: known authentication, unsupported-endpoint, and provider-error
messages are treated as failures so sweeps cannot report a false success.
Task evaluation is independent of process success. External aliases currently
execute adapter smoke exercises and cannot contribute to task-resolution
rates. Schema-2 comparison suppresses their historical success percentages;
see [evaluation semantics](EVALUATION.md). Step accounting delegates to the
owning agent adapter's final counter and reports unknown instead of guessing
from text or streaming JSON keys. No runner or collector branches on agent
identity to interpret that counter.

Source-file installation stages bytes in a private sibling directory before
atomically replacing its destination. HTTP downloads ignore user curlrc files,
use a ten-second connection limit and a five-minute total deadline, and are
also watched by ASB for size/deadline violations. General sources are bounded
to 1 GiB; runtime archives retain their 256 MiB bound. Local copies use the
same publication path. File type, size, any recipe-owned checksum, executable
permissions and any registered version probe are checked before replacement.
Pinned Perfetto version probes have a separate ten-second deadline and their
owned subprocess is stopped/reaped on timeout; this is not one global deadline
over every step of a multi-artifact installation.
Failed preparation leaves the prior destination and its permissions unchanged;
destination symlinks are replaced rather than followed. This is per-file
atomic publication, not an all-or-nothing transaction over a tool's multiple
artifacts. Abrupt ASB termination can retain private staging files, but those
are not published or considered installed. An explicit unsigned `--source`
does not acquire a checksum that the user or recipe never supplied.

`asb tool install` installs a pinned package recipe into the configured tools
directory for supported agents and support tools, or downloads/copies an
executable supplied with `--source`. Record/replay instead uses ASB's embedded
tape proxy automatically and has no tool-catalog entry. npm recipes are
project-local; Python recipes use a
project-local virtual environment. `asb workload install` downloads a prompt
URL, resolves a pinned external workload alias, or copies a local prompt into
the configured workloads directory. The legacy `terminal-bench`
alias downloads a pinned upstream source archive into a local bundle directory;
the runner copies those sources into `external-workload/` in the workspace.
The three original SWE-bench family adapters now own a pinned official task
catalog installer, SDK dependency and installed-task discovery. They do not
fall back to synthetic trial IDs. The development run path connects those
sessions to actual instance execution and official grading, reusing the
selected Rust agent adapter inside the task environment. ASB automatically
stages its running executable: a static ELF directly, or a dynamic ELF with
its matching loader and libraries for the same-architecture Linux task image.
The scoped launcher does not replace guest system libraries or export a
library search path to benchmark clients. `ASB_GUEST_BINARY` is an optional
development override, not a required build/install step. Runtime staging and
recursive gates retain the invoking executable's kernel identity
on Linux, so replacement of its installed pathname cannot switch the running
attempt to a different binary or invalidate a recursive process gate. Host-loopback endpoint
forwarding uses a private, bounded raw TCP relay over the owned Docker exec
stream: only task-loopback listeners are opened, and host destinations remain
on the host. Endpoint and HTTP/HTTPS/SOCKS proxy ports are rewritten before
installation; TLS hostnames and raw protocol bytes are preserved. The relay
spans installation/execution and closes on cancellation or any failure.
Guest project configuration explicitly projects the guest tool, workload and
workspace paths plus proxy settings. Host-only cgroup parents and delegated
Perfetto instances never cross this boundary; guest parsing uses their empty
defaults. The relay rewrites proxy endpoints on that projected configuration,
not on a serialized copy of host capabilities.
Measured isolated sessions verify the daemon PID against this host's kernel
boot ID, namespace PID mapping, stopped state, process start time and owned
container cgroup before dispatching through the collector interface. They
never move Docker processes into native ASB cgroups. CPU accounting reads
existing container counters over the agent interval and labels those totals
as whole-container CPU, separately from agent-tree samples. Zero sampled CPU
without observed activity is unavailable, not an exact zero total. Isolated
memory/worker sampling, core CPU accounting and representative Perfetto
capture are qualified; this is not qualification of every optional collector
or full-population planning.
Collector attachment and guest bootstrap belong to agent preparation, with a
separate bounded deadline. The execution budget starts only at resume, and
an explicit end event stops execution accounting before metadata transfer and
official grading. Perfetto resolves the numeric PID to one process lifetime
alive at the start marker; ambiguous PID generations leave metrics unavailable
with a scope warning instead of silently mixing processes or inventing zeros.
Remote sessions separate installation preparation from protocol execution with
private typed prepared-bootstrap data. Production always constructs this data
through runtime validation and checksum-pinned prefetch; protocol-only unit
fixtures cannot enable a production preparation bypass. Dispatch, bounded event
receipt, measured-target resume and completion decoding remain separate phases
inside the same failure/cancellation envelope.
Failed preparation/execution cancels the controller through EOF and drains its
bounded protocol until cleanup acknowledgement and child exit, retaining the
original error and any cleanup failure. Normal completion still requires a
successful explicit close. Session destruction also attempts bounded owner
cleanup when a failure between preparation and execution bypasses both paths;
abrupt process termination still requires a recovery mechanism.
Only an explicit protocol close authorizes normal destructive teardown.
EOF, malformed input, failed protocol publication, exception unwinding and
preparation failure mark an interrupted attempt instead. Each original
workload adapter quiesces its declared resources under its existing owner lock,
retaining stopped containers, partial files, volumes, build images, submissions
and any existing official grades. It never calls normal Compose-down/volume
deletion or image cleanup on this path. All durable journal/daemon/owner
checks precede stopping resources. A changed journal or unconfirmed stop is a
preservation failure, not successful cleanup. `interruption.json` explicitly
states that preservation is neither grading nor retry permission.
Cancellation accepts a stopped-preserved acknowledgement and still reports
the interrupted operation as failed; normal completion requires removal and
cannot accept that acknowledgement as a completed attempt. A preserved
environment's filesystem is retained, not its running process/shell state.
Official sessions also track whether their latest evaluator response actually
completed a matching task grade. Errors, missing/contradictory resolved flags
and mismatched identities cannot take the normal close/removal path. Session
finish cancels into stopped preservation and reports the artifact location
instead. Valid unresolved and empty-submission outcomes are completed grades,
not permission to repeat the client. A later evaluator failure invalidates
the previous completion flag rather than allowing old evidence to delete
new or ungraded work.
Task controllers now persist a private `resources.json` before creating their
declared containers, Compose networks and volumes. The journal binds the exact
names to one attempt, a random ownership label and the Docker daemon identity;
an operating-system lock excludes recovery while the controller owns it. The
installed `asb_resources.py --quiesce ATTEMPT_DIRECTORY` helper validates the
entire declaration and observed ownership before stopping any container. It
looks up only the declared names and uses their immutable SDK objects, never a
host-wide listing, pruning or PID signals. Stopped containers, their files and
volumes are preserved. Live-controller refusal, wrong-daemon/owner rejection
and actual SIGKILL recovery are covered by provider-free tests.
This is observed resource quiescence, not a resume or grading receipt: a
daemon-side pending creation can finish after its controller dies. Source-build
images are now journaled too: SWE-bench declares its source-build tag before
invocation, while the generic Compose bridge replaces SDK shared build-output
names with per-attempt tags and records their recovery labels before building.
Prebuilt/upstream images are not claimed. Normal teardown removes only those
owned tags with `noprune` and without force; it does not remove shared aliases,
parent images or build caches. Recovery preserves these images and reports
their immutable IDs, keeping them available for later inspection/evaluation.
Read-only outcome reconciliation requires matching image-cleanup records when
the journal declares build outputs. Interrupted evaluator recovery, recovered
submission grading and sweep-slot reconciliation still require integration;
stopped preservation alone does not supply these missing outcomes.
Before launching an official workload controller, ASB publishes a private,
immutable attempt reference under the host workspace's `.asb-attempts/`. It
contains only the intended task/dataset and project-local artifact location,
not an actual attempt outcome or measured baseline. In a sweep, the generic
allocation observer binds that reference to the existing replica start intent,
selected input digest and configuration. A failed binding stops before the
controller is spawned. Completed replica receipts additionally seal that
allocation record; legacy/native outcomes without an allocation remain valid.
`asb workload recover REFERENCE [--json]` uses `WorkloadAdapter::recover` to
select the owning SDK's resource helper. Reference locations are bounded and
validated against the artifact-owning project, not the workspace directory
(which can be external or inside another project). Helpers provide a bounded
one-shot response and must exit successfully. Human output describes preserved
work; JSON contains only the recovery report. A no-op inspection of already
absent resources does not add files to sealed evaluator evidence. Recovery
still cannot assign a grade or admit an interrupted slot for retry.
`asb workload recover REFERENCE --inspect-grade [--json]` instead inspects an
existing official grading report through `WorkloadAdapter::inspect_grade`.
The owning adapter checks task/submission identity, baseline, SDK and its own
evaluator artifacts. Removed attempts retain their normal cleanup checks;
stopped-preserved attempts additionally require matching interruption/resource
declarations. The inactive-owner lock is acquired before reading evidence.
Missing, inconsistent, ambiguous or incomplete grades are errors, never an
invented unresolved grade. No agent, evaluator or Docker operation runs.
The report explicitly leaves process success, exit code, elapsed time, tokens
and steps null. `recorded_cleanup` describes retained evidence, not a new
observation of Docker state. This is current local artifact consistency, not
historical attestation or recovered execution metrics.
Inspection uses a private temporary support copy and the owning project's
installed SDK interpreter; it never replaces installed SDK helper files that
another worker may be using. Temporary copies are removed on success/error,
while retained artifacts remain unchanged. The operation publishes no outcome
or resume receipt and cannot admit a new attempt. Recovered grading and
outcome import remain separate work in the standalone inspection command.
`asb sweep SWEEP.toml --import-grades` explicitly imports completed retained
official grades into separate `.asb-imported-grades/` reporting records. It
holds the sweep output lock, loads the saved plan without execution preflight,
and binds each grade to its original configuration, start intent and workload
allocation. All selected candidates are validated before any publication.
Existing outcomes/measurements are never replaced, repeated identical imports
are unchanged, and conflicting or duplicate actual attempts are errors. This
operation neither runs clients/evaluators nor creates completion/resume receipts.
Compare revalidates the original bindings and adapter-owned evidence before
using an import. Inspection reports include bounded SHA-256/length entries
for exact artifact bytes read, detecting edits even when parsed grades agree.
These are current local consistency checks, not historical attestation.
Reports label the source `imported-official-grade`; process status, time,
tokens, steps and concurrency cohort stay unknown. False grades stay false.
SDK metadata inspection currently repeats per imported attempt during compare;
population-scale presentation/inspection optimization remains delivery work.
Actual recovered grading and interrupted execution continuation are separate
from this report-only operation.
`asb workload recover REFERENCE --grade [--json]` adds explicit retained
submission grading through `WorkloadAdapter::grade_retained`. The three original
SWE-bench registrations now implement it: their installed pinned catalog must
match the retained task/dataset/base, and the captured patch hash/length/image
must match the original submission. A new official evaluator runs against that
frozen patch and original immutable image, never a new agent or restarted task.
Completed true/false grades are validated and reused rather than rerun. Earlier
failed evaluator artifacts stay in place; a private create-new
`selected-evaluation.json` explicitly identifies the validated evaluator instead
of guessing the latest directory. Unfinished evaluator journals are independently
locked and quiesced before another grading attempt. Missing ownership evidence,
conflicting completed grades, changed patches/catalogs and unavailable images
are errors. Grading failure preserves artifacts without selecting a grade.
If complete official SWE-bench reports remain after a missing/failed ASB
wrapper, explicit grading instead reconstructs their normalized outcome without
Docker, another evaluator or an agent. The original evaluator lock must be
inactive and its task-specific cleanup confirmed. Frozen SDK inputs must match
the installed pinned task, candidate patch, image and SDK; the official summary
and instance report must agree. A new metadata-only evaluation directory keeps
bounded copies plus a manifest binding every original input/report and the
old wrapper's bytes or absence. No original artifact is replaced. Private
staging keeps an interrupted copy from looking like another official grader;
a complete derived result missed before selection can be reconciled without
rerunning tests. Inspection/import/compare revalidate the source manifest,
ownership and cleanup as well as the copied reports. Changed/incomplete or
ambiguous raw outcomes remain errors, never permission to repeat tests.
The helper observes EOF/SIGTERM to cancel its official evaluator; raw descriptor
reads avoid a buffered-stdin daemon shutdown race. Returned reports use the same
grade-only inspection schema and still cannot invent missing execution metrics
or admit a new agent attempt.
Original Terminal-Bench 1.0 now supports the first official verifier invocation
for an explicitly captured pre-verifier environment. It binds the installed
definition hash/dataset/SDK, original container and image, and every declared
resource. Under the original owner lock it stops and resumes only those exact
containers, never Compose up/build, a new agent or replacement volumes. Source
preparation persists the main service's network aliases and explicit static
addresses, so grading can restore its original connectivity after an agent
override. Dynamic addresses are not pinned unnecessarily; unrelated containers
and networks are not removed. Success/failure/cancellation stop and preserve the
owned resources, and a recovered grade is selected only after cleanup confirms
preservation. SDK test logs/results use a unique recovered directory/session;
the original submission and prior records are not overwritten. Missing execution
metrics remain unknown. Normal and recovered Terminal-Bench grades must identify
their upstream environment baseline, not pretend to have a Git-task baseline.

A durable `evaluation-started.json` is written before side-effecting tests.
Missing results after that marker require outcome reconstruction, not an implicit
test retry against already-mutated state. Original tasks requiring the lost
agent shell cannot be truthfully graded in a new shell and are refused before
restart. The new operation therefore does not reconstruct lost process/shell
state or recover an already-started verifier.
Original Harbor now also supports first-verifier grading of a captured,
stopped-preserved environment. It validates the installed original task/SDK,
definition, image, resource ownership and network topology before resuming
only those exact containers. Its SDK environment delegates execution/copy to
verified immutable container IDs, never Compose up/build/discovery or a new
agent. The official verifier still owns test upload, command construction and
binary reward parsing. Original log mounts remain unchanged; current verifier
outputs are downloaded into the new private recovered evaluation directory.
Transport output and child shutdown are bounded, including full-pipe failure.
Normal Harbor grading now publishes the same durable verifier-start record.
Completed false/true grades are preserved and reused, recovered cleanup must
confirm stopped preservation before selection, and inspection/import/compare
follow the explicit selected evaluator.
Both original Terminal SDKs now publish a create-new, durable SDK-return
observation before the final normalized result. It binds the parsed outcome,
task identity, start record and raw pane/reward bytes (including absent reward
formats). If final publication is lost, retained grading reconstructs from that
record without an SDK invocation, Docker or a restarted service. Private staging
and explicit selection preserve every original artifact; inspection/import/compare
revalidate the original observation, raw files, wrapper bytes/absence and cleanup.
An interrupted metadata copy or missed selection can be reconciled without tests.
Missing trustworthy SDK-return evidence remains unknown; raw logs alone are not
permission to infer success or repeat a side-effecting verifier.

Explicit sweep reconciliation can also close an allocated, started slot whose
ordinary run record was lost, provided adapter-owned inspection confirms its
official grade. This creates a separate grade-only closure, not an ordinary
execution completion receipt. It binds the original selection/start/allocation,
current declared input digest and preserved per-slot artifacts. Resume validates
that seal and does not rerun the client. Comparison independently checks the
official grade without requiring today's executable to match the old one.
Timing, process status, tokens, steps and execution cohort remain unknown;
reused output returns a non-success status disclosing that uncertainty, even
for a resolved grade. Partial collector files remain diagnostic downloads,
not final measurements or inline performance graphs. Compatible report-only
imports remain preserved and are deduplicated only in presentation.
Unobserved grades, conflicting ordinary outcomes and changed preservation
evidence still cannot authorize silent retry. Final continuation/disposition
review and delivery qualification remain unfinished.
See the full benchmark delivery
checklist before claiming complete upstream benchmark support.
Their shared installer also downloads a checksum-pinned original
`SWE-bench/swe-bench-tasks` archive into the project cache and evaluator tool
directory. It validates every selected population's identity, original
evaluation data and test lists against that archive. When an image is not local
and its registry pull fails, preparation can build the exact original task
Dockerfile through scoped Docker Buildx access. This revision's Dockerfiles
are self-contained; the build context contains only the Dockerfile, never gold
patches, test programs, task metadata or host directories. Builds require
Buildx/BuildKit per-build `--resource` support, enforce 2 CPUs/8 GiB/600 seconds,
record archive/Dockerfile/image identities and use a unique ASB-labeled image
tag. Cleanup removes only that tag with `noprune`, not published images or
shared caches. Source building does not change daemon settings or the selected
default builder. The evaluation uses the exact resulting immutable image ID.
The explicit `terminal-bench-1.0` and `terminal-bench-2.1` adapters likewise
install their original complete task catalogs and own environment sessions.
Version 1.0 delegates its Compose environment, tmux tests and parser to the
original SDK; 2.1 delegates environment construction and verifier/reward parsing
to Harbor. Neither SDK chooses an agent/provider/model. Both consume the same
generic guest execution contract as SWE-bench, with attempt-scoped teardown
and original environment-definition baseline evidence. A missing prebuilt
Harbor image can fall back to its original task Dockerfile within the same
preparation time budget. Temporary SDK overlays stay in the attempt directory;
no global image/build pruning or host service changes are performed.
Selected-client bootstrap comes from the installer recipe and owning tool
adapter's declared distribution assets. The host prefetches verified archives
only; it does not replace runtimes used by other clients or bridges. The guest
installs its declared runtimes and selected client with the same Rust recipes,
without relying on curl in the task image. Preparation carries its project
root/configuration explicitly, so external catalog locations do not redirect
SDK or attempt storage. Transfer receipts and temporary files remain scoped
to the project attempt; public npm caches are copied only for npm recipes.
An adapter-owned execution preflight publishes the compiled SDK helpers and
prefetches selected guest runtime archives before any task environment starts.
Sweeps deduplicate this over selected project/workload/agent combinations and
perform it before preserving old outputs or taking input snapshots. Read-only
runtime metadata discovery does not download or publish anything. Repeated
identical helper publication preserves the existing private regular file;
changed helpers replace it atomically without following destination symlinks.
Other aliases install pinned prompt metadata. These commands do not handle
credentials.
`asb init` prepares `.asb/python/venv` using an installed Python 3.12 or 3.13,
including interpreters managed by uv. If none is available, uv can download
Python 3.12 into `.asb/python/uv-python` using the project proxy settings.
If setup is unavailable, init warns and Python tool installs retry it later;
other project commands remain usable. Each Python agent retains its own
isolated package environment created from the project interpreter. Agent-run
subprocesses see the project Python on PATH; a Python tool's own environment
comes first so its subprocesses can access the tool's dependencies. If an old tool environment
uses a different Python, ASB moves it to a numbered `venv.asb-backup-*`
directory before creating the replacement; it never deletes that backup.
OpenJiuwen's Python recipe pins `filelock` 3.32.7 because the 4.2.2 release
stalled while the pinned OpenJiuwen client acquired filesystem locks. A fresh
`asb tool install openjiuwen` applies the tested dependency to the local venv.
The `bpftrace` recipe copies the exact host executable into the project and
writes `bpftrace-compatibility.json` with its version, kernel release, and
architecture. This keeps measurement runs tied to the kernel on the machine
where the tool was installed; rerun `asb tool install bpftrace` after changing
kernels or moving the project to another host. If no host/distribution binary
is available, it downloads the pinned source release and builds bpftrace in the
project directory with the local compiler and development libraries. If the
host binary or source build prerequisites are unavailable, the failure prints
typical copy-paste commands for Red Hat/Fedora, Debian/Ubuntu, and openEuler
so the user can install either the matching host tool or the needed development
packages before retrying.
Built-in BPFTrace profiles are small, separate source files under
`crates/asb-cli/src/collectors/adapters/bpftrace/`. They start only after ASB knows the
agent PID and therefore scope their probes to the benchmarked process. The
portable catalog covers scheduler wakeup latency, CPU migration, futex waits,
syscall rate, and kernel lock contention. Each profile has its own JSON result
and SVG; comparison consumes only its `@asb_*` aggregate maps, embeds that
profile SVG in the HTML artifact view, and emits one cross-run chart per
numeric metric. NUMA topology and language-runtime data-structure probes remain
custom profiles because their event layouts are neither portable nor stable.
Custom sources are bounded, regular UTF-8 files, opened once during preflight.
The adapter retains the exact validated and parameter-bound bytes in generic,
session-owned typed collector state; configure and attachment never reopen the
path or reconstruct configuration from the environment. Other adapters use the
same state contract without collector-specific orchestration branches. Attachment
adds ASB's process-tree tracking prelude and requires readiness before the agent
resumes. A missing preparation state is an error, not permission to reread.
Every custom process probe needs a positive, side-effect-free process-tree
predicate. The validator handles complete multiline blocks and nested Boolean
expressions, rejects membership mutation and opaque preprocessing, and reserves
`@asb_tree` for the generated prelude. This is a process-scope contract, not a
sandbox for arbitrary custom actions: custom programs remain trusted code.
Numeric custom parameters use namespaced identifier substitution rather than
the unsupported bpftrace `-D` option; see the development guide for migration.
The `perfetto` recipe downloads SHA-256-pinned official Tracebox and trace
processor binaries into the project tool directory. It supports Linux x86_64
and aarch64. Ftrace itself is a tracefs interface provided by the running host
kernel, rather than an executable to copy into the project. Perfetto preflight
records its delegated named instance through the owned manual service. Missing
delegation points to `asb init --configure-host`; capture never escalates through
sudo or resets the primary tracefs instance. Other collectors may use their
documented bounded `sudo -n` capability probes. Normal commands and CI do not
require kernel tracing access.
Preparation also probes perf counter and record access, the project-local
Perfetto trace processor, process-scoped pidstat
CPU/I/O sampling, explicit flamegraph executables, and custom bpftrace program
readability. CPU/I/O adapters prefer the project-local pidstat installed with
either sysstat tool. Tape and local bridge startup occurs before the agent and
reports executable and scratch-directory access failures. These checks cannot
guarantee future access after permissions change, access to the yet-unspawned
agent PID, or successful attachment of arbitrary custom bpftrace programs.
Granting unrestricted passwordless sudo for a tracing executable can enable
arbitrary privileged operations; ASB does not install sudoers rules.
ASB opens its delegated instance's marker as the benchmark user. After
Tracebox reports readiness, ASB checks the matching `tracing_on` if readable
and writes a non-secret, run-specific startup marker before starting the agent.
It does not enable or disable host-wide tracing. An inactive trace or failed
marker write stops the run with a tracing-specific diagnostic. After recording,
the trace processor must find the startup marker in the trace; missing markers
are reported as an incomplete trace rather than silently accepted. A permission
failure points to the explicit scoped host-reconfiguration flow. A later tracefs
remount or reboot requires reconfiguration. Concurrent captures sharing this
process's instance may observe each other's markers; analysis uses each
session's unique trace identity and the selected process lifetime.
The `pi` agent uses the pinned `@mariozechner/pi-coding-agent` npm package and
its non-interactive print mode. The generic endpoint negotiator selects a
compatible API interface without coupling Pi to a particular provider.
Installing `cli2key` also asks its bridge to refresh the local API key; model
discovery and runs then start the bridge and use that key ephemerally.
`asb tool list` and `asb workload list` expose these two catalogs without
installing anything. Command-specific help is available with `--help`.
Model discovery excludes models whose live metadata explicitly reports no tool
support. Models with unknown support remain selectable because several official
catalogs omit that field; execution warns about the uncertainty. Documented
standard capabilities are used for Anthropic and the cli2key bridge when their
catalogs omit that bit. Offline replay uses the cassette's recorded tool request
instead of contacting the provider.
Ollama discovery recognizes the server's capability arrays and, when a list
entry omits capabilities, queries `/api/show` for that model. Explicit absence
of tool support is rejected; missing capability data is reported as unknown.
For Codex with Ollama, the agent adapter writes an
isolated local provider configuration using Ollama's Responses endpoint;
record and replay instead use the tape endpoint. This routing does not itself
guarantee that a particular model can complete an agent workload.
The ASB source checkout is not a benchmark project: `init` rejects its
distinctive workspace layout before creating any files.

The reusable `cassette` backend defines multi-track manifests, compatibility
identities, bounded selective ZIP reading and reproducible selection plans.
It neither starts providers nor changes workloads. Metadata parsing uses a
validated bounded central-directory snapshot before enabling payload reads;
selected entries receive CRC, length and SHA-256 validation. Its single-owner
`RecordingJournal` publishes immutable recording intent, started slots and track
commits into a private partial directory. Frozen publication binds those records
to a bounded, fully verified Deflate ZIP. A durable prepared receipt precedes
no-clobber final linking and parent-directory sync; interrupted owners recover
the exact prepared bytes rather than rebuilding a different archive. Publication
confirmation is separate from resumable staging cleanup. Missing published
archives are never silently recreated. Per-track evidence binds immutable
metadata and selected entry bytes, not staging paths that cleanup removes;
batch evidence verification reuses one archive index. Sweep and cassette output
owners share a private persistent-inode lock implementation, with domain-owned
paths and diagnostics. Finalization requires the caller to stop/join recording
producers; freezing the journal does not drain a proxy itself. This backend is a
delivery foundation, not yet a change to run/sweep's JSONL CLI behavior. See
[multi-track delivery contracts](MULTITRACK.md) for the integration sequence.

Recording and replay are implemented as a separate `tape` module behind the
runner boundary. The `run --record` and `run --replay` flags enable the
configured proxy chain; the default chain contains ASB's built-in loopback
`llmtape` adapter. Generic chain management and the `ProxyAdapter` and
`ProxySession` traits live in `src/proxy/mod.rs`; each registered implementation
lives in `src/proxy/adapters/`, with llmtape's settings, process and replay
validation contained in its own adapter. A hop receives its own adapter-specific
settings and the endpoint of its next hop. The chain is started from the provider inward, so an agent always receives
one loopback ingress URL and remains unaware of whether zero, one, or many
proxy hops are enabled. Set `ASB_PROXY_CHAIN` to a comma-separated list ordered
from agent to provider to configure additional registered adapters; an empty
list is rejected when record/replay is enabled. Provider adapters continue to
own endpoint routing, record-mode upstream resolution, and optional local-
bridge lifecycle. The runner and sweep executor retain no provider-specific
proxy knowledge and tear every hop down after the child exits. `--record`
writes a JSONL cassette with a bounded `delay_ms` response latency for every
interaction; `--replay` requires that cassette and never starts cli2key or a
live provider. Passing `--respect-record-timings` with `--replay` makes llmtape
match each request first, then wait for its recorded delay before returning the
response. The same option is available on `asb sweep`. This keeps the
benchmark configuration contract unchanged and keeps the tape component an
optional, project-local development capability rather than a runtime
dependency. The proxy checks normalized complete request bodies and rejects
mismatches and extra requests. The default llmtape adapter is embedded in ASB
and requires only Node.js; it does not use or install an external recorder
package. ASB never falls back to an unqualified host `PATH` lookup; set
`ASB_LLM_TAPE` explicitly to use another compatible executable for the llmtape
adapter.
The built-in proxy binds an operating-system-selected loopback port itself and
atomically publishes a bounded readiness receipt in its private session directory.
ASB checks that receipt's child PID, loopback host and nonzero port before
exposing the endpoint; an unrelated listening socket cannot satisfy readiness.
Explicit `ASB_LLM_TAPE` overrides retain the legacy fixed-port/TCP readiness
protocol for compatibility. Such overrides are responsible for their own
listener ownership; they do not provide the built-in receipt guarantee.
Embedded proxy invocations also hold a private stdin lifetime pipe. Parent
exit closes it, including death before Node startup. EOF and normal shutdown
close the listener and its connections, prevent further recording, and wait
for owned outbound subprocesses with bounded TERM/KILL escalation. An abnormal
embedded proxy exit cannot confirm cleanup. Legacy overrides retain their own
stdin/lifecycle contract and do not acquire this built-in guarantee.
Measurement collection remains
explicitly opt-in through `asb run --measurements DIR`; CI tests the tape on
loopback but does not start live agents, live providers, or host collectors.

Finite human-entered names use bounded edit-distance suggestions on failure.
The original value is never replaced automatically; provider model IDs remain
provider-specific unless a finite local catalog is available.

The provider baseline stage is deliberately separate from command construction.
It installs only protocol-wide environment variables. Provider-owned local
bridge lifecycle and target-specific configuration are delegated through their
respective registries and adapters, so adding an integration does not add a
target-name branch to `run` or the benchmark configuration format.

Claude Code, Goose, Cline, and Continue receive the endpoint selected by the
generic negotiator. Continue receives a project-local model YAML generated for
the selected model and interface.
Goose's adapter sets its `OPENAI_HOST` or `ANTHROPIC_HOST` to the tape endpoint
for both recording and replay; replay uses a placeholder credential.

Setup, sweep execution and comparison share the same manifest type and bounded
reader: up to 65,536 configurations in 16 MiB, independently of each individual
configuration/prompt's smaller bound. All manifest reference paths are checked
before execution. Setup resolves each selected trial set once, checks the whole
agent/model/task cross product before creating files, and rejects oversized
selections instead of publishing a sweep it cannot consume. These format
bounds accommodate complete original populations independently of scheduling.
`asb sweep SWEEP.toml --jobs N` runs up to N distinct configurations (`1..=32`,
default 1). This is separate from each configuration's identical replicas.
`--max-active-attempts N` bounds the sum of admitted replica counts (`1..=64`)
and defaults to max(jobs, largest selected concurrency), preserving sequential
replica choices. A configuration must fit the budget; invalid admission is
rejected before preserving/moving existing results. A bounded FIFO worker pool
holds replica-weighted slots through preparation, execution, grading and cleanup.
The count budget is not a host RAM reservation; task environment CPU/memory/PID
limits remain adapter-owned, and operators must choose jobs for host capacity.
Selected scheduling is saved in the plan and disclosed in JSON/HTML reports
because concurrent configurations share host resources.
Parallel terminal output uses private, bounded per-configuration files and is
drained as whole configuration blocks on completion. Output errors stop new
admission and join admitted work; owned temporary duplicates are then removed.
Sequential defaults retain immediate preparation warnings and streaming
`--show-output`. Raw agent output conflicts with `--json` rather than corrupting
JSONL. Shared tool dependency publication is serialized without serializing
execution. Each JSONL summary includes agent/provider/model/workload names and
its actual task/attempt identity, so completion-order output remains identifiable.
Scheduling and output exclusion are separate from completed-group continuation
and do not claim abrupt-crash container recovery or full-population evaluation.
Without `--resume`, every new sweep starts fresh output workspaces. Previous results and reports
are protected by a per-output operating-system file lock held from planning
through final comparison. A competing writer fails before moving prior data;
unrelated manifests remain independent. The persistent lock file is never
unlinked, and process exit releases its lock without a stale-lease repair.
This exclusion is not container recovery after abrupt host/process failure.
Previous results and reports
are moved under `<stem>-history/<run-id>/`, retaining the original result/report
layout so relative artifact links remain usable. Default replay recordings
are hard-linked into the new attempt without copying payloads; explicitly
selected input roots inside old results are rebased to their preserved path.
New default record attempts do not write those links. Live/record/replay mode
and replay timing choice are saved in the selection snapshot and surfaced by
comparison; old records without this information remain mode-unknown.

CLI and TUI setup share a bounded sweep publisher. Repeated CLI selections
expand into the same agent/provider-scoped-model/workload/trial cross product;
`--output` names the sweep for a matrix and a benchmark for a scalar selection.
All CLI selections are validated before publication. New sibling configuration
directories and create-new manifests avoid replacing existing sweeps; generated
benchmarks have distinct logs and measurement directories. Trial catalogs are
loaded once per workload. Explicit model IDs require no provider discovery;
`--model all` (including provider-scoped `=all`) deliberately uses the owning
provider's live discovery and fails on unavailable/empty catalogs before
publishing files. Other `all` selectors expand from the owning registries,
local workload definitions and enabled/original trial catalogs. Expansion is
deduplicated and retains the normal bounded sweep contract. `all` is reserved
and cannot be a selectable identifier; dynamic catalogs reject collisions,
the TUI excludes colliding entries and CI checks every static registry.

`asb compare BENCHMARK_OR_SWEEP.toml` dispatches by TOML shape, not filename.
Standalone reports reuse the normal replica-path helper, read the most recent
bounded JSONL record per configured log, and resolve project-scoped measurement
directories. Current run logs also record actual measurement overrides, so
comparison does not guess capture locations. Logs' transcripts never enter the
report. Identity and measurement/outcome consistency checks apply to standalone
rows too; old measurements cannot silently replace a newer logged attempt.
`--report` invokes the platform browser opener only after successful generation,
without shell interpolation. Failure to open leaves the generated report intact.
Automatic sweep comparison never invokes an opener.

Sweep comparison reads the isolated `<sweep>-results/` directories
created by `asb sweep`. It writes bounded `comparison.json` data, an offline
`report.html`, and SVG charts to `results/<sweep>-comparison/`; no provider or
agent is contacted. The generic outcome reader additionally loads recorded
per-replica task summaries when optional measurement collection is disabled or
its run record is absent. It validates actual identity and slot mapping, rejects
conflicts with existing measurement records and discloses its result source.
Grades do not disappear merely because collection was disabled, and missing
collector/host/time-series data is never fabricated. Outcome reporting does not
prove resume eligibility or historical integrity. The report links every result artifact and explains the
measurement fields. Its dependency-free JavaScript can enable or disable
benchmarks, agents, providers, models, and workloads without rerunning the
sweep. Each chart
has numbered y-axis ticks and a metric-derived unit label (for example `%`,
`ms`, `KiB/s`, `ops/s`, `tokens`, or `value`) so the plotted scale is readable
without opening the machine-readable summary. Collector stdout is never
treated as a time series; only an explicit `series` array can produce a line
graph. When perf permissions, samples, or FlameGraph tools are unavailable,
`flamegraph.json` records an `unavailable` status and actionable reason instead
of silently pretending that a flamegraph exists. Before the agent starts,
perf and configured BPFTrace collectors run a bounded direct capability probe;
only a collector whose probe needs elevation is retried with `sudo -n`. If
that non-interactive authorization is unavailable, preparation stops and
prints a scoped sudoers repair command rather than prompting or starting a
partial benchmark.
