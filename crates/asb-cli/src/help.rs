//! Concise command-specific help text.

use crate::{Error, io_error};
use std::io::Write;

/// Write the top-level command summary.
pub fn root(output: &mut dyn Write) -> Result<(), Error> {
    write_text(
        output,
        "ASB\n\nUsage:\n  asb init [PROJECT_DIR] [--reset]\n  asb setup tui\n  asb setup --agent AGENT --workload WORKLOAD --provider PROVIDER --model MODEL [--workspace DIR] [--output FILE]\n  asb run CONFIG [--record|--replay] [--cassette FILE] [--json] [--measurements DIR]\n  asb sweep SWEEP.toml [--record|--replay] [--json]\n  asb provider models [PROVIDER] [--json] [--free]\n  asb tool list\n  asb tool install AGENT [--source URL|PATH] [--dir DIR] [--dry-run]\n  asb workload list [--dir DIR]\n  asb workload install SOURCE.txt|URL [--name NAME] [--dir DIR]\n\nUse `asb COMMAND --help` for concise help for one command.",
    )
}

/// Write help for a command or nested subcommand.
pub fn command(name: &str, output: &mut dyn Write) -> Result<(), Error> {
    let text = match name {
        "init" => {
            "Usage: asb init [PROJECT_DIR] [--reset]\n\nCreate project state and bundled workloads. Existing projects are preserved unless --reset is supplied."
        }
        "setup" => {
            "Usage: asb setup tui | asb setup --agent AGENT --workload WORKLOAD --provider PROVIDER --model MODEL [--workspace DIR] [--output FILE]\n\n`setup tui` opens the modular keyboard-driven wizard and writes one TOML per selected combination plus a sweep TOML. The non-interactive form writes one configuration. Generated configurations enable every measurement module; credentials remain in the environment."
        }
        "setup tui" => {
            "Usage: asb setup tui\n\nOpen the modular terminal wizard. Select agents, providers, models, workloads, and measurements with Space; navigate with arrows and Tab or Enter. The wizard writes one uniquely named TOML per compatible combination and one sweep TOML."
        }
        "run" => {
            "Usage: asb run CONFIG [--record|--replay] [--cassette FILE] [--json] [--measurements DIR]\n\nRun one configured workload. `--record` routes the provider through project-local llmtape and writes a cassette; `--replay` uses that cassette without a live provider. The flags are mutually exclusive. Full agent output is logged; terminal output is limited to success rate, elapsed time, and token usage. Install llmtape with `asb tool install llmtape`."
        }
        "sweep" => {
            "Usage: asb sweep SWEEP.toml [--record|--replay] [--json]\n\nValidate a `configs = [...]` sweep manifest (not a normal benchmark TOML), then run each referenced benchmark sequentially. Each run gets its own `<sweep>-results/<index>-<name>/` folder containing an isolated config, workspace, log, measurements, and tape cassette. `--record` and `--replay` are mutually exclusive; replay never contacts a live provider."
        }
        "provider" => {
            "Usage: asb provider models [PROVIDER] [--json] [--free]\n\nAccess provider-related commands. `models` lists live provider catalogs."
        }
        "provider models" => {
            "Usage: asb provider models [PROVIDER] [--json] [--free]\n\nList live tool-capable models for one provider, or all supported providers when omitted. Models without tool-calling metadata are excluded. `--free` limits discovery to OpenRouter models marked free (including zero pricing), OpenCode Zen models ending in `-free`, and local Ollama models. OpenCode Zen's catalog is public and can be queried without an API key."
        }
        "tool" => {
            "Usage: asb tool COMMAND\n\nCommands:\n  list                 List built-in agent installers.\n  install AGENT ...    Install one agent into the project."
        }
        "tool list" => {
            "Usage: asb tool list\n\nList every built-in agent and support-tool installer and its local executable."
        }
        "tool install" => {
            "Usage: asb tool install NAME [--source URL|PATH] [--dir DIR] [--dry-run]\n\nInstall a pinned built-in agent or support tool, or copy/download an executable with --source. `cli2key` installs the local provider bridge; `llmtape` installs the local record/replay proxy. If npm or Python is missing, ASB prints a copy-paste user-local repair command."
        }
        "workload" => {
            "Usage: asb workload COMMAND\n\nCommands:\n  list                 List workload files in the project.\n  install SOURCE ...   Copy or download a workload prompt."
        }
        "workload list" => {
            "Usage: asb workload list [--dir DIR]\n\nList installable .txt workloads in the configured directory."
        }
        "workload install" => {
            "Usage: asb workload install SOURCE.txt|URL [--name NAME] [--dir DIR]\n\nCopy or download one prompt into the project workload directory."
        }
        _ => return root(output),
    };
    write_text(output, text)
}

fn write_text(output: &mut dyn Write, text: &str) -> Result<(), Error> {
    writeln!(output, "{text}").map_err(io_error)
}
