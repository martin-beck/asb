//! Live execution of one configured benchmark.
mod attempt;
mod batch;
pub(crate) use batch::{dataset_fingerprint, replay_protocol_fingerprint};
pub(crate) mod campaign;
mod compaction;
mod completion;
pub(crate) mod diagnostics;
pub(crate) mod disposition;
#[cfg(test)]
use disposition::RunStatus;
use disposition::{FailureKind, RunDisposition, RunFacts, RunFailure};
mod native;
mod phases;
mod progress;
pub(crate) mod recording;
pub(crate) mod repeated;
use phases::{
    AdmittedAttempt, AttemptIssues, ExecutionEvidence, FinalizedAttempt, MeasuredOutput,
    PreparedAttempt,
};
pub(crate) mod replica;
pub(crate) mod reuse;
#[cfg(target_os = "linux")]
pub(crate) mod workload_process;

use super::{Error, io_error, read_config};
use crate::agents;
use crate::benchmark::BenchmarkConfig;
use crate::benchmark_paths::{benchmark_stem, component};
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
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

const MAX_OUTPUT: usize = 1024 * 1024;
const MAX_AGENT_RUNTIME: Duration =
    Duration::from_secs(crate::planning::DEFAULT_NATIVE_EXECUTION_SECONDS);

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
    #[serde(skip_serializing_if = "Option::is_none")]
    recording: Option<recording::Evidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution: Option<repeated::ExecutionIdentity>,
    planning: crate::planning::ResolvedPlan,
    effective_budgets: crate::planning::EffectiveBudgets,
    execution_plan: Option<PathBuf>,
    execution_plan_fingerprint: Option<crate::planning::ExecutionPlanFingerprint>,
    module: Option<workloads::contracts::ModuleDescriptor>,
    benchmark_outcome: Option<workloads::outcome::BenchmarkOutcome>,
    replica_index: Option<usize>,
    execution_cohort: Option<&'a measurements::ExecutionCohort>,
    schema_version: u32,
    #[serde(flatten)]
    disposition: RunDisposition,
    agent: &'a str,
    workload: &'a str,
    task: workloads::TaskIdentity,
    provider: &'a str,
    model: &'a str,
    process_success: bool,
    workload_success: Option<bool>,
    workload_resource_disposition: Option<workloads::ResourceDisposition>,
    workload_artifacts: Vec<PathBuf>,
    workload_evaluation: &'static str,
    workload_baseline_passed: Option<bool>,
    workload_baseline: workloads::BaselineEvidence,
    workload_failure: Option<workloads::EvaluationFailure>,
    success_rate: Option<f64>,
    adapter_smoke_success: Option<bool>,
    elapsed_ms: u128,
    exit_code: Option<i32>,
    #[serde(flatten)]
    observation: crate::process_termination::Observation,
    token_usage: measurements::TokenUsage,
    steps: Option<u64>,
    tool_calls: Vec<crate::proxy::tool_calls::ToolCall>,
    tool_calls_available: bool,
    #[serde(skip)]
    adapter_tool_call_count: Option<usize>,
    /// Private, validated input for cassette publication. This is never put
    /// in user-facing run JSON or logs; the recorder seals it as an auxiliary
    /// `local-tools` cassette payload.
    #[serde(skip)]
    local_tool_transcript: Option<Vec<u8>>,
    #[serde(skip)]
    deterministic_manifest: Option<Vec<u8>>,
    #[serde(skip)]
    native_trace: Option<Vec<u8>>,
    /// Complete private native-tool stream supplied by an adapter.  This is
    /// consumed only by recording publication, never user-facing output.
    native_tool_sidecar: Option<Vec<u8>>,
    /// Adapter-owned bounded sidecars retained only by the sealed cassette.
    #[serde(skip)]
    sealed_artifacts: Vec<agents::SealedArtifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cassette: Option<String>,
    /// Actual capture directory, including a CLI --measurements override.
    #[serde(skip_serializing_if = "Option::is_none")]
    measurement_directory: Option<PathBuf>,
    stdout: String,
    stderr: String,
}

impl RunResult<'_> {
    fn tool_call_count(&self) -> Option<usize> {
        if self.tool_calls_available {
            Some(self.tool_calls.len())
        } else {
            self.adapter_tool_call_count
        }
    }
}

/// Execution choices shared by single runs, replicas, and sweeps.
#[derive(Clone, Copy, Default)]
pub(crate) struct RunOptions<'a> {
    pub track: Option<crate::cassette::Selector>,
    pub replay_seed: Option<u64>,
    pub recording: Option<&'a recording::Recorder>,
    /// Opaque, recorder-owned provider capture selected before dispatch.
    pub provider_capture_path: Option<&'a Path>,
    pub repeat: Option<u64>,
    /// Per-execution agent turn budget. This is enforced only by adapters
    /// which explicitly declare support; unsupported adapters fail closed.
    pub max_steps: Option<u64>,
    pub batch: Option<&'a repeated::BatchIdentity>,
    /// Exact selected native replay inputs for this batch.  This is private
    /// execution state and is never serialized into a plan or result.
    #[cfg_attr(test, allow(dead_code))]
    pub native_tool_inputs: Option<&'a [&'a campaign::ReplayInput]>,
    pub verify_initial_material: bool,
    /// Sweep preflight already validated the exact project-local installation
    /// requirements for this fresh run population. Direct and resumed runs
    /// leave this false and revalidate before execution.
    pub requirements_prepared: bool,
    pub preparation_gate: Option<&'a batch::Gate>,
    pub launch_gate: Option<&'a batch::Gate>,
    pub policy: Option<&'a crate::planning::ResolvedPlan>,
    pub execution_cohort: Option<&'a measurements::ExecutionCohort>,
    pub json: bool,
    pub show_output: bool,
    /// Suppress diagnostic evidence paths and detailed replay/recording information.
    pub quiet_output: bool,
    /// Print the selected trial before any run work begins.
    pub announce_trial: bool,
    /// The process was launched with both presentation streams attached to a terminal.
    pub interactive: bool,
    pub measurement_dir: Option<&'a str>,
    pub mode: Option<tape::Mode>,
    /// Adapter-local tool recording/replay is independently selectable from
    /// provider recording/replay, while sharing the sealed cassette carrier.
    pub tool_mode: Option<tape::Mode>,
    /// Internal callers predating the split options retain the historical
    /// combined behavior unless they opt into an explicit tool selection.
    pub tool_mode_explicit: bool,
    /// Transparent protocol recording for declared ordinary MCP servers.
    pub mcp_mode: Option<tape::Mode>,
    pub cassette: Option<&'a str>,
    /// Explicit fresh destination for a bounded native workspace capsule.
    pub native_capsule_record: Option<&'a str>,
    /// Verified native workspace capsule restored before an LLM replay.
    pub native_capsule_replay: Option<&'a str>,
    pub respect_record_timings: bool,
    pub replica: replica::Identity,
    pub on_completion: Option<&'a dyn replica::CompletionObserver>,
    pub reused: Option<&'a std::collections::BTreeMap<usize, serde_json::Value>>,
}

impl RunOptions<'_> {
    fn effective_tool_mode(&self) -> Option<tape::Mode> {
        if self.tool_mode_explicit {
            self.tool_mode
        } else {
            self.mode
        }
    }

    fn replay_statistics(&self) -> Option<crate::cassette::Statistics> {
        self.batch
            .and_then(|batch| batch.replay_statistics(self.replica.index))
    }
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
    let cassette = options
        .cassette
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| default_cassette(config_path, &config));
    let derived_capsule = automatic_native_capsule(
        &config.agent,
        options.mode,
        options.native_capsule_record.is_some() || options.native_capsule_replay.is_some(),
        &cassette,
    );
    let options = RunOptions {
        native_capsule_record: options
            .native_capsule_record
            .or((options.mode == Some(tape::Mode::Record))
                .then_some(derived_capsule.as_deref())
                .flatten()),
        native_capsule_replay: options.native_capsule_replay.or_else(|| {
            // LLM replay is independently useful.  Only opt into the
            // conventional workspace sidecar when the recorder produced
            // one; an explicit --native-capsule-replay remains strict.
            (options.mode == Some(tape::Mode::Replay))
                .then_some(derived_capsule.as_deref())
                .flatten()
                .filter(|path| Path::new(path).exists())
        }),
        ..options
    };
    if options.native_capsule_record.is_some()
        && (options.mode != Some(tape::Mode::Record)
            || config.concurrency != 1
            || options.repeat.is_some_and(|repeat| repeat != 1))
    {
        return Err(Error::Input(
            "--native-capsule-record requires --llm-record, concurrency = 1, and at most --repeat 1"
                .into(),
        ));
    }
    if options.native_capsule_replay.is_some()
        && (options.mode != Some(tape::Mode::Replay)
            || config.concurrency != 1
            || options.repeat.is_some_and(|repeat| repeat != 1))
    {
        return Err(Error::Input(
            "--native-capsule-replay requires --llm-replay, concurrency = 1, and at most --repeat 1"
                .into(),
        ));
    }
    let disk_policy = crate::planning::resolve(&config)?;
    let output_directory = config_path.parent().unwrap_or_else(|| Path::new("."));
    if (options.track.is_some() || options.replay_seed.is_some())
        && options.mode != Some(tape::Mode::Replay)
    {
        return Err(Error::Input("--track and --seed require --replay".into()));
    }
    if options.announce_trial {
        write_trial_start(output, &config)?;
    }
    let bundled_replay = if options.mode == Some(tape::Mode::Replay) {
        let source = replay_cassette(config_path, &config, options.cassette)?;
        crate::tape::require_replay_cassette(&source)?;
        crate::cassette::Source::is_bundle(&source)
            .map_err(|error| Error::Input(error.to_string()))?
    } else {
        false
    };
    // Reject an invalid replay request before asking about capacity for work
    // which cannot start.  Every executable run remains subject to the
    // conservative capacity preflight before any repetition or execution.
    crate::disk_space::confirm_capacity(
        output_directory,
        crate::disk_space::benchmark(
            &config,
            Some(&disk_policy),
            options.repeat.unwrap_or(1),
            options.mode,
        ),
        options.interactive && !options.json,
        options.json,
        output,
    )?;
    // The campaign journal is the only bounded publication/staging path for
    // auxiliary replay artifacts.  A singleton campaign is semantically one
    // direct execution, but gives MCP relay and native capsule modes their
    // sealed sidecars without retaining an additional direct-run format.
    let requires_artifact_campaign = options.mode == Some(tape::Mode::Record)
        || options.mcp_mode.is_some()
        || options.native_capsule_record.is_some()
        || options.native_capsule_replay.is_some();
    if let Some(repeat) = options.repeat.or((bundled_replay
        || options.track.is_some()
        || options.replay_seed.is_some()
        || requires_artifact_campaign)
        .then_some(1))
    {
        return repeated::run(config_path, config, repeat, options, output, warnings);
    }
    run_loaded(config_path, config, options, output, warnings)
}

fn automatic_native_capsule(
    agent: &str,
    mode: Option<tape::Mode>,
    explicit: bool,
    cassette: &Path,
) -> Option<String> {
    (!explicit && matches!(mode, Some(tape::Mode::Record | tape::Mode::Replay)))
        .then(|| {
            crate::agents::find(agent).filter(|adapter| adapter.supports_native_workspace_capsule())
        })
        .flatten()
        .map(|_| format!("{}.native-capsule", cassette.display()))
}

fn write_trial_start(output: &mut dyn Write, config: &BenchmarkConfig) -> Result<(), Error> {
    let trial = config.trial.as_deref().unwrap_or("default");
    let trial = trial
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    writeln!(output, "Starting trial: {trial}")
        .and_then(|()| output.flush())
        .map_err(io_error)
}

/// Allocate fresh, benchmark-scoped output for one direct (non-campaign) run.
///
/// Existing evidence is never reused or replaced. `create_dir` claims the next
/// numeric subdirectory atomically, so concurrent invocations also receive
/// separate workspaces, logs, and collector captures.
fn isolated_direct_config(
    config_path: &Path,
    mut config: BenchmarkConfig,
) -> Result<(BenchmarkConfig, PathBuf), Error> {
    let configured_workspace = resolve_log_path(config_path, &config.workspace);
    let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
    let root = parent
        .join("results")
        .join(format!("{}-runs", benchmark_stem(config_path)));
    std::fs::create_dir_all(&root).map_err(|error| {
        Error::Run(format!(
            "cannot create benchmark results root {}: {error}",
            root.display()
        ))
    })?;
    let result_directory = (1u32..=999_999)
        .find_map(|index| {
            let candidate = root.join(format!("run-{index:03}"));
            match std::fs::create_dir(&candidate) {
                Ok(()) => Some(Ok(candidate)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(Error::Run(format!(
                    "cannot allocate benchmark result directory {}: {error}",
                    candidate.display()
                )))),
            }
        })
        .ok_or_else(|| {
            Error::Run(format!(
                "no free benchmark result directory (run-001 through run-999999) under {}",
                root.display()
            ))
        })??;
    config.workspace = result_directory
        .join("workspace")
        .to_string_lossy()
        .into_owned();
    // A direct run owns a fresh result workspace, but an existing configured
    // workspace is its initial task material.  Copy it before workload
    // preparation adds missing built-in fixtures, keeping attempts isolated
    // without silently discarding the caller's checked-out source tree.
    if configured_workspace.is_dir() {
        for index in 1..=config.concurrency {
            let workspace = if config.concurrency == 1 {
                config.workspace.clone()
            } else {
                parallel_path(&config.workspace, index)
            };
            workloads::copy_tree(&configured_workspace, Path::new(&workspace))
                .map_err(Error::Run)?;
        }
    }
    config.log_file = result_directory
        .join("run.log")
        .to_string_lossy()
        .into_owned();
    if let Some(measurements) = config.measurements.as_mut().filter(|value| value.enabled) {
        measurements.directory = result_directory
            .join("measurements")
            .to_string_lossy()
            .into_owned();
    }
    Ok((config, result_directory))
}

fn run_loaded(
    config_path: &Path,
    mut config: BenchmarkConfig,
    options: RunOptions<'_>,
    output: &mut dyn Write,
    warnings: &Mutex<BTreeSet<String>>,
) -> Result<(), Error> {
    crate::planning::resolve_snapshot(&config, options.measurement_dir.is_some(), options.policy)?;
    if !options.replica.valid() {
        return Err(Error::Input("invalid benchmark replica identity".into()));
    }
    if let Some(reused) = options.reused {
        if !reused.is_empty()
            && (options.verify_initial_material || options.preparation_gate.is_some())
        {
            return Err(Error::Input(
                "fresh preparation cannot reuse historical replica slots".into(),
            ));
        }
        if reused
            .keys()
            .any(|index| !(1..=config.concurrency).contains(index))
        {
            return Err(Error::Input(
                "reused replica slots are outside the selected population".into(),
            ));
        }
        if config.concurrency == 1
            && let Some(summary) = reused.get(&1)
        {
            return reuse::render_reused(std::slice::from_ref(summary), options.json, output);
        }
    }
    let configured_log = public_direct_run(&options).then(|| config.log_file.clone());
    if configured_log.is_some() {
        (config, _) = isolated_direct_config(config_path, config)?;
    }
    let mut cohort = measurements::ExecutionCohort {
        id: workloads::TaskIdentity::local("execution-cohort", None).attempt_id,
        selected_replicas: config.concurrency,
        admitted_slots: (1..=config.concurrency)
            .filter(|slot| !options.reused.is_some_and(|saved| saved.contains_key(slot)))
            .collect(),
        dispatch: None,
    };
    if let Some(index) = cohort.admitted_slots.first() {
        cohort.dispatch = Some(cohort.dispatch_for(*index, config.concurrency)?);
    }
    let options = RunOptions {
        execution_cohort: Some(&cohort),
        ..options
    };
    let generated_log = config.log_file.clone();
    let concurrency = config.concurrency;
    let result = run_dispatched(config_path, config, options, output, warnings);
    if let Some(configured_log) = configured_log {
        publish_configured_logs(config_path, &configured_log, &generated_log, concurrency)?;
    }
    result
}

/// Keep the explicit configured log as a user-facing append-only view while
/// the canonical attempt evidence stays under its fresh result directory.
fn publish_configured_logs(
    config_path: &Path,
    configured: &str,
    generated: &str,
    concurrency: usize,
) -> Result<(), Error> {
    use std::io::Write;

    for index in 1..=concurrency {
        let source = if concurrency == 1 {
            PathBuf::from(generated)
        } else {
            PathBuf::from(parallel_path(generated, index))
        };
        let destination = if concurrency == 1 {
            resolve_log_path(config_path, configured)
        } else {
            resolve_log_path(config_path, &parallel_path(configured, index))
        };
        let bytes = match std::fs::read(&source) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(Error::Io(error.to_string())),
        };
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| Error::Io(error.to_string()))?;
        }
        let mut output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(destination)
            .map_err(|error| Error::Io(error.to_string()))?;
        output
            .write_all(&bytes)
            .and_then(|()| output.sync_all())
            .map_err(|error| Error::Io(error.to_string()))?;
    }
    Ok(())
}

/// Only the public one-shot entrypoint allocates a new result generation.
/// Internal campaign, preparation, reuse, and completion paths already own
/// their isolated directories and must remain side-effect free here.
fn public_direct_run(options: &RunOptions<'_>) -> bool {
    !options.verify_initial_material
        && options.reused.is_none()
        && options.execution_cohort.is_none()
        && options.preparation_gate.is_none()
        && options.launch_gate.is_none()
        && options.batch.is_none()
        && options.on_completion.is_none()
}

/// Consume the workflow's selected runtime admission, rather than replacing its
/// cohort or manufacturing gate arrivals for retained peers. Only the workflow
/// owner, not CLI/deserialized options, can supply this internal runtime object.
fn run_admitted_batch(
    config_path: &Path,
    config: BenchmarkConfig,
    options: RunOptions<'_>,
    admission: &batch::Admission,
    output: &mut dyn Write,
    warnings: &Mutex<BTreeSet<String>>,
) -> Result<(), Error> {
    crate::planning::resolve_snapshot(&config, options.measurement_dir.is_some(), options.policy)?;
    if options.replica != replica::Identity::default()
        || !admission.matches(config.concurrency, options.reused)
        || options.execution_cohort.is_some()
        || options.preparation_gate.is_some()
        || options.launch_gate.is_some()
    {
        return Err(Error::Input(
            "batch admission differs from original pending replica population".into(),
        ));
    }
    admission.claim()?;
    let options = RunOptions {
        execution_cohort: Some(admission.cohort()),
        preparation_gate: Some(admission.material()),
        launch_gate: Some(admission.launch()),
        verify_initial_material: true,
        ..options
    };
    run_dispatched(config_path, config, options, output, warnings)
}

fn run_dispatched(
    config_path: &Path,
    config: BenchmarkConfig,
    options: RunOptions<'_>,
    output: &mut dyn Write,
    warnings: &Mutex<BTreeSet<String>>,
) -> Result<(), Error> {
    // Parallel sweep configurations can share project-local tool paths.
    // Serialize readiness publication, not benchmark execution.
    static RUNTIME_PREPARATION: Mutex<()> = Mutex::new(());
    if !options.requirements_prepared {
        let _preparation = RUNTIME_PREPARATION
            .lock()
            .map_err(|_| Error::Run("shared runtime preparation failed; retry the sweep".into()))?;
        let mut messages = Vec::new();
        crate::install::ensure_execution_requirements(&config, config_path, &mut messages)?;
        if !options.json {
            output.write_all(&messages).map_err(io_error)?;
        }
    }
    let gate = (options.verify_initial_material && options.preparation_gate.is_none())
        .then(|| batch::Gate::new(config.concurrency, None, Duration::from_secs(1800)))
        .transpose()?;
    let options = RunOptions {
        preparation_gate: options.preparation_gate.or(gate.as_ref()),
        ..options
    };
    if config.concurrency > 1 {
        // Concurrent replicas buffer ordinary output for deterministic ordered
        // presentation. A shared one-line display would race those buffers.
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
    let mut completed = Vec::with_capacity(config.concurrency);
    for index in 1..=config.concurrency {
        if let Some(summary) = options.reused.and_then(|reused| reused.get(&index)) {
            let mut buffer = Vec::new();
            let result =
                reuse::render_reused(std::slice::from_ref(summary), options.json, &mut buffer);
            completed.push((index - 1, result, buffer));
            continue;
        }
        let replica = replica::configuration(
            config,
            replica::Identity {
                index,
                total: config.concurrency,
            },
        )?;
        let cassette = tape_request.map(|(mode, requested)| {
            let base = requested
                .map(PathBuf::from)
                .unwrap_or_else(|| default_cassette(config_path, config));
            (mode, replica_cassette(mode, &base, index))
        });
        let measurement = options
            .measurement_dir
            .map(|path| PathBuf::from(parallel_path(path, index)));
        jobs.push((index - 1, replica, measurement, cassette));
    }

    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for (index, replica, measurement, cassette) in jobs {
            #[cfg(test)]
            let test_rendezvous = REPLICA_TEST_RENDEZVOUS.with(|value| value.borrow().clone());
            let spawn = if injected_replica_spawn_failure(index) {
                #[cfg(test)]
                if let Some((ready, changed)) = test_rendezvous.as_deref() {
                    let ready = ready.lock().unwrap();
                    let (ready, _) = changed
                        .wait_timeout_while(ready, Duration::from_secs(2), |ready| !*ready)
                        .unwrap();
                    assert!(*ready, "replica did not reach test rendezvous");
                }
                Err(std::io::Error::other("injected replica spawn failure"))
            } else {
                std::thread::Builder::new().spawn_scoped(scope, move || {
                    run_replica_catching(index, options, |buffer| {
                        #[cfg(test)]
                        if let Some((ready, changed)) = test_rendezvous.as_deref() {
                            let gate = options.preparation_gate.expect("test requires gate");
                            let mut arrival = gate.register(replica::Identity {
                                index: index + 1,
                                total: config.concurrency,
                            })?;
                            buffer.extend_from_slice(b"replica output before cancellation\n");
                            *ready.lock().unwrap() = true;
                            changed.notify_one();
                            return arrival.ready(crate::cassette::Compatibility {
                                agent: "agent".into(),
                                provider: "provider".into(),
                                model: "model".into(),
                                workload: "workload".into(),
                                task: "task".into(),
                                dataset_sha256: None,
                                prompt_sha256: "a".repeat(64),
                                material_sha256: "b".repeat(64),
                                protocol_sha256: None,
                            });
                        }
                        let mode = cassette.as_ref().map(|(mode, _)| *mode);
                        let cassette_path =
                            cassette.as_ref().map(|(_, path)| path.to_string_lossy());
                        let cassette_path = cassette_path.as_deref();
                        let measurement_path = measurement.as_deref().and_then(Path::to_str);
                        run_mode(
                            config_path,
                            replica,
                            RunOptions {
                                replica: replica::Identity {
                                    index: index + 1,
                                    total: config.concurrency,
                                },
                                measurement_dir: measurement_path,
                                mode,
                                cassette: cassette_path,
                                interactive: false,
                                ..options
                            },
                            buffer,
                            warnings,
                        )
                    })
                })
            };
            match spawn {
                Ok(handle) => handles.push((index, handle)),
                Err(error) => {
                    let reason = format!("cannot start benchmark replica {}: {error}", index + 1);
                    abort_replica_gates(options, &reason);
                    completed.push((index, Err(Error::Run(reason)), Vec::new()));
                    break;
                }
            }
        }
        for (index, handle) in handles {
            completed.push(handle.join().unwrap_or_else(|_| {
                (
                    index,
                    Err(Error::Run("benchmark replica worker panicked".into())),
                    Vec::new(),
                )
            }));
        }
    });

    completed.sort_by_key(|(index, _, _)| *index);
    let mut failures: Vec<(String, Vec<usize>)> = Vec::new();
    let mut presentation = Ok(());
    for (index, result, buffer) in completed {
        if presentation.is_ok() {
            presentation = output.write_all(&buffer).map_err(io_error);
        }
        if let Err(error) = result {
            add_replica_failure(&mut failures, index + 1, error.to_string());
        }
    }
    let primary = if failures.is_empty() {
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
    };
    diagnostics::preserve_primary(primary, presentation)
}

#[cfg(test)]
type ReplicaTestRendezvous = std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>;

#[cfg(test)]
thread_local! {
    static REPLICA_SPAWN_FAILURE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static REPLICA_TEST_RENDEZVOUS: std::cell::RefCell<Option<ReplicaTestRendezvous>> = const { std::cell::RefCell::new(None) };
}

fn injected_replica_spawn_failure(index: usize) -> bool {
    #[cfg(test)]
    {
        REPLICA_SPAWN_FAILURE.with(|failure| failure.get() == Some(index))
    }
    #[cfg(not(test))]
    {
        let _ = index;
        false
    }
}

fn abort_replica_gates(options: RunOptions<'_>, reason: &str) {
    if let Some(gate) = options.preparation_gate {
        gate.abort(reason);
    }
    if let Some(gate) = options.launch_gate {
        gate.abort(reason);
    }
}

fn run_replica_catching(
    index: usize,
    options: RunOptions<'_>,
    execute: impl FnOnce(&mut Vec<u8>) -> Result<(), Error>,
) -> (usize, Result<(), Error>, Vec<u8>) {
    let mut buffer = Vec::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(&mut buffer)))
        .unwrap_or_else(|_| {
            let reason = format!("benchmark replica {} worker panicked", index + 1);
            abort_replica_gates(options, &reason);
            Err(Error::Run(reason))
        });
    (index, result, buffer)
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
        (1..=crate::benchmark::MAX_CONCURRENCY)
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
    crate::benchmark_paths::replica_path(path, index)
}

fn run_mode(
    config_path: &Path,
    config: BenchmarkConfig,
    options: RunOptions<'_>,
    output: &mut dyn Write,
    warnings: &Mutex<BTreeSet<String>>,
) -> Result<(), Error> {
    if options.mode == Some(tape::Mode::Record) && options.provider_capture_path.is_none() {
        return Err(Error::Run(
            "record dispatch lost its owned provider capture path before attempt admission".into(),
        ));
    }
    // Register before any fallible setup. Early return or unwinding aborts peers.
    let mut arrival = options
        .preparation_gate
        .map(|gate| gate.register(options.replica))
        .transpose()?;
    let policy = crate::planning::resolve_snapshot(
        &config,
        options.measurement_dir.is_some(),
        options.policy,
    )?;
    let admitted = AdmittedAttempt::admit(config_path, config, options)?;
    attempt::ConfiguredAttempt::configure(config_path, admitted, policy, output, warnings)?.run(
        &mut arrival,
        output,
        warnings,
    )
}

fn finish_provider_attempt(
    attempt: Result<(), Error>,
    arrival: &mut Option<batch::Arrival<'_>>,
    bridge: &mut Option<Box<dyn providers::ProviderSession>>,
) -> Result<(), Error> {
    // Abort unsettled material admission before potentially slow provider
    // teardown. Launch guards inside the attempt have already notified peers.
    if let Err(error) = &attempt
        && let Some(arrival) = arrival.as_mut()
    {
        arrival.abort(&error.to_string());
    }
    providers::finish_result(attempt, bridge)
}

/// Prevent replayed agents from contacting any live provider, updater, or
/// telemetry endpoint while preserving access to the local tape proxy.
///
/// Replay is intentionally fail-closed at the child boundary.  A refused
/// loopback proxy is preferable to an inherited corporate proxy or a direct
/// outbound connection; `NO_PROXY` keeps the ASB-owned loopback endpoint
/// reachable.  Node/Bun clients require the explicit opt-in to honor these
/// standard proxy variables.
fn apply_replay_network_guard(command: &mut Command) {
    const DENY_PROXY: &str = "http://127.0.0.1:9";
    const LOOPBACK: &str = "127.0.0.1,localhost,::1";
    for variable in [
        "http_proxy",
        "HTTP_PROXY",
        "https_proxy",
        "HTTPS_PROXY",
        "all_proxy",
        "ALL_PROXY",
    ] {
        command.env(variable, DENY_PROXY);
    }
    command
        .env("no_proxy", LOOPBACK)
        .env("NO_PROXY", LOOPBACK)
        .env("NODE_USE_ENV_PROXY", "1");
}

fn abort_prepared(
    error: Error,
    arrival: &mut batch::Arrival<'_>,
    session: &mut dyn workloads::WorkloadSession,
) -> Error {
    // Wake siblings before remote preservation/cleanup, which can take minutes.
    arrival.abort(&error.to_string());
    match session.cancel_prepared() {
        Ok(()) => error,
        Err(cleanup) => preparation_cancellation_error(error, cleanup),
    }
}

fn preparation_cancellation_error(primary: Error, cleanup: String) -> Error {
    let append = |reason| format!("{reason}; preparation cancellation failed: {cleanup}");
    match primary {
        Error::Input(reason) => Error::Input(append(reason)),
        Error::Io(reason) => Error::Io(append(reason)),
        Error::Run(reason) => Error::Run(append(reason)),
        Error::Usage(reason) => Error::Run(append(reason.into())),
    }
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
#[cfg(test)]
fn run_measured_agent(
    command: &mut Command,
    target: &crate::launch::TargetLaunch,
    route: Option<crate::launch::CommandRoute>,
    session: Option<&mut measurements::Session>,
    show_output: bool,
    output: &mut dyn Write,
) -> Result<MeasuredOutput, Error> {
    run_measured_agent_with_launch(
        command,
        target,
        route,
        session,
        show_output,
        output,
        NativeLaunch {
            readiness: None,
            runtime: MAX_AGENT_RUNTIME,
        },
    )
}

struct NativeLaunch<'a> {
    readiness: Option<&'a dyn workloads::session::LaunchReadiness>,
    runtime: Duration,
}

fn native_launch_failure(
    child: &mut OwnedChild,
    launch: Option<&dyn workloads::session::LaunchReadiness>,
    primary: impl std::fmt::Display,
) -> Error {
    // Release waiting siblings before potentially slow bounded child cleanup.
    if let Some(launch) = launch {
        launch.abort();
    }
    Error::Run(child.failure(primary))
}

#[cfg(test)]
fn run_measured_agent_with_launch(
    command: &mut Command,
    target: &crate::launch::TargetLaunch,
    route: Option<crate::launch::CommandRoute>,
    session: Option<&mut measurements::Session>,
    show_output: bool,
    output: &mut dyn Write,
    control: NativeLaunch<'_>,
) -> Result<MeasuredOutput, Error> {
    run_measured_agent_with_launch_progress(
        command,
        target,
        route,
        session,
        show_output,
        output,
        progress::Context::default(),
        None,
        control,
    )
}

// Progress is deliberately a separate argument: it is presentation-only and
// must not become part of launch, measurement, or agent-custody state.
#[allow(clippy::too_many_arguments)]
fn run_measured_agent_with_launch_progress(
    command: &mut Command,
    target: &crate::launch::TargetLaunch,
    route: Option<crate::launch::CommandRoute>,
    mut session: Option<&mut measurements::Session>,
    show_output: bool,
    output: &mut dyn Write,
    progress: progress::Context,
    stdin_prompt: Option<&[u8]>,
    control: NativeLaunch<'_>,
) -> Result<MeasuredOutput, Error> {
    let interactive_progress = progress.enabled();
    progress::status(
        interactive_progress,
        "starting agent and attaching execution collectors",
    );
    let NativeLaunch {
        readiness: launch,
        runtime,
    } = control;
    let mut started = Instant::now();
    let gated = route
        .as_ref()
        .is_some_and(crate::launch::CommandRoute::is_gated);
    #[cfg(target_os = "linux")]
    let admission = if let Some(route) = route.filter(|route| route.is_gated()) {
        let validated = route
            .validate_gate(command)
            .map_err(|error| Error::Run(format!("cannot validate ASB gate launch: {error}")))?;
        Some(
            crate::process_gate::Admission::with_principal_observation(validated).map_err(
                |error| {
                    Error::Run(format!(
                        "cannot establish ASB process gate custody: {error}"
                    ))
                },
            )?,
        )
    } else {
        None
    };
    if let Some(session) = session.as_deref_mut() {
        session.agent_launch_prepared(target);
    }
    if launch.is_some() && (!gated || !cfg!(target_os = "linux")) {
        return Err(Error::Run(
            "shared launch requires a verifiable stopped native process gate".into(),
        ));
    }
    let mut child = {
        let mut emit = |event| {
            if let Some(session) = session.as_deref_mut() {
                session.event(event).map_err(Error::Run)?;
            }
            Ok(())
        };
        emit(LifecycleEvent::MeasurementsReady)?;
        emit(LifecycleEvent::AgentSpawn)?;
        if launch.is_none() {
            emit(LifecycleEvent::AgentExecuteBegin)?;
        }
        #[cfg(target_os = "linux")]
        let spawned = match admission {
            Some(admission) => admission.spawn().map(OwnedChild::Gated),
            None => OwnedChild::spawn_direct(command),
        };
        #[cfg(not(target_os = "linux"))]
        let spawned = OwnedChild::spawn_direct(command);
        match spawned {
            Ok(child) => child,
            Err(error) => {
                let mut primary = format!(
                    "could not start {}: {error}",
                    target.program.to_string_lossy()
                );
                for event in [
                    LifecycleEvent::AgentExecuteEnd,
                    LifecycleEvent::AgentSpawnFailed,
                ] {
                    if let Err(secondary) = emit(event) {
                        primary.push_str(&format!("; {secondary}"));
                    }
                }
                return Err(Error::Run(primary));
            }
        }
    };
    let _launch_failure = workloads::session::LaunchFailureGuard(launch);
    if let Some(prompt) = stdin_prompt {
        let Some(mut stdin) = child.take_stdin() else {
            return Err(native_launch_failure(
                &mut child,
                launch,
                "agent adapter requested stdin prompt delivery but stdin is unavailable",
            ));
        };
        if let Err(error) = stdin.write_all(prompt) {
            return Err(native_launch_failure(
                &mut child,
                launch,
                format!("could not deliver the agent prompt through stdin: {error}"),
            ));
        }
        // EOF is part of the adapter contract: clients must not wait for
        // inherited terminal input after receiving ASB's instruction document.
        drop(stdin);
    }
    if let Some(session) = session.as_deref_mut()
        && let Err(error) = session.event(LifecycleEvent::AgentStarted)
    {
        return Err(native_launch_failure(&mut child, launch, error));
    }
    #[cfg(unix)]
    if launch.is_some()
        && let Err(error) = child.wait_stopped()
    {
        return Err(native_launch_failure(
            &mut child,
            launch,
            format!("native launch target is not stopped: {error}"),
        ));
    }
    if let Some(session) = session.as_deref_mut()
        && let Err(error) = session.attach_pid(child.id())
    {
        let mut primary = error;
        for event in [
            LifecycleEvent::AgentExecuteEnd,
            LifecycleEvent::AgentAttachFailed,
        ] {
            if let Err(secondary) = session.event(event) {
                primary.push_str(&format!("; {secondary}"));
            }
        }
        return Err(native_launch_failure(&mut child, launch, primary));
    }
    if let Some(launch) = launch {
        if let Err(error) = launch.ready() {
            return Err(native_launch_failure(&mut child, Some(launch), error));
        }
        if let Some(session) = session.as_deref_mut()
            && let Err(error) = session.event(LifecycleEvent::AgentExecuteBegin)
        {
            return Err(native_launch_failure(&mut child, Some(launch), error));
        }
        started = Instant::now();
    }
    #[cfg(unix)]
    if gated && let Err(error) = child.resume() {
        return Err(native_launch_failure(
            &mut child,
            launch,
            format!("could not resume measured agent process: {error}"),
        ));
    }
    let mut execution_finished = None;
    let mut progress = progress.display();
    let result = capture::capture_with_output_streams_progress(
        child,
        show_output,
        output,
        None,
        runtime,
        PresentationPolicy::PreserveEvidence,
        progress
            .as_mut()
            .map(|display| display as &mut dyn capture::Progress),
        &mut || {
            execution_finished = Some(Instant::now());
            Ok(())
        },
    );
    progress::status(interactive_progress, "tearing down execution collectors");
    // Freeze execution before collector/proxy teardown, even if that teardown
    // fails. A completed client output is evidence for official grading.
    let elapsed_ms = execution_finished
        .unwrap_or_else(Instant::now)
        .duration_since(started)
        .as_millis();
    let mut issues = AttemptIssues::default();
    if let Some(session) = session {
        issues.event(Some(session), LifecycleEvent::AgentExecuteEnd);
        issues.event(
            Some(session),
            if result.is_ok() {
                LifecycleEvent::AgentExited
            } else {
                LifecycleEvent::AgentWaitFailed
            },
        );
    }
    match result {
        Ok(captured) => {
            let only_truncated = captured.only_transcript_truncated();
            issues.output_error = captured.output_error;
            // A full bounded presentation buffer is not a broken capture:
            // the process exited and both pipes were drained.  Keep its
            // truncation marker in the private capture result, but reserve
            // completion-blocking errors for a stream fault or drain timeout.
            if !only_truncated && let Some(error) = captured.capture_error {
                issues.capture_error = Some(error.clone());
                issues.run_error(error);
            }
            Ok(MeasuredOutput {
                observation: captured.observation,
                output: captured.output,
                elapsed_ms,
                issues,
            })
        }
        Err(error) => {
            let mut primary = format!(
                "could not wait for {}: {error}",
                target.program.to_string_lossy()
            );
            if let Some(secondary) = issues.collector_failure() {
                primary.push_str(&format!("; {secondary}"));
            }
            Err(Error::Run(primary))
        }
    }
}

#[cfg(unix)]
fn gate_target_available(program: impl AsRef<std::ffi::OsStr>) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let program = program.as_ref();
    let path = Path::new(program);
    if !path.is_file() {
        return !program.as_bytes().contains(&b'/');
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
            component(&config.agent),
            component(&config.workload),
            component(&config.model)
        ))
}

/// Explicit paths win. Preserve historical JSONL/base-or-replica routing when
/// present; otherwise discover the new default ZIP recording. Presence includes
/// invalid files/links, which must fail normally rather than silently fall back.
fn replay_cassette(
    config_path: &Path,
    config: &BenchmarkConfig,
    requested: Option<&str>,
) -> Result<PathBuf, Error> {
    if let Some(path) = requested {
        return Ok(PathBuf::from(path));
    }
    let legacy = default_cassette(config_path, config);
    for path in std::iter::once(legacy.clone()).chain(
        (1..=crate::benchmark::MAX_CONCURRENCY)
            .map(|replica| crate::benchmark_paths::replica_path(&legacy, replica)),
    ) {
        if crate::cassette::Source::is_present(&path)
            .map_err(|failure| Error::Input(failure.to_string()))?
        {
            return Ok(legacy);
        }
    }
    let bundle = default_bundle_cassette(config_path, config);
    Ok(
        if crate::cassette::Source::is_present(&bundle)
            .map_err(|failure| Error::Input(failure.to_string()))?
        {
            bundle
        } else {
            legacy
        },
    )
}

fn default_bundle_cassette(config_path: &Path, config: &BenchmarkConfig) -> PathBuf {
    default_cassette(config_path, config).with_extension("asb.zip")
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
    crate::evidence::append_json(&path, result).map_err(Error::Run)
}

pub(crate) fn resolve_log_path(config_path: &Path, configured_path: &str) -> PathBuf {
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
    let mut summary = serde_json::json!({
        "measurement_directory": result.measurement_directory,
        "planning": result.planning,
        "effective_budgets": result.effective_budgets,
        "execution_plan": result.execution_plan,
        "execution_plan_fingerprint": result.execution_plan_fingerprint,
        "module": result.module,
        "benchmark_outcome": result.benchmark_outcome,
        "replica_index": result.replica_index,
        "execution_cohort": result.execution_cohort,
        "schema_version": result.schema_version,
        "agent": result.agent,
        "workload": result.workload,
        "provider": result.provider,
        "model": result.model,
        "task": result.task,
        "process_success": result.process_success,
        "process_termination": result.observation.process_termination,
        "workload_success": result.workload_success,
        "workload_resource_disposition": result.workload_resource_disposition,
        "workload_artifacts": result.workload_artifacts,
        "workload_evaluation": result.workload_evaluation,
        "workload_baseline_passed": result.workload_baseline_passed,
        "workload_baseline": result.workload_baseline,
        "workload_failure": result.workload_failure,
        "adapter_smoke_success": result.adapter_smoke_success,
        "failure_kind": result.disposition.failure.as_ref().map(|f| f.kind),
        "failure_reason": result.disposition.failure.as_ref().map(|f| &f.reason),
        "success_rate": result.success_rate,
        "elapsed_ms": result.elapsed_ms,
        "steps": result.steps,
        "tool_calls_available": result.tool_calls_available,
        "tool_call_count": result.tool_call_count(),
        "token_usage": result.token_usage,
    });
    if let Some(execution) = &result.execution {
        summary["execution"] =
            serde_json::to_value(execution).map_err(|error| Error::Io(error.to_string()))?;
    }
    if let Some(diagnostic) = result.observation.process_termination_diagnostic {
        summary["process_termination_diagnostic"] =
            serde_json::to_value(diagnostic).map_err(|error| Error::Io(error.to_string()))?;
    }
    if let Some(recording) = &result.recording {
        summary["recording"] =
            serde_json::to_value(recording).map_err(|error| Error::Io(error.to_string()))?;
    }
    // Sweep output is JSONL: keep each run summary on exactly one line so
    // callers can stream and parse it without having to recover pretty JSON
    // objects from a mixed human transcript.
    serde_json::to_writer(&mut *output, &summary).map_err(|error| Error::Io(error.to_string()))?;
    writeln!(output).map_err(io_error)
}

fn render_human(result: &RunResult<'_>, output: &mut dyn Write) -> Result<(), Error> {
    writeln!(
        output,
        "success_rate={} elapsed_ms={} steps={} tool_calls={} input_tokens={} output_tokens={} cached_tokens={} total_tokens={} evaluation={}",
        result.success_rate.map_or_else(|| "unavailable".into(), |value| format!("{value:.3}")),
        result.elapsed_ms,
        format_token(result.steps),
        result
            .tool_call_count()
            .map_or_else(|| "unknown".into(), |count| count.to_string()),
        format_token(result.token_usage.input_tokens),
        format_token(result.token_usage.output_tokens),
        format_token(result.token_usage.cached_tokens),
        format_token(result.token_usage.total_tokens),
        result.workload_evaluation,
    )
    .map_err(io_error)?;
    Ok(())
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
    _stdout: &[u8],
    _stderr: &[u8],
    _final_response: Option<&str>,
) -> Option<String> {
    (!status.success()).then(|| {
        format!(
            "agent exited unsuccessfully ({status}); inspect the benchmark log for agent output"
        )
    })
}

// Direct and gated children retain explicit custody on every return path.
#[cfg(test)]
pub(crate) fn fixture_module_descriptor(
    name: &str,
    descriptor: Option<workloads::contracts::ModuleDescriptor>,
) -> Option<workloads::contracts::ModuleDescriptor> {
    phases::entry_test_support::module_descriptor(name, descriptor)
}

mod capture;
#[cfg(unix)]
pub(crate) use capture::transport::GuestTransport;
use capture::{OwnedChild, PresentationPolicy, capture_with_output_streams};

pub(crate) struct GuestProcessOutput {
    pub output: Output,
    pub transcript_truncated: bool,
}

#[cfg(test)]
fn wait_with_output(
    child: OwnedChild,
    show_output: bool,
    output: &mut dyn Write,
) -> std::io::Result<Output> {
    wait_with_output_streams(
        child,
        show_output,
        output,
        None,
        MAX_AGENT_RUNTIME,
        &mut || Ok(()),
    )
}

pub(crate) fn run_guest_process(
    command: &mut Command,
    stdin_prompt: Option<&[u8]>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    runtime: Duration,
    on_spawn: &mut dyn FnMut(u32) -> Result<(), Error>,
) -> Result<GuestProcessOutput, Error> {
    #[cfg(target_os = "linux")]
    let mut descendants = crate::process_gate::DescendantScope::new()
        .map_err(|error| Error::Run(format!("cannot retain guest agent descendants: {error}")))?;
    let mut child = OwnedChild::spawn_direct(command)
        .map_err(|error| Error::Run(format!("cannot start guest agent: {error}")))?;
    if let Some(prompt) = stdin_prompt {
        let Some(mut stdin) = child.take_stdin() else {
            return Err(Error::Run(
                "guest agent requested stdin prompt delivery but stdin is unavailable".into(),
            ));
        };
        stdin.write_all(prompt).map_err(crate::io_error)?;
    }
    on_spawn(child.id())?;
    let captured = capture_with_output_streams(
        child,
        true,
        stdout,
        Some(stderr),
        runtime,
        PresentationPolicy::CancelOnTransportFailure,
        &mut || {
            #[cfg(target_os = "linux")]
            return descendants.quiesce();
            #[cfg(not(target_os = "linux"))]
            Ok(())
        },
    )
    .map_err(|error| Error::Run(format!("cannot wait for guest agent: {error}")))?;
    if let Some(error) = &captured.capture_error
        && !captured.only_transcript_truncated()
    {
        return Err(Error::Run(format!("cannot wait for guest agent: {error}")));
    }
    Ok(GuestProcessOutput {
        output: captured.output,
        transcript_truncated: captured.transcript_truncated,
    })
}

#[cfg(test)]
fn wait_with_output_streams(
    child: OwnedChild,
    show_output: bool,
    output: &mut dyn Write,
    error_output: Option<&mut dyn Write>,
    runtime: Duration,
    after_exit: &mut dyn FnMut() -> std::io::Result<()>,
) -> std::io::Result<Output> {
    let captured = capture_with_output_streams(
        child,
        show_output,
        output,
        error_output,
        runtime,
        PresentationPolicy::CancelOnTransportFailure,
        after_exit,
    )?;
    if let Some(error) = captured.capture_error {
        return Err(std::io::Error::other(error));
    }
    Ok(captured.output)
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
    command_for_with_interface(config, provider_base_url, prompt, interface, None)
}

fn command_for_with_interface(
    config: &BenchmarkConfig,
    provider_base_url: Option<&str>,
    prompt: &str,
    interface: Option<crate::endpoint::Interface>,
    max_steps: Option<u64>,
) -> Result<CommandSpec, Error> {
    if let Some(adapter) = agents::find(&config.agent) {
        if max_steps.is_some() && !adapter.supports_max_steps() {
            return Err(Error::Input(format!(
                "agent `{}` does not support --max-steps; remove the flag or select an adapter with an enforced turn limit",
                config.agent
            )));
        }
        return Ok(adapter.command(&agents::CommandContext {
            executable: adapter.executable(),
            config,
            endpoint: provider_base_url,
            interface,
            prompt,
            max_steps,
            native_tool_mode: None,
            native_tool: None,
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
        prompt_delivery: agents::PromptDelivery::Argument,
    })
}

fn bounded(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_OUTPUT)]).into_owned()
}

#[cfg(test)]
#[path = "../tests/unit/runner_boundaries.rs"]
mod boundary_tests;

#[cfg(test)]
mod tests {
    use super::{
        RunDisposition, RunResult, RunStatus, command_for, isolated_direct_config, render_human,
        render_json, replica_cassette, resolve_log_path, run_with_tape, write_log,
        write_trial_start,
    };
    use crate::benchmark::BenchmarkConfig;
    use crate::measurements::TokenUsage;

    #[test]
    fn concurrent_run_reports_injected_replica_spawn_failure() {
        let config: BenchmarkConfig = toml::from_str(
            "agent='fixture'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'\nconcurrency=2\n",
        ).unwrap();
        let gate = super::batch::Gate::new(2, None, std::time::Duration::from_secs(30)).unwrap();
        let mut output = Vec::new();
        let warnings = std::sync::Mutex::new(std::collections::BTreeSet::new());
        super::REPLICA_SPAWN_FAILURE.with(|failure| failure.set(Some(1)));
        super::REPLICA_TEST_RENDEZVOUS.with(|value| {
            *value.borrow_mut() = Some(std::sync::Arc::new((
                std::sync::Mutex::new(false),
                std::sync::Condvar::new(),
            )))
        });
        let result = super::run_concurrent(
            std::path::Path::new("benchmark.toml"),
            &config,
            super::RunOptions {
                preparation_gate: Some(&gate),
                ..Default::default()
            },
            &mut output,
            &warnings,
        );
        super::REPLICA_SPAWN_FAILURE.with(|failure| failure.set(None));
        super::REPLICA_TEST_RENDEZVOUS.with(|value| *value.borrow_mut() = None);
        let message = result.unwrap_err().to_string();
        assert!(
            message.contains("cannot start benchmark replica 2"),
            "{message}"
        );
        assert!(String::from_utf8_lossy(&output).contains("replica output before cancellation"));
        assert!(
            gate.register(super::replica::Identity { index: 2, total: 2 })
                .is_err()
        );
    }

    #[test]
    fn tool_mode_selection_preserves_legacy_and_explicit_absence() {
        use super::RunOptions;
        use crate::tape::Mode;
        let legacy = RunOptions {
            mode: Some(Mode::Record),
            ..Default::default()
        };
        assert_eq!(legacy.effective_tool_mode(), Some(Mode::Record));
        let llm_only = RunOptions {
            mode: Some(Mode::Record),
            tool_mode_explicit: true,
            ..Default::default()
        };
        assert_eq!(llm_only.effective_tool_mode(), None);
        let tool_record = RunOptions {
            mode: Some(Mode::Record),
            tool_mode: Some(Mode::Record),
            tool_mode_explicit: true,
            ..Default::default()
        };
        assert_eq!(tool_record.effective_tool_mode(), Some(Mode::Record));
        let tool_replay = RunOptions {
            mode: Some(Mode::Replay),
            tool_mode: Some(Mode::Replay),
            tool_mode_explicit: true,
            ..Default::default()
        };
        assert_eq!(tool_replay.effective_tool_mode(), Some(Mode::Replay));
    }

    #[test]
    fn trial_start_uses_the_selected_name_without_record_or_replay_metadata() {
        let mut config: BenchmarkConfig = toml::from_str(
            "agent='pi'\nworkload='bug-fix'\nprovider='local'\nmodel='fixture'\ntrial='case-42'\n",
        )
        .unwrap();
        let mut output = Vec::new();
        write_trial_start(&mut output, &config).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Starting trial: case-42\n"
        );

        config.trial = None;
        let mut output = Vec::new();
        write_trial_start(&mut output, &config).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Starting trial: default\n"
        );
    }

    #[test]
    fn direct_runs_allocate_fresh_benchmark_scoped_result_directories() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("direct-results", None).attempt_id);
        std::fs::create_dir_all(&root).expect("project");
        let config_path = root.join("opencode-deepseek-swe.toml");
        std::fs::write(&config_path, "fixture").expect("configuration");
        let config: BenchmarkConfig = toml::from_str(
            "agent='opencode'\nworkload='bug-fix'\nprovider='openrouter'\nmodel='deepseek-v4.1-flash'\nworkspace='workspace'\nlog_file='benchmark.log'\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=[]\n",
        )
        .expect("configuration parses");

        let (first, first_directory) =
            isolated_direct_config(&config_path, config.clone()).expect("first allocation");
        let (second, second_directory) =
            isolated_direct_config(&config_path, config).expect("second allocation");
        assert_eq!(
            first_directory,
            root.join("results/opencode-deepseek-swe-runs/run-001")
        );
        assert_eq!(
            second_directory,
            root.join("results/opencode-deepseek-swe-runs/run-002")
        );
        assert!(first_directory.is_dir());
        assert!(second_directory.is_dir());
        assert_eq!(
            first.workspace,
            first_directory.join("workspace").to_string_lossy()
        );
        assert_eq!(
            first.log_file,
            first_directory.join("run.log").to_string_lossy()
        );
        assert_eq!(
            first.measurements.unwrap().directory,
            first_directory.join("measurements").to_string_lossy()
        );
        assert_eq!(
            second.measurements.unwrap().directory,
            second_directory.join("measurements").to_string_lossy()
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn direct_parallel_replicas_each_receive_independent_configured_workspace() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("direct-seed", None).attempt_id);
        std::fs::create_dir_all(root.join("workspace/nested")).unwrap();
        std::fs::write(root.join("workspace/nested/source.txt"), "original").unwrap();
        let path = root.join("fixture.toml");
        let config: BenchmarkConfig = toml::from_str(
            "agent='pi'\nworkload='bug-fix'\nprovider='local'\nmodel='fixture'\nworkspace='workspace'\nconcurrency=2\n",
        ).unwrap();
        let (selected, _) = isolated_direct_config(&path, config).unwrap();
        let replica = |index| {
            super::replica::configuration(&selected, super::replica::Identity { index, total: 2 })
                .unwrap()
        };
        let first = std::path::PathBuf::from(replica(1).workspace).join("nested/source.txt");
        let second = std::path::PathBuf::from(replica(2).workspace).join("nested/source.txt");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "original");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "original");
        std::fs::write(&first, "changed").unwrap();
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "original");
        assert_eq!(
            std::fs::read_to_string(root.join("workspace/nested/source.txt")).unwrap(),
            "original"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn intent_publication_failure_stops_before_provider_workload_or_measurement_startup() {
        let _disk = crate::disk_space::test_available_mib(100_000);
        struct FailedIntent;
        impl super::replica::CompletionObserver for FailedIntent {
            fn starting(
                &self,
                _: super::replica::Identity,
                _: Option<&crate::measurements::ExecutionCohort>,
            ) -> Result<(), crate::Error> {
                Err(crate::Error::Io("fixture intent publication failed".into()))
            }
            fn completed(
                &self,
                _: super::replica::Identity,
                _: &std::path::Path,
            ) -> Result<(), crate::Error> {
                panic!("no attempt should have started")
            }
        }
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("failed-intent", None).attempt_id);
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("benchmark.toml");
        std::fs::write(
            &config,
            "agent='fixture'\nworkload='bug-fix'\nprovider='fixture'\nmodel='fixture'\n",
        )
        .unwrap();
        let mut output = Vec::new();
        let error = super::run_with_diagnostics(
            config.to_str().unwrap(),
            super::RunOptions {
                on_completion: Some(&FailedIntent),
                ..Default::default()
            },
            &mut output,
            &std::sync::Mutex::new(Default::default()),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("fixture intent publication failed")
        );
        assert!(output.is_empty());
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            1,
            "no workspace, measurements or project state may be created"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

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
        let target = crate::launch::TargetLaunch::direct(command);
        let _ = super::run_measured_agent(
            command,
            &target,
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
        let dir = std::env::var_os("ASB_TEST_PERFETTO_OUTPUT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| tools.parent().unwrap().join("trace-test-output"));
        capture_perfetto_fixture(&tools, &dir, false, None, false);
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires project-local Perfetto, isolated tracefs delegation and user namespaces"]
    fn live_perfetto_survivor_keeps_markers_and_scope_after_a_sibling_fails() {
        let tools = std::path::PathBuf::from(std::env::var_os("ASB_TEST_PERFETTO_TOOLS").unwrap());
        let root = tools
            .parent()
            .unwrap()
            .join(crate::workloads::TaskIdentity::local("perfetto-overlap", None).attempt_id);
        let barrier =
            std::sync::Arc::new((std::sync::Mutex::new(0usize), std::sync::Condvar::new()));
        let threads = [true, false].map(|fail| {
            let tools = tools.clone();
            let dir = root.join(if fail { "failed-sibling" } else { "survivor" });
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                capture_perfetto_fixture(&tools, &dir, fail, Some(&barrier), false)
            })
        });
        let outcomes = threads.map(|thread| thread.join());
        let [failed, survivor] = outcomes.map(Result::unwrap);
        assert!(
            failed.0 < survivor.1 && survivor.0 < failed.1,
            "captures did not overlap: {failed:?}, {survivor:?}"
        );
        assert!(
            failed.1 < survivor.1,
            "survivor did not remain active after failure: {failed:?}, {survivor:?}"
        );
        assert!(
            failed.3 < survivor.2,
            "failed sibling recorder had not stopped before survivor execution ended"
        );
        println!("Overlapping Perfetto qualification: {}", root.display());
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires project-local Perfetto, delegated tracefs and user namespaces"]
    fn live_perfetto_streaming_preserves_markers_beyond_the_service_ring_capacity() {
        let tools = std::path::PathBuf::from(std::env::var_os("ASB_TEST_PERFETTO_TOOLS").unwrap());
        let dir = tools
            .parent()
            .unwrap()
            .join(crate::workloads::TaskIdentity::local("perfetto-streaming", None).attempt_id);
        capture_perfetto_fixture(&tools, &dir, false, None, true);
        let output = std::process::Command::new(tools.join("perfetto/bin/trace_processor_shell"))
            .arg("query")
            .arg(dir.join("perfetto-scheduling.perfetto-trace"))
            .arg("SELECT SUM(value) FROM stats WHERE name='traced_buf_bytes_written'")
            .output()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        let written: u64 = text.lines().nth(1).unwrap().parse().unwrap();
        assert!(
            written > 128 * 1024 * 1024,
            "did not exceed the configured ring: {written}"
        );
        println!("Streaming trace bytes before compression: {written}");
    }

    #[cfg(target_os = "linux")]
    fn capture_perfetto_fixture(
        tools: &std::path::Path,
        dir: &std::path::Path,
        fail: bool,
        barrier: Option<&(std::sync::Mutex<usize>, std::sync::Condvar)>,
        pressure: bool,
    ) -> (u64, u64, std::time::Instant, std::time::Instant) {
        let mut session = crate::measurements::Session::start_with_collectors(
            dir,
            &["perfetto".into()],
            Some(tools),
        )
        .expect("start real Perfetto");
        let id = session.trace_id().to_owned();
        if pressure {
            // Fixed non-secret test data, paced below the owned kernel buffer's
            // drain capacity. Compression happens after the service ring, so
            // this exceeds the old ring without retaining an enormous file.
            let marker = format!(
                "I|{}|fixture.pressure.{}",
                std::process::id(),
                "x".repeat(1000)
            );
            for _ in 0..640 {
                for _ in 0..256 {
                    session.write_trace_marker(&marker).unwrap();
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        if let Some(barrier) = barrier {
            let mut ready = barrier.0.lock().unwrap();
            *ready += 1;
            barrier.1.notify_all();
            let (ready, _) = barrier
                .1
                .wait_timeout_while(ready, std::time::Duration::from_secs(15), |count| {
                    *count < 2
                })
                .unwrap();
            assert_eq!(
                *ready, 2,
                "sibling did not start within the qualification bound"
            );
        }
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", if fail { "sleep 0.2; exit 1" } else { "sleep 6" }])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let execution_started = std::time::Instant::now();
        let target = crate::launch::TargetLaunch::direct(&command);
        let output = super::run_measured_agent(
            &mut command,
            &target,
            None,
            Some(&mut session),
            false,
            &mut Vec::new(),
        )
        .expect("run fixture");
        let agent_ended = std::time::Instant::now();
        let elapsed_ms = execution_started.elapsed().as_millis();
        assert_eq!(output.status.success(), !fail);
        if fail {
            session
                .event(crate::collectors::LifecycleEvent::RunFailed)
                .unwrap();
        }
        session
            .event(crate::collectors::LifecycleEvent::RunFinishing)
            .unwrap();
        session
            .finish(crate::measurements::RunMeasurement {
                planning: None,
                effective_budgets: None,
                execution: None,
                recording: None,
                observation: Default::default(),
                workload_resource_disposition: None,
                workload_artifacts: &[],
                execution_plan_fingerprint: None,
                module: None,
                benchmark_outcome: None,
                replica_index: None,
                execution_cohort: None,
                schema_version: 1,
                run_id: "test".into(),
                status: if fail {
                    RunStatus::Failed
                } else {
                    RunStatus::Completed
                },
                success: !fail,
                process_success: !fail,
                workload_success: None,
                failure_kind: None,
                failure_reason: None,
                success_rate: None,
                adapter_smoke_success: None,
                agent: "fixture",
                workload: "fixture",
                task: None,
                workload_evaluation: "fixture",
                workload_baseline_passed: Some(false),
                workload_baseline: None,
                workload_failure: None,
                provider: "fixture",
                model: "fixture",
                elapsed_ms,
                exit_code: output.status.code(),
                token_usage: crate::measurements::TokenUsage::default(),
                steps: Some(1),
                tool_call_count: Some(0),
                tool_calls: &[],
                started_unix_ms: 0,
            })
            .expect("finish collector");
        let recorder_stopped = std::time::Instant::now();
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
        let markers: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("perfetto-tracepoints.json")).unwrap())
                .unwrap();
        assert_eq!(markers["marker_verified"], true);
        assert!(markers["marker_warning"].is_null(), "{markers}");
        assert!(markers["scope_warning"].is_null(), "{markers}");
        assert_eq!(markers["capture_complete"], true, "{markers}");
        assert!(markers["capture_warning"].is_null(), "{markers}");
        let data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("perfetto.json")).unwrap()).unwrap();
        assert!(
            data["metrics"]["perfetto.scheduler_events"]
                .as_f64()
                .unwrap_or(0.0)
                > 0.0,
            "{data}"
        );
        let bounds = std::process::Command::new(tools.join("perfetto/bin/trace_processor_shell"))
            .arg("query").arg(&trace)
            .arg(format!("SELECT MIN(ts), MAX(ts) FROM slice WHERE name IN ('asb.agent.started.{id}', 'asb.agent.exited.{id}')"))
            .output().unwrap();
        assert!(bounds.status.success());
        let text = String::from_utf8(bounds.stdout).unwrap();
        let values: Vec<u64> = text
            .lines()
            .nth(1)
            .unwrap()
            .split(',')
            .map(|s| s.parse().unwrap())
            .collect();
        assert_eq!(values.len(), 2);
        assert!(values[1] > values[0]);
        println!("Perfetto qualification: {}", dir.display());
        (values[0], values[1], agent_ended, recorder_stopped)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn child_is_killed_and_reaped_if_collector_attachment_aborts() {
        let child =
            super::OwnedChild::spawn_direct(std::process::Command::new("sleep").arg("10")).unwrap();
        let pid = child.id();
        let owned = child;
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
        let child = super::OwnedChild::spawn_direct(
            std::process::Command::new("sh")
                .args(["-c", "printf ready; exec sleep 10"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped()),
        )
        .unwrap();
        let pid = child.id();
        let error = super::wait_with_output(child, true, &mut FailingWriter).unwrap_err();
        assert!(error.to_string().contains("fixture output failure"));
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn guest_process_returns_a_bounded_transcript_after_output_overflow() {
        let mut command = std::process::Command::new("python3");
        command
            .args(["-c", "import os; os.write(1, b'x' * (9 * 1024 * 1024))"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let result = super::run_guest_process(
            &mut command,
            None,
            &mut stdout,
            &mut stderr,
            std::time::Duration::from_secs(10),
            &mut |_| Ok(()),
        )
        .unwrap();
        assert!(result.output.status.success());
        assert!(result.transcript_truncated);
        assert_eq!(result.output.stdout.len(), super::capture::CAPTURE_LIMIT);
        assert_eq!(stdout.len(), 9 * 1024 * 1024);
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
        std::fs::remove_file(&recorded_replica).unwrap();
        let last_replica = root.join("record-concurrency-64.jsonl");
        std::fs::write(&last_replica, "test").unwrap();
        assert_eq!(
            replica_cassette(crate::tape::Mode::Replay, &base, 1),
            last_replica
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_replay_without_explicit_cassette_fails_closed() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "asb-replay-no-cassette-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(root.join(".asb")).unwrap();
        std::fs::write(
            root.join(".asb/project.toml"),
            toml::to_string(&crate::project::ProjectConfig::default()).unwrap(),
        )
        .unwrap();
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
        assert!(
            !root.join(".asb/execution-plans").exists(),
            "an absent replay cassette must fail before replica plans are admitted"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_capsule_capture_rejects_repeat_or_concurrency_before_execution() {
        let root = std::env::temp_dir().join(format!(
            "asb-native-capsule-preflight-{}",
            crate::workloads::TaskIdentity::local("capsule-preflight", None).attempt_id
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(root.join(".asb")).unwrap();
        std::fs::write(
            root.join(".asb/project.toml"),
            toml::to_string(&crate::project::ProjectConfig::default()).unwrap(),
        )
        .unwrap();
        let config = root.join("benchmark.toml");
        std::fs::write(
            &config,
            "agent='pi'\nworkload='bug-fix'\nprovider='not-a-provider'\nmodel='model'\nconcurrency=2\n",
        )
        .unwrap();
        let capsule = root.join("capsule");
        let error = run_with_tape(
            config.to_str().unwrap(),
            super::RunOptions {
                mode: Some(crate::tape::Mode::Record),
                native_capsule_record: Some(capsule.to_str().unwrap()),
                ..Default::default()
            },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("native-capsule-record"));
        assert!(!capsule.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_capsule_replay_rejects_repeat_or_concurrency_before_execution() {
        let root = std::env::temp_dir().join(format!(
            "asb-native-capsule-replay-preflight-{}",
            crate::workloads::TaskIdentity::local("capsule-replay-preflight", None).attempt_id
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir(root.join(".asb")).unwrap();
        std::fs::write(
            root.join(".asb/project.toml"),
            toml::to_string(&crate::project::ProjectConfig::default()).unwrap(),
        )
        .unwrap();
        let config = root.join("benchmark.toml");
        std::fs::write(
            &config,
            "agent='pi'\nworkload='bug-fix'\nprovider='not-a-provider'\nmodel='model'\nconcurrency=2\n",
        )
        .unwrap();
        let capsule = root.join("capsule");
        let error = run_with_tape(
            config.to_str().unwrap(),
            super::RunOptions {
                mode: Some(crate::tape::Mode::Replay),
                native_capsule_replay: Some(capsule.to_str().unwrap()),
                ..Default::default()
            },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("native-capsule-replay"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn singleton_campaign_preserves_explicit_tool_replay_rejection() {
        let root = std::env::temp_dir().join(format!(
            "asb-explicit-tool-campaign-{}",
            crate::workloads::TaskIdentity::local("explicit-tool-campaign", None).attempt_id
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join(".asb-tools/codex/bin/codex");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, "#!/bin/sh\nexit 99\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::create_dir(root.join(".asb")).unwrap();
        std::fs::write(
            root.join(".asb/project.toml"),
            toml::to_string(&crate::project::ProjectConfig::default()).unwrap(),
        )
        .unwrap();
        let config = root.join("benchmark.toml");
        std::fs::write(
            &config,
            "agent='codex'\nworkload='bug-fix'\nprovider='not-a-provider'\nmodel='model'\nconcurrency=1\n",
        )
        .unwrap();
        let capsule = root.join("capsule");
        let error = run_with_tape(
            config.to_str().unwrap(),
            super::RunOptions {
                mode: Some(crate::tape::Mode::Record),
                tool_mode: Some(crate::tape::Mode::Record),
                tool_mode_explicit: true,
                native_capsule_record: Some(capsule.to_str().unwrap()),
                ..Default::default()
            },
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not yet provide an ASB tool interception transport")
        );
        assert!(!capsule.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completion_records_preserve_unresolved_outcomes_cleanup_and_private_evidence() {
        use super::completion;
        use crate::workloads;
        let root = std::env::temp_dir()
            .join(workloads::TaskIdentity::local("completion-record-test", None).attempt_id);
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("benchmark.toml");
        let mut result = result();
        result.stdout = "private agent transcript must not enter checkpoint".into();
        result.stderr = "private agent stderr must not enter checkpoint".into();
        let evidence = vec![root.join("official-evaluator-evidence")];
        for resolved in [true, false] {
            result.workload_success = Some(resolved);
            result.success_rate = Some(if resolved { 1.0 } else { 0.0 });
            completion::persist(
                &config,
                "run.log",
                &result,
                true,
                &evidence,
                &crate::proxy::Evidence {
                    captures: Vec::new(),
                    output_files: Vec::new(),
                    cleanup_confirmed: true,
                },
                &Default::default(),
            )
            .unwrap();
        }
        let directory = root.join(".asb-completions");
        let files: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 2, "outcomes must not overwrite one another");
        let mut outcomes = Vec::new();
        for file in files {
            assert_eq!(file.extension().unwrap(), "json");
            let bytes = std::fs::read(&file).unwrap();
            let text = String::from_utf8(bytes.clone()).unwrap();
            assert!(!text.contains("private agent"));
            let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(record["schema_version"], 2);
            assert_eq!(record["replica"], serde_json::json!({"index":1,"total":1}));
            assert_eq!(record["outputs"], serde_json::json!([root.join("run.log")]));
            assert_eq!(record["scope"], "task-outcome-not-yet-a-resume-receipt");
            assert_eq!(
                record["summary"]["task"]["attempt_id"],
                result.task.attempt_id
            );
            assert_eq!(
                record["workload_evidence"][0],
                evidence[0].to_str().unwrap()
            );
            let resolved = record["summary"]["workload_success"].as_bool().unwrap();
            assert_eq!(record["workload_cleanup_confirmed"], true);
            assert_eq!(record["workload_resource_disposition"], "not-required");
            assert_eq!(
                record["workload_resource_disposition"],
                record["summary"]["workload_resource_disposition"]
            );
            assert_eq!(record["proxy_cleanup_confirmed"], true);
            assert_eq!(record["proxy_evidence"], serde_json::json!([]));
            outcomes.push(resolved);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        outcomes.sort();
        assert_eq!(outcomes, [false, true]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            let other = root.join("other");
            std::fs::create_dir(&other).unwrap();
            std::os::unix::fs::symlink(&other, root.join("redirect")).unwrap();
            std::os::unix::fs::symlink(&other, other.join(".asb-completions")).unwrap();
            assert!(
                completion::persist(
                    &root.join("redirect/benchmark.toml"),
                    "run.log",
                    &result,
                    true,
                    &[],
                    &crate::proxy::Evidence {
                        captures: Vec::new(),
                        output_files: Vec::new(),
                        cleanup_confirmed: true
                    },
                    &Default::default(),
                )
                .is_err()
            );
            assert_eq!(std::fs::read_dir(&other).unwrap().count(), 1);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    fn result<'a>() -> RunResult<'a> {
        RunResult {
            observation: Default::default(),
            recording: None,
            execution: None,
            planning: crate::planning::resolve(
                &toml::from_str(
                    "agent='pi'\nprovider='fixture'\nmodel='fixture'\nworkload='bug-fix'",
                )
                .unwrap(),
            )
            .unwrap(),
            effective_budgets: Default::default(),
            execution_plan: None,
            execution_plan_fingerprint: None,
            module: None,
            benchmark_outcome: None,
            replica_index: None,
            execution_cohort: None,
            schema_version: 2,
            disposition: RunDisposition {
                status: RunStatus::Completed,
                failure: None,
                success: true,
            },
            agent: "codex",
            workload: "bug-fix",
            task: crate::workloads::TaskIdentity::local("bug-fix", None),
            provider: "openrouter",
            model: "test-model",
            process_success: true,
            workload_success: Some(true),
            workload_resource_disposition: Some(crate::workloads::ResourceDisposition::NotRequired),
            workload_artifacts: Vec::new(),
            workload_evaluation: "bundled-fixture",
            workload_baseline_passed: Some(false),
            workload_baseline: crate::workloads::BaselineEvidence::Measured {
                passed: Some(false),
            },
            workload_failure: None,
            success_rate: Some(1.0),
            adapter_smoke_success: None,
            elapsed_ms: 12,
            exit_code: Some(0),
            token_usage: TokenUsage {
                input_tokens: Some(3),
                output_tokens: Some(4),
                cached_tokens: Some(1),
                total_tokens: Some(7),
                source: Some("test".into()),
            },
            steps: Some(2),
            tool_calls: Vec::new(),
            tool_calls_available: true,
            adapter_tool_call_count: None,
            local_tool_transcript: None,
            deterministic_manifest: None,
            native_trace: None,
            native_tool_sidecar: None,
            sealed_artifacts: Vec::new(),
            cassette: None,
            measurement_directory: None,
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
    fn unavailable_tool_call_evidence_is_not_rendered_as_zero() {
        let mut result = result();
        result.tool_calls_available = false;
        let mut output = Vec::new();
        render_human(&result, &mut output).expect("human rendering succeeds");
        let text = String::from_utf8(output).expect("utf8");
        assert!(text.contains("tool_calls=unknown"));

        let mut json = Vec::new();
        render_json(&result, &mut json).expect("json rendering succeeds");
        let value: serde_json::Value = serde_json::from_slice(&json).expect("valid json");
        assert_eq!(value["tool_calls_available"], false);
        assert!(value["tool_call_count"].is_null());
        result.adapter_tool_call_count = Some(2);
        let mut json = Vec::new();
        render_json(&result, &mut json).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(value["tool_call_count"], 2);
        assert_eq!(value["tool_calls_available"], false);
        result.tool_calls_available = true;
        assert_eq!(result.tool_call_count(), Some(result.tool_calls.len()));
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
        let expected = result();
        assert_eq!(value["agent"], expected.agent);
        assert_eq!(value["workload"], expected.workload);
        assert_eq!(value["provider"], expected.provider);
        assert_eq!(value["model"], expected.model);
        assert!(value.get("stdout").is_none());
        assert!(value.get("execution").is_none());
        assert!(value.get("recording").is_none());
        assert_eq!(output.iter().filter(|byte| **byte == b'\n').count(), 1);
    }

    #[test]
    fn json_output_preserves_present_campaign_identity_like_the_saved_log() {
        let mut result = result();
        result.execution = Some(
            serde_json::from_value(serde_json::json!({
                "admission":crate::planning::BatchAdmissionPolicy::current(),
                "campaign":"fixture-campaign", "batch":2, "replica":1,
                "slot":3, "total":4, "replay_track":7, "replay_seed":42,
                "replay_algorithm":"fixture-selection"
            }))
            .unwrap(),
        );
        let saved = serde_json::to_value(&result).unwrap();
        let mut output = Vec::new();
        render_json(&result, &mut output).unwrap();
        let summary: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(summary.get("execution"), saved.get("execution"));
        assert!(summary["execution"].is_object());
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
            planning: None,
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
            retention: Default::default(),
            mcp_servers: Vec::new(),
        };
        let command = command_for(&config, Some("http://127.0.0.1:3456/v1"), "prompt")
            .expect("known agent has a command");
        assert_eq!(command.program, "openhands");
        assert_eq!(
            command.prompt_delivery,
            crate::agents::PromptDelivery::Embedded
        );
        assert!(command.args.contains(&"--json".into()));
        assert_eq!(command.args.last().map(String::as_str), Some("prompt"));
    }

    #[test]
    fn token_reporting_commands_request_machine_readable_agent_output() {
        let base = BenchmarkConfig {
            planning: None,
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
            retention: Default::default(),
            mcp_servers: Vec::new(),
        };
        let mini = command_for(&base, Some("http://127.0.0.1:3456/v1"), "prompt").unwrap();
        assert!(!mini.args.iter().any(|argument| argument == "--output"));
        let mut prepared = std::process::Command::new(&mini.program);
        prepared.args(&mini.args);
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            authentication: crate::endpoint::Authentication::Bearer,
            base_url: "http://127.0.0.1:1/v1".into(),
            credential: None,
        };
        let run = std::path::Path::new("fixture-private-run");
        crate::agents::find("mini-swe")
            .unwrap()
            .prepare(&mut crate::agents::PreparationContext {
                command: &mut prepared,
                model: &base.model,
                connection: &connection,
                workspace: std::path::Path::new("fixture-workspace"),
                runtime_dir: std::path::Path::new("fixture-private"),
                run_dir: run,
                mcp_servers: &[],
                mcp_tape: None,
                tape: None,
            })
            .unwrap();
        let args: Vec<_> = prepared.get_args().collect();
        assert_eq!(args[args.len() - 2], "--output");
        assert_eq!(
            std::path::Path::new(args[args.len() - 1]),
            run.join("mini-swe/trajectory.json")
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
                planning: None,
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
                retention: Default::default(),
                mcp_servers: Vec::new(),
            };
            let command = command_for(&config, Some("http://127.0.0.1:3456/v1"), "prompt")
                .expect("new agent command");
            assert_eq!(command.program, program, "{agent} program");
            assert!(
                command.args.iter().any(|value| value == marker),
                "{agent} marker"
            );
            assert_eq!(
                command.prompt_delivery,
                crate::agents::PromptDelivery::Embedded,
                "{agent} embeds prompt exactly once"
            );
        }
    }

    #[test]
    fn pi_cli2key_uses_compact_print_mode_with_adapter_summary() {
        let config = BenchmarkConfig {
            planning: None,
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
            retention: Default::default(),
            mcp_servers: Vec::new(),
        };
        let command = command_for(&config, Some("http://127.0.0.1:3456/v1"), "prompt")
            .expect("pi has a command");
        assert_eq!(command.program, "pi");
        assert_eq!(command.args[0], "--provider");
        assert_eq!(command.args[1], "asb");
        assert!(command.args.contains(&"--mode".into()));
        assert!(command.args.contains(&"text".into()));
        assert!(command.args.contains(&"--print".into()));
        assert_eq!(
            command.prompt_delivery,
            crate::agents::PromptDelivery::Argument
        );
    }

    #[test]
    fn provider_error_words_in_workload_output_do_not_imply_agent_failure() {
        let status = std::process::Command::new("true").status().unwrap();
        for output in [
            "Authentication required. Please use /login command to sign in to your account",
            "litellm.NotFoundError: Endpoint /v1/chat/completions was not found.",
            "litellm.InternalServerError: AnthropicException",
        ] {
            assert!(super::process_successful(&status, output.as_bytes(), b""));
        }
    }

    #[test]
    fn untrusted_refusal_words_do_not_imply_process_failure() {
        let status = std::process::Command::new("true").status().unwrap();
        for output in ["I can't run tests", "The workspace appears to be read-only"] {
            assert!(super::process_successful(&status, output.as_bytes(), b""));
        }
    }

    #[test]
    fn agent_failure_reasons_are_specific_without_exposing_transcript() {
        let failed_status = std::process::Command::new("false").status().unwrap();
        let reason = super::process_failure_reason(
            &failed_status,
            b"Authentication required. Please use /login command to sign in to your account secret-value",
            b"",
            None,
        ).unwrap();
        assert!(reason.contains("agent exited unsuccessfully"), "{reason}");
        assert!(reason.contains("exit status"), "{reason}");
        assert!(!reason.contains("secret-value"));
    }

    #[test]
    fn structured_agent_completion_is_not_failed_by_a_workload_test_phrase() {
        let status = std::process::Command::new("true").status().unwrap();
        assert!(
            super::process_failure_reason(
                &status,
                b"agent protocol completed",
                b"",
                Some("official workload test reported unexpected keyword argument")
            )
            .is_none()
        );
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
        assert!(super::process_successful_with_final_response(
            &status,
            b"Tool output",
            b"",
            Some("I cannot modify files in the workspace."),
        ));
        assert!(super::process_successful_with_final_response(
            &status,
            b"litellm.InternalServerError: upstream unavailable",
            b"",
            Some("Updated the implementation and verified the supplied test passes."),
        ));
        assert!(super::process_successful_with_final_response(
            &status,
            b"litellm.InternalServerError: upstream unavailable\nAgent finished",
            b"",
            None,
        ));
    }

    #[test]
    fn opencode_command_uses_generic_provider_model_namespace() {
        let config = BenchmarkConfig {
            planning: None,
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
            retention: Default::default(),
            mcp_servers: Vec::new(),
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
            planning: None,
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
            retention: Default::default(),
            mcp_servers: Vec::new(),
        };
        let error = match command_for(&config, None, "prompt") {
            Ok(_) => panic!("typo rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("did you mean `opencode`?"));
    }

    #[test]
    fn replay_network_guard_allows_only_loopback_proxy_targets() {
        let mut command = std::process::Command::new("agent");
        super::apply_replay_network_guard(&mut command);
        let values = command
            .get_envs()
            .filter_map(|(key, value)| {
                value.map(|value| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect::<std::collections::HashMap<_, _>>();
        for variable in [
            "http_proxy",
            "HTTP_PROXY",
            "https_proxy",
            "HTTPS_PROXY",
            "all_proxy",
            "ALL_PROXY",
        ] {
            assert_eq!(values[variable], "http://127.0.0.1:9");
        }
        assert_eq!(values["NO_PROXY"], "127.0.0.1,localhost,::1");
        assert_eq!(values["no_proxy"], "127.0.0.1,localhost,::1");
        assert_eq!(values["NODE_USE_ENV_PROXY"], "1");
    }

    #[test]
    fn native_workspace_adapters_derive_a_capsule_only_for_ordinary_tape_modes() {
        let cassette = std::path::Path::new("recording.asb.zip");
        assert_eq!(
            super::automatic_native_capsule(
                "hermes",
                Some(crate::tape::Mode::Record),
                false,
                cassette
            ),
            Some("recording.asb.zip.native-capsule".into())
        );
        assert_eq!(
            super::automatic_native_capsule(
                "hermes",
                Some(crate::tape::Mode::Replay),
                false,
                cassette
            ),
            Some("recording.asb.zip.native-capsule".into())
        );
        assert_eq!(
            super::automatic_native_capsule(
                "opencode",
                Some(crate::tape::Mode::Record),
                false,
                cassette
            ),
            None
        );
        assert_eq!(
            super::automatic_native_capsule(
                "hermes",
                Some(crate::tape::Mode::Record),
                true,
                cassette
            ),
            None
        );
    }
}
