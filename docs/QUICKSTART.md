# Quickstart

Build ASB, initialize a project, install an agent and workload, create a
configuration, then run it:

```sh
make update
cargo build --locked
make install
./target/debug/asb init
./target/debug/asb tool list
./target/debug/asb tool install codex
./target/debug/asb tool install perfetto
./target/debug/asb workload list
./target/debug/asb provider models openrouter
./target/debug/asb workload install \
  https://raw.githubusercontent.com/martin-beck/asb/main/workloads/bug-fix.txt \
  --name bug-fix
./target/debug/asb setup \
  --agent codex \
  --workload customer-task \
  --provider openrouter \
  --model openai/gpt-oss-20b:free \
  --output benchmark.toml
./target/debug/asb run benchmark.toml
```

The build regenerates `target/asb.bash` and `target/_asb` from the command and
option catalog; install places them in the current user's Bash and Zsh
completion directories.

Credentials normally come from the environment used to launch ASB. For example,
OpenRouter agents can use `OPENROUTER_API_KEY`; cli2key integrations can use
`CLI2KEY_API_KEY` and `CLI2KEY_BASE_URL`. ASB does not store or inspect these
values. If the environment variable is absent, ASB checks the bounded local
file `$HOME/.api_key_<provider>` (such as `$HOME/.api_key_openrouter`) without
printing it. `asb provider models openrouter` uses the same credential lookup to print the current
catalog; `asb provider models --json` queries every supported provider and reports
missing credentials or unavailable endpoints individually. Add `--free` to list
only OpenRouter models marked free, OpenCode Zen models ending in `-free`, and
models served by local Ollama; for example, `asb provider models openrouter --free`
or `asb provider models opencode-zen --free`. OpenCode Zen discovery works
without an API key; an API key is still required to run a benchmark through Zen.
The catalog includes only models whose provider metadata advertises tool
calling. ASB repeats this check before live execution, while replay requires a
matching tool-calling request in the cassette.

To record real responses once and replay them without contacting the provider:

```sh
asb run benchmark.toml --record --repeat 3 --cassette trials.asb.zip
asb run benchmark.toml --replay --repeat 5 --cassette trials.asb.zip --seed 42
asb run benchmark.toml --replay --cassette trials.asb.zip --track max:tokens
```

Repeat counts batches at the configuration's concurrency. Each bundle execution
starts with fresh workload and agent-session state, preserving your existing
workspace. Replay still runs the agent and its tools, requires matching installed
workload material, and is not a filesystem snapshot. Use `--respect-record-timings`
when the recorded provider delays should be reproduced. Cassettes contain provider
responses; keep them private. The offline track viewer and sweep bundle support
are separate pending integration steps.

For retained replay mismatches, the separate `asb-replay-debug` Ratatui binary
provides an offline field and text diff. See the [replay debugger tutorial](REPLAY_DEBUGGER.md)
for building it, opening a campaign, and switching between replicas.

For cli2key model discovery, install the bridge once and query it directly:

```sh
asb tool install cli2key
asb provider models cli2key
```

The install refreshes the local bridge key. The query starts the project-local
bridge, uses that key without printing it, and stops the bridge afterward.

For a local cli2key bridge, install the bridge and select `--provider cli2key`:

```sh
asb tool install cli2key
asb setup --agent opencode --workload bug-fix --provider cli2key --model gpt-6.1-sol --output benchmark.toml
asb run benchmark.toml
```

ASB starts the project-local bridge, obtains its API key, and configures the
selected agent's API environment automatically. OpenCode uses the bridge's
Anthropic Messages API. If using an
already running external bridge, set both `CLI2KEY_BASE_URL` and its matching
`CLI2KEY_API_KEY`. The bridge supports `gpt-6-astra`, `gpt-6.1-sol`, and
`gpt-6-luna`.

The bundled bridge executes workloads through `codex`, `opencode`,
`qwen-code`, `mini-swe`, `openhands`, `openjiuwen`, and `workbuddy`.
WorkBuddy receives the bridge endpoint through its project-local CodeBuddy API
settings. Aider requires OpenAI Chat Completions and therefore needs an
OpenAI-compatible provider. Gemini still requires its Google provider.

`asb init` copies the bundled `.txt` workloads into `workloads/` and seeds
`.asb/fixtures/` with the source, tests, and scripts required by every built-in
task, so they are available immediately after initialization. Before each run,
the selected fixture is copied into the workspace without overwriting edits.
Add a file such as
`workloads/my-task.txt`, then select it with `--workload my-task`. To keep
workloads elsewhere, set `ASB_WORKLOAD_DIR` before `asb run`.
Initialize a separate benchmark directory, not the ASB source checkout;
`asb init` rejects the source checkout to avoid polluting it with project data.
Running `asb init` again warns without changing anything. Use
`asb init --reset` only when you want managed project state removed and rebuilt.

Install a local agent executable or external prompt with:

```sh
asb tool install my-agent --source /path/to/my-agent
asb workload install https://example.org/workloads/customer-task.txt
# Or install a complete pinned external workload bundle by its short name:
asb workload install swe-bench
```

The built-in recipes install into `.asb-tools/<agent>/` and run from there.
Source-installed agents use `.asb-tools/<agent>/bin/`; workloads are stored in
`workloads/`. These paths come from `.asb/project.toml`, so changing that file
changes every command consistently. Add `--dir DIR` only when you need a
one-off destination.

All runtime collectors are process-scoped: mpstat/iostat result names use
pidstat for the agent and child tasks, perf uses `-p`, built-in bpftrace
profiles use the agent PID, and Perfetto filters scheduler events to that PID.
To collect scheduler and process-lifecycle evidence for a run, install the
project-local Perfetto Tracebox tool above, enable the `perfetto` measurement
collector in the generated run configuration, and run normally. ASB stores its
trace, configuration, and fixed lifecycle markers in that run's measurement
directory; it never writes prompts, credentials, provider responses, or model
configuration into the trace. The same run records `perf stat` counters when
the host permits them.

Run output is concise and human-readable by default. Add `--json` to emit the
structured result for scripts:

```sh
./target/debug/asb run benchmark.toml --json
```

For live debugging, also stream the agent's stdout and stderr while the run is
active:

```sh
./target/debug/asb run benchmark.toml --show-output
```

For a generated sweep, use the same option:

```sh
./target/debug/asb sweep sweep.toml --show-output
```

Use `asb COMMAND --help` for concise help on every command and subcommand.
`asb tool list` enumerates all built-in agent and support-tool installers; `asb workload list`
enumerates local prompt files and installable external aliases. The
`swe-bench` and `terminal-bench` aliases fetch pinned upstream source archives
into the configured workload directory and copy them into
`external-workload/` for runs. Other aliases fetch pinned prompt metadata.
External aliases currently execute only small ASB adapter smoke fixtures, not
the upstream suite's actual tasks or official evaluator. A passing smoke
exercise has no benchmark success rate. See [evaluation semantics](EVALUATION.md)
before interpreting agent comparisons.

The built-in recipes are:

| Agent | Installer | Local executable |
| --- | --- | --- |
| `codex` | pinned npm package | `codex` |
| `opencode` | pinned npm package | `opencode` |
| `workbuddy` | pinned npm package | `codebuddy` |
| `qwen-code` | pinned npm package | `qwen` |
| `mini-swe` | pinned Python package | `mini` |
| `openhands` | pinned Python package | `openhands` |
| `openjiuwen` | pinned Python package | `openjiuwen` |
| `claude-code` | pinned npm package | `claude` |
| `goose` | pinned upstream Linux binary | `goose` |
| `hermes` | pinned Python package | `hermes` |
| `cline` | pinned npm package | `cline` |
| `continue` | pinned npm package | `cn` |
| `cli2key` | pinned npm bridge package | `codex-bridge` |

The npm recipes require `npm`; Python recipes use the project's Python 3.12 or
3.13 environment. Initialization uses an installed interpreter or uv, which
can download Python 3.12 into the project if necessary. An unavailable Python
does not prevent initialization; installation retries setup. `asb tool install AGENT --dry-run` prints the
exact local installation plan without changing the project.

If a prerequisite is missing, ASB explains what is needed. npm can be installed into the user account with
`fnm`; Python can be provisioned with `uv`; missing `curl` or `tar` gets the
matching package-manager command. Run the printed command in the same shell,
then retry the original `asb tool install` or workload command.
For kernel-sensitive profiling, run `asb tool install bpftrace` in the benchmark
project. ASB copies the host-compatible binary locally and records the current
kernel release and architecture; reinstall it after a kernel upgrade or when
moving the project to another machine. If the distribution has no binary,
`asb tool install bpftrace` builds the pinned source release locally; the
installer reports copy-paste package commands for the three supported distro
families (Red Hat/Fedora, Debian/Ubuntu, and openEuler) if that fallback
cannot be configured or compiled. The same commands are shown when no matching
host `bpftrace` package is available.

ASB's built-in BPFTrace profiles are scoped to the spawned agent PID, rather
than sampling unrelated host work. Add entries such as
`"bpftrace:sched-latency"` and `"bpftrace:futex-wait"` to
`measurements.collectors`, or set
`ASB_BPFTRACE_PROFILES=sched-latency,futex-wait` before `asb run`. The available
profiles are `sched-latency`, `cpu-migrations`, `futex-wait`, `syscall-rate`,
and `lock-contention`; `all` selects the full catalog. Each profile writes a
separate JSON result and SVG, and comparison reports graph every extracted
numeric metric.
