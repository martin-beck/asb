//! Shell completion script generation.

use crate::{Error, io_error};
use std::fmt::Write as FmtWrite;
use std::io::Write;

const TOP_LEVEL: &[&str] = &[
    "init", "setup", "run", "sweep", "provider", "tool", "workload",
];
const INIT_OPTIONS: &[&str] = &["--reset"];
const SETUP_OPTIONS: &[&str] = &[
    "--agent",
    "--workload",
    "--provider",
    "--model",
    "--workspace",
    "--output",
];
const RUN_OPTIONS: &[&str] = &[
    "--record",
    "--replay",
    "--cassette",
    "--json",
    "--measurements",
];
const SWEEP_OPTIONS: &[&str] = &["--record", "--replay", "--json"];
const PROVIDER_SUBCOMMANDS: &[&str] = &["models"];
const PROVIDER_MODELS_OPTIONS: &[&str] = &["--json", "--free"];
const TOOL_SUBCOMMANDS: &[&str] = &["list", "install"];
const TOOL_INSTALL_VALUES: &[&str] = &[
    "codex",
    "pi",
    "opencode",
    "codeagent",
    "workbuddy",
    "gemini",
    "qwen-code",
    "aider",
    "mini-swe",
    "openhands",
    "openjiuwen",
    "cli2key",
    "llmtape",
    "--source",
    "--dir",
    "--dry-run",
];
const WORKLOAD_SUBCOMMANDS: &[&str] = &["list", "install"];
const WORKLOAD_LIST_OPTIONS: &[&str] = &["--dir"];
const WORKLOAD_INSTALL_OPTIONS: &[&str] = &["--name", "--dir"];

/// Write a Bash completion script assembled from the CLI command catalog.
pub(crate) fn bash(output: &mut dyn Write) -> Result<(), Error> {
    output
        .write_all(generate_bash().as_bytes())
        .map_err(io_error)
}

/// Write a Zsh completion script assembled from the CLI command catalog.
pub(crate) fn zsh(output: &mut dyn Write) -> Result<(), Error> {
    output
        .write_all(generate_zsh().as_bytes())
        .map_err(io_error)
}

fn generate_bash() -> String {
    let mut script = String::from("# bash completion for asb\n_asb() {\n");
    script.push_str("    local cur command subcommand\n    COMPREPLY=()\n");
    script.push_str("    cur=\"${COMP_WORDS[COMP_CWORD]}\"\n");
    script.push_str("    command=\"${COMP_WORDS[1]}\"\n");
    script.push_str("    subcommand=\"${COMP_WORDS[2]}\"\n\n");
    let _ = writeln!(
        script,
        "    if [[ ${{COMP_CWORD}} -eq 1 ]]; then\n        COMPREPLY=( $(compgen -W '{}' -- \"$cur\") )\n        return\n    fi\n",
        words(TOP_LEVEL)
    );
    script.push_str("    case \"$command\" in\n");
    simple_case(&mut script, "init", INIT_OPTIONS);
    simple_case(&mut script, "setup", SETUP_OPTIONS);
    simple_case(&mut script, "run", RUN_OPTIONS);
    simple_case(&mut script, "sweep", SWEEP_OPTIONS);
    nested_case(
        &mut script,
        "provider",
        PROVIDER_SUBCOMMANDS,
        &[("models", PROVIDER_MODELS_OPTIONS)],
    );
    nested_case(
        &mut script,
        "tool",
        TOOL_SUBCOMMANDS,
        &[("install", TOOL_INSTALL_VALUES)],
    );
    nested_case(
        &mut script,
        "workload",
        WORKLOAD_SUBCOMMANDS,
        &[
            ("list", WORKLOAD_LIST_OPTIONS),
            ("install", WORKLOAD_INSTALL_OPTIONS),
        ],
    );
    script.push_str("    esac\n}\ncomplete -F _asb asb\n");
    script
}

fn simple_case(script: &mut String, command: &str, values: &[&str]) {
    let _ = writeln!(
        script,
        "        {command}) COMPREPLY=( $(compgen -W '{}' -- \"$cur\") ) ;;",
        words(values)
    );
}

fn nested_case(
    script: &mut String,
    command: &str,
    subcommands: &[&str],
    nested_values: &[(&str, &[&str])],
) {
    let _ = writeln!(script, "        {command})");
    let _ = writeln!(
        script,
        "            if [[ ${{COMP_CWORD}} -eq 2 ]]; then\n                COMPREPLY=( $(compgen -W '{}' -- \"$cur\") )",
        words(subcommands)
    );
    for (subcommand, values) in nested_values {
        let _ = writeln!(
            script,
            "            elif [[ \"$subcommand\" == {subcommand} ]]; then\n                COMPREPLY=( $(compgen -W '{}' -- \"$cur\") )",
            words(values)
        );
    }
    script.push_str("            fi ;;\n");
}

fn words(values: &[&str]) -> String {
    values.join(" ")
}

fn generate_zsh() -> String {
    let mut script = String::from("#compdef asb\n\n_asb() {\n");
    script.push_str("  local state\n  typeset -A opt_args\n");
    script.push_str("  _arguments -C '1:command:->command' '*::arg:->args'\n");
    script.push_str("  case $state in\n    command)\n      local -a commands=(\n");
    for command in TOP_LEVEL {
        let _ = writeln!(script, "        '{command}'");
    }
    script.push_str("      )\n      _describe 'command' commands\n      ;;\n    args)\n      case $words[1] in\n");
    zsh_simple_case(&mut script, "init", INIT_OPTIONS);
    zsh_simple_case(&mut script, "setup", SETUP_OPTIONS);
    zsh_simple_case(&mut script, "run", RUN_OPTIONS);
    zsh_simple_case(&mut script, "sweep", SWEEP_OPTIONS);
    zsh_nested_case(
        &mut script,
        "provider",
        PROVIDER_SUBCOMMANDS,
        &[("models", PROVIDER_MODELS_OPTIONS)],
    );
    zsh_nested_case(
        &mut script,
        "tool",
        TOOL_SUBCOMMANDS,
        &[("install", TOOL_INSTALL_VALUES)],
    );
    zsh_nested_case(
        &mut script,
        "workload",
        WORKLOAD_SUBCOMMANDS,
        &[
            ("list", WORKLOAD_LIST_OPTIONS),
            ("install", WORKLOAD_INSTALL_OPTIONS),
        ],
    );
    script.push_str("      esac\n      ;;\n  esac\n}\n");
    script
}

fn zsh_simple_case(script: &mut String, command: &str, options: &[&str]) {
    let _ = writeln!(script, "        {command})");
    script.push_str("          _arguments");
    for option in options {
        let _ = write!(script, " '{option}[complete {option}]'");
    }
    script.push_str("\n          ;;\n");
}

fn zsh_nested_case(
    script: &mut String,
    command: &str,
    subcommands: &[&str],
    nested_values: &[(&str, &[&str])],
) {
    let _ = writeln!(script, "        {command})");
    let _ = writeln!(script, "          if (( CURRENT == 3 )); then");
    let _ = writeln!(
        script,
        "            _values 'subcommand' {}",
        words(subcommands)
    );
    for (subcommand, values) in nested_values {
        let _ = writeln!(
            script,
            "          elif [[ $words[2] == {subcommand} ]]; then"
        );
        script.push_str("            _arguments");
        for value in *values {
            let _ = write!(script, " '{value}[complete {value}]'");
        }
        script.push('\n');
    }
    script.push_str("          fi\n          ;;\n");
}
