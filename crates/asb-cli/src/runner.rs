//! Live execution of one configured benchmark.

use super::{Error, io_error, read_config};
use crate::agents;
use crate::benchmark::BenchmarkConfig;
use crate::measurements;
use crate::models;
use crate::prepare;
use crate::project;
use crate::providers;
use crate::suggestions;
use crate::tape;
use crate::workloads;
use serde::Serialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use std::time::Instant;

const MAX_OUTPUT: usize = 1024 * 1024;
const MAX_AGENT_RUNTIME: Duration = Duration::from_secs(15 * 60);

#[derive(Serialize)]
struct RunResult<'a> {
    status: &'a str,
    agent: &'a str,
    workload: &'a str,
    provider: &'a str,
    model: &'a str,
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

/// Execute one configured benchmark, optionally through a local llmtape proxy.
pub fn run_with_tape(
    path: &str,
    json: bool,
    show_output: bool,
    measurement_dir: Option<&str>,
    mode: Option<tape::Mode>,
    cassette: Option<&str>,
    respect_record_timings: bool,
    output: &mut dyn Write,
) -> Result<(), Error> {
    let config_path = Path::new(path);
    let text = read_config(config_path)?;
    let config: BenchmarkConfig = toml::from_str(&text)
        .map_err(|error| Error::Input(format!("cannot parse {path}: {error}")))?;
    let tape_request = mode.map(|value| (value, cassette));
    if config.concurrency > 1 {
        return run_concurrent(
            config_path,
            &config,
            json,
            show_output,
            measurement_dir,
            tape_request,
            respect_record_timings,
            output,
        );
    }
    run_mode(
        config_path,
        config,
        json,
        show_output,
        measurement_dir,
        tape_request,
        respect_record_timings,
        output,
    )
}

/// Execute identical replicas concurrently. Each replica gets an isolated
/// workspace, log, measurement directory, and record cassette so agents never
/// mutate one another's files. Output is buffered per replica and emitted in
/// replica order after completion to keep the terminal readable.
fn run_concurrent(
    config_path: &Path,
    config: &BenchmarkConfig,
    json: bool,
    show_output: bool,
    measurement_dir: Option<&str>,
    tape_request: Option<(tape::Mode, Option<&str>)>,
    respect_record_timings: bool,
    output: &mut dyn Write,
) -> Result<(), Error> {
    let mut jobs = Vec::with_capacity(config.concurrency);
    for index in 1..=config.concurrency {
        let mut replica = config.clone();
        replica.concurrency = 1;
        replica.workspace = parallel_path(&replica.workspace, index);
        replica.log_file = parallel_path(&replica.log_file, index);
        if let Some(measurements) = replica.measurements.as_mut() {
            measurements.directory = parallel_path(&measurements.directory, index);
        }
        let cassette = tape_request.and_then(|(mode, requested)| {
            requested
                .map(|path| append_path_suffix(Path::new(path), index))
                .or_else(|| {
                    (mode == tape::Mode::Record).then(|| {
                        append_path_suffix(&default_cassette(config_path, &replica), index)
                    })
                })
                .map(|path| (mode, path))
        });
        let measurement = measurement_dir.map(|path| PathBuf::from(parallel_path(path, index)));
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
                            json,
                            show_output,
                            measurement_path,
                            mode.map(|mode| (mode, cassette_path)),
                            respect_record_timings,
                            &mut buffer,
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
    let mut failures = Vec::new();
    for (index, result, buffer) in completed {
        output.write_all(&buffer).map_err(io_error)?;
        if let Err(error) = result {
            failures.push(format!("replica {index}: {error}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::Run(format!(
            "{} concurrent benchmark replica(s) failed: {}",
            failures.len(),
            failures.join("; ")
        )))
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
    json: bool,
    show_output: bool,
    measurement_dir: Option<&str>,
    tape_request: Option<(tape::Mode, Option<&str>)>,
    respect_record_timings: bool,
    output: &mut dyn Write,
) -> Result<(), Error> {
    let tape_cassette = tape_request.map(|(mode, requested)| {
        (
            mode,
            requested
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| default_cassette(config_path, &config)),
        )
    });
    if let Some((tape::Mode::Replay, cassette)) = tape_cassette.as_ref() {
        models::ensure_replay_tool_capable(cassette, &config.model)?;
    } else {
        models::ensure_tool_capable(&config.provider, &config.model)?;
    }
    let (project_root, project_config) = project::load(config_path)?;
    let project_root = project_root.canonicalize().map_err(|error| {
        Error::Input(format!(
            "cannot resolve benchmark project directory: {error}"
        ))
    })?;
    let workspace_name = if config.workspace == "asb-workspace" {
        &project_config.workspace_dir
    } else {
        &config.workspace
    };
    let workspace = project::resolve(&project_root, workspace_name);
    std::fs::create_dir_all(&workspace).map_err(|error| {
        Error::Run(format!(
            "cannot create workspace {}: {error}",
            workspace.display()
        ))
    })?;
    let workload_directory = std::env::var_os("ASB_WORKLOAD_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| project::resolve(&project_root, &project_config.workloads_dir));
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
    let tape_mode = tape_request.map(|(mode, _)| mode);
    let bridge = providers::start_local_bridge(
        &config.provider,
        &project_root,
        &project_config.tools_dir,
        tape_mode == Some(tape::Mode::Replay),
    )?;
    let mut tape_session = if let Some((mode, cassette)) = tape_cassette.as_ref() {
        let upstream = providers::tape_upstream(
            &config.provider,
            providers::local_bridge_endpoint(&config.provider, bridge.as_ref()),
        );
        Some(tape::Session::start(
            *mode,
            cassette,
            upstream.as_deref(),
            respect_record_timings,
        )?)
    } else {
        None
    };
    let tape_url = tape_session.as_ref().map(|session| session.url());
    // Adapters run with the benchmark workspace as their current directory.
    // Pass the canonical workspace explicitly so a relative configuration
    // value is not resolved a second time inside that directory (which is
    // especially important for OpenJiuwen's persistent workspace).
    let mut command_config = config.clone();
    command_config.workspace = workspace.to_string_lossy().into_owned();
    let specification = command_for(
        &command_config,
        tape_url.or_else(|| providers::local_bridge_endpoint(&config.provider, bridge.as_ref())),
        &prompt,
    )?;
    let mut program = specification.program;
    if let Some(local) = agents::resolve(&config.agent, &project_root, &project_config.tools_dir) {
        program = local;
    }
    let started = Instant::now();
    let mut command = Command::new(&program);
    command.args(specification.args);
    if specification.append_prompt {
        command.arg(prompt);
    }
    command
        .current_dir(&workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    providers::configure(&mut command, &config.provider, &config.model);
    let provider_endpoint =
        tape_url.or_else(|| providers::local_bridge_endpoint(&config.provider, bridge.as_ref()));
    let provider_credentials =
        providers::local_bridge_credentials(&config.provider, bridge.as_ref(), provider_endpoint);
    prepare::apply_with_tape(
        &mut command,
        &config.provider,
        &config.model,
        provider_credentials,
        tape_url,
        tape_mode == Some(tape::Mode::Replay),
    )?;
    agents::prepare(
        &mut command,
        &config.agent,
        &config.provider,
        &config.model,
        provider_credentials,
        &project::resolve(&project_root, &project_config.tools_dir),
    )?;
    if let Some(url) = tape_url {
        providers::configure_tape(
            &mut command,
            &config.provider,
            url,
            tape_mode.expect("tape URL implies tape mode"),
        );
    }
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
        session.trace_instant("asb.measurements.ready");
        session.trace_instant("asb.agent.spawn");
        session.trace_begin("asb.agent.execute");
    }
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            if let Some(session) = measurement_session.as_mut() {
                session.trace_end();
                session.trace_instant("asb.agent.spawn_failed");
            }
            return Err(Error::Run(format!("could not start {program}: {error}")));
        }
    };
    if let Some(session) = measurement_session.as_mut() {
        session.trace_instant("asb.agent.started");
        session.attach_pid(child.id()).map_err(Error::Run)?;
    }
    let child = wait_with_output(child, show_output, output)
        .map_err(|error| Error::Run(format!("could not wait for {program}: {error}")))?;
    if let Some(session) = measurement_session.as_mut() {
        session.trace_end();
        session.trace_instant("asb.agent.exited");
    }
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
                    "record completed without creating cassette {}; verify the provider endpoint and llmtape installation: {error}",
                    cassette.display()
                ));
            }
        }
    }
    let elapsed_ms = started.elapsed().as_millis();
    let success =
        run_error.is_none() && process_successful(&child.status, &child.stdout, &child.stderr);
    let adapter_artifacts = agents::find(&config.agent)
        .map(|adapter| {
            adapter.collect(&agents::CollectionContext {
                workspace: &workspace,
            })
        })
        .unwrap_or_default();
    let mut token_usage = measurements::token_usage_from_sources(
        &config.provider,
        &config.agent,
        &bounded(&child.stdout),
        &bounded(&child.stderr),
        tape_cassette
            .as_ref()
            .map(|(_, cassette)| cassette.as_path()),
    );
    if let Some(artifact_usage) = adapter_artifacts.token_usage {
        token_usage = token_usage
            .merge_with_source(artifact_usage, "agent-artifact")
            .with_derived_total();
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
    let result = RunResult {
        status: if success { "completed" } else { "failed" },
        agent: &config.agent,
        workload: &config.workload,
        provider: &config.provider,
        model: &config.model,
        success_rate: if success { 1.0 } else { 0.0 },
        elapsed_ms,
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
        let measured = measurements::RunMeasurement {
            schema_version: 1,
            run_id: format!("{}-{}", result.workload, result.elapsed_ms),
            status: result.status,
            success,
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
    if let Some(error) = run_error {
        Err(Error::Run(error))
    } else if success {
        Ok(())
    } else {
        Err(Error::Run("agent exited unsuccessfully".into()))
    }
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
        "success_rate": result.success_rate,
        "elapsed_ms": result.elapsed_ms,
        "steps": result.steps,
        "token_usage": result.token_usage,
    });
    serde_json::to_writer_pretty(&mut *output, &summary)
        .map_err(|error| Error::Io(error.to_string()))?;
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

fn process_successful(status: &std::process::ExitStatus, stdout: &[u8], stderr: &[u8]) -> bool {
    if !status.success() {
        return false;
    }
    let output = format!(
        "{}\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    );
    ![
        "Authentication required. Please use /login command to sign in to your account",
        "Endpoint /v1/chat/completions was not found",
        "litellm.NotFoundError",
        "litellm.InternalServerError",
        "AnthropicException",
        "unexpected keyword argument",
        "The API provider's servers are down or overloaded",
    ]
    .iter()
    .any(|marker| output.contains(marker))
}

enum OutputChunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Finished,
}

/// Wait for an agent while optionally forwarding both output streams live.
/// Readers run independently so a verbose agent cannot deadlock on a full
/// stdout or stderr pipe. The complete bounded-by-the-existing-runner capture
/// is still returned for logging and token parsing.
fn wait_with_output(
    mut child: Child,
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

fn command_for(
    config: &BenchmarkConfig,
    provider_base_url: Option<&str>,
    prompt: &str,
) -> Result<CommandSpec, Error> {
    if let Some(adapter) = agents::find(&config.agent) {
        return Ok(adapter.command(&agents::CommandContext {
            config,
            endpoint: provider_base_url,
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
    if let Some(candidates) = providers::known_models(&config.provider) {
        if let Some(candidate) = suggestions::closest(&config.model, candidates) {
            return Err(Error::Input(format!(
                "unknown model `{}` for provider `{}`; did you mean `{candidate}`?",
                config.model, config.provider
            )));
        }
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
    use super::{RunResult, command_for, render_human, render_json, resolve_log_path, write_log};
    use crate::benchmark::BenchmarkConfig;
    use crate::measurements::TokenUsage;

    fn result<'a>() -> RunResult<'a> {
        RunResult {
            status: "completed",
            agent: "codex",
            workload: "bug-fix",
            provider: "openrouter",
            model: "test-model",
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
                .any(|pair| pair[0] == "-f" && pair[1] == "json")
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
        assert_eq!(command.args[1], "cli2key");
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
    fn opencode_zen_command_uses_opencode_provider_model_namespace() {
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
        assert!(command.args.windows(2).any(|pair| {
            pair == [
                "--model".to_owned(),
                "opencode/mimo-v2.6-flash-free".to_owned(),
            ]
        }));
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
