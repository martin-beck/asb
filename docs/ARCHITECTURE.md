# Architecture

ASB is deliberately split into a small CLI and a file-backed workload directory:

- `benchmark`: the configuration record and workload prompt catalogue;
- `config`: the `setup` command and TOML writer;
- `tui`: the Ratatui setup wizard. It composes reusable selection panes for
  agents, providers, models, workloads, and measurements, with filtering and
  keyboard navigation. Its model pane uses the existing provider discovery
  path and falls back only when a catalog is unavailable, then writes test
  configurations and a sweep manifest;
- `install`: local copying of agent executables and workload prompt files;
- `providers`: a concise provider-name and environment-variable registry;
- `models`: bounded live model-catalog discovery using provider-specific
  endpoints and response shapes;
- `prepare`: provider-specific, pre-launch environment preparation hooks;
- `project`: `.asb/project.toml` and shared project paths. `init` creates the
  file, copies the bundled workload prompts and fixture trees, and creates its
  configured `tools_dir`, `workloads_dir`, and `workspace_dir`;
  every command resolves relative paths from this project root;
- `runner`: configuration loading, bounded agent-output logging, and live agent
  process execution;
- `measurements`: opt-in system/runtime collection. It writes bounded JSON
  records for host facts, elapsed time, success rate, and normalized input,
  output, cached, and total token usage, and
  owns optional `mpstat`, `iostat`, `perf`, BPFTrace, and FlameGraph adapters;
  each adapter is isolated from the runner and can be extended without adding
  provider-specific fields to the benchmark configuration;
- `workloads/`: one UTF-8 `.txt` prompt per workload name. Built-in supporting
  files are stored under `.asb/fixtures/<name>/` and materialized into the run
  workspace without overwriting existing edits.

The CLI is the stable user boundary. A benchmark configuration names an agent,
workload, provider, model, workspace, and log file. Provider credentials are never stored
in that file. The runner passes provider/model selection through environment
variables and gives the selected agent one bounded workload prompt.
Provider credentials are not stored in benchmark configuration. Known providers
document conventional credential and endpoint variable names; model discovery
and child launch use the environment first and then a bounded
`$HOME/.api_key_<provider>` file. Any custom provider name is accepted unchanged.
The `provider models` command is the one exception that reads the selected credential
only long enough to send a catalog request; it sends it through curl's stdin
configuration, never includes it in arguments or output, and never stores it.
OpenCode Zen model discovery uses its public `/zen/v1/models` catalog and does
not require `OPENCODE_API_KEY`; the key is still required for benchmark
execution.
Workloads are loaded from `workloads/` beside the configuration file. Set
`ASB_WORKLOAD_DIR` to use another directory; add a new `.txt` file to extend
the catalogue.

`run`, `record`, and `replay` append one bounded JSON record per execution to
the configured `log_file`, including the captured agent output and error text.
Token usage is collected from generic/provider/agent environment variables,
structured agent JSON events, and recorded cassette responses, then normalized
without inferring counts from text size. OpenCode is launched in JSON event
mode so its provider-reported usage can be read for OpenRouter and cli2key.
Their terminal output is intentionally limited to success rate, elapsed time,
and token usage; `--json` changes only the representation of those metrics.

`asb tool install` installs a pinned package recipe into the configured tools
directory for supported agents and support tools, or downloads/copies an
executable supplied with `--source`. The `llmtape` support tool is project-local
and is automatically preferred by record/replay when installed. npm recipes are
project-local; Python recipes use a
project-local virtual environment. `asb workload install` downloads a prompt
URL, resolves a pinned external workload alias, or copies a local prompt into
the configured workloads directory. The `swe-bench` and `terminal-bench`
aliases download pinned upstream source archives into local bundle directories;
the runner copies those sources into `external-workload/` in the workspace.
Other aliases install pinned prompt metadata. These commands do not handle
credentials.
The `pi` agent uses the pinned `@mariozechner/pi-coding-agent` npm package and
its non-interactive print mode. For `cli2key`, the preparation layer maps Pi to
the bridge's OpenAI-compatible endpoint without storing credentials.
Installing `cli2key` also asks its bridge to refresh the local API key; model
discovery and runs then start the bridge and use that key ephemerally.
`asb tool list` and `asb workload list` expose these two catalogs without
installing anything. Command-specific help is available with `--help`.
Model discovery is fail-closed for tool use: provider metadata must advertise
`tools`, `tool_choice`, function calling, or an equivalent capability before a
model is listed. Execution repeats that validation; offline replay uses the
cassette's recorded tool request instead of contacting the provider.
The ASB source checkout is not a benchmark project: `init` rejects its
distinctive workspace layout before creating any files.

Recording and replay are implemented as a separate `tape` module behind the
runner boundary. It starts the project-local `llmtape` loopback proxy,
routes provider endpoints to that proxy, and tears it down after the child
exits. `record` writes a JSONL cassette; `replay` requires that cassette and
never starts cli2key or a live provider. This keeps the benchmark
configuration contract unchanged and keeps llmtape an optional, project-local
development tool rather than a runtime dependency. Installation includes the
llmtape npm library and a small project-local proxy wrapper because the npm
package itself does not publish a CLI. ASB never falls back to a
host `PATH` lookup; use `asb tool install llmtape` in the benchmark project or
set `ASB_LLM_TAPE` explicitly. Measurement collection remains
explicitly opt-in through `asb run --measurements DIR`; CI does not start live
agents, llmtape, or host collectors.

Finite human-entered names use bounded edit-distance suggestions on failure.
The original value is never replaced automatically; provider model IDs remain
provider-specific unless a finite local catalog is available.

The preparation stage is deliberately separate from command construction. Its
cli2key hook starts or connects to the OpenAI-compatible bridge, supplies the
bridge endpoint and API key to the selected agent, and tears down a bridge
process owned by ASB after the run. Additional preparation operations can be
added without changing `run` or the benchmark configuration format.
