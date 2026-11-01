//! Small command line front end for configuring and running ASB benchmarks.

mod agents;
mod benchmark;
mod benchmark_paths;
pub mod cassette;
mod collectors;
mod compare;
mod completion;
mod config;
mod disk_space;
mod endpoint;
mod evidence;
#[cfg(all(test, target_os = "linux"))]
#[path = "../tests/unit/executable_fixture_writer.rs"]
mod executable_fixture_writer_tests;
mod executables;
#[cfg(test)]
#[path = "../tests/support/fixture_namespace.rs"]
pub(crate) mod fixture_namespace;
#[cfg(all(test, unix))]
#[path = "../tests/support/fixture_writer.rs"]
pub(crate) mod fixture_writer;
#[cfg(unix)]
mod helper_exchange;
mod http_server;
mod install;
mod launch;
#[cfg(all(test, target_os = "linux"))]
#[path = "../tests/support/material_guardian.rs"]
pub(crate) mod material_guardian;
mod measurements;
mod models;
mod output_lock;
#[cfg(unix)]
mod owned_fd_exec;
mod parser;
mod planning;
mod presets;
mod process_cleanup;
mod process_gate;
mod process_termination;
mod project;
mod providers;
mod proxy;
mod python_runtime;
#[cfg(all(test, unix))]
#[path = "../tests/support/recording_provider.rs"]
pub(crate) mod recording_provider_fixture;
mod runner;
mod runtime_inputs;
mod selection_names;
mod suggestions;
mod sweep;
mod tape;
#[cfg(all(test, unix))]
#[path = "../tests/support/mod.rs"]
pub(crate) mod test_support;
#[cfg(all(test, unix))]
impl fixture_writer::Keeper for process_cleanup::OwnedReaper {
    fn adopt(&self, child: std::process::Child) {
        self.adopt(child);
    }
}
#[cfg(all(test, unix))]
pub(crate) fn write_test_script(
    root: &std::path::Path,
    path: &std::path::Path,
    bytes: &[u8],
    mode: Option<u32>,
) -> std::io::Result<()> {
    fixture_writer::write_with(root, path, bytes, mode, || {
        process_cleanup::OwnedReaper::new("test-executable-writer").map_err(std::io::Error::other)
    })
}
#[cfg(all(test, unix))]
pub(crate) fn write_test_file(
    root: &std::path::Path,
    path: &std::path::Path,
    bytes: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    write_test_script(root, path, bytes.as_ref(), None)
}
mod tools;
mod tui;
mod usage;
mod workloads;

use std::ffi::OsString;
use std::io::{self, IsTerminal, Read, Write};
use std::process::ExitCode;

/// Run the process CLI.
#[must_use]
pub fn entry(args: Vec<OsString>) -> ExitCode {
    if args.first().is_some_and(|arg| arg == "__asb-owned-fd-exec") {
        #[cfg(unix)]
        return owned_fd_exec::entry(&args[1..]);
        #[cfg(not(unix))]
        return ExitCode::from(2);
    }
    if args.first().is_some_and(|arg| arg == "__asb-staging") {
        return workloads::staging::entry(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "__asb-material") {
        #[cfg(target_os = "linux")]
        return workloads::census::entry(&args[1..]);
        #[cfg(not(target_os = "linux"))]
        return ExitCode::from(2);
    }
    if let Some(code) = collectors::legacy_private_service_entry(&args) {
        return code;
    }
    if args
        .first()
        .is_some_and(|arg| arg == "__asb-collector-service")
    {
        return collectors::private_service_entry(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "__asb-namespace") {
        return match agents::driver::namespace_id() {
            Ok(namespace) => {
                println!("{namespace}");
                ExitCode::SUCCESS
            }
            Err(_) => ExitCode::from(2),
        };
    }
    if args.first().is_some_and(|arg| arg == "__asb-target") {
        return workloads::target::entry(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "__asb-relay") {
        return workloads::relay::entry(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "__asb-agent") {
        return agents::driver::entry();
    }
    if args.first().is_some_and(|arg| arg == "__asb-tool-replay") {
        return agents::tool_replay_mcp::entry(&args[1..]);
    }
    if let Some(code) = agents::legacy_bridge_entry(&args) {
        return code;
    }
    if args.first().is_some_and(|arg| arg == "__asb-agent-bridge") {
        return agents::agent_bridge_entry(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "__asb-mcp-relay") {
        return agents::mcp_relay::entry(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "__asb-gate") {
        return process_gate::run_gate(&args[1..]);
    }
    if args
        .first()
        .is_some_and(|arg| arg == "__asb-native-trace-launch")
    {
        #[cfg(unix)]
        return process_gate::trace_launch(&args[1..]);
        #[cfg(not(unix))]
        return ExitCode::from(2);
    }
    if args.first().is_some_and(|arg| arg == "__asb-install") {
        return install::operation::entry(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "__asb-http-server") {
        return http_server::entry(&args[1..]);
    }
    // Do not retain either global stream lock over dispatch. Install transfer
    // observers render from owned worker threads and are joined before the
    // command returns; retaining stderr here would make the main thread wait
    // for a worker that is itself waiting to acquire that same lock.
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let interactive = stdout.is_terminal() && stderr.is_terminal();
    ExitCode::from(parser::run_with_interactive(
        &args,
        &mut stdout,
        &mut stderr,
        interactive,
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
    read_config_after_inspection(path, || {})
}

fn read_config_after_inspection(
    path: &std::path::Path,
    inspected: impl FnOnce(),
) -> Result<String, CliError> {
    const MAX_CONFIG_BYTES: u64 = crate::benchmark::MAX_CONFIG_BYTES as u64;
    let oversized = || {
        CliError::Input(format!(
            "benchmark configuration {} is larger than 64 KiB",
            path.display()
        ))
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Open the selected object without blocking on a FIFO; symlinks to
        // ordinary configuration files retain their established behavior.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|error| CliError::Input(format!("cannot open {}: {error}", path.display())))?;
    let metadata = file
        .metadata()
        .map_err(|error| CliError::Input(format!("cannot inspect {}: {error}", path.display())))?;
    if !metadata.is_file() {
        return Err(CliError::Input(format!(
            "benchmark configuration {} must be a regular file",
            path.display()
        )));
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(oversized());
    }
    inspected();
    let mut contents = Vec::new();
    // A concurrent writer can grow a regular file after metadata inspection.
    // Bound the actual read as well as the opened object's initial length.
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut contents)
        .map_err(|error| CliError::Input(format!("cannot read {}: {error}", path.display())))?;
    if contents.len() as u64 > MAX_CONFIG_BYTES {
        return Err(oversized());
    }
    String::from_utf8(contents).map_err(|error| {
        CliError::Input(format!(
            "benchmark configuration {} is not valid UTF-8: {error}",
            path.display()
        ))
    })
}

#[cfg(test)]
#[path = "../tests/unit/config_reader.rs"]
mod config_reader_tests;

pub(crate) use CliError as Error;

#[cfg(test)]
mod adapter_layout_tests {
    use std::path::Path;

    struct CategoryLayout<'a> {
        category: &'a str,
        ids: Vec<&'a str>,
        root_modules: &'a [&'a str],
        root_directories: &'a [&'a str],
        shared_family_adapters: &'a [(&'a str, &'a str)],
    }

    #[test]
    fn registered_adapters_have_consistent_paths() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let categories = [
            CategoryLayout {
                category: "agents",
                ids: super::agents::names().collect(),
                // Attempt artifact ownership, the sealed causal local-tool replay
                // transport, deterministic-execution manifests, and bounded
                // native workspace capsules are generic adapter infrastructure.
                // Individual adapters only opt in.
                root_modules: &[
                    "mod.rs",
                    "driver.rs",
                    "artifacts.rs",
                    "capsule.rs",
                    "determinism.rs",
                    "mcp_framing.rs",
                    "mcp_relay.rs",
                    "mcp_transcript.rs",
                    "native_trace.rs",
                    "tool_replay.rs",
                    "tool_replay_mcp.rs",
                ],
                root_directories: &[],
                shared_family_adapters: &[],
            },
            CategoryLayout {
                category: "providers",
                ids: super::providers::names().to_vec(),
                root_modules: &["mod.rs", "preparation.rs"],
                root_directories: &[],
                shared_family_adapters: &[],
            },
            CategoryLayout {
                category: "workloads",
                ids: super::workloads::external_adapter_names().collect(),
                // Task/session contracts are category-wide, not adapters.
                root_modules: &[
                    "mod.rs",
                    "session.rs",
                    "material.rs",   // Generic bounded initial-file material identity.
                    "census.rs",     // Descriptor-owned guest material observations.
                    "attributes.rs", // Bounded visible Linux attribute observations.
                    "contracts.rs",  // Generic module metadata and protocol schema.
                    "evaluation.rs", // Known producer categories and extensible saved classification.
                    "selection.rs",  // Generic pre-execution catalog/identity contract.
                    "recovery.rs",   // Generic attempt references and adapter dispatch.
                    "outcome.rs",    // Generic evaluation, metric and lineage contract.
                    "summary.rs",    // Workload-resolution summary publication.
                    "staging.rs",    // Fixed-prefix guest instrumentation ownership.
                    "controller.rs",
                    "task_protocol.rs", // Generic private task request/event/grade contracts.
                    "remote.rs",
                    "relay.rs",
                    "target.rs",
                ],
                root_directories: &["controller"], // Generic Unix/legacy protocol transports.
                // LiveCodeBench publishes versioned scenario populations from
                // one pinned official harness; its one owning adapter module
                // deliberately owns only this explicit ID family.
                shared_family_adapters: &[("livecodebench", "livecodebench.rs")],
            },
            CategoryLayout {
                category: "proxy",
                ids: super::proxy::names().collect(),
                // Category-wide capture evidence, not a provider/target adapter.
                root_modules: &["mod.rs", "capture.rs", "tool_calls.rs", "usage.rs"],
                root_directories: &[],
                shared_family_adapters: &[],
            },
            CategoryLayout {
                category: "collectors",
                ids: super::collectors::names().collect(),
                // Category-wide bounded output codec, not a target adapter.
                root_modules: &["mod.rs", "output.rs"],
                root_directories: &[],
                shared_family_adapters: &[],
            },
        ];
        for CategoryLayout {
            category,
            ids,
            root_modules,
            root_directories,
            shared_family_adapters,
        } in categories
        {
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
                    let name = entry.file_name();
                    assert!(
                        name == "adapters"
                            || root_directories.iter().any(|allowed| name == *allowed),
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
                let stem = id.replace(['-', '.'], "_");
                assert!(
                    !root_directories.contains(&stem.as_str()),
                    "registered {category} adapter {id} cannot be a category-wide directory"
                );
                // A new generic module must not accidentally allow an owning
                // target adapter back into the category root.
                for filename in [format!("{stem}.rs"), format!("{stem}_agent.rs")] {
                    assert!(
                        !root_modules.contains(&filename.as_str()),
                        "registered {category} adapter {id} cannot be a category-wide module"
                    );
                }
                let direct_owner = adapters.join(format!("{stem}.rs")).is_file()
                    || adapters.join(format!("{stem}_agent.rs")).is_file();
                let shared_family_owner = shared_family_adapters.iter().any(|(family, file)| {
                    (id == *family || id.starts_with(&format!("{family}-")))
                        && adapters.join(file).is_file()
                });
                assert!(
                    direct_owner || shared_family_owner,
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
            vec!["workload", "recover", "--help"],
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
