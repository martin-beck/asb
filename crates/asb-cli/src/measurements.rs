//! Optional, file-backed measurements for a benchmark run.
//!
//! Measurement collection is deliberately opt-in.  The default run path does
//! not inspect the host or start any auxiliary process.  When enabled, every
//! result is written as bounded JSON so later processing never has to parse
//! terminal output.

use crate::collectors;
mod cohort;
mod producer;
mod snapshot;
pub(crate) use producer::command as producer_command;
pub(crate) use producer::{Completion as ProducerCompletion, Policy as ProducerPolicy, Producer};
use snapshot::ProcessTreeSnapshot;
pub(crate) use snapshot::SnapshotCommand;
#[cfg(test)]
mod coverage_tests;
pub use cohort::ExecutionCohort;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Child;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_CAPTURE: usize = 256 * 1024;
const MAX_RUNS: usize = 10_000;
static NEXT_TRACE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub source: Option<String>,
}

pub(crate) fn with_artifact_usage(
    mut usage: TokenUsage,
    artifact: Option<TokenUsage>,
) -> TokenUsage {
    if let Some(artifact) = artifact {
        let complete = artifact.input_tokens.is_some() && artifact.output_tokens.is_some();
        match usage.source.as_deref() {
            Some(source) if source.contains("cassette") => {}
            None | Some("agent-output") if complete => {
                usage = TokenUsage::default()
                    .merge_with_source(artifact, "agent-artifact")
                    .with_derived_total();
            }
            _ => {
                usage = usage
                    .merge_with_source(artifact, "agent-artifact")
                    .with_derived_total()
            }
        }
    }
    usage
}

/// Wall-clock and process-accounting measurements for one benchmark component.
///
/// Values that cannot be observed portably are left `null`; ASB never infers
/// network or I/O time from a residual wall-clock duration.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ComponentTiming {
    pub wall_ms: u128,
    pub cpu_user_ms: Option<f64>,
    pub cpu_system_ms: Option<f64>,
    pub cpu_accounting_source: Option<String>,
    pub io_wait_ms: Option<f64>,
    pub scheduler_wait_ms: Option<f64>,
    pub network_wait_ms: Option<f64>,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
struct ProcessSample {
    user_ticks: u64,
    system_ticks: u64,
    io_delay_ticks: u64,
    runqueue_ns: u64,
}

/// A PID is reusable; samplers bind it to one kernel process generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessIdentity {
    pub pid: u32,
    pub start_time_ticks: u64,
}

impl ProcessIdentity {
    pub(crate) fn read(pid: u32) -> Option<Self> {
        Self::read_at(Path::new("/proc"), pid)
    }

    fn read_at(proc: &Path, pid: u32) -> Option<Self> {
        Self::record_at(proc, pid).map(|(identity, _)| identity)
    }

    fn record_at(proc: &Path, pid: u32) -> Option<(Self, u32)> {
        let mut stat = String::new();
        File::open(proc.join(pid.to_string()).join("stat"))
            .ok()?
            .take(65537)
            .read_to_string(&mut stat)
            .ok()?;
        if stat.len() > 65536 || stat.split_whitespace().next()?.parse::<u32>().ok()? != pid {
            return None;
        }
        let fields = stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .collect::<Vec<_>>();
        if matches!(*fields.first()?, "Z" | "X" | "x") {
            return None;
        }
        let start_time_ticks = fields.get(19)?.parse().ok()?;
        let parent = fields.get(1)?.parse().ok()?;
        (start_time_ticks > 0).then_some((
            Self {
                pid,
                start_time_ticks,
            },
            parent,
        ))
    }

    pub(crate) fn matches(self) -> bool {
        self.matches_at(Path::new("/proc"))
    }

    fn matches_at(self, proc: &Path) -> bool {
        Self::read_at(proc, self.pid) == Some(self)
    }

    pub(crate) fn in_tree(self, root: Self) -> bool {
        self.in_tree_at(root, Path::new("/proc"))
    }

    fn in_tree_at(self, root: Self, proc: &Path) -> bool {
        if !root.matches_at(proc) {
            return false;
        }
        let mut current = self;
        let mut seen = BTreeSet::new();
        for _ in 0..512 {
            if !seen.insert(current.pid) {
                return false;
            }
            let Some((observed, parent)) = Self::record_at(proc, current.pid) else {
                return false;
            };
            if observed != current {
                return false;
            }
            if current == root {
                return self.matches_at(proc) && root.matches_at(proc);
            }
            let Some(identity) = Self::read_at(proc, parent) else {
                return false;
            };
            current = identity;
        }
        false
    }
}

#[derive(Clone, Copy, Default)]
struct ProcessAccounting {
    first: ProcessSample,
    latest: ProcessSample,
    initialized: bool,
}

impl ProcessAccounting {
    fn update(&mut self, sample: ProcessSample) {
        if !self.initialized {
            self.first = sample;
            self.initialized = true;
        }
        self.latest = sample;
    }

    fn delta(self) -> ProcessSample {
        ProcessSample {
            user_ticks: self.latest.user_ticks.saturating_sub(self.first.user_ticks),
            system_ticks: self
                .latest
                .system_ticks
                .saturating_sub(self.first.system_ticks),
            io_delay_ticks: self
                .latest
                .io_delay_ticks
                .saturating_sub(self.first.io_delay_ticks),
            runqueue_ns: self
                .latest
                .runqueue_ns
                .saturating_sub(self.first.runqueue_ns),
        }
    }
}

struct ProcessSampler {
    stop: std::sync::mpsc::Sender<()>,
    accounting: Arc<Mutex<ProcessAccounting>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

pub(crate) type TreeSnapshotArguments = Box<dyn Fn(&[u32]) -> SnapshotCommand + Send>;

impl ProcessSampler {
    fn start(root: ProcessIdentity, interval_ms: u64) -> Option<Self> {
        let initial = read_process_tree_sample(root)?;
        let (stop, receiver) = std::sync::mpsc::channel();
        let accounting = Arc::new(Mutex::new(ProcessAccounting {
            first: initial,
            latest: initial,
            initialized: true,
        }));
        let thread_accounting = Arc::clone(&accounting);
        let handle = std::thread::Builder::new()
            .name("asb-process-timing".into())
            .spawn(move || {
                loop {
                    let Some(sample) = read_process_tree_sample(root) else {
                        break;
                    };
                    if let Ok(mut state) = thread_accounting.lock() {
                        state.update(sample);
                    }
                    if receiver.recv_timeout(Duration::from_millis(interval_ms))
                        != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    {
                        break;
                    }
                }
                if let Some(sample) = read_process_tree_sample(root)
                    && let Ok(mut state) = thread_accounting.lock()
                {
                    state.update(sample);
                }
            })
            .ok()?;
        Some(Self {
            stop,
            accounting,
            handle: Some(handle),
        })
    }

    fn stop(mut self) -> Option<ProcessSample> {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.accounting.lock().ok().map(|state| state.delta())
    }
}

#[derive(Debug, Serialize)]
struct ComponentMeasurements {
    schema_version: u32,
    clock_ticks_per_second: Option<u64>,
    components: BTreeMap<String, ComponentTiming>,
}

/// Stable names used in generated configuration files and downstream tools.
pub fn available_collectors() -> Vec<String> {
    collectors::all()
        .iter()
        .map(|collector| collector.id().to_owned())
        .collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct RunMeasurement<'a> {
    /// Exact runtime identity inputs; independently usable without a run log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) planning: Option<&'a crate::planning::ResolvedPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effective_budgets: Option<crate::planning::EffectiveBudgets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) execution: Option<&'a crate::runner::campaign::ExecutionIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) recording: Option<&'a crate::runner::recording::Evidence>,
    #[serde(flatten)]
    pub(crate) observation: crate::process_termination::Observation,
    pub(crate) execution_plan_fingerprint: Option<&'a str>,
    pub(crate) module: Option<&'a crate::workloads::contracts::ModuleDescriptor>,
    pub(crate) benchmark_outcome: Option<&'a crate::workloads::outcome::BenchmarkOutcome>,
    pub replica_index: Option<usize>,
    pub execution_cohort: Option<&'a ExecutionCohort>,
    pub schema_version: u32,
    pub run_id: String,
    pub(crate) status: crate::runner::disposition::RunStatus,
    pub success: bool,
    pub process_success: bool,
    pub workload_success: Option<bool>,
    pub workload_resource_disposition: Option<crate::workloads::ResourceDisposition>,
    pub workload_artifacts: &'a [PathBuf],
    pub(crate) failure_kind: Option<crate::runner::disposition::FailureKind>,
    pub failure_reason: Option<&'a str>,
    pub success_rate: Option<f64>,
    pub adapter_smoke_success: Option<bool>,
    pub agent: &'a str,
    pub workload: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<&'a crate::workloads::TaskIdentity>,
    pub workload_evaluation: &'a str,
    pub workload_baseline_passed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workload_baseline: Option<&'a crate::workloads::BaselineEvidence>,
    pub workload_failure: Option<&'a crate::workloads::EvaluationFailure>,
    pub provider: &'a str,
    pub model: &'a str,
    pub elapsed_ms: u128,
    pub exit_code: Option<i32>,
    pub token_usage: TokenUsage,
    pub steps: Option<u64>,
    pub started_unix_ms: u128,
}

#[derive(Debug, Serialize)]
struct CollectorResult {
    #[serde(flatten)]
    observation: crate::process_termination::Observation,
    schema_version: u32,
    collector: String,
    status: String,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    elapsed_ms: u128,
    stdout: String,
    stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    artifact: Option<String>,
    /// Parsed, stable metrics. Raw stdout/stderr remain available for detail,
    /// but comparison must consume this bounded machine-readable map instead.
    metrics: BTreeMap<String, f64>,
    /// Bounded samples emitted by collectors that support interval output.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    series: BTreeMap<String, Vec<f64>>,
    /// Timestamp in milliseconds for each adapter's observed series sample.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    series_timestamps: BTreeMap<String, Vec<f64>>,
}

#[derive(Serialize)]
struct LifecycleRecord {
    sequence: usize,
    elapsed_us: u128,
    event: collectors::LifecycleEvent,
}

pub(crate) struct Collector {
    pub(crate) name: String,
    pub(crate) adapter: &'static dyn collectors::CollectorAdapter,
    pub(crate) producer: Producer,
    pub(crate) started: Instant,
    pub(crate) captures: [crate::evidence::Capture; 2],
}
impl Collector {
    pub(crate) fn spawn(
        name: &str,
        adapter: &'static dyn collectors::CollectorAdapter,
        command: Command,
        route: crate::launch::CommandRoute,
        captures: [crate::evidence::Capture; 2],
    ) -> Result<Self, String> {
        Self::spawn_with_dependency(name, adapter, command, route, captures, None)
    }
    pub(crate) fn spawn_with_dependency(
        name: &str,
        adapter: &'static dyn collectors::CollectorAdapter,
        mut command: Command,
        route: crate::launch::CommandRoute,
        captures: [crate::evidence::Capture; 2],
        dependency: Option<std::sync::Arc<dyn Send + Sync>>,
    ) -> Result<Self, String> {
        let files = [captures[0].writer()?, captures[1].writer()?];
        let producer = Producer::spawn_with_dependency(
            &mut command,
            route,
            adapter.producer_policy(name),
            files,
            dependency,
        )?;
        Ok(Self {
            name: name.into(),
            adapter,
            producer,
            started: Instant::now(),
            captures,
        })
    }
    pub(crate) fn startup_failure(mut self, reason: String) -> String {
        let completion = self.producer.stop();
        let raw = format!(
            "raw captures retained at {} and {}",
            self.captures[0].path().display(),
            self.captures[1].path().display()
        );
        match completion.error {
            Some(cleanup) => format!("{reason}; {cleanup}; {raw}"),
            None => format!("{reason}; {raw}"),
        }
    }
    /// Optional collectors can fail after their process and capture worker have
    /// been admitted but before they observe the measured target. Treat that
    /// as an unavailable collector, rather than discovering it only during
    /// finalization after a benchmark may already have been graded.
    pub(crate) fn startup_exit(&mut self) -> Result<Option<String>, String> {
        self.producer.poll().map(|completion| {
            completion.map(|completion| {
                format!(
                    "collector principal exited during startup: {:?}",
                    completion.exit
                )
            })
        })
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn fixture(
        name: &str,
        adapter: &'static dyn collectors::CollectorAdapter,
        captures: [crate::evidence::Capture; 2],
    ) -> Self {
        let mut launch = crate::launch::ProcessLaunch::new("python3");
        launch.arg("-I").arg("-S").arg("-u").arg("-c").arg("import signal,sys\nsignal.signal(signal.SIGINT,lambda *_: sys.exit(0))\nprint('ASB_FIXTURE_READY',flush=True)\nsignal.pause()\n");
        let (command, route) = producer_command(&mut launch).unwrap();
        let mut collector = Self::spawn(name, adapter, command, route, captures).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !collector.captures[0]
            .startup_diagnostic(4096)
            .unwrap()
            .contains("ASB_FIXTURE_READY")
        {
            assert!(collector.producer.poll().unwrap().is_none());
            assert!(
                Instant::now() < deadline,
                "owned fixture did not become ready"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        collector
    }
}

struct StoppedCollector {
    collector: Collector,
    completion: producer::Completion,
    elapsed_ms: u128,
}

/// A measurement session owns all optional collector processes for one run.
pub struct Session {
    adapters: Vec<&'static dyn collectors::CollectorAdapter>,
    namespace_only: bool,
    pub(crate) dir: PathBuf,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) tool_dir: Option<PathBuf>,
    pub(crate) cgroup_parent: Option<PathBuf>,
    started_unix_ms: u128,
    pub(crate) collectors: Vec<Collector>,
    stopped_collectors: Vec<StoppedCollector>,
    states: collectors::CollectorStates,
    shared_artifacts: BTreeMap<collectors::SharedArtifact, PathBuf>,
    run_started: bool,
    run_failed: bool,
    run_finishing: bool,
    lifecycle_started: Instant,
    lifecycle: Vec<LifecycleRecord>,
    #[cfg(test)]
    pub(crate) test_attach_failure: bool,
    #[cfg(test)]
    pub(crate) test_event_failure: Option<collectors::LifecycleEvent>,
    pub(crate) trace_id: String,
    sudo_collectors: BTreeSet<String>,
    component_starts: BTreeMap<String, Instant>,
    component_timings: BTreeMap<String, ComponentTiming>,
    process_sampler: Option<ProcessSampler>,
    process_identity: Option<ProcessIdentity>,
    incomplete_collectors: BTreeMap<String, String>,
    clock_ticks_per_second: Option<u64>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // An early runner error (for example, an incomplete replay cassette)
        // must not orphan process-scoped collectors. `finish` takes ownership
        // of the collector list, so this only handles paths that return before
        // normal finalization.
        if self.run_started && !self.run_finishing && !self.run_failed {
            let _ = self.event(collectors::LifecycleEvent::RunFailed);
        }
        // Each producer owns its single stop operation and fallback custody.
        // Dropping one failed producer cannot bypass its siblings.
        let deadline = Instant::now()
            + self
                .collectors
                .iter()
                .map(|collector| collector.producer.stop_budget())
                .max()
                .unwrap_or_default();
        for collector in &self.collectors {
            collector.producer.request_stop();
        }
        for collector in &mut self.collectors {
            let _ = collector.producer.stop_before(deadline);
        }
        self.collectors.clear();
        self.stop_process_sampler();
        if self.run_started && !self.run_finishing {
            let _ = write_json(&self.dir.join("lifecycle.json"), &self.lifecycle);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "../tests/unit/measurement_lifecycle_faults.rs"]
mod lifecycle_fault_tests;

#[cfg(all(test, target_os = "linux"))]
#[path = "../tests/unit/collector_custody.rs"]
mod collector_custody_tests;

impl Session {
    pub fn started_unix_ms(&self) -> u128 {
        self.started_unix_ms
    }
    /// Start a session and write the host description.  Collectors are enabled
    /// only by explicit ASB_MEASURE_* variables, keeping CI deterministic.
    /// Start with the stable collector names from a generated configuration.
    /// An empty list means environment-only opt-in, useful for tests and
    /// hand-written configurations.
    #[cfg(test)]
    pub fn start_with_collectors(
        dir: &Path,
        selected: &[String],
        tool_dir: Option<&Path>,
    ) -> Result<Self, String> {
        let project_root = crate::project::load(dir).ok().map(|(root, _)| root);
        Self::start_with_adapters(
            dir,
            selected,
            tool_dir,
            None,
            collectors::all().to_vec(),
            false,
            project_root.as_deref(),
        )
    }

    /// Use exactly the negotiated adapter set. Later environment changes cannot
    /// activate an adapter omitted from the saved plan.
    /// Project authority is explicit; output paths never select capabilities.
    pub(crate) fn start_with_resolved_collectors_and_parent(
        project_root: &Path,
        dir: &Path,
        selected: &[String],
        active: &[String],
        tool_dir: Option<&Path>,
        cgroup_parent: Option<&Path>,
    ) -> Result<Self, String> {
        let adapters = active
            .iter()
            .map(|name| {
                collectors::find(name).ok_or_else(|| format!("unknown resolved collector {name}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Self::start_with_adapters(
            dir,
            selected,
            tool_dir,
            cgroup_parent,
            adapters,
            false,
            Some(project_root),
        )
    }

    pub(crate) fn start_in_execution_namespace(
        dir: &Path,
        selected: &[String],
    ) -> Result<Self, String> {
        if selected.is_empty() || selected.len() > 32 {
            return Err("invalid execution-namespace collector selection".into());
        }
        let mut adapters = Vec::new();
        for name in selected {
            let adapter = collectors::find(name)
                .filter(|adapter| adapter.prefers_execution_namespace())
                .ok_or("collector cannot run in the execution namespace")?;
            if adapters
                .iter()
                .any(|existing: &&dyn collectors::CollectorAdapter| existing.id() == adapter.id())
            {
                return Err("duplicate execution-namespace collector selection".into());
            }
            adapters.push(adapter);
        }
        Self::start_with_adapters(dir, selected, None, None, adapters, true, None)
    }

    fn start_with_adapters(
        dir: &Path,
        selected: &[String],
        tool_dir: Option<&Path>,
        cgroup_parent: Option<&Path>,
        adapters: Vec<&'static dyn collectors::CollectorAdapter>,
        namespace_only: bool,
        project_root: Option<&Path>,
    ) -> Result<Self, String> {
        let selected = collectors::expand_dependencies(selected);
        let selected = selected.as_slice();
        fs::create_dir_all(dir).map_err(|e| {
            format!(
                "cannot create measurements directory {}: {e}; inspect access with `namei -l '{}'`",
                dir.display(),
                shell_quote(&dir.to_string_lossy())
            )
        })?;
        check_measurement_directory(dir)?;
        // Privilege and tracefs checks must complete before any agent process
        // is prepared. This prevents a partially measured benchmark run.
        let (sudo_collectors, states) =
            preflight_collectors(project_root, dir, selected, tool_dir, &adapters)?;
        let started_unix_ms = unix_ms();
        let mut session = Self {
            adapters,
            namespace_only,
            dir: dir.to_path_buf(),
            project_root: project_root.map(Path::to_path_buf),
            tool_dir: tool_dir.map(Path::to_path_buf),
            cgroup_parent: cgroup_parent.map(Path::to_path_buf),
            started_unix_ms,
            collectors: Vec::new(),
            stopped_collectors: Vec::new(),
            states,
            shared_artifacts: BTreeMap::new(),
            run_started: false,
            run_failed: false,
            run_finishing: false,
            lifecycle_started: Instant::now(),
            lifecycle: Vec::new(),
            #[cfg(test)]
            test_attach_failure: false,
            #[cfg(test)]
            test_event_failure: None,
            trace_id: format!(
                "{}-{}",
                std::process::id(),
                NEXT_TRACE_ID.fetch_add(1, Ordering::Relaxed)
            ),
            sudo_collectors,
            component_starts: BTreeMap::new(),
            component_timings: BTreeMap::new(),
            process_sampler: None,
            process_identity: None,
            incomplete_collectors: BTreeMap::new(),
            clock_ticks_per_second: clock_ticks_per_second(),
        };
        for collector in session.adapters.clone() {
            collector.configure(&mut session, selected)?;
        }
        for collector in session.adapters.clone() {
            collector.start(&mut session)?;
        }
        Ok(session)
    }

    /// Add a private fixture hook without a production registry/config surface.
    #[cfg(test)]
    pub(crate) fn add_test_adapter(&mut self, adapter: &'static dyn collectors::CollectorAdapter) {
        self.adapters.push(adapter);
    }

    /// Dispatch every hook and stop scoped capture even when a hook fails.
    pub(crate) fn event(&mut self, event: collectors::LifecycleEvent) -> Result<(), String> {
        let mut errors = Vec::new();
        #[cfg(test)]
        if self.test_event_failure == Some(event) {
            errors.push(format!("fixture collector event failure: {event:?}"));
        }
        let elapsed_us = self.lifecycle_started.elapsed().as_micros();
        // Timestamp the event before potentially slow collector shutdown or
        // processing, so their overhead cannot become agent execution time.
        self.record_component_event(event);
        for collector in self.adapters.clone() {
            if let Err(error) = collector.lifecycle_event(self, event) {
                errors.push(error);
            }
        }
        if matches!(
            event,
            collectors::LifecycleEvent::AgentExecuteEnd | collectors::LifecycleEvent::RunFailed
        ) {
            self.stop_process_sampler();
            if let Err(error) = self.stop_collector_processes(true) {
                errors.push(error);
            }
        }
        if self.lifecycle.len() < 256 {
            self.lifecycle.push(LifecycleRecord {
                sequence: self.lifecycle.len(),
                elapsed_us,
                event,
            });
        } else {
            errors.push("too many benchmark lifecycle events".into());
        }
        match event {
            collectors::LifecycleEvent::RunStarted => self.run_started = true,
            collectors::LifecycleEvent::RunFailed => self.run_failed = true,
            _ => {}
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn record_component_event(&mut self, event: collectors::LifecycleEvent) {
        let now = Instant::now();
        match event {
            collectors::LifecycleEvent::PhaseBegin(phase) => {
                self.component_starts.insert(phase.name().to_owned(), now);
            }
            collectors::LifecycleEvent::PhaseEnd(phase)
            | collectors::LifecycleEvent::PhaseFailed(phase) => {
                self.finish_component(phase.name(), now);
            }
            collectors::LifecycleEvent::AgentExecuteBegin => {
                self.component_starts.insert("agent.execute".into(), now);
            }
            collectors::LifecycleEvent::AgentExecuteEnd => {
                self.finish_component("agent.execute", now);
            }
            _ => {}
        }
    }

    fn finish_component(&mut self, name: &str, now: Instant) {
        let Some(started) = self.component_starts.remove(name) else {
            return;
        };
        self.component_timings
            .entry(name.to_owned())
            .or_default()
            .wall_ms = now.duration_since(started).as_millis();
    }

    fn start_process_sampler(&mut self, pid: u32) {
        if self.namespace_only {
            return;
        }
        if self.process_sampler.is_some() {
            return;
        }
        self.process_sampler = self
            .process_identity
            .or_else(|| ProcessIdentity::read(pid))
            .and_then(|root| ProcessSampler::start(root, measurement_interval_ms()));
    }

    fn stop_process_sampler(&mut self) {
        let Some(sampler) = self.process_sampler.take() else {
            return;
        };
        let accounting = sampler.stop();
        let Some(timing) = self.component_timings.get_mut("agent.execute") else {
            return;
        };
        let Some(accounting) = accounting else {
            return;
        };
        if let Some(hz) = self.clock_ticks_per_second {
            timing.cpu_user_ms = Some(accounting.user_ticks as f64 * 1_000.0 / hz as f64);
            timing.cpu_system_ms = Some(accounting.system_ticks as f64 * 1_000.0 / hz as f64);
            timing.cpu_accounting_source = Some("proc-sampling".into());
            timing.io_wait_ms = Some(accounting.io_delay_ticks as f64 * 1_000.0 / hz as f64);
        }
        timing.scheduler_wait_ms = Some(accounting.runqueue_ns as f64 / 1_000_000.0);
    }

    fn write_component_measurements(&self) -> Result<(), String> {
        // RunMeasurement.elapsed_ms is execution-only (guest-observed for
        // isolated workloads). The total must cover preparation, execution,
        // grading and cleanup on the same clock as the component events.
        // Freeze at RunFinishing so collector/report post-processing does not
        // inflate the measured lifecycle. Sessions without that event retain
        // an honest elapsed-session fallback.
        let total_wall_ms = self
            .lifecycle
            .iter()
            .rev()
            .find(|record| record.event == collectors::LifecycleEvent::RunFinishing)
            .map_or_else(
                || self.lifecycle_started.elapsed().as_millis(),
                |record| record.elapsed_us / 1_000,
            );
        let mut components = self.component_timings.clone();
        components.insert(
            "run.total".into(),
            ComponentTiming {
                wall_ms: total_wall_ms,
                ..ComponentTiming::default()
            },
        );
        write_json(
            &self.dir.join("components.json"),
            &ComponentMeasurements {
                schema_version: 1,
                clock_ticks_per_second: self.clock_ticks_per_second,
                components,
            },
        )
    }

    pub(crate) fn record_cpu_accounting(&mut self, user_usec: u64, system_usec: u64, source: &str) {
        let timing = self
            .component_timings
            .entry("agent.execute".into())
            .or_default();
        timing.cpu_user_ms = Some(user_usec as f64 / 1_000.0);
        timing.cpu_system_ms = Some(system_usec as f64 / 1_000.0);
        timing.cpu_accounting_source = Some(source.into());
    }

    /// Return process-tree sampler totals when cgroup delegation is absent.
    pub(crate) fn sampled_cpu_accounting(&self) -> Option<(f64, f64)> {
        let timing = self.component_timings.get("agent.execute")?;
        Some((timing.cpu_user_ms?, timing.cpu_system_ms?))
    }

    /// No observed CPU at sampling resolution is not proof that a short
    /// isolated execution consumed zero CPU. Keep missing totals missing.
    pub(crate) fn suppress_unobserved_sampled_cpu(&mut self) {
        if let Some(timing) = self.component_timings.get_mut("agent.execute")
            && timing.cpu_accounting_source.as_deref() == Some("proc-sampling")
            && timing.cpu_user_ms == Some(0.0)
            && timing.cpu_system_ms == Some(0.0)
        {
            timing.cpu_user_ms = None;
            timing.cpu_system_ms = None;
            timing.cpu_accounting_source = None;
        }
    }

    pub(crate) fn cgroup_parent(&self) -> Option<&Path> {
        self.cgroup_parent.as_deref()
    }

    pub(crate) fn trace_id(&self) -> &str {
        &self.trace_id
    }

    pub(crate) fn state<T: Default + 'static>(&mut self) -> Rc<RefCell<T>> {
        self.states.state::<T>()
    }

    pub(crate) fn set_artifact(&mut self, kind: collectors::SharedArtifact, path: PathBuf) {
        self.shared_artifacts.insert(kind, path);
    }

    pub(crate) fn artifact(&self, kind: collectors::SharedArtifact) -> Option<&Path> {
        self.shared_artifacts.get(&kind).map(PathBuf::as_path)
    }

    pub(crate) fn agent_launch_prepared(&mut self, target: &crate::launch::TargetLaunch) {
        for collector in self.adapters.clone() {
            collector.agent_launch_prepared(self, target);
        }
    }

    pub(crate) fn execution_namespace_collectors(&mut self) -> Vec<String> {
        self.adapters
            .clone()
            .into_iter()
            .filter(|adapter| {
                adapter.prefers_execution_namespace() && adapter.execution_namespace_requested(self)
            })
            .map(|adapter| adapter.id().into())
            .collect()
    }

    pub(crate) fn export_execution_namespace_state(
        &mut self,
    ) -> Result<BTreeMap<String, serde_json::Value>, String> {
        self.event(collectors::LifecycleEvent::AgentExecuteEnd)?;
        let mut result = BTreeMap::new();
        for adapter in self.adapters.clone() {
            if adapter.prefers_execution_namespace() && adapter.execution_namespace_requested(self)
            {
                result.insert(
                    adapter.id().into(),
                    adapter.export_execution_namespace_state(self)?,
                );
            }
        }
        Ok(result)
    }

    pub(crate) fn import_execution_namespace_state(
        &mut self,
        values: &BTreeMap<String, serde_json::Value>,
    ) -> Result<(), String> {
        for adapter in self.adapters.clone() {
            if adapter.prefers_execution_namespace()
                && adapter.execution_namespace_requested(self)
                && let Some(value) = values.get(adapter.id())
            {
                adapter.import_execution_namespace_state(self, value)?;
            }
        }
        Ok(())
    }

    pub(crate) fn measurement_target(&self) -> Option<ProcessIdentity> {
        self.process_identity
    }

    /// Attach collectors to the selected child after it has been spawned.
    pub fn attach_pid(&mut self, pid: u32) -> Result<(), String> {
        self.process_identity = ProcessIdentity::read(pid);
        #[cfg(test)]
        if self.test_attach_failure {
            return Err("injected collector attachment failure".into());
        }
        for collector in self.adapters.clone() {
            if collector.requires_pid() {
                collector.attach_pid(self, pid)?;
            }
        }
        self.start_process_sampler(pid);
        Ok(())
    }

    /// Attach only after the owning execution adapter proves host identity.
    pub(crate) fn attach_isolated_pid(
        &mut self,
        pid: u32,
        start_time_ticks: u64,
    ) -> Result<(), String> {
        let root = ProcessIdentity {
            pid,
            start_time_ticks,
        };
        if !root.matches() {
            return Err("isolated process identity changed before collector attachment".into());
        }
        self.process_identity = Some(root);
        for collector in self.adapters.clone() {
            if collector.requires_pid() {
                if !root.matches() {
                    return Err("isolated process exited during collector attachment".into());
                }
                collector.attach_isolated_pid(self, pid)?;
            }
        }
        self.start_process_sampler(pid);
        Ok(())
    }

    /// Finish collectors and write the run record and aggregate summary.
    pub fn finish<'a>(mut self, measurement: RunMeasurement<'a>) -> Result<(), String> {
        self.stop_process_sampler();
        let mut errors = Vec::new();
        let mut observe = |result: Result<(), String>| {
            if let Err(error) = result {
                errors.push(error);
            }
        };
        observe(self.stop_collector_processes(false));
        observe(self.process_stopped_collectors());
        observe(write_json(&self.dir.join("run.json"), &measurement));
        if self.run_started {
            observe(write_json(
                &self.dir.join("lifecycle.json"),
                &self.lifecycle,
            ));
        }
        for adapter in self.adapters.clone() {
            observe(adapter.finish(&mut self));
        }
        observe(self.write_component_measurements());
        observe(update_summary(&self.dir, &measurement));
        // Every explicit publication has been attempted. Drop must not silently
        // retry a failed/unconfirmed lifecycle commit or rewrite its evidence.
        self.run_finishing = true;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn stop_collector_processes(&mut self, execution_only: bool) -> Result<(), String> {
        let finished_at = Instant::now();
        let mut errors = Vec::new();
        let (collectors, remaining) = std::mem::take(&mut self.collectors)
            .into_iter()
            .partition::<Vec<_>, _>(|collector| {
                !execution_only || collector.adapter.stop_at_execution_end()
            });
        self.collectors = remaining;
        let deadline = Instant::now()
            + collectors
                .iter()
                .map(|collector| collector.producer.stop_budget())
                .max()
                .unwrap_or_default();
        for collector in &collectors {
            collector.producer.request_stop();
        }
        for mut collector in collectors {
            let mut completion = collector.producer.stop_before(deadline);
            if completion.capture_complete
                && !collector
                    .adapter
                    .accepts_termination(&collector.name, &completion)
            {
                completion.capture_complete = false;
                completion.error = Some(format!(
                    "collector principal terminated unexpectedly: {:?}",
                    completion.exit
                ));
            }
            if let Some(error) = &completion.error {
                errors.push(format!(
                    "cannot finish {} collector: {error}",
                    collector.name
                ));
                self.incomplete_collectors
                    .insert(collector.name.clone(), error.clone());
            }
            let elapsed_ms = finished_at.duration_since(collector.started).as_millis();
            self.stopped_collectors.push(StoppedCollector {
                collector,
                completion,
                elapsed_ms,
            });
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    // Reporting remains after workload evaluation: optional export errors
    // must never cancel a completed agent before the official grader runs.
    fn process_stopped_collectors(&mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        for StoppedCollector {
            mut collector,
            completion,
            elapsed_ms,
        } in std::mem::take(&mut self.stopped_collectors)
        {
            // Admission owns exact source handles, not post-exit pathnames.
            let captures = &mut collector.captures;
            let producer_incomplete = self.incomplete_collectors.contains_key(&collector.name)
                || !completion.capture_complete
                || completion.guardian_cleanup_confirmed == Some(false);
            let mut read_failed = producer_incomplete;
            // An unfinished worker can still own these exact writer handles.
            // Neither freeze nor parse that evidence as a stopped capture.
            if !producer_incomplete {
                for capture in captures.iter_mut() {
                    if let Err(error) = capture.freeze() {
                        read_failed = true;
                        errors.push(error);
                    }
                }
            }
            let mut diagnostic =
                |capture: &crate::evidence::Capture| match capture.diagnostic(MAX_CAPTURE) {
                    Ok(text) => text,
                    Err(error) => {
                        read_failed = true;
                        errors.push(error);
                        String::new()
                    }
                };
            let stdout = if producer_incomplete {
                String::new()
            } else {
                diagnostic(&captures[0])
            };
            let mut stderr = if producer_incomplete {
                String::new()
            } else {
                diagnostic(&captures[1])
            };
            if let Some(error) = self.incomplete_collectors.get(&collector.name) {
                stderr = bounded(format!("{error}\n{stderr}").as_bytes());
            }
            let context = collectors::ProcessContext::from_completion(
                &collector.name,
                &completion,
                [&stdout, &stderr],
                [&captures[0], &captures[1]],
            );
            let mut data = match if read_failed {
                Err("producer capture is incomplete".into())
            } else {
                collector.adapter.process(self, &context)
            } {
                Ok(data) => data,
                Err(error) => {
                    read_failed |= collector.adapter.retain_raw_on_processing_failure();
                    let mut data = collectors::ProcessData::from_output(&context);
                    data.status = "failed".into();
                    stderr = bounded(
                        format!("collector result processing failed: {error}\n{stderr}").as_bytes(),
                    );
                    data
                }
            };
            for capture in captures.iter() {
                if let Err(error) = capture.verify_stopped() {
                    read_failed = true;
                    errors.push(error);
                }
            }
            if read_failed {
                data.status = "failed".into();
                data.metrics.clear();
                data.series.clear();
                data.timestamps.clear();
            }
            let result = CollectorResult {
                observation: completion
                    .exit
                    .map(crate::process_termination::Observation::observed)
                    .unwrap_or_default(),
                schema_version: 1,
                collector: collector.name.clone(),
                status: data.status,
                exit_code: completion.exit.and_then(|status| status.code()),
                exit_signal: completion.exit.and_then(|status| {
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        status.signal()
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = status;
                        None
                    }
                }),
                elapsed_ms,
                stdout,
                stderr,
                artifact: data.artifact,
                metrics: data.metrics,
                series: data.series,
                series_timestamps: data.timestamps,
            };
            if let Err(error) =
                write_json(&self.dir.join(format!("{}.json", collector.name)), &result)
            {
                errors.push(format!("{error}; raw capture cleanup skipped; inspect {} and {}; do not repeat execution to repair reporting",captures[0].path().display(),captures[1].path().display()));
            } else if !read_failed {
                for capture in captures.iter() {
                    if let Err(error) = capture.consume() {
                        errors.push(error);
                    }
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    pub(crate) fn start_tree_snapshot(
        &mut self,
        name: &str,
        enabled: bool,
        executable: &Path,
        pid: u32,
        arguments: TreeSnapshotArguments,
    ) -> Result<(), String> {
        if !enabled {
            return Ok(());
        }
        let stdout_path = self.dir.join(format!("{name}.stdout"));
        let stderr_path = self.dir.join(format!("{name}.stderr"));
        let captures = [
            crate::evidence::Capture::create(&stdout_path)?,
            crate::evidence::Capture::create(&stderr_path)?,
        ];
        {
            let executable = executable.to_path_buf();
            let sampler = ProcessTreeSnapshot::start(
                self.process_identity
                    .or_else(|| ProcessIdentity::read(pid))
                    .ok_or("process-tree sampler target has exited")?,
                executable,
                arguments,
                captures[0].writer()?,
                captures[1].writer()?,
            )
            .ok_or_else(|| format!("cannot start {name} process-tree sampler"))?;
            self.collectors.push(Collector {
                name: name.into(),
                adapter: collectors::find(name).expect("registered collector"),
                producer: Producer::snapshot(sampler),
                started: Instant::now(),
                captures,
            });
            Ok(())
        }
    }

    #[cfg(test)]
    pub(crate) fn command(&self, name: &str) -> Command {
        let executable = self.executable(name);
        if self.sudo_collectors.contains(name) {
            let mut command = Command::new("sudo");
            command.args(["-n", "--"]).arg(executable);
            return command;
        }
        Command::new(executable)
    }

    pub(crate) fn collector_command(
        &self,
        name: &str,
    ) -> Result<(Command, crate::launch::CommandRoute), String> {
        let executable = self.executable(name);
        let mut launch = if self.sudo_collectors.contains(name) {
            let mut launch = crate::launch::ProcessLaunch::new("sudo");
            launch.arg("-n").arg("--").arg(executable);
            launch
        } else {
            crate::launch::ProcessLaunch::new(executable)
        };
        producer::command(&mut launch)
    }

    fn executable(&self, name: &str) -> PathBuf {
        if let Some(tool_dir) = &self.tool_dir {
            let binary = tool_binary_name(name);
            for local in [
                tool_dir.join(name).join("bin").join(binary),
                tool_dir.join("bin").join(binary),
            ] {
                if local.is_file() {
                    return local;
                }
            }
        }
        PathBuf::from(tool_binary_name(name))
    }
}

fn check_measurement_directory(dir: &Path) -> Result<(), String> {
    let path = dir.join(format!(
        ".asb-write-probe-{}-{}",
        std::process::id(),
        NEXT_TRACE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => {
            drop(file);
            fs::remove_file(&path).map_err(|error| {
                format!(
                    "cannot remove ASB's measurement write probe in {}: {error}",
                    dir.display()
                )
            })?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Err(format!(
            "cannot write to measurements directory {}: {error}. Check parent-directory access; if you own the directory, run `chmod u+rwx -- '{}'` and retry.",
            dir.display(),
            shell_quote(&dir.to_string_lossy())
        )),
        Err(error) => Err(format!(
            "cannot write to measurements directory {}: {error}; check disk space, mount state, and access",
            dir.display()
        )),
    }
}

pub fn token_usage_from_sources(
    provider: &str,
    agent: &str,
    stdout: &str,
    stderr: &str,
    cassette: Option<&Path>,
) -> TokenUsage {
    let context = collectors::UsageContext {
        provider,
        agent,
        stdout,
        stderr,
        cassette,
    };
    collectors::all()
        .iter()
        .find_map(|adapter| adapter.usage(&context))
        .unwrap_or_default()
}
/// Read and normalize usage events from an agent artifact.
pub fn token_usage_from_document(value: &serde_json::Value) -> TokenUsage {
    collectors::all()
        .iter()
        .find_map(|adapter| adapter.usage_document(value))
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "measurements/token_usage_mock_tests.rs"]
mod token_usage_mock_tests;
/// Read an adapter-verified complete-run counter; absent evidence is unknown.
pub fn steps_from_sources(agent: &str, stdout: &str, stderr: &str) -> Option<u64> {
    collectors::all()
        .iter()
        .find_map(|adapter| adapter.steps(agent, stdout, stderr))
}

fn update_summary(dir: &Path, run: &RunMeasurement<'_>) -> Result<(), String> {
    update_summary_value(dir, serde_json::to_value(run).map_err(|e| e.to_string())?)
}

fn update_summary_value(dir: &Path, run: serde_json::Value) -> Result<(), String> {
    let path = dir.join("summary.json");
    crate::evidence::update_json(&path, |old| {
        let mut document = if let Some(bytes) = old {
            serde_json::from_slice::<serde_json::Value>(bytes)
                .map_err(|_| "historical summary JSON is malformed; preserve it before repair")?
                .as_object()
                .cloned()
                .ok_or("historical summary must be an object; preserve it before repair")?
        } else {
            serde_json::Map::new()
        };
        let mut runs = match document.remove("runs") {
            Some(serde_json::Value::Array(runs))
                if runs.iter().all(serde_json::Value::is_object) =>
            {
                runs
            }
            None if old.is_none() => Vec::new(),
            _ => return Err(
                "historical summary runs must be an array of objects; preserve it before repair"
                    .into(),
            ),
        };
        runs.push(run);
        if runs.len() > MAX_RUNS {
            runs.drain(..runs.len() - MAX_RUNS);
        }
        let mut resolution = crate::workloads::ResolutionSummary::default();
        for run in &runs {
            let kind = run
                .get("workload_evaluation")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let kind = crate::workloads::scored_kind(
                kind,
                &crate::workloads::BaselineEvidence::from_record(run),
            );
            resolution.observe(
                kind,
                run.get("workload_success")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true),
            );
        }
        let fields = serde_json::json!({"schema_version": 2, "runs": runs, "success_count": resolution.resolved_count, "failure_count": resolution.unresolved_count, "task_count": resolution.task_count, "unscored_count": resolution.unscored_count, "success_rate": resolution.success_rate, "success_rate_definition": "resolved task attempts / all selected task attempts; smoke exercises excluded; not pass@k"});
        document.extend(fields.as_object().expect("summary object").clone());
        Ok(document)
    })
}

pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    crate::evidence::write_json(path, value)
}

#[cfg(test)]
#[path = "../tests/unit/measurement_publication.rs"]
pub(crate) mod publication_tests;
pub(crate) fn write_unavailable(path: &Path, collector: &str, error: String) -> Result<(), String> {
    write_json(
        path,
        &CollectorResult {
            observation: Default::default(),
            schema_version: 1,
            collector: collector.into(),
            status: "unavailable".into(),
            exit_code: None,
            exit_signal: None,
            elapsed_ms: 0,
            stdout: String::new(),
            stderr: error,
            artifact: None,
            metrics: BTreeMap::new(),
            series: BTreeMap::new(),
            series_timestamps: BTreeMap::new(),
        },
    )
}

pub(crate) fn escape_svg(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub(crate) fn safe_svg_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn tool_binary_name(name: &str) -> &str {
    crate::tools::find(name)
        .and_then(|tool| tool.executable())
        .unwrap_or(name)
}

pub(crate) fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact")
        .to_owned()
}

pub(crate) fn parse_float(value: &str) -> Option<f64> {
    value
        .trim_matches(|character: char| {
            !character.is_ascii_digit() && character != '.' && character != '-'
        })
        .parse()
        .ok()
}

pub(crate) fn measurement_interval_ms() -> u64 {
    std::env::var("ASB_MEASURE_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1_000)
        .clamp(100, 60_000)
}

pub(crate) fn pidstat_interval_seconds() -> String {
    // pidstat accepts whole-second intervals. Preserve the requested lower
    // bound while rounding sub-second requests up to the nearest supported
    // interval; perf stat below supports the full millisecond range.
    measurement_interval_ms().div_ceil(1_000).to_string()
}

pub(crate) fn bounded(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_CAPTURE)]).into_owned()
}
pub(crate) fn env_true(name: &str) -> bool {
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Probe privileged collectors before the benchmark starts. A direct probe is
/// preferred; only collectors that actually fail the probe are retried with
/// non-interactive sudo. Perf is intentionally not granted a sudoers rule:
/// its kernel policy has a narrower, user-applied repair path below.
fn preflight_collectors(
    project_root: Option<&Path>,
    measurement_dir: &Path,
    selected: &[String],
    tool_dir: Option<&Path>,
    adapters: &[&'static dyn collectors::CollectorAdapter],
) -> Result<(BTreeSet<String>, collectors::CollectorStates), String> {
    let mut sudo_collectors = BTreeSet::new();
    let mut states = collectors::CollectorStates::default();
    let mut context = collectors::PreflightContext {
        project_root,
        measurement_dir,
        selected,
        tool_dir,
        sudo_collectors: &mut sudo_collectors,
        states: &mut states,
    };
    for collector in adapters {
        collector.preflight(&mut context)?;
    }
    Ok((sudo_collectors, states))
}

pub(crate) fn passwordless_sudo_available() -> bool {
    Command::new("sudo")
        .args(["-n", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub(crate) fn resolve_executable(tool_dir: Option<&Path>, name: &str) -> PathBuf {
    if let Some(tool_dir) = tool_dir {
        let binary = tool_binary_name(name);
        for local in [
            tool_dir.join(name).join("bin").join(binary),
            tool_dir.join("bin").join(binary),
        ] {
            if local.is_file() {
                return local;
            }
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join(tool_binary_name(name));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(tool_binary_name(name))
}

pub(crate) fn passwordless_sudo_error(name: &str, executable: &Path) -> String {
    let command = sudoers_repair_command(name, executable);
    format!(
        "{name} collector needs elevated access, but its direct probe failed and passwordless sudo is unavailable.\nWhat to do: ask the host administrator to configure the necessary non-interactive access for `{}`; edit and validate a policy with:\n  {command}\nGranting unrestricted sudo access to a tracing program can allow arbitrary privileged operations. ASB does not modify sudoers or prompt for a password.",
        executable.display()
    )
}

fn sudoers_repair_command(name: &str, _executable: &Path) -> String {
    let file = format!("/etc/sudoers.d/asb-{name}");
    format!("sudo visudo -f {file}")
}

fn shell_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

fn read_process_sample(pid: u32) -> Option<ProcessSample> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let schedstat = fs::read_to_string(format!("/proc/{pid}/schedstat")).ok();
    parse_process_sample(&stat, schedstat.as_deref())
}

/// Return the root process and every live descendant, including descendants
/// created after the collector attached.  Process-scoped collectors must use
/// this tree rather than a single PID so bridge/agent helper processes are not
/// silently omitted while unrelated host processes remain excluded.
pub(crate) fn process_tree_pids(root: u32) -> Vec<u32> {
    let mut children: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return vec![root];
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(parent) = parse_process_parent(&stat) else {
            continue;
        };
        children.entry(parent).or_default().push(pid);
    }
    descendant_pids(root, &children)
}

fn descendant_pids(root: u32, children: &BTreeMap<u32, Vec<u32>>) -> Vec<u32> {
    let mut tree = vec![root];
    let mut seen = BTreeSet::from([root]);
    let mut index = 0;
    while index < tree.len() {
        let pid = tree[index];
        if let Some(descendants) = children.get(&pid) {
            for descendant in descendants {
                if seen.insert(*descendant) {
                    tree.push(*descendant);
                }
            }
        }
        index += 1;
    }
    tree.sort_unstable();
    tree.dedup();
    tree
}

fn read_process_tree_sample(root: ProcessIdentity) -> Option<ProcessSample> {
    if !root.matches() {
        return None;
    }
    let mut total = ProcessSample::default();
    let mut observed = false;
    for pid in process_tree_pids(root.pid) {
        let Some(identity) = ProcessIdentity::read(pid) else {
            continue;
        };
        if !identity.in_tree(root) {
            continue;
        }
        let Some(sample) = read_process_sample(pid) else {
            continue;
        };
        if !identity.in_tree(root) {
            continue;
        }
        total.user_ticks = total.user_ticks.saturating_add(sample.user_ticks);
        total.system_ticks = total.system_ticks.saturating_add(sample.system_ticks);
        total.io_delay_ticks = total.io_delay_ticks.saturating_add(sample.io_delay_ticks);
        total.runqueue_ns = total.runqueue_ns.saturating_add(sample.runqueue_ns);
        observed = true;
    }
    (observed && root.matches()).then_some(total)
}

fn parse_process_parent(stat: &str) -> Option<u32> {
    let (_, rest) = stat.rsplit_once(')')?;
    let fields = rest.split_whitespace().collect::<Vec<_>>();
    fields.get(1)?.parse().ok()
}

fn parse_process_sample(stat: &str, schedstat: Option<&str>) -> Option<ProcessSample> {
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let user_ticks = fields.get(11)?.parse().ok()?;
    let system_ticks = fields.get(12)?.parse().ok()?;
    // Linux procfs field 42 is delayacct_blkio_ticks. It is zero when
    // delay accounting is disabled, which is intentionally reported as zero.
    let io_delay_ticks = fields
        .get(39)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let runqueue_ns = schedstat
        .and_then(|value| value.split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(0);
    Some(ProcessSample {
        user_ticks,
        system_ticks,
        io_delay_ticks,
        runqueue_ns,
    })
}

fn clock_ticks_per_second() -> Option<u64> {
    let output = Command::new("getconf")
        .arg("CLK_TCK")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn process_fixture(proc: &Path, pid: u32, parent: u32, start: u64, state: &str) {
        fs::create_dir_all(proc.join(pid.to_string())).unwrap();
        let mut fields = vec!["0".to_owned(); 40];
        fields[0] = state.into();
        fields[1] = parent.to_string();
        fields[19] = start.to_string();
        fs::write(
            proc.join(pid.to_string()).join("stat"),
            format!("{pid} (agent (nested) name) {}", fields.join(" ")),
        )
        .unwrap();
    }

    #[test]
    fn process_generation_and_ancestry_reject_reuse_exit_reparenting_and_cycles() {
        let proc = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("proc-identities", None).attempt_id);
        process_fixture(&proc, 1, 0, 1, "S");
        process_fixture(&proc, 10, 1, 100, "T");
        process_fixture(&proc, 20, 10, 200, "S");
        let root = ProcessIdentity::read_at(&proc, 10).unwrap();
        let child = ProcessIdentity::read_at(&proc, 20).unwrap();
        assert!(child.in_tree_at(root, &proc));
        assert!(root.in_tree_at(root, &proc));
        // A recycled PID is not the original process, even if its new parent
        // has the same PID. Moving a live task outside the tree also excludes it.
        process_fixture(&proc, 20, 10, 201, "S");
        assert!(!child.matches_at(&proc));
        assert!(!child.in_tree_at(root, &proc));
        process_fixture(&proc, 20, 1, 200, "S");
        assert!(child.matches_at(&proc));
        assert!(!child.in_tree_at(root, &proc));
        for state in ["Z", "X", "x"] {
            process_fixture(&proc, 20, 10, 200, state);
            assert!(!child.matches_at(&proc));
        }
        process_fixture(&proc, 20, 21, 200, "S");
        process_fixture(&proc, 21, 20, 210, "S");
        assert!(!child.in_tree_at(root, &proc));
        process_fixture(&proc, 20, 10, 200, "S");
        process_fixture(&proc, 10, 1, 101, "T");
        assert!(!child.in_tree_at(root, &proc));
        fs::remove_file(proc.join("10/stat")).unwrap();
        assert!(!root.matches_at(&proc));
        process_fixture(&proc, 20, 10, 0, "S");
        assert!(ProcessIdentity::read_at(&proc, 20).is_none());
        fs::write(proc.join("20/stat"), "partial").unwrap();
        assert!(ProcessIdentity::read_at(&proc, 20).is_none());
        fs::remove_dir_all(proc).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn stale_sampler_or_isolated_target_never_attaches_to_a_reused_pid() {
        let current = ProcessIdentity::read(std::process::id()).unwrap();
        let stale = ProcessIdentity {
            start_time_ticks: current.start_time_ticks + 1,
            ..current
        };
        assert!(ProcessSampler::start(stale, 1000).is_none());
        let dir = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("stale-target", None).attempt_id);
        let mut session = Session::start_with_collectors(&dir, &[], None).unwrap();
        assert!(
            session
                .attach_isolated_pid(stale.pid, stale.start_time_ticks)
                .is_err()
        );
        assert!(session.process_identity.is_none());
        assert!(session.process_sampler.is_none());
        assert!(session.collectors.is_empty());
        drop(session);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn agent_end_stops_timing_sampler_without_waiting_for_the_next_interval() {
        let dir = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("sampler-end", None).attempt_id);
        let mut session = Session::start_with_collectors(&dir, &[], None).unwrap();
        let root = ProcessIdentity::read(std::process::id()).unwrap();
        session.process_sampler = ProcessSampler::start(root, 10000);
        assert!(session.process_sampler.is_some());
        session
            .event(collectors::LifecycleEvent::AgentExecuteBegin)
            .unwrap();
        let start = Instant::now();
        session
            .event(collectors::LifecycleEvent::AgentExecuteEnd)
            .unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(session.process_sampler.is_none());
        drop(session);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unobserved_sampled_cpu_is_unknown_but_exact_zero_cgroup_totals_are_preserved() {
        let dir = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("sampled-zero", None).attempt_id);
        let mut session = Session::start_with_collectors(&dir, &[], None).unwrap();
        for (source, expected) in [("proc-sampling", None), ("cgroup-v2", Some(0.0))] {
            session.component_timings.insert(
                "agent.execute".into(),
                ComponentTiming {
                    cpu_user_ms: Some(0.0),
                    cpu_system_ms: Some(0.0),
                    cpu_accounting_source: Some(source.into()),
                    ..ComponentTiming::default()
                },
            );
            session.suppress_unobserved_sampled_cpu();
            assert_eq!(
                session.component_timings["agent.execute"].cpu_user_ms,
                expected
            );
            assert_eq!(
                session.component_timings["agent.execute"].cpu_system_ms,
                expected
            );
        }
        drop(session);
        fs::remove_dir_all(dir).unwrap();
    }
    use crate::collectors::adapters::bpftrace::{
        bpftrace_profile_names, parse_bpftrace_metrics, parse_bpftrace_series,
        selected_bpftrace_profiles, write_bpftrace_profile_svg,
    };
    use crate::collectors::adapters::iostat::{parse_iostat_metrics, parse_pidstat_io_metrics};
    use crate::collectors::adapters::mpstat::{parse_mpstat_metrics, parse_pidstat_cpu_metrics};
    use crate::collectors::adapters::perf::{
        parse_perf_stat_metrics, parse_perf_stat_series, perf_permission_error,
    };
    use crate::collectors::adapters::perfetto::{
        parse_perfetto_series_output, perfetto_config, perfetto_metric_queries, perfetto_scope_sql,
        perfetto_series_queries, write_perfetto_metric_svgs,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn perfetto_metrics_produce_distinct_escaped_svg_artifacts() {
        let dir = std::env::temp_dir().join(format!("asb-metric-svg-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let metrics = BTreeMap::from([
            ("perfetto.sched_switch".to_owned(), 3.0),
            ("perfetto.wait<unsafe>".to_owned(), 12.0),
        ]);
        write_perfetto_metric_svgs(&dir, None, 2, &metrics).unwrap();
        let first = fs::read_to_string(dir.join("perfetto-metric-sched_switch.svg")).unwrap();
        let second = fs::read_to_string(dir.join("perfetto-metric-wait-unsafe-.svg")).unwrap();
        assert!(first.contains("sched_switch"));
        assert!(second.contains("wait&lt;unsafe&gt;"));
        assert!(!second.contains("wait<unsafe>"));
        assert!(second.contains("native trace unavailable"));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn perfetto_without_metrics_reports_tracepoints_as_a_fallback() {
        let dir = std::env::temp_dir().join(format!("asb-metric-fallback-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        write_perfetto_metric_svgs(&dir, None, 9, &BTreeMap::new()).unwrap();
        let svg = fs::read_to_string(dir.join("perfetto-metric-tracepoints.svg")).unwrap();
        assert!(svg.contains("lifecycle markers: 9"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bpftrace_svg_renders_each_metric_and_escapes_status() {
        let dir = std::env::temp_dir().join(format!("asb-bpf-svg-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let metrics = BTreeMap::from([
            ("sched.total".to_owned(), 7.0),
            ("sched.max".to_owned(), 3.0),
        ]);
        write_bpftrace_profile_svg(&dir, "sched_latency", "failure <mock>", &metrics).unwrap();
        let svg = fs::read_to_string(dir.join("bpftrace-sched_latency.svg")).unwrap();
        assert!(svg.contains("failure &lt;mock&gt;"));
        assert!(svg.contains("total: 7"));
        assert!(svg.contains("max: 3"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn collector_svg_write_errors_report_the_artifact_type() {
        let absent =
            std::env::temp_dir().join(format!("asb-no-such-metric-dir-{}", std::process::id()));
        assert!(
            write_perfetto_metric_svgs(&absent, None, 0, &BTreeMap::new())
                .unwrap_err()
                .contains("Perfetto metric SVG")
        );
        assert!(
            write_bpftrace_profile_svg(&absent, "sched", "error", &BTreeMap::new())
                .unwrap_err()
                .contains("bpftrace profile SVG")
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn execution_end_finalizes_only_scoped_collectors_before_grading() {
        use crate::collectors::CollectorAdapter;
        struct Fixture(bool);
        impl collectors::CollectorAdapter for Fixture {
            fn producer_policy(&self, _: &str) -> ProducerPolicy {
                ProducerPolicy {
                    stream_bytes: 1024 * 1024,
                    graceful: Some(Duration::from_secs(1)),
                }
            }
            fn id(&self) -> &'static str {
                if self.0 {
                    "execution-fixture"
                } else {
                    "whole-run-fixture"
                }
            }
            fn requires_pid(&self) -> bool {
                self.0
            }
            fn process(
                &self,
                _: &mut Session,
                context: &collectors::ProcessContext<'_>,
            ) -> Result<collectors::ProcessData, String> {
                assert!(context.stderr_reader().is_ok());
                if self.0 {
                    std::thread::sleep(Duration::from_millis(150));
                }
                Ok(collectors::ProcessData::from_output(context))
            }
        }
        static SCOPED: Fixture = Fixture(true);
        static WHOLE: Fixture = Fixture(false);
        for end in [
            collectors::LifecycleEvent::AgentExecuteEnd,
            collectors::LifecycleEvent::RunFailed,
        ] {
            let dir = std::env::temp_dir()
                .join(crate::workloads::TaskIdentity::local("collector-boundary", None).attempt_id);
            let mut session = Session::start_with_collectors(&dir, &[], None).unwrap();
            session
                .event(collectors::LifecycleEvent::RunStarted)
                .unwrap();
            session
                .event(collectors::LifecycleEvent::AgentExecuteBegin)
                .unwrap();
            for adapter in [&SCOPED, &WHOLE] {
                let stdout_path = dir.join(format!("{}.stdout", adapter.id()));
                let stderr_path = dir.join(format!("{}.stderr", adapter.id()));
                let captures = [
                    crate::evidence::Capture::create(&stdout_path).unwrap(),
                    crate::evidence::Capture::create(&stderr_path).unwrap(),
                ];
                captures[0]
                    .writer()
                    .unwrap()
                    .write_all(b"bounded fixture counter\n")
                    .unwrap();
                session
                    .collectors
                    .push(Collector::fixture(adapter.id(), adapter, captures));
            }
            session.event(end).unwrap();
            assert_eq!(session.collectors.len(), 1);
            assert!(session.collectors[0].producer.poll().unwrap().is_none());
            assert_eq!(session.stopped_collectors.len(), 1);
            assert_eq!(
                session.stopped_collectors[0]
                    .completion
                    .guardian_cleanup_confirmed,
                Some(true)
            );
            assert!(!dir.join("execution-fixture.json").exists());
            assert!(!dir.join("whole-run-fixture.json").exists());
            let recorded_end = session.lifecycle.last().unwrap().elapsed_us;
            // Simulate grading after the safe execution boundary. Reporting
            // and its deliberately slow processor run only after that phase.
            std::thread::sleep(Duration::from_millis(150));
            session.stop_collector_processes(false).unwrap();
            session.process_stopped_collectors().unwrap();
            assert!(session.lifecycle_started.elapsed().as_micros() >= recorded_end + 100_000);
            if end == collectors::LifecycleEvent::AgentExecuteEnd {
                assert!(
                    session.lifecycle_started.elapsed().as_millis()
                        >= session.component_timings["agent.execute"].wall_ms + 100
                );
            }
            // Final session processing must not process an already finalized
            // execution collector again or replace its retained report.
            fs::write(dir.join("execution-fixture.json"), "preserved").unwrap();
            session.stop_collector_processes(false).unwrap();
            session.process_stopped_collectors().unwrap();
            assert!(session.collectors.is_empty());
            assert!(dir.join("whole-run-fixture.json").is_file());
            for name in ["execution-fixture", "whole-run-fixture"] {
                assert!(!dir.join(format!("{name}.stdout")).exists());
                assert!(!dir.join(format!("{name}.stderr")).exists());
            }
            assert_eq!(
                fs::read_to_string(dir.join("execution-fixture.json")).unwrap(),
                "preserved"
            );
            drop(session);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn failing_optional_collector_processing_does_not_preempt_grading() {
        struct Failing;
        impl collectors::CollectorAdapter for Failing {
            fn producer_policy(&self, _: &str) -> ProducerPolicy {
                ProducerPolicy {
                    stream_bytes: 1024 * 1024,
                    graceful: Some(Duration::from_secs(1)),
                }
            }
            fn id(&self) -> &'static str {
                "failing-export"
            }
            fn requires_pid(&self) -> bool {
                true
            }
            fn process(
                &self,
                _: &mut Session,
                context: &collectors::ProcessContext<'_>,
            ) -> Result<collectors::ProcessData, String> {
                assert!(context.stderr_reader().is_ok());
                Err("fixture SVG export failed".into())
            }
        }
        static FAILING: Failing = Failing;
        let dir = std::env::temp_dir().join(
            crate::workloads::TaskIdentity::local("collector-export-failure", None).attempt_id,
        );
        let mut session = Session::start_with_collectors(&dir, &[], None).unwrap();
        let stdout_path = dir.join("failing-export.stdout");
        let stderr_path = dir.join("failing-export.stderr");
        let captures = [
            crate::evidence::Capture::create(&stdout_path).unwrap(),
            crate::evidence::Capture::create(&stderr_path).unwrap(),
        ];
        captures[0]
            .writer()
            .unwrap()
            .write_all(b"bounded counter\n")
            .unwrap();
        session
            .collectors
            .push(Collector::fixture("failing-export", &FAILING, captures));
        session
            .event(collectors::LifecycleEvent::AgentExecuteEnd)
            .unwrap();
        assert!(!dir.join("failing-export.json").exists());
        assert!(session.collectors.is_empty());
        assert_eq!(
            session.stopped_collectors[0]
                .completion
                .guardian_cleanup_confirmed,
            Some(true)
        );
        // Evaluation can complete before the intentionally failing export is
        // even invoked. Its report must fail honestly, not erase that grade.
        fs::write(dir.join("official-grade.json"), "{\"resolved\":true}").unwrap();
        session.process_stopped_collectors().unwrap();
        let result: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("failing-export.json")).unwrap())
                .unwrap();
        assert_eq!(result["status"], "failed");
        assert!(!dir.join("failing-export.stdout").exists());
        assert!(!dir.join("failing-export.stderr").exists());
        assert!(
            result["stderr"]
                .as_str()
                .unwrap()
                .contains("fixture SVG export failed")
        );
        assert_eq!(
            fs::read_to_string(dir.join("official-grade.json")).unwrap(),
            "{\"resolved\":true}"
        );
        drop(session);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn component_total_without_finishing_event_uses_session_clock() {
        let dir = std::env::temp_dir().join(format!("asb-component-total-{}", unix_ms()));
        let mut session = Session::start_with_collectors(&dir, &[], None).unwrap();
        session.lifecycle_started = Instant::now() - Duration::from_secs(3);
        session.write_component_measurements().unwrap();
        let components: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("components.json")).unwrap())
                .unwrap();
        assert!(
            components["components"]["run.total"]["wall_ms"]
                .as_u64()
                .unwrap()
                >= 3_000
        );
        drop(session);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writes_machine_readable_run_and_summary() {
        let dir = std::env::temp_dir().join(format!("asb-measure-{}", unix_ms()));
        let mut session = Session::start_with_collectors(&dir, &[], None).expect("session");
        session
            .event(collectors::LifecycleEvent::RunStarted)
            .unwrap();
        session
            .event(collectors::LifecycleEvent::PhaseBegin(
                collectors::LifecyclePhase::WorkloadPreparation,
            ))
            .unwrap();
        session
            .event(collectors::LifecycleEvent::PhaseEnd(
                collectors::LifecyclePhase::WorkloadPreparation,
            ))
            .unwrap();
        let run = RunMeasurement {
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
            status: crate::runner::disposition::RunStatus::Completed,
            success: true,
            process_success: true,
            workload_success: Some(true),
            failure_kind: None,
            failure_reason: None,
            success_rate: Some(1.0),
            adapter_smoke_success: None,
            agent: "a",
            workload: "w",
            task: None,
            workload_evaluation: "bundled-fixture",
            workload_baseline_passed: Some(false),
            workload_baseline: None,
            workload_failure: None,
            provider: "p",
            model: "m",
            elapsed_ms: 2,
            exit_code: Some(0),
            token_usage: TokenUsage::default(),
            steps: Some(1),
            started_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        };
        session
            .event(collectors::LifecycleEvent::RunFinishing)
            .unwrap();
        // Deliberately distinct deterministic lifecycle and execution clocks:
        // exporting components must not replace total time with the 2ms run
        // metric or include any later collector/report finalization delay.
        session.lifecycle.last_mut().unwrap().elapsed_us = 7_500_000;
        session.finish(run).expect("finish");
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("summary.json")).unwrap()).unwrap();
        assert_eq!(parsed["success_rate"], 1.0);
        let recorded: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("run.json")).unwrap()).unwrap();
        assert_eq!(recorded["elapsed_ms"], 2);
        let components: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("components.json")).unwrap())
                .unwrap();
        assert!(components["components"]["workload.prepare"]["wall_ms"].is_number());
        assert_eq!(components["components"]["run.total"]["wall_ms"], 7_500);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_openrouter_usage_details_from_agent_json() {
        let usage = token_usage_from_sources(
            "openrouter",
            "opencode",
            r#"{"usage":{"prompt_tokens":120,"completion_tokens":30,"total_tokens":150,"prompt_tokens_details":{"cached_tokens":40}}}"#,
            "",
            None,
        );
        assert_eq!(usage.input_tokens, Some(120));
        assert_eq!(usage.output_tokens, Some(30));
        assert_eq!(usage.cached_tokens, Some(40));
        assert_eq!(usage.total_tokens, Some(150));
        assert_eq!(usage.source.as_deref(), Some("agent-output"));
    }

    #[test]
    fn parses_cli2key_text_aliases_and_derives_total() {
        let usage = token_usage_from_sources(
            "cli2key",
            "codex",
            "input_tokens=8 output_tokens=5 cached_tokens=2",
            "",
            None,
        );
        assert_eq!(usage.input_tokens, Some(8));
        assert_eq!(usage.output_tokens, Some(5));
        assert_eq!(usage.cached_tokens, Some(2));
        assert_eq!(usage.total_tokens, Some(13));
        assert_eq!(usage.source.as_deref(), Some("agent-output"));
    }

    #[test]
    fn parses_human_agent_token_summary() {
        let usage = token_usage_from_sources("cli2key", "codex", "tokens used\n42,567\n", "", None);
        assert_eq!(usage.total_tokens, Some(42_567));
        assert_eq!(usage.source.as_deref(), Some("agent-output"));
    }

    #[test]
    fn parses_usage_events_emitted_by_each_supported_agent_family() {
        let cases = [
            (
                "codex",
                r#"response.completed {"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}"#,
                7,
                3,
                10,
            ),
            (
                "pi",
                r#"event: {"type":"turn_end","usage":{"input":7,"output":3,"total":10,"cacheRead":2}}"#,
                7,
                3,
                10,
            ),
            (
                "qwen-code",
                r#"{"type":"result","stats":{"total_tokens":10,"input_tokens":7,"output_tokens":3,"cached":2}}"#,
                7,
                3,
                10,
            ),
            (
                "workbuddy",
                r#"done {"usage":{"inputTokens":7,"outputTokens":3,"totalTokens":10}}"#,
                7,
                3,
                10,
            ),
            (
                "openjiuwen",
                r#"{"type":"llm_usage","payload":{"usage_metadata":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}}"#,
                7,
                3,
                10,
            ),
            (
                "openhands",
                r#"llm response {"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}"#,
                7,
                3,
                10,
            ),
            (
                "mini-swe",
                r#"result {"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}"#,
                7,
                3,
                10,
            ),
            (
                "claude-code",
                r#"{"type":"result","usage":{"input_tokens":17,"output_tokens":4,"cache_read_input_tokens":2,"total_tokens":21}}"#,
                17,
                4,
                21,
            ),
            (
                "goose",
                r#"{"message":{"usage":{"prompt_tokens":19,"completion_tokens":6,"total_tokens":25}}}"#,
                19,
                6,
                25,
            ),
            (
                "cline",
                r#"event {"usage":{"inputTokens":23,"outputTokens":8,"cachedTokens":3,"totalTokens":31}}"#,
                23,
                8,
                31,
            ),
            (
                "continue",
                r#"{"usage":{"input":31,"output":10,"cached":4,"total":41}}"#,
                31,
                10,
                41,
            ),
        ];
        for (agent, output, input, response, total) in cases {
            let usage = token_usage_from_sources("cli2key", agent, output, "", None);
            assert_eq!(usage.input_tokens, Some(input), "{agent} input");
            assert_eq!(usage.output_tokens, Some(response), "{agent} output");
            assert_eq!(usage.total_tokens, Some(total), "{agent} total");
        }
    }

    #[test]
    fn derives_mini_swe_total_from_complete_input_and_output() {
        let usage = token_usage_from_sources(
            "cli2key",
            "mini-swe",
            r#"{"usage":{"input_tokens":182398,"output_tokens":4189,"total_tokens":116}}"#,
            "",
            None,
        );
        assert_eq!(usage.input_tokens, Some(182398));
        assert_eq!(usage.output_tokens, Some(4189));
        assert_eq!(usage.total_tokens, Some(186587));
    }

    #[test]
    fn parses_provider_usage_metadata_and_agent_raw_usage_shapes() {
        let cases = [
            (
                "workbuddy",
                r#"[{"rawUsage":{"inputTokens":21,"outputTokens":5,"totalTokens":26,"inputTokensDetails":{"cached_tokens":3}}}]"#,
            ),
            (
                "openhands",
                r#"usage: input: 31 output: 7 cached: 4 total: 38"#,
            ),
            (
                "openjiuwen",
                r#"{"type":"llm_usage","payload":{"usage_metadata":{"input_tokens":41,"output_tokens":9,"cache_read_tokens":6,"total_tokens":50}}}"#,
            ),
        ];
        for (agent, output) in cases {
            let usage = token_usage_from_sources("anthropic", agent, output, "", None);
            assert_eq!(
                usage.input_tokens,
                Some(if agent == "workbuddy" {
                    21
                } else if agent == "openhands" {
                    31
                } else {
                    41
                }),
                "{agent} input"
            );
            assert_eq!(
                usage.output_tokens,
                Some(if agent == "workbuddy" {
                    5
                } else if agent == "openhands" {
                    7
                } else {
                    9
                }),
                "{agent} output"
            );
            assert_eq!(
                usage.cached_tokens,
                Some(if agent == "workbuddy" {
                    3
                } else if agent == "openhands" {
                    4
                } else {
                    6
                }),
                "{agent} cached"
            );
            assert_eq!(
                usage.total_tokens,
                Some(if agent == "workbuddy" {
                    26
                } else if agent == "openhands" {
                    38
                } else {
                    50
                }),
                "{agent} total"
            );
        }
    }

    #[test]
    fn counts_observed_completions_without_inventing_a_fallback() {
        let output = r#"{"type":"run_result","iterations":2}"#;
        assert_eq!(steps_from_sources("cline", output, ""), Some(2));
        assert_eq!(steps_from_sources("codex", "completed", ""), None);
        assert_eq!(steps_from_sources("codex", "", ""), None);
    }

    #[test]
    fn reads_usage_from_a_recorded_cassette() {
        let path = std::env::temp_dir().join(format!("asb-usage-cassette-{}", unix_ms()));
        fs::write(
            &path,
            r#"{"request":{},"response":{"body":"{\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3,\"total_tokens\":5}}"}}"#,
        )
        .expect("cassette writes");
        let usage = token_usage_from_sources("cli2key", "codex", "", "", Some(&path));
        assert_eq!(usage.input_tokens, Some(2));
        assert_eq!(usage.output_tokens, Some(3));
        assert_eq!(usage.total_tokens, Some(5));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn perf_permission_error_suggests_kernel_policy_not_sudoers() {
        let message = perf_permission_error();
        assert!(message.contains("kernel.perf_event_paranoid=-1"));
        assert!(message.contains("sudo sysctl -w kernel.perf_event_paranoid=-1"));
        assert!(message.contains("/etc/sysctl.d/99-asb-perf.conf"));
        assert!(message.contains("does not create sudoers rules"));
        assert!(!message.contains("sudoers.d"));
    }

    #[test]
    fn parses_mpstat_cpu_breakdown_into_averaged_metrics() {
        let metrics = parse_mpstat_metrics(
            "12:00:00 AM CPU %usr %nice %sys %iowait %irq %soft %steal %guest %gnice %idle\n12:00:00 AM all 1.0 2.0 3.0 4.0 5.0 6.0 7.0 8.0 9.0 55.0\n",
        );
        assert_eq!(metrics["cpu.user_pct"], 1.0);
        assert_eq!(metrics["cpu.system_pct"], 3.0);
        assert_eq!(metrics["cpu.iowait_pct"], 4.0);
        assert_eq!(metrics["cpu.guest_nice_pct"], 9.0);
        assert_eq!(metrics["cpu.idle_pct"], 55.0);
    }

    #[test]
    fn parses_process_scoped_pidstat_cpu_and_io_metrics() {
        let cpu = parse_pidstat_cpu_metrics(
            "Linux 6.1\n12:00:00 UID PID %usr %system %guest %wait %CPU CPU Command\n12:00:01 1000 42 10.00 2.00 0.00 1.00 12.00 3 agent\n12:00:02 1000 42 20.00 4.00 0.00 0.00 24.00 3 agent\n",
        );
        assert_eq!(cpu["process.cpu.user_pct"], 15.0);
        assert_eq!(cpu["process.cpu.system_pct"], 3.0);
        assert_eq!(cpu["process.cpu.total_pct"], 18.0);
        let io = parse_pidstat_io_metrics(
            "Linux 6.1\n12:00:00 UID PID kB_rd/s kB_wr/s kB_ccwr/s iodelay Command\n12:00:01 1000 42 1.0 2.0 0.5 3 agent\n",
        );
        assert_eq!(io["process.io.read_kib_per_s"], 1.0);
        assert_eq!(io["process.io.write_kib_per_s"], 2.0);
        assert_eq!(io["process.io.delay_ticks"], 3.0);
        let series = crate::collectors::adapters::mpstat::parse_series(
            "Linux 6.1\n12:00:00 UID PID %usr %system %guest %wait %CPU CPU Command\n12:00:01 1000 42 10.00 2.00 0.00 1.00 12.00 3 agent\n12:00:02 1000 42 20.00 4.00 0.00 0.00 24.00 3 agent\n",
        );
        assert_eq!(series["process.cpu.user_pct"], vec![10.0, 20.0]);
        assert_eq!(series["process.cpu.total_pct"], vec![12.0, 24.0]);
        let io_series = crate::collectors::adapters::iostat::parse_series(
            "Linux 6.1\n12:00:01 UID PID kB_rd/s kB_wr/s kB_ccwr/s iodelay Command\n12:00:01 1000 42 1.0 2.0 0.5 3 agent\n12:00:02 1000 42 2.0 4.0 1.0 5 agent\n",
        );
        assert_eq!(io_series["process.io.read_kib_per_s"], vec![1.0, 2.0]);
    }

    #[test]
    fn parses_iostat_cpu_and_device_throughput_and_latency() {
        let metrics = parse_iostat_metrics(
            "avg-cpu:  %user   %nice %system %iowait  %steal   %idle\n           1.00    2.00    3.00    4.00    5.00   85.00\n\nDevice            r/s     rkB/s   r_await w/s     wkB/s   w_await aqu-sz %util\nvda              10.00   20.00   1.50    30.00   40.00   2.50    0.10   7.00\n",
        );
        assert_eq!(metrics["cpu.user_pct"], 1.0);
        assert_eq!(metrics["cpu.iowait_pct"], 4.0);
        assert_eq!(metrics["device.read_ops_per_s"], 10.0);
        assert_eq!(metrics["device.read_await_ms"], 1.5);
        assert_eq!(metrics["device.write_kib_per_s"], 40.0);
        assert_eq!(metrics["device.util_pct"], 7.0);
    }

    #[test]
    fn parses_perf_stat_counters_into_stable_metrics() {
        let metrics = parse_perf_stat_metrics(
            "12.34,msec,task-clock,100.00,\n8,,context-switches,100.00,\n2,,cpu-migrations,100.00,\n4,,page-faults,100.00,\n1234,,cycles,100.00,\n567,,instructions,100.00,\n",
        );
        assert_eq!(metrics["perf.task_clock_ms"], 12.34);
        assert_eq!(metrics["perf.context_switches"], 8.0);
        assert_eq!(metrics["perf.cpu_migrations"], 2.0);
        assert_eq!(metrics["perf.page_faults"], 4.0);
        assert_eq!(metrics["perf.observed_cycles_count"], 1234.0);
        assert_eq!(metrics["perf.observed_instructions_count"], 567.0);
        let series = parse_perf_stat_series(
            "0.000,12.34,msec,task-clock,100.00,\n1.000,20.00,msec,task-clock,100.00,\n0.000,8,,context-switches,100.00,\n",
        );
        assert_eq!(series["perf.task_clock_ms"], vec![12.34, 20.0]);
        assert_eq!(series["perf.context_switches"], vec![8.0]);
    }

    #[test]
    fn bpftrace_profiles_are_target_scoped_and_parse_only_asb_metrics() {
        assert_eq!(
            bpftrace_profile_names().collect::<Vec<_>>(),
            vec![
                "sched-latency",
                "cpu-migrations",
                "futex-wait",
                "syscall-rate",
                "lock-contention",
                "socket-operations",
                "io-wait"
            ]
        );
        let profiles = selected_bpftrace_profiles(&[
            "bpftrace:sched-latency".into(),
            "bpftrace:futex-wait".into(),
        ])
        .expect("profile selection");
        assert_eq!(profiles.len(), 2);
        assert_eq!(
            selected_bpftrace_profiles(&["bpftrace".into()])
                .expect("generic bpftrace selects built-ins")
                .len(),
            bpftrace_profile_names().count() - 2
        );
        assert!(
            profiles
                .iter()
                .all(|profile| profile.program.contains("@asb_tree"))
        );
        assert!(
            profiles
                .iter()
                .all(|profile| profile.program.contains("interval:s:1"))
        );
        let metrics = parse_bpftrace_metrics(
            "sched-latency",
            "@asb_tree[123]: 1\n@asb_wakeup_latency_events: 7\n@asb_wakeup_latency_total_ns: 420\n@other: 99\n",
        );
        assert_eq!(metrics["bpftrace.sched-latency.wakeup_latency_events"], 7.0);
        assert_eq!(
            metrics["bpftrace.sched-latency.wakeup_latency_total_ns"],
            420.0
        );
        assert_eq!(metrics.len(), 2);
        let series = parse_bpftrace_series(
            "sched-latency",
            "@asb_wakeup_latency_events: 2\n@asb_wakeup_latency_events: 5\n",
        );
        assert_eq!(
            series["bpftrace.sched-latency.wakeup_latency_events"],
            vec![2.0, 5.0]
        );
    }

    #[test]
    fn perfetto_config_collects_scheduler_process_and_trace_marker_events() {
        let config = perfetto_config(None);
        for event in [
            "sched/sched_switch",
            "sched/sched_waking",
            "sched/sched_wakeup_new",
            "sched/sched_process_exit",
            "task/task_newtask",
            "ftrace/print",
        ] {
            assert!(config.contains(event), "missing {event}");
        }
        assert!(config.contains("linux.process_stats"));
        assert!(config.contains("compact_sched"));
        assert!(!config.contains("sched_stat_"));
    }

    #[test]
    fn project_local_perfetto_uses_tracebox_binary_name() {
        assert_eq!(tool_binary_name("perfetto"), "tracebox");
        assert_eq!(tool_binary_name("perf"), "perf");
    }

    #[test]
    fn perfetto_configuration_uses_supported_tracebox_schema() {
        let config = perfetto_config(Some(42));
        assert!(!config.contains("pid_filter"));
        assert!(config.contains("scan_all_processes_on_start: false"));
    }

    #[test]
    fn perfetto_queries_scope_to_the_target_process_tree() {
        let metrics = perfetto_metric_queries(Some(42), "1-2");
        assert!(
            metrics
                .iter()
                .all(|(_, query)| query.contains("WITH RECURSIVE tree"))
        );
        assert!(metrics.iter().all(|(_, query)| query.contains("pid=42")));
        assert!(
            metrics
                .iter()
                .any(|(_, query)| query.contains("th.upid IN tree"))
        );
        let series = perfetto_series_queries(Some(42), "1-2");
        assert!(
            series
                .iter()
                .all(|(_, query)| query.contains("asb.agent.started.1-2"))
        );
        assert!(
            series
                .iter()
                .all(|(_, query)| query.contains("asb.agent.exited.1-2"))
        );
        assert_ne!(perfetto_scope_sql(42, "1-2"), perfetto_scope_sql(42, "1-3"));
        assert!(
            series
                .iter()
                .all(|(_, query)| query.contains("bounds.start_ts"))
        );
        let event_series = &series[0].1;
        assert!(event_series.contains("CAST((s.ts-bounds.start_ts)/1000000000 AS INTEGER)"));
        assert!(event_series.contains("s.ts >= bounds.start_ts"));
        let duration_series = &series[1].1;
        assert!(duration_series.contains("bucket_ranges(bucket)"));
        assert!(duration_series.contains("relevant AS"));
        assert!(duration_series.contains(
            "MIN(r.ts+r.dur,bounds.end_ts,bounds.start_ts+(bucket_ranges.bucket+1)*1000000000)"
        ));
        assert!(
            duration_series.contains("MAX(r.ts,bounds.start_ts+bucket_ranges.bucket*1000000000)")
        );
        assert!(perfetto_metric_queries(None, "1-2").is_empty());
    }

    #[test]
    fn parses_perfetto_csv_series_with_trace_timestamps() {
        let (timestamps, values) = parse_perfetto_series_output(
            "\"timestamp_s\",\"value\"\n\"12\",\"3\"\n\"13.5\",\"7\"\n",
        );
        assert_eq!(timestamps, vec![12_000.0, 13_500.0]);
        assert_eq!(values, vec![3.0, 7.0]);
    }

    #[test]
    fn process_accounting_separates_cpu_io_and_scheduler_wait() {
        let stat = "123 (agent) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40 41 42 43 44 45 46 47";
        let sample = parse_process_sample(stat, Some("100000 250000 1")).expect("sample");
        assert_eq!(sample.user_ticks, 11);
        assert_eq!(sample.system_ticks, 12);
        assert_eq!(sample.io_delay_ticks, 39);
        assert_eq!(sample.runqueue_ns, 250_000);
    }

    #[test]
    fn malformed_process_accounting_is_unavailable_not_zero_guesswork() {
        assert!(parse_process_sample("not a proc stat", None).is_none());
        assert!(parse_process_sample("123 (agent) S", None).is_none());
    }

    #[test]
    fn process_tree_includes_nested_helpers_but_not_unrelated_processes() {
        let children = BTreeMap::from([(10, vec![11, 12]), (11, vec![13]), (99, vec![100])]);
        assert_eq!(descendant_pids(10, &children), vec![10, 11, 12, 13]);
    }

    #[test]
    fn parses_process_parent_from_proc_stat_with_spaces_in_command() {
        assert_eq!(parse_process_parent("42 (agent helper) S 7 8 9"), Some(7));
    }
}
