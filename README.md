# Agent Systems Benchmark

ASB is a small command-line benchmark runner. Initialize a project, install an
agent/workload, create a configuration, then run it:

```text
asb init [PROJECT_DIR] [--reset]
asb tool list
asb tool install AGENT
asb tool install llmtape
asb workload list
asb workload install SOURCE.txt|URL
asb workload tui
asb provider models [PROVIDER] [--free]
asb setup tui
asb setup --agent AGENT --workload WORKLOAD --provider PROVIDER --model MODEL [--workspace DIR] [--output FILE]
asb run CONFIG [--record|--replay] [--cassette FILE] [--json] [--measurements DIR]
asb sweep SWEEP.toml [--record|--replay] [--json]
asb compare SWEEP.toml
```

`setup tui` opens a Ratatui wizard with agent, provider, model, workload, and
measurement steps. Select multiple entries with Space; Tab or Enter advances
only after a selection. It writes one uniquely named test TOML per compatible
combination and one sweep TOML referencing those files. The model step queries
the selected provider catalogs and lists all returned tool-capable models (for
example DeepSeek, GLM, GPT, and Fable on OpenRouter), falling back to a pinned
local default only when discovery is unavailable. When generation succeeds, the
wizard prints the exact `asb sweep <generated>.toml` command to run the sweep.
`asb workload tui` opens
the same Ratatui interaction components in a
two-level workload/trial tree. Select workloads and enabled trials, then it
writes `<workload>.workload.toml` files; `setup tui` presents those trial sets
as selectable workload entries and generates one benchmark configuration per
enabled trial. Both TUIs prefix every selectable item with a colored `ⓘ`:
green means runnable, red means missing, and yellow means not locally
verifiable (for example a remote provider catalog). On an agent, external
workload, or collector, `Ctrl-I` invokes the bounded project-local installer
when one exists and refreshes the status; providers and models remain
discovery-only. While an installer runs, the footer is refreshed live as
`Installing (XX%)`; completion is shown in green and failure/unavailability in
red. Installer subprocess output is appended to `.asb/install.log` instead of
overwriting the TUI. `init` writes `.asb/project.toml` with project-local tool, workload, and
workspace directories and copies the bundled workload prompts into
`workloads/` and seeds `.asb/fixtures/` with the source, tests, and supporting
files for every built-in workload. `run` copies the selected fixture into the
workspace without overwriting existing edits. `setup` writes a human-readable
TOML configuration; when `--output` is supplied it also prints the exact
`asb run <file>` command for that configuration.
Run `asb init` in a separate benchmark project directory; it intentionally
refuses to modify the ASB source checkout.
If the directory is already initialized, ASB warns and makes no changes;
rerun with `--reset` to remove managed state and create a fresh project.
`run` loads the project and benchmark configuration, selects the workload
prompt, and starts the configured agent with the provider and model in the
environment. `setup` writes a `log_file` entry (for example `foo.log` for
`foo.toml`); full bounded agent output is appended there. The terminal prints
only success rate, elapsed time, and token usage; pass `--json` for those same
metrics as JSON. Credentials stay outside the configuration. Pass `--measurements DIR` to write machine-readable
`system.json`, `run.json`, and bounded `summary.json` records. Token usage is
normalized into input, output, cached, and total tokens. ASB reads generic
`ASB_*_TOKENS` variables, provider/agent-prefixed equivalents such as
`OPENROUTER_PROMPT_TOKENS` and `CLI2KEY_COMPLETION_TOKENS`, structured JSON
usage emitted by agents, and recorded response usage in llmtape cassettes.
All supported agent families' usage events are normalized, including JSON
embedded in human-readable output, so OpenRouter and cli2key runs report the
same input, output, cached, and total fields whenever the provider reports
them. A missing usage field remains `null`; ASB never guesses token counts from
text length.

Measurement collectors are opt-in environment switches so normal and CI runs
remain deterministic: `ASB_MEASURE_MPSTAT=1`, `ASB_MEASURE_IOSTAT=1`, and
`ASB_MEASURE_PERF=1` start the corresponding local tools. BPFTrace programs
are selected with `ASB_MEASURE_BPFTRACE=1` and a semicolon-separated
`ASB_BPFTRACE_PROGRAMS` list such as
`program.bt?PID=123&INTERVAL=1`. Flamegraphs use the local tools named by
`ASB_FLAMEGRAPH_STACKCOLLAPSE` and `ASB_FLAMEGRAPH_RENDER`; the SVG is paired
with machine-readable `flamegraph.json` metadata. The mpstat and iostat files
retain bounded raw samples and add averaged machine-readable CPU, I/O
throughput, latency, queue-depth, and utilization metrics. `asb compare` uses
those metrics rather than scraping every raw sample, so it emits one canonical
runtime graph plus useful unit-labelled resource graphs. If perf permissions,
samples, or FlameGraph tools are unavailable, `flamegraph.json` records the
reason and status as `unavailable`; an ordinary `asb run` is unaffected.
Before a run starts, perf and configured BPFTrace collectors are probed without
prompting. If direct access fails, ASB tries `sudo -n` and uses it for that
collector only; if passwordless sudo is not already authorized, the run stops
before the agent starts and prints a scoped `/etc/sudoers.d/asb-<collector>`
repair command.

Configurations generated by `asb setup` include an enabled `measurements`
section listing every collector module and defaulting its output directory to
`measurements`, plus a `log_file` defaulting to the configuration's stem with a
`.log` extension. A hand-written configuration can omit that section to retain
the no-collection behavior, or pass `--measurements DIR` to enable collection
for one run.

`asb sweep` automatically runs the same comparison after all benchmark entries
finish, including partial sweeps so failed or missing entries remain visible.
`asb compare SWEEP.toml` can be run at any time to regenerate
`results/<sweep>-comparison/report.html` in
addition to `comparison.json` and SVG charts. Open the report locally for a
professional, offline overview: every benchmark row shows its agent, provider,
model, workload, all numeric measurements, and links to raw run/collector/log
artifacts. Checkbox filters can enable or disable individual benchmarks,
agents, providers, models, and workloads without contacting a provider or
rerunning the sweep.

`asb run CONFIG --record` runs the same configuration through the local `llmtape`
proxy and writes a JSONL cassette (by default below `cassettes/` next to the
configuration). Install the proxy into the current benchmark project with
`asb tool install llmtape`; this installs the pinned npm library plus ASB's
project-local `llmtape` proxy wrapper. No global llmtape installation is required. ASB
does not search the host `PATH` implicitly. To deliberately use another
executable, set `ASB_LLM_TAPE`. Use `--cassette FILE` to choose an explicit
path. `asb run CONFIG --replay` starts llmtape's cassette server and routes
all supported provider endpoints to it; it refuses a missing cassette and does
not start cli2key or connect to any live provider. Cassettes are local test
artifacts and should not be committed when they contain provider responses.

`provider models` queries live provider model catalogs. Pass a provider to query one,
or omit it to query every supported provider and report unavailable providers
without stopping the rest. Use `--json` for machine-readable output. Add `--free`
to list only free OpenRouter models, OpenCode Zen models ending in `-free`,
and local Ollama models; with `--free`, omitting the provider queries only
those providers. OpenCode Zen model discovery is public and does not require
an API key. Model
discovery uses the provider credential and endpoint environment variables but
never writes or prints credential values.
Only models that advertise tool-calling support are enumerated. `run` and
`run --record` revalidate the selected model before launch; `run --replay` validates that
the cassette contains a tool-calling request for the configured model. Models
without tool support are rejected because the bundled workloads depend on
agent tools.
When an environment credential is absent, ASB also checks the bounded local
file `$HOME/.api_key_<provider>` (for example `$HOME/.api_key_openrouter`).
For `cli2key`, `asb tool install cli2key` refreshes the local bridge key, and
`asb provider models cli2key` starts the project-local bridge and discovers its
available Codex models without requiring `CLI2KEY_API_KEY`.
`asb provider models opencode-zen --free` queries OpenCode Zen's public model
catalog without an API key and lists its models marked with the `-free` suffix;
an `OPENCODE_API_KEY` is required only when running a benchmark through Zen.

Every command and subcommand has concise help; use `asb COMMAND --help`.
`asb tool list` shows all built-in agent and support-tool installers and `asb workload list`
shows the `.txt` prompts available in the current project plus installable
external workload aliases. Install one by its short name, for example
`asb workload install swe-bench`; ASB fetches the pinned upstream source into
the current project's workload directory. `swe-bench` and `terminal-bench`
install complete local source bundles and materialize them into the run
workspace; the other aliases install their pinned upstream prompt metadata.

Before a run, ASB executes a modular preparation stage. For `cli2key`, it
starts the project-local bridge when `CLI2KEY_BASE_URL` is not supplied, reads
the bridge API key, and configures the selected agent with the bridge endpoint,
API key, and selected model. OpenCode uses the bridge's Anthropic Messages API
for compatibility. The bridge process is stopped when the run finishes. If an
external bridge endpoint is supplied, `CLI2KEY_API_KEY` must contain its
matching key.

The bundled cli2key bridge executes workloads through `codex`, `opencode`,
`qwen-code`, `mini-swe`, `openhands`, `openjiuwen`, and `workbuddy`.
WorkBuddy uses project-local CodeBuddy model settings configured for the
bridge's Responses API. Aider requires OpenAI Chat Completions and therefore
needs an OpenAI-compatible provider; Gemini still requires its Google provider.
Built-in tool recipes install pinned releases into the project (and do not
modify global tools): `codex`, `pi`, `opencode`, `workbuddy`, `gemini`,
`qwen-code`, and `cli2key` use npm; `aider`, `mini-swe`, `openhands`, and
`openjiuwen` use an isolated Python virtual environment. Any other agent can
be installed from a local executable or URL with `--source`.

If npm, Python, curl, or tar is unavailable, the failed command prints a
copy-paste repair command. The npm repair uses user-local `fnm`; the Python
repair uses user-local `uv`; after running the printed command, retry ASB.

Pi is run in non-interactive print mode. With `provider = "cli2key"`, ASB
maps the request to Pi's OpenAI-compatible provider and supplies the bridge
endpoint and generated API key through the child environment.

The CLI is intentionally modular. Configuration, workload prompts, process
execution, and optional measurements are separate modules so recording/replay,
additional collectors, tool commands, and an external frontend can be added
without changing the setup file.

Finite command, tool, workload, provider, and locally enumerated model names
fail closed on typos and print the closest match as a question; ASB never
silently substitutes a value.

## Build

```sh
cargo build --locked
cargo test --locked
```

Install the already-built binary into the current user's `~/.local/bin` with:

```sh
make install
```

`make build` regenerates `target/asb.bash` and `target/_asb` from ASB's command
and option catalog. `make install` installs Bash completion at
`~/.local/share/bash-completion/completions/asb` and Zsh completion at
`~/.local/share/zsh/site-functions/_asb`, alongside the binary.

The only workspace member is `crates/asb-cli`.

Update a checkout to the current `main` branch with:

```sh
make update
```

## Example

```sh
asb setup \
  --agent codex \
  --workload original.repository-navigation \
  --provider openrouter \
  --model openai/gpt-oss-20b:free \
  --output benchmark.toml

asb run benchmark.toml
```
