//! Small command line front end for configuring and running ASB benchmarks.

mod benchmark;
mod cli2key;
mod completion;
mod config;
mod help;
mod install;
mod measurements;
mod models;
mod prepare;
mod project;
mod providers;
mod runner;
mod suggestions;
mod sweep;
mod tape;
mod tui;
mod workloads;

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::process::ExitCode;

const COMMANDS: &[&str] = &[
    "init", "setup", "run", "sweep", "provider", "tool", "workload",
];

/// Run the process CLI.
#[must_use]
pub fn entry(args: Vec<OsString>) -> ExitCode {
    ExitCode::from(run(
        &args,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    ))
}

/// Run one CLI request with injectable streams for tests.
pub fn run(args: &[OsString], stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    let words = match args
        .iter()
        .map(|value| value.to_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
    {
        Some(words) => words,
        None => return fail(stderr, "arguments must be valid UTF-8"),
    };
    let result = match words.as_slice() {
        [] => write_help(stdout),
        [command] if command == "--help" || command == "-h" => write_help(stdout),
        [command] if command == "--version" || command == "-V" => {
            writeln!(stdout, "asb {}", env!("CARGO_PKG_VERSION")).map_err(io_error)
        }
        [command, help_arg] if help_arg == "--help" || help_arg == "-h" => {
            help::command(command, stdout)
        }
        [command] if command == "tool" || command == "workload" || command == "provider" => {
            help::command(command, stdout)
        }
        [flag, shell] if flag == "--__generate-completion" && shell == "bash" => {
            completion::bash(stdout)
        }
        [flag, shell] if flag == "--__generate-completion" && shell == "zsh" => {
            completion::zsh(stdout)
        }
        [command, subcommand, help_arg]
            if (command == "provider" && subcommand == "models")
                && (help_arg == "--help" || help_arg == "-h") =>
        {
            help::command("provider models", stdout)
        }
        [command, subcommand, help_arg]
            if command == "setup"
                && subcommand == "tui"
                && (help_arg == "--help" || help_arg == "-h") =>
        {
            help::command("setup tui", stdout)
        }
        [command, subcommand, help_arg]
            if (command == "tool" || command == "workload")
                && (subcommand == "list" || subcommand == "install")
                && (help_arg == "--help" || help_arg == "-h") =>
        {
            help::command(&format!("{command} {subcommand}"), stdout)
        }
        [command, subcommand, rest @ ..] if command == "setup" && subcommand == "tui" => {
            tui::setup(rest, stdout)
        }
        [command, rest @ ..] if command == "setup" => config::setup(rest, stdout),
        [command, rest @ ..] if command == "init" => project::init(rest, stdout),
        [command, rest @ ..] if command == "run" => match parse_run_args(rest) {
            Ok((path, json, measurements, mode, cassette)) => runner::run_with_tape(
                &path,
                json,
                measurements.as_deref(),
                mode,
                cassette.as_deref(),
                stdout,
            ),
            Err(error) => Err(error),
        },
        [command, rest @ ..] if command == "sweep" => match parse_sweep_args(rest) {
            Ok((path, json, mode)) => sweep::run(&path, json, mode, stdout),
            Err(error) => Err(error),
        },
        [command, subcommand] if command == "tool" && subcommand == "list" => {
            install::tool_list(stdout)
        }
        [command, subcommand, rest @ ..] if command == "tool" && subcommand == "install" => {
            install::tool(rest, stdout)
        }
        [command, subcommand] if command == "workload" && subcommand == "list" => {
            install::workload_list(&[], stdout)
        }
        [command, subcommand, rest @ ..] if command == "workload" && subcommand == "list" => {
            install::workload_list(rest, stdout)
        }
        [command, subcommand, rest @ ..] if command == "workload" && subcommand == "install" => {
            install::workload(rest, stdout)
        }
        [command, subcommand, rest @ ..] if command == "provider" && subcommand == "models" => {
            models::list(rest, stdout)
        }
        [command, subcommand, ..] if command == "provider" => Err(CliError::Input(
            suggestions::unknown("provider command", subcommand, &["models"]),
        )),
        [command, subcommand, ..] if command == "tool" => Err(CliError::Input(
            suggestions::unknown("tool command", subcommand, &["list", "install"]),
        )),
        [command, subcommand, ..] if command == "workload" => Err(CliError::Input(
            suggestions::unknown("workload command", subcommand, &["list", "install"]),
        )),
        [command, ..] => Err(CliError::Input(suggestions::unknown(
            "command", command, COMMANDS,
        ))),
    };
    match result {
        Ok(()) => 0,
        Err(error) => fail(stderr, &error.to_string()),
    }
}

fn parse_run_args(
    args: &[String],
) -> Result<
    (
        String,
        bool,
        Option<String>,
        Option<tape::Mode>,
        Option<String>,
    ),
    CliError,
> {
    parse_run_args_named(args, "run")
}

fn parse_run_args_named(
    args: &[String],
    command: &str,
) -> Result<
    (
        String,
        bool,
        Option<String>,
        Option<tape::Mode>,
        Option<String>,
    ),
    CliError,
> {
    let mut path = None;
    let mut json = false;
    let mut measurements = None;
    let mut cassette = None;
    let mut mode = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            "--record" => {
                if mode.replace(tape::Mode::Record).is_some() {
                    return Err(CliError::Input(
                        "--record and --replay are mutually exclusive; choose one".into(),
                    ));
                }
            }
            "--replay" => {
                if mode.replace(tape::Mode::Replay).is_some() {
                    return Err(CliError::Input(
                        "--record and --replay are mutually exclusive; choose one".into(),
                    ));
                }
            }
            "--measurements" => {
                index += 1;
                let value = args.get(index).ok_or(CliError::Usage(
                    "Usage: asb run CONFIG [--record|--replay] [--cassette FILE] [--json] [--measurements DIR]",
                ))?;
                measurements = Some(value.clone());
            }
            "--cassette" => {
                index += 1;
                let value = args.get(index).ok_or(CliError::Usage(
                    "Usage: asb run CONFIG [--record|--replay] [--cassette FILE] [--json] [--measurements DIR]",
                ))?;
                cassette = Some(value.clone());
            }
            value if value.starts_with('-') => {
                return Err(CliError::Input(format!(
                    "unknown {command} option `{value}`; use `asb {command} --help`"
                )));
            }
            value if path.is_none() => path = Some(value.to_owned()),
            value => {
                return Err(CliError::Input(format!(
                    "unexpected {command} argument `{value}`; use `asb {command} --help`"
                )));
            }
        }
        index += 1;
    }
    if cassette.is_some() && mode.is_none() {
        return Err(CliError::Input(
            "--cassette requires --record or --replay; choose a tape mode first".into(),
        ));
    }
    path.map(|value| (value, json, measurements, mode, cassette))
        .ok_or(CliError::Usage("Usage: asb run CONFIG [--record|--replay] [--cassette FILE] [--json] [--measurements DIR]"))
}

fn parse_sweep_args(args: &[String]) -> Result<(String, bool, Option<tape::Mode>), CliError> {
    let mut path = None;
    let mut json = false;
    let mut mode = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            "--record" => {
                if mode.replace(tape::Mode::Record).is_some() {
                    return Err(CliError::Input(
                        "--record and --replay are mutually exclusive; choose one".into(),
                    ));
                }
            }
            "--replay" => {
                if mode.replace(tape::Mode::Replay).is_some() {
                    return Err(CliError::Input(
                        "--record and --replay are mutually exclusive; choose one".into(),
                    ));
                }
            }
            value if value.starts_with('-') => {
                return Err(CliError::Input(format!(
                    "unknown sweep option `{value}`; use `asb sweep --help`"
                )));
            }
            value if path.is_none() => path = Some(value.to_owned()),
            value => {
                return Err(CliError::Input(format!(
                    "unexpected sweep argument `{value}`; use `asb sweep --help`"
                )));
            }
        }
        index += 1;
    }
    path.map(|value| (value, json, mode)).ok_or(CliError::Usage(
        "Usage: asb sweep SWEEP.toml [--record|--replay] [--json]",
    ))
}

fn write_help(output: &mut dyn Write) -> Result<(), CliError> {
    help::root(output)
}

fn fail(stderr: &mut dyn Write, message: &str) -> u8 {
    let _ = writeln!(stderr, "asb: {message}");
    2
}

fn io_error(error: io::Error) -> CliError {
    CliError::Io(error.to_string())
}

#[derive(Debug)]
enum CliError {
    Usage(&'static str),
    Input(String),
    Io(String),
    Run(String),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(message) => formatter.write_str(message),
            Self::Input(message) | Self::Io(message) | Self::Run(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl std::error::Error for CliError {}

pub(crate) fn read_config(path: &std::path::Path) -> Result<String, CliError> {
    const MAX_CONFIG_BYTES: u64 = 64 * 1024;
    let metadata = std::fs::metadata(path)
        .map_err(|error| CliError::Input(format!("cannot read {}: {error}", path.display())))?;
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(CliError::Input(
            "benchmark configuration is larger than 64 KiB".into(),
        ));
    }
    let mut file = std::fs::File::open(path)
        .map_err(|error| CliError::Input(format!("cannot open {}: {error}", path.display())))?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|error| CliError::Input(format!("cannot read {}: {error}", path.display())))?;
    Ok(contents)
}

pub(crate) use CliError as Error;

#[cfg(test)]
mod tests {
    use super::run;
    use std::ffi::OsString;

    #[test]
    fn every_command_has_concise_help() {
        for words in [
            vec!["init", "--help"],
            vec!["setup", "--help"],
            vec!["setup", "tui", "--help"],
            vec!["run", "--help"],
            vec!["sweep", "--help"],
            vec!["provider", "--help"],
            vec!["provider", "models", "--help"],
            vec!["tool", "--help"],
            vec!["tool", "list", "--help"],
            vec!["tool", "install", "--help"],
            vec!["workload", "--help"],
            vec!["workload", "list", "--help"],
            vec!["workload", "install", "--help"],
        ] {
            let is_tool_install_help = words == ["tool", "install", "--help"];
            let args = words.into_iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            assert_eq!(run(&args, &mut stdout, &mut stderr), 0);
            assert!(String::from_utf8_lossy(&stdout).starts_with("Usage:"));
            if is_tool_install_help {
                assert!(String::from_utf8_lossy(&stdout).contains("provider bridge"));
            }
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn bash_completion_contains_public_commands() {
        let mut help = Vec::new();
        assert_eq!(
            run(&[OsString::from("--help")], &mut help, &mut Vec::new()),
            0
        );
        assert!(!String::from_utf8(help).unwrap().contains("completion"));

        let args = [
            OsString::from("--__generate-completion"),
            OsString::from("bash"),
        ];
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        assert_eq!(run(&args, &mut stdout, &mut stderr), 0);
        let script = String::from_utf8(stdout).unwrap();
        assert!(script.starts_with("# bash completion for asb"));
        assert!(!script.contains("completion)"));
        assert!(script.contains("cli2key"));
        for flag in [
            "--reset",
            "--agent",
            "--workload",
            "--provider",
            "--model",
            "--workspace",
            "--output",
            "--json",
            "--cassette",
            "--source",
            "--dir",
            "--dry-run",
            "--name",
        ] {
            assert!(script.contains(flag), "missing completion for {flag}");
        }
        assert!(script.contains("complete -F _asb asb"));
        assert!(stderr.is_empty());

        let args = [
            OsString::from("--__generate-completion"),
            OsString::from("zsh"),
        ];
        let mut stdout = Vec::new();
        assert_eq!(run(&args, &mut stdout, &mut Vec::new()), 0);
        let script = String::from_utf8(stdout).unwrap();
        assert!(script.starts_with("#compdef asb"));
        assert!(!script.contains("completion)"));
        assert!(script.contains("cli2key"));
    }

    #[test]
    fn run_accepts_json_and_measurements_in_either_order() {
        let first = super::parse_run_args(&[
            "benchmark.toml".into(),
            "--json".into(),
            "--measurements".into(),
            "measurements".into(),
        ])
        .unwrap();
        assert_eq!(first.0, "benchmark.toml");
        assert!(first.1);
        assert_eq!(first.2.as_deref(), Some("measurements"));
        assert!(first.3.is_none());
        let second = super::parse_run_args(&[
            "--measurements".into(),
            "measurements".into(),
            "benchmark.toml".into(),
        ])
        .unwrap();
        assert_eq!(second.0, "benchmark.toml");
        assert_eq!(second.2.as_deref(), Some("measurements"));
        assert!(second.3.is_none());
    }

    #[test]
    fn run_tape_modes_are_exclusive_and_accept_a_cassette() {
        let parsed = super::parse_run_args(&[
            "benchmark.toml".into(),
            "--record".into(),
            "--cassette".into(),
            "cassettes/demo.jsonl".into(),
            "--json".into(),
        ])
        .expect("cassette options parse");
        assert_eq!(parsed.0, "benchmark.toml");
        assert!(parsed.1);
        assert_eq!(parsed.3, Some(crate::tape::Mode::Record));
        assert_eq!(parsed.4.as_deref(), Some("cassettes/demo.jsonl"));
        assert!(
            super::parse_run_args(&[
                "benchmark.toml".into(),
                "--record".into(),
                "--replay".into()
            ])
            .is_err()
        );
    }

    #[test]
    fn unknown_commands_and_subcommands_offer_close_matches() {
        let mut stderr = Vec::new();
        assert_eq!(
            run(&[OsString::from("worklod")], &mut Vec::new(), &mut stderr),
            2
        );
        assert!(String::from_utf8_lossy(&stderr).contains("did you mean `workload`?"));
        stderr.clear();
        assert_eq!(
            run(
                &[OsString::from("provider"), OsString::from("modls"),],
                &mut Vec::new(),
                &mut stderr
            ),
            2
        );
        assert!(String::from_utf8_lossy(&stderr).contains("did you mean `models`?"));
    }
}
