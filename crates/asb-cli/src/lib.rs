//! Small command line front end for configuring and running ASB benchmarks.

mod agents;
mod benchmark;
mod collectors;
mod compare;
mod completion;
mod config;
mod endpoint;
mod install;
mod measurements;
mod models;
mod parser;
mod process_gate;
mod project;
mod providers;
mod proxy;
mod python_runtime;
mod runner;
mod suggestions;
mod sweep;
mod tape;
mod tools;
mod tui;
mod workloads;

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::process::ExitCode;

/// Run the process CLI.
#[must_use]
pub fn entry(args: Vec<OsString>) -> ExitCode {
    if args.first().is_some_and(|arg| arg == "__asb-gate") {
        return process_gate::run_gate(&args[1..]);
    }
    ExitCode::from(run(
        &args,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    ))
}

/// Run one CLI request with injectable streams for tests.
pub fn run(args: &[OsString], stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    parser::run(args, stdout, stderr)
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
mod adapter_layout_tests {
    use std::path::Path;

    #[test]
    fn registered_adapters_have_consistent_paths() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let categories: [(&str, Vec<&str>, &[&str]); 5] = [
            ("agents", super::agents::names().collect(), &["mod.rs"]),
            (
                "providers",
                super::providers::names().to_vec(),
                &["mod.rs", "preparation.rs"],
            ),
            (
                "workloads",
                super::workloads::external_adapter_names().collect(),
                &["mod.rs"],
            ),
            ("proxy", super::proxy::names().collect(), &["mod.rs"]),
            (
                "collectors",
                super::collectors::names().collect(),
                &["mod.rs"],
            ),
        ];
        for (category, ids, root_modules) in categories {
            let root = source.join(category);
            let adapters = root.join("adapters");
            assert!(
                adapters.join("mod.rs").is_file(),
                "{category} registry missing"
            );
            assert!(!source.join(format!("{category}.rs")).exists());
            for entry in std::fs::read_dir(&root).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    assert_eq!(
                        entry.file_name(),
                        "adapters",
                        "unexpected adapter directory: {}",
                        path.display()
                    );
                }
                if path.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                    let name = entry.file_name();
                    assert!(
                        root_modules.iter().any(|allowed| name == *allowed),
                        "target-specific module outside adapters: {}",
                        path.display()
                    );
                }
            }
            for id in ids {
                let stem = id.replace('-', "_");
                assert!(
                    adapters.join(format!("{stem}.rs")).is_file()
                        || adapters.join(format!("{stem}_agent.rs")).is_file(),
                    "missing {category} adapter file for {id}"
                );
            }
            for entry in std::fs::read_dir(&adapters).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    assert!(
                        adapters
                            .join(format!("{}.rs", entry.file_name().to_string_lossy()))
                            .is_file(),
                        "adapter helper directory has no owning adapter: {}",
                        path.display()
                    );
                }
            }
        }
    }
}

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
            vec!["compare", "--help"],
            vec!["provider", "--help"],
            vec!["provider", "models", "--help"],
            vec!["tool", "--help"],
            vec!["tool", "list", "--help"],
            vec!["tool", "install", "--help"],
            vec!["workload", "--help"],
            vec!["workload", "list", "--help"],
            vec!["workload", "install", "--help"],
            vec!["workload", "tui", "--help"],
        ] {
            let install_help = words == ["tool", "install", "--help"];
            let args = words.into_iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            assert_eq!(run(&args, &mut stdout, &mut stderr), 0);
            assert!(String::from_utf8_lossy(&stdout).starts_with("Usage:"));
            if install_help {
                assert!(String::from_utf8_lossy(&stdout).contains("provider bridge"));
            }
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn generated_completion_is_hidden_from_help_and_available_internally() {
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
        assert_eq!(run(&args, &mut stdout, &mut Vec::new()), 0);
        let script = String::from_utf8(stdout).unwrap();
        assert!(script.starts_with("# bash completion for asb"));
        assert!(script.contains("compare"));
    }

    #[test]
    fn parser_rejects_freestanding_arguments_and_conflicting_modes() {
        let mut stderr = Vec::new();
        assert_eq!(
            run(&[OsString::from("run")], &mut Vec::new(), &mut stderr),
            2
        );
        assert!(String::from_utf8_lossy(&stderr).contains("CONFIG"));
        stderr.clear();
        assert_eq!(
            run(
                &["run".into(), "a.toml".into(), "b.toml".into()],
                &mut Vec::new(),
                &mut stderr
            ),
            2
        );
        assert!(String::from_utf8_lossy(&stderr).contains("unexpected argument"));
        stderr.clear();
        assert_eq!(
            run(
                &[
                    "run".into(),
                    "a.toml".into(),
                    "--record".into(),
                    "--replay".into()
                ],
                &mut Vec::new(),
                &mut stderr
            ),
            2
        );
        assert!(String::from_utf8_lossy(&stderr).contains("cannot be used with"));
        stderr.clear();
        assert_eq!(
            run(
                &[
                    "run".into(),
                    "a.toml".into(),
                    "--respect-record-timings".into()
                ],
                &mut Vec::new(),
                &mut stderr,
            ),
            2
        );
        assert!(String::from_utf8_lossy(&stderr).contains("requires --replay"));
    }
}
