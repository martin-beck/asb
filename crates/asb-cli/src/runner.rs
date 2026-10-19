//! Live execution of one configured benchmark.

use super::{Error, io_error, read_config};
use crate::agents;
use crate::benchmark::BenchmarkConfig;
use crate::collectors::{LifecycleEvent, LifecyclePhase};
use crate::measurements;
use crate::models;
use crate::project;
use crate::providers;
use crate::suggestions;
use crate::tape;
use crate::workloads;
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use std::time::Instant;

const MAX_OUTPUT: usize = 1024 * 1024;
const MAX_AGENT_RUNTIME: Duration = Duration::from_secs(15 * 60);

fn emit_warning(
    warnings: &Mutex<BTreeSet<String>>,
    output: &mut dyn Write,
    message: String,
) -> Result<(), Error> {
    let mut seen = warnings
        .lock()
        .map_err(|_| Error::Run("warning state unavailable".into()))?;
    if !seen.contains(&message) {
        writeln!(output, "{message}").map_err(io_error)?;
        seen.insert(message);
    }
    Ok(())
}

#[derive(Serialize)]
struct RunResult<'a> {
    status: &'a str,
    agent: &'a str,
    workload: &'a str,
    provider: &'a str,
    model: &'a str,
    process_success: bool,
    workload_success: Option<bool>,
    workload_evaluation: &'static str,
    failure_kind: Option<&'static str>,
    failure_reason: Option<String>,
    success_rate: f64,
    elapsed_ms: u128,
    exit_code: Option<i32>,
    token_usage: measurements::TokenUsage,
    steps: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cassette: Option<String>,
    stdout: String,
    stderr: String,
}

/// Execution choices shared by single runs, replicas, and sweeps.
#[derive(Clone, Copy, Default)]
pub(crate) struct RunOptions<'a> {
    pub json: bool,
    pub show_output: bool,
    pub measurement_dir: Option<&'a str>,
    pub mode: Option<tape::Mode>,
    pub cassette: Option<&'a str>,
    pub respect_record_timings: bool,
}

/// Execute one configured benchmark, optionally through a local llmtape proxy.
pub(crate) fn run_with_tape(
    path: &str,
    options: RunOptions<'_>,
    output: &mut dyn Write,
) -> Result<(), Error> {
    run_with_diagnostics(path, options, output, &Mutex::new(BTreeSet::new()))
}

pub(crate) fn run_with_diagnostics(
    path: &str,
    options: RunOptions<'_>,
    output: &mut dyn Write,
    warnings: &Mutex<BTreeSet<String>>,
) -> Result<(), Error> {
    let config_path = Path::new(path);
    let text = read_config(config_path)?;
    let config: BenchmarkConfig = toml::from_str(&text)
        .map_err(|error| Error::Input(format!("cannot parse {path}: {error}")))?;
    if let Some(measures) = config.measurements.as_ref().filter(|value| value.enabled) {
        let (root, project_config) = project::load(config_path)?;
        let tools_root = project::resolve(&root, &project_config.tools_dir);
        let mut messages = Vec::new();
        crate::install::ensure_collector_dependencies(
            &measures.collectors,
            &root,
            &tools_root,
            &mut messages,
            &mut |_, _| {},
            false,
        )?;
        if !options.json {
            output.write_all(&messages).map_err(io_error)?;
        }
    }
    if config.concurrency > 1 {
        return run_concurrent(config_path, &config, options, output, warnings);
    }
    run_mode(config_path, config, options, output, warnings)
}

/// Execute identical replicas concurrently. Each replica gets an isolated
/// workspace, log, measurement directory, and record cassette so agents never
/// mutate one another's files. Output is buffered per replica and emitted in
/// replica order after completion to keep the terminal readable.
fn run_concurrent(
    config_path: &Path,
    config: &BenchmarkConfig,
    options: RunOptions<'_>,
    output: &mut dyn Write,
    warnings: &Mutex<BTreeSet<String>>,
) -> Result<(), Error> {
    let tape_request = options.mode.map(|mode| (mode, options.cassette));
    let mut jobs = Vec::with_capacity(config.concurrency);
    for index in 1..=config.concurrency {
        let mut replica = config.clone();
        replica.concurrency = 1;
        replica.workspace = parallel_path(&replica.workspace, index);
        replica.log_file = parallel_path(&replica.log_file, index);
        if let Some(measurements) = replica.measurements.as_mut() {
            measurements.directory = parallel_path(&measurements.directory, index);
        }
        let cassette = tape_request.map(|(mode, requested)| {
            let base = requested
                .map(PathBuf::from)
                .unwrap_or_else(|| default_cassette(config_path, config));
            (mode, replica_cassette(mode, &base, index))
        });
        let measurement = options
            .measurement_dir
            .map(|path| PathBuf::from(parallel_path(path, index)));
        jobs.push((replica, measurement, cassette));
    }

    let mut completed = Vec::with_capacity(jobs.len());
    std::thread::scope(|scope| {
        let handles =
            jobs.into_iter()
                .enumerate()
                .map(|(index, (replica, measurement, cassette))| {
                    scope.spawn(move || {
                        let mut buffer = Vec::new();
                        let mode = cassette.as_ref().map(|(mode, _)| *mode);
                        let cassette_path =
                            cassette.as_ref().map(|(_, path)| path.to_string_lossy());
                        let cassette_path = cassette_path.as_deref();
                        let measurement_path = measurement.as_deref().and_then(Path::to_str);
                        let result = run_mode(
                            config_path,
                            replica,
                            RunOptions {
                                measurement_dir: measurement_path,
                                mode,
                                cassette: cassette_path,
                                ..options
                            },
                            &mut buffer,
                            warnings,
                        );
                        (index, result, buffer)
                    })
                });
        for handle in handles {
            completed.push(
                handle
                    .join()
                    .expect("benchmark concurrency worker panicked"),
            );
        }
    });

    completed.sort_by_key(|(index, _, _)| *index);
    let mut failures: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, result, buffer) in completed {
        output.write_all(&buffer).map_err(io_error)?;
        if let Err(error) = result {
            add_replica_failure(&mut failures, index + 1, error.to_string());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::Run(format!(
            "{} concurrent benchmark replica(s) failed: {}",
            failures
                .iter()
                .map(|(_, indexes)| indexes.len())
                .sum::<usize>(),
            failures
                .iter()
                .map(|(message, indexes)| format!(
                    "replica{} {}: {message}",
                    if indexes.len() == 1 { "" } else { "s" },
                    indexes
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
                .collect::<Vec<_>>()
                .join("; ")
        )))
    }
}

fn add_replica_failure(failures: &mut Vec<(String, Vec<usize>)>, index: usize, message: String) {
    if let Some((_, indexes)) = failures.iter_mut().find(|(item, _)| *item == message) {
        indexes.push(index);
    } else {
        failures.push((message, vec![index]));
    }
}

// A single recording can be replayed independently by every replica. When no
// single recording exists, retain compatibility with concurrent recordings.
fn replica_cassette(mode: tape::Mode, base: &Path, index: usize) -> PathBuf {
    if mode == tape::Mode::Replay && base.is_file() {
        base.to_path_buf()
    } else {
        let candidate = append_path_suffix(base, index);
        if mode != tape::Mode::Replay || candidate.is_file() {
            return candidate;
        }
        // Sweep replay may intentionally use a larger concurrency than the
        // recording. Reuse an existing replica recording rather than waiting
        // forever for a cassette that can never be produced. Keep the
        // requested path as the fail-closed fallback when no recording exists.
        (1..=32)
            .map(|recorded| append_path_suffix(base, recorded))
            .find(|path| path.is_file())
            .unwrap_or(candidate)
    }
}

fn parallel_path(value: &str, index: usize) -> String {
    append_path_suffix(Path::new(value), index)
        .to_string_lossy()
        .into_owned()
}

fn append_path_suffix(path: &Path, index: usize) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("benchmark");
    let extension = path.extension().and_then(|value| value.to_str());
    let name = match extension {
        Some(extension) => format!("{stem}-concurrency-{index}.{extension}"),
        None => format!("{stem}-concurrency-{index}"),
    };
    if let Some(parent) = path.parent() {
        parent.join(name)
    } else {
        PathBuf::from(name)
    }
}

fn run_mode(
    config_path: &Path,
    config: BenchmarkConfig,
    options: RunOptions<'_>,
    output: &mut dyn Write,
    warnings: &Mutex<BTreeSet<String>>,
) -> Result<(), Error> {
    let RunOptions {
        json,
        show_output,
        measurement_dir,
        mode,
        cassette,
        respect_record_timings,
    } = options;
    let tape_request = mode.map(|mode| (mode, cassette));
    let tape_cassette = tape_request.map(|(mode, requested)| {
        (
            mode,
            requested
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| default_cassette(config_path, &config)),
        )
    });
    let (project_root, project_config) = project::load(config_path)?;
    let project_root = project_root.canonicalize().map_err(|error| {
        Error::Input(format!(
            "cannot resolve benchmark project directory: {error}"
        ))
    })?;
    let _network_project = project::network_project(&project_root);
    let configured_measurements = config
        .measurements
        .as_ref()
        .filter(|value| value.enabled)
        .map(|value| project::resolve(&project_root, &value.directory));
    let measurement_path = measurement_dir
        .map(std::path::PathBuf::from)
        .or(configured_measurements);
    let measurement_collectors = config
        .measurements
        .as_ref()
        .filter(|value| value.enabled)
        .map_or(&[][..], |value| value.collectors.as_slice());
    let mut measurement_session = measurement_path
        .as_deref()
        .map(|dir| {
            measurements::Session::start_with_collectors(
                dir,
                measurement_collectors,
                Some(&project::resolve(&project_root, &project_config.tools_dir)),
            )
        })
        .transpose()
        .map_err(Error::Run)?;
    if let Some(session) = measurement_session.as_mut() {
        session
            .event(LifecycleEvent::RunStarted)
            .map_err(Error::Run)?;
    }
    if let Some((tape::Mode::Replay, cassette)) = tape_cassette.as_ref() {
        models::ensure_replay_tool_capable(cassette, &config.model)?;
    }
    let mut discovered_tool_support = None;
    let selected_offer = instrument_phase(
        measurement_session.as_mut(),
        LifecyclePhase::ProviderDiscovery,
        || {
            agents::find(&config.agent)
        .map(|adapter| {
            let offered = if let Some((tape::Mode::Replay, cassette)) = tape_cassette.as_ref() {
                providers::replay_endpoints(&config.provider, &config.model, models::replay_interface(cassette)?)
            } else {
                let (offers, snapshot_fallback, tool_support) = providers::discover_endpoints(&config.provider, &config.model);
                discovered_tool_support = tool_support;
                if snapshot_fallback && !json {
                    emit_warning(warnings, output, format!("Warning: current endpoint metadata is unavailable for provider `{}` and model `{}`; using the built-in endpoint snapshot, which may be out of date.", config.provider, config.model))?;
                }
                offers
            };
            crate::endpoint::choose_from(
                adapter.id(),
                adapter.supported_endpoints(),
                &config.provider,
                &config.model,
                &offered,
            )
        })
        .transpose()
        },
    )?;
    let tape_mode = tape_request.map(|(mode, _)| mode);
    let bridge = instrument_phase(
        measurement_session.as_mut(),
        LifecyclePhase::ProviderSetup,
        || {
            let bridge = providers::start_local_bridge(
                &config.provider,
                &project_root,
                &project_config.tools_dir,
                tape_mode == Some(tape::Mode::Replay),
            )?;
            if tape_mode != Some(tape::Mode::Replay) {
                let endpoint =
                    providers::local_bridge_endpoint(&config.provider, bridge.as_deref());
                let bridge_catalog = providers::local_bridge_credentials(
                    &config.provider,
                    bridge.as_deref(),
                    endpoint,
                )
                .map(|(key, url)| (url, key));
                if discovered_tool_support == Some(false) {
                    return Err(Error::Input(format!(
                        "provider metadata reports that model `{}` does not support tools",
                        config.model
                    )));
                }
                if !models::ensure_tool_capable(&config.provider, &config.model, bridge_catalog)?
                    && discovered_tool_support != Some(true)
                    && !json
                {
                    emit_warning(
                        warnings,
                        output,
                        format!(
                            "Warning: provider `{}` lists model `{}` but does not report whether it supports tools; the agent may fail if the model cannot use tools.",
                            config.provider, config.model
                        ),
                    )?;
                }
            }
            Ok(bridge)
        },
    )?;
    let workspace_name = if config.workspace == "asb-workspace" {
        &project_config.workspace_dir
    } else {
        &config.workspace
    };
    let workspace = project::resolve(&project_root, workspace_name);
    let workload_directory = std::env::var_os("ASB_WORKLOAD_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| project::resolve(&project_root, &project_config.workloads_dir));
    let prompt = instrument_phase(
        measurement_session.as_mut(),
        LifecyclePhase::WorkloadPreparation,
        || {
            std::fs::create_dir_all(&workspace).map_err(|error| {
                Error::Run(format!(
                    "cannot create workspace {}: {error}",
                    workspace.display()
                ))
            })?;
            let prompt = workloads::load_trial(
                &config.workload,
                config.trial.as_deref(),
                config.trial_set.as_deref(),
                &workload_directory,
            )
            .map_err(Error::Input)?;
            workloads::prepare_fixture(&config.workload, &workspace).map_err(Error::Run)?;
            workloads::prepare_external(&config.workload, &workload_directory, &workspace)
                .map_err(Error::Run)?;
            Ok(prompt)
        },
    )?;
    let mut tape_session = instrument_phase(
        measurement_session.as_mut(),
        LifecyclePhase::ProxyPreparation,
        || {
            if let Some((mode, cassette)) = tape_cassette.as_ref() {
                let bridge_url =
                    providers::local_bridge_endpoint(&config.provider, bridge.as_deref());
                let upstream = if *mode == tape::Mode::Replay {
                    None
                } else if let Some(offer) = selected_offer.as_ref() {
                    Some(providers::selected_tape_upstream(
                        &config.provider,
                        offer.clone(),
                        bridge_url,
                    )?)
                } else {
                    providers::tape_upstream(&config.provider, bridge_url)
                };
                Ok(Some(tape::Session::start(
                    *mode,
                    cassette,
                    upstream.as_deref(),
                    respect_record_timings,
                    &workspace,
                )?))
            } else {
                Ok(None)
            }
        },
    )?;
    let tape_url = tape_session.as_ref().map(|session| session.url());
    let provider_endpoint =
        tape_url.or_else(|| providers::local_bridge_endpoint(&config.provider, bridge.as_deref()));
    let provider_credentials =
        providers::local_bridge_credentials(&config.provider, bridge.as_deref(), provider_endpoint);
    let connection = selected_offer
        .map(|offer| {
            providers::endpoint_connection(
                &config.provider,
                offer,
                provider_credentials,
                tape_url.zip(tape_mode).map(|(url, mode)| (mode, url)),
            )
        })
        .transpose()?;
    // Adapters run with the benchmark workspace as their current directory.
    // Pass the canonical workspace explicitly so a relative configuration
    // value is not resolved a second time inside that directory (which is
    // especially important for OpenJiuwen's persistent workspace).
    let mut command_config = config.clone();
    command_config.workspace = workspace.to_string_lossy().into_owned();
    let specification = command_for_with_interface(
        &command_config,
        connection
            .as_ref()
            .map(|connection| connection.base_url.as_str())
            .or(provider_endpoint),
        &prompt,
        connection.as_ref().map(|connection| connection.interface),
    )?;
    let mut program = specification.program;
    if let Some(local) = agents::resolve(&config.agent, &project_root, &project_config.tools_dir) {
        program = local;
    }
    let started = Instant::now();
    let mut command = agents::find(&config.agent).map_or_else(
        || Ok(Command::new(&program)),
        |adapter| adapter.process(&program),
    )?;
    project::apply_network_proxy(&mut command, config_path)?;
    let target_args = specification.args.clone();
    command.args(specification.args);
    // Do not leak an enclosing Codex session into the benchmark agent. These
    // variables are host-runner integration state; if inherited, a project
    // local Codex CLI can advertise the coordinator's tools as an
    // `additional_tools` request that cli2key does not support.
    for variable in [
        "CODEX_REMOTE_PAYLOAD",
        "CODEX_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_PERMISSION_PROFILE",
        "CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS",
        "CODEX_MANAGED_PACKAGE_ROOT",
        "CODEX_MANAGED_BY_NPM",
        "CODEX_MANAGED_BY_BUN",
        "CODEX_MANAGED_BY_PNPM",
        "CODEX_MANAGED_BY_VITE_PLUS",
        "CODEX_CI",
    ] {
        command.env_remove(variable);
    }
    if specification.append_prompt {
        command.arg(prompt);
    }
    command
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    providers::configure(
        &mut command,
        &config.provider,
        &config.model,
        tape_mode == Some(tape::Mode::Replay),
    );
    if tape_mode == Some(tape::Mode::Replay) {
        agents::mask_replay_credentials(&mut command);
    }
    providers::apply_with_tape(
        &mut command,
        &config.provider,
        &config.model,
        provider_credentials,
        tape_url,
        tape_mode == Some(tape::Mode::Replay),
    )?;
    // Agent state must not appear inside the workload checkout, where it can
    // affect git status, tools, and therefore the recorded agent trajectory.
    let mut runtime_path = workspace.as_os_str().to_os_string();
    runtime_path.push(".asb-agent-runtime");
    let runtime_path = PathBuf::from(runtime_path);
    // Run-specific agent artifacts must not be confused with earlier runs or
    // simultaneous replicas, including after process IDs are reused.
    static RUN_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let run_dir = runtime_path.join("runs").join(format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        RUN_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    ));
    instrument_phase(
        measurement_session.as_mut(),
        LifecyclePhase::AgentPreparation,
        || {
            if let Some(connection) = connection.as_ref() {
                agents::prepare(
                    &config.agent,
                    &mut agents::PreparationContext {
                        command: &mut command,
                        model: &config.model,
                        connection,
                        runtime_dir: &runtime_path,
                        run_dir: &run_dir,
                    },
                )?;
            }
            Ok(())
        },
    )?;
    if let Some(url) = tape_url {
        providers::configure_tape(
            &mut command,
            &config.provider,
            url,
            tape_mode.expect("tape URL implies tape mode"),
        );
    }
    project::apply_python_path(&mut command, &project_root, Path::new(&program))?;
    let child = run_measured_agent(
        &mut command,
        &program,
        Some(&target_args),
        measurement_session.as_mut(),
        show_output,
        output,
    )?;
    let mut run_error = tape_session
        .take()
        .and_then(|session| session.stop().err())
        .map(|error| error.to_string());
    if let Some((tape::Mode::Record, cassette)) = tape_cassette.as_ref() {
        match std::fs::metadata(cassette) {
            Ok(metadata) if metadata.len() > 0 => {}
            Ok(_) => {
                run_error = Some(format!(
                    "record created an empty cassette {}; the agent made no provider request; verify its provider base URL and credentials",
                    cassette.display()
                ));
            }
            Err(error) => {
                run_error = Some(format!(
                    "record completed without creating cassette {}; verify the provider endpoint and proxy adapter installation: {error}",
                    cassette.display()
                ));
            }
        }
    }
    let elapsed_ms = started.elapsed().as_millis();
    // Prefer the agent's structured final response when deciding whether a
    // zero-exit process succeeded. Tool output can legitimately contain a
    // transient failed command that the agent repairs before its final answer;
    // scanning that whole transcript would incorrectly fail the run.
    let final_response = agents::final_response(&config.agent, &bounded(&child.stdout), &workspace);
    let agent_failure = if config.agent == "openjiuwen" && final_response.is_none() {
        Some("OpenJiuwen exited without a final response; the last event appears to be an unexecuted tool call. Retry with a compatible OpenJiuwen client or inspect the benchmark log.".into())
    } else {
        process_failure_reason(
            &child.status,
            &child.stdout,
            &child.stderr,
            final_response.as_deref(),
        )
    };
    let process_success = agent_failure.is_none();
    let (workload_success, grading_failure) = if process_success && run_error.is_none() {
        match instrument_operation(
            measurement_session.as_mut(),
            LifecyclePhase::WorkloadEvaluation,
            || {
                workloads::evaluate(
                    &config.workload,
                    &workspace,
                    final_response.as_deref().unwrap_or_default(),
                )
            },
        )? {
            Ok(workloads::Evaluation::Passed) => (Some(true), None),
            Ok(workloads::Evaluation::Failed(reason)) => {
                if let Some(session) = measurement_session.as_mut() {
                    session
                        .event(LifecycleEvent::PhaseFailed(
                            LifecyclePhase::WorkloadEvaluation,
                        ))
                        .map_err(Error::Run)?;
                }
                (
                    Some(false),
                    Some((
                        "workload_check_failed",
                        format!("workload check failed: {reason}"),
                    )),
                )
            }
            Err(error) => (
                None,
                Some((
                    "grader_error",
                    format!("workload grader could not evaluate the result: {error}"),
                )),
            ),
        }
    } else {
        (None, None)
    };
    let success = run_error.is_none() && process_success && workload_success == Some(true);
    let failure = if let Some(error) = run_error.as_ref() {
        Some(("run_error", error.clone()))
    } else if let Some(error) = agent_failure {
        Some(("agent_failure", error))
    } else {
        grading_failure
    };
    let adapter_artifacts = agents::find(&config.agent)
        .map(|adapter| {
            adapter.collect(&agents::CollectionContext {
                workspace: &workspace,
                run_dir: &run_dir,
                interface: connection.as_ref().map(|connection| connection.interface),
                anthropic_input_includes_cache: crate::providers::anthropic_input_includes_cache(
                    &config.provider,
                ),
            })
        })
        .unwrap_or_default();
    let mut token_usage = measurements::token_usage_from_sources(
        &config.provider,
        &config.agent,
        &String::from_utf8_lossy(&child.stdout),
        &String::from_utf8_lossy(&child.stderr),
        tape_cassette
            .as_ref()
            .map(|(_, cassette)| cassette.as_path()),
    );
    if let Some(artifact_usage) = adapter_artifacts.token_usage {
        let artifact_complete =
            artifact_usage.input_tokens.is_some() && artifact_usage.output_tokens.is_some();
        match token_usage.source.as_deref() {
            Some(source) if source.contains("cassette") => {}
            None | Some("agent-output") if artifact_complete => {
                token_usage = measurements::TokenUsage::default()
                    .merge_with_source(artifact_usage, "agent-artifact")
                    .with_derived_total();
            }
            _ => {
                token_usage = token_usage
                    .merge_with_source(artifact_usage, "agent-artifact")
                    .with_derived_total();
            }
        }
    }
    for path in adapter_artifacts.cleanup_paths {
        let _ = std::fs::remove_file(path);
    }
    let steps = measurements::steps_from_sources(
        &config.agent,
        &bounded(&child.stdout),
        &bounded(&child.stderr),
    );
    let mut stderr = bounded(&child.stderr);
    if let Some(error) = run_error.as_deref() {
        stderr.push_str("\nASB run error: ");
        stderr.push_str(error);
        stderr.push('\n');
    }
    if let Some((kind, reason)) = &failure {
        stderr.push_str("\nASB failure (");
        stderr.push_str(kind);
        stderr.push_str("): ");
        stderr.push_str(reason);
        stderr.push('\n');
    }
    let run_metrics = crate::collectors::run_metrics(&crate::collectors::RunObservation {
        elapsed_ms,
        success,
    });
    let result = RunResult {
        status: if success { "completed" } else { "failed" },
        agent: &config.agent,
        workload: &config.workload,
        provider: &config.provider,
        model: &config.model,
        process_success,
        workload_success,
        workload_evaluation: workloads::evaluation_kind(&config.workload, &workspace),
        failure_kind: failure.as_ref().map(|(kind, _)| *kind),
        failure_reason: failure.as_ref().map(|(_, reason)| reason.clone()),
        success_rate: run_metrics.success_rate,
        elapsed_ms: run_metrics.elapsed_ms,
        exit_code: child.status.code(),
        token_usage,
        steps,
        cassette: tape_cassette
            .as_ref()
            .map(|(_, cassette)| cassette.display().to_string()),
        stdout: bounded(&child.stdout),
        stderr,
    };
    write_log(config_path, &config.log_file, &result)?;
    if let Some(session) = measurement_session {
        let mut session = session;
        if !success {
            session
                .event(LifecycleEvent::RunFailed)
                .map_err(Error::Run)?;
        }
        session
            .event(LifecycleEvent::RunFinishing)
            .map_err(Error::Run)?;
        let measured = measurements::RunMeasurement {
            schema_version: 1,
            run_id: format!("{}-{}", result.workload, result.elapsed_ms),
            status: result.status,
            success,
            process_success: result.process_success,
            workload_success: result.workload_success,
            failure_kind: result.failure_kind,
            failure_reason: result.failure_reason.as_deref(),
            workload_evaluation: result.workload_evaluation,
            success_rate: result.success_rate,
            agent: result.agent,
            workload: result.workload,
            provider: result.provider,
            model: result.model,
            elapsed_ms,
            exit_code: result.exit_code,
            token_usage: result.token_usage.clone(),
            steps: result.steps,
            started_unix_ms: session.started_unix_ms(),
        };
        session.finish(measured).map_err(Error::Run)?;
    }
    if json {
        render_json(&result, output)?;
    } else {
        render_human(&result, output)?;
    }
    failure.map_or(Ok(()), |(_, reason)| Err(Error::Run(reason)))
}

fn instrument_operation<T, E>(
    mut session: Option<&mut measurements::Session>,
    phase: LifecyclePhase,
    operation: impl FnOnce() -> Result<T, E>,
) -> Result<Result<T, E>, Error> {
    if let Some(session) = session.as_deref_mut() {
        session
            .event(LifecycleEvent::PhaseBegin(phase))
            .map_err(Error::Run)?;
    }
    let result = operation();
    if let Some(session) = session {
        session
            .event(LifecycleEvent::PhaseEnd(phase))
            .map_err(Error::Run)?;
        if result.is_err() {
            session
                .event(LifecycleEvent::PhaseFailed(phase))
                .map_err(Error::Run)?;
        }
    }
    Ok(result)
}

fn instrument_phase<T>(
    session: Option<&mut measurements::Session>,
    phase: LifecyclePhase,
    operation: impl FnOnce() -> Result<T, Error>,
) -> Result<T, Error> {
    instrument_operation(session, phase, operation)?
}

/// Run the agent while delivering every lifecycle event through the collector
/// interface, including failures after the execution span has begun.
fn run_measured_agent(
    command: &mut Command,
    program: &str,
    target_args: Option<&[String]>,
    mut session: Option<&mut measurements::Session>,
    show_output: bool,
    output: &mut dyn Write,
) -> Result<Output, Error> {
    #[cfg(unix)]
    if session.is_some()
        && let Some(target_args) = target_args
        && gate_target_available(program)
    {
        let executable = std::env::current_exe()
            .map_err(|error| Error::Run(format!("cannot resolve ASB process gate: {error}")))?;
        crate::process_gate::install(command, &executable, program, target_args);
    }
    let child = {
        let mut emit = |event| {
            if let Some(session) = session.as_deref_mut() {
                session.event(event).map_err(Error::Run)?;
            }
            Ok(())
        };
        emit(LifecycleEvent::MeasurementsReady)?;
        emit(LifecycleEvent::AgentSpawn)?;
        emit(LifecycleEvent::AgentExecuteBegin)?;
        let child = match command.spawn() {
            Ok(child) => OwnedChild(child),
            Err(error) => {
                emit(LifecycleEvent::AgentExecuteEnd)?;
                emit(LifecycleEvent::AgentSpawnFailed)?;
                return Err(Error::Run(format!("could not start {program}: {error}")));
            }
        };
        emit(LifecycleEvent::AgentStarted)?;
        child
    };
    if let Some(session) = session.as_deref_mut()
        && let Err(error) = session.attach_pid(child.id())
    {
        session
            .event(LifecycleEvent::AgentExecuteEnd)
            .map_err(Error::Run)?;
        session
            .event(LifecycleEvent::AgentAttachFailed)
            .map_err(Error::Run)?;
        return Err(Error::Run(error));
    }
    #[cfg(unix)]
    if session.is_some() && target_args.is_some() {
        crate::process_gate::resume(child.id()).map_err(|error| {
            Error::Run(format!("could not resume measured agent process: {error}"))
        })?;
    }
    let result = wait_with_output(child, show_output, output);
    if let Some(session) = session {
        session
            .event(LifecycleEvent::AgentExecuteEnd)
            .map_err(Error::Run)?;
        session
            .event(if result.is_ok() {
                LifecycleEvent::AgentExited
            } else {
                LifecycleEvent::AgentWaitFailed
            })
            .map_err(Error::Run)?;
    }
    result.map_err(|error| Error::Run(format!("could not wait for {program}: {error}")))
}

#[cfg(unix)]
fn gate_target_available(program: &str) -> bool {
    let path = Path::new(program);
    if !path.is_file() {
        return !program.contains('/');
    }
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let Some(first_line) = bytes.split(|byte| *byte == b'\n').next() else {
        return true;
    };
    if !first_line.starts_with(b"#!") {
        return true;
    }
    let interpreter = first_line[2..]
        .split(|byte| byte.is_ascii_whitespace())
        .find(|part| !part.is_empty())
        .and_then(|part| std::str::from_utf8(part).ok());
    interpreter.is_none_or(|value| !value.starts_with('/') || Path::new(value).is_file())
}

fn default_cassette(config_path: &Path, config: &BenchmarkConfig) -> std::path::PathBuf {
    let stem = config_path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("benchmark");
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("cassettes")
        .join(format!(
            "{stem}-{}-{}-{}.jsonl",
            safe_name(&config.agent),
            safe_name(&config.workload),
            safe_name(&config.model)
        ))
}

fn safe_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn write_log(
    config_path: &Path,
    configured_path: &str,
    result: &RunResult<'_>,
) -> Result<(), Error> {
    let path = resolve_log_path(config_path, configured_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::Run(format!(
                "cannot create log directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| Error::Run(format!("cannot open log file {}: {error}", path.display())))?;
    serde_json::to_writer(&mut file, result).map_err(|error| {
        Error::Run(format!("cannot write log file {}: {error}", path.display()))
    })?;
    writeln!(file).map_err(|error| {
        Error::Run(format!(
            "cannot finish log file {}: {error}",
            path.display()
        ))
    })
}

fn resolve_log_path(config_path: &Path, configured_path: &str) -> PathBuf {
    let path = Path::new(configured_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
    }
}

fn render_json(result: &RunResult<'_>, output: &mut dyn Write) -> Result<(), Error> {
    let summary = serde_json::json!({
        "process_success": result.process_success,
        "workload_success": result.workload_success,
        "workload_evaluation": result.workload_evaluation,
        "failure_kind": result.failure_kind,
        "failure_reason": result.failure_reason,
        "success_rate": result.success_rate,
        "elapsed_ms": result.elapsed_ms,
        "steps": result.steps,
        "token_usage": result.token_usage,
    });
    // Sweep output is JSONL: keep each run summary on exactly one line so
    // callers can stream and parse it without having to recover pretty JSON
    // objects from a mixed human transcript.
    serde_json::to_writer(&mut *output, &summary).map_err(|error| Error::Io(error.to_string()))?;
    writeln!(output).map_err(io_error)
}

fn render_human(result: &RunResult<'_>, output: &mut dyn Write) -> Result<(), Error> {
    writeln!(
        output,
        "success_rate={:.3} elapsed_ms={} steps={} input_tokens={} output_tokens={} cached_tokens={} total_tokens={}",
        result.success_rate,
        result.elapsed_ms,
        result.steps,
        format_token(result.token_usage.input_tokens),
        format_token(result.token_usage.output_tokens),
        format_token(result.token_usage.cached_tokens),
        format_token(result.token_usage.total_tokens),
    )
    .map_err(io_error)
}

fn format_token(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".into(), |value| value.to_string())
}

#[cfg(test)]
fn process_successful(status: &std::process::ExitStatus, stdout: &[u8], stderr: &[u8]) -> bool {
    process_successful_with_final_response(status, stdout, stderr, None)
}

#[cfg(test)]
fn process_successful_with_final_response(
    status: &std::process::ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
    final_response: Option<&str>,
) -> bool {
    process_failure_reason(status, stdout, stderr, final_response).is_none()
}

fn process_failure_reason(
    status: &std::process::ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
    final_response: Option<&str>,
) -> Option<String> {
    let transcript = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    // Once a structured final response exists, inspect that response only.
    // Earlier command output is diagnostic evidence, not the agent outcome.
    let output = final_response.unwrap_or(&transcript);
    let has_completion_evidence = final_response.is_some()
        || output.contains("Agent finished")
        || output.contains("Saved trajectory to '.asb-mini-swe.traj.json'")
        || output.contains("Saved trajectory to \".asb-mini-swe.traj.json\"");
    // Provider and authentication failures are unambiguous and must remain
    // fatal even when an agent emitted a later structured response. Generic
    // command errors are ambiguous (the agent may have repaired them), so
    // inspect those only in the final response when one is available.
    let has_provider_fatal_marker = [
        "Authentication required. Please use /login command to sign in to your account",
        "Endpoint /v1/chat/completions was not found",
        "litellm.NotFoundError",
        "litellm.InternalServerError",
        "AnthropicException",
        "The API provider's servers are down or overloaded",
    ]
    .iter()
    .any(|marker| transcript.contains(marker));
    let has_command_fatal_marker = ["unexpected keyword argument"]
        .iter()
        .any(|marker| output.contains(marker));
    let cannot_run_tests = [
        "I can’t run tests",
        "I can't run tests",
        "I cannot run tests",
    ]
    .iter()
    .any(|marker| output.contains(marker));
    let has_refusal_marker = [
        "can't inspect",
        "cannot inspect",
        "don't have access to the terminal",
        "do not have access to the terminal",
        "no terminal or filesystem",
        "workspace is read-only",
        "workspace appears to be read-only",
        "unable to modify files",
        "can't modify files",
        "cannot modify files",
    ]
    .iter()
    .any(|marker| output.contains(marker));
    if has_provider_fatal_marker {
        let message = if transcript.contains(
            "Authentication required. Please use /login command to sign in to your account",
        ) {
            "agent reported an authentication failure; check its credentials and provider configuration"
        } else if transcript.contains("Endpoint /v1/chat/completions was not found")
            || transcript.contains("litellm.NotFoundError")
        {
            "agent reported a missing provider endpoint; check the provider URL and API compatibility"
        } else {
            "agent reported a provider error; inspect the benchmark log and check provider availability"
        };
        Some(if status.success() {
            message.into()
        } else {
            format!("{message} ({status})")
        })
    } else if !status.success() {
        Some(format!(
            "agent exited unsuccessfully ({status}); inspect the benchmark log for agent output"
        ))
    } else if (final_response.is_some() || !has_completion_evidence) && has_command_fatal_marker {
        Some("agent reported an incompatible command or argument; inspect the benchmark log and agent configuration".into())
    } else if (final_response.is_some() || !has_completion_evidence) && cannot_run_tests {
        Some(
            "agent reported it could not run tests; inspect the benchmark log and test environment"
                .into(),
        )
    } else if (final_response.is_some() || !has_completion_evidence) && has_refusal_marker {
        Some("agent reported it could not access the terminal or workspace, or could not modify files; inspect the benchmark log and permissions".into())
    } else {
        None
    }
}

enum OutputChunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Finished,
}

// std::process::Child does not stop a process on drop. Own it from spawn
// through all collector attachment, output and wait error paths.
struct OwnedChild(Child);

impl std::ops::Deref for OwnedChild {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

/// Wait for an agent while optionally forwarding both output streams live.
/// Readers run independently so a verbose agent cannot deadlock on a full
/// stdout or stderr pipe. The complete bounded-by-the-existing-runner capture
/// is still returned for logging and token parsing.
fn wait_with_output(
    mut child: OwnedChild,
    show_output: bool,
    output: &mut dyn Write,
) -> std::io::Result<Output> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("agent stdout was not piped"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("agent stderr was not piped"))?;
    let (sender, receiver) = mpsc::channel();
    let stdout_sender = sender.clone();
    std::thread::spawn(move || read_stream(stdout, OutputStream::Stdout, stdout_sender));
    std::thread::spawn(move || read_stream(stderr, OutputStream::Stderr, sender));

    let mut stdout_capture = Vec::new();
    let mut stderr_capture = Vec::new();
    let mut finished_streams = 0;
    let mut status = None;
    let deadline = Instant::now() + MAX_AGENT_RUNTIME;
    let mut timed_out = false;
    while (finished_streams < 2 || status.is_none()) && !timed_out {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(OutputChunk::Stdout(chunk)) => {
                if show_output {
                    output.write_all(&chunk)?;
                    output.flush()?;
                }
                stdout_capture.extend_from_slice(&chunk);
            }
            Ok(OutputChunk::Stderr(chunk)) => {
                if show_output {
                    output.write_all(&chunk)?;
                    output.flush()?;
                }
                stderr_capture.extend_from_slice(&chunk);
            }
            Ok(OutputChunk::Finished) => finished_streams += 1,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if status.is_none() {
            status = child.try_wait()?;
        }
        if status.is_none() && Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            status = Some(child.wait()?);
        }
    }
    let status = status.unwrap_or(child.wait()?);
    if timed_out {
        stderr_capture.extend_from_slice(
            format!(
                "agent execution exceeded {} seconds\n",
                MAX_AGENT_RUNTIME.as_secs()
            )
            .as_bytes(),
        );
    }
    Ok(Output {
        status,
        stdout: stdout_capture,
        stderr: stderr_capture,
    })
}

enum OutputStream {
    Stdout,
    Stderr,
}

fn read_stream<R: Read>(mut stream: R, kind: OutputStream, sender: mpsc::Sender<OutputChunk>) {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => {
                let chunk = buffer[..size].to_vec();
                let message = match kind {
                    OutputStream::Stdout => OutputChunk::Stdout(chunk),
                    OutputStream::Stderr => OutputChunk::Stderr(chunk),
                };
                if sender.send(message).is_err() {
                    return;
                }
            }
            Err(_) => break,
        }
    }
    let _ = sender.send(OutputChunk::Finished);
}

type CommandSpec = agents::CommandSpec;

#[cfg(test)]
fn command_for(
    config: &BenchmarkConfig,
    provider_base_url: Option<&str>,
    prompt: &str,
) -> Result<CommandSpec, Error> {
    let interface = agents::find(&config.agent).and_then(|adapter| {
        crate::endpoint::select(
            adapter.supported_endpoints(),
            &providers::offered_endpoints(&config.provider, &config.model),
        )
        .map(|offer| offer.interface)
    });
    command_for_with_interface(config, provider_base_url, prompt, interface)
}

fn command_for_with_interface(
    config: &BenchmarkConfig,
    provider_base_url: Option<&str>,
    prompt: &str,
    interface: Option<crate::endpoint::Interface>,
) -> Result<CommandSpec, Error> {
    if let Some(adapter) = agents::find(&config.agent) {
        return Ok(adapter.command(&agents::CommandContext {
            config,
            endpoint: provider_base_url,
            interface,
            prompt,
        }));
    }
    let agent_names = agents::names().collect::<Vec<_>>();
    if let Some(candidate) = suggestions::closest(&config.agent, &agent_names) {
        return Err(Error::Input(format!(
            "unknown agent `{}`; did you mean `{candidate}`?",
            config.agent
        )));
    }
    Ok(CommandSpec {
        program: config.agent.clone(),
        args: Vec::new(),
        append_prompt: true,
    })
}

fn bounded(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_OUTPUT)]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{
        RunResult, command_for, render_human, render_json, replica_cassette, resolve_log_path,
        run_with_tape, write_log,
    };
    use crate::benchmark::BenchmarkConfig;
    use crate::measurements::TokenUsage;

    #[test]
    fn replica_failures_group_only_exact_matches() {
        let mut failures = Vec::new();
        super::add_replica_failure(&mut failures, 1, "permission denied\nrepair A".into());
        super::add_replica_failure(&mut failures, 2, "permission denied\nrepair B".into());
        super::add_replica_failure(&mut failures, 3, "permission denied\nrepair A".into());
        assert_eq!(
            failures,
            vec![
                ("permission denied\nrepair A".into(), vec![1, 3]),
                ("permission denied\nrepair B".into(), vec![2]),
            ]
        );
    }

    #[test]
    fn identical_perfetto_marker_errors_are_grouped_across_four_replicas() {
        let mut failures = Vec::new();
        let error = "cannot write ASB lifecycle markers to /sys/kernel/tracing/trace_marker: Permission denied.\nWhat to do:\n  repair command";
        for replica in 1..=4 {
            super::add_replica_failure(&mut failures, replica, error.into());
        }
        assert_eq!(failures, vec![(error.into(), vec![1, 2, 3, 4])]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn every_instrumented_phase_closes_and_reports_failure_through_the_collector() {
        use crate::collectors::LifecyclePhase;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("asb-phase-{}-{unique}", std::process::id()));
        let mut session =
            crate::measurements::Session::start_with_collectors(&dir, &[], None).unwrap();
        let marker = dir.join("marker");
        session.set_test_marker(&marker);
        let phases = [
            LifecyclePhase::ProviderDiscovery,
            LifecyclePhase::ProviderSetup,
            LifecyclePhase::WorkloadPreparation,
            LifecyclePhase::ProxyPreparation,
            LifecyclePhase::AgentPreparation,
            LifecyclePhase::WorkloadEvaluation,
        ];
        let mut expected = String::new();
        for phase in phases {
            let result = super::instrument_operation(Some(&mut session), phase, || {
                Err::<(), _>("test failure")
            })
            .unwrap();
            assert!(result.is_err());
            expected.push_str(&format!(
                "B|{}|asb.{}.{}EI|{}|asb.{}.failed.{}",
                std::process::id(),
                phase.name(),
                session.trace_id(),
                std::process::id(),
                phase.name(),
                session.trace_id()
            ));
        }
        assert_eq!(std::fs::read_to_string(marker).unwrap(), expected);
        drop(session);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    fn check_lifecycle(command: &mut std::process::Command, expected: &[&str], fail_attach: bool) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("asb-events-{}-{unique}", std::process::id()));
        let mut session =
            crate::measurements::Session::start_with_collectors(&dir, &[], None).unwrap();
        let trace_id = session.trace_id().to_owned();
        session.test_attach_failure = fail_attach;
        let marker = dir.join("marker");
        session.set_test_marker(&marker);
        let _ = super::run_measured_agent(
            command,
            "fixture",
            None,
            Some(&mut session),
            false,
            &mut Vec::new(),
        );
        let actual = std::fs::read_to_string(&marker).unwrap();
        let pid = std::process::id();
        let expected = expected
            .iter()
            .map(|event| {
                if *event == "end" {
                    "E".to_owned()
                } else {
                    let (kind, name) = event.split_once(':').unwrap();
                    format!("{kind}|{pid}|{name}.{trace_id}")
                }
            })
            .collect::<String>();
        assert_eq!(actual, expected);
        drop(session);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn measured_agent_emits_all_success_events_in_order() {
        let mut command = std::process::Command::new("/bin/true");
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        check_lifecycle(
            &mut command,
            &[
                "I:asb.measurements.ready",
                "I:asb.agent.spawn",
                "B:asb.agent.execute",
                "I:asb.agent.started",
                "end",
                "I:asb.agent.exited",
            ],
            false,
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn measured_agent_emits_spawn_failure_events_in_order() {
        let mut command = std::process::Command::new("/asb-test-path-that-does-not-exist");
        check_lifecycle(
            &mut command,
            &[
                "I:asb.measurements.ready",
                "I:asb.agent.spawn",
                "B:asb.agent.execute",
                "end",
                "I:asb.agent.spawn_failed",
            ],
            false,
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn measured_agent_emits_wait_failure_events_in_order() {
        let mut command = std::process::Command::new("/bin/true");
        check_lifecycle(
            &mut command,
            &[
                "I:asb.measurements.ready",
                "I:asb.agent.spawn",
                "B:asb.agent.execute",
                "I:asb.agent.started",
                "end",
                "I:asb.agent.wait_failed",
            ],
            false,
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn measured_agent_emits_attach_failure_events_in_order() {
        let mut command = std::process::Command::new("/bin/sleep");
        command
            .arg("10")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        check_lifecycle(
            &mut command,
            &[
                "I:asb.measurements.ready",
                "I:asb.agent.spawn",
                "B:asb.agent.execute",
                "I:asb.agent.started",
                "end",
                "I:asb.agent.attach_failed",
            ],
            true,
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires a project-local Perfetto installation and host tracing privileges"]
    fn live_perfetto_captures_measured_agent_events_in_order() {
        let tools = std::path::PathBuf::from(
            std::env::var_os("ASB_TEST_PERFETTO_TOOLS").expect("set ASB_TEST_PERFETTO_TOOLS"),
        );
        let dir = tools.parent().unwrap().join("trace-test-output");
        let mut session = crate::measurements::Session::start_with_collectors(
            &dir,
            &["perfetto".into()],
            Some(&tools),
        )
        .expect("start real Perfetto");
        let id = session.trace_id().to_owned();
        let mut command = std::process::Command::new("/bin/sleep");
        command
            .arg("0.2")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = super::run_measured_agent(
            &mut command,
            "sleep",
            None,
            Some(&mut session),
            false,
            &mut Vec::new(),
        )
        .expect("run fixture");
        assert!(output.status.success());
        session
            .finish(crate::measurements::RunMeasurement {
                schema_version: 1,
                run_id: "test".into(),
                status: "completed",
                success: true,
                process_success: true,
                workload_success: Some(true),
                failure_kind: None,
                failure_reason: None,
                success_rate: 1.0,
                agent: "fixture",
                workload: "fixture",
                workload_evaluation: "fixture",
                provider: "fixture",
                model: "fixture",
                elapsed_ms: 200,
                exit_code: Some(0),
                token_usage: crate::measurements::TokenUsage::default(),
                steps: 1,
                started_unix_ms: 0,
            })
            .expect("finish collector");
        let trace = dir.join("perfetto-scheduling.perfetto-trace");
        let query = format!("SELECT name FROM slice WHERE name GLOB 'asb.*.{id}' ORDER BY ts");
        let result = std::process::Command::new(tools.join("perfetto/bin/trace_processor_shell"))
            .arg("query")
            .arg(&trace)
            .arg(query)
            .output()
            .expect("trace processor");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let rows = String::from_utf8_lossy(&result.stdout);
        let expected = [
            "asb.measurements.ready",
            "asb.agent.spawn",
            "asb.agent.execute",
            "asb.agent.started",
            "asb.agent.exited",
        ];
        let mut cursor = 0;
        for name in expected {
            let needle = format!("{name}.{id}");
            let offset = rows[cursor..]
                .find(&needle)
                .unwrap_or_else(|| panic!("missing {needle} in {rows}"));
            cursor += offset + needle.len();
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn child_is_killed_and_reaped_if_collector_attachment_aborts() {
        let child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let pid = child.id();
        let owned = super::OwnedChild(child);
        // Simulate returning from the runner after attachment fails.
        drop(owned);
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn child_is_killed_and_reaped_when_output_writer_fails() {
        struct FailingWriter;
        impl std::io::Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("fixture output failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let child = std::process::Command::new("sh")
            .args(["-c", "printf ready; exec sleep 10"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let error = super::wait_with_output(super::OwnedChild(child), true, &mut FailingWriter)
            .unwrap_err();
        assert!(error.to_string().contains("fixture output failure"));
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn concurrent_replay_fans_out_single_recording_and_supports_replica_recordings() {
        let root =
            std::env::temp_dir().join(format!("asb-replica-cassettes-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let base = root.join("record.jsonl");
        assert_eq!(
            replica_cassette(crate::tape::Mode::Replay, &base, 8),
            root.join("record-concurrency-8.jsonl")
        );
        std::fs::write(&base, "test").unwrap();
        for index in 1..=8 {
            assert_eq!(
                replica_cassette(crate::tape::Mode::Replay, &base, index),
                base
            );
        }
        std::fs::remove_file(&base).unwrap();
        let recorded_replica = root.join("record-concurrency-2.jsonl");
        std::fs::write(&recorded_replica, "test").unwrap();
        assert_eq!(
            replica_cassette(crate::tape::Mode::Replay, &base, 3),
            recorded_replica
        );
        assert_eq!(
            replica_cassette(crate::tape::Mode::Record, &base, 8),
            root.join("record-concurrency-8.jsonl")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_replay_without_explicit_cassette_fails_closed() {
        let root =
            std::env::temp_dir().join(format!("asb-replay-no-cassette-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("benchmark.toml");
        std::fs::write(&config, "agent='pi'\nworkload='bug-fix'\nprovider='not-a-provider'\nmodel='model'\nconcurrency=8\n").unwrap();
        let error = run_with_tape(
            config.to_str().unwrap(),
            super::RunOptions {
                mode: Some(crate::tape::Mode::Replay),
                ..Default::default()
            },
            &mut Vec::new(),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("replay cassette"), "{message}");
        assert!(!message.contains("unsupported provider"), "{message}");
        std::fs::remove_dir_all(root).unwrap();
    }

    fn result<'a>() -> RunResult<'a> {
        RunResult {
            status: "completed",
            agent: "codex",
            workload: "bug-fix",
            provider: "openrouter",
            model: "test-model",
            process_success: true,
            workload_success: Some(true),
            workload_evaluation: "bundled-fixture",
            failure_kind: None,
            failure_reason: None,
            success_rate: 1.0,
            elapsed_ms: 12,
            exit_code: Some(0),
            token_usage: TokenUsage {
                input_tokens: Some(3),
                output_tokens: Some(4),
                cached_tokens: Some(1),
                total_tokens: Some(7),
                source: Some("test".into()),
            },
            steps: 2,
            cassette: None,
            stdout: "done".into(),
            stderr: String::new(),
        }
    }

    #[test]
    fn human_output_is_concise_and_not_json() {
        let mut output = Vec::new();
        render_human(&result(), &mut output).expect("human rendering succeeds");
        let text = String::from_utf8(output).expect("utf8");
        assert!(text.starts_with("success_rate=1.000"));
        assert!(text.contains("elapsed_ms=12"));
        assert!(text.contains("steps=2"));
        assert!(text.contains("cached_tokens=1"));
        assert!(text.contains("total_tokens=7"));
        assert!(!text.contains("done"));
        assert!(!text.starts_with('{'));
    }

    #[test]
    fn json_output_remains_machine_readable() {
        let mut output = Vec::new();
        render_json(&result(), &mut output).expect("json rendering succeeds");
        let value: serde_json::Value = serde_json::from_slice(&output).expect("valid json");
        assert_eq!(value["success_rate"], 1.0);
        assert_eq!(value["elapsed_ms"], 12);
        assert_eq!(value["steps"], 2);
        assert_eq!(value["token_usage"]["total_tokens"], 7);
        assert_eq!(value["token_usage"]["cached_tokens"], 1);
        assert!(value.get("stdout").is_none());
        assert_eq!(output.iter().filter(|byte| **byte == b'\n').count(), 1);
    }

    #[test]
    fn log_path_is_relative_to_configuration_file() {
        assert_eq!(
            resolve_log_path(std::path::Path::new("projects/foo.toml"), "foo.log"),
            std::path::PathBuf::from("projects/foo.log")
        );
        assert_eq!(
            resolve_log_path(
                std::path::Path::new("projects/foo.toml"),
                "/tmp/asb-run.log"
            ),
            std::path::PathBuf::from("/tmp/asb-run.log")
        );
    }

    #[test]
    fn log_contains_full_bounded_result_while_summary_hides_agent_output() {
        let directory = std::env::temp_dir().join(format!("asb-runner-log-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("log test directory");
        let config = directory.join("foo.toml");
        write_log(&config, "foo.log", &result()).expect("log writes");
        let text = std::fs::read_to_string(directory.join("foo.log")).expect("log reads");
        let value: serde_json::Value = serde_json::from_str(text.trim()).expect("log JSON");
        assert_eq!(value["stdout"], "done");
        assert_eq!(value["success_rate"], 1.0);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn cli2key_command_mappings_place_workload_once() {
        let config = BenchmarkConfig {
            agent: "openhands".into(),
            workload: "bug-fix".into(),
            provider: "cli2key".into(),
            model: "gpt-test".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
        };
        let command = command_for(&config, Some("http://127.0.0.1:3456/v1"), "prompt")
            .expect("known agent has a command");
        assert_eq!(command.program, "openhands");
        assert!(!command.append_prompt);
        assert!(command.args.contains(&"--json".into()));
        assert_eq!(command.args.last().map(String::as_str), Some("prompt"));
    }

    #[test]
    fn token_reporting_commands_request_machine_readable_agent_output() {
        let base = BenchmarkConfig {
            agent: "mini-swe".into(),
            workload: "bug-fix".into(),
            provider: "cli2key".into(),
            model: "gpt-test".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
        };
        let mini = command_for(&base, Some("http://127.0.0.1:3456/v1"), "prompt").unwrap();
        assert!(
            mini.args
                .windows(2)
                .any(|pair| pair[0] == "--output" && pair[1] == ".asb-mini-swe.traj.json")
        );

        let mut openjiuwen = base.clone();
        openjiuwen.agent = "openjiuwen".into();
        let command = command_for(&openjiuwen, Some("http://127.0.0.1:3456/v1"), "prompt").unwrap();
        assert!(
            command
                .args
                .windows(2)
                .any(|pair| pair[0] == "-f" && pair[1] == "stream-json")
        );
    }

    #[test]
    fn newly_supported_agent_commands_are_noninteractive_and_machine_readable() {
        for (agent, program, marker) in [
            ("claude-code", "claude", "--output-format"),
            ("goose", "goose", "--output-format"),
            ("cline", "cline", "--json"),
            ("continue", "cn", "--auto"),
        ] {
            let config = BenchmarkConfig {
                agent: agent.into(),
                workload: "bug-fix".into(),
                provider: "cli2key".into(),
                model: "gpt-test".into(),
                concurrency: 1,
                trial_set: None,
                trial: None,
                workspace: "workspace".into(),
                log_file: "run.log".into(),
                measurements: None,
            };
            let command = command_for(&config, Some("http://127.0.0.1:3456/v1"), "prompt")
                .expect("new agent command");
            assert_eq!(command.program, program, "{agent} program");
            assert!(
                command.args.iter().any(|value| value == marker),
                "{agent} marker"
            );
            assert!(!command.append_prompt, "{agent} embeds prompt exactly once");
        }
    }

    #[test]
    fn pi_cli2key_uses_openai_bridge_and_print_mode() {
        let config = BenchmarkConfig {
            agent: "pi".into(),
            workload: "bug-fix".into(),
            provider: "cli2key".into(),
            model: "gpt-6.1-sol".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
        };
        let command = command_for(&config, Some("http://127.0.0.1:3456/v1"), "prompt")
            .expect("pi has a command");
        assert_eq!(command.program, "pi");
        assert_eq!(command.args[0], "--provider");
        assert_eq!(command.args[1], "asb");
        assert!(command.args.contains(&"--print".into()));
        assert!(command.args.contains(&"--mode".into()));
        assert!(command.args.contains(&"json".into()));
        assert!(command.append_prompt);
    }

    #[test]
    fn zero_exit_provider_errors_are_not_successes() {
        let status = std::process::Command::new("true")
            .status()
            .expect("true exists");
        assert!(!super::process_successful(
            &status,
            b"Authentication required. Please use /login command to sign in to your account",
            b""
        ));
        assert!(!super::process_successful(
            &status,
            b"litellm.NotFoundError: Endpoint /v1/chat/completions was not found.",
            b""
        ));
        assert!(!super::process_successful(
            &status,
            b"litellm.InternalServerError: AnthropicException - unexpected keyword argument 'system'",
            b""
        ));
        assert!(super::process_successful(&status, b"completed", b""));
    }

    #[test]
    fn zero_exit_tool_refusals_are_not_successes() {
        let status = std::process::Command::new("true")
            .status()
            .expect("true exists");
        assert!(!super::process_successful(
            &status,
            b"I can't run tests or inspect src/parser.py because I do not have access to the terminal",
            b""
        ));
        assert!(!super::process_successful(
            &status,
            b"The workspace appears to be read-only; I can't modify files",
            b""
        ));
    }

    #[test]
    fn agent_failure_reasons_are_specific_without_exposing_transcript() {
        let status = std::process::Command::new("true").status().unwrap();
        for (output, expected) in [
            (
                "Authentication required. Please use /login command to sign in to your account secret-value",
                "authentication failure",
            ),
            (
                "Endpoint /v1/chat/completions was not found secret-value",
                "missing provider endpoint",
            ),
            ("litellm.InternalServerError secret-value", "provider error"),
            (
                "unexpected keyword argument secret-value",
                "incompatible command or argument",
            ),
            ("I can't run tests secret-value", "could not run tests"),
            (
                "workspace is read-only secret-value",
                "could not access the terminal or workspace",
            ),
        ] {
            let reason =
                super::process_failure_reason(&status, output.as_bytes(), b"", None).unwrap();
            assert!(reason.contains(expected), "{reason}");
            assert!(!reason.contains("secret-value"));
        }
        let failed_status = std::process::Command::new("false").status().unwrap();
        let reason = super::process_failure_reason(
            &failed_status,
            b"Authentication required. Please use /login command to sign in to your account secret-value",
            b"",
            None,
        ).unwrap();
        assert!(reason.contains("authentication failure"), "{reason}");
        assert!(reason.contains("exit status"), "{reason}");
        assert!(!reason.contains("secret-value"));
    }

    #[test]
    fn transient_agent_test_errors_do_not_hide_completion() {
        let status = std::process::Command::new("true")
            .status()
            .expect("true exists");
        assert!(super::process_successful(
            &status,
            b"Tool returned TypeError: unexpected keyword argument 'reverse'\nAgent finished",
            b""
        ));
        assert!(super::process_successful(
            &status,
            b"Tool returned TypeError: unexpected keyword argument 'reverse'\nSaved trajectory to '.asb-mini-swe.traj.json'",
            b""
        ));
        assert!(super::process_successful_with_final_response(
            &status,
            b"Tool returned TypeError: unexpected keyword argument 'reverse'",
            b"",
            Some("Updated the implementation and verified the supplied test passes."),
        ));
        assert!(!super::process_successful_with_final_response(
            &status,
            b"Tool output",
            b"",
            Some("I cannot modify files in the workspace."),
        ));
        assert!(!super::process_successful_with_final_response(
            &status,
            b"litellm.InternalServerError: upstream unavailable",
            b"",
            Some("Updated the implementation and verified the supplied test passes."),
        ));
        assert!(!super::process_successful_with_final_response(
            &status,
            b"litellm.InternalServerError: upstream unavailable\nAgent finished",
            b"",
            None,
        ));
    }

    #[test]
    fn opencode_command_uses_generic_provider_model_namespace() {
        let config = BenchmarkConfig {
            agent: "opencode".into(),
            workload: "bug-fix".into(),
            provider: "opencode-zen".into(),
            model: "mimo-v2.6-flash-free".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
        };
        let command = command_for(&config, None, "prompt").expect("known agent");
        assert!(
            command.args.windows(2).any(|pair| {
                pair == ["--model".to_owned(), "asb/mimo-v2.6-flash-free".to_owned()]
            })
        );
        assert!(
            command
                .args
                .windows(2)
                .any(|pair| { pair == ["--dir".to_owned(), "workspace".to_owned()] })
        );
    }

    #[test]
    fn unknown_agent_suggests_a_close_match() {
        let config = BenchmarkConfig {
            agent: "opencod".into(),
            workload: "bug-fix".into(),
            provider: "local".into(),
            model: "test-model".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
        };
        let error = match command_for(&config, None, "prompt") {
            Ok(_) => panic!("typo rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("did you mean `opencode`?"));
    }
}
