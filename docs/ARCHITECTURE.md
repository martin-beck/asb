# Architecture

The maintained overview below explains ASB's runtime boundaries. It is
supplemented—not replaced—by the generated [Rust type diagram](asb-type-diagram.mmd)
and its [SVG rendering](asb-type-diagram.svg). Regenerate both with `make uml`.
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
  the mpstat/iostat result names use process-scoped `pidstat` sampling for the
  agent process and its child tasks, retaining bounded raw output and exposing
  averaged, unit-named CPU and I/O metrics under a machine-readable `metrics`
  object plus bounded per-sample `series` arrays. `perf stat` uses the same
  interval contract. `ASB_MEASURE_INTERVAL_MS` accepts 100--60000 ms and
  defaults to 1000 ms; pidstat rounds sub-second requests to whole seconds,
  while perf supports millisecond periods. Comparisons emit line SVGs for
  series with at least two points and expose the same hover, zoom, pan, reset,
  and enlarge controls as bar charts. Built-in BPFTrace profiles remain
  bounded cumulative one-second snapshots and final aggregates (custom
  interval programs may add series), while Perfetto retains its timestamped
  trace and extracts timestamped scheduler-event and scheduler-duration
  series; those trace timestamps are used as line-chart x coordinates;
  the `system` record also captures bounded host-comparison metadata: CPU model,
  vendor and Arm implementer/part, package/core/thread topology, frequency
  governors and limits, NUMA node count, VM/container classification, compiler,
  runtime and selected collector versions, and relevant kernel settings. It
  records only sanitized values (never raw proc/sysfs/cgroup text, command
  paths, credentials, or provider data) and omits unavailable fields.
  Every measured run also writes `components.json`: lifecycle component wall
  times plus process-accounted user/system CPU time, block-I/O delay, and
  scheduler run-queue wait for the agent execution component. Network wait is
  represented as `null` unless a collector can observe it directly; ASB never
  assigns residual wall time to a guessed category.
  the Perfetto adapter validates tracefs/ftrace access and starts project-local
  Tracebox before preparation begins. It waits for recording readiness before
  emitting fixed, non-secret lifecycle tracepoints; after the agent is spawned,
  it scopes scheduler analysis to that PID and its descendants. Each concurrent
  run has a distinct trace identity. The project-local trace processor
  extracts stable aggregates,
  and ASB preserves the native `.perfetto-trace`, a bounded JSON record, and
  one portable SVG per extracted metric for the comparison report;
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

The CLI is the stable user boundary. A benchmark configuration names an agent,
workload, provider, model, concurrency, workspace, and log file. `concurrency`
defaults to `1`; values greater than one run isolated replicas of the same
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
their sum because cached input is a subset of input. OpenHands and OpenJiuwen are invoked in
machine-readable JSON modes; mini-SWE writes a project-local trajectory that
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

`asb tool install` installs a pinned package recipe into the configured tools
directory for supported agents and support tools, or downloads/copies an
executable supplied with `--source`. Record/replay instead uses ASB's embedded
tape proxy automatically and has no tool-catalog entry. npm recipes are
project-local; Python recipes use a
project-local virtual environment. `asb workload install` downloads a prompt
URL, resolves a pinned external workload alias, or copies a local prompt into
the configured workloads directory. The `swe-bench` and `terminal-bench`
aliases download pinned upstream source archives into local bundle directories;
the runner copies those sources into `external-workload/` in the workspace.
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
The `perfetto` recipe downloads SHA-256-pinned official Tracebox and trace
processor binaries into the project tool directory. It supports Linux x86_64
and aarch64. Ftrace itself is a tracefs interface provided by the running host
kernel, rather than an executable to copy into the project. Before a selected
collector can start, ASB probes that exact interface; it retries the collector
through `sudo -n` only when the direct probe fails, and rejects the run before
the agent starts when the needed passwordless authorization is absent. Normal
commands and CI do not require kernel tracing access.
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
ASB opens the marker as the benchmark user during preflight: sudo access for
Tracebox does not give ASB permission to write lifecycle markers. After
Tracebox reports readiness, ASB checks the matching `tracing_on` if readable
and writes a non-secret, run-specific startup marker before starting the agent.
It does not enable or disable host-wide tracing. An inactive trace or failed
marker write stops the run with a tracing-specific diagnostic. After recording,
the trace processor must find the startup marker in the trace; missing markers
are reported as an incomplete trace rather than silently accepted. A permission
failure provides a systemd
tmpfiles command that assigns the marker to the current user with mode 0600;
this replaces the marker's previous ownership and permissions. It does not
grant parent-directory traversal or override MAC policy, and a later tracefs
remount may reset the permissions. The startup marker can appear in other active traces.
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

`asb compare SWEEP.toml` reads the isolated `<sweep>-results/` directories
created by `asb sweep`. It writes bounded `comparison.json` data, an offline
`report.html`, and SVG charts to `results/<sweep>-comparison/`; no provider or
agent is contacted. The report links every result artifact and explains the
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
