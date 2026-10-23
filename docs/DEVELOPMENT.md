# Development

`benchmark_sdk_session.py` tests the private SDK-shell launch protocol without
Docker, providers or installed clients. The opt-in
`benchmark_live_sdk_session.py` instead needs the installed Terminal-Bench SDK
Python interpreter, `ASB_TEST_BENCHMARK_ASB` pointing to a frozen executable and
`ASB_TEST_SWECATALOG_ROOT` pointing to the owned benchmark project. It exercises
the original SDK shell with fake clients and the actual registered-adapter guest
driver: parent identity/user/cwd/env, nonzero/zero status and real child-versus-
bridge measurement target. `benchmark_live_lifetime.py` exercises the lazy
same-shell production hook with a fixture executable, retained Jupyter service
and original SDK grader, requiring the same parent before/after grading.
Neither is an installed coding-client, non-root or full repeated-batch
qualification. Successful owned
resources are removed; failures are stopped/preserved with private evidence.

`benchmark_live_sdk_agent.py` uses the same explicit SDK/project/frozen-binary
opt-ins with the REAL pinned Pi package and unpatched production bootstrap.
Prefetch the pinned Node archive and public npm cache before running: guest
installation deliberately sets npm offline and never falls back to a fake
client. An isolated `asb tool install pi --dir DIR` can populate the project's
public cache without replacing existing tools. Inference uses a synthetic
loopback provider with fixture-only credentials; the known upstream reference
solution still needs ordinary bridge egress for Debian/PyPI tool dependencies.
Its verbose installation output goes to a private guest log, returning only a
summary or a bounded failure tail. Neither production capture bounds nor the
original official grader is changed. Assertions require one actual tool call,
two provider calls, exact package identity, unchanged original SDK parent and a
resolved original grade. Failed attempts are retained, not silently retried or
promoted into successful qualification evidence.

`benchmark_live_repeated_original.py` exercises the public `asb run --repeat 2`
path at concurrency two. With `ASB_TEST_BENCHMARK_SUITE=terminal-bench-1.0`, its
default hello-world task qualifies normal completion and the explicit
`ASB_TEST_BENCHMARK_FAULT=block-verifier-once` continuation case. Select
`ASB_TEST_BENCHMARK_TASK=jupyter-notebook-server` for the original true same-shell
task with an installed Pi client, four official grades and a retained background
service. Only this qualification creates a dedicated owned egress bridge for the
reference solution's real Debian/PyPI dependencies; inference stays synthetic.
It reads the unchanged upstream solution, captures verbose installs privately,
checks per-attempt client context/log identity and compares baseline, admission,
planning and fingerprint evidence across both batches. Successful resources are
removed; failed resources are stopped/preserved, with the owned network retained
when still attached. It does not claim live LLM task solving or all SDK/client
context combinations. Use the same frozen-binary/project opt-ins and original
SDK Python interpreter as the single-client qualification above.

Real workload qualifications must use a separate frozen executable, not
`target/debug/asb`. Cargo package verification can replace the ordinary build
output even though the extracted packaged-crate tests use another target tree.
Build first, allocate a new project-local qualification directory, copy the
executable there, and pass that exact copy to `ASB_TEST_BENCHMARK_ASB`:

```sh
cargo build --offline --locked
mkdir -p target/qualification
asb_qualification_dir=$(mktemp -d "$PWD/target/qualification/frozen.XXXXXX")
install -m 755 target/debug/asb "$asb_qualification_dir/asb"
sha256sum "$asb_qualification_dir/asb"
```

Keep the copy unchanged and verify its hash after qualification. Ordinary
build/package gates may then run without replacing the selected executable.
Fresh per-campaign artifacts and actual running-process executable identities
remain required; a frozen copy is not evidence that a workload or grader passed.

The repeated-original qualification's normal test requires all four official
grades to pass. Its separate opt-in Terminal 1.0 fault case uses
`ASB_TEST_BENCHMARK_FAULT=block-verifier-once` and test method
`RepeatedOriginalQualification.test_confirmed_preserved_grader_error_continues_new_batches_without_retry`.
It modifies only the first fresh guest's `/tests` path, deliberately preventing
verifier injection. It requires four distinct attempts, continued fresh batches,
an overall failure and unknown grade with positively stopped-preserved artifacts.
The exclusive grading-start marker binds the original attempt; protocol unit
tests separately count the single evaluate request and cached cancellation.
Neither test retries a retained verifier or converts a missing grade into zero.

`benchmark_live_repeated_sweep_original.py` reuses the installed-Pi/original-SDK
qualification with the same explicit project/frozen-binary opt-ins. It runs one
configuration at C2/N2 through the public sweep, validates four actual report
execution/attempt identities and grades, then requires completed-only resume to
issue zero additional fixture inference requests and preserve all declared result
and verifier inventories, seals and receipts. Inventories include roots, file
content/modes, directory modes and symlink targets without following them, with
4096-entry/64 MiB bounds and propagated traversal errors. Both qualification
harnesses join the Python source-hash receipt. Default discovery includes only
this normal live qualification, not the inherited costly verifier-fault case.
Its inventory and emitted-row tests run provider-/Docker-free in ordinary
`benchmark_protocol` CI; the actual installed-SDK test is skipped without opt-in.
This qualifies completed-only reuse, not interrupted SDK-worker recovery.

The first local Terminal1 sweep attempt failed before client execution because
Docker's predefined address pools were fully subnetted. Both owned attempts
were stopped/preserved and the frozen binary hash stayed unchanged. The raw
qualification/Compose logs remain local evidence; this is not a passing original
SDK qualification. Do not prune unrelated networks or change daemon configuration
to turn the qualification green. The run also disclosed an unavailable initial
input seal, which must be resolved before claiming original-SDK resume readiness.

`benchmark_live_material_environment.py` uses the same explicit image/binary/
artifact-parent opt-ins as the mounted-root qualification below. It prepares
two network-none services per fixture, checks independent fresh generated
identities, shared-volume mutation and shared-versus-independent equal-content
storage, then removes successful owned fixtures only. Failed fixtures are
stopped/preserved. No agent, provider or evaluator runs. The separate ordinary
`benchmark_material_environment.py` suite tests whole-round ordering, one shared
budget, generated-file normalization, private resource replacement and refusal
of uncovered namespaces/networks/devices. These are not routed-network or
complete original-catalog qualifications.

The same environment qualification has explicit disjoint-bind and shared-bind
view tests. They use declared local source directories beneath each owned
attempt, distinguish shared regular/symlink inodes from equal independent
copies, and exercise overlapping tmpfs-hidden views. A source subtree hidden
from every guest must not affect identity; material visible through either view
must. Run the named tests with the same frozen executable and cached-image
opt-ins. Ordinary planner/root/environment tests additionally cover source
replacement, prior/failed guest validation, local-kernel refusal, initial round
mutation and shared-budget exhaustion before any host content read.

`benchmark_live_material_mounts.py` is an opt-in mounted-root qualification.
Set `ASB_TEST_MATERIAL_IMAGE` to an already cached Linux image ID,
`ASB_TEST_BENCHMARK_ASB` to the built binary and `ASB_TEST_SWECATALOG_ROOT` to
an existing project artifact parent; run with the installed Docker SDK Python.
It creates two journal-owned containers with local binds, a named volume and
both tmpfs option forms, checks stable fresh material and volume mutation,
then removes only successful owned fixtures. Failed fixtures are stopped and
preserved for inspection. No image download, provider, agent or evaluator runs.
Do not rebuild the selected binary while qualification is running. This checks
single-container mount composition, not multiple services or complete networks.

Keep the project small and offline-buildable after dependencies are installed.
New behavior belongs in one of the three CLI modules and should preserve the
existing `setup` and `run` command forms.

Keep adapter-specific unit tests in the owning adapter module (or beside its
implementation helper). Shared modules keep registry, dispatch, and
cross-adapter tests; provider preparation cases belong to their providers.

Use the pinned Rust toolchain and run:

Offline tests require Node.js22 and a genuine Python3.12 or3.13 executable
named python3.12 or python3.13 on PATH, with the standard-library venv module.
Provision these prerequisites before testing: cargo --offline does not prevent
product init from bootstrapping Python if the host lacks a supported version.
The hosted CI job explicitly provisions Python3.12 with the official
[setup-python action](https://github.com/actions/setup-python); the hardened
native job only verifies its already-installed compatible runtime and never
tries to install into its read-only /opt tool cache.

The offline_cli ordinary Project fixture verifies actual supported version
output and venv import on its effective PATH/child cwd before every init
invocation (including repeated-init/refusal tests). This is a test prerequisite,
not a product restriction: an unmet prerequisite fails precisely before invoking
ASB or downloading anything. An explicit test-only IntentionalBootstrap policy
is used ONLY by the existing incompatible-runtime/fake-uv, missing-runtime and
opt-in real-download fixtures. Their deliberately overridden PATH remains
authoritative. No version is globally faked and no product bootstrap flag is
introduced. Actual project initialization still installs Python when necessary.

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --locked
make rust-doc
make uml
```

The default `Rust CI` workflow is the development gate. It intentionally does
not build or test a distributable crate, so ordinary pull requests do not pay
for a second complete test suite and a clean release rebuild. Run the separate
manual `Package validation` workflow before a release or when changing package
contents:

```sh
gh workflow run package-validation.yml --ref main
```

That workflow runs `cargo package --locked`, verifies required runtime assets,
tests the extracted crate, and smoke-installs its executable. It is not a
required development-build check.

`make rust-doc` runs `cargo doc --locked --no-deps --document-private-items`,
because the CLI implementation modules are intentionally private. `make uml`
installs the pinned AutoUML release under `target/tools/`, generates the local
type-diagram sources and renderings, and leaves those derived files ignored.
`make flows` generates local dataflow and control-flow diagrams; `make
interactive` adds the local HTML renderings. These diagrams explicitly show
source relationships but never publish local paths or branch names. They
supplement the maintained architecture overview and should be regenerated when
reviewing public or private type relationships.

`benchmark_live_execution_transport.py` qualifies the default owned Docker
transport with `ASB_TEST_MATERIAL_IMAGE` and `ASB_TEST_SWECATALOG_ROOT` opt-ins.
It checks the actual stopped target's namespace mapping, separate stdout/stderr,
nonzero child status, execution timeout, cancellation, malformed-gate recovery
and confirmed resource removal. Short execution budgets use an independent
preparation budget. It starts no coding agent, provider or grader, and is not
SDK-shell qualification. Success removes only its declared container; failure
stops and preserves that owned fixture. Its receipt binds the exact Python
transport source hash, which must stay unchanged during execution.

Function-level dataflow/control diagrams use Graphviz's packed `osage` layout.
This keeps module boxes and every directed call/label, including cross-module
cycles, without the clustered hierarchical-ranking failure in Graphviz 2.43.
The trait/UML renderer remains unchanged. Diagram regressions verify all node,
edge and label counts against the complete current source and normalize paths
so absolute-path tests exercise the same graph as relative-path build commands.

`make build` writes its generated diagrams and interactive views under `target/`,
leaving the tracked documentation unchanged. Run `make uml` and
`make interactive` explicitly when updating the published diagrams in `docs/`.
`make update` saves changes to legacy generated diagram paths in a Git stash
before pulling; it reports the stash and does not touch unrelated changes.
The backup may contain manual edits: inspect it with `git stash show -p` and
restore it deliberately if needed. A conflicting unrelated edit can still
prevent the fast-forward, and no stash is automatically discarded.

These checks are offline: replay tests start the embedded tape proxy and test
clients on loopback, but CI never starts a real agent, bridge, or live provider.
`cargo test --locked --offline --test agent_artifact_ownership` drives actual
native replay and guest-driver clients through A-then-silent-B attempts. The
former fixed workspace trajectory reproduced stale answers, 34 tokens and four
steps after a naturally failed unlink. The fixed routes use fresh private output
paths, retain foreign legacy workspace bytes and never attribute A to B. Actual
scratch-cleanup/publication failure, symlink, malformed and oversized fixtures
retain independent grades/usage or mark unavailable evidence without changing
confirmed process quiescence. Native execution uses only the embedded loopback
replay; guest execution uses an inert local client, never a container/provider.
`cargo test --locked --offline --lib agents::artifacts` checks single bounded
snapshots, namespace collisions, file/symlink/in-place replacements, absent-then-
created files, explicit settlement failure and unwind cleanup. Deterministic
per-owner unit hooks replace the namespace after its pathname check but before
the relative open, and replace the source after its identity check but before
the custody move. Both regular and symlink replacements retain foreign bytes;
the hooks do not exist in production builds. Frozen evidence survives publication
unwind. `agents::driver::artifact_publication_tests` uses the actual OS publisher
with first/final write blockers: first failure retains raw JSON, final failure
retains the durable pending record and its answer/steps/quiescence facts. This
publisher unit seam is not an original-SDK retained-service qualification.
The actual guest-entry first-publication fixture also verifies retained raw JSON.
Production managed-transport seams separately admit one fake grader callback for
pending/failed ancillary scratch status with matching retained owner evidence,
and none for missing first metadata. The non-retained guest fallback confirms
reset before one fake grader callback and never fabricates answer/usage. These
are deterministic admission seams, not original-SDK or installed-client claims.
Shared identity
decision tests reject unknown and mismatched IDs, including high 128-bit changes.
Windows handle identity is source-only qualification here; identity-unavailable
hosts retain isolated evidence with a diagnostic instead of guessing deletion
ownership or disabling execution. Build the current ASB binary before focused
actual-entry tests and never relink while they are running.
The ownership-window follow-up passed 61 adapter tests (including the snapshot,
custody-window and OS-publisher seams), the exact adapter-layout guard, actual
artifact/guest/completion tests, the existing 24 protocol harness tests, the
updated 21 execution and 10 guest Python cases, strict all-target/all-feature
Clippy and formatting. This old-base checkpoint has no full-suite or integrated
current-main qualification yet; Windows remains source-qualified only.
The subsequent native publication-loss regression uses the actual ASB Mini-SWE
route, embedded loopback replay and bundled local repository-navigation grader,
not an external SDK verifier. With the log blocked and measurements disabled,
the previous binary returned a completed grade/34 tokens/four steps but deleted
the only raw trajectory. The fixed fixture requires a recoverable mode-0600
frozen copy containing that exact answer/usage/steps, a locatable diagnostic,
one client call and no completion receipt. Real runner-entry observation also
checks grading, workload finish and provider finish exactly once with blocked
logging and either disabled or failed measurements; summary output is not
appended twice. Unit cases verify exact frozen bytes after source replacement,
create-new file/symlink refusal, permission failure and a test-only synchronization
fault, all retaining the original when handoff is unconfirmed. No old-base full
suite or integrated-current-main qualification is implied by these focused gates.
The native handoff checkpoint passed 63 adapter tests, 73 runner tests (three
explicit opt-in cases ignored), the layout guard, the extended actual-entry
fixture, strict all-target/all-feature Clippy, formatting and binary build.

`cargo test --locked --offline --lib runner::capture` uses real local children
for simultaneous stdout/stderr pressure, finite capture, closed/held pipes,
execution/drain deadlines, writer failure, unwind and uncertain cleanup custody.
An isolated Linux subreaper fixture owns and reaps the held-pipe descendant;
unrelated sentinels survive. Deterministic private faults distinguish interrupted
reads from permanent read failure and exercise pre-spawn keeper exhaustion and
cleanup polling uncertainty. No output-reader thread or unbounded queue is used.
The Unix transport fixture pressures an unread socket, checks cancellation and
flag restoration, and rejects nonpipe output without changing its flags.
`cargo test --locked --offline --lib full_runner_entry_bounded_capture` needs the
current actual ASB binary built first. Five replay-only full-entry cases preserve
true/false/native file grades, exactly-once grading/provider/workload cleanup and
report outcomes while withholding receipts after capture limits/read faults.
They retain validated cassette usage and independent mini-SWE artifact usage and
steps, but reject usage inferred from the incomplete transcript. These qualify
Linux execution, not native Windows or arbitrary blocking borrowed writers;
the Windows private-pipe pump retains a source implementation, and non-Unix guest
presentation remains caller-controlled. Retained workload services keep their
existing ThroughEvaluation supervisor and are covered by the separate guest
agent/owner-loss tests.
`cargo test --locked --offline --lib actual_unmeasured_sweep_capture_failure`
also runs actual sequential sweeps with true/false and nullable native rewards,
automatic comparison, repeat regeneration and resume refusal without another
client. Latest-log configuration/task/slot mutations and malformed trailing
records must leave the previous report and source bytes unchanged. These use
the same private process-local evaluator fixture; the native fixture declares
matching selection metadata before normal saved-plan publication, not by
editing retained evidence or adding an SDK capability. The bounded log fallback
is reporting-only and cannot create missing completion receipts. Separate log
tests reject latest-attempt conflicts with older private outcomes and ambiguous
multi-replica slots, while preserving private record bytes.
`cargo test --locked --offline --test tui_install` exercises the actual ASB
install helper and real terminal backend with local fake npm/node executables.
Closing the backend's output pipe forces EPIPE while the input PTY stays open:
the original drawing error, cleanup diagnostics and restored echo/canonical mode
are all checked. Ctrl-C, Esc, owner loss and helper loss stop owned slow,
double-forked TERM-resistant writers; unrelated sentinels survive and no writes
continue after confirmed cleanup. Successful and failed installs, descriptor
noninheritance, nonterminal refusal and invoking-binary pathname replacement
are checked without package downloads, live agents or provider credentials.
`cargo test --locked --offline --lib install::operation` additionally covers
fixed protocol validation, nonblocking queue pressure, pre-spawn keeper failure
and unwinding after actual installer admission. The unwind fixture also breaks
the real guardian's receipt channel with stderr redirected to `/dev/full`:
cleanup uncertainty must retain custody without a second panic, and the real
PTY must regain echo/canonical mode. Actual helper routes verify tool-package,
workload-validation and collector-dependency phases, with every enum wire code
round-tripped and unknown/reserved phases rejected. Build the current ASB binary
first for focused library tests, as for native gate qualification below.
Receiver-specific fixtures pair the actual `ManagedGate` with owned OS sockets
to reject duplicate/post-final/malformed records, missing final status despite
a successful guardian, and success status despite a failed guardian. A flooded
socket proves a live tick consumes only its 64-frame budget. These deliberate
sender faults are distinct from the actual-helper/PTY qualification above.
The owning bpftrace installer fixture holds local fake download/build commands
under a real gate, verifies the phase and completed-work percentage before
releasing either command, and never needs an installed host collector.
Dependency settlement tests run a zero-exit fake installer with missing,
non-executable and healthy outputs; only refreshed ready files permit selection.
`cargo test --locked --offline --test tui_terminal` drives the actual current
binary through a bounded PTY. Workload fixtures use only local fake catalogs:
advertised Enter finishes, Tab edits, Back discards, saved choices reopen,
edits replace subsets, and deselected workloads are excluded. Checkpoints
inspect disk before finish and cancellation, then verify exact published
subsets and terminal echo/canonical-mode restoration. No agent/provider runs
or host changes are needed.
The same PTY fixture publishes concurrency 64 through the actual setup wizard.
`cargo test --locked --offline --lib tui::identity_tests` checks all concurrency
values through generated TOML, colliding prompt/trial-set labels and repeated
suffixes, reserved raw identifiers, label-independent installation/readiness,
provider/model bindings, collector dependencies and saved trial IDs. Model
discovery is injected and executable fixtures are local inert files.
`cargo test --locked --offline --lib executables::tests` installs local shell
fixtures through every registered agent/tool source-install route, then checks
readiness and actually executes the selected fixture. Isolated subprocess
cases cover external execution overrides, broken/non-executable/symlink
candidates, host-tool policy and custom project roots. The Codex adapter also
checks its native-package preference and explicit-source precedence. No package
manager, provider, real agent or download is invoked by these path-policy tests.
The native preparation regression combines Codex native-package preference with
fresh HOME/CODEX_HOME, explicit override precedence and unusable-candidate refusal.
`offline_cli::unusable_selected_agent_never_falls_back_to_a_different_path_installation`
also runs the actual CLI with a valid marker-writing PATH fixture: non-executable
and dangling project selections must refuse launch, while removing the local
selection must actually launch that fixture. It uses legacy loopback replay and
never contacts a provider or runs an installed coding client.

`cargo test --locked --offline --lib install::source` checks atomic installation
with local fake downloads and version probes. The shared private `OwnedReaper`
is established fallibly before each downloader or version subprocess starts.
Normal errors and unwinding bound the caller's cleanup wait; an unconfirmed
direct child transfers to that already-owned worker rather than being dropped
or delaying the caller indefinitely. Uncertain cleanup prevents publication
and preserves the original failure alongside its cleanup diagnostic. Worker
creation failure launches no tool. Linux-only deterministic fault fixtures
verify pending/error handoff, idle-worker exit, pidfd-backed reaping, unrelated
sentinel survival and unchanged previously installed bytes/mode. No network,
host settings or arbitrary descendant-tree supervision is involved.
`cargo test --locked --offline --lib publication_tests` exercises source-file
publication with an offline fake curl executable and real SHA-256 computation.
It checks partial HTTP failures, empty/oversized bodies, checksum/probe failures,
missing transport, timeout/child reaping, symlinks, rename failure and successful
local/remote replacement, including nonblocking rejection of FIFO sources.
Staged version-probe failure and timeout also retain the prior installation.
A blocked downloader proves that the previous ready
executable remains visible until complete validated bytes are committed. These
tests make no network requests and install no host packages.
Perf-stat regression fixtures process a complete synthetic file larger than
the diagnostic cap, retain missing-interval timestamps and hybrid CPU counter
totals, and reject invalid or excessive input without publishing partial totals.
They need neither perf permissions nor a live agent/provider.
Flamegraph pipeline regressions use synthetic local tools and pin each direct
child's identity with a pidfd. Success, stage failures, output-file errors,
preflight failures, timeout and unwinding must stop and reap every admitted
stage while leaving an unrelated sentinel alive. Cleanup signals only owned
direct children, never a process-name/PID-parent search. The caller's cleanup
wait is bounded; an unconfirmed kernel-stuck child remains owned by a fallback
reaper and is reported as a cleanup failure, not a completed graph. These
fixtures neither invoke host perf nor qualify arbitrary tool descendants.
The fallback worker is created fallibly before any tool is spawned. Creation
failure starts no tools; cleanup transfers uncertain direct children through
a private ownership slot, without spawning a thread or waiting on that worker.
Deterministic test-only faults cover worker-creation failure and uncertain
cleanup polling. Exit receipts also check idle, failed and unwinding workers;
these tests do not need a genuinely kernel-stuck process.
CPU/I/O and BPF adapter fixtures likewise place final samples/cumulative maps
beyond 256 KiB, pass unrelated diagnostic strings and require full-file means,
series and final values. Shared output tests reject incomplete, oversized,
invalid-UTF-8 and nonfinite data and enforce the processed JSON budget without
allocating a second serialized buffer. These are offline fixtures, not native
BPF clock-series qualification.
`cargo test --locked --offline --test compare_setup` checks repeated CLI setup
selections, scoped models/trials, dependency expansion, non-overwriting sweep
publication, standalone measured/unmeasured reports and an injected browser
opener. Browser tests do not launch a real GUI or contact a provider.
`cargo test --locked --offline --test report_filters` executes the embedded
report JavaScript under Node with an injected DOM fixture: every filter
dimension, combined intersections, replica identities, select/clear all,
empty charts, hover cleanup and selection restoration. SVG-generation tests
also check that bars, series/legends and composition rows carry benchmark groups.
`cargo test --locked --offline --lib compare::pipeline_tests` exercises actual
report generation without benchmarks or providers: cross-row/population
validation before writes, bounded growing/replaced/FIFO inputs, symlink-parent
path semantics, aligned nullable observations, full-source JSON with bounded
graphs, gap preservation, atomic regeneration, persistent writer exclusion,
recoverable previous generations and truthful post-publication failures.
It runs the actual generated SVG and embedded interaction script under Node's
synthetic DOM, including observed-sample hover, zoom, pan and modal controls;
signed scalar fixtures additionally check negative/mixed/extreme finite and
subnormal geometry, unchanged JSON values, embedded HTML and signed-value hover.
this is not browser-layout qualification. The ignored
`real_headless_browser_qualifies_generated_report_interactions` test can use an
existing local browser selected explicitly with `ASB_TEST_CHROMIUM`; it never
downloads one or modifies host/Snap permissions. A host confinement failure is
not a passing browser qualification. Existing state/event SVGs remain intact.
Private-backend tests force unsupported exchange, failed no-replace capability,
backup/commit/rollback and directory-sync errors on real owned directories. They
prove old user-added files and unrelated sentinels survive and a racing foreign
destination is never overwritten. The recoverable fallback explicitly permits
a short missing-destination/crash window; after a crash, preserve the sibling
`.asb-report-previous-*` backup before restoring it to an absent report path.
Linux uses `renameat2`; macOS uses `renamex_np` with `RENAME_SWAP`/`RENAME_EXCL`.
These are source contracts, not a claim of native macOS execution on Linux.
Reserved-`all` tests cover complete registry expansion, enabled/original trial
populations, catalog-name collisions and provider-scoped model discovery using
only a bounded loopback HTTP fixture with no real provider credentials.
The agent endpoint matrix tests every declared interface/authentication pair
and rejects every undeclared pair before configuration or command mutation.
These are protocol-only fixtures: no agent/provider is executed. See the
[independently checked repair ledger](CODE_SMELL_REPAIRS.md) for the broader
code-smell audit and remaining delivery classes.
Continuation metadata is optional for a fresh grade, not optional for reuse.
The generation resolver shares passive manifest/start-reference wire types with
the publisher, validates the entire original population before payload proofs,
and derives new prepared-material paths without granting execution authority.
Settlement binds inherited receipt bytes exactly and publishes only new slots.
The generation-resolution fixtures cover substituted metadata, stripped start
references, changed old receipt bytes, bounded new material and mixed sealing.
The public repeated-sweep interruption fixture uses C4/N2/jobs1 and a closed
output pipe to retain four genuine first-batch receipts (including a false grade)
and an untouched second batch. Public continuation consumes the classified owner,
publishes all configuration manifests before any new client, and retains all
campaign locks through settlement and comparison. Shared batch execution preserves
original IDs and emits new generation references/material without overwriting
old receipts or input seals. Recording continuation still requires immutable
union publication/recovery and is not delivered by the replay/live activation.
These fixtures use local synthetic clients, not installed agents or SDKs.
The public matrix additionally qualifies seeded ZIP/private payload routing,
known-incomplete numeric selection disclosure, measured dispatch/reporting and
live mode against an owned synthetic provider. All preserve the false grade,
eight original result slots and zero new clients on the second resume. A corrupt
last old receipt prevents any new admission. The source checkpoint passed the
complete offline suite (1,289 library cases, 13 opt-in cases ignored), strict
all-target/all-feature Clippy, build and unchanged-binary verification. After a
test-only fixture deadline adjustment, all 47 repeated-run cases and Clippy
passed again. Rust docs, UML, flow SVGs and interactive diagrams regenerated;
Graphviz retained its straight-edge fallback warning, with no graph content
removed. This is not original-SDK, live LLM, packaged-install or hosted-CI proof.
Campaign input-root regressions in `sweep::checkpoint::campaigns` check exact
shared plan binding, private-input/configuration changes, original-source
replacement and output independence without clients. The actual `repeated_run`
case `campaign_input_seals_stay_stable` drives a registered Pi adapter backed by
a local fixture executable: four synthetic-provider recordings and four replays
with that provider shut down keep stable seals. This is not an installed Pi,
live-provider or SDK qualification. The owning sweep-phase subprocess matrix
also blocks initial key and final seal publication for two-batch campaigns;
four real local grades and all report rows survive with one warning, while
invalid keys and failed-seal diagnostics remain preserved. Build the actual ASB
first and keep source/executable stable throughout runtime/full qualification.
Runtime-discovery regressions require building the actual ASB binary first,
then running `cargo test --offline --locked --lib runtime_inputs::tests` and
`cargo test --offline --locked --test benchmark_protocol`. The inherited-pipe
fixtures use an outer real native gate/watchdog and an unrelated sentinel;
cleanup is confirmed before assertions, including deliberately red historical
reproductions. Private short budgets exercise the unchanged 60-second policy's
algorithm. Python tests cover during-read probe limits, remaining deadlines,
permanent read errors versus Interrupted, pre-spawn keeper failure and uncertain
handoff refusing further admission. Serialized pathname accounting must match
the existing schema-1 JSON, including escaping/separators/newline, rather than
introducing a smaller arbitrary pathname cap. These are offline Linux/Unix
contracts, not qualification of the retained non-Unix legacy fallback or
whole-SDK descendant ownership outside a discovery gate.
The successful inherited-pipe fixtures allow five seconds for native-gate and
Python startup (including the nested probe's deliberate 50 ms deadline). Their
outer watchdog is ten seconds, below the descendant's 30-second hold. The
deliberately blocked-input case retains its 150 ms exchange deadline. This
separates host startup scheduling from the cleanup invariant after an observed
181 ms packaged-CI success-case timeout; production's 60-second policy, mandatory
success, descendant-reaping and unrelated-sentinel assertions are unchanged.
If an outcome, replica/group receipt or initial/final input seal cannot be
published, human output identifies the missing evidence without changing the
grade or retrying a client. JSONL instead emits a separate schema-1 object with
`event: "continuation_warning"`, a stable `code`, display path, explicit
`path_role` (`destination`, or `source_outcome` when an observer cannot report
its receipt destination), safe reason and
explicit remediation. Consumers must distinguish these events from schema-2
benchmark summaries (`task`, `process_success`, `workload_success`); unchanged
successful publication adds no event. Arbitrary helper/observer text is not
printed. Identical stage/configuration/cause warnings are emitted once across
replicas; different causes remain distinct. A writer failure is reported without
discarding an existing benchmark failure. Existing sealed logs and grade
records are never rewritten to add these presentation diagnostics. Missing
starts/seals may not be reconstructable: preserve results and use explicit
continuation/reconciliation only when its normal checks permit it.
`cargo test --offline --locked --test completion_diagnostics` uses actual blocked
publication paths, a private catalog-command fixture and a local synthetic
client, not a provider, installed agent package or workload container. It checks
both grades, client failures, concurrent deduplication, broken writers and
conservative resume without another client invocation.
Report identity regressions cover literal dots/brackets versus nested paths,
empty/backslash keys, Unicode and colliding/long graph filenames, case-insensitive
aliases, repeated configuration stems and deterministic regeneration. Numeric
paths escape object-key separators; use `comparison.json`'s graph list rather
than constructing filenames for unusual metric names. Conflicting collector
identities fail explicitly instead of silently replacing a prior observation.
Shell completions are generated from the actual Clap command schema using a
pinned compatible `clap_complete`. Tests enumerate every public command/flag
for both shells, execute Bash completions in every command context, and check
writer errors and the internal helper's separate/equals argument forms.
Registry suggestions affect only the completion clone, not custom tool parsing.
`cargo test --locked cassette::` exercises the provider-free multi-track backend:
manifest and compatibility bounds, stable seeded choices, selector edge cases,
bounded metadata inspection, selective corruption checks and durable publication.
Journal tests inject failures around atomic metadata writes/syncs/links; finalizer
tests cover ZIP output bounds, freeze/preparation/final-link/confirmation faults,
individual cleanup boundaries, exact-candidate recovery and foreign targets.
An owned test child is actually killed at recording/prepared/linked/published
stages, checking active-owner exclusion and recovery without new attempts. No
agent/provider/container runs. Fault hooks exist only in test builds and have no
environment/configuration surface in the installed CLI. `run` integrates the
backend through its own recording/replay coordinators; sweep/resume/report and
viewer delivery remain separate steps in MULTITRACK.md.
The read-only `TrackEvidence::verify_batch` regressions use bounded synthetic
recordings with no clients: duplicate and conflicting references, 257 recordings,
aggregate quota boundaries, late metadata errors, selective corruption, active
writer/publication races, lost targets, between-pass mutation and exact opened
descriptor identity. Test-only observations count catalogue and payload reads;
they are not production fault switches. Grouped reports perform two bounded
catalogue passes per published recording, with one open archive at a time, not
one pass per row. Per-archive format limits and whole-operation `ProofLimits`
are separate; single proofs retain independent journal/manifest allowances.
`cassette::tests::reservation` checks byte-exact wire compatibility, all optional
digest combinations, escaped logical values, actual maximum-statistics journal/
manifest/proof sizes and ZIP directory digit transitions. Capacity planning uses
the same private serialization projections as real evidence and creates no
observed fingerprint or file. Exact/minus-one/overflow quotas and a pure full
ZIP32 population remain covered. Campaign layout and compare reject altered
reservations; the genuine material-gate fixture refuses same-length wrong names
before journal creation and persists actual hashes, never sizing placeholders.
The repeated-sweep CLI suite additionally selects 60 configurations at repeat
1000 with long model names: each fits independently, but the aggregate must be
rejected with zero synthetic-provider/client calls and unchanged previous
index/report. No live credentials, SDK or containers are involved.
`cargo test --locked --offline --lib runner::recording` includes deterministic
source-name and parent-namespace replacement after original validation, private
custody move/restore/unlink/sync faults, EEXIST arrivals, no recursive deletion,
and unwind retention. `raw_custody_consumption` in the actual `repeated_run`
target records true/false local file grades through a synthetic loopback provider,
checks track/continuation evidence after raw deletion and replays selected tracks
with the provider closed. Build the actual ASB first and keep its hash stable
through the runtime checks. These qualify Linux fixtures, not a live provider,
external SDK, native Windows, or crash-atomic multi-entry restoration.
`cargo test --locked --offline --test repeated_run` additionally records real
embedded-proxy ZIP captures with a bounded loopback provider and synthetic agent.
It checks no-overwrite/fresh defaults, ordinary complete failed recordings,
seeded selections under changed arrival ordering, actual per-slot response
identity, statistic ties, selected-only integrity, every compatibility field,
source replacement after private staging, legacy selection/unknown metadata,
strict partial consumption and zero-client/inference metadata-budget rejection.
Recording commit failure preserves true grades and existing primary failures but
publishes failed typed log/measurement disposition before withholding completion
and stopping later admission. These tests use fixture-only credentials and no
installed agents, real LLM calls, containers or privileged collectors.
`cargo test --locked --lib runner::batch` checks the internal cancellable
preparation gate and runner no-launch/cancellation wiring. Material regressions
live in `workloads::material` and `workloads::session`: bounded canonical files,
unsupported links, immutable initial identity and evaluator side effects. These
provider-free tests qualify the native preparation foundation, not the pending
repeat/bundle CLI or official environment identity integration. See
MULTITRACK-EXECUTION-DESIGN.md for the remaining requirements.
`cargo test --locked --test benchmark_protocol` includes pure Python qualification
of the official prepared-material protocol and bounded raw TAR reader. These
fixtures need no Docker, SDK installation or provider. They cover the current
single-container/non-root/unmounted declaration, semantic Git state, rejected
uncovered services/mounts/ancestors, archive bounds and cancellation. They do not
qualify real installed environments or the remaining broader material coverage.
The separate opt-in `benchmark_live_material.py` uses the installed SWE-bench
SDK interpreter and `ASB_TEST_SWECATALOG_ROOT` to compare two actual original
Lite preparations, repeat capture, change Git configuration and reject a dirty
worktree. It launches no agent, provider or evaluator. Its fixture containers
are normally removed; diagnostic artifacts stay in the selected project. This
is representative single-container qualification, not full-population or
root/mount/service coverage. Do not treat a skipped opt-in as a passing live test.
Set `ASB_TEST_BENCHMARK_ASB` as well to qualify the actual root-helper path in
those two fresh preparations. `benchmark_material_root.py` and
`benchmark_material_exec.py` supply provider/Docker-free composition, exact
staging verification and transport-fault tests through `benchmark_protocol`.
`benchmark_live_repeated_original.py` drives public `run --repeat 2` with
concurrency two, the installed Pi client and four original SWE-bench Lite attempts
against a synthetic loopback provider supplying a known repair. It uses the same
ASB/project opt-ins, may download public client packages into isolated guests,
and never uses a real provider key or calls an LLM. This qualifies the
controller/runner/grade path, not AI solving or full mount/service coverage.
Set `ASB_TEST_BENCHMARK_SUITE=terminal-bench-1.0` or `terminal-bench-2.1`
to apply the same counts, isolation, compatibility, official grading and cleanup
assertions to those installed original SDK classes. Missing installation fails
explicitly; these opt-ins are not claimed green merely because SWE-bench passed.
The same opt-in checks actual Docker root-header metadata inspection, retained
120-second socket read timeout and early response closure. Pure
`benchmark_docker.py` tests response ownership before the first read, after a
partial read, at EOF and on error. Directory-header inspection never advances
to descendants in the pure material fixture. Separate per-path archive hashes
are not used as an environment identity: they cannot preserve cross-path inode
relationships and multiply RPC cost over large change inventories.
`cargo test --locked --lib workloads::census` qualifies descriptor-pinned Linux
declared-root observations and global resource accounting. `cargo test --locked
--test material_census` exercises the private actual-binary protocol with an empty
environment, strict bounded input and redacted hash/count-only output. These
components do not establish privileged-attribute, mount, complete-namespace or
snapshot coverage. Visible xattrs/ACLs are read and hashed by
`workloads::attributes`; run `cargo test --locked --lib workloads::attributes`
for binary names/values, bounded probes, OverlayFS filtering, two-pass mutation,
shared budgets and symlink replacement tests. The census additionally checks
attribute changes and root-level symlink target isolation. ASB does not request
extra privileges to enumerate hidden attributes; that coverage stays unclaimed.
Selective v3 census tests additionally cover image-anchor/selector bounds,
cross-root hardlink closure, mixed full/selective roots, exact directory and
symlink selection and replacement before deferred reads. Selective mode still
walks metadata/attributes but explicitly does not claim all file bytes were read;
the enclosing adapter must validate the real image and complete change inventory.
The opt-in `benchmark_live_census.py` needs the installed SWE SDK interpreter,
`ASB_TEST_SWECATALOG_ROOT` and `ASB_TEST_BENCHMARK_ASB`. It stages the same ASB
runtime in a newly owned original task, checks cross-root hardlinks versus copies,
reads the original repository and verifies normal container removal. Its
attribute mutation case changes only a new fixture file and confirms that binary
values affect identity without appearing in output. The fixture also compares
two actual image-anchored root observations plus full-content shared memory;
it does not establish full mount/service/network equivalence. Its declared
root subprocesses have a separate 90-second watchdog; the helper's own 60-second
deadline is cooperative between filesystem operations. A subprocess timeout takes
the enclosing owned workload's stopped-preservation path; killing a Docker CLI
alone does not prove guest-exec termination. No provider, real client,
evaluator or host mount is used; artifacts stay in the selected project.
`cargo test --locked --test repeated_run` exercises the native repeat prototype
through the real CLI with a fake client and loopback replay. It verifies exact
counts, overlap/sequential batches, fresh material, private replay input copies,
failure disposition, output closure and measurement override paths without
credentials, real agents, containers or privileged tracing.
Its capture-overflow regression retains independent true/false file grades,
finishes the already-admitted sibling, withholds the incomplete slot's private
outcome/receipts and stops before batch two. The common capture fixtures also
require shared-launch failure notification before stopping the owned child.
The repeat suite also routes Perfetto preflight through the actual CLI with
external measurement destinations, including one inside a differently configured
project. It uses fake tool probes and stops before tracing/agent launch, verifying
that capability authority comes only from the benchmark project, not output paths.
The opt-in `benchmark_live_repeated.py` instead needs
`ASB_TEST_PERFETTO_PROJECT` pointing to an already delegated benchmark project
and `ASB_TEST_BENCHMARK_ASB` pointing to the current executable. It runs two
sequential batches of two synthetic replay clients with real Perfetto captures,
including one ordinary client failure and a nested foreign measurement project.
It verifies overlap, later-batch admission, four complete scoped traces and
released project leases. It never configures host tracing, installs clients,
contacts an LLM or uses Docker. Artifacts remain under the selected project;
do not rebuild/replace the selected executable during qualification.
The cli2key concurrency regression uses two mock bridge listeners and mock
clients, with fixture-only credentials. Both authenticated local requests must
overlap and reach different owned servers; both servers must be reaped afterward.
It does not use the installed cli2key package or a backend subscription.
`cargo test --locked --offline --lib cli2key` additionally checks real local
startup fixtures with oversized/unterminated output, excessive messages/bytes,
silent startup, post-readiness draining and cleanup while another output
writer remains open. Reader/reaper creation fails before child admission.
Injected errors and unwinding after either spawn, shared-deadline/poll-error
handoff, pidfd-pinned reaping and unrelated sentinel survival require no host
credentials or provider. The same guard/reaper remains owned on success.
The caller's cleanup wait has one five-second budget across both children and
readers; unconfirmed children retain fallback custody rather than being
dropped. This does not establish descendant or abrupt-crash cleanup.
Repeated-run provider fixtures exercise actual two-replica batches: uncertain
provider finish preserves true/false grades but prevents admission of another
batch; confirmed finish permits later negative-grade attempts. A channel-driven
early-error fixture requires waiting material peers to observe cancellation
before provider teardown can complete. Output failures retain their I/O class
when provider cleanup also fails. These fixtures use replay and root-scoped
test-only provider sessions, not live credentials or provider processes.
The `provider_finish` library fixtures additionally run seven actual replay-only
benchmark attempts: true/false grades, client failure, early preparation failure
and successful finish. They require explicit finish once, preserve each grade
and primary error, report cleanup uncertainty and withhold completion receipts.
Native completed rewards with nullable resolution remain valid through normal
finish; provider uncertainty preserves their numeric reward and execution
observation while setting structured cleanup confirmation false.
Loopback catalog fixtures cover successful parsing, invalid JSON and HTTP failure
combined with finish failure. Both use only fixture credentials. Output-worker
exit receipts confirm cooperative exit even after the caller releases an
unfinished thread handle at its deadline; no unconditional join is used.
Final admission also rechecks both children after proxy readiness. A pinned
two-child fixture makes the bridge exit after its key and before the proxy's
marker, then requires rejection, complete direct-child reaping and sentinel
survival. An actual embedded Node compatibility hop routes through a held
loopback HTTP fixture using its real owned curl child. Explicit finish must
close the outbound connection, reap curl and exit cooperatively with status
zero. An ignored-EOF fixture verifies escalation after the two-second grace
inside the same five-second caller budget, with an uncertainty diagnostic.
The same embedded fixture aborts an unfinished POST while a sibling outbound
request completes, then shuts down with another partial POST still open. Body
collection/URL/forwarding errors remain inside one handler envelope, and closing
or destroyed responses receive no late 502. Request-event receipts establish
the partial-body states without relying on a sleep. The concurrent CLI fixture
retains bounded fixture-only Node stderr when reporting a startup failure.
The agent-process sampler regression launches a bounded local Node fixture,
changes its process title and checks two seconds of continued sampling, an
unrelated Node process and the original process's exit. It uses no provider,
agent package, container or privileged collector.
The deterministic pre-exec regression holds an owned child before exec using
a bounded pipe handshake. Matching inherited identity must not enter the
historical worker census or RSS total; executing the selected entry point
still qualifies. The existing concurrent Node fixture retains exact worker
counts and unrelated-script/tool exclusions. Fork-only workers remain an
explicitly unclassified lower bound, not a guessed count.
Run `asb run` with credentials only as an explicit local
smoke/benchmark operation.

Model catalog transport uses nonblocking Unix pipes, polling request input,
response output and diagnostic output concurrently. A single 35-second
operation budget includes a 30-second I/O phase and up to five seconds reserved
for direct-child cleanup; individual reads, writes and exit observation never
restart that clock. Bodies remain capped at 16 MiB and diagnostics retain only
the final 8 KiB for sanitized HTTP/proxy/content-type metadata. Credentials stay
in the in-memory request sent through stdin, never argv or a transport file.
Errors, deadline expiry and unwinding close owned pipe handles and stop/reap
only the direct curl child. If bounded exit observation cannot confirm cleanup,
an already-established private `OwnedReaper` retains custody and the request
fails explicitly. This bounds caller-side observation, not an uninterruptible
kernel operation or scheduling delay. It does not supervise arbitrary curl
descendants; inherited open pipes cannot extend the I/O deadline.
The bounded transport currently requires Unix. Non-Unix model discovery fails
before spawning a request; a cancellable non-Unix pipe implementation remains
an outstanding portability contract, not an unbounded fallback.
`cargo test --offline --locked --lib models::` includes private subprocess
fixtures for duplex pipe pressure, blocked stdin, oversized output, inherited
stdout/stderr, deadlines, setup errors, uncertain cleanup and unwinding. Linux
pidfd observations verify original direct-child exit/reaping and unrelated
sentinel survival. Existing fake-curl fixtures retain HTTP/proxy/JSON/error and
credential-redaction checks, and require curl's actual size/time arguments to
match the shared reader/watchdog contract. Real subprocess fixtures accept an
exactly 16 MiB JSON body and reject one extra byte. No real curl request,
installed agent or provider is used by these tests.

Coverage is an optional local diagnostic, not a required CI check or merge
threshold. The former `coverage-90-percent` CI job and required check were
removed deliberately. To run the retained offline diagnostic locally:

```sh
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov --version 0.9.0 --locked
bash scripts/check-coverage.sh
```

The diagnostic script reports and checks its historical 90% thresholds for
both LLVM's complete aggregate and production-only coverage. Those local
thresholds are informational for delivery, not a branch-protection policy. The
production denominator comes from LLVM's non-test `asb` executable map, without
excluding any production modules; unit and integration tests contribute the
execution counts, but adding test-body lines cannot inflate that denominator.
Isolated test subprocesses discard inherited state and credentials while
retaining only LLVM's coverage-output setting. Reports and uncovered-file
summaries are retained under `target/coverage/`, including on CI failures.
Each invocation removes only its four generated report files before checking or
building, so an early failure cannot leave a stale green summary; unrelated
evidence files remain intact. Workspace coverage objects and raw profiles are
cleaned while dependency compilation caches are retained. This also prevents
obsolete test-binary maps from distorting totals after version or feature changes.
No live provider, real agent installation, or privileged collector is necessary.
The production map excludes only registry/git package directories identified by
locked offline host-filtered Cargo metadata (including relocated/symlinked caches).
Unbuilt cross-target packages need not be downloaded for native coverage. Workspace
and local path-package sources stay in the denominator; no product module is excluded.
Missing-project capability fixtures test the same private initialized-file guard
used by production, and ancestor discovery uses an owned fixture parent/child.
They never assume TMPDIR has no real ancestor project or pass one to host setup.

`cargo test --locked --offline --lib project::` includes the shared host-setup
request/consent/capability policy tests. Fake executors exercise the actual sysctl
and cgroup orchestration, every prerequisite failure, interactive versus
passwordless mode, independent group consent, `--yes`, invalid/empty/EOF answers,
and reader/writer/flush errors. These tests execute no host commands. Existing
isolated fake-sudo/proc-input fixtures additionally check production argv/stdin
construction without changing sysctls or cgroups. A prompt or pre-execution
output failure grants no approval; execution failures remain best-effort and
never imply rollback of commands that already succeeded.

Token accounting has synthetic, provider-free regression fixtures for all
registered agent families, plus replay CLI checks for JSON, logs, and
measurement files. Run the focused checks with `cargo test --offline --locked
token_usage_mock_tests` and `cargo test --offline --locked --test offline_cli
mock_replay_`. An offline Cargo flag prevents dependency downloads, and no
test requires a provider key.

The optional `local-host-checks` CI job repeats the ordinary checks on a local
Linux x86-64 GitHub Actions runner labeled `asb-local-ci`. It runs only for
pushes to `main` or a manual dispatch of `main`, and is disabled unless the repository variable
`ASB_LOCAL_CI_ENABLED` is set to `true`. The normal GitHub-hosted job remains
enabled for pull requests and pushes. The local runner needs the pinned Rust
toolchain, Node.js 22 (or a functional Actions tool cache), and access to the
dependencies needed for `cargo package`.

ASB is public. The job's event condition does not prevent an untrusted pull
request from proposing a different workflow that targets a registered self-hosted
runner. Do not register or enable a local runner until its isolation and runner
access controls have been explicitly chosen and verified; do not expose host
credentials, project trees, or host services to untrusted jobs. A dedicated
account and service hardening reduce risk but are not a disposable VM boundary.
In particular, a native runner shares the host network and can reach loopback
services, and a job runs with the runner account's own credentials. Review the
full workflow and code before approving external pull-request workflow runs.

The opt-in real-agent fixture test launches an installed client against a
synthetic loopback provider with dummy credentials. On Linux, compile it and
run each client in a separate rootless network and PID namespace with only
loopback enabled (replace the executable with the project-local installed binary).
The PID namespace also stops client-launched helper daemons when the test exits:

```sh
cargo test --locked --test real_codex_smoke --no-run
unshare -cfpn --keep-caps sh -c 'ip link set lo up && exec env ASB_REAL_AGENT_ID=codex ASB_REAL_AGENT_BIN=codex ASB_REAL_AGENT_EXECUTABLE=/absolute/path/to/codex cargo test --locked --test real_codex_smoke real_agent_uses_selected_openrouter_endpoint -- --ignored --nocapture'
```

The fixture refuses to run if it detects non-loopback network routes. Set
`ASB_REAL_PROVIDER=anthropic` to exercise the Anthropic Messages interface, or
`ASB_REAL_PROVIDER=cli2key ASB_REAL_MODEL=gpt-6-astra` for the local bridge's
Responses interface. The test uses fixture keys and never needs a live
provider credential. Some clients perform other initialization before sending
the benchmark prompt, so an endpoint probe alone is not a successful run.

Workspaces, caches, and build outputs stay under `/srv/data/projects`. Do not
put credentials, machine-specific paths, prompts, or provider responses in the
repository. Live runs receive credentials from the agent/provider environment.

Measurement smoke tests are local-only. Use `asb run CONFIG --measurements
DIR` when a host profile is needed; keep `ASB_MEASURE_*` collectors disabled in
CI. Measurement JSON must remain bounded and must not contain credentials,
provider responses, or full agent transcripts.

Owned guest staging uses the shared `asb_staging` helper for both material
capture and client bootstrap. Fixed prefixes are created once with the task's
configured numeric UID/GID, and uploaded files receive those owners directly.
Capture-to-bootstrap reuse verifies the exact runtime tree and directory identity;
it does not restage or recursively change ownership. The guest keeps directory
descriptors and an attempt-bound private receipt through preparation. A genuine
task cwd of `/` is allowed with that receipt; a workspace inside an instrumentation
prefix, including through a symlink alias, is not. These are local lifecycle and
consistency checks, not an authentication or signing prerequisite. Read-only
whole-root observation may run as root inside the owned container; installers,
clients and SDK shells retain the configured user and environment.

`benchmark_staging.py` and the Rust staging tests cover ownership, reuse,
replacement and malformed receipt failures without Docker. Opt-in
`benchmark_live_staging.py` requires `ASB_TEST_BENCHMARK_ASB` (a separately copied,
hashed ASB binary), `ASB_TEST_STAGING_IMAGE` (an explicitly selected cached Linux
image with `/bin/sh`, `id`, `mkdir`, `tail` and `cat`), and
`ASB_TEST_SWECATALOG_ROOT` (an evidence directory). Run it using a Python with
Docker SDK installed. It uses an isolated networkless container, numeric nonroot
UID/GID, the real guest driver and a fixture client at cwd `/`, with no provider
or grading. It also checks that protected root-owned material is unreadable by
that client identity, is stable across privileged read-only observations, and
changes its census fingerprint when the fixture controller edits its content.
It removes its successful container; a failed one is stopped and
preserved. `benchmark_live_sdk_session.py` additionally exercises the actual
original SDK shell and root cwd, using the installed Terminal SDK/catalog.
Both retain binary and all bundled Python source hashes. Neither is evidence of
AI solving, a full repeated workload run, or live-provider qualification.

Lifecycle events are sent through the collector interface, and an ordered,
bounded `lifecycle.json` journal is written for measured runs, including early
failures. Optional live Perfetto tests require a project-local installation and
working tracefs permissions: set `ASB_TEST_PERFETTO_TOOLS` to the project's
tools directory and run the ignored `live_perfetto` tests serially. They use an
offline replay fixture, not a live provider; ordinary CI does not require
privileged tracing.
`cargo test --locked --offline --lib disposition` checks the private operational
decision matrix and twelve isolated actual-runner cases. Pending evaluation
combined with collector, output, client, run or cleanup failure retains its
evaluator state but records failed run status. True/false grades and native
numeric rewards with null resolution survive broken streaming presentation.
The fixtures check log/measurement/report/return consistency, private outcome
records (not completed receipts), and exactly-once evaluation/workload/provider
cleanup. Existing checkpoint invariants continue to reject pending/error or
uncertain cleanup for sealing. No JSONL status field or public async evaluator
is added. `unknown_historical_run_categories_remain_display_only_and_unscored`
checks real report generation without rejecting extension strings.
`cargo test --locked --offline --lib evaluation_kind_contract_tests` checks
literal known-producer labels, borrowed unknown extensions, baseline/eligibility
separation and unchanged selection JSON. Run the existing
`workloads::contracts::baselines` tests alongside it: their five official
descriptor/protocol-fingerprint literal goldens must not be regenerated for a
type extraction. Native null-reward, historical-report, disposition, selection,
planning and checkpoint cases remain independent required regression families.
Build the actual binary first, as for the phase tests below.

Runner phase regressions use `cargo test --locked --offline --lib runner::phases`
and `cargo test --locked --offline --lib workloads::remote::execution_tests`.
`cargo test --locked --offline --lib sweep::` also runs the consuming sweep
phase regressions. Nine isolated actual-runner cases exercise sequential JSON
writer failure, completion of an admitted replica group, reuse without client
reruns, rejected changed execution mode, initial/history presentation failures,
ordinary benchmark failure continuing, and combined benchmark/presentation/
comparison or final-input-seal failures. They retain grades, task/plan metadata,
receipts, partial reports and unadmitted interrupted-output sentinels. Actual
bounded spool failure with a held sibling proves worker-side admission stop
and joined permit release; failed spool preservation/creation cannot claim
cleanup ownership. Sticky-writer tests distinguish transient Interrupted I/O
from permanent write/flush failure and WriteZero. Existing planning/resources,
native reward, false-grade and reconciliation invariant fixtures remain required.
These fixtures use only project-local programs and loopback replay; build the
current binary first and never rebuild it during runtime tests.
For a focused library-only invocation, run `cargo build --locked --offline`
first: the full-entry regression executes the actual ASB binary beside the
test executable as its native process gate. Ordinary full `cargo test` builds
that binary as part of its CLI integration targets. The full-entry fixture
runs isolated attempts through the real bundled grader, saved logs,
measurements and completion policy; the larger outcome matrix tests consuming
phase transitions with synthetic execution evidence, not full benchmark runs.
Its real blocked-log-directory cases retain JSON and measurement grades,
evaluate/finish exactly once, preserve primary and ancillary diagnostics and
publish no completion receipt. Existing directories are never overwritten.
They exercise independent client exit/grade/cleanup outcomes, failing lifecycle
markers before and after execution, actual child-output retention, native and
guest terminal-answer policy, incomplete recording/replay, explicit preservation
on interruption and final measurement-publication failures. A separate
`measurement_lifecycle_faults` test checks actual owned collector shutdown and
unrelated-process survival despite an earlier failing adapter or full journal.
The native-plan fixture verifies wrapper/endpoint/replay preparation without
executing an installed client. All fixtures are provider-free and unprivileged;
controller fixtures do not qualify Docker, SDK installation or a live benchmark.
`cargo test --locked --offline --lib evidence::` checks private staged writes,
short/Interrupted/zero/ENOSPC writes, serializer failures, file/directory sync
and rename faults, regular-file restrictions and coordinated real-process
writers. Measurement publication fixtures preserve malformed summary bytes and
unknown history metadata. `runner::phases::tests::publication` drives eleven
actual replay-only attempts with Boolean/native/pending grades, client and
presentation failure, partial log/measurement writes and post-commit durability
uncertainty. Client/evaluation/cleanup counts, identities, independent final
outputs and withheld receipts are checked together. Reports refuse stale or
malformed retained evidence instead of presenting it as the current attempt.
The pre-fix serializer, malformed-history and two-process interleaving failures
were observed before implementation; logs remain local qualification evidence.
Extended review fixtures reproduce the exact-bound unterminated log and raw
collector deletion before failed/unconfirmed publication. Stopped local child
fixtures require raw-byte retention, sibling exports and independent run/
lifecycle/summary finalization. Success consumes only identity-bound sources;
an adapter replacement, a deterministic check/move race and refused cleanup
retain foreign/owned bytes with actionable paths. Two additional actual entry
cases retain native/pending grades and exact-once evaluation/cleanup through
collector-result staging and post-commit sync failure, including broken output.
Platform-confirmation regressions exercise all three policy decisions on Linux:
only Unix invokes the directory-sync callback, Windows move flags exclude
copy/deferred operations, and a missing/uninspectable private source is never
called an unchanged precommit failure. A real Linux rename followed by an
injected native-outcome failure retains its staging directory without a Drop
cleanup/retry or a false published/unpublished claim. Existing short/zero/ENOSPC,
file-sync, postcommit confirmation and cleanup tests remain required. Windows
API declarations/semantics are source-reviewed only: no Windows target compile
or runtime was performed, and Linux policy tests do not qualify native Windows
rename behavior or portable directory durability.
`cargo test --locked --offline --lib runner::phases::tests::bundle_publication`
combines actual bundled recording/replay with private publication faults. Nine
isolated cases (eleven actual attempts including clean recording baselines) use
the same bounded synthetic loopback provider as `repeated_run`, fake Pi clients
and fixture-only keys. True/false and nullable native grades survive committed
recordings followed by short/precommit or postcommit log/measurement failures.
Combined recorder commit/publication failures retain primary client/grade facts,
failed recording disposition and raw bytes, and stop before another batch.
Selected seeded replay preserves track/algorithm/usage after source replacement
despite publication failure. Assertions check exact-once client/evaluate/workload/
provider-observer finish, independent final outputs, immutable association/raw
digests and absent completion receipts. Current measurement and summary schemas
retain the same optional repeated `execution` and `recording` objects as logs
and streamed summaries. Publication-fault qualification checks their agreement
alongside task/cohort/fingerprint/usage; legacy absent objects remain unknown.
No production fault switch, SDK, container, privileged collector or live LLM is
used. Build the current ASB executable first and keep it unchanged while testing.
The artifact-handoff test in the same module adds six actual native MiniSWE
record/replay cases without another provider implementation. The fake executable
writes only the adapter's actual current-private `--output` trajectory; its
stdout deliberately disagrees with the frozen answer and claims 999 tokens.
True/false grades must use the frozen answer, steps remain four, validated tape
usage stays authoritative at five tokens, and the exact retained 0600 trajectory
independently contains 34 tokens. Commit plus log/measurement failures and selected
private replay after source replacement must retain a locatable frozen copy,
consume only the original owned trajectory, finish/evaluate/publish once, stop
further admission and withhold unsafe receipts. This extends the source-approved
integration's offline evidence, not native Windows or original-SDK qualification.
The independent-offset fixture keeps its producer at offset five while full,
prefix and interleaved frozen readers run; this assertion also remains enabled
on Windows. An additional cross-platform source-replacement fixture must read
admitted A=17, never replacement B=99. Shared pure file-ID decisions compare all
128 bits and preserve unknown-ID semantics. A Windows-only future-native test
checks the reopened description is read-only. These tests do not constitute
native Windows execution in the Linux qualification; the ReOpenFile declaration,
sharing/access/inheritability flags and full ID layout are source-reviewed.
Human-terminal write/flush failures stop further presentation but finish an
already-admitted execution, evaluation and explicit cleanup; broken guest-owner
transport still cancels/preserves. Tests cover both policies. The elapsed clock
freezes at native supervised-child exit observation before collector/proxy
teardown; it does not claim an exact inner-client kernel timestamp. Guests keep
their explicit execution observation. These phase changes do not repair the
separately tracked stream-memory bounds or blocking owned-child wait limits.

Concurrent Perfetto captures use one manual `traced`/`traced_probes` service per
ASB process and exclusively lease a delegated named tracefs instance. All client
commands use the explicit `perfetto` applet; neither preflight nor capture uses
Tracebox autostart or `--reset-ftrace`. Configure optional delegation with
`asb init --configure-host` (existing project data is preserved). Init prints
every privileged command and requires y/N, or explicit `--yes` approval.
Two project-specific instances allow two independent ASB processes; replicas
inside one process share a service without serializing benchmark execution.
Install util-linux `unshare` and enable unprivileged user/mount namespaces for
the instance-only bootstrap. No permission to read the primary trace snapshot
is required. For ignored library tracing tests, first `cargo build --locked`
and set `ASB_TEST_PERFETTO_HELPER` to that built ASB executable: libtest itself
cannot execute ASB's hidden recursive helper. Production uses its own executable.
Private-service registration queries are bounded and use no provider/network.
Reconfigure after reboot. No tracing privileges or host mutation are needed by
ordinary CI: service fixtures use local Unix sockets and mock daemons, check
creator-thread exit, owner-process death, failed-client isolation, lease lifetime,
and bounded log draining. Actual overlapping captures remain an opt-in local
qualification; mock ownership tests do not prove real scheduler coverage.
The normal-shutdown lease regression deliberately keeps the same locked file
description in an independent pipe-blocked child. Once owned daemons are reaped,
the lease must be reacquirable while that incidental child remains alive. This
deterministically covers a fork-before-exec inheritance window without relying
on scheduling sleeps. Separate tests keep inherited ownership on uncertain
cleanup and retain the unchanged owner-SIGKILL daemon-shutdown contract.
The daemon-identity fixture separately waits for an atomically published,
child-authored post-exec PID/descriptor observation before checking absent
versus deliberately inherited lease descriptors. Process creation alone is
not that observation: a spawn fast path can resume its parent during exec,
before close-on-exec cleanup. Both live-child lease assertions and rejection
of the subsequently reaped child remain mandatory; no tracing service runs.
The ignored `live_perfetto_streaming_preserves_markers_beyond_the_service_ring_capacity`
test writes fixed, non-secret marker pressure beyond the configured 128 MiB service
buffer, then requires complete, loss-free capture and scoped agent measurements.
The ordinary offline coverage fixtures reject missing/duplicate boundaries,
data loss and malformed processor output. The ignored
`real_processor_validates_success_failure_and_duplicate_terminal_order` test
uses synthetic JSON traces and the installed trace processor without kernel
tracing; it checks normal completion, failed-then-finishing and failed-only
termination, rejecting reversed or duplicate terminal markers.
The separate ignored `real_trace_processor_verifies_synthetic_marker_capture`
test uses the same tool path but only reads a synthetic trace; it does not
start tracing or require tracing privileges.

State-summary tests execute the actual materialized SQL through Python's
standard-library SQLite: over 5,000 intervals, left/right clipping, unrelated
roots, runnable versus on-CPU states, unknown I/O classification and excluded
unfinished intervals. Aggregate totals do not depend on the 4,096-interval/
64-thread SVG view. Malformed or incomplete aggregate output publishes no
partial totals. The ignored `installed_processor_aggregates_retained_scoped_trace`
test accepts `ASB_TEST_PERFETTO_TOOLS`, `ASB_TEST_PERFETTO_SUMMARY_TRACE`,
`ASB_TEST_PERFETTO_SUMMARY_PID`, and `ASB_TEST_PERFETTO_SUMMARY_ID` for explicit
local verification against retained native captures. It never starts tracing,
an agent or a provider, and writes a separate local summary instead of changing
the retained source results. It expects a fork-heavy qualification trace with
more than 4,096 observed intervals and unavailable I/O-wait classification.

Each completed run records `process_success` separately from
`workload_success`. Legacy external workload aliases install a pinned reference
plus a small deterministic adapter trial and local structural checker, not upstream
dataset instances. Even a passing adapter smoke check is unscored for benchmark
task resolution (`success_rate = null`, `workload_success = null`); its separate
`adapter_smoke_success` records only that exercise. Actual local task attempts
are scored by their evaluator, never by process exit alone. See
[the full benchmark integration ledger](FULL_BENCHMARK_INTEGRATION.md) for real SWE-bench and versioned
Terminal-Bench sessions; they use their official upstream evaluators instead
of the legacy structural checker. Optional runtime qualification scripts
`benchmark_live_swebench.py`, `benchmark_live_terminal_1.py`, and
`benchmark_live_harbor.py` need explicit local dataset/SDK/Docker opt-in, use
known fixture submissions and never contact a live LLM provider. The separate
`benchmark_live_swebench_source.py` qualification also requires
`ASB_TEST_SWE_SOURCE_ARCHIVE` pointing to the pinned source archive installed
by `asb workload install`. It forces only its own prebuilt lookup/pull to fail,
builds an original Dockerfile under the production CPU/memory/time limits,
grades known submissions, and checks cleanup without deleting shared images.
It requires current Docker Buildx/BuildKit with per-build `--resource` support.
It additionally verifies declared source-image ownership, per-attempt tag
cleanup, original-image preservation and official summaries without host-wide
Docker inventories. The original Terminal-Bench qualifications check owned
Compose build labels and removal; set `ASB_TEST_HARBOR_FORCE_FALLBACK=1` to
exercise original 2.1 source fallback instead of using its prebuilt image.
`benchmark_live_parallel.py` uses the normal `ASB_TEST_BENCHMARK_ASB` and
`ASB_TEST_SWECATALOG_ROOT` opt-ins plus installed Pi, SWE-bench Lite and original
Terminal-Bench 1.0. A loopback fixture barrier requires both real clients to
reach it before either can receive its known repair. The test checks independent
official grades, lifecycle journals, scoped collectors, comparison graphs and
container cleanup. It never contacts a live LLM provider. Do not rebuild or
package over the selected executable while a runtime qualification is using it.
The same fixture removes completion receipts only after successful grading and
cleanup, verifies that ordinary resume refuses the missing evidence, injects a
contradictory retained grade and checks that explicit reconciliation publishes
no receipts, then restores the original report and reconciles both outcomes
without further provider requests. `benchmark_retained.py` supplies the
provider-free retained-artifact protocol tests for all five adapters.
Repeat the mixed original-task qualification with
`ASB_TEST_BENCHMARK_MEASUREMENTS=disabled` to omit collection for SWE-bench and
explicitly disable it for Terminal-Bench. It must still report official grades,
coverage, reconciliation provenance and success-rate graphs, without creating
measurement directories. The ordinary provider-free `unmeasured_sweep` CLI
test separately checks default/disabled collection, nonzero agent exits with
valid grades, unresolved tasks, comparison regeneration and cached resume.
Ordinary CI
uses synthetic provider-free protocol tests.
The remote execution unit tests use a private prepared-bootstrap fixture to
exercise controller dispatch, output, metadata, lifecycle failures, deadlines and
preserving cancellation independently of installation. This fixture does not
qualify an installed runtime, SDK or workload container. Production execution
always validates the runtime and performs checksum-pinned bootstrap preparation;
there is no CLI, configuration or environment switch to bypass that phase.
`cargo test --locked --offline --lib workloads::task_protocol` covers private
ready/completion/grade constructors, delegated namespace-owner mutations,
normal versus preserving cleanup acknowledgements and exact Rust/Python wire
identifiers, including initial-material dispatch and preparation-failure events.
Shared-launch fixtures require the typed agent request to preserve its remaining
preparation allowance and notify peers before cancellation. Positive preserved
settlement is cached independently of grade permission; failed normal removal
retains its diagnostic without fabricating unknown cleanup. Remote execution
fixtures additionally pair the actual Python
task-server loop with the actual Rust controller/session; only SDK/agent work
is replaced by an inert process-local fixture. Both grades retain a nonzero
client exit and binary output, then consume normal-close permission. A second
bad grade or evaluator exception revokes the first grade's permission and
requires stopped preservation without normal close. Generic controller tests
retain untagged inspection, sanitized errors, framing limits and cancellation
draining, including unknown in-flight events. These are offline protocol tests,
not original SDK/container qualification.
`cargo test --locked --offline --lib workloads::controller::transport_tests`
qualifies the Unix caller-side transport with actual non-reading children,
duplex pipe pressure, bounded queued events, exact/oversized/unterminated frames,
one write/response deadline and one cleanup acknowledgement/exit deadline.
A Linux fixture retains only its own child's stdout writer through `/proc`,
then proves Controller destruction releases the parent read descriptor without
waiting for that writer. It creates no orphan descendant. Fault fixtures pin
the direct child with a pidfd, force pending/error exit observation and unwind
after admission, require the pre-established keeper to reap it and leave an
unrelated sentinel alive. Worker creation failure launches no controller; pipe
setup failure remains inside the existing custody guard. No fixture needs a
genuinely kernel-stuck task. Python fake-monotonic-clock tests account for delayed
polls and reject a late queued close instead of authorizing teardown. These
checks do not qualify Docker resources, arbitrary descendants, OS spawn latency
or the retained legacy non-Unix blocking transport.
`benchmark_live_resources.py` additionally checks actual abrupt controller
SIGKILL with an installed SWE-bench task image: the durable ownership journal
quiesces only its recorded container, retains a partial output file and an
attempt-owned committed image, and leaves a simultaneously running unrelated
fixture container and shared original image untouched. The image commit checks
the daemon ownership/recovery contract, not interrupted BuildKit internals.
Run it with
the evaluator virtual environment and explicit `ASB_TEST_SWECATALOG_ROOT`.
Artifacts are retained under that project; fixture containers are removed only
after the preservation assertions. This check does not authorize a new
benchmark attempt, assign a task grade or prove absence of in-flight daemon
creation operations. Ordinary CI covers the same ownership/locking contracts
with standard-library fixtures and requires no Docker access.
`benchmark_live_recovery.py` exercises the public CLI rather than constructing
only a resource journal. With `ASB_TEST_BENCHMARK_ASB` and
`ASB_TEST_SWECATALOG_ROOT`, it starts a cached original SWE-bench Lite attempt
through ASB with replay-only provider transport, locates its durable reference,
pins the exact controller child with a pidfd, kills it, and calls
`asb workload recover REFERENCE --json`. It verifies stopped/preserved partial
files and the absence of a fabricated submission, grade or replacement attempt.
The ordinary mixed-sweep qualification additionally checks that each reference
matches its replica's original start intent and completion seal. No test calls
a live LLM provider.

`benchmark_live_cold_install.py` checks public-source installation into a new
project with empty tool, home and package/download caches. Set
`ASB_TEST_BENCHMARK_ASB` to the compiled binary and
`ASB_TEST_COLD_INSTALL_ROOT` to an existing project-local artifact parent. It
initializes without approving host changes, installs all five full original
catalogs with their SDK dependencies, installs Pi and checks its executable.
Logs and bounded population/step receipts remain under the printed project.
Without both opt-ins the test skips; ordinary CI never downloads these SDKs.
Host prerequisites and Docker images are not removed, so this is cold-project
installation qualification, not a completely cold host or benchmark score.
After it passes, the printed project can be used as
`ASB_TEST_SWECATALOG_ROOT` for `benchmark_live_parallel.py`; use that project's
installed SWE-bench Python interpreter. This exercises the newly installed SDKs,
catalogs and client with known repairs from a synthetic provider, without live
LLM credentials.

`benchmark_live_interrupted.py` checks interrupted teardown against original
task environments and official false grades. Set `ASB_TEST_SWECATALOG_ROOT`
and `ASB_TEST_INTERRUPTED_WORKLOADS` (comma-separated original workload IDs),
then run it with the corresponding installed SDK's Python interpreter. Run
the three SWE-bench IDs together with the SWE-bench interpreter, and each
Terminal-Bench version with its own tool interpreter. It verifies stopped
containers, readable partial files, unchanged submissions/official grades and
preserved build images, then removes only the positively-owned fixture
resources. It never invokes an LLM. `benchmark_live_task_server.py` additionally
checks real EOF, SIGTERM and interruption during official evaluation; normal
completion still removes resources, while interrupted attempts retain them.
These are retention/teardown checks, not recovered grading or retry receipts.

`benchmark_live_inspection.py` qualifies public `workload recover --inspect-grade`
against retained original SDK fixture reports without running Docker, an agent,
provider or evaluator. Set `ASB_TEST_BENCHMARK_ASB`, `ASB_TEST_SWECATALOG_ROOT`
and `ASB_TEST_GRADE_INSPECTION_SOURCES`: a JSON object mapping all five workload
IDs to `{"artifacts":"/absolute/retained/task","resolved":false}` (or the
fixture's known true grade). It copies completed fixture evidence under new
canonical references, checks the CLI report and unchanged source/copy/SDK
bytes, and retains the qualification directory. The copy is not claimed to
be a past ASB execution or historical attestation. Ordinary provider-free
`benchmark_retained.py` additionally exercises all five CLI adapter routes,
active-owner refusal, malformed/absent grades, unknown execution fields and
temporary-copy cleanup. The mixed real-task qualification inspects both
completed grades before resume, proving that inspection does not invalidate
the original receipts or start another client.

`benchmark_import.py` exercises public `sweep --import-grades` and comparison
for all five original adapters using provider-free retained artifacts and SDK
metadata fixtures. It checks true/false grades, multi-replica mappings, unknown
execution metrics, unchanged retries, late-candidate failure before publication,
active owners, conflicting outcomes, duplicate attempts, original intent/config
mismatch and byte-level evidence edits. Ordinary resume cannot use imports as
completion evidence. The mixed original-task qualification temporarily archives
its own outcomes/measurements, imports both real official grades, regenerates
SVG/HTML without new provider requests, then restores the original evidence.

`benchmark_grading.py` covers explicit retained-patch grading for all three
original SWE-bench populations with provider-free evaluator/SDK seams. It checks
both grades, immutable submissions, preservation of completed grades and failed
predecessors, malformed/missing ownership, active prior graders, cancellation,
catalog/patch mismatch, explicit evaluator selection and clean helper shutdown
while stdin remains open. EOF and SIGTERM cancellation do not rerun agents.
The same suite covers raw official-outcome reconstruction for both grades in
all three populations, missing/failed wrappers, active owners, incomplete or
changed reports/inputs, changed SDK/image, interrupted metadata publication,
and source changes after reconstruction. This path never starts Docker or an
evaluator. Import/compare additionally binds reconstructed outcomes to the
original raw bytes, not just copied reports.

Grade-only closure contracts are tested in `sweep::checkpoint::completed::retained`
and `runner::reuse`: both grades, unknown execution observations, duplicate
attempts, changed original bindings, changed preserved outputs and validation
of all candidates before publication. Reporting fixtures cover all five
adapters and ignore incomplete execution measurements after explicit closure.
The mixed original-task qualification also archives only its own ordinary
completion records and seals, then closes both slots through public
`--resume --reconcile-completed`, checks unchanged repeats without new provider
requests and regenerates honest grade-only comparison output. These tests
must not promote a recovered grade to measured process success.
The completed-outcome invariant matrix starts from the current schema-2
retained fixture with original start intents and allocations, mutates each
protocol, identity, grade, cleanup and path obligation independently, and
requires sealing, continuation and reconciliation to reject without publishing
any group or replica receipt. Wire fixtures retain schema-1 compatibility and
reject unknown/malformed records; the unmeasured sweep CLI fixture additionally
checks invalid groups cannot start another synthetic client or change receipts.
That public CLI check also removes only the late invalid member's group and
slot receipts, requires its exact invariant diagnostic before allocation
inspection, and checks no replacement receipt or changed outcome bytes. Valid
earlier/sibling receipts stay unchanged. The allocated unit fixture separately
proves all-candidate validation precedes publication; neither fixture qualifies
an original task runtime.
Structured-outcome checkpoint fixtures additionally seal completed native
rewards with null resolution and both process results, preserving legacy
structured/unstructured Boolean grades. Independent mutations reject pending,
error, missing primary rewards, policy/module, lineage, process/resolution,
cleanup and evaluator-root mismatches through the same validated constructor.

An opt-in complete-catalog planning/reporting check runs with no agent,
provider, Docker or evaluator execution:

```sh
ASB_TEST_SWECATALOG_ROOT=/absolute/installed/benchmark-project cargo test --locked --offline --lib installed_original_populations_plan_and_report_all_instances_without_execution -- --ignored --nocapture
```

It validates installed catalogs through owning adapters, writes and reloads a
3,263-entry selection plan, generates the comparison and checks all five exact
population denominators. Every attempt is reported unevaluated and every full
population resolution score remains null. The printed project-local directory
retains its artifacts. This proves scale/selection/reporting, not AI task solving
or exhaustive evaluation of all upstream task environments.
The opt-in `benchmark_live_grading.py` uses the installed original catalogs/SDK,
Docker and `ASB_TEST_BENCHMARK_ASB`/`ASB_TEST_SWECATALOG_ROOT`. It captures and
preserves actual patches without grading, then invokes public `--grade` for
unresolved cases in all three populations and a known-correct Lite patch. It
checks unchanged repeat requests/inspection, original task containers remaining
stopped, unknown execution metrics and positively-owned fixture cleanup. No LLM
is used, and this is not full-population or AI-solving qualification. Do not
rebuild the selected binary while this qualification is running.
After each real grade the fixture now archives only its wrapper/selection to
simulate lost ASB publication, leaves all official SDK files untouched, and
requires public reconstruction to succeed with a deliberately unavailable
Docker endpoint. Repeat grading/inspection must retain the reconstructed
result, original raw bytes and stopped task without another evaluation. This
is controlled fixture fault injection, not proof of every possible crash gap.

`benchmark_terminal_grading.py` supplies provider-free original 1.0 SDK seams
for retained true/false grading, completed-grade preservation, definition/capture
mismatch, lost-shell refusal, non-retry of side-effecting tests, stopped cleanup
after SDK failure and network alias/static-address restoration. Shared resource
tests check exact ownership/identity, preservation and owner cancellation without
touching unrelated containers. Retained-artifact tests also reject the wrong
environment baseline kind/revision/hash for both original Terminal versions.
`benchmark_import.py` covers importing a recovered 1.0 grade into comparison and
rejecting edited cleanup evidence without replacing the import.

The opt-in `benchmark_live_terminal_grading.py` needs the same explicit ASB/project
variables as SWE grading and the installed original Terminal-Bench Python. It
captures known incorrect/correct hello-world states, preserves their containers,
calls public `--grade`, checks the official SDK result, retained files, stopped
resources, unchanged repeat calls/inspection and unknown execution metrics. The
incorrect case first moves its owned main container to a new qualification-only
network, proving recovery reconnects original aliases and removes that override
connection without deleting unrelated networks. The test removes only its
positively-owned fixture resources/network and retains artifacts. No LLM or agent
is used. Representative cases do not establish full-population AI performance or
reconstruct lost shell state. Normal mixed-sweep and Harbor SDK qualifications
remain regression gates when changing shared network/metadata behavior.

`benchmark_harbor_grading.py` covers original 2.1 first-verifier retained grading
through provider-free SDK seams, true/false grade preservation, cancellation,
changed catalog/definition/capture, non-retry markers, immutable service IDs,
bounded output and full-pipe subprocess teardown. Both original Terminal
versions are covered by recovered-grade import/compare checks.
The opt-in `benchmark_live_harbor_grading.py` uses the same ASB/project variables
and the project's Harbor SDK interpreter. It captures incorrect and known-correct
`cancel-async-tasks` states without grading, preserves the original containers,
calls public `workload recover --grade`, and checks official rewards, unchanged
submissions/repeat calls/inspection, stopped resources and unknown execution
metrics. Only positively-owned fixture resources are removed afterward. It
never calls an agent or live provider and does not prove full-population AI
performance or reconstruction of a verifier that already started.

`benchmark_verifier_reconstruction.py` covers both original SDKs' durable return
records and metadata-only recovery. It checks normal/recovered evaluator
locations, missing/failed wrappers, both grades, source tampering, cancellation,
missing observations/cleanup and interrupted copy/selection. SDK-return facts
must match the raw artifacts; no SDK, Docker or provider starts during recovery.
Both public retained-grading qualifications now archive only wrapper/selection
after actual SDK completion and require recovery with Docker deliberately
unavailable. Original bytes, stopped state and repeated grading/inspection must
stay unchanged. This fault injection cannot recover a grade never observed or
published by the SDK adapter; such outcomes remain unknown and are not retried
implicitly. Import/compare tests also validate these source bindings.

See
[evaluation semantics](EVALUATION.md). Perfetto
post-processing scopes scheduler counts and series to the benchmark process
tree and lifecycle interval; it never compares host-wide scheduler noise.
On Linux, measured commands run through a process gate that is also a child
subreaper. This retains helpers that daemonize or outlive an intermediate
wrapper under the benchmark root, so perf inheritance, bpftrace fork tracking,
pidstat sampling, and process-accounted timing include the complete relevant
tree without collecting unrelated host processes.
Changes to native custody must preserve the private Hello/Quiescent protocol,
Starting-versus-Admitted cleanup distinction, stopped-owner-loss watcher and
wait-only fallback ownership. Build the actual current `asb` binary before
running the `process_gate` library tests: those tests exercise the real gate,
not the libtest executable. Run the `guest_agent` native-gate matrix as well;
it covers owner loss after validated Hello, while stopped, after resume and
after a short-lived spawning thread exits, with double-forked TERM-resistant
writers and an unrelated sentinel. Protocol stand-ins test malformed/absent
receipts and bounded fallback custody only; they do not qualify kernel
descendant teardown or a live provider. No root permissions are required.
Launch-descriptor regressions use the actual current ASB binary with offline
local programs. They cover cleared/removed/overridden environments, raw OS
argument/environment bytes, explicit client argv[0], child-only PATH, working
directory, stdin/stdout/stderr, current UID/GID, an owned process group and an
existing async-signal-safe configuration callback. NUL inputs fail before
spawn; missing clients cannot become success. The original binary-replacement,
guest-wrapper and owner-loss matrices remain required. The previous valid
ASBGATE1 Hello is explicitly rejected without resuming the old runtime.
Full-entry gate-route regressions replace the carrier, shorten/change framing
or retarget its executable; every case must fail before a child or reaper is
created. Validation consumes the one-shot route, and admission exclusively
borrows that configured command until spawn. Create a fresh launch command for
another attempt rather than reusing pre-exec descriptor callbacks.
Repeated native admission retains this descriptor route: tests separately inspect
the actual client (not gate framing), verify fresh adapter state and executable
precedence, wait for siblings without resuming the first target, and cancel a
verified stopped guardian without ever launching its application. The selected
execution clock still starts only after shared readiness releases the batch.
Do not reconstruct an opaque `Command` from `get_envs`/`get_args`: those APIs
do not reveal environment clearing, credentials, I/O and arbitrary callbacks.
Rust 1.93 runs pre-exec callbacks before applying its prepared environment;
see the [pinned execution order](https://github.com/rust-lang/rust/blob/1.93.0/library/std/src/sys/process/unix/unix.rs#L383-L408).
The guest driver likewise owns a private child supervisor. By default both
supervisors stop and reap leftover helpers before accepting an execution result.
An adapter may instead delegate guest service lifetime through evaluation to
its owned private PID namespace. Principal exit ends execution accounting;
bounded private metadata confirms the handoff, output pipes remain drained,
and SDK teardown stops/removes services only after grading. Native and SWE-bench
execution retain default quiescence. The opt-in `benchmark_live_lifetime.py`
qualification uses the original Jupyter task, a known reference solution through
a synthetic client and the official verifier. It requires
`ASB_TEST_BENCHMARK_ASB`/`ASB_TEST_SWECATALOG_ROOT` and the original Terminal SDK
interpreter; it never calls a live LLM. This qualifies service lifetime, not AI
solving or preservation of an agent's interactive shell variables. Offline
guest fixtures exercise double forks, ignored TERM, inherited output pipes,
nonzero exits and output-consumer failure, and verify that unrelated fixture
processes survive. The optional `benchmark_live_quiescence.py` qualification
uses the installed SWE-bench fixture and explicit
`ASB_TEST_BENCHMARK_ASB`/`ASB_TEST_SWECATALOG_ROOT` paths to check the actual
Docker kernel API and filesystem-preserving reset path. It starts only a
synthetic client and never contacts an LLM provider.

Two opt-in host checks exercise the accounting and BPF paths without a provider:

```sh
systemd-run --user --scope -p Delegate=yes -- env CARGO_NET_OFFLINE=true cargo test --offline --locked --test offline_cli live_delegated_cgroup_replay_keeps_exited_child_cpu -- --ignored --nocapture
ASB_TEST_BPFTRACE=/usr/bin/bpftrace cargo test --offline --locked --test offline_cli live_bpftrace_socket_profile_captures_replay_and_generates_graphs -- --ignored --nocapture
```

The cgroup test checks two concurrent replicas, exited CPU children, published
component counters and leaf cleanup. The BPF test needs an installed bpftrace
and existing tracepoint permissions or passwordless sudo. It checks actual
socket syscall counts and the generated SVG/HTML comparison from loopback
replay. The additional `io-wait` profile needs enabled scheduler statistics;
preflight rejects a disabled or unavailable host setting with a copyable
command. Tests never change this host-wide setting.

### Custom BPFTrace program contract

Built-in cumulative time series use complete timestamp frames. A custom
interval handler can opt into the same protocol:

```bpftrace
interval:s:1 {
  printf("ASB_BPFTRACE_SNAPSHOT %llu\n", nsecs(monotonic));
  print(@asb_calls);
  printf("ASB_BPFTRACE_SNAPSHOT_END\n");
}
```

Use `nsecs(monotonic)` explicitly: the pinned bpftrace defaults plain `nsecs`
to the boot clock, which can differ after suspend. Frame timestamps must
strictly increase. Each key retains only its own observed timestamps; absent
keys remain gaps. Nested/incomplete/malformed frames fail collection instead
of publishing partial series. Unframed maps remain scalar-only. bpftrace
prints maps asynchronously by reference, so these are measured callback
timestamps, not atomic map snapshots; timing JSON discloses that limitation.
The ignored socket-profile replay test holds a synthetic local client for
multiple intervals and checks actual native timestamps, scalar counts and
comparison graphs. It never contacts a provider and never changes host settings.

Use `ASB_MEASURE_BPFTRACE=1` and
`ASB_BPFTRACE_PROGRAMS='/absolute/program.bt?ASB_PARAM_LIMIT=12'` when opting into
custom programs. Separate at most eight programs with `;` and at most 32
parameters per program with `&`. Specifications are limited to 16 KiB, each
regular UTF-8 source to 256 KiB, and the combined source bodies to 1 MiB.
For example, a program body can contain:

```bpftrace
tracepoint:raw_syscalls:sys_enter /@asb_tree[pid]/ {
  if (@asb_calls < ASB_PARAM_LIMIT) { @asb_calls = count(); }
}
```

Parameters must be `ASB_PARAM_` followed by an uppercase ASCII identifier
(at most 64 characters total). Values are canonical decimal integers from zero
through 9223372036854775807: no signs, leading zeros, expressions or strings.
Rename old parameters such as `limit=12` to `ASB_PARAM_LIMIT=12` in both the
configuration and source. Duplicate, missing and unused parameters fail
preparation. Substitution changes only complete identifier tokens, never
comments, strings, probe provider/path/symbol fields, map/variable names or
member accesses. ASB does not pass `-D`
or accept custom preprocessor directives.

Each process-probe predicate must positively constrain an ASB-maintained tree
key (`pid`, `tid`, or supported scheduler `args` PID fields). Every OR branch
must retain scope; additional AND filters are allowed. Predicate assignments,
increments/decrements and unsupported calls are rejected before scope proof.
The read-only `str` and `strncmp` filters are supported. Custom actions cannot
reference reserved `@asb_tree`; control handlers may aggregate existing results
but cannot replace process-scoped probes. This validates process scope, not
arbitrary action safety; custom programs are trusted code, especially under sudo.

The source is read and validated once at preflight, then retained through
configuration and attachment, even if the original file changes or disappears.
Attachment checks startup readiness before resuming the benchmarked agent.
Offline tests cover mutations, malformed/multiline/UTF-8 sources, numeric
binding, and replacement/deletion after preflight. When `/usr/bin/bpftrace` is
installed, the unprivileged `-e SOURCE -V` regression checks the real CLI option
route without `-D`; an absent default executable is reported as an explicit
skip. `ASB_TEST_BPFTRACE` can select another executable (an invalid explicit path
fails). This options-only test does not claim script compilation or kernel
attachment. The opt-in privileged tests above cover actual attachment.

For the installed bpftrace 0.20.2, source inspection confirms that `BEGIN`
executes before the process probes attach, but its buffered `printf` is consumed
by `poll_output` only after both attachment loops succeed. ASB waits for the
observed output, not merely execution of `BEGIN`. See the
[upstream run sequence](https://github.com/bpftrace/bpftrace/blob/v0.20.2/src/bpftrace.cpp#L1207-L1284).
This version-specific source evidence is not a substitute for opt-in live
attachment qualification on a different tool or kernel.

Custom startup establishes the shared private `OwnedReaper` before spawning.
Readiness failure or unwinding kills the owned child and polls against one
five-second cleanup deadline; uncertain exit transfers custody to that worker
and preserves the startup error alongside an explicit cleanup diagnostic.
Readiness success transfers the still-owned child to the collector session.
Real-child fixtures check failure before spawn, success transfer, pidfd-pinned
reaping, unrelated sentinel survival, poll/deadline handoff and unwinding.
This bounds the caller, not a kernel-stuck process's actual exit. Other built-in
collector startup/normal-stop paths retain their existing lifecycle policies;
this change does not claim they all have bounded cleanup.
