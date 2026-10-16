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
./target/debug/asb tool install llmtape
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

Run output is concise and human-readable by default. Add `--json` to emit the
structured result for scripts:

```sh
./target/debug/asb run benchmark.toml --json
```

Use `asb COMMAND --help` for concise help on every command and subcommand.
`asb tool list` enumerates all built-in agent and support-tool installers; `asb workload list`
enumerates local prompt files and installable external aliases. The
`swe-bench` and `terminal-bench` aliases fetch pinned upstream source archives
into the configured workload directory and copy them into
`external-workload/` for runs. Other aliases fetch pinned prompt metadata.

The built-in recipes are:

| Agent | Installer | Local executable |
| --- | --- | --- |
| `codex` | pinned npm package | `codex` |
| `opencode` | pinned npm package | `opencode` |
| `workbuddy` | pinned npm package | `codebuddy` |
| `gemini` | pinned npm package | `gemini` |
| `qwen-code` | pinned npm package | `qwen` |
| `aider` | pinned Python package | `aider` |
| `mini-swe` | pinned Python package | `mini` |
| `openhands` | pinned Python package | `openhands` |
| `openjiuwen` | pinned Python package | `openjiuwen` |
| `cli2key` | pinned npm bridge package | `codex-bridge` |
| `llmtape` | pinned npm package | `llmtape` |

The npm recipes require `npm`; the Python recipes require `python3` with
virtual-environment support. `asb tool install AGENT --dry-run` prints the
exact local installation plan without changing the project.

If a prerequisite is missing, ASB stops before changing the tool and prints a
copy-paste repair command. npm can be installed into the user account with
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
