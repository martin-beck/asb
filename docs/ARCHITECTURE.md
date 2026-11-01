# Architecture

Benchmark cassette/result labels and manifest references share the private
`benchmark_paths` policy. Its legacy component normalization is lossy and keeps
existing artifact names stable; it must not be used as a uniqueness allocator
or standalone path sandbox. Comparison retains its collision-free graph-name
allocator. Sweep execution and comparison share the relative-TOML reference
checks, including existing symlink behavior and actionable path diagnostics.

The versioned [benchmark module contracts](BENCHMARK_MODULES.md) separate suite,
dataset, harness and evaluation protocol. Owning adapters expose descriptors
through `asb workload describe`; new selection snapshots bind the same metadata.

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
  configurations and a sweep manifest. Each choice carries a typed domain key
  independently of its rendered/search label: prompt names and trial-set files
  remain distinct even when their labels collide. Provider/model bindings,
  installation and collector dependencies consume those keys; saved trial edits
  reopen by trial ID. Six named pane kinds define navigation order and install
  eligibility. Concurrency choices use the benchmark's shared maximum and
  publish exactly one numeric value;
  workload adapters may additionally contribute a bounded setup extension.
  The wizard adds the extension tab only while its owning workload is selected,
  reconstructs values from the adapter's installed catalog, and drops values
  immediately when that workload is deselected. Extension controls such as
  `[all]` and `[none]` are frontend-only selection operations, never workload
  identifiers persisted into benchmark TOML;
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
  process execution. Its common child-pipe pump owns both readers without
  reader threads or an output queue. Each stream retains at most 8 MiB while
  continuing to drain excess; two 16 KiB read buffers and bounded batches keep
  aggregate capture/working storage bounded during execution. These are evidence
  prefixes, not whole-stream totals or diagnostic tails; the existing 1 MiB
  persisted-log prefix is a separate presentation budget. Interrupted reads retry;
  permanent read errors are never EOF. Execution, one-second post-exit draining
  and six-second direct-child cleanup have separate absolute deadlines. The
  owned child's confirmed exit freezes execution timing before pump drain work;
  native gates retain their existing quiescence-confirmed exit boundary.
  Direct-child fallback custody is established before spawn, including for
  unwinding; uncertain cleanup transfers the owned child rather than waiting
  indefinitely or dropping it. Native gate admission and descendant ownership
  remain separate. Incomplete capture is an operational failure that withholds
  continuation receipts, not a replacement official grade. It suppresses
  transcript-derived answer/usage/step assertions while retaining independently
  collected adapter artifacts and actually validated cassette accounting.
  Unix readers use nonblocking descriptors; Windows has a private-pipe
  availability pump, source-reviewed but not runtime-qualified on Linux.
  Other std platforms fail before direct spawn until a cancellable pipe backend
  exists. Arbitrary borrowed `Write` implementations remain caller-controlled
  and can block inside a write: there is no claim of hard cancellation there.
  The Unix quiescent guest route instead owns deadline-aware duplicates of its
  controller pipe/socket endpoints; it rejects terminals/regular files before
  changing flags, restores the shared file-description flags on destruction,
  and cancels on transport failure. The controller owns only the opposite read
  ends; no unrelated producer shares these private write descriptions. Native
  presentation errors preserve evidence. Non-Unix guest presentation still has
  the borrowed-writer limitation. Retained ThroughEvaluation service lifetime
  and its existing bounded capture/ownership are unchanged. Shared native launch
  failures notify waiting peers before bounded child settlement. Repeated
  execution cannot admit another batch from an incomplete captured slot, even
  when an independent file grade and sibling outcome are available;
- `compare`: offline reports use explicit loaded, globally validated, prepared,
  rendered and published phases. Stable row identities and typed result sources
  bind original evaluation evidence; preparation computes population coverage
  and unique chart identities once. Structured outcomes, recorded modules,
  execution-plan fingerprints and retained-grade provenance are validated at
  the source boundary. Preparation computes metric-policy and binary-resolution
  cohorts once, including conservative missing-slot coverage; JSON and HTML
  render those same summaries without consulting the live workload registry.
  Native rewards retain nullable resolution and independent execution status.
  Recorded planning objects, controller caps and module revisions remain
  provenance metadata; they never become observed metric charts.
  See [benchmark outcomes](BENCHMARK_OUTCOMES.md) for policy semantics.
  Sources are opened once as bounded regular
  files (64 KiB per benchmark, 32 MiB retained configurations, 16 MiB per
  manifest/collector/preview and 64 MiB per saved plan). Manifest, configuration
  and saved-plan validation consume retained snapshots, never a refreshed second
  identity read. Collector previews retain physical paths separately from published
  logical links. Nullable time-series observations keep aligned timestamps in
  full JSON; SVGs disclose a 2048-observation bound preserving segment endpoints
  and extrema, never bridge unknown gaps, and distinguish sample indices from
  measured milliseconds. Raw state/event timelines are not downsampled.
  Scalar charts retain accepted negative metrics around an explicit zero
  baseline, with signed labels and original observations in hover metadata.
  Signed scalar and series coordinates share overflow-safe normalization;
  nonnegative scalar scales retain their existing zero-to-maximum geometry.
  A comparison of a sweep first acquires the sweep's persistent output lock
  before reading any source and holds it through publication, excluding result
  rotation by a concurrent sweep. Nested result comparisons and nested sweeps
  acquire enclosing sweep owners from outermost to innermost. Sweep-owned
  final comparison borrows the existing locks rather than acquiring them again.
  A separate persistent report
  lock guards private same-filesystem staging;
  Linux and macOS native rename/exchange publish a whole generation after rendering
  and synchronization. Replaced generations, including user-added files, remain
  recoverable backups. If exchange is unsupported, a no-replace capability probe
  runs only in the private stage before moving the old report. A documented
  recoverable backup/commit/rollback path then has a brief destination-absence
  and crash window; it never overwrites a foreign destination. Failed rollback
  retains the old backup and prints a restore command. Missing safe no-replace
  support fails closed before moving user data. No native macOS runtime
  qualification is implied by Linux tests. Post-commit
  durability or confirmation errors explicitly report that publication occurred;
- `measurements`: opt-in process-scoped runtime collection. It writes bounded JSON
  records for host facts and process/runtime observations, and dispatches only
  host/process collection through the `CollectorAdapter` registry in
  `collectors/adapters/`. Agent adapters parse usage and steps, provider adapters
  define provider usage semantics, replay provides cassette observations, and
  workload evaluation owns task success and summary aggregation;
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
  CPU/I/O and BPF processing reads complete stopped stdout files, not their
  256 KiB diagnostic prefix. Input is bounded to 16 MiB; truncated final lines,
  invalid UTF-8, nonfinite parsed values or processed data above a 12 MiB
  serialized budget fail the collector without prefix measurements. The
  processed budget reserves worst-case sixfold JSON diagnostic escaping and
  outer fields beneath comparison's 16 MiB
  result limit. Temporary full output is removed after processing on either
  success or failure; an optional collector error never erases an official grade.
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
  and enlarge controls as bar charts. Built-in BPFTrace profiles emit bounded
  cumulative one-second map groups, each framed by a kernel
  `nsecs(monotonic)` timestamp. Per-metric series retain the actual shared-clock
  offsets from the execution-start event in milliseconds; sparse keys have
  their own timestamps and are never zero-padded or aligned by array index.
  Preparation/post-execution groups are excluded from series. Final unframed
  maps still supply scalar aggregates, not invented final timestamp samples.
  Legacy/custom output without timestamp frames remains aggregate-only.
  Timing sidecars disclose missing clocks/observations and asynchronous map
  printing: a callback timestamp is not an atomic counter snapshot. This
  follows the [pinned bpftrace clock and print semantics](https://github.com/bpftrace/bpftrace/blob/v0.20.2/docs/reference_guide.md).
  Perfetto retains its timestamped
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
  `cpu_accounting_source` to `cgroup-v2`; without delegation, that same
  `cpu-accounting` adapter owns the Linux procfs process-tree fallback sampler.
  `measurements` only publishes its normalized component timing. The
  `cpu-accounting.json` record contains the source,
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
  Declining or an invalid answer runs no listed mutation commands. A failed
  command is reported and init remains usable. It never edits sudoers or changes
  these settings silently.
  The private `project::host_setup` policy separates declared operations,
  capability, explicit consent, and execution. A consumed approval owns exactly
  one command group. The initial passwordless-sudo probe is shared within the
  sysctl/cgroup setup invocation, not across processes or independent groups.
  An approved interactive sysctl does not promote cgroup setup to interactive
  sudo, even if sudo refreshed credentials; this preserves the prior policy.
  Sysctl failures do not suppress independent settings, while cgroup creation,
  ownership, permissions and optional controller enabling stop at the first
  failed prerequisite. Failure is best-effort, not rollback: earlier successful
  operations may remain applied. Tracefs retains its separate lease and setup
  contract, sharing only the explicit consent parser.
  On a systemd cgroup-v2 host, the same best-effort init path creates a
  user-owned `asb.slice` beneath the current user slice, enables the CPU
  controller, and records its parent in `.asb/project.toml`; measured runs use
  that parent automatically. A non-systemd layout or failed setup is reported
  and falls back to process sampling. Init never changes resource limits or
  edits sudoers.
  `asb compare` also writes an observed scheduler thread-time composition SVG.
  These are concurrent process-tree totals, not additive elapsed-wall categories:
  elapsed wall is shown only as a separate numeric reference, never as a residual
  computed by subtracting multi-thread totals. CPU/user/system/block-I/O component
  measurements retain their own separate metric charts.
  the Perfetto adapter validates tracefs/ftrace access and starts project-local
  Tracebox before preparation begins. It waits for recording readiness before
  emitting fixed, non-secret lifecycle tracepoints; after the agent is spawned,
  it scopes scheduler analysis to that PID and its descendants. Each concurrent
  run has a distinct trace identity. The project-local trace processor
  extracts stable aggregates,
  and ASB preserves the native `.perfetto-trace`, a bounded JSON record, one
  portable SVG per extracted metric, and a bounded per-thread scheduler-state
  timeline SVG. Its first 4,096 observed intervals over at most 64 unique thread
  lifetimes are a view, not the aggregate source. Labels disclose displayed/full
  interval and thread counts; the axis spans the actual execution bounds in
  milliseconds, including gaps. Complete positive-duration state totals use a
  separate materialized query with both boundary overlaps clipped. Unfinished
  states (`dur=-1`) are counted as excluded, never guessed to last until the
  end marker. `Running` means on-CPU; `R` and `R+` mean runnable, as defined by
  [the pinned Perfetto tracker](https://github.com/google/perfetto/blob/v58.2/src/trace_processor/importers/common/thread_state_tracker.cc#L27-L55).
  Missing blocked-reason I/O classification remains unknown (`io_wait_ms=null`)
  rather than becoming measured zero; any positively observed I/O wait retains
  its own metric. Unique thread IDs keep reused numeric TIDs in separate lanes.
  Neither observed totals nor the bounded view reconstruct unobserved states;
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

The runner uses private consuming attempt states rather than one shared mutable
orchestration context. `AdmittedAttempt` publishes start intent before resolving
the project; `PreparedAttempt` owns the workload, optional proxy and measurement
sessions through execution. Native command intent becomes an owned
`PreparedNative` only after adapter, endpoint, Python, runtime and replay
preparation, so the gate executes that exact command. `ExecutionEvidence` owns
captured output, guest observations and the frozen execution clock.
Proxy settlement consumes execution into `SettledAttempt`; evaluation consumes
that into `EvaluatedAttempt`, retaining an independent `EvaluationOutcome`.
Explicit cleanup then yields `FinalizedAttempt`. Existing adapter cancellation
on aborted transitions preserves unfinished resources rather than calling
destructive normal-close from Drop. Provider bridge ownership remains outside
these transitions until proxy settlement and evaluation have finished.

### Layered replay

Replay has independent protocol, tool, and native-execution layers.  The LLM
layer is an ordered, provider-neutral loopback proxy chain; agents receive only
its ingress endpoint.  The MCP layer uses the same adapter-owned session
lifecycle, but preserves the agent's normal MCP configuration and transport.
The native layer observes process lifecycle through ASB's existing exclusive
gate.  It never uses `LD_PRELOAD`: injected libraries are incomplete for
statically linked binaries, alternate runtimes, and direct kernel APIs, and
would become part of the measured program.

Each later native-replay increment must keep these boundaries distinct:

- A compact, append-only index records opaque trace-local process identities,
  causal relationships, content digests, sizes, and relative timing only.
  It never serializes host PIDs, command lines, paths, prompts, credentials, or
  unbounded stdout/stderr.  Provider-only recordings remain compatible when
  they have no native-trace artifact.  When a selected recording does contain
  one, ASB validates its bounded schema immediately before staging that replay
  batch; malformed trace evidence stops replay before the agent is launched.
- A private execution capsule supplies replayable filesystem and executable
  state.  It is a transparent runtime mechanism, not an agent security policy:
  the agent still uses ordinary paths, binaries, subprocesses, and direct APIs.
  Replay reconstitutes only recorded inputs and declared workspace effects.
- The first lifecycle observer remains deliberately lightweight: it records the
  root and causal forks, then releases arbitrary helper descendants rather than
  holding a long-lived syscall tracer.  A capsule backend may add detailed
  executable and filesystem facts only when it can prove complete cleanup and
  preserve normal child stdio/MCP shutdown behavior.
- Record/replay must remain measurement-friendly.  Time and resource
  collectors attach to the actual agent process tree; replay metadata is
  bounded, content-addressed, and stored once.  No sampling loop, full syscall
  log, kernel-wide trace, or duplicate process-output capture is permitted.

This progression permits independently qualified changes: proxy/session
normalization, compact capsule contracts, filesystem/executable capture,
offline native replay, then an end-to-end agent qualification.  A replay
request fails closed when the selected adapter lacks the completed layer; it
never silently falls back to a live provider or host tool.

After explicit cleanup, private `RunDisposition::from_facts` derives one
operational status, primary `RunFailure` and success decision from independent
execution, evaluation, cleanup, collector and presentation facts. `RunStatus`
and `FailureKind` serialize the existing log/measurement strings; failure keys
remain nullable and the public JSONL summary gains no status field. A pending
evaluation remains pending even when an operational failure makes the run
status failed. Numeric rewards, nullable resolution and Boolean grades do not
change. Legacy measurement `success` remains grade-derived, not an operational
success flag. Reports disclose actual recorded failures independently of that
legacy flag, and otherwise preserve healthy pending/native outcomes. Historical
status/failure strings remain display data, not strict enum-deserialization
boundaries; workload evaluation kinds remain adapter-extensible and unknown
historical grader categories remain unscored. Recording commit failure updates
the typed operational disposition before log/measurement publication, preserving
any earlier primary failure and all independent grades/rewards. Post-publication errors retain
their separate ancillary policy rather than rewriting an immutable disposition.

Post-execution lifecycle failures are ancillary `AttemptIssues`, not missing
submissions or replacement grades. Dispatch attempts every collector hook and
execution-scoped shutdown even after a hook fails. Before execution/resume,
required instrumentation errors still fail closed. After output exists, official
evaluation and explicit cleanup run despite client/proxy/collector failures.
Human presentation and guest-owner transport have distinct failure policies:
broken presentation stops writes while already-admitted work completes; losing
the owner transport still cancels/preserves. A final measurement-publication
error leaves the immutable task outcome available and prevents a completion
receipt, so an incomplete measurement cannot silently authorize continuation.
Run-log publication is a separate typed obligation: a failed log write never
skips final measurement publication or hides the grade from terminal rendering.
Both log and measurement publication must be confirmed before a completion
record/observer receipt is permitted. Original run failures precede ancillary
log/measurement diagnostics; repairing reporting does not repeat the agent.

Post-grade evidence publication prepares serialized JSON before touching a
published file. Each destination has a persistent-inode lock (shared lock
mechanics, five-second contention observation) held across its opened regular
snapshot, private sibling staging, atomic per-file rename and parent-directory
synchronization on Unix. Windows instead synchronizes the prepared writable
file and requests `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`; it never
requests a cross-volume copy, delayed move or weaker rename retry. Other
platforms retain file synchronization and their standard rename operation.
No non-Unix policy attempts an ordinary `File::open(directory)` or claims
portable directory/crash durability. The Windows route is source-qualified
against pinned Rust/Windows API declarations, not natively compiled or executed
by the Linux qualification. See Microsoft's [MoveFileExW contract](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw),
[FlushFileBuffers access contract](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers)
and [directory-opening requirements](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew).
Symlinks, pipes and other nonregular destinations are rejected.
Unix snapshots additionally bind device/inode identity. Other platforms retain
conservative creation/last-write/size/permission consistency checks instead of
rejecting all existing files; Linux fixtures do not qualify Windows runtime.
Log history is streamed without truncating original bytes, validating complete
JSONL records under the existing 16 MiB per-record consumer bound. A historical
malformed record is preserved and reported, not joined to the next attempt.
Summary read/modify/write holds the same lock and preserves unknown metadata;
malformed historical JSON or runs never silently becomes empty history.
Prepared encoding stops at the consumer's 16 MiB bound, including the newline
for log records, before publishing oversized output.
Pre-commit errors retain old published bytes; post-rename confirmation errors
explicitly say that evidence was published but durability is unconfirmed. They
do not roll back or repeat execution.
If native rename reports failure and its private regular source is missing or
uninspectable, the commit outcome is instead unknown: destination and exact
staging are retained without retry, rollback or a claim that publication did
or did not happen. A remaining regular private source classifies a precommit
failure under the quiet same-filesystem staging contract, not an adversarial
filesystem transaction guarantee. This classification is separate from a
known successful rename followed by a failed confirmation.
Post-commit scratch retention after successful synchronization has its own
published-and-synchronized diagnostic and retained path; it does not pretend
that a known successful sync was uncertain. Explicit cleanup refusal retains
that stage rather than silently retrying in Drop. Independent measurement outputs
and adapter finish hooks are all attempted; one publication failure cannot skip the
others. Explicit finish suppresses silent lifecycle-publication retries in Drop.
This is per-file cooperating-writer ownership, not a multi-file transaction,
hostile filesystem confinement or arbitrary crash-recovery protocol.

Stopped collectors pin opened regular stdout/stderr captures before processing.
Their admitted handles precede producer startup. Unix readers use `read_at`;
Windows `seek_read` updates a cursor, so Windows first uses
[`ReOpenFile`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-reopenfile)
on the admitted object to obtain a separate read-only description, never a
pathname refresh or shared-offset `try_clone`. Its sharing preserves read,
write and delete access; inheritability is cleared before any producer starts.
Full volume/128-bit file IDs are compared when both queries are available.
Unavailable file-ID queries do not fabricate proof or change the native
object-based reopen into a pathname lookup. Both stopped readers and startup
diagnostics use that separate read description, preserving the writer cursor.
This Windows implementation remains source-qualified only, without native
target compilation/runtime evidence; Unix behavior and conservative non-Unix
raw deletion policy are unchanged.
Result publication failure or durability uncertainty skips raw consumption and
reports the source paths, without skipping sibling exports or final run records.
Only confirmed collector-result publication permits raw cleanup. Unix device/
inode binding and a second check after atomic movement into private custody
prevent deleting a replacement between a pathname check and unlink. A moved
foreign regular entry is restored without clobbering a new arrival, or retained
with an exact inspection path when restore is unsafe. Cleanup failures retain
their source/staging diagnostics and cannot erase the task grade. Platforms
without native capture identity retain scratch conservatively; no Windows
cleanup runtime qualification is claimed. Unterminated historical log records
must also leave room for their required newline inside the consumer window.

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
Known Rust producer categories use the internal `KnownEvaluationKind` domain
across adapters, sessions, planning catalogs and runner facts. Its explicit
`as_str()` projection retains the existing wire spellings; it is not a closed
serde enum for persisted records. Saved readers classify borrowed strings as
known or extension without changing unknown bytes. Eligibility and baseline
downgrade share that contract, while baseline support remains distinct from
eligibility: measured-false evidence still supports an extension at the low-level
predicate, but an unknown category cannot earn binary resolution credit. Native
reward/null-resolution policy and arbitrary recorded protocol IDs are unchanged.
An adapter can also expose a read-only `PlanningCatalog`: dataset identity,
available task IDs and evaluator scope, without preparing an environment.
Verified preparation additionally has an internal opt-in material-ready gate:
native file sessions fingerprint adapter-owned initial files before baseline
evaluation and reject evaluator mutation. Workers retain their sessions locally;
only immutable identities cross the cancellable rendezvous before proxy/agent
startup. Early failure and timeout release peers, and explicit cancellation
retains official workload preservation semantics. Legacy runs do not acquire
these scans. The native `run --repeat` prototype publishes an intended-slot plan,
allocates fresh campaigns and batches, freezes private legacy replay inputs and
links every completed outcome. Batches continue only after confirmed preparation,
complete output publication and cleanup. Measurement overrides relocate only
measurements, not project-local workspaces/logs. Collector preflight and sessions
receive the benchmark's project root explicitly: an external measurement path,
even beneath a different project, never selects its tracing capabilities or
service registry. Official sessions now expose an
on-demand versioned material identity through the workload controller, separate
from evaluator baseline evidence. The shared capture verifies declared resource
ownership and binds actual prepared file content plus image/runtime definition.
The controller stages the running ASB helper into an absent owned root, verifies
its exact bounded TAR tree before reuse/execution, and composes two complete
image-anchored root observations with change/deletion inventories plus
full-content shared memory. The complete composer covers declared mounts,
services and supported owned/default-bridge runtime/network topology;
unsupported coverage has no narrow fallback in verified CLI execution. Owned
local bind sources additionally receive a joint multi-root census for cross-service
regular-file and symlink hardlink relationships. `PreparedSampler` exposes bind
views only after a successful guest census matched exact source/root witnesses.
Every new census or failed callback invalidates that authorization. A complete
initial guest round precedes host content reads and must agree with both later
rounds. Joint host observations bracket each later whole guest round and recheck
service authority, source chains and locality. Identical source paths share a
logical root, but only subtrees hidden in every view are excluded; each binding's
nested/semantic exclusions remain explicit. Host paths and numeric inode
witnesses are private, never public identities. Host and guest reads share one
bounded allowance without resets. This adds relevant owned local filesystem
observations, not a portable host snapshot or provenance guarantee.
The TAR reader never extracts files; legacy runs never
request the capture. Broader
official material coverage and real SDK qualification, bundled recording/replay
and campaign-aware comparison remain integration work; see
[the execution design](MULTITRACK-EXECUTION-DESIGN.md).
Sweep planning caches each read-only planning catalog once per installed workload and saves a bounded
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
the actual source cassette directories for legacy sweeps, including rebased/
preserved history paths. Campaign sweeps instead bind the exact referenced
plans, original configuration sources and staged private selected JSONL files,
after all publications and before execution. Their input seals exclude original
replay archives and growing recording/output directories. The same validated
index-to-campaign binding serves reporting and sealing; stability is not proof
of selected-payload identity or permission to resume. Record cassettes are
outputs, not replay inputs. Interpreter and native
loader dependencies are added by shared runtime discovery, not inferred merely
from an executable's pathname.

Campaign continuation consumes the original input owner and a whole-population
classification under every campaign lock. Only positively untouched slots enter
a new dispatch; completed false grades remain reused failures. All configuration
manifests are published and rechecked before one final admission marker returns
consuming worker capabilities. The parent `ActiveMode::Continued` retains its
input/campaign guard through scoped worker joins, strict receipt sealing,
read-only input verification and comparison. Passive generation wire/resolution
lives with the runner's campaign model, not in a runner-to-sweep dependency.
Fresh and continued runs share `BatchRun`; continued material/finished metadata
is generation-scoped while original slot starts/outcomes remain create-new.
Replay routes saved private JSONL and frozen choices without reopening sources.
Inherited receipt bytes are verified before new receipts publish; original seals
are not rewritten. Recording continuation still needs the separate immutable
union/recovery contract in MULTITRACK-RECORDING-CONTINUATION.md.

On Unix, runtime discovery owns one nonblocking duplex exchange, with a 1 MiB
serialized request and 4 MiB complete response, a single 60-second exchange
deadline and bounded cleanup. Linux launches the exact typed command through
the invoking ASB's native gate, retaining detached helper descendants until
quiescence; no raw process-group signal or blocking pipe-thread join is used.
Other Unix hosts retain only direct-child custody. Nested POSIX Python/loader
probes enforce 1 MiB stdout and 64 KiB stderr limits during reads and use the
remaining discovery budget. One pre-established Python direct-child keeper
per helper retains uncertain cleanup and refuses further probe admission.
Outside the enclosing Linux discovery gate, that keeper does not establish
descendant or crash-persistent ownership. Non-Unix Rust exchange and Python
probe fallbacks preserve their prior healthy behavior but retain their legacy
blocking/capture limitations; they are not qualified by Linux tests.
After workload cleanup and measurement completion, the runner writes private
`.asb-completions/` outcome records beside the run log, including unresolved
outcomes, actual task identity, cleanup status and workload-owned evaluator
evidence paths. These records omit agent stdout/stderr. Resource settlement
remains independent of its grade: removed,
not-created, not-required, or positively stopped-preserved resources may admit
later fresh batches. Unknown settlement stops admission. Preserved resources
retain their artifact locations and never authorize an implicit verifier retry.
New outcome, log and measurement records agree on disposition and artifact
references. Legacy handling requires both outcome disposition fields absent;
explicit null denotes unknown settlement, not a legacy omission. When one replica
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
Group sealing, replica continuation and reconciliation consume one private
validated outcome representation. Its constructor checks named protocol/cleanup,
cohort, selection/configuration identity, grade/baseline and evidence-path
invariants before exposing task summaries or evidence roots. Unknown process
or grade observations remain invalid for ordinary execution reuse; false
observations are retained as completed outcomes.
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
Native runner custody is represented by `ManagedGate`, not a raw `Child`
whose exit could be mistaken for descendant quiescence. Before spawn,
`Admission` fallibly establishes a wait-only fallback owner and a private,
nonblocking, versioned socket. The gate emits its PID-bound `Hello` only after
the owner watcher exists; the runner validates both that receipt and stopped
readiness before admitting execution with CONT. A stale launcher without this
ABI is never resumed: startup cleanup uses KILL only, without TERM that could
indirectly wake an old launcher's watcher. The diagnostic asks the user to
rebuild/use the current ASB runtime; this is compatibility, not authorization.
The socket is immediately CLOEXEC in the gate and explicitly closed in its
forked wake-only child. The client cannot inherit the gate's proof writer.
Normal completion requires both direct-child exit and a matching, ordered
`Quiescent` frame, emitted only after owned descendants are stopped and reaped;
EOF, principal exit and a helper result are not equivalent proofs. Receipt
reads and writes are bounded and do not wait for EOF from duplicate writers.
If a teardown sweep cannot establish quiescence, the gate retains supervision
and retries rather than exiting and orphaning its tree. Caller cancellation
waits at most six seconds before reporting explicit uncertainty and handing
the still-owned gate to the precreated wait-only reaper. That reaper never
kills an admitted guardian. Parent exit still wakes the independently owned
gate through its existing pinned watcher. This is not a promise that an
unkillable kernel task disappears within the caller deadline, nor a repair of
the separate pre-exec allocation and output-reader resource-limit classes.
The staged launch-descriptor repair separates an adapter's `ProcessLaunch`
(raw OS executable, argument prefix and explicit client argv[0]) from its actual
native `Command`. That command is constructed as the gate before any provider,
adapter, environment, working-directory or I/O configuration and is never
rebuilt from getters. Rust therefore applies the real `env_clear`, removals,
overrides, stdio, child credentials and custom process settings. The remaining
ASB pre-exec callback only changes the private receipt descriptor's CLOEXEC
flag using `fcntl` and captures an OS error; it allocates no argument/environment
buffers and never reads the environment after fork. A typed target view keeps
collectors from treating the gate executable and private framing as the agent.
The same adapter descriptor constructs direct commands for guest execution and
runtime input discovery. Client argv[0] belongs in the descriptor, not in the
gate command's argv[0]. Private `ASBGATE2` admission refuses the previous
`ASBGATE1` runtime before CONT because it cannot interpret the new target frame.
The actual native child retains PATH lookup under its final environment. A
collector cannot infer inheritance versus `env_clear` from `Command` getters;
bare targets without a provable explicit PATH and unsupported byte-level
identities remain unavailable instead of being guessed from host PATH.
When lifecycle tracing is enabled, the gate records a compact, opaque identity
for a directly named regular-file root executable only: its SHA-256 digest and
byte count, capped at 8 MiB. It records neither paths, arguments, executable
bytes, output nor host PIDs. A missing, bare-PATH, changing, non-regular or
oversized executable simply has no identity; it never blocks a live run. This
is provenance for the later native replay capsule, not executable capture. The
gate remains lifecycle-only: ASB does not use `LD_PRELOAD`, per-syscall ptrace,
kernel-wide tracing or a sampling loop for this evidence.
Before establishing cleanup custody, the private route verifies the original
gate executable, exact OS-string target framing and initial adapter arguments.
A changed carrier, replaced prefix or truncation fails without spawning a child
or creating a reaper. Admission
accepts only that validated gated-route token and holds its exclusive command
borrow through spawn, so there is no mutable configuration gap after checking.
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
External cli2key endpoints own no local processes. A local provider session
retains its startup guard and pre-established fallible `OwnedReaper` through
both bridge and compatibility-hop lifetimes. Before each child starts, a
fallible output worker owns a nonblocking Unix socket transport. Readiness is
limited to five seconds, 64 KiB, 64 messages and 4 KiB per line (including
output without a newline). Only the key/readiness result enters a single-slot
channel; subsequent output is continuously drained and discarded. No startup
output or key is included in diagnostics. Non-Unix local startup fails before
spawning and can still use an explicitly configured external endpoint.
Admission rechecks every owned child after compatibility-hop readiness, so a
bridge that exited after its initial key cannot admit a ready local session.
This observes the boundary, not future process liveness.
Failure, unwinding and session destruction share one five-second cleanup deadline
across both children and readers. Closing the embedded hop's stdin lifetime
pipe gives it at most two seconds within that budget to close its listener,
cancel outbound work and observe its own curl subprocesses' close events.
The hop uses the network client's existing bounded TERM/KILL escalation for
those declared children. Failed cooperative acknowledgement is diagnostic;
remaining direct children receive KILL. Unconfirmed children transfer to the existing reaper;
cooperatively stopped readers retain their own transports until exit. Only
already-finished threads are joined; after the shared deadline an unfinished
handle is released and the nonblocking worker exits when scheduled to observe
its stop flag. Worker-exit receipts test that behavior separately from caller
waiting. Startup errors retain their original diagnostic and cleanup uncertainty.
The generic `ProviderSession::finish` contract explicitly settles model catalog
sessions after fetch/parse, and benchmark sessions after grading/workload cleanup
but before result publication. A finish error preserves the original catalog,
client or official grading failure and the independent grade, reports ancillary
cleanup uncertainty and prevents a completion receipt. Early preparation errors
also attempt finish exactly once; silent Drop cleanup is the unwind fallback.
This bounds caller waiting, not actual kernel-stuck exit, descendant supervision
or crash-persistent recovery.
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
The generic catalog transport polls all three Unix pipes concurrently rather
than blocking on request writes or stream-reader joins. Its absolute budget is
35 seconds, with I/O ending by 30 seconds and cleanup consuming only the reserved
remainder. The 16 MiB body and 8 KiB diagnostic-tail limits apply independently
of curl's own limits. A privately owned direct child is stopped/reaped on errors;
uncertain exit transfers to pre-established reaper custody and cannot produce a
successful catalog. Inherited descendant pipe handles do not prolong the caller.
This is not descendant supervision or a guarantee about uninterruptible kernel
operations. Non-Unix discovery explicitly fails before spawn until a cancellable
transport is implemented; no blocking compatibility fallback remains.
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
machine-readable output. Mini-SWE's trajectory and Hermes's usage ledger now
belong to fresh private per-attempt namespaces outside the task workspace.
The adapters declare and configure their own artifact paths, including the
guest's explicit mounted run directory; generic execution has no agent-ID
branch for file answers. After execution, each bounded regular JSON file is
opened without following symlinks and frozen once. Answer, usage and steps use
that same document rather than independently refreshing a workspace pathname.
Existing workspace trajectories are neither interpreted nor deleted.
The namespace handle stays pinned from preparation. Unix `openat` and Windows
rooted `NtCreateFile` resolve the child relative to that handle, not a second
full-path traversal after checking the namespace. Explicit consuming scratch
settlement checks file identity and frozen size/time observations. On Unix it
atomically moves the source entry into a create-new, owner-only custody directory
before rechecking and unlinking the custody entry; a replacement in the source
check-to-move window is retained there, never deleted. Windows disposition acts
on the checked open file handle. Replaced namespace paths and foreign source
entries are never path-deleted. Empty private namespaces/custody directories can
remain intentionally. This is normal concurrency protection, not an atomic
conditional-unlink sandbox against a hostile same-UID actor invading the private
custody directory itself. Fixed per-artifact reasons identify only declared
namespace/filename and failure class, never file contents or raw OS/JSON errors.
Malformed/oversized/unreadable snapshots are retained. Before freezing, unwind
has a best-effort guard; after freezing, unwind retains evidence for inspection.
Explicit failed settlement is never retried by Drop, which never prints.
Before native scratch settlement, each valid frozen snapshot is transferred to
`<private-runtime>/runs/<attempt>/mini-swe/frozen-trajectory.json` (or the
declaring adapter's equivalent `frozen-<filename>`). This writes the exact
already-validated bytes, never a second source-path read or JSON re-encoding.
Create-new, nofollow, pinned-relative creation refuses existing files/symlinks.
Unix files use mode 0600 inside the owned 0700 namespace and synchronize both
file and namespace; Windows retains its namespace ACL and uses rooted
write-through creation plus file flush (source qualification only, not a claim
of portable directory flushing). Failed creation/write/synchronization prevents
source consumption, records a fixed attempt-relative diagnostic and still allows
the independent publishers to run. Original deletion retains the existing
identity/version and custody checks. The durable evidence copy is intentionally
retained even after successful publication, not counted as unremoved scratch.
It can contain complete agent conversation/usage and sensitive data; do not
share it as a harmless diagnostic. Each raw snapshot/copy is at most 16 MiB,
with at most 16 declared artifacts (256 MiB raw-copy storage per attempt at the
API ceiling; currently one for each owning adapter). Frozen raw bytes also stay
in bounded memory alongside the parsed document until handoff. Copies accumulate
across attempts: there is no project-wide storage quota or automatic evidence
expiry. Disk-full errors retain the original and are not a reason to retry the
agent. These file/namespace flushes are not a multi-file publication transaction.
Log/measurement publishers still consume one immutable result exactly once;
their failures disclose locatable attempt-relative frozen evidence when available.
Scratch retention is ancillary run failure, not unconfirmed workload/container
quiescence: official grading and independent workload/provider cleanup and
publication still run once. Guest metadata carries a separate optional artifact
diagnostic while retaining the existing process/namespace ownership facts.
Guest publication first synchronizes a usable result marked scratch-settlement
pending, then consumes settlement and publishes its final ancillary status.
Failed first publication retains raw source. Failed final publication leaves
the honest pending result available for grading; it never asserts cleanup.
These two publications are not a multi-file transaction. A retained-service
workload still requires the existing durable result/owner/READY authorization;
missing first publication does not fabricate that authority.
Unix file identity is qualified with offline fixtures. Windows retains a
handle-based volume/128-bit-file-ID implementation with reparse-point refusal,
root-relative access and handle-based disposition;
its source is reviewed but Linux execution does not qualify Windows runtime.
An identity-unavailable filesystem/platform still runs and freezes isolated
evidence, but refuses destructive settlement and reports retained scratch.
Their terminal output is intentionally limited to success
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

Codex's adapter requests the pinned client's final-message file in a fresh,
bounded private artifact namespace. It does not use intermediate JSONL
messages as a final answer. The frozen artifact crosses isolated guest
execution through the same deferred adapter-artifact handoff used for other
private state, and is available to workload grading after collection.

Agent/tool launch and installer/TUI readiness share one project executable
policy. An explicit `<tools>/<id>/bin/<executable>` source override wins over
adapter-owned package preferences, then the npm/venv and configured shared-bin
layouts are considered. Codex owns its native npm-package preference in its
agent adapter; the generic resolver does not branch on agent identities. A
present but broken, non-executable or non-file higher-priority candidate stops
resolution instead of silently executing stale fallback code. Symlinks must
resolve to executable regular files. These checks do not eliminate changes
between preparation and execution. Relative/custom tool directories and legacy
shared-bin candidates are rooted in the selected project, never an unrelated
working directory. `ASB_TOOL_DIR` remains an execution-only override: a missing
candidate falls back to the project, a present unusable candidate stops launch,
and neither case changes project-local installed status. Only explicitly
host-backed tool recipes retain their separate host-availability fallback;
host agents and external overrides are not project installations.

Installer recipes prepare typed local-path/URL and project/runtime inputs before
dispatch. Ordinary Host and pinned Goose binaries share bounded staged-file
publication, while automatic dependencies retain no-clobber semantics and
package managers keep their existing installation authority. A probe-visible
file is identity/content/mode/bounds checked, detached into private custody,
then copied from its admitted handle into an unexposed commit stage. Cleanup
also detaches exposed entries before deleting only positively owned files or
empty directories; foreign arrivals and failed explicit settlements remain
reported evidence, without a Drop retry. This is ordinary private quiet custody,
not a hostile same-UID sandbox or package-manager rollback. Details and current
qualification limits are in [the recipe repair contract](INSTALLER_RECIPE_REPAIR.md).

All three TUI installation flows share one owned process operation. A private
versioned helper runs synchronous tool, workload or collector-dependency
installation under the same `ProcessLaunch`/`ManagedGate` native supervisor as
benchmark execution. Blocking package managers and their double-forked helpers
remain owned descendants; no detached installer thread outlives the wizard.
The invoking runtime's existing executable-identity policy also selects this
recursive helper, retaining the Linux running image across pathname replacement.
Linux native custody is required before starting TUI installation; unsupported
hosts fail explicitly rather than using an unsupervised fallback.

An inherited private nonblocking Unix datagram channel carries eight-byte
versioned progress/final frames. Identifiers are bounded to 128 entries of
128 bytes. Progress contains a percentage and a bounded semantic `ProgressStage`
selected at the actual owning installer phase. Its UI label is static; raw stage
strings, installer output, provider credentials and arbitrary errors never cross
this channel. Full queues drop transient progress; the parent coalesces each
bounded polling batch into its latest phase/percentage. Unknown phases, reserved
bits and a progress stage on a final frame are rejected. Final status
has a one-second send budget, with fixed input/I/O/execution/request failure
categories and a project install-log hint. Original failures are appended to
the existing project install log when possible, using its existing append and
permissions policy. That log can contain sensitive details and should not be
shared without inspection; the fixed protocol does not make the log private.
The helper marks its protocol descriptor
CLOEXEC before invoking installers. EOF and helper success alone cannot grant
success: the parent also requires matching successful gate exit and its
descendant-quiescence receipt, then each selection flow refreshes readiness.
Collector dependencies are rechecked with the same generic project-executable
predicate after that refresh; a zero installer exit cannot select a collector
whose dependency remains missing or non-executable. Phase-begin callbacks precede
long source/build commands, including each bpftrace download/extract/configure/
build step. Percentages at those boundaries describe previously completed work.

Drawing/input errors, Esc and Ctrl-C explicitly stop the owned gate and retain
both the primary error and any unconfirmed-cleanup diagnostic. Unwinding uses
the same bounded stop and pre-established wait-only keeper; no Drop worker or
unbounded join is introduced. Best-effort Drop diagnostics do not panic if
stderr fails, allowing the terminal guard to restore mode during unwinding.
The existing gate watcher retains supervision
after abrupt owner loss. Cleanup uncertainty is a failure, not proof that a
kernel-stuck task stopped within the caller's six-second observation bound.
Completed installation writes are not rolled back. Selection changes still
require success/readiness, and cancellation publishes no setup or trial-set
selection. A terminal guard attempts raw-mode, keyboard, alternate-screen and
cursor restoration independently on normal/error/unwind and partial startup;
one failed output operation cannot suppress raw-mode restoration.

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
The private task protocol uses typed request/event identifiers and validated
ready/completed views above the generic JSON-lines transport. Ready validation
binds the selected dataset instance, baseline, public input and effective task
budget; completion validates guest metadata, output encoding and quiescence or
the declared namespace-owned service handoff. Measured attachment and end-boundary
proofs remain in the measured phase; unmeasured completion gains no invented
collector requirement. Untagged one-shot retained-inspection responses remain
supported by the generic transport.
Workload-owned launch transports sit beneath the shared Docker workspace
gate/deadline/output state machine. The default transport uses the Docker exec
identity; a different launcher supplies its actual stopped client identity and
child status without treating a bridge's PID or exit code as the agent's.
Namespace-only targets cannot satisfy host collector verification. A retained
workload keeps the entire iterable transport until namespace teardown, not only
its output stream. Closure is idempotent but sticky failures, including
cancellation exceptions in a drain worker, prevent false cleanup confirmation.
The SDK-shell launch strategy is still pending; this abstraction alone does not
change Terminal's current direct-exec client launch semantics.
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
the interrupted operation as failed. Positive preservation can admit later
fresh attempts, but is not successful normal removal or a completed grade.
Normal-close preservation retains an explicit teardown failure diagnostic.
A preserved
environment's filesystem is retained, not its running process/shell state.
Official sessions also track whether their latest evaluator response actually
completed a matching task grade. Errors, missing/contradictory resolved flags
and mismatched identities cannot take the normal close/removal path. Session
finish cancels into stopped preservation and reports the artifact location
instead. Valid unresolved and empty-submission outcomes are completed grades,
not permission to repeat the client. A later evaluator failure invalidates
the previous completion permission rather than allowing old evidence to delete
new or ungraded work.
That permission is a private matching-completed-grade token, cleared before
each evaluator request and consumed by normal close, rather than a writable
Boolean. True and false official binary grades can construct it; evaluator
errors, pending/unknown outcomes and mismatched identities cannot. Distinct
normal-close and cancellation acknowledgement validators retain their different
cleanup and child-exit requirements. Cancellation deliberately drains in-flight
and unknown events without printing their payloads. The wire spellings, adapter
evaluation-kind extension strings and structured/native outcome APIs do not
change. Existing official Python task sessions still supply binary grades;
this refactoring does not add native-reward transport to those SDK sessions.
On Unix, controller stdin/stdout retain private standard process pipes with
nonblocking parent ends and blocking child ends. One caller-owned duplex pump
keeps an eight-event queue and a 1 MiB frame bound (one oversize probe byte);
there is no detached
reader or blocking reader join. Requests start one absolute deadline before
encoding/writing, and the same deadline covers the response. One-shot inspection
also constrains error cleanup and child-exit observation to its deadline. Task
sessions retain their separate preserving-cancellation budget after an execution
or evaluator failure. Agent dispatch and
resume writes consume the existing execution-protocol deadline rather than
starting another clock. A partial failed write closes stdin: it cannot be retried
as a new request or interpreted as normal close.
Normal close, cancellation and final exit observation share the earliest
cleanup deadline, so a failed close does not restart another cleanup budget.
Only the existing validated acknowledgements can confirm resource cleanup.
Unix destruction closes its owned pipes, gives EOF up to three seconds, then
signals only the still-owned direct controller with TERM inside a total
fifteen-second fallback budget. An existing shorter cleanup deadline takes
precedence. Deadline/poll uncertainty sends KILL to that direct child and
transfers custody to an `OwnedReaper` established fallibly before spawn; caller
cleanup never performs an unconditional child wait or creates a Drop worker.
This is bounded caller observation, not proof that a kernel-stuck controller
has exited or that its Docker resources were preserved. Resource recovery still
requires the adapter's journal/ownership checks. The fallback is neither
descendant supervision nor crash-persistent recovery.
Python task-loop waits use actual monotonic deadlines, including checking a
late dequeued request before it can authorize close; idle, collector attachment,
queue publication and execution/evaluation defaults retain purpose-specific
names and existing values. The non-Unix controller intentionally retains its
legacy blocking pipes, reader and Drop behavior for compatibility. It is not
qualified by the Unix tests and does not gain this bounded-I/O/custody guarantee;
cancellable non-Unix transport remains unresolved portability work. Process
spawn, operating-system scheduling and arbitrary SDK operations are not given
a new hard real-time guarantee. Shared preparation retains its independent
remaining-time allowance: dispatch writes cannot consume the later execution
budget. Initial-material request writing and response reading share one deadline.
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
Schema 2 additionally permits one optional sealed native sidecar per track at
`native/<track-id>.jsonl`. Its catalogue records only completion, event count,
byte length and digest; the cassette layer treats bytes as opaque and rejects
orphaned, duplicate, incomplete, changed or non-canonically framed sidecars.
The journal stages that sidecar before the track's immutable commit, so a future
native transport cannot consume an unsealed post-commit attachment. Schema 1
recordings remain readable when they contain no native sidecar. Native ingress,
event semantics and replay dispatch remain outside this storage contract.
`NativeToolTransport` is the next private boundary: it consumes only a complete
sealed sidecar and requires the exact bounded lifecycle `launch`, session alias,
`poll`, `write`, `output`, `exit`, then one terminal workspace-effect digest.
The sealed private stream retains bounded exact request/output bytes solely for
replay, alongside aliases, terminal status and digests. Command arguments,
paths, prompts and every native payload remain absent from manifests, public
evidence and reports. No agent is wired to this transport yet.
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
writer-owned batch evidence verification reuses one archive index. Read-only
report verification instead validates all reference and journal/catalogue
metadata before selected JSONL, then uses a second bounded catalogue pass with
the same receipt and metadata-size identity. Only one archive stays open, avoiding
a descriptor-count cap on valid sweeps. Each receipt length check and decoder
share one opened regular file. Whole-operation `ProofLimits` bound references,
metadata, directory/end-record snapshots, compressed input and distinct selected
bytes separately from per-archive format limits; single proofs retain their
independent journal and manifest allowances. Capacity planning uses the same
private compatibility/manifest/reference wire
projections, with a separate size-only digest type that cannot enter observed
compatibility. Metadata reservations bind real read-only selections and are
recomputed rather than trusted on load. Fresh sweeps sum them before publication
or dependency warming; genuine material admission checks selected names and
capacity before journal creation. Response sizes remain runtime bounds, not
guessed reservations. Association receipts retain their separate runner-owned
128-byte-per-slot policy. Sweep and cassette output
owners share a private persistent-inode lock implementation, with domain-owned
paths and diagnostics. Finalization requires the caller to stop/join recording
producers; freezing the journal does not drain a proxy itself. This backend is a
delivery foundation integrated by the run-owned recording/replay coordinators.
After confirmed archive publication and observed producer cleanup, raw recording
consumption pins the producer namespace and moves a candidate into fresh private
custody before identity/digest revalidation. Only that private candidate can be
unlinked; the original source name and replacement namespaces are never deletion
targets. Foreign candidates are restored without replacement or retained with a
locatable diagnostic. Actual unlink followed by failed namespace synchronization
is reported as removed but durability-unconfirmed, not falsely retained. Empty
private custody directories remain; no recursive/Drop retry cleanup is implied.
Run recordings publish ZIP bundles from fresh executions; replay freezes a
selection plan before any client admission, selectively stages private JSONL,
and compares the source identity with adapter-observed initial material at the
shared preparation gate. Per-batch aliases project the selected track IDs while
each proxy owns independent progress. Provider adapters and proxy protocols do
not parse archives or choose tracks. Legacy no-selector JSONL replay retains
its previous workspace behavior; explicit repeats/track selections are fresh.
Sweep bundle/resume/report integration and the offline viewer remain separate
delivery steps. See
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
proxy knowledge and tear every hop down after the child exits. The recorder
writes internal JSONL captures with a bounded `delay_ms` response latency for
every interaction; replay consumes selected JSONL and never starts cli2key or a
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

Private consuming sweep phases enforce that ordering: `PreparedSweep` owns the
output lock and validated plan/count/resource admission. Fresh preparation runs
workload preflight before preserving old output; resumed preparation requires
validated inputs and continuation state rather than an optional checkpoint.
Activation creates/binds the result layout. After that boundary, presentation
failures produce a zero/partial `SettledSweep`, not an early return bypassing
input settlement and comparison. Only settled, joined execution can finalize;
the output lock remains owned through final comparison publication. Optional
fresh input seals still cannot block a fresh grade or authorize reuse.

A sticky presentation writer observes the first permanent write/flush failure
and performs no further writes to that sink. Interrupted I/O remains transient;
a nonempty zero-byte write is a real failure. Sequential JSON output failure
stops before the next configuration, while an already-admitted replica group
finishes grading and cleanup. Parallel configuration reports carry independent
spool-failure and spool-creation observations. The scheduler stops its queue in
the worker before publishing a locally observed spool failure, while already
admitted workers finish and release count/resource permits. Ordinary benchmark
errors do not select that stop policy. Post-join cleanup removes only spools
created by this invocation, never an unadmitted interrupted-resume spool or a
file whose preservation/creation failed. Failed terminals suppress comparison
summary bytes through `io::sink`, not artifact generation. Original grades and
benchmark errors remain primary; reporting/input diagnostics stay ancillary.
This does not make output preservation or multi-receipt publication transactional.
Saved execution mode is a lowercase enum with the same live/record/replay/unknown
wire values, missing historical mode defaults to unknown, and timing remains
replay-only. Report projections and serialized fingerprint inputs are unchanged.

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

Setup bounds concrete source-string storage and collection cardinality before
pretty-TOML serialization, then enforces the exact encoded configuration,
aggregate and manifest limits before creating output. Scalar files use synced
atomic replacement while preserving an existing regular file's mode; sweep
manifests use synced atomic no-replace linking only after all configurations
and their generation directory have been synchronized. Failed generations are
retained with their exact paths, never recursively cleaned. Small publication
stages retain live identity owners and detach exposed entries into private quiet
custody before commit/settlement; foreign arrivals remain recoverable evidence.
Known committed-but-unconfirmed durability/presentation errors explicitly report
publication; ambiguous actual commit errors instead retain evidence and require
destination inspection without retry/rollback or a false nonpublication claim.
Generated absolute runtime paths which cannot be represented in UTF-8 are
refused before any setup mutation, never lossy-converted. These are not
cross-file transactions or hostile same-UID sandbox
guarantees. See [setup publication](SETUP_PUBLICATION_REPAIR.md) for platform,
serializer and focused-qualification limits.

The workload trial wizard reduces explicit browsing, editing and saved states.
An editing state owns its workload and trial pane; saving replaces that
workload's in-memory subset. In the saved state, Enter finishes and publishes,
while Tab reopens the saved choices. Back discards unsaved edits. Browsing or
deselecting a workload retains its saved subset for later reselection, but
publication includes only selected workloads and requires each to have a saved
subset. Saving and cancellation publish no trial-set files.
Finish checks every saved ID against the current catalogs and serializes all
selected configurations before writing any file. File I/O remains individually
fallible; this is not an atomic transaction across multiple trial-set files.

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
prove resume eligibility or historical integrity. If no private outcome was
published (including incomplete capture), comparison can use the most recent
bounded schema-2 record from the sweep's fixed isolated replica log. It strips
transcripts, validates configuration/task/slot/policy bindings and rejects a
latest-log conflict with a recorded outcome or measurement before publication.
The original benchmark's configured log location cannot redirect this fallback.
Log evidence has unknown cleanup confirmation and is explicitly reporting-only:
it creates no outcome, completion/group receipt or resume permission. A failed
latest record cannot silently borrow an earlier grade. Explicit imported or
grade-only recovered outcomes keep their existing unknown-execution policy;
their interrupted partial logs remain diagnostic and are not promoted by this
fallback. The report links every result artifact and explains the
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
