# Development

Keep the project small and offline-buildable after dependencies are installed.
New behavior belongs in one of the three CLI modules and should preserve the
existing `setup` and `run` command forms.

Keep adapter-specific unit tests in the owning adapter module (or beside its
implementation helper). Shared modules keep registry, dispatch, and
cross-adapter tests; provider preparation cases belong to their providers.

Use the pinned Rust toolchain and run:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked
cargo build --locked
make rust-doc
make uml
```

`make rust-doc` runs `cargo doc --locked --no-deps --document-private-items`,
because the CLI implementation modules are intentionally private. `make uml`
installs the pinned AutoUML release under `target/tools/`, regenerates
`docs/asb-type-diagram.mmd`, normalizes AutoUML's source-path identifiers into
stable logical names, and renders `docs/asb-type-diagram.svg`. The normalized
diagram explicitly shows trait implementation edges and never publishes local
paths or branch names. It supplements the maintained architecture overview;
review both when changing public or private type relationships.

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
`cargo test --locked --offline --test compare_setup` checks repeated CLI setup
selections, scoped models/trials, dependency expansion, non-overwriting sweep
publication, standalone measured/unmeasured reports and an injected browser
opener. Browser tests do not launch a real GUI or contact a provider.
`cargo test --locked --offline --test report_filters` executes the embedded
report JavaScript under Node with an injected DOM fixture: every filter
dimension, combined intersections, replica identities, select/clear all,
empty charts, hover cleanup and selection restoration. SVG-generation tests
also check that bars, series/legends and composition rows carry benchmark groups.
Reserved-`all` tests cover complete registry expansion, enabled/original trial
populations, catalog-name collisions and provider-scoped model discovery using
only a bounded loopback HTTP fixture with no real provider credentials.
`cargo test --locked cassette::` exercises the provider-free multi-track backend:
manifest and compatibility bounds, stable seeded choices, selector edge cases,
bounded metadata inspection, selective corruption checks and durable publication.
Journal tests inject failures around atomic metadata writes/syncs/links; finalizer
tests cover ZIP output bounds, freeze/preparation/final-link/confirmation faults,
individual cleanup boundaries, exact-candidate recovery and foreign targets.
An owned test child is actually killed at recording/prepared/linked/published
stages, checking active-owner exclusion and recovery without new attempts. No
agent/provider/container runs. Fault hooks exist only in test builds and have no
environment/configuration surface in the installed CLI. The backend does not yet
expose new run/sweep flags; integration follows MULTITRACK.md.
The cli2key concurrency regression uses two mock bridge listeners and mock
clients, with fixture-only credentials. Both authenticated local requests must
overlap and reach different owned servers; both servers must be reaped afterward.
It does not use the installed cli2key package or a backend subscription.
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

Lifecycle events are sent through the collector interface, and an ordered,
bounded `lifecycle.json` journal is written for measured runs, including early
failures. Optional live Perfetto tests require a project-local installation and
working tracefs permissions: set `ASB_TEST_PERFETTO_TOOLS` to the project's
tools directory and run the ignored `live_perfetto` tests serially. They use an
offline replay fixture, not a live provider; ordinary CI does not require
privileged tracing.
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
