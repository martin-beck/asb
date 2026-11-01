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
use serde::Serialize;
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
#[cfg(test)]
use std::time::Duration;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_CAPTURE: usize = 256 * 1024;
static NEXT_TRACE_ID: AtomicU64 = AtomicU64::new(0);

pub use crate::usage::TokenUsage;

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

pub(crate) type TreeSnapshotArguments = Box<dyn Fn(&[u32]) -> SnapshotCommand + Send>;

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

/// Concise descriptions for human-facing collector discovery.
pub fn collector_description(id: &str) -> Option<&'static str> {
    collectors::find(id).map(|collector| collector.description())
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
    /// Number of structured tool-call events retained in tool-calls.json.
    ///
    /// A replay that did not validate cannot establish which prefix of the
    /// cassette was served, so it must remain unavailable rather than being
    /// presented as zero calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_count: Option<usize>,
    #[serde(skip)]
    pub(crate) tool_calls: &'a [crate::proxy::tool_calls::ToolCall],
    pub started_unix_ms: u128,
}

#[derive(Serialize)]
struct ToolCallMeasurements<'a> {
    schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
    availability: &'static str,
    calls: &'a [crate::proxy::tool_calls::ToolCall],
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
            completion.and_then(|completion| {
                // A collector such as perf may legitimately produce a bounded
                // capture and exit before the measured process does. Preserve
                // that completed capture for normal adapter processing. Only
                // surface an abnormal principal exit (or an incomplete capture)
                // as startup unavailability.
                if let Some(error) = completion.error {
                    return Some(format!(
                        "collector capture failed during startup: {error}; principal exit: {:?}",
                        completion.exit
                    ));
                }
                completion
                    .exit
                    .is_some_and(|status| !status.success())
                    .then(|| {
                        format!(
                            "collector principal exited during startup: {:?}",
                            completion.exit
                        )
                    })
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
        _namespace_only: bool,
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
        ) && let Err(error) = self.stop_collector_processes(true)
        {
            errors.push(error);
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

    /// Record the CPU collector's Linux `/proc` fallback without exposing the
    /// sampling implementation to orchestration or other adapters.
    pub(crate) fn record_process_tree_accounting(
        &mut self,
        user_ticks: u64,
        system_ticks: u64,
        io_delay_ticks: u64,
        runqueue_ns: u64,
    ) {
        let timing = self
            .component_timings
            .entry("agent.execute".into())
            .or_default();
        if let Some(hz) = self.clock_ticks_per_second {
            timing.cpu_user_ms = Some(user_ticks as f64 * 1_000.0 / hz as f64);
            timing.cpu_system_ms = Some(system_ticks as f64 * 1_000.0 / hz as f64);
            timing.cpu_accounting_source = Some("proc-sampling".into());
            timing.io_wait_ms = Some(io_delay_ticks as f64 * 1_000.0 / hz as f64);
        }
        timing.scheduler_wait_ms = Some(runqueue_ns as f64 / 1_000_000.0);
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

    /// Record the PID returned by the exact direct agent launch.  Unlike a
    /// later /proc discovery, this preserves launch provenance for clients
    /// that intentionally rewrite argv after exec.
    pub(crate) fn agent_process_spawned(&mut self, pid: u32) -> Result<(), String> {
        for collector in self.adapters.clone() {
            collector.agent_process_spawned(self, pid)?;
        }
        Ok(())
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
        Ok(())
    }

    /// Finish collectors and write the run record and aggregate summary.
    pub fn finish<'a>(mut self, measurement: RunMeasurement<'a>) -> Result<(), String> {
        let mut errors = Vec::new();
        let mut observe = |result: Result<(), String>| {
            if let Err(error) = result {
                errors.push(error);
            }
        };
        observe(self.stop_collector_processes(false));
        observe(self.process_stopped_collectors());
        observe(write_json(
            &self.dir.join("tool-calls.json"),
            &ToolCallMeasurements {
                schema_version: 1,
                count: measurement.tool_call_count,
                availability: if measurement.tool_call_count.is_some() {
                    "observed"
                } else {
                    "unavailable"
                },
                calls: measurement.tool_calls,
            },
        ));
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
        observe(
            serde_json::to_value(&measurement)
                .map_err(|error| error.to_string())
                .and_then(|record| crate::workloads::summary::update_summary(&self.dir, record)),
        );
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
                && !collector.adapter.allows_failed_termination(&collector.name)
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
            // Some adapters can turn a complete failed process exit into an
            // unavailable result. Let that adapter inspect the stopped files,
            // but retain those files afterward: a failed capture is evidence,
            // not a successful collector result eligible for consumption.
            let failed_termination = completion.capture_complete
                && !collector
                    .adapter
                    .accepts_termination(&collector.name, &completion)
                && collector.adapter.allows_failed_termination(&collector.name);
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
            read_failed |= failed_termination;
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

    /// A collector adapter may retry its own target-specific probe through an
    /// already configured passwordless sudo route. Generic orchestration never
    /// decides which collector needs elevation.
    pub(crate) fn enable_passwordless_sudo_collector(&mut self, name: &str) -> bool {
        if self.sudo_collectors.contains(name) {
            return true;
        }
        if !passwordless_sudo_available() {
            return false;
        }
        self.sudo_collectors.insert(name.into());
        true
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

pub(crate) fn tool_binary_name(name: &str) -> &str {
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
/// existing non-interactive sudo. ASB never creates or modifies a sudoers
/// rule; perf still reports an explicit host-policy remediation if neither
/// route can use its required events.
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

fn parse_process_parent(stat: &str) -> Option<u32> {
    let (_, rest) = stat.rsplit_once(')')?;
    let fields = rest.split_whitespace().collect::<Vec<_>>();
    fields.get(1)?.parse().ok()
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
    use crate::usage::{
        from_sources as token_usage_from_sources, steps_from_agent as steps_from_sources,
    };
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

    #[test]
    fn unavailable_tool_calls_are_not_serialized_as_zero() {
        let document = serde_json::to_value(ToolCallMeasurements {
            schema_version: 1,
            count: None,
            availability: "unavailable",
            calls: &[],
        })
        .expect("tool-call evidence serializes");
        assert!(document.get("count").is_none());
        assert_eq!(document["availability"], "unavailable");
        assert_eq!(document["calls"], serde_json::json!([]));
    }
    use std::time::{SystemTime, UNIX_EPOCH};

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
        let tool_calls = vec![crate::proxy::tool_calls::ToolCall {
            ordinal: 1,
            source: "llmtape.provider_response",
            kind: "function_call".into(),
            name: Some("read_file".into()),
            id: Some("call-1".into()),
            arguments_present: true,
            response_observed_unix_ms: Some(10),
            execution_duration_ms: Some(3),
            classes: vec!["filesystem.read".into()],
        }];
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
            tool_call_count: Some(tool_calls.len()),
            tool_calls: &tool_calls,
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
        assert_eq!(recorded["tool_call_count"], 1);
        let tool_calls: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("tool-calls.json")).unwrap())
                .unwrap();
        assert_eq!(tool_calls["count"], 1);
        assert_eq!(tool_calls["availability"], "observed");
        assert_eq!(tool_calls["calls"][0]["name"], "read_file");
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
        let prefixed =
            token_usage_from_sources("cli2key", "codex", "info: tokens used\n1,234\n", "", None);
        assert_eq!(prefixed.total_tokens, Some(1234));
    }

    #[test]
    fn other_agents_task_output_does_not_become_codex_usage() {
        let usage = token_usage_from_sources("cli2key", "goose", "tokens used\n42,567\n", "", None);
        assert!(
            !usage.any(),
            "task output was mistaken for usage: {usage:?}"
        );
    }

    #[test]
    fn other_agents_task_output_does_not_become_opencode_usage() {
        let usage = token_usage_from_sources(
            "cli2key",
            "goose",
            r#"{"tokens":{"input":10,"output":20}}"#,
            "",
            None,
        );
        assert!(
            !usage.any(),
            "task output was mistaken for usage: {usage:?}"
        );
    }

    #[test]
    fn opencode_typed_step_usage_remains_available() {
        let usage = token_usage_from_sources(
            "cli2key",
            "opencode",
            r#"{"type":"step_finish","part":{"tokens":{"input":10,"output":20,"cache":{"read":3,"write":2}}}}"#,
            "",
            None,
        );
        assert_eq!(usage.input_tokens, Some(15));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.cached_tokens, Some(3));
        assert_eq!(usage.total_tokens, Some(35));
        let legacy = token_usage_from_sources(
            "cli2key",
            "opencode",
            r#"{"tokens":{"input":"1200","output":44,"cache":{"read":"900"},"total":"1244"}}"#,
            "",
            None,
        );
        assert_eq!(legacy.input_tokens, Some(1200));
        assert_eq!(legacy.output_tokens, Some(44));
        assert_eq!(legacy.cached_tokens, Some(900));
        assert_eq!(legacy.total_tokens, Some(1244));
        let jsonl = token_usage_from_sources(
            "cli2key",
            "opencode",
            "{\"tokens\":{\"input\":10,\"output\":2}}\n{\"tokens\":{\"input\":20,\"output\":3}}\n",
            "",
            None,
        );
        assert_eq!(jsonl.input_tokens, Some(30));
        assert_eq!(jsonl.output_tokens, Some(5));
        assert_eq!(jsonl.total_tokens, Some(35));
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
                9,
                3,
                12,
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
                "claude-code",
                r#"{"type":"result","usage":{"input_tokens":17,"output_tokens":4,"cache_read_input_tokens":2,"total_tokens":21}}"#,
                17,
                4,
                21,
            ),
            (
                "goose",
                r#"{"messages":[],"metadata":{"input_tokens":19,"output_tokens":6,"total_tokens":25}}"#,
                19,
                6,
                25,
            ),
            (
                "cline",
                r#"{"type":"run_result","usage":{"inputTokens":23,"outputTokens":8,"cachedTokens":3,"totalTokens":31}}"#,
                23,
                8,
                31,
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
    fn mini_swe_stdout_does_not_supply_untrusted_token_usage() {
        let usage = token_usage_from_sources(
            "cli2key",
            "mini-swe",
            r#"{"usage":{"input_tokens":182398,"output_tokens":4189,"total_tokens":116}}"#,
            "",
            None,
        );
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.total_tokens, None);
    }

    #[test]
    fn parses_provider_usage_metadata_and_agent_raw_usage_shapes() {
        let cases = [
            (
                "workbuddy",
                r#"[{"rawUsage":{"inputTokens":21,"outputTokens":5,"totalTokens":26,"inputTokensDetails":{"cached_tokens":3}}}]"#,
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
    fn process_tree_includes_nested_helpers_but_not_unrelated_processes() {
        let children = BTreeMap::from([(10, vec![11, 12]), (11, vec![13]), (99, vec![100])]);
        assert_eq!(descendant_pids(10, &children), vec![10, 11, 12, 13]);
    }

    #[test]
    fn parses_process_parent_from_proc_stat_with_spaces_in_command() {
        assert_eq!(parse_process_parent("42 (agent helper) S 7 8 9"), Some(7));
    }
}
