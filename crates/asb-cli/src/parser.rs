//! The single typed argument parser for the `asb` executable.

use super::{
    CliError, Error, completion, config, http_server, install, models, project, runner, sweep,
    tape, tui,
};
use clap::{ArgGroup, Args, CommandFactory, Parser, Subcommand};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Use the parser's schema for completions.  The real parser deliberately
/// accepts custom tool and workload sources; this completion-only command adds
/// the registered choices without changing that runtime contract.
pub(crate) fn completion_command() -> clap::Command {
    let providers = crate::providers::names();
    let workloads = registered_workload_names();
    let models = registered_model_names();
    Cli::command()
        .mut_subcommand("setup", |setup| {
            setup
                .mut_arg("agent", |arg| possible(arg, crate::agents::names()))
                .mut_arg("workload", |arg| possible(arg, workloads.clone()))
                .mut_arg("provider", |arg| possible(arg, providers.iter().copied()))
                .mut_arg("model", |arg| possible_owned(arg, models.clone()))
                .mut_arg("measure", |arg| {
                    let mut values = crate::measurements::available_collectors();
                    for collector in crate::collectors::all() {
                        if let Some(extension) = collector.selection_extension() {
                            values.extend(extension.all_selection);
                            values.extend(
                                extension.options.into_iter().map(|option| option.selection),
                            );
                        }
                    }
                    possible_owned(arg, values)
                })
        })
        .mut_subcommand("provider", |provider| {
            provider.mut_subcommand("models", |models| {
                models.mut_arg("provider", |arg| possible(arg, providers.iter().copied()))
            })
        })
        .mut_subcommand("tool", |tool| {
            tool.mut_subcommand("install", |install| {
                install.mut_arg("name", |arg| possible(arg, crate::install::tool_names()))
            })
        })
        .mut_subcommand("workload", |workload| {
            workload
                .mut_arg("name", |arg| possible(arg, registered_workload_names()))
                .mut_subcommand("describe", |describe| {
                    describe.mut_arg("name", |arg| possible(arg, workloads.clone()))
                })
                .mut_subcommand("install", |install| {
                    install.mut_arg("source", |arg| possible(arg, workloads))
                })
        })
        .mut_subcommand("measure", |measure| {
            measure.mut_arg("name", |arg| {
                possible(
                    arg,
                    crate::collectors::all()
                        .iter()
                        .filter(|adapter| adapter.selection_extension().is_some())
                        .map(|adapter| adapter.id()),
                )
            })
        })
}

fn possible<I, T>(argument: clap::Arg, values: I) -> clap::Arg
where
    I: IntoIterator<Item = T>,
    T: Into<clap::builder::PossibleValue>,
{
    argument.value_parser(clap::builder::PossibleValuesParser::new(values))
}

fn possible_owned(argument: clap::Arg, values: Vec<String>) -> clap::Arg {
    // Clap's generated completion schema owns no dynamic values. These strings
    // live only for the brief, one-shot generator invocation.
    possible(
        argument,
        values
            .into_iter()
            .map(|value| Box::leak(value.into_boxed_str()) as &'static str),
    )
}

fn registered_workload_names() -> Vec<&'static str> {
    let mut values = crate::workloads::builtin_names()
        .chain(crate::install::external_workload_names())
        .collect::<Vec<_>>();
    values.sort_unstable();
    values.dedup();
    values
}

fn registered_model_names() -> Vec<String> {
    let mut values = Vec::new();
    for provider in crate::providers::names() {
        if let Some(models) = crate::providers::known_models(provider) {
            for model in models {
                values.push((*model).to_owned());
            }
        }
    }
    values.sort();
    values.dedup();
    values
}

pub(crate) fn run(args: &[OsString], stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    run_with_interactive(args, stdout, stderr, false)
}

/// Execute with the terminal capability observed by the process entry point.
/// Injectable writers deliberately never infer terminal control support.
pub(crate) fn run_with_interactive(
    args: &[OsString],
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    interactive: bool,
) -> u8 {
    // Like the native gate helpers, completion generation is an internal entry
    // point, not a public option to duplicate in generated shell candidates.
    const COMPLETION_FLAG: &str = "--__generate-completion";
    const VALUES_FLAG: &str = "--__complete-values";
    let separate = args.first().is_some_and(|arg| arg == COMPLETION_FLAG);
    let joined = args
        .first()
        .and_then(|arg| arg.to_str())
        .and_then(|arg| arg.strip_prefix(COMPLETION_FLAG))
        .and_then(|suffix| suffix.strip_prefix('='));
    if separate || joined.is_some() {
        if args.len() != if separate { 2 } else { 1 } {
            return fail(
                stderr,
                "completion helper requires exactly one shell: bash or zsh",
            );
        }
        let shell = if separate { args[1].to_str() } else { joined };
        let result = match shell {
            Some("bash") => completion::bash(stdout),
            Some("zsh") => completion::zsh(stdout),
            _ => Err(Error::Input("completion shell must be bash or zsh".into())),
        };
        return result.map_or_else(|error| fail(stderr, &error.to_string()), |_| 0);
    }
    if args.first().is_some_and(|arg| arg == VALUES_FLAG) {
        return completion::values(&args[1..], stdout)
            .map_or_else(|error| fail(stderr, &error.to_string()), |_| 0);
    }
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(OsString::from("asb"));
    argv.extend_from_slice(args);
    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) => {
            let mut text = error.render().to_string();
            if let Some(suggestion) = close_command(args) {
                text.push_str(&format!("\nasb: did you mean `{suggestion}`?\n"));
            }
            if error.use_stderr() {
                let _ = write!(stderr, "{text}");
            } else {
                let _ = write!(stdout, "{text}");
            }
            return error.exit_code() as u8;
        }
    };
    // `init` establishes a project and help has no project effect. Every
    // operational command is project-local, including listing and discovery.
    let requires_project =
        cli.command.is_some() && !matches!(cli.command.as_ref(), Some(CommandLine::Init(_)));
    let base = match std::env::current_dir() {
        Ok(cwd) => cli
            .project_dir
            .as_ref()
            .map_or(cwd.clone(), |path| cwd.join(path)),
        Err(error) => return fail(stderr, &io_error(error).to_string()),
    };
    if base.to_str().is_none()
        && matches!(
            cli.command.as_ref(),
            Some(CommandLine::Run(_) | CommandLine::Sweep(_) | CommandLine::Compare(_))
        )
    {
        return fail(
            stderr,
            "cannot represent this invocation directory as UTF-8 for the selected command",
        );
    }
    if requires_project {
        let checked = if cli.project_dir.is_some() {
            project::require_project_directory(&base)
        } else {
            project::require_initialized(&base)
        };
        if let Err(error) = checked {
            return fail(stderr, &error.to_string());
        }
    }
    let _network = requires_project.then(|| project::network_project_at(&base, &base));
    let resolve = |path: &str| base.join(path).to_string_lossy().into_owned();
    let result = match cli.command {
        None => stdout
            .write_all(Cli::command().render_help().to_string().as_bytes())
            .map_err(io_error),
        Some(CommandLine::Init(mut options)) => {
            if cli.project_dir.is_some() {
                options.root = Some(
                    options
                        .root
                        .map_or_else(|| base.clone(), |root| base.join(root)),
                );
            }
            if options.configure_host {
                project::configure_host(options.root, options.yes, stdout)
            } else {
                project::init(
                    options.root,
                    options.reset,
                    options.clean,
                    options.yes,
                    stdout,
                )
            }
        }
        Some(CommandLine::Setup(options)) => match options.mode {
            Some(SetupMode::Tui) => {
                tui::setup_at(options.input.as_deref(), options.last, &base, stdout)
            }
            None => {
                if let Some(preset) = options.preset.as_deref() {
                    config::setup_preset_at(
                        preset,
                        options.workload.clone(),
                        options.output.clone(),
                        &base,
                        stdout,
                    )
                } else {
                    config::setup_at(options.into(), &base, stdout)
                }
            }
        },
        Some(CommandLine::Run(options)) => {
            let tape = match tape_selection(TapeFlags::from_run(&options)) {
                Ok(selection) => selection,
                Err(error) => return fail(stderr, &error.to_string()),
            };
            if (options.track.is_some() || options.seed.is_some())
                && tape.llm != Some(tape::Mode::Replay)
            {
                return fail(
                    stderr,
                    "--track and --seed require --llm-replay or --replay",
                );
            }
            if options.cassette.is_some() && tape.carrier.is_none() {
                return fail(
                    stderr,
                    "--cassette requires --record, --replay, --llm-record, --llm-replay, --tool-record, --tool-replay, --mcp-record, or --mcp-replay",
                );
            }
            if options.respect_record_timings && tape.llm != Some(tape::Mode::Replay) {
                return fail(
                    stderr,
                    "--respect-record-timings requires --replay or --llm-replay; recording always stores response delays",
                );
            }
            let report_path = resolve(&options.path);
            let measurement_dir = options.measurements.as_deref().map(&resolve);
            let cassette = options.cassette.as_deref().map(&resolve);
            let native_capsule_record = options.native_capsule_record.as_deref().map(&resolve);
            let native_capsule_replay = options.native_capsule_replay.as_deref().map(&resolve);
            let json = options.json;
            runner::run_with_tape(
                &report_path,
                runner::RunOptions {
                    invocation_dir: Some(&base),
                    track: options.track,
                    replay_seed: options.seed,
                    repeat: options.repeat.or(options.record.then_some(1)),
                    max_steps: options.max_steps,
                    json: options.json,
                    show_output: options.show_output,
                    quiet_output: !options.verbose,
                    announce_trial: !options.json,
                    measurement_dir: measurement_dir.as_deref(),
                    mode: tape.carrier,
                    tool_mode: tape.tool,
                    tool_mode_explicit: tape.tool_explicit,
                    mcp_mode: tape.mcp,
                    cassette: cassette.as_deref(),
                    native_capsule_record: native_capsule_record.as_deref(),
                    native_capsule_replay: native_capsule_replay.as_deref(),
                    respect_record_timings: options.respect_record_timings,
                    interactive: interactive && !options.json,
                    ..Default::default()
                },
                stdout,
            )
            .and_then(|_| {
                if json {
                    Ok(())
                } else if options.verbose {
                    super::compare::generate(&report_path, stdout).map(|_| ())
                } else {
                    super::compare::run_links(&report_path, stdout)
                }
            })
        }
        Some(CommandLine::Sweep(options)) => {
            let tape = match tape_selection(TapeFlags::from_sweep(&options)) {
                Ok(selection) => selection,
                Err(error) => return fail(stderr, &error.to_string()),
            };
            if (options.track.is_some() || options.seed.is_some())
                && tape.llm != Some(tape::Mode::Replay)
            {
                return fail(
                    stderr,
                    "--track and --seed require --llm-replay or --replay",
                );
            }
            if options.respect_record_timings && tape.llm != Some(tape::Mode::Replay) {
                return fail(
                    stderr,
                    "--respect-record-timings requires --replay or --llm-replay; recording always stores response delays",
                );
            }
            let generated_preset = match options.preset.as_deref() {
                Some(preset) => {
                    match config::setup_preset_for_sweep(preset, options.json, &base, stdout) {
                        Ok(path) => Some(path),
                        Err(error) => return fail(stderr, &error.to_string()),
                    }
                }
                None => None,
            };
            let path = generated_preset
                .as_ref()
                .map(|preset| preset.path.to_string_lossy().into_owned())
                .or(options.path.as_deref().map(&resolve))
                .expect("clap requires either a sweep path or a preset");
            if generated_preset.is_some()
                && !options.json
                && let Err(error) = writeln!(stdout, "Starting generated preset sweep: {}", path)
            {
                return fail(stderr, &io_error(error).to_string());
            }
            let cassette_root = options.cassette_root.as_deref().map(|path| base.join(path));
            sweep::run_with_options(
                &path,
                sweep::Options {
                    invocation_dir: Some(&base),
                    repeat: options
                        .repeat
                        .or(generated_preset.as_ref().map(|preset| preset.repeat)),
                    max_steps: options.max_steps,
                    track: options.track,
                    seed: options.seed,
                    json: options.json,
                    show_output: options.show_output,
                    quiet_output: !options.verbose,
                    interactive: interactive && !options.json,
                    mode: tape.carrier,
                    tool_mode: tape.tool,
                    tool_mode_explicit: tape.tool_explicit,
                    mcp_mode: tape.mcp,
                    respect_record_timings: options.respect_record_timings,
                    cassette_root: cassette_root.as_deref(),
                    jobs: usize::from(options.jobs),
                    max_active_attempts: options.max_active_attempts.map(usize::from),
                    resume: options.resume || options.continue_after_failures.is_some(),
                    continue_after_failures: options
                        .continue_after_failures
                        .map(|value| value.unwrap_or(usize::MAX)),
                    reconcile_completed: options.reconcile_completed,
                    import_grades: options.import_grades,
                    recover_recording: options.recover_recording,
                    recover_continuation: options.recover_continuation,
                },
                stdout,
            )
        }
        Some(CommandLine::Compare(options)) => {
            super::compare::generate(&resolve(&options.path), stdout).and_then(|report| {
                if options.report {
                    super::compare::open_report(&report)
                } else {
                    Ok(())
                }
            })
        }
        Some(CommandLine::Provider(options)) => match options.command {
            ProviderCommand::Models(args) => models::list(args.into(), stdout),
        },
        Some(CommandLine::Tool(options)) => match options.command {
            ToolCommand::List(_) => install::tool_list(stdout),
            ToolCommand::Install(args) => {
                let mut previous = None;
                install::tool_with_progress_at(args.into(), &base, stdout, &mut |percent, stage| {
                    if previous.replace(stage) != Some(stage) {
                        let _ = writeln!(stderr, "Installing: {} ({percent}%)", stage.label());
                    }
                })
            }
            ToolCommand::Http(args) => match args.command {
                HttpCommand::Start => http_server::start_manual(&base, stdout),
                HttpCommand::Stop => http_server::stop_manual(&base, stdout),
            },
        },
        Some(CommandLine::Workload(options)) => match options.command {
            Some(WorkloadCommand::List(args)) => {
                install::workload_list_at(args.into(), &base, stdout)
            }
            Some(WorkloadCommand::Describe(args)) => {
                crate::workloads::describe_module(&args.name, stdout)
            }
            Some(WorkloadCommand::Install(args)) => {
                let mut previous = None;
                install::workload_with_progress_at(
                    args.into(),
                    &base,
                    stdout,
                    &mut |percent, stage| {
                        if previous.replace(stage) != Some(stage) {
                            let _ = writeln!(stderr, "Installing: {} ({percent}%)", stage.label());
                        }
                    },
                )
            }
            Some(WorkloadCommand::Trials) => options.name.as_deref().map_or_else(
                || {
                    Err(Error::Input(
                        "`asb workload WORKLOAD trials` requires a workload name".into(),
                    ))
                },
                |name| list_workload_trials(name, &base, stdout),
            ),
            Some(WorkloadCommand::Tui) => tui::workload_tui_at(&base, stdout),
            Some(WorkloadCommand::Recover(args)) => {
                if args.grade {
                    crate::workloads::recovery::grade_retained(
                        &base.join(&args.reference),
                        args.json,
                        stdout,
                    )
                } else if args.inspect_grade {
                    crate::workloads::recovery::inspect_grade(
                        &base.join(&args.reference),
                        args.json,
                        stdout,
                    )
                } else {
                    crate::workloads::recovery::run(&base.join(&args.reference), args.json, stdout)
                }
            }
            None => Err(Error::Input(
                "choose a workload command, or use `asb workload WORKLOAD trials`".into(),
            )),
        },
        Some(CommandLine::Measure(options)) => match (options.name, options.command) {
            (None, None) => list_measures(stdout),
            (Some(name), Some(MeasureCommand::Programs)) => list_collector_options(&name, stdout),
            _ => Err(Error::Input(
                "use `asb measure` or `asb measure COLLECTOR programs`".into(),
            )),
        },
    };
    match result {
        Ok(()) => 0,
        Err(error) => fail(stderr, &error.to_string()),
    }
}

fn close_command(args: &[OsString]) -> Option<&'static str> {
    let first = args.first()?.to_str()?;
    let (value, choices): (&str, &[&str]) = match first {
        "provider" => (args.get(1)?.to_str()?, &["models"]),
        "tool" => (args.get(1)?.to_str()?, &["list", "install"]),
        "workload" => (
            args.get(1)?.to_str()?,
            &["list", "install", "trials", "tui", "recover"],
        ),
        "measure" => (args.get(2)?.to_str()?, &["programs"]),
        _ => (
            first,
            &[
                "init", "setup", "run", "sweep", "compare", "provider", "tool", "workload",
                "measure",
            ],
        ),
    };
    super::suggestions::closest(value, choices)
}

#[derive(Clone, Copy)]
struct TapeFlags {
    record: bool,
    replay: bool,
    legacy_record: bool,
    legacy_replay: bool,
    llm_record: bool,
    llm_replay: bool,
    tool_record: bool,
    tool_replay: bool,
    mcp_record: bool,
    mcp_replay: bool,
}

impl TapeFlags {
    fn from_run(args: &RunArgs) -> Self {
        Self {
            // The concise selectors deliberately enable both recording layers.
            // The current sealed cassette has one lifecycle, so selecting an
            // explicit layer still reserves the same private carrier. This
            // preserves a causal tool transcript beside its provider tape.
            record: args.record || args.llm_record || args.tool_record || args.mcp_record,
            replay: args.replay || args.llm_replay || args.tool_replay || args.mcp_replay,
            legacy_record: args.record,
            legacy_replay: args.replay,
            llm_record: args.llm_record,
            llm_replay: args.llm_replay,
            tool_record: args.tool_record,
            tool_replay: args.tool_replay,
            mcp_record: args.mcp_record,
            mcp_replay: args.mcp_replay,
        }
    }

    fn from_sweep(args: &SweepArgs) -> Self {
        Self {
            record: args.record || args.llm_record || args.tool_record || args.mcp_record,
            replay: args.replay || args.llm_replay || args.tool_replay || args.mcp_replay,
            legacy_record: args.record,
            legacy_replay: args.replay,
            llm_record: args.llm_record,
            llm_replay: args.llm_replay,
            tool_record: args.tool_record,
            tool_replay: args.tool_replay,
            mcp_record: args.mcp_record,
            mcp_replay: args.mcp_replay,
        }
    }
}

fn tape_mode(flags: TapeFlags) -> Result<Option<tape::Mode>, CliError> {
    match (flags.record, flags.replay) {
        (true, true) => Err(CliError::Input(
            "record and replay selections are mutually exclusive; choose one direction".into(),
        )),
        (true, false) => Ok(Some(tape::Mode::Record)),
        (false, true) => Ok(Some(tape::Mode::Replay)),
        (false, false) => Ok(None),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TapeSelection {
    /// The cassette lifecycle. Tool-only traffic still needs the sealed
    /// carrier because local-tool evidence is a private cassette sidecar.
    carrier: Option<tape::Mode>,
    llm: Option<tape::Mode>,
    tool: Option<tape::Mode>,
    /// The tool selection is resolved explicitly, including its absence for
    /// LLM-only or MCP-only flags. A selected tool mode requires interception.
    tool_explicit: bool,
    mcp: Option<tape::Mode>,
}

fn tape_selection(flags: TapeFlags) -> Result<TapeSelection, CliError> {
    let carrier = tape_mode(flags)?;
    let direction = carrier;
    Ok(TapeSelection {
        carrier,
        llm: (flags.legacy_record || flags.legacy_replay || flags.llm_record || flags.llm_replay)
            .then_some(direction)
            .flatten(),
        tool: (flags.legacy_record
            || flags.legacy_replay
            || flags.tool_record
            || flags.tool_replay)
            .then_some(direction)
            .flatten(),
        tool_explicit: flags.tool_record
            || flags.tool_replay
            || (!(flags.legacy_record || flags.legacy_replay)
                && (flags.llm_record || flags.llm_replay || flags.mcp_record || flags.mcp_replay)),
        mcp: (flags.mcp_record || flags.mcp_replay)
            .then_some(direction)
            .flatten(),
    })
}

fn fail(stderr: &mut dyn Write, message: &str) -> u8 {
    let _ = writeln!(stderr, "asb: {message}");
    2
}

fn io_error(error: io::Error) -> CliError {
    CliError::Io(error.to_string())
}

fn list_measures(output: &mut dyn Write) -> Result<(), Error> {
    for id in crate::measurements::available_collectors() {
        let description = crate::measurements::collector_description(&id).unwrap_or("collector");
        writeln!(output, "{id}\t{description}").map_err(|error| Error::Io(error.to_string()))?;
    }
    Ok(())
}

fn list_collector_options(name: &str, output: &mut dyn Write) -> Result<(), Error> {
    if crate::collectors::find(name).is_none() {
        let names = crate::collectors::all()
            .iter()
            .map(|adapter| adapter.id())
            .collect::<Vec<_>>();
        return Err(Error::Input(crate::suggestions::unknown(
            "collector",
            name,
            &names,
        )));
    }
    let extension = crate::collectors::find(name)
        .and_then(|adapter| adapter.selection_extension())
        .ok_or_else(|| Error::Input(format!("collector `{name}` has no selectable programs; use `asb measure` to list collectors")))?;
    if extension.all_selection.is_some() {
        writeln!(output, "all\t{}", extension.all_description)
            .map_err(|error| Error::Io(error.to_string()))?;
    }
    for option in extension.options {
        writeln!(output, "{}\t{}", option.label, option.description)
            .map_err(|error| Error::Io(error.to_string()))?;
    }
    Ok(())
}

fn list_workload_trials(name: &str, base: &Path, output: &mut dyn Write) -> Result<(), Error> {
    let (root, project) = project::load(base)?;
    let directory = project::environment_path("ASB_WORKLOAD_DIR", base)
        .unwrap_or_else(|| project::resolve(&root, &project.workloads_dir));
    let trials = crate::workloads::installed_trials(name, &directory).map_err(Error::Input)?;
    if trials.is_empty() {
        return Err(Error::Input(format!(
            "workload `{name}` has no installed trials; run `asb workload install {name}` and retry"
        )));
    }
    for trial in trials {
        writeln!(output, "{}", trial.id).map_err(|error| Error::Io(error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tape_flag_tests {
    use super::*;

    #[test]
    fn explicit_project_never_changes_another_threads_working_directory() {
        struct ObserveCwd {
            seen: Option<PathBuf>,
        }
        impl Write for ObserveCwd {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.seen = Some(std::thread::spawn(std::env::current_dir).join().unwrap()?);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let root = std::env::temp_dir().join(format!(
            "asb-project-concurrent-cwd-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(root.join(".asb")).unwrap();
        std::fs::write(
            root.join(".asb/project.toml"),
            toml::to_string(&project::ProjectConfig::default()).unwrap(),
        )
        .unwrap();
        let expected = std::env::current_dir().unwrap();
        let mut output = ObserveCwd { seen: None };
        let mut stderr = Vec::new();
        assert_eq!(
            run(
                &[
                    "tool".into(),
                    "list".into(),
                    "--project-dir".into(),
                    root.clone().into_os_string(),
                ],
                &mut output,
                &mut stderr,
            ),
            0,
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        assert_eq!(output.seen.as_deref(), Some(expected.as_path()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nested_project_commands_keep_independent_contexts() {
        struct Nested {
            inner_root: PathBuf,
            nested_output: Vec<u8>,
            invoked: bool,
        }
        impl Write for Nested {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if !self.invoked {
                    self.invoked = true;
                    let code = run(
                        &[
                            "workload".into(),
                            "list".into(),
                            "--project-dir".into(),
                            self.inner_root.clone().into_os_string(),
                        ],
                        &mut self.nested_output,
                        &mut Vec::new(),
                    );
                    assert_eq!(code, 0);
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let root = std::env::temp_dir().join(format!(
            "asb-nested-projects-{}",
            crate::tools::runtime::unique_id()
        ));
        for (project, workload) in [("a", "alpha"), ("b", "beta")] {
            let path = root.join(project);
            std::fs::create_dir_all(path.join(".asb")).unwrap();
            std::fs::create_dir_all(path.join("workloads")).unwrap();
            std::fs::write(
                path.join(".asb/project.toml"),
                toml::to_string(&project::ProjectConfig::default()).unwrap(),
            )
            .unwrap();
            std::fs::write(
                path.join("workloads").join(format!("{workload}.txt")),
                workload,
            )
            .unwrap();
        }
        let mut output = Nested {
            inner_root: root.join("b"),
            nested_output: Vec::new(),
            invoked: false,
        };
        assert_eq!(
            run(
                &[
                    "tool".into(),
                    "list".into(),
                    "--project-dir".into(),
                    root.join("a").into_os_string()
                ],
                &mut output,
                &mut Vec::new()
            ),
            0
        );
        let nested = String::from_utf8(output.nested_output).unwrap();
        assert!(nested.contains("beta"));
        assert!(!nested.contains("alpha"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn init_uses_explicit_project_directory() {
        let root = std::env::temp_dir().join(format!(
            "asb-init-explicit-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(root.join(".asb")).unwrap();
        std::fs::write(
            root.join(".asb/project.toml"),
            toml::to_string(&project::ProjectConfig::default()).unwrap(),
        )
        .unwrap();
        let mut output = Vec::new();
        let mut error = Vec::new();
        assert_eq!(
            run(
                &[
                    "--project-dir".into(),
                    root.clone().into_os_string(),
                    "init".into()
                ],
                &mut output,
                &mut error
            ),
            2
        );
        assert!(
            String::from_utf8(error)
                .unwrap()
                .contains("already initialized")
        );
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains(&root.display().to_string())
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_directory_is_global_and_restores_the_callers_directory() {
        let root = std::env::temp_dir().join(format!(
            "asb-parser-project-directory-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        project::init(Some(root.clone()), false, false, false, &mut Vec::new()).unwrap();
        let original = std::env::current_dir().unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        assert_eq!(
            run(
                &[
                    "tool".into(),
                    "list".into(),
                    "--project-dir".into(),
                    root.clone().into_os_string(),
                ],
                &mut stdout,
                &mut stderr,
            ),
            0
        );
        assert!(String::from_utf8(stdout).unwrap().contains("cli2key"));
        assert!(stderr.is_empty());
        assert_eq!(std::env::current_dir().unwrap(), original);

        // An explicit --project-dir must be the initialized root itself,
        // rather than borrowing this initialized parent's configuration.
        let missing = root.join("not-a-project");
        std::fs::create_dir_all(&missing).unwrap();
        let mut stderr = Vec::new();
        assert_eq!(
            run(
                &[
                    "tool".into(),
                    "list".into(),
                    "--project-dir".into(),
                    missing.clone().into_os_string(),
                ],
                &mut Vec::new(),
                &mut stderr,
            ),
            2
        );
        assert!(
            String::from_utf8(stderr)
                .unwrap()
                .contains("no initialized project found")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_llm_and_tool_flags_select_the_sealed_tape_direction() {
        let cli = Cli::try_parse_from([
            "asb",
            "run",
            "benchmark.toml",
            "--llm-record",
            "--tool-record",
        ])
        .unwrap();
        let Some(CommandLine::Run(args)) = cli.command else {
            panic!("expected run arguments");
        };
        let selection = tape_selection(TapeFlags::from_run(&args)).unwrap();
        assert_eq!(selection.llm, Some(tape::Mode::Record));
        assert_eq!(selection.tool, Some(tape::Mode::Record));
        assert!(selection.tool_explicit);
        assert_eq!(selection.mcp, None);

        let cli = Cli::try_parse_from(["asb", "run", "benchmark.toml", "--mcp-record"]).unwrap();
        let Some(CommandLine::Run(args)) = cli.command else {
            panic!("expected run arguments");
        };
        let selection = tape_selection(TapeFlags::from_run(&args)).unwrap();
        assert_eq!(selection.carrier, Some(tape::Mode::Record));
        assert_eq!(selection.llm, None);
        assert_eq!(selection.tool, None);
        assert_eq!(selection.mcp, Some(tape::Mode::Record));

        let cli = Cli::try_parse_from(["asb", "run", "benchmark.toml", "--mcp-replay"]).unwrap();
        let Some(CommandLine::Run(args)) = cli.command else {
            panic!("expected run arguments");
        };
        let selection = tape_selection(TapeFlags::from_run(&args)).unwrap();
        assert_eq!(selection.carrier, Some(tape::Mode::Replay));
        assert_eq!(selection.mcp, Some(tape::Mode::Replay));

        let cli =
            Cli::try_parse_from(["asb", "sweep", "sweep.toml", "--llmreplay", "--tool-replay"])
                .unwrap();
        let Some(CommandLine::Sweep(args)) = cli.command else {
            panic!("expected sweep arguments");
        };
        let selection = tape_selection(TapeFlags::from_sweep(&args)).unwrap();
        assert_eq!(selection.llm, Some(tape::Mode::Replay));
        assert_eq!(selection.tool, Some(tape::Mode::Replay));

        let cli = Cli::try_parse_from(["asb", "run", "benchmark.toml", "--llm-record"]).unwrap();
        let Some(CommandLine::Run(args)) = cli.command else {
            panic!("expected run arguments");
        };
        let selection = tape_selection(TapeFlags::from_run(&args)).unwrap();
        assert_eq!(selection.llm, Some(tape::Mode::Record));
        assert_eq!(selection.tool, None);
        assert!(selection.tool_explicit);

        let cli = Cli::try_parse_from(["asb", "run", "benchmark.toml", "--record"]).unwrap();
        let Some(CommandLine::Run(args)) = cli.command else {
            panic!("expected run arguments");
        };
        let selection = tape_selection(TapeFlags::from_run(&args)).unwrap();
        assert_eq!(selection.tool, Some(tape::Mode::Record));
        assert!(!selection.tool_explicit);
    }

    #[test]
    fn llm_only_modes_preserve_absence_of_tool_selection() {
        for flag in ["--llm-record", "--llm-replay"] {
            let cli = Cli::try_parse_from(["asb", "run", "benchmark.toml", flag]).unwrap();
            let Some(CommandLine::Run(args)) = cli.command else {
                panic!("expected run");
            };
            let selection = tape_selection(TapeFlags::from_run(&args)).unwrap();
            assert!(selection.tool.is_none());
            assert!(
                selection.tool_explicit,
                "a resolved absent tool selection must not fall back to the legacy mode"
            );
            let cli = Cli::try_parse_from(["asb", "sweep", "sweep.toml", flag]).unwrap();
            let Some(CommandLine::Sweep(args)) = cli.command else {
                panic!("expected sweep");
            };
            let selection = tape_selection(TapeFlags::from_sweep(&args)).unwrap();
            assert!(selection.tool.is_none() && selection.tool_explicit);
        }
    }

    #[test]
    fn run_and_sweep_verbose_flags_are_opt_in() {
        let cli = Cli::try_parse_from(["asb", "run", "benchmark.toml"]).unwrap();
        let Some(CommandLine::Run(args)) = cli.command else {
            panic!("expected run arguments");
        };
        assert!(!args.verbose);

        let cli = Cli::try_parse_from(["asb", "run", "benchmark.toml", "-v"]).unwrap();
        let Some(CommandLine::Run(args)) = cli.command else {
            panic!("expected run arguments");
        };
        assert!(args.verbose);

        let cli = Cli::try_parse_from(["asb", "sweep", "sweep.toml", "--verbose"]).unwrap();
        let Some(CommandLine::Sweep(args)) = cli.command else {
            panic!("expected sweep arguments");
        };
        assert!(args.verbose);
    }

    #[test]
    fn record_and_replay_directions_conflict_across_layers() {
        let error = Cli::try_parse_from([
            "asb",
            "run",
            "benchmark.toml",
            "--llm-record",
            "--tool-replay",
        ])
        .unwrap_err();
        assert!(error.to_string().contains("cannot be used with"));
    }

    #[test]
    fn native_capsule_record_requires_an_explicit_llm_recording() {
        let error = Cli::try_parse_from([
            "asb",
            "run",
            "benchmark.toml",
            "--native-capsule-record",
            "capsule",
        ])
        .unwrap_err();
        assert!(error.to_string().contains("--llm-record"));

        let cli = Cli::try_parse_from([
            "asb",
            "run",
            "benchmark.toml",
            "--llm-record",
            "--native-capsule-record",
            "capsule",
        ])
        .unwrap();
        let Some(CommandLine::Run(args)) = cli.command else {
            panic!("expected run arguments");
        };
        assert_eq!(args.native_capsule_record.as_deref(), Some("capsule"));
    }

    #[test]
    fn native_capsule_replay_requires_an_explicit_llm_replay() {
        let error = Cli::try_parse_from([
            "asb",
            "run",
            "benchmark.toml",
            "--native-capsule-replay",
            "capsule",
        ])
        .unwrap_err();
        assert!(error.to_string().contains("--llmreplay"));

        let cli = Cli::try_parse_from([
            "asb",
            "run",
            "benchmark.toml",
            "--llm-replay",
            "--native-capsule-replay",
            "capsule",
        ])
        .unwrap();
        let Some(CommandLine::Run(args)) = cli.command else {
            panic!("expected run arguments");
        };
        assert_eq!(args.native_capsule_replay.as_deref(), Some("capsule"));
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "asb",
    version,
    about = "Agent Systems Benchmark command line interface"
)]
struct Cli {
    /// Initialized ASB project directory used for the complete command.
    #[arg(long, global = true, value_name = "DIR", value_hint = clap::ValueHint::DirPath)]
    project_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<CommandLine>,
}

#[derive(Debug, Subcommand)]
enum CommandLine {
    Init(InitArgs),
    Setup(SetupArgs),
    Run(RunArgs),
    Sweep(SweepArgs),
    Compare(CompareArgs),
    Provider(ProviderArgs),
    Tool(ToolArgs),
    Workload(WorkloadArgs),
    Measure(MeasureArgs),
}

#[derive(Debug, Args)]
struct InitArgs {
    #[arg(value_name = "PROJECT_DIR", value_hint = clap::ValueHint::DirPath)]
    root: Option<PathBuf>,
    #[arg(long)]
    reset: bool,
    /// Move initialized configuration aside while retaining installed assets and results.
    #[arg(long, conflicts_with_all = ["reset", "configure_host"])]
    clean: bool,
    /// Update optional host capabilities without resetting project data.
    #[arg(long, conflicts_with_all = ["reset", "clean"])]
    configure_host: bool,
    /// Approve every privileged host-setup questionnaire without prompting.
    #[arg(long, short = 'y')]
    yes: bool,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum SetupMode {
    Tui,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Selection options accept the reserved keyword 'all'. Models/trials also accept PROVIDER=all or WORKLOAD=all. --model all queries the selected providers; other selectors use local catalogs. Paths and concurrency do not accept 'all'."
)]
struct SetupArgs {
    #[arg(value_enum)]
    mode: Option<SetupMode>,
    /// Optional benchmark or sweep TOML whose selections prefill the setup TUI.
    #[arg(value_name = "CONFIG", requires = "mode", value_hint = clap::ValueHint::FilePath)]
    input: Option<PathBuf>,
    /// Reopen the sweep or benchmark configuration generated by the last successful setup TUI run.
    #[arg(long, requires = "mode", conflicts_with = "input")]
    last: bool,
    /// Agent ID; repeat for a sweep.
    #[arg(long)]
    agent: Vec<String>,
    /// Workload ID; repeat for a sweep.
    #[arg(long)]
    workload: Vec<String>,
    /// Repeatable instance ID; optionally WORKLOAD=INSTANCE for scoped trials.
    #[arg(long, value_name = "INSTANCE")]
    trial: Vec<String>,
    /// Repeatable trial-set TOML; includes all enabled trials unless --trial is given.
    #[arg(long, value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    trial_set: Vec<String>,
    /// Provider ID; repeat for a sweep.
    #[arg(long)]
    provider: Vec<String>,
    /// Repeat to select models; use PROVIDER=MODEL to scope a model to one provider.
    #[arg(long)]
    model: Vec<String>,
    /// Repeat to select collectors (alias: --collector). Dependencies are included.
    #[arg(long, alias = "collector")]
    measure: Vec<String>,
    #[arg(long, value_name = "N")]
    concurrency: Option<usize>,
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    workspace: Option<String>,
    /// Repository-owned preset in the bundled presets directory.
    #[arg(long, value_name = "PRESET.toml", conflicts_with_all = ["agent", "trial", "trial_set", "provider", "model", "measure", "concurrency", "workspace"])]
    preset: Option<String>,
    /// Benchmark TOML for one selection; sweep TOML for repeated selections.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    output: Option<String>,
}

impl From<SetupArgs> for config::SetupOptions {
    fn from(value: SetupArgs) -> Self {
        Self {
            agent: value.agent,
            workload: value.workload,
            trial: value.trial,
            trial_set: value.trial_set,
            provider: value.provider,
            model: value.model,
            measure: value.measure,
            concurrency: value.concurrency,
            workspace: value.workspace,
            output: value.output,
            retention: crate::benchmark::RetentionConfig::default(),
            repeat: None,
        }
    }
}

#[derive(Debug, Args)]
#[command(
    group(ArgGroup::new("record-direction").args(["record", "llm_record", "tool_record", "mcp_record"]).multiple(true).conflicts_with("replay-direction")),
    group(ArgGroup::new("replay-direction").args(["replay", "llm_replay", "tool_replay", "mcp_replay"]).multiple(true))
)]
struct RunArgs {
    #[arg(value_name = "CONFIG", value_hint = clap::ValueHint::FilePath)]
    path: String,
    /// Sequential batches of fresh isolated executions at configured concurrency.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=65533))]
    repeat: Option<u64>,
    /// Maximum model/tool turns the selected agent may take. Unsupported agents are rejected before launch.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=65533))]
    max_steps: Option<u64>,
    /// Replay one track ID, or min|max|median:tokens|duration|requests.
    #[arg(long)]
    track: Option<crate::cassette::Selector>,
    /// Reproducible whole-track sampling with replacement (ZIP bundles only).
    #[arg(long)]
    seed: Option<u64>,
    #[arg(long)]
    record: bool,
    #[arg(long)]
    replay: bool,
    /// Record provider requests and responses.
    #[arg(long)]
    llm_record: bool,
    /// Replay provider requests and responses.
    #[arg(long = "llmreplay", visible_alias = "llm-replay")]
    llm_replay: bool,
    /// Record adapter-supported local tool calls and their agent-visible results.
    #[arg(long)]
    tool_record: bool,
    /// Replay recorded adapter-supported local tool results without executing tools.
    #[arg(long)]
    tool_replay: bool,
    /// Transparently record traffic to declared ordinary MCP servers.
    #[arg(long)]
    mcp_record: bool,
    /// Replay declared ordinary MCP servers only from sealed relay evidence.
    #[arg(long)]
    mcp_replay: bool,
    #[arg(long, help = "replay each response after its recorded response delay")]
    respect_record_timings: bool,
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    cassette: Option<String>,
    /// Capture a bounded native workspace capsule beside an LLM recording.
    #[arg(long, value_name = "DIR", value_hint = clap::ValueHint::DirPath, requires = "llm_record")]
    native_capsule_record: Option<String>,
    /// Restore and apply a bounded native workspace capsule beside an LLM replay.
    #[arg(long, value_name = "DIR", value_hint = clap::ValueHint::DirPath, requires = "llm_replay")]
    native_capsule_replay: Option<String>,
    #[arg(long)]
    json: bool,
    /// Print execution evidence paths, replay selections, cassette details, and report links.
    #[arg(short = 'v', long)]
    verbose: bool,
    #[arg(long = "show-output", conflicts_with = "json")]
    show_output: bool,
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    measurements: Option<String>,
}

#[derive(Debug, Args)]
#[command(
    group(ArgGroup::new("record-direction").args(["record", "llm_record", "tool_record", "mcp_record"]).multiple(true).conflicts_with("replay-direction")),
    group(ArgGroup::new("replay-direction").args(["replay", "llm_replay", "tool_replay", "mcp_replay"]).multiple(true)),
    group(ArgGroup::new("sweep-input").args(["path", "preset"]).required(true).multiple(false))
)]
struct SweepArgs {
    #[arg(value_name = "SWEEP", value_hint = clap::ValueHint::FilePath, group = "sweep-input")]
    path: Option<String>,
    /// Materialize a repository-owned preset in this initialized project, then run its sweep.
    #[arg(long, value_name = "PRESET.toml", group = "sweep-input")]
    preset: Option<String>,
    /// Sequential fresh batches per configuration, at its configured concurrency.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=65533))]
    repeat: Option<u64>,
    /// Maximum model/tool turns per benchmark execution. Unsupported agents are rejected before launch.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=65533))]
    max_steps: Option<u64>,
    /// Replay one track ID, or min|max|median:tokens|duration|requests.
    #[arg(long)]
    track: Option<crate::cassette::Selector>,
    /// Shared reproducible seed for configuration-ordered bundled selections.
    #[arg(long)]
    seed: Option<u64>,
    /// Import existing official grades for reporting only; never run an agent or evaluator.
    #[arg(long, conflicts_with_all = ["resume", "reconcile_completed", "record", "replay", "show_output", "respect_record_timings", "cassette_root", "max_active_attempts"])]
    import_grades: bool,
    /// Reuse validated completed groups and run untouched configurations; incomplete groups require recovery.
    #[arg(long)]
    resume: bool,
    /// Continue an interrupted sweep. Optionally stop after this many failed benchmarks; without a value, continue after any number of failures.
    #[arg(long = "continue", num_args = 0..=1, value_name = "FAILED")]
    continue_after_failures: Option<Option<usize>>,
    /// Finish an already frozen union publication/ledger; never execute agents or evaluators.
    #[arg(long, requires_all = ["resume", "record"], conflicts_with_all = ["import_grades", "reconcile_completed", "repeat", "track", "seed", "show_output", "cassette_root", "respect_record_timings", "max_active_attempts"])]
    recover_recording: bool,
    /// Link a fully staged continuation; metadata only, never run agents or evaluators.
    #[arg(long, requires = "resume", conflicts_with_all = ["record", "replay", "recover_recording", "import_grades", "reconcile_completed", "repeat", "track", "seed", "show_output", "cassette_root", "respect_record_timings", "max_active_attempts", "jobs"])]
    recover_continuation: bool,
    #[arg(
        long,
        requires = "resume",
        help = "validate retained completed grades and repair missing receipts; never retry an agent"
    )]
    reconcile_completed: bool,
    #[arg(long)]
    record: bool,
    #[arg(long)]
    replay: bool,
    /// Record provider requests and responses.
    #[arg(long)]
    llm_record: bool,
    /// Replay provider requests and responses.
    #[arg(long = "llmreplay", visible_alias = "llm-replay")]
    llm_replay: bool,
    /// Record adapter-supported local tool calls and their agent-visible results.
    #[arg(long)]
    tool_record: bool,
    /// Replay recorded adapter-supported local tool results without executing tools.
    #[arg(long)]
    tool_replay: bool,
    /// Transparently record traffic to declared ordinary MCP servers.
    #[arg(long)]
    mcp_record: bool,
    /// Replay declared ordinary MCP servers only from sealed relay evidence.
    #[arg(long)]
    mcp_replay: bool,
    #[arg(long, help = "replay each response after its recorded response delay")]
    respect_record_timings: bool,
    #[arg(long)]
    json: bool,
    /// Print execution evidence paths, replay selections, cassette details, and report links.
    #[arg(short = 'v', long)]
    verbose: bool,
    #[arg(
        long = "show-output",
        conflicts_with = "json",
        help = "show agent output; parallel jobs buffer it by configuration"
    )]
    show_output: bool,
    /// Separate cassette root for recording, or the record root when replaying.
    #[arg(long, value_name = "DIR", value_hint = clap::ValueHint::DirPath)]
    cassette_root: Option<String>,
    /// Maximum distinct configurations running at once (not identical replicas).
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=32))]
    jobs: u16,
    /// Total simultaneous replicas across all jobs; defaults to max(jobs, largest concurrency).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=64))]
    max_active_attempts: Option<u16>,
}

#[derive(Debug, Args)]
struct CompareArgs {
    #[arg(value_name = "BENCHMARK_OR_SWEEP", value_hint = clap::ValueHint::FilePath)]
    path: String,
    /// Open the generated HTML report in the default web browser.
    #[arg(long)]
    report: bool,
}
#[derive(Debug, Args)]
struct ProviderArgs {
    #[command(subcommand)]
    command: ProviderCommand,
}
#[derive(Debug, Subcommand)]
enum ProviderCommand {
    Models(ModelArgs),
}
#[derive(Debug, Args)]
struct ToolArgs {
    #[command(subcommand)]
    command: ToolCommand,
}
#[derive(Debug, Subcommand)]
enum ToolCommand {
    List(ToolListArgs),
    Install(ToolInstallArgs),
    /// Serve the latest report and its Perfetto traces from this project.
    Http(HttpArgs),
}
#[derive(Debug, Args, Default)]
struct ToolListArgs {}
#[derive(Debug, Args)]
struct HttpArgs {
    #[command(subcommand)]
    command: HttpCommand,
}
#[derive(Debug, Subcommand)]
enum HttpCommand {
    /// Start or replace this project's report server.
    Start,
    /// Stop this project's report server.
    Stop,
}
#[derive(Debug, Args)]
struct WorkloadArgs {
    /// Workload whose installed trials will be listed.
    #[arg(value_name = "WORKLOAD")]
    name: Option<String>,
    #[command(subcommand)]
    command: Option<WorkloadCommand>,
}
#[derive(Debug, Subcommand)]
enum WorkloadCommand {
    List(WorkloadListArgs),
    /// Print a registered module's versioned JSON contract without installing or executing it.
    Describe(WorkloadDescribeArgs),
    Install(WorkloadInstallArgs),
    /// List the selected workload's installed trials.
    Trials,
    Tui,
    #[command(
        after_help = "By default, stops only a departed controller's recorded task containers, preserving partial files and volumes. --inspect-grade instead reads an existing official grade without changing the attempt or running an agent/evaluator. Neither operation resumes a sweep."
    )]
    Recover(WorkloadRecoverArgs),
}
#[derive(Debug, Args)]
struct WorkloadDescribeArgs {
    #[arg(value_name = "WORKLOAD")]
    name: String,
}
#[derive(Debug, Args)]
struct MeasureArgs {
    /// Collector whose available programs will be listed.
    #[arg(value_name = "COLLECTOR")]
    name: Option<String>,
    #[command(subcommand)]
    command: Option<MeasureCommand>,
}
#[derive(Debug, Subcommand)]
enum MeasureCommand {
    /// List the collector's selectable programs.
    Programs,
}
#[derive(Debug, Args)]
struct WorkloadRecoverArgs {
    #[arg(value_name = "REFERENCE", value_hint = clap::ValueHint::FilePath)]
    reference: PathBuf,
    /// Grade a retained unfinished submission with its official evaluator; never rerun the agent.
    #[arg(long, conflicts_with = "inspect_grade")]
    grade: bool,
    /// Read an existing official grade without running an agent/evaluator or changing the attempt.
    #[arg(long)]
    inspect_grade: bool,
    #[arg(long)]
    json: bool,
}
#[derive(Debug, Args, Default)]
struct WorkloadListArgs {
    #[arg(long)]
    dir: Option<PathBuf>,
}
#[derive(Debug, Args)]
struct WorkloadInstallArgs {
    #[arg(value_name = "SOURCE")]
    source: String,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    dir: Option<PathBuf>,
}
#[derive(Debug, Args)]
struct ModelArgs {
    #[arg(value_name = "PROVIDER")]
    provider: Option<String>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    free: bool,
}

impl From<ModelArgs> for models::ModelListOptions {
    fn from(value: ModelArgs) -> Self {
        Self {
            provider: value.provider,
            json: value.json,
            free_only: value.free,
        }
    }
}
impl From<ToolInstallArgs> for install::ToolInstallOptions {
    fn from(value: ToolInstallArgs) -> Self {
        Self {
            name: value.name,
            source: value.source,
            dir: value.dir,
            dry_run: value.dry_run,
        }
    }
}
impl From<WorkloadInstallArgs> for install::WorkloadInstallOptions {
    fn from(value: WorkloadInstallArgs) -> Self {
        Self {
            source: value.source,
            name: value.name,
            dir: value.dir,
        }
    }
}
impl From<WorkloadListArgs> for install::WorkloadListOptions {
    fn from(value: WorkloadListArgs) -> Self {
        Self { dir: value.dir }
    }
}
#[derive(Debug, Args)]
#[command(
    after_help = "Install tools, including provider bridge tools. List available tools with `asb tool list`."
)]
struct ToolInstallArgs {
    #[arg(value_name = "TOOL")]
    name: String,
    #[arg(long)]
    source: Option<String>,
    #[arg(long)]
    dir: Option<PathBuf>,
    #[arg(long)]
    dry_run: bool,
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn internal_completion_forms_errors_and_writer_failure_are_explicit() {
        for shell in ["bash", "zsh"] {
            for args in [
                vec!["--__generate-completion".into(), shell.into()],
                vec![format!("--__generate-completion={shell}").into()],
            ] {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                assert_eq!(super::run(&args, &mut stdout, &mut stderr), 0);
                assert!(String::from_utf8(stdout).unwrap().contains("_asb"));
                assert!(stderr.is_empty());
            }
        }
        for args in [
            vec!["--__generate-completion".into()],
            vec![
                "--__generate-completion".into(),
                "bash".into(),
                "unexpected".into(),
            ],
            vec!["--__generate-completion=bash".into(), "unexpected".into()],
            vec!["--__generate-completion=fish".into()],
            vec!["--__generate-completion=".into()],
            vec!["--__generate-completion".into(), "fish".into()],
        ] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            assert_eq!(super::run(&args, &mut stdout, &mut stderr), 2);
            assert!(stdout.is_empty());
            assert!(!stderr.is_empty());
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let args = [
                "--__generate-completion".into(),
                std::ffi::OsString::from_vec(vec![0xff]),
            ];
            assert_eq!(super::run(&args, &mut Vec::new(), &mut Vec::new()), 2);
        }
        struct Broken;
        impl std::io::Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "fixture",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let args = ["--__generate-completion=bash".into()];
        let mut stderr = Vec::new();
        assert_eq!(super::run(&args, &mut Broken, &mut stderr), 2);
        assert!(!stderr.is_empty());
        // Suggestions must not constrain requests for explicit custom binaries.
        Cli::try_parse_from([
            "asb",
            "tool",
            "install",
            "private-source",
            "--source",
            "fixture-binary",
        ])
        .expect("registry suggestions must not constrain actual custom-tool parsing");
    }

    #[test]
    fn all_setup_options_are_typed_and_order_independent() {
        let cli = Cli::try_parse_from([
            "asb",
            "setup",
            "--model",
            "gpt-6.1-sol",
            "--agent",
            "opencode",
            "--output",
            "benchmark.toml",
            "--provider",
            "cli2key",
            "--workload",
            "bug-fix",
            "--workspace",
            "workspace",
            "--trial",
            "one",
            "--trial-set",
            "selected.workload.toml",
        ])
        .expect("typed setup options parse");
        let rendered = format!("{cli:?}");
        assert!(rendered.contains("gpt-6.1-sol"));
        assert!(rendered.contains("benchmark.toml"));
        assert!(rendered.contains("selected.workload.toml"));
    }

    #[test]
    fn setup_tui_accepts_one_preselection_toml() {
        let cli = Cli::try_parse_from(["asb", "setup", "tui", "saved-sweep.toml"])
            .expect("TUI preselection path parses");
        let rendered = format!("{cli:?}");
        assert!(rendered.contains("Tui"));
        assert!(rendered.contains("saved-sweep.toml"));
        assert!(Cli::try_parse_from(["asb", "setup", "saved-sweep.toml"]).is_err());
    }

    #[test]
    fn setup_tui_last_is_exclusive_with_an_explicit_preselection() {
        Cli::try_parse_from(["asb", "setup", "tui", "--last"]).expect("last TUI selection parses");
        assert!(
            Cli::try_parse_from(["asb", "setup", "tui", "saved-sweep.toml", "--last",]).is_err()
        );
    }

    #[test]
    fn single_dash_long_options_are_rejected_by_the_common_parser() {
        let error = Cli::try_parse_from(["asb", "setup", "-workload", "bug-fix"])
            .expect_err("single dash long option must be rejected");
        assert!(error.to_string().contains("unexpected argument"));
    }

    #[test]
    fn measure_catalog_lists_collectors_and_bpftrace_programs() {
        let root = std::env::temp_dir().join(format!(
            "asb-parser-measure-project-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        crate::project::init(Some(root.clone()), false, false, false, &mut Vec::new()).unwrap();
        let mut output = Vec::new();
        assert_eq!(
            super::run(
                &[
                    "measure".into(),
                    "--project-dir".into(),
                    root.clone().into_os_string(),
                ],
                &mut output,
                &mut Vec::new(),
            ),
            0
        );
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("bpftrace\tselected process-tree eBPF programs"));
        assert!(output.contains("agent-memory\tselected agent process-tree memory usage"));
        assert!(output.contains("tool-calls\tstructured agent tool-call observations"));

        let mut output = Vec::new();
        assert_eq!(
            super::run(
                &[
                    "measure".into(),
                    "bpftrace".into(),
                    "programs".into(),
                    "--project-dir".into(),
                    root.clone().into_os_string(),
                ],
                &mut output,
                &mut Vec::new(),
            ),
            0
        );
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("all\tall built-in eBPF programs"));
        assert!(output.contains("sched-latency\twakeup-to-run latency"));
        assert!(output.contains("io-wait\tcompleted kernel-accounted I/O waits"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bpftrace_profile_selectors_are_typed_setup_values() {
        Cli::try_parse_from([
            "asb",
            "setup",
            "--agent",
            "opencode",
            "--provider",
            "cli2key",
            "--model",
            "gpt-6.1-sol",
            "--workload",
            "bug-fix",
            "--measure",
            "bpftrace:all",
        ])
        .expect("bpftrace profile selection parses");
        Cli::try_parse_from(["asb", "workload", "terminal-bench-1.0", "trials"])
            .expect("workload trial listing parses");
    }

    #[test]
    fn sweep_continue_accepts_an_optional_failure_budget() {
        for args in [
            vec!["asb", "sweep", "suite.toml", "--continue"],
            vec!["asb", "sweep", "suite.toml", "--continue", "3"],
        ] {
            Cli::try_parse_from(args).expect("continue sweep option parses");
        }
    }

    #[test]
    fn sweep_preset_is_an_alternative_to_a_manifest_path() {
        Cli::try_parse_from(["asb", "sweep", "--preset", "harnesstax.toml"])
            .expect("preset sweep parses");
        assert!(Cli::try_parse_from(["asb", "sweep"]).is_err());
        assert!(
            Cli::try_parse_from([
                "asb",
                "sweep",
                "existing-sweep.toml",
                "--preset",
                "harnesstax.toml",
            ])
            .is_err()
        );
    }
}
