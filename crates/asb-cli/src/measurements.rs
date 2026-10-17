//! Optional, file-backed measurements for a benchmark run.
//!
//! Measurement collection is deliberately opt-in.  The default run path does
//! not inspect the host or start any auxiliary process.  When enabled, every
//! result is written as bounded JSON so later processing never has to parse
//! terminal output.

use crate::collectors;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_CAPTURE: usize = 256 * 1024;
const MAX_RUNS: usize = 10_000;
const FLAMEGRAPH_TIMEOUT: Duration = Duration::from_secs(60);

/// A bounded, target-PID-scoped bpftrace program. Profiles use only stable
/// kernel tracepoint families where possible; users can still supply a
/// runtime-specific program through ASB_BPFTRACE_PROGRAMS.
#[derive(Clone, Copy)]
struct BpftraceProfile {
    name: &'static str,
    description: &'static str,
    program: &'static str,
}

const BPFTRACE_PROFILES: &[BpftraceProfile] = &[
    BpftraceProfile {
        name: "sched-latency",
        description: "wakeup-to-run latency for agent tasks",
        program: include_str!("bpftrace_profiles/sched_latency.bt"),
    },
    BpftraceProfile {
        name: "cpu-migrations",
        description: "agent task migrations between CPUs",
        program: include_str!("bpftrace_profiles/cpu_migrations.bt"),
    },
    BpftraceProfile {
        name: "futex-wait",
        description: "futex wait count and duration for agent threads",
        program: include_str!("bpftrace_profiles/futex_wait.bt"),
    },
    BpftraceProfile {
        name: "syscall-rate",
        description: "aggregate system-call pressure from the agent process",
        program: include_str!("bpftrace_profiles/syscall_rate.bt"),
    },
    BpftraceProfile {
        name: "lock-contention",
        description: "kernel lock contention for agent threads when supported",
        program: include_str!("bpftrace_profiles/lock_contention.bt"),
    },
];

/// Names accepted by `bpftrace:PROFILE` collector selections and
/// `ASB_BPFTRACE_PROFILES=PROFILE,...`.
pub fn bpftrace_profile_names() -> impl Iterator<Item = &'static str> {
    BPFTRACE_PROFILES.iter().map(|profile| profile.name)
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub source: Option<String>,
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
    pub schema_version: u32,
    pub run_id: String,
    pub status: &'a str,
    pub success: bool,
    pub success_rate: f64,
    pub agent: &'a str,
    pub workload: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub elapsed_ms: u128,
    pub exit_code: Option<i32>,
    pub token_usage: TokenUsage,
    pub steps: u64,
    pub started_unix_ms: u128,
}

#[derive(Debug, Serialize)]
struct SystemInfo {
    schema_version: u32,
    collected_unix_ms: u128,
    os: &'static str,
    architecture: &'static str,
    logical_cpus: usize,
    kernel_release: Option<String>,
    memory_total_kib: Option<u64>,
}

#[derive(Debug, Serialize)]
struct CollectorResult {
    schema_version: u32,
    collector: String,
    status: String,
    exit_code: Option<i32>,
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
    /// Timestamp in milliseconds for each Perfetto series sample.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    series_timestamps: BTreeMap<String, Vec<f64>>,
}

struct Collector {
    name: String,
    child: Child,
    started: Instant,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
}

/// A measurement session owns all optional collector processes for one run.
pub struct Session {
    dir: PathBuf,
    tool_dir: Option<PathBuf>,
    started_unix_ms: u128,
    collectors: Vec<Collector>,
    perf_data: Option<PathBuf>,
    perf_enabled: bool,
    perfetto_trace: Option<PathBuf>,
    perfetto_enabled: bool,
    trace_marker: Option<PathBuf>,
    tracepoint_count: u64,
    perfetto_metrics: BTreeMap<String, f64>,
    bpftrace_profiles: Vec<BpftraceProfile>,
    mpstat_enabled: bool,
    iostat_enabled: bool,
    bpftrace_enabled: bool,
    sudo_collectors: BTreeSet<String>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // An early runner error (for example, an incomplete replay cassette)
        // must not orphan process-scoped collectors. `finish` takes ownership
        // of the collector list, so this only handles paths that return before
        // normal finalization.
        for mut collector in self.collectors.drain(..) {
            let _ = collector.child.kill();
            let _ = collector.child.wait();
        }
    }
}

impl Session {
    pub fn started_unix_ms(&self) -> u128 {
        self.started_unix_ms
    }
    /// Start a session and write the host description.  Collectors are enabled
    /// only by explicit ASB_MEASURE_* variables, keeping CI deterministic.
    /// Start with the stable collector names from a generated configuration.
    /// An empty list means environment-only opt-in, useful for tests and
    /// hand-written configurations.
    pub fn start_with_collectors(
        dir: &Path,
        selected: &[String],
        tool_dir: Option<&Path>,
    ) -> Result<Self, String> {
        fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create measurements directory: {e}"))?;
        // Privilege and tracefs checks must complete before any agent process
        // is prepared. This prevents a partially measured benchmark run.
        let sudo_collectors = preflight_collectors(dir, selected, tool_dir)?;
        let started_unix_ms = unix_ms();
        let info = SystemInfo {
            schema_version: 1,
            collected_unix_ms: started_unix_ms,
            os: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
            logical_cpus: std::thread::available_parallelism()
                .map_or(1, std::num::NonZeroUsize::get),
            kernel_release: read_trimmed("/proc/sys/kernel/osrelease"),
            memory_total_kib: read_mem_total(),
        };
        write_json(&dir.join("system.json"), &info)?;

        let session = Self {
            dir: dir.to_path_buf(),
            tool_dir: tool_dir.map(Path::to_path_buf),
            started_unix_ms,
            collectors: Vec::new(),
            perf_data: None,
            perf_enabled: selected_or_env("perf", "ASB_MEASURE_PERF", selected),
            perfetto_trace: None,
            perfetto_enabled: selected_or_env("perfetto", "ASB_MEASURE_PERFETTO", selected),
            trace_marker: None,
            tracepoint_count: 0,
            perfetto_metrics: BTreeMap::new(),
            bpftrace_profiles: selected_bpftrace_profiles(selected)?,
            mpstat_enabled: selected_or_env("mpstat", "ASB_MEASURE_MPSTAT", selected),
            iostat_enabled: selected_or_env("iostat", "ASB_MEASURE_IOSTAT", selected),
            bpftrace_enabled: selected_or_env("bpftrace", "ASB_MEASURE_BPFTRACE", selected),
            sudo_collectors,
        };
        Ok(session)
    }

    /// Emit a fixed-name userspace Perfetto/ftrace instant. Names are constant
    /// ASB lifecycle markers only; configurations, prompts, credentials, and
    /// provider responses are never written to the kernel trace buffer.
    pub fn trace_instant(&mut self, name: &str) {
        self.write_trace_marker(&format!("I|{}|{name}", std::process::id()));
    }

    /// Begin a fixed-name ASB lifecycle span in the Perfetto timeline.
    pub fn trace_begin(&mut self, name: &str) {
        self.write_trace_marker(&format!("B|{}|{name}", std::process::id()));
    }

    /// End the most recently opened ASB lifecycle span.
    pub fn trace_end(&mut self) {
        self.write_trace_marker("E");
    }

    /// Attach perf to the selected child after it has been spawned.
    pub fn attach_pid(&mut self, pid: u32) -> Result<(), String> {
        // PID-sensitive collectors start only after the agent exists. This
        // prevents scheduler/process traces and process statistics from
        // including unrelated agents running on the host.
        self.start_perfetto(pid)?;
        self.start_process_snapshots(pid)?;
        if self.bpftrace_enabled && std::env::var_os("ASB_BPFTRACE_PROGRAMS").is_some() {
            self.start_bpftrace(pid)?;
        }
        if !self.perf_enabled {
            self.start_bpftrace_profiles(pid)?;
            return Ok(());
        }
        let data = self.dir.join("perf.data");
        let stderr_path = self.dir.join("perf.stderr");
        let output =
            File::create(&stderr_path).map_err(|e| format!("cannot create perf output: {e}"))?;
        let stdout_path = self.dir.join("perf.stdout");
        let stdout = File::create(&stdout_path)
            .map_err(|e| format!("cannot create perf standard output: {e}"))?;
        let child = match self
            .command("perf")
            // Keep perf data bounded enough for the offline flamegraph
            // renderer while retaining process-scoped call samples.
            .args(["record", "-g", "-F", "19", "-p"])
            .arg(pid.to_string())
            .args(["-o"])
            .arg(&data)
            .stderr(Stdio::from(output))
            .stdout(Stdio::from(stdout))
            .spawn()
        {
            Ok(child) => child,
            Err(error) if !env_true("ASB_MEASURE_PERF") => {
                write_unavailable(&self.dir.join("perf.json"), "perf", error.to_string())?;
                self.start_bpftrace_profiles(pid)?;
                return Ok(());
            }
            Err(error) => {
                return Err(format!(
                    "cannot start perf (disable with ASB_MEASURE_PERF=0): {error}"
                ));
            }
        };
        self.perf_data = Some(data);
        self.collectors.push(Collector {
            name: "perf".into(),
            child,
            started: Instant::now(),
            stdout_path,
            stderr_path,
        });
        let stat_stdout_path = self.dir.join("perf-stat.stdout");
        let stat_stderr_path = self.dir.join("perf-stat.stderr");
        let stat_stdout = File::create(&stat_stdout_path)
            .map_err(|e| format!("cannot create perf stat output: {e}"))?;
        let stat_stderr = File::create(&stat_stderr_path)
            .map_err(|e| format!("cannot create perf stat error output: {e}"))?;
        let interval_ms = measurement_interval_ms().to_string();
        let stat = self
            .command("perf")
            .args([
                "stat",
                "-I",
                &interval_ms,
                "-x,",
                "-e",
                "task-clock,context-switches,cpu-migrations,page-faults,cycles,instructions",
                "-p",
            ])
            .arg(pid.to_string())
            .stdout(Stdio::from(stat_stdout))
            .stderr(Stdio::from(stat_stderr))
            .spawn();
        match stat {
            Ok(child) => self.collectors.push(Collector {
                name: "perf-stat".into(),
                child,
                started: Instant::now(),
                stdout_path: stat_stdout_path,
                stderr_path: stat_stderr_path,
            }),
            Err(error) => write_unavailable(
                &self.dir.join("perf-stat.json"),
                "perf-stat",
                error.to_string(),
            )?,
        }
        self.start_bpftrace_profiles(pid)?;
        Ok(())
    }

    /// Finish collectors and write the run record and aggregate summary.
    pub fn finish<'a>(mut self, measurement: RunMeasurement<'a>) -> Result<(), String> {
        let collectors = std::mem::take(&mut self.collectors);
        for mut collector in collectors {
            let status = match collector.child.try_wait() {
                Ok(Some(status)) => status,
                Ok(None) => {
                    // Tracebox writes its protobuf trace and bpftrace writes its map
                    // summaries on an orderly interrupt. Do not SIGKILL either collector:
                    // a short benchmark would otherwise lose the very artifacts ASB is
                    // meant to report.
                    stop_collector(
                        &mut collector.child,
                        collector.name == "perf"
                            || collector.name == "perfetto"
                            || collector.name.starts_with("bpftrace-"),
                    )
                    .map_err(|e| format!("cannot finish {} collector: {e}", collector.name))?
                }
                Err(error) => {
                    return Err(format!(
                        "cannot inspect {} collector: {error}",
                        collector.name
                    ));
                }
            };
            let stdout = read_bounded(&collector.stdout_path);
            let stderr = read_bounded(&collector.stderr_path);
            let _ = fs::remove_file(&collector.stdout_path);
            let _ = fs::remove_file(&collector.stderr_path);
            let stopped_by_runner = status.code().is_none();
            let metrics_source = if collector.name == "perf-stat" {
                &stderr
            } else {
                &stdout
            };
            let mut metrics = parse_collector_metrics(&collector.name, metrics_source);
            let series = parse_collector_series(&collector.name, metrics_source);
            let (series, series_timestamps) = if collector.name == "perfetto" {
                let (mut trace_series, timestamps) = self.perfetto_trace_series();
                trace_series.extend(series);
                (trace_series, timestamps)
            } else {
                (series, BTreeMap::new())
            };
            if collector.name == "perfetto" {
                self.perfetto_metrics = self.perfetto_trace_metrics();
                metrics.extend(self.perfetto_metrics.clone());
            }
            let artifact = match collector.name.as_str() {
                "perf" => self
                    .perf_data
                    .as_ref()
                    .filter(|path| path.is_file())
                    .map(|path| file_name(path)),
                "perfetto" => self
                    .perfetto_trace
                    .as_ref()
                    .filter(|path| path.is_file())
                    .map(|path| file_name(path)),
                _ => None,
            };
            let collector_status = if status.success()
                || (stopped_by_runner && collector.name != "perf" && !stdout.is_empty())
                || (stopped_by_runner && collector.name == "perf-stat" && !stderr.is_empty())
                || (stopped_by_runner
                    && collector_artifact_exists(
                        &collector.name,
                        self.perf_data.as_deref(),
                        self.perfetto_trace.as_deref(),
                    )) {
                "completed"
            } else {
                "failed"
            };
            let collector_status = if collector.name == "perfetto"
                && (stderr.contains("Failed to create FtraceController")
                    || stderr.contains("FtraceController"))
            {
                "unavailable"
            } else {
                collector_status
            };
            if let Some(profile) = collector.name.strip_prefix("bpftrace-") {
                write_bpftrace_profile_svg(&self.dir, profile, collector_status, &metrics)?;
            }
            let result = CollectorResult {
                schema_version: 1,
                collector: collector.name.clone(),
                status: collector_status.into(),
                exit_code: status.code(),
                elapsed_ms: collector.started.elapsed().as_millis(),
                stdout,
                stderr,
                artifact,
                metrics,
                series,
                series_timestamps,
            };
            write_json(&self.dir.join(format!("{}.json", collector.name)), &result)?;
        }
        write_json(&self.dir.join("run.json"), &measurement)?;
        self.write_flamegraph()?;
        if self.perfetto_enabled {
            write_json(
                &self.dir.join("perfetto-tracepoints.json"),
                &serde_json::json!({
                    "schema_version": 1,
                    "collector": "perfetto",
                    "tracepoint_count": self.tracepoint_count,
                    "trace_marker": self.trace_marker.as_ref().map(|path| path.display().to_string()),
                    "trace": self.perfetto_trace.as_ref().map(|path| file_name(path)),
                }),
            )?;
            write_perfetto_metric_svgs(
                &self.dir,
                self.perfetto_trace.as_deref(),
                self.tracepoint_count,
                &self.perfetto_metrics,
            )?;
        }
        update_summary(&self.dir, &measurement)
    }

    fn start_perfetto(&mut self, pid: u32) -> Result<(), String> {
        if !self.perfetto_enabled {
            return Ok(());
        }
        let config = self.dir.join("perfetto-scheduling.pbtx");
        fs::write(&config, perfetto_config(Some(pid)))
            .map_err(|error| format!("cannot write Perfetto trace configuration: {error}"))?;
        let trace = self.dir.join("perfetto-scheduling.perfetto-trace");
        // A sudo-launched Tracebox otherwise creates a root-only 0600 trace.
        // Precreating the artifact keeps its ownership with the benchmark user.
        File::create(&trace)
            .map_err(|error| format!("cannot create Perfetto trace artifact: {error}"))?;
        let stdout_path = self.dir.join("perfetto.stdout");
        let stderr_path = self.dir.join("perfetto.stderr");
        let child = self
            .command("perfetto")
            .args(["--txt", "-c"])
            .arg(&config)
            .args(["-o"])
            .arg(&trace)
            .stdout(Stdio::from(File::create(&stdout_path).map_err(
                |error| format!("cannot create Perfetto output: {error}"),
            )?))
            .stderr(Stdio::from(File::create(&stderr_path).map_err(
                |error| format!("cannot create Perfetto error output: {error}"),
            )?))
            .spawn();
        match child {
            Ok(child) => {
                self.perfetto_trace = Some(trace);
                self.trace_marker = trace_marker_path();
                self.collectors.push(Collector {
                    name: "perfetto".into(),
                    child,
                    started: Instant::now(),
                    stdout_path,
                    stderr_path,
                });
            }
            Err(error) if !env_true("ASB_MEASURE_PERFETTO") => write_unavailable(
                &self.dir.join("perfetto.json"),
                "perfetto",
                format!("{error}; install it with `asb tool install perfetto` in this project"),
            )?,
            Err(error) => {
                return Err(format!(
                    "cannot start Perfetto Tracebox (disable with ASB_MEASURE_PERFETTO=0): {error}"
                ));
            }
        }
        Ok(())
    }

    fn write_trace_marker(&mut self, event: &str) {
        let Some(path) = self.trace_marker.as_ref() else {
            return;
        };
        if OpenOptions::new()
            .write(true)
            .open(path)
            .and_then(|mut file| file.write_all(event.as_bytes()))
            .is_ok()
        {
            self.tracepoint_count = self.tracepoint_count.saturating_add(1);
        }
    }

    /// Summarize the native trace with the installer-provided trace processor.
    /// The trace itself remains the authoritative artifact; these aggregates
    /// make scheduler activity comparable in ASB's offline SVG and HTML views.
    fn perfetto_trace_metrics(&self) -> BTreeMap<String, f64> {
        let mut metrics = BTreeMap::new();
        let Some(trace) = self.perfetto_trace.as_ref().filter(|path| path.is_file()) else {
            return metrics;
        };
        metrics.insert(
            "perfetto.trace_bytes".into(),
            trace.metadata().map_or(0, |metadata| metadata.len()) as f64,
        );
        metrics.insert("perfetto.tracepoints".into(), self.tracepoint_count as f64);
        let Some(tool_dir) = self.tool_dir.as_ref() else {
            return metrics;
        };
        let processor = tool_dir.join("perfetto/bin/trace_processor_shell");
        if !processor.is_file() {
            return metrics;
        }
        for (metric, query) in [
            ("perfetto.scheduler_events", "SELECT COUNT(*) FROM sched"),
            ("perfetto.processes", "SELECT COUNT(*) FROM process"),
            ("perfetto.threads", "SELECT COUNT(*) FROM thread"),
        ] {
            let Ok(output) = Command::new(&processor)
                .args(["query"])
                .arg(trace)
                .arg(query)
                .output()
            else {
                continue;
            };
            if output.status.success() {
                if let Some(value) = bounded(&output.stdout).lines().rev().find_map(parse_float) {
                    metrics.insert(metric.into(), value);
                }
            }
        }
        metrics
    }

    /// Extract timestamped scheduler samples directly from the Perfetto trace.
    /// The timestamp column is retained in milliseconds so comparison graphs
    /// use trace time rather than synthetic point indexes.
    fn perfetto_trace_series(&self) -> (BTreeMap<String, Vec<f64>>, BTreeMap<String, Vec<f64>>) {
        let mut series = BTreeMap::new();
        let mut timestamps = BTreeMap::new();
        let Some(trace) = self.perfetto_trace.as_ref().filter(|path| path.is_file()) else {
            return (series, timestamps);
        };
        let Some(tool_dir) = self.tool_dir.as_ref() else {
            return (series, timestamps);
        };
        let processor = tool_dir.join("perfetto/bin/trace_processor_shell");
        if !processor.is_file() {
            return (series, timestamps);
        }
        for (name, query) in [
            (
                "perfetto.scheduler_events_per_s",
                "SELECT ts/1000000000 AS timestamp_s, COUNT(*) AS value FROM sched GROUP BY timestamp_s ORDER BY timestamp_s",
            ),
            (
                "perfetto.scheduler_duration_ms_per_s",
                "SELECT ts/1000000000 AS timestamp_s, SUM(dur)/1000000.0 AS value FROM sched GROUP BY timestamp_s ORDER BY timestamp_s",
            ),
        ] {
            let Ok(output) = Command::new(&processor)
                .args(["query"])
                .arg(trace)
                .arg(query)
                .output()
            else {
                continue;
            };
            if !output.status.success() {
                continue;
            }
            let (sample_timestamps, values) =
                parse_perfetto_series_output(&bounded(&output.stdout));
            if values.len() >= 2 {
                timestamps.insert(name.into(), sample_timestamps);
                series.insert(name.into(), values);
            }
        }
        (series, timestamps)
    }

    fn start_snapshot(
        &mut self,
        name: &str,
        variable: &str,
        args: &[&str],
        enabled: bool,
        command_name: &str,
    ) -> Result<(), String> {
        if !enabled {
            return Ok(());
        }
        let stdout_path = self.dir.join(format!("{name}.stdout"));
        let stderr_path = self.dir.join(format!("{name}.stderr"));
        let child = match self
            .command(command_name)
            .args(args)
            .stdout(Stdio::from(
                File::create(&stdout_path)
                    .map_err(|e| format!("cannot create {name} output: {e}"))?,
            ))
            .stderr(Stdio::from(File::create(&stderr_path).map_err(|e| {
                format!("cannot create {name} error output: {e}")
            })?))
            .spawn()
        {
            Ok(child) => child,
            Err(error) if !env_true(variable) => {
                write_unavailable(
                    &self.dir.join(format!("{name}.json")),
                    name,
                    error.to_string(),
                )?;
                return Ok(());
            }
            Err(error) => {
                return Err(format!(
                    "cannot start {name}; install it or disable {variable}=1: {error}"
                ));
            }
        };
        self.collectors.push(Collector {
            name: name.into(),
            child,
            started: Instant::now(),
            stdout_path,
            stderr_path,
        });
        Ok(())
    }

    fn start_process_snapshots(&mut self, pid: u32) -> Result<(), String> {
        let interval = pidstat_interval_seconds();
        self.start_snapshot(
            "mpstat",
            "ASB_MEASURE_MPSTAT",
            &["-u", "-T", "ALL", "-p", &pid.to_string(), &interval],
            self.mpstat_enabled,
            "pidstat",
        )?;
        self.start_snapshot(
            "iostat",
            "ASB_MEASURE_IOSTAT",
            &["-d", "-T", "ALL", "-p", &pid.to_string(), &interval],
            self.iostat_enabled,
            "pidstat",
        )?;
        Ok(())
    }

    fn start_bpftrace(&mut self, pid: u32) -> Result<(), String> {
        let specs = std::env::var("ASB_BPFTRACE_PROGRAMS")
            .map_err(|_| "ASB_MEASURE_BPFTRACE=1 requires ASB_BPFTRACE_PROGRAMS".to_owned())?;
        for (index, spec) in specs
            .split(';')
            .filter(|s| !s.trim().is_empty())
            .enumerate()
        {
            let (program, params) = spec.split_once('?').map_or((spec, ""), |parts| parts);
            let mut command = self.command("bpftrace");
            command.args(["-p", &pid.to_string()]);
            command.arg(program);
            for parameter in params.split('&').filter(|s| !s.is_empty()) {
                let (key, value) = parameter.split_once('=').ok_or_else(|| {
                    format!("invalid bpftrace parameter `{parameter}`; use PROGRAM?name=value")
                })?;
                if key.is_empty() || value.is_empty() {
                    return Err(format!("invalid bpftrace parameter `{parameter}`"));
                }
                command.args(["-D", &format!("{key}={value}")]);
            }
            let stdout_path = self.dir.join(format!("bpftrace-{index}.stdout"));
            let stderr_path = self.dir.join(format!("bpftrace-{index}.stderr"));
            let child = match command
                .stdout(Stdio::from(
                    File::create(&stdout_path)
                        .map_err(|e| format!("cannot create bpftrace output: {e}"))?,
                ))
                .stderr(Stdio::from(File::create(&stderr_path).map_err(|e| {
                    format!("cannot create bpftrace error output: {e}")
                })?))
                .spawn()
            {
                Ok(child) => child,
                Err(error) if !env_true("ASB_MEASURE_BPFTRACE") => {
                    write_unavailable(
                        &self.dir.join(format!("bpftrace-{index}.json")),
                        &format!("bpftrace-{index}"),
                        error.to_string(),
                    )?;
                    continue;
                }
                Err(error) => {
                    return Err(format!("cannot start bpftrace program {program}: {error}"));
                }
            };
            self.collectors.push(Collector {
                name: format!("bpftrace-{index}"),
                child,
                started: Instant::now(),
                stdout_path,
                stderr_path,
            });
        }
        Ok(())
    }

    /// Start each selected built-in profile after the agent PID is known. This
    /// keeps all probes scoped to the benchmarked agent rather than the host.
    fn start_bpftrace_profiles(&mut self, pid: u32) -> Result<(), String> {
        for profile in &self.bpftrace_profiles {
            let name = format!("bpftrace-{}", profile.name);
            let stdout_path = self.dir.join(format!("{name}.stdout"));
            let stderr_path = self.dir.join(format!("{name}.stderr"));
            let stdout = File::create(&stdout_path)
                .map_err(|error| format!("cannot create {name} output: {error}"))?;
            let stderr = File::create(&stderr_path)
                .map_err(|error| format!("cannot create {name} error output: {error}"))?;
            let child = self
                .command("bpftrace")
                .args(["-e", profile.program])
                .arg(pid.to_string())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .spawn();
            match child {
                Ok(child) => self.collectors.push(Collector {
                    name,
                    child,
                    started: Instant::now(),
                    stdout_path,
                    stderr_path,
                }),
                Err(error) if !env_true("ASB_MEASURE_BPFTRACE") => write_unavailable(
                    &self.dir.join(format!("bpftrace-{}.json", profile.name)),
                    &format!("bpftrace-{}", profile.name),
                    format!("{}; profile: {}", error, profile.description),
                )?,
                Err(error) => {
                    return Err(format!(
                        "cannot start bpftrace profile {}: {error}",
                        profile.name
                    ));
                }
            }
        }
        Ok(())
    }

    fn write_flamegraph(&self) -> Result<(), String> {
        let Some(data) = self.perf_data.as_ref() else {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "perf recording did not start; check perf.json for the host error",
            );
        };
        if !data.is_file() || data.metadata().map_or(true, |metadata| metadata.len() == 0) {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "perf produced no samples; check perf.json and host perf permissions",
            );
        }
        let collapse = std::env::var_os("ASB_FLAMEGRAPH_STACKCOLLAPSE").or_else(|| {
            self.tool_dir.as_ref().and_then(|dir| {
                [
                    dir.join("flamegraph/bin/stackcollapse-perf.pl"),
                    dir.join("bin/stackcollapse-perf.pl"),
                ]
                .into_iter()
                .find(|path| path.is_file())
                .map(|path| path.into_os_string())
            })
        });
        let render = std::env::var_os("ASB_FLAMEGRAPH_RENDER").or_else(|| {
            self.tool_dir.as_ref().and_then(|dir| {
                [
                    dir.join("flamegraph/bin/flamegraph"),
                    dir.join("bin/flamegraph"),
                ]
                .into_iter()
                .find(|path| path.is_file())
                .map(|path| path.into_os_string())
            })
        });
        let (Some(collapse), Some(render)) = (collapse, render) else {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "install stackcollapse-perf.pl and flamegraph in the project tool directory",
            );
        };
        let mut script = self
            .command("perf")
            .args(["script", "-i"])
            .arg(data)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start perf script for flamegraph: {e}"))?;
        let script_stdout = script
            .stdout
            .take()
            .ok_or_else(|| "perf script stdout was not piped".to_owned())?;
        let folded_path = self.dir.join("flamegraph.folded");
        let mut collapsed = Command::new(collapse)
            .stdin(Stdio::from(script_stdout))
            .stdout(Stdio::from(File::create(&folded_path).map_err(|e| {
                format!("cannot create flamegraph folded output: {e}")
            })?))
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot start flamegraph stack collapse script: {e}"))?;
        if !wait_bounded(&mut script, FLAMEGRAPH_TIMEOUT)? {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "perf script exceeded the 60 second flamegraph deadline or failed",
            );
        }
        if !wait_bounded(&mut collapsed, FLAMEGRAPH_TIMEOUT)? {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "the stack collapse script exceeded the 60 second flamegraph deadline or failed",
            );
        }
        let mut render = Command::new(render);
        render
            .stdin(Stdio::from(File::open(&folded_path).map_err(|e| {
                format!("cannot open collapsed flamegraph output: {e}")
            })?))
            .stdout(Stdio::from(
                File::create(self.dir.join("flamegraph.svg"))
                    .map_err(|e| format!("cannot create flamegraph.svg: {e}"))?,
            ));
        let mut render = render
            .spawn()
            .map_err(|e| format!("cannot start flamegraph renderer: {e}"))?;
        if !wait_bounded(&mut render, FLAMEGRAPH_TIMEOUT)? {
            return write_flamegraph_status(
                &self.dir,
                "unavailable",
                None,
                "the flamegraph renderer exceeded the 60 second deadline or failed",
            );
        }
        write_flamegraph_status(&self.dir, "completed", Some("flamegraph.svg"), "")
    }

    fn command(&self, name: &str) -> Command {
        let executable = self.executable(name);
        if self.sudo_collectors.contains(name) {
            let mut command = Command::new("sudo");
            command.args(["-n", "--"]).arg(executable);
            return command;
        }
        Command::new(executable)
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

fn parse_perfetto_series_output(output: &str) -> (Vec<f64>, Vec<f64>) {
    let mut timestamps = Vec::new();
    let mut values = Vec::new();
    for line in output.lines() {
        let fields = line
            .split(',')
            .map(|field| field.trim().trim_matches('"'))
            .collect::<Vec<_>>();
        if fields.len() < 2 {
            continue;
        }
        let (Some(timestamp_s), Some(value)) = (parse_float(fields[0]), parse_float(fields[1]))
        else {
            continue;
        };
        timestamps.push(timestamp_s * 1_000.0);
        values.push(value);
        if values.len() >= 2_048 {
            break;
        }
    }
    (timestamps, values)
}

/// Extract token usage from environment conventions, agent output, and an
/// optional llmtape cassette. Providers and agents use different names and
/// response shapes, so all sources are normalized into one stable record.
pub fn token_usage_from_sources(
    provider: &str,
    agent: &str,
    stdout: &str,
    stderr: &str,
    cassette: Option<&Path>,
) -> TokenUsage {
    let mut usage = usage_from_environment(provider, agent);
    let output_usage = usage_from_text(stdout).merge(usage_from_text(stderr));
    usage = usage.merge_with_source(output_usage, "agent-output");
    if let Some(path) = cassette {
        if let Ok(text) = read_bounded_text(path, 16 * 1024 * 1024) {
            usage = usage.merge_with_source(usage_from_text(&text), "cassette");
        }
    }
    usage.with_derived_total()
}

/// Read a bounded machine-readable agent artifact (for example mini-SWE's
/// trajectory) and normalize any usage events it contains.
pub fn token_usage_from_path(path: &Path) -> TokenUsage {
    read_bounded_text(path, 16 * 1024 * 1024)
        .map(|text| usage_from_text(&text).with_derived_total())
        .unwrap_or_default()
}

/// Count bounded agent actions/turns from structured or human-readable output.
/// The count is deliberately provider-neutral: agents may expose `step`,
/// `turn`, tool, command, or iteration events under different JSON shapes.
pub fn steps_from_sources(agent: &str, stdout: &str, stderr: &str) -> u64 {
    let mut count = 0_u64;
    let mut saw_structured = false;
    for line in stdout.lines().chain(stderr.lines()) {
        let lower = line.to_ascii_lowercase();
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            saw_structured = true;
            count = count.saturating_add(structured_step_count(&value));
            continue;
        }
        for value in embedded_json_values(line) {
            saw_structured = true;
            count = count.saturating_add(structured_step_count(&value));
        }
        if !saw_structured
            && ["step", "turn", "iteration", "tool call", "command"]
                .iter()
                .any(|marker| lower.contains(marker))
        {
            count = count.saturating_add(1);
        }
    }
    // A successful agent that emitted no event still consumed one execution
    // step; this keeps per-agent comparisons meaningful without guessing token
    // usage or inventing additional actions.
    if count == 0 && !stdout.trim().is_empty() && !agent.is_empty() {
        1
    } else {
        count
    }
}

fn structured_step_count(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::Array(values) => values.iter().map(structured_step_count).sum(),
        serde_json::Value::Object(object) => {
            let event_count = object
                .get("type")
                .and_then(serde_json::Value::as_str)
                .filter(|event| {
                    let event = event.to_ascii_lowercase();
                    ["step", "turn", "iteration", "tool", "command", "action"]
                        .iter()
                        .any(|marker| event.contains(marker))
                })
                .map_or(0, |_| 1);
            if event_count > 0 {
                return event_count;
            }
            let mut count = 0;
            for (key, child) in object {
                let lower = key.to_ascii_lowercase();
                if [
                    "step",
                    "turn",
                    "iteration",
                    "tool_call",
                    "toolcall",
                    "command",
                    "action",
                ]
                .iter()
                .any(|marker| lower.contains(marker))
                    && !child.is_object()
                {
                    count += 1;
                }
                count += structured_step_count(child);
            }
            count
        }
        _ => 0,
    }
}

impl TokenUsage {
    fn merge(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.or(other.input_tokens),
            output_tokens: self.output_tokens.or(other.output_tokens),
            cached_tokens: self.cached_tokens.or(other.cached_tokens),
            // Agent artifacts occasionally report a partial or stale total
            // (mini-SWE has emitted a small per-step total).  Keep the
            // largest observed total until the canonical input+output total
            // is derived below instead of letting a later artifact overwrite
            // the complete run.
            total_tokens: match (self.total_tokens, other.total_tokens) {
                (Some(left), Some(right)) => Some(left.max(right)),
                (Some(value), None) | (None, Some(value)) => Some(value),
                (None, None) => None,
            },
            source: self.source.or(other.source),
        }
    }

    fn add(self, other: Self) -> Self {
        Self {
            input_tokens: add_values(self.input_tokens, other.input_tokens),
            output_tokens: add_values(self.output_tokens, other.output_tokens),
            cached_tokens: add_values(self.cached_tokens, other.cached_tokens),
            total_tokens: add_values(self.total_tokens, other.total_tokens),
            source: self.source.or(other.source),
        }
    }

    pub(crate) fn merge_with_source(self, other: Self, source: &str) -> Self {
        let found = other.any();
        let mut merged = self.merge(other);
        if found {
            merged.source = Some(match merged.source.take() {
                Some(existing) if existing != source => format!("{existing}+{source}"),
                _ => source.into(),
            });
        }
        merged
    }

    pub(crate) fn with_derived_total(mut self) -> Self {
        // Cached input is a subset of input tokens, so total is input+output
        // rather than input+output+cached.  Prefer this canonical value over
        // provider-specific totals whenever both components are available.
        if let Some((input, output)) = self.input_tokens.zip(self.output_tokens) {
            self.total_tokens = Some(input.saturating_add(output));
        }
        self
    }

    fn any(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cached_tokens.is_some()
            || self.total_tokens.is_some()
    }
}

fn usage_from_environment(provider: &str, agent: &str) -> TokenUsage {
    let mut prefixes = vec!["ASB".to_owned()];
    for value in [provider, agent] {
        let normalized = value
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>();
        if !normalized.is_empty() && !prefixes.iter().any(|prefix| prefix == &normalized) {
            prefixes.push(normalized);
        }
    }
    let lookup = |suffixes: &[&str]| {
        prefixes.iter().find_map(|prefix| {
            suffixes
                .iter()
                .find_map(|suffix| env_u64(&format!("{prefix}_{suffix}")))
        })
    };
    let input = lookup(&["INPUT_TOKENS", "PROMPT_TOKENS", "INPUT_TOKEN_COUNT"]);
    let output = lookup(&["OUTPUT_TOKENS", "COMPLETION_TOKENS", "OUTPUT_TOKEN_COUNT"]);
    let cached = lookup(&[
        "CACHED_TOKENS",
        "CACHE_READ_INPUT_TOKENS",
        "PROMPT_CACHED_TOKENS",
    ]);
    let total = lookup(&["TOTAL_TOKENS", "TOKEN_COUNT"]);
    let usage = TokenUsage {
        input_tokens: input,
        output_tokens: output,
        cached_tokens: cached,
        total_tokens: total,
        source: None,
    };
    if usage.any() {
        TokenUsage {
            source: Some("environment".into()),
            ..usage
        }
    } else {
        usage
    }
}

impl Default for TokenUsage {
    fn default() -> Self {
        Self {
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            total_tokens: None,
            source: None,
        }
    }
}

fn usage_from_text(text: &str) -> TokenUsage {
    let mut usage = TokenUsage::default();
    if text.contains('\n')
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(text.trim())
    {
        usage = usage.add(usage_from_json(&value));
    }
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            usage = usage.add(usage_from_json(&value));
            continue;
        }
        for value in embedded_json_values(line) {
            usage = usage.add(usage_from_json(&value));
        }
        if line.to_ascii_lowercase().contains("tokens used")
            && let Some(value) = lines
                .peek()
                .and_then(|next| next.trim().replace(',', "").parse::<u64>().ok())
        {
            usage.total_tokens = Some(value);
            let _ = lines.next();
            continue;
        }
        // JSON fields are already accounted for above.  Avoid treating their
        // `"name":number` members as a second key/value event.
        if !line.contains(['{', '}']) {
            usage = usage.add(usage_from_key_values(line));
        }
    }
    usage
}

/// Extract complete JSON objects from agent prefixes such as `event: {...}`.
/// Agents frequently mix human-readable progress text with JSON usage events;
/// requiring a whole line to be JSON made token accounting work only for the
/// agent that emits a pure JSON stream.
fn embedded_json_values(text: &str) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    let mut start = None;
    let mut depth = 0_u32;
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' if depth == 0 => {
                start = Some(index);
                depth = 1;
            }
            '{' if depth > 0 => depth += 1,
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = start.take()
                        && let Ok(value) = serde_json::from_str(&text[start..=index])
                    {
                        values.push(value);
                    }
                }
            }
            _ => {}
        }
    }
    values
}

fn usage_from_json(value: &serde_json::Value) -> TokenUsage {
    let mut usage = TokenUsage::default();
    if let Some(object) = value.as_object() {
        let short_usage = object.contains_key("total")
            || (object.contains_key("input") && object.contains_key("output"))
            || object.contains_key("cached")
            || object.contains_key("cacheRead");
        usage.input_tokens = find_number(
            object,
            &[
                "input_tokens",
                "prompt_tokens",
                "inputTokens",
                "promptTokens",
                "input_token_count",
                "prompt_token_count",
                "promptTokenCount",
                "inputTokenCount",
            ],
        )
        .or_else(|| {
            short_usage
                .then(|| find_number(object, &["input", "prompt"]))
                .flatten()
        });
        usage.output_tokens = find_number(
            object,
            &[
                "output_tokens",
                "completion_tokens",
                "outputTokens",
                "completionTokens",
                "output_token_count",
                "completion_token_count",
                "candidatesTokenCount",
                "outputTokenCount",
            ],
        )
        .or_else(|| {
            short_usage
                .then(|| find_number(object, &["output", "completion"]))
                .flatten()
        });
        usage.cached_tokens = find_number(
            object,
            &[
                "cached_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
                "cachedTokens",
                "cache_read_tokens",
                "cacheRead",
                "cache_read",
                "cacheReadInputTokens",
                "cacheCreationInputTokens",
                "cachedContentTokenCount",
            ],
        )
        .or_else(|| {
            short_usage
                .then(|| find_number(object, &["cached"]))
                .flatten()
        });
        usage.total_tokens = find_number(
            object,
            &[
                "total_tokens",
                "totalTokens",
                "token_count",
                "tokens_used",
                "totalTokenCount",
            ],
        )
        .or_else(|| {
            short_usage
                .then(|| find_number(object, &["total"]))
                .flatten()
        });
        for (name, child) in object {
            if name == "tokens" {
                usage = usage.merge(usage_from_opencode_tokens(child));
            } else if ![
                "input_tokens",
                "prompt_tokens",
                "inputTokens",
                "promptTokens",
                "input_token_count",
                "prompt_token_count",
                "promptTokenCount",
                "inputTokenCount",
                "output_tokens",
                "completion_tokens",
                "outputTokens",
                "completionTokens",
                "output_token_count",
                "completion_token_count",
                "candidatesTokenCount",
                "outputTokenCount",
                "cached_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
                "cachedTokens",
                "cache_read_tokens",
                "cacheRead",
                "cache_read",
                "cacheReadInputTokens",
                "cacheCreationInputTokens",
                "cachedContentTokenCount",
                "total_tokens",
                "totalTokens",
                "token_count",
                "tokens_used",
                "totalTokenCount",
                "input",
                "prompt",
                "output",
                "completion",
                "cached",
                "cache_creation_input_tokens",
                "cacheCreationInputTokens",
                "total",
            ]
            .contains(&name.as_str())
            {
                usage = usage.merge(usage_from_json(child));
            }
        }
    } else if let Some(array) = value.as_array() {
        for child in array {
            usage = usage.merge(usage_from_json(child));
        }
    } else if let Some(text) = value.as_str() {
        if let Ok(nested) = serde_json::from_str::<serde_json::Value>(text) {
            usage = usage.merge(usage_from_json(&nested));
        }
    }
    usage
}

fn usage_from_opencode_tokens(value: &serde_json::Value) -> TokenUsage {
    let Some(object) = value.as_object() else {
        return TokenUsage::default();
    };
    let cached = object
        .get("cache")
        .and_then(|cache| {
            cache
                .get("read")
                .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        })
        .or_else(|| find_number(object, &["cached", "cacheRead", "cache_read"]));
    TokenUsage {
        input_tokens: find_number(object, &["input"]),
        output_tokens: find_number(object, &["output"]),
        cached_tokens: cached,
        total_tokens: find_number(object, &["total"]),
        source: None,
    }
}

fn add_values(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn find_number(object: &serde_json::Map<String, serde_json::Value>, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        object.get(*name).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
    })
}

fn usage_from_key_values(line: &str) -> TokenUsage {
    let mut usage = TokenUsage::default();
    let normalized = line.replace(['=', ':', ',', ';', '{', '}', '[', ']'], " ");
    let tokens = normalized.split_whitespace().collect::<Vec<_>>();
    for (index, token) in tokens.iter().enumerate() {
        let key = token
            .trim_matches(['"', '\'', '.', ','])
            .to_ascii_lowercase()
            .replace(['-', '_'], "");
        if matches!(key.as_str(), "sent" | "received") {
            if let Some(number) = index
                .checked_sub(1)
                .and_then(|previous| tokens.get(previous))
                .and_then(|value| parse_token_count(value.trim_matches(['"', '\''])))
            {
                if key == "sent" {
                    usage.input_tokens = Some(number);
                } else {
                    usage.output_tokens = Some(number);
                }
            }
            continue;
        }
        let (key, number) = if let Some(number) = tokens
            .get(index + 1)
            .and_then(|value| parse_token_count(value.trim_matches(['"', '\''])))
        {
            (key, number)
        } else if let Some(number) = tokens
            .get(index + 2)
            .and_then(|value| parse_token_count(value.trim_matches(['"', '\''])))
            && matches!(
                key.as_str(),
                "input" | "output" | "cache" | "cached" | "total"
            )
        {
            let compound = tokens[index + 1]
                .to_ascii_lowercase()
                .replace(['-', '_'], "");
            let key = format!("{key}{compound}");
            (key, number)
        } else {
            continue;
        };
        match key.as_str() {
            "input" | "inputtokens" | "inputtoken" | "prompt" | "prompttokens"
            | "prompttokencount" | "inputtokencount" => usage.input_tokens = Some(number),
            "output"
            | "outputtokens"
            | "outputtoken"
            | "completion"
            | "completiontokens"
            | "candidatestokencount"
            | "outputtokencount" => usage.output_tokens = Some(number),
            "cached"
            | "cachedtokens"
            | "cacheread"
            | "cachereadinputtokens"
            | "cachecreationinputtokens"
            | "cachedcontenttokencount" => usage.cached_tokens = Some(number),
            "total" | "totaltokens" | "totaltokencount" | "tokencount" | "tokensused" => {
                usage.total_tokens = Some(number)
            }
            _ => {}
        }
    }
    usage
}

fn parse_token_count(value: &str) -> Option<u64> {
    let value = value.trim_matches(['"', '\'', '.', ',']);
    let (number, multiplier) = match value
        .chars()
        .last()
        .map(|suffix| suffix.to_ascii_lowercase())
    {
        Some('k') => (&value[..value.len() - 1], 1_000.0),
        Some('m') => (&value[..value.len() - 1], 1_000_000.0),
        _ => (value, 1.0),
    };
    number.parse::<f64>().ok().and_then(|value| {
        let value = value * multiplier;
        (value.is_finite() && value >= 0.0 && value <= u64::MAX as f64)
            .then_some(value.round() as u64)
    })
}

fn read_bounded_text(path: &Path, max_bytes: u64) -> Result<String, std::io::Error> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn update_summary(dir: &Path, run: &RunMeasurement<'_>) -> Result<(), String> {
    let path = dir.join("summary.json");
    let mut runs: Vec<serde_json::Value> = if path.exists() {
        let mut text = String::new();
        File::open(&path)
            .map_err(|e| format!("cannot open summary: {e}"))?
            .read_to_string(&mut text)
            .map_err(|e| format!("cannot read summary: {e}"))?;
        serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("runs").cloned())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    runs.push(serde_json::to_value(run).map_err(|e| e.to_string())?);
    if runs.len() > MAX_RUNS {
        runs.drain(..runs.len() - MAX_RUNS);
    }
    let success_count = runs
        .iter()
        .filter(|v| v.get("success").and_then(serde_json::Value::as_bool) == Some(true))
        .count();
    let summary = serde_json::json!({"schema_version": 1, "runs": runs, "success_count": success_count, "failure_count": runs.len() - success_count, "success_rate": success_count as f64 / runs.len().max(1) as f64});
    write_json(&path, &summary)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let file = File::create(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    serde_json::to_writer_pretty(file, value)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}
fn write_unavailable(path: &Path, collector: &str, error: String) -> Result<(), String> {
    write_json(
        path,
        &CollectorResult {
            schema_version: 1,
            collector: collector.into(),
            status: "unavailable".into(),
            exit_code: None,
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

#[derive(Debug, Serialize)]
struct FlamegraphStatus<'a> {
    schema_version: u32,
    status: &'a str,
    artifact: Option<&'a str>,
    reason: Option<&'a str>,
}

fn wait_bounded(child: &mut Child, timeout: Duration) -> Result<bool, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child
            .try_wait()
            .map_err(|error| format!("cannot poll collector process: {error}"))?
        {
            Some(status) => return Ok(status.success()),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(false);
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

fn write_flamegraph_status(
    dir: &Path,
    status: &'static str,
    artifact: Option<&'static str>,
    reason: &'static str,
) -> Result<(), String> {
    write_json(
        &dir.join("flamegraph.json"),
        &FlamegraphStatus {
            schema_version: 1,
            status,
            artifact,
            reason: (!reason.is_empty()).then_some(reason),
        },
    )
}

/// Write one dependency-free SVG per stable Perfetto metric. Raw traces are
/// intentionally not converted to text: they can contain host process names,
/// while these SVGs contain only ASB's fixed aggregate metrics.
fn write_perfetto_metric_svgs(
    dir: &Path,
    trace: Option<&Path>,
    tracepoint_count: u64,
    metrics: &BTreeMap<String, f64>,
) -> Result<(), String> {
    let mut all_metrics = metrics.clone();
    if all_metrics.is_empty() {
        all_metrics.insert("perfetto.tracepoints".into(), tracepoint_count as f64);
    }
    for (name, value) in all_metrics {
        let trace_state = trace
            .filter(|path| path.is_file())
            .map_or("native trace unavailable", |_| "native trace recorded");
        let label = name.trim_start_matches("perfetto.");
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"760\" height=\"180\" viewBox=\"0 0 760 180\"><title>Perfetto {}</title><rect width=\"760\" height=\"180\" fill=\"#101827\"/><rect x=\"16\" y=\"16\" width=\"728\" height=\"148\" rx=\"10\" fill=\"#182334\"/><text x=\"36\" y=\"50\" fill=\"#e5edf8\" font-family=\"sans-serif\" font-size=\"20\" font-weight=\"bold\">Perfetto: {}</text><text x=\"36\" y=\"92\" fill=\"#78dba9\" font-family=\"monospace\" font-size=\"28\">{:.0}</text><text x=\"36\" y=\"132\" fill=\"#b9c7dc\" font-family=\"sans-serif\" font-size=\"15\">{} · lifecycle markers: {tracepoint_count}</text></svg>\n",
            escape_svg(label),
            escape_svg(label),
            value,
            trace_state,
        );
        fs::write(
            dir.join(format!("perfetto-metric-{}.svg", safe_svg_name(label))),
            svg,
        )
        .map_err(|error| format!("cannot write Perfetto metric SVG: {error}"))?;
    }
    Ok(())
}

/// Each built-in eBPF profile owns one SVG. The comparison layer additionally
/// creates one cross-run graph per numeric metric in that profile.
fn write_bpftrace_profile_svg(
    dir: &Path,
    profile: &str,
    status: &str,
    metrics: &BTreeMap<String, f64>,
) -> Result<(), String> {
    let mut lines = vec![format!("status: {status}")];
    for (name, value) in metrics {
        lines.push(format!(
            "{}: {:.0}",
            name.rsplit('.').next().unwrap_or(name),
            value
        ));
    }
    if metrics.is_empty() {
        lines.push("no aggregate values emitted; inspect the paired JSON result".into());
    }
    let height = 76 + lines.len() * 26;
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"760\" height=\"{height}\" viewBox=\"0 0 760 {height}\"><title>bpftrace {}</title><rect width=\"760\" height=\"{height}\" fill=\"#101827\"/><rect x=\"16\" y=\"16\" width=\"728\" height=\"{}\" rx=\"10\" fill=\"#182334\"/><text x=\"36\" y=\"48\" fill=\"#e5edf8\" font-family=\"sans-serif\" font-size=\"20\" font-weight=\"bold\">bpftrace: {}</text>",
        escape_svg(profile),
        height - 32,
        escape_svg(profile)
    );
    for (index, line) in lines.iter().enumerate() {
        let color = if index == 0 && status == "completed" {
            "#78dba9"
        } else if index == 0 {
            "#ffb86b"
        } else {
            "#b9c7dc"
        };
        svg.push_str(&format!(
            "<text x=\"36\" y=\"{}\" fill=\"{color}\" font-family=\"monospace\" font-size=\"15\">{}</text>",
            76 + index * 26,
            escape_svg(line)
        ));
    }
    svg.push_str("</svg>\n");
    fs::write(dir.join(format!("bpftrace-{profile}.svg")), svg)
        .map_err(|error| format!("cannot write bpftrace profile SVG: {error}"))
}

fn escape_svg(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn safe_svg_name(value: &str) -> String {
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

fn parse_collector_metrics(name: &str, output: &str) -> BTreeMap<String, f64> {
    match name {
        "mpstat" => {
            let metrics = parse_pidstat_cpu_metrics(output);
            if metrics.is_empty() {
                parse_mpstat_metrics(output)
            } else {
                metrics
            }
        }
        "iostat" => {
            let metrics = parse_pidstat_io_metrics(output);
            if metrics.is_empty() {
                parse_iostat_metrics(output)
            } else {
                metrics
            }
        }
        "perf-stat" => parse_perf_stat_metrics(output),
        profile if profile.starts_with("bpftrace-") => {
            parse_bpftrace_metrics(profile.trim_start_matches("bpftrace-"), output)
        }
        _ => BTreeMap::new(),
    }
}

fn parse_collector_series(name: &str, output: &str) -> BTreeMap<String, Vec<f64>> {
    match name {
        "mpstat" => parse_pidstat_series(
            output,
            &["%usr", "%system", "%guest", "%wait", "%CPU"],
            |name| {
                format!(
                    "process.cpu.{}",
                    match name {
                        "%usr" => "user_pct",
                        "%system" => "system_pct",
                        "%guest" => "guest_pct",
                        "%wait" => "wait_pct",
                        "%CPU" => "total_pct",
                        _ => "unknown",
                    }
                )
            },
        ),
        "iostat" => parse_pidstat_series(
            output,
            &["kB_rd/s", "kB_wr/s", "kB_ccwr/s", "iodelay"],
            |name| {
                format!(
                    "process.io.{}",
                    match name {
                        "kB_rd/s" => "read_kib_per_s",
                        "kB_wr/s" => "write_kib_per_s",
                        "kB_ccwr/s" => "cancelled_write_kib_per_s",
                        "iodelay" => "delay_ticks",
                        _ => "unknown",
                    }
                )
            },
        ),
        "perf-stat" => parse_perf_stat_series(output),
        profile if profile.starts_with("bpftrace-") => {
            parse_bpftrace_series(profile.trim_start_matches("bpftrace-"), output)
        }
        _ => BTreeMap::new(),
    }
}

fn parse_pidstat_series(
    output: &str,
    wanted: &[&str],
    name: impl Fn(&str) -> String,
) -> BTreeMap<String, Vec<f64>> {
    let mut header = None;
    let mut samples: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for line in output.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.first().is_some_and(|field| *field == "Average:")
            || fields.first().is_some_and(|field| *field == "Linux")
        {
            continue;
        }
        if fields.iter().any(|field| wanted.contains(field)) {
            header = Some(fields);
            continue;
        }
        let Some(header_fields) = header.as_ref() else {
            continue;
        };
        if fields.len() != header_fields.len() {
            continue;
        }
        for wanted_name in wanted {
            let Some(index) = header_fields.iter().position(|field| field == wanted_name) else {
                continue;
            };
            let Some(value) = fields.get(index).and_then(|field| parse_float(field)) else {
                continue;
            };
            samples.entry(name(wanted_name)).or_default().push(value);
        }
    }
    samples.retain(|_, values| !values.is_empty());
    samples
}

fn parse_perf_stat_series(output: &str) -> BTreeMap<String, Vec<f64>> {
    let mut series: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for line in output.lines() {
        let fields = line.split(',').map(str::trim).collect::<Vec<_>>();
        let Some(event_index) = fields.iter().position(|field| {
            matches!(
                *field,
                "task-clock"
                    | "context-switches"
                    | "cpu-migrations"
                    | "page-faults"
                    | "cycles"
                    | "instructions"
            )
        }) else {
            continue;
        };
        let Some(value) = event_index
            .checked_sub(2)
            .and_then(|index| fields.get(index))
            .and_then(|field| parse_float(field))
        else {
            continue;
        };
        let Some(name) = perf_metric_name(fields[event_index]) else {
            continue;
        };
        series
            .entry(format!("perf.{name}"))
            .or_default()
            .push(value);
    }
    series
}

/// bpftrace prints aggregation maps as `@asb_metric: value`. Only these
/// ASB-owned map names are accepted, so arbitrary program output cannot create
/// comparison metrics accidentally.
fn parse_bpftrace_metrics(profile: &str, output: &str) -> BTreeMap<String, f64> {
    let mut metrics = BTreeMap::new();
    for line in output.lines() {
        let Some((name, value)) = line.trim().split_once(':') else {
            continue;
        };
        let Some(name) = name.trim().strip_prefix("@asb_") else {
            continue;
        };
        let Some(value) = parse_float(value) else {
            continue;
        };
        metrics.insert(format!("bpftrace.{profile}.{name}"), value);
    }
    metrics
}

fn parse_bpftrace_series(profile: &str, output: &str) -> BTreeMap<String, Vec<f64>> {
    let mut series: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for line in output.lines() {
        let Some((name, value)) = line.trim().split_once(':') else {
            continue;
        };
        let Some(name) = name.trim().strip_prefix("@asb_") else {
            continue;
        };
        let Some(value) = parse_float(value) else {
            continue;
        };
        series
            .entry(format!("bpftrace.{profile}.{name}"))
            .or_default()
            .push(value);
    }
    series
}

/// Parse `perf stat -x,` output into stable unit-named fields. Unsupported
/// counters remain absent instead of being inferred from unrelated values.
fn parse_perf_stat_metrics(output: &str) -> BTreeMap<String, f64> {
    let mut metrics = BTreeMap::new();
    for line in output.lines() {
        let fields = line.split(',').map(str::trim).collect::<Vec<_>>();
        let Some(event_index) = fields.iter().position(|field| {
            matches!(
                *field,
                "task-clock"
                    | "context-switches"
                    | "cpu-migrations"
                    | "page-faults"
                    | "cycles"
                    | "instructions"
            )
        }) else {
            continue;
        };
        let Some(value) = event_index
            .checked_sub(2)
            .and_then(|index| fields.get(index))
            .and_then(|field| parse_float(field))
        else {
            continue;
        };
        let Some(name) = perf_metric_name(fields[event_index]) else {
            continue;
        };
        metrics.insert(format!("perf.{name}"), value);
    }
    metrics
}

fn perf_metric_name(event: &str) -> Option<&'static str> {
    match event {
        "task-clock" => Some("task_clock_ms"),
        "context-switches" => Some("context_switches"),
        "cpu-migrations" => Some("cpu_migrations"),
        "page-faults" => Some("page_faults"),
        "cycles" => Some("cycles"),
        "instructions" => Some("instructions"),
        _ => None,
    }
}

fn tool_binary_name(name: &str) -> &str {
    if name == "perfetto" { "tracebox" } else { name }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact")
        .to_owned()
}

fn collector_artifact_exists(
    name: &str,
    perf_data: Option<&Path>,
    perfetto_trace: Option<&Path>,
) -> bool {
    let path = match name {
        "perf" => perf_data,
        "perfetto" => perfetto_trace,
        _ => None,
    };
    path.is_some_and(|path| path.metadata().is_ok_and(|metadata| metadata.len() > 0))
}

fn trace_marker_path() -> Option<PathBuf> {
    [
        PathBuf::from("/sys/kernel/tracing/trace_marker"),
        PathBuf::from("/sys/kernel/debug/tracing/trace_marker"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

fn perfetto_config(pid: Option<u32>) -> String {
    // Tracebox's bundled schema rejects FtraceConfig.pid_filter. Process
    // scoping is applied during post-processing using ASB lifecycle markers.
    let _ = pid;
    r#"duration_ms: 3600000
buffers { size_kb: 65536 fill_policy: RING_BUFFER }
data_sources {
  config {
    name: "linux.ftrace"
    ftrace_config {
      compact_sched { enabled: true }
      ftrace_events: "sched/sched_switch"
      ftrace_events: "sched/sched_waking"
      ftrace_events: "sched/sched_wakeup_new"
      ftrace_events: "sched/sched_process_exit"
      ftrace_events: "sched/sched_process_free"
      ftrace_events: "task/task_newtask"
      ftrace_events: "task/task_rename"
      ftrace_events: "ftrace/print"
    }
  }
}
data_sources {
  config {
    name: "linux.process_stats"
    process_stats_config { scan_all_processes_on_start: false }
  }
}
"#
    .to_owned()
}

fn perfetto_probe_config() -> String {
    perfetto_config(None).replacen("duration_ms: 3600000", "duration_ms: 250", 1)
}

fn parse_mpstat_metrics(output: &str) -> BTreeMap<String, f64> {
    let mut header = None;
    let mut totals: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if let Some(cpu) = fields.iter().position(|field| *field == "CPU")
            && fields
                .get(cpu + 1)
                .is_some_and(|field| field.starts_with('%'))
        {
            header = Some((cpu, fields[cpu + 1..].to_vec()));
            continue;
        }
        let Some((cpu_index, names)) = &header else {
            continue;
        };
        if fields.get(*cpu_index).copied() != Some("all") {
            continue;
        }
        for (index, name) in names.iter().enumerate() {
            let Some(value) = fields
                .get(cpu_index + 1 + index)
                .and_then(|field| parse_float(field))
            else {
                continue;
            };
            let key = mpstat_name(name);
            let entry = totals.entry(key).or_default();
            entry.0 += value;
            entry.1 += 1;
        }
    }
    totals
        .into_iter()
        .filter_map(|(name, (sum, count))| {
            (count > 0).then_some((format!("cpu.{name}"), sum / count as f64))
        })
        .collect()
}

/// Parse process-scoped `pidstat -u` output. The collector is retained under
/// the stable `mpstat` result name for compatibility, but it no longer uses
/// host-wide mpstat CPU totals.
fn parse_pidstat_cpu_metrics(output: &str) -> BTreeMap<String, f64> {
    parse_pidstat_columns(
        output,
        &["%usr", "%system", "%guest", "%wait", "%CPU"],
        |name| {
            format!(
                "process.cpu.{}",
                match name {
                    "%usr" => "user_pct",
                    "%system" => "system_pct",
                    "%guest" => "guest_pct",
                    "%wait" => "wait_pct",
                    "%CPU" => "total_pct",
                    _ => "unknown",
                }
            )
        },
    )
}

/// Parse process-scoped `pidstat -d` output. Values are averaged over the
/// selected agent process and its child tasks, excluding unrelated processes.
fn parse_pidstat_io_metrics(output: &str) -> BTreeMap<String, f64> {
    parse_pidstat_columns(
        output,
        &["kB_rd/s", "kB_wr/s", "kB_ccwr/s", "iodelay"],
        |name| {
            format!(
                "process.io.{}",
                match name {
                    "kB_rd/s" => "read_kib_per_s",
                    "kB_wr/s" => "write_kib_per_s",
                    "kB_ccwr/s" => "cancelled_write_kib_per_s",
                    "iodelay" => "delay_ticks",
                    _ => "unknown",
                }
            )
        },
    )
}

fn parse_pidstat_columns(
    output: &str,
    wanted: &[&str],
    name: impl Fn(&str) -> String,
) -> BTreeMap<String, f64> {
    let mut header = None;
    let mut totals: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for line in output.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.first().is_some_and(|field| *field == "Average:") {
            continue;
        }
        if fields.iter().any(|field| wanted.contains(field)) {
            header = Some(fields);
            continue;
        }
        let Some(header_fields) = header.as_ref() else {
            continue;
        };
        if fields.len() != header_fields.len()
            || fields.first().is_some_and(|field| *field == "Linux")
        {
            continue;
        }
        for wanted_name in wanted {
            let Some(index) = header_fields.iter().position(|field| field == wanted_name) else {
                continue;
            };
            let Some(value) = fields.get(index).and_then(|field| parse_float(field)) else {
                continue;
            };
            let entry = totals.entry((*wanted_name).to_owned()).or_default();
            entry.0 += value;
            entry.1 += 1;
        }
    }
    totals
        .into_iter()
        .filter(|(_, (_, count))| *count > 0)
        .map(|(metric, (sum, count))| (name(&metric), sum / count as f64))
        .collect()
}

fn mpstat_name(name: &str) -> String {
    let mapped = match name.trim_start_matches('%') {
        "usr" => Some("user_pct"),
        "nice" => Some("nice_pct"),
        "sys" => Some("system_pct"),
        "iowait" => Some("iowait_pct"),
        "irq" => Some("irq_pct"),
        "soft" => Some("soft_pct"),
        "steal" => Some("steal_pct"),
        "guest" => Some("guest_pct"),
        "gnice" => Some("guest_nice_pct"),
        _ => None,
    };
    mapped
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{}_pct", name.trim_start_matches('%')))
}

fn parse_iostat_metrics(output: &str) -> BTreeMap<String, f64> {
    let mut metrics = BTreeMap::new();
    let mut lines = output.lines().peekable();
    let mut device_totals: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    while let Some(line) = lines.next() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if line.trim_start().starts_with("avg-cpu") {
            let headers: Vec<&str> = line
                .split_once(':')
                .map_or_else(Vec::new, |(_, rest)| rest.split_whitespace().collect());
            let Some(values) = lines
                .next()
                .map(|line| line.split_whitespace().collect::<Vec<_>>())
            else {
                continue;
            };
            for (header, value) in headers.iter().zip(values.iter()) {
                if let Some(value) = parse_float(value) {
                    metrics.insert(format!("cpu.{}", iostat_name(header)), value);
                }
            }
            continue;
        }
        if fields.first().copied() != Some("Device") || fields.len() < 2 {
            continue;
        }
        let headers = fields[1..].to_vec();
        while let Some(next) = lines.peek().copied() {
            let values: Vec<&str> = next.split_whitespace().collect();
            if values.is_empty() || values.first().is_some_and(|value| *value == "Device") {
                break;
            }
            if values.first().is_some_and(|value| *value == "avg-cpu:") {
                break;
            }
            let _ = lines.next();
            if values.len() < headers.len() + 1 {
                continue;
            }
            for (header, value) in headers.iter().zip(values[1..].iter()) {
                let Some(value) = parse_float(value) else {
                    continue;
                };
                let entry = device_totals.entry(iostat_name(header)).or_default();
                entry.0 += value;
                entry.1 += 1;
            }
        }
    }
    for (name, (sum, count)) in device_totals {
        if count > 0 {
            metrics.insert(format!("device.{name}"), sum / count as f64);
        }
    }
    metrics
}

fn iostat_name(name: &str) -> String {
    let mapped = match name {
        "%user" => Some("user_pct"),
        "%nice" => Some("nice_pct"),
        "%system" => Some("system_pct"),
        "%iowait" => Some("iowait_pct"),
        "%steal" => Some("steal_pct"),
        "%idle" => Some("idle_pct"),
        "r/s" => Some("read_ops_per_s"),
        "rkB/s" => Some("read_kib_per_s"),
        "rrqm/s" => Some("read_merged_ops_per_s"),
        "%rrqm" => Some("read_merged_pct"),
        "r_await" => Some("read_await_ms"),
        "rareq-sz" => Some("read_request_kib"),
        "w/s" => Some("write_ops_per_s"),
        "wkB/s" => Some("write_kib_per_s"),
        "wrqm/s" => Some("write_merged_ops_per_s"),
        "%wrqm" => Some("write_merged_pct"),
        "w_await" => Some("write_await_ms"),
        "wareq-sz" => Some("write_request_kib"),
        "d/s" => Some("discard_ops_per_s"),
        "dkB/s" => Some("discard_kib_per_s"),
        "drqm/s" => Some("discard_merged_ops_per_s"),
        "%drqm" => Some("discard_merged_pct"),
        "d_await" => Some("discard_await_ms"),
        "dareq-sz" => Some("discard_request_kib"),
        "f/s" => Some("flush_ops_per_s"),
        "f_await" => Some("flush_await_ms"),
        "aqu-sz" => Some("queue_depth"),
        "%util" => Some("util_pct"),
        _ => None,
    };
    mapped.map(str::to_owned).unwrap_or_else(|| {
        name.trim_matches('%')
            .replace(['/', '-'], "_")
            .to_lowercase()
    })
}

fn parse_float(value: &str) -> Option<f64> {
    value
        .trim_matches(|character: char| {
            !character.is_ascii_digit() && character != '.' && character != '-'
        })
        .parse()
        .ok()
}

fn measurement_interval_ms() -> u64 {
    std::env::var("ASB_MEASURE_INTERVAL_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1_000)
        .clamp(100, 60_000)
}

/// Stop a collector without allowing a wrapper such as `sudo` to strand its
/// real tracing child.  The previous unbounded `wait()` could deadlock an
/// entire sweep after the agent had already exited.
fn stop_collector(child: &mut Child, graceful: bool) -> std::io::Result<std::process::ExitStatus> {
    let pid = child.id();
    if graceful {
        let _ = Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = child.kill();
    // `sudo` may have spawned the actual collector as a direct child. Reap
    // that child too; otherwise the pipe-backed collector can outlive ASB.
    let _ = Command::new("pkill")
        .args(["-KILL", "-P", &pid.to_string()])
        .status();
    child.wait()
}

fn pidstat_interval_seconds() -> String {
    // pidstat accepts whole-second intervals. Preserve the requested lower
    // bound while rounding sub-second requests up to the nearest supported
    // interval; perf stat below supports the full millisecond range.
    measurement_interval_ms().div_ceil(1_000).to_string()
}

fn bounded(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_CAPTURE)]).into_owned()
}
fn read_bounded(path: &Path) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    let mut limited = (&mut file).take(MAX_CAPTURE as u64);
    if limited.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    bounded(&bytes)
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
    measurement_dir: &Path,
    selected: &[String],
    tool_dir: Option<&Path>,
) -> Result<BTreeSet<String>, String> {
    let mut sudo_collectors = BTreeSet::new();
    let bpftrace_selected = (selected_or_env("bpftrace", "ASB_MEASURE_BPFTRACE", selected)
        && std::env::var_os("ASB_BPFTRACE_PROGRAMS").is_some())
        || !selected_bpftrace_profiles(selected)?.is_empty();
    let probes = [
        (
            "perf",
            selected_or_env("perf", "ASB_MEASURE_PERF", selected),
            vec!["stat", "-e", "task-clock", "--", "sleep", "0.01"],
        ),
        (
            "bpftrace",
            bpftrace_selected,
            vec!["-e", "BEGIN { exit(); }"],
        ),
    ];
    for (name, enabled, args) in probes {
        if !enabled {
            continue;
        }
        let executable = resolve_executable(tool_dir, name);
        let direct = Command::new(&executable)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match direct {
            Ok(status) if status.success() => continue,
            Ok(_) => {
                if name == "perf" {
                    return Err(perf_permission_error());
                }
                let sudo = Command::new("sudo")
                    .args(["-n", "--"])
                    .arg(&executable)
                    .args(&args)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                if sudo.is_ok_and(|status| status.success()) {
                    sudo_collectors.insert(name.to_owned());
                    continue;
                }
                return Err(passwordless_sudo_error(name, &executable));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!("cannot probe {name} collector: {error}"));
            }
        }
    }
    let bpftrace_profiles = selected_bpftrace_profiles(selected)?;
    if !bpftrace_profiles.is_empty() {
        let executable = resolve_executable(tool_dir, "bpftrace");
        if !executable.is_file() {
            return Err(format!(
                "selected bpftrace profiles need a project-local bpftrace executable. What to do: run `asb tool install bpftrace` in this project, then rerun the benchmark."
            ));
        }
        for profile in bpftrace_profiles {
            if bpftrace_profile_probe(&executable, profile, false)? {
                continue;
            }
            if bpftrace_profile_probe(&executable, profile, true)? {
                sudo_collectors.insert("bpftrace".to_owned());
                continue;
            }
            if passwordless_sudo_available() {
                return Err(format!(
                    "bpftrace profile `{}` cannot attach on this kernel. It is not a sudo authorization failure; inspect the kernel tracepoint support and the profile's JSON/SVG documentation before rerunning.",
                    profile.name
                ));
            }
            return Err(passwordless_sudo_error("bpftrace", &executable));
        }
    }
    if selected_or_env("perfetto", "ASB_MEASURE_PERFETTO", selected) {
        let executable = resolve_executable(tool_dir, "perfetto");
        // A missing optional collector is reported as unavailable when the
        // session starts, preserving the existing opt-in behavior. Once it is
        // installed, however, validate the real tracefs/ftrace path before
        // starting an agent.
        if executable.is_file() {
            if trace_marker_path().is_none() {
                return Err(perfetto_tracefs_error());
            }
            if perfetto_probe(&executable, measurement_dir, false)? {
                return Ok(sudo_collectors);
            }
            if perfetto_probe(&executable, measurement_dir, true)? {
                sudo_collectors.insert("perfetto".to_owned());
            } else {
                return Err(passwordless_sudo_error("perfetto", &executable));
            }
        }
    }
    Ok(sudo_collectors)
}

/// Compile and attach a concrete profile before the agent starts. The appended
/// BEGIN exit keeps the probe bounded while exercising the exact tracepoints,
/// predicates, and target-PID parameter form used during the run.
fn bpftrace_profile_probe(
    executable: &Path,
    profile: BpftraceProfile,
    sudo: bool,
) -> Result<bool, String> {
    let program = format!("{}\nBEGIN {{ exit(); }}", profile.program);
    let mut command = if sudo {
        let mut command = Command::new("sudo");
        command.args(["-n", "--"]);
        command.arg(executable);
        command
    } else {
        Command::new(executable)
    };
    command
        .args(["-e", &program, "0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .map_err(|error| format!("cannot probe bpftrace profile {}: {error}", profile.name))
}

fn passwordless_sudo_available() -> bool {
    Command::new("sudo")
        .args(["-n", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Probe the exact ftrace configuration used by ASB. `--background-wait`
/// waits for Perfetto to acknowledge every data source, so a successful
/// process launch cannot mask an unavailable tracefs instance.
fn perfetto_probe(executable: &Path, measurement_dir: &Path, sudo: bool) -> Result<bool, String> {
    let config = measurement_dir.join(".perfetto-preflight.pbtx");
    let trace = measurement_dir.join(".perfetto-preflight.perfetto-trace");
    fs::write(&config, perfetto_probe_config())
        .map_err(|error| format!("cannot write Perfetto preflight configuration: {error}"))?;
    let mut command = if sudo {
        let mut command = Command::new("sudo");
        command.args(["-n", "--"]);
        command.arg(executable);
        command
    } else {
        Command::new(executable)
    };
    let result = command
        .args(["--txt", "-c"])
        .arg(&config)
        .args(["-o"])
        .arg(&trace)
        .arg("--background-wait")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = fs::remove_file(&config);
    let _ = fs::remove_file(&trace);
    match result {
        Ok(status) => Ok(status.success()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && sudo => Ok(false),
        Err(error) => Err(format!("cannot probe Perfetto ftrace collector: {error}")),
    }
}

fn perfetto_tracefs_error() -> String {
    "Perfetto needs the Linux tracefs ftrace interface, but ASB could not find trace_marker under /sys/kernel/tracing or /sys/kernel/debug/tracing. This is a host kernel interface, not an installable project tool. What to do: mount tracefs or enable kernel tracing, then rerun. If mounting tracefs requires privileges, grant the required passwordless sudo access before starting the benchmark; ASB never prompts for a password.".into()
}

fn resolve_executable(tool_dir: Option<&Path>, name: &str) -> PathBuf {
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
            let candidate = directory.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(tool_binary_name(name))
}

fn passwordless_sudo_error(name: &str, executable: &Path) -> String {
    let command = sudoers_repair_command(name, executable);
    format!(
        "{name} collector needs elevated access, but its direct probe failed and passwordless sudo is unavailable.\nWhat to do: grant only this executable passwordless sudo, then rerun:\n  {command}\nThe generated rule should authorize `{}` for the current user; ASB never prompts for a sudo password.",
        executable.display()
    )
}

fn perf_permission_error() -> String {
    "perf cannot access performance counters because kernel.perf_event_paranoid is too restrictive.\nWhat to do: allow user-space perf access, then rerun the benchmark:\n  sudo sysctl -w kernel.perf_event_paranoid=-1\nTo persist the setting across reboots:\n  printf 'kernel.perf_event_paranoid=-1\\n' | sudo tee /etc/sysctl.d/99-asb-perf.conf\nASB does not create sudoers rules for perf.".into()
}

fn sudoers_repair_command(name: &str, executable: &Path) -> String {
    let path = shell_quote(&executable.to_string_lossy());
    let file = format!("/etc/sudoers.d/asb-{name}");
    format!(
        "sudo sh -c 'printf \"%s ALL=(root) NOPASSWD: {path}\\n\" \"$USER\" > {file} && chmod 440 {file} && visudo -cf {file}'"
    )
}

fn shell_quote(value: &str) -> String {
    value.replace('\'', "'\\''")
}

fn selected_or_env(name: &str, variable: &str, selected: &[String]) -> bool {
    debug_assert_eq!(
        collectors::find(name).and_then(|collector| collector.environment_flag()),
        Some(variable)
    );
    let Some(collector) = collectors::find(name) else {
        return false;
    };
    // Resolve these catalog properties here even when the execution path only
    // needs enablement. It verifies that every selectable collector carries
    // complete lifecycle metadata for the PID-attached runner.
    let _tool = collector.tool_id();
    let _pid_attached = collector.requires_pid();
    collectors::selected_or_environment(name, selected)
}

fn selected_bpftrace_profiles(selected: &[String]) -> Result<Vec<BpftraceProfile>, String> {
    let mut requested = selected
        .iter()
        .filter_map(|value| value.strip_prefix("bpftrace:"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if let Ok(value) = std::env::var("ASB_BPFTRACE_PROFILES") {
        requested.extend(
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        );
    }
    if selected.iter().any(|value| value == "bpftrace") && requested.is_empty() {
        requested.extend(
            BPFTRACE_PROFILES
                .iter()
                .map(|profile| profile.name.to_owned()),
        );
    }
    if requested.iter().any(|value| value == "all") {
        return Ok(BPFTRACE_PROFILES.to_vec());
    }
    requested.sort();
    requested.dedup();
    requested
        .iter()
        .map(|name| {
            BPFTRACE_PROFILES
                .iter()
                .find(|profile| profile.name == name)
                .copied()
                .ok_or_else(|| {
                    format!(
                        "unknown bpftrace profile `{name}`; choose one of: {}",
                        bpftrace_profile_names().collect::<Vec<_>>().join(", ")
                    )
                })
        })
        .collect()
}
fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}
fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}
fn read_trimmed(path: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}
fn read_mem_total() -> Option<u64> {
    read_trimmed("/proc/meminfo")?.lines().find_map(|line| {
        line.strip_prefix("MemTotal:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn writes_machine_readable_run_and_summary() {
        let dir = std::env::temp_dir().join(format!("asb-measure-{}", unix_ms()));
        let session = Session::start_with_collectors(&dir, &[], None).expect("session");
        let run = RunMeasurement {
            schema_version: 1,
            run_id: "test".into(),
            status: "completed",
            success: true,
            success_rate: 1.0,
            agent: "a",
            workload: "w",
            provider: "p",
            model: "m",
            elapsed_ms: 2,
            exit_code: Some(0),
            token_usage: TokenUsage::default(),
            steps: 1,
            started_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        };
        session.finish(run).expect("finish");
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join("summary.json")).unwrap()).unwrap();
        assert_eq!(parsed["success_rate"], 1.0);
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
                r#"{"benchmark_usage":{"task_summary":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}}"#,
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
                r#"Token usage: input_tokens=41 output_tokens=9 cacheReadInputTokens=6 totalTokens=50"#,
            ),
        ];
        for (agent, output) in cases {
            let usage = token_usage_from_sources("cli2key", agent, output, "", None);
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
    fn counts_structured_agent_steps_and_falls_back_to_one() {
        let output = r#"{"type":"tool_call","step":1}
{"type":"tool_call","step":2}"#;
        assert_eq!(steps_from_sources("codex", output, ""), 2);
        assert_eq!(steps_from_sources("codex", "completed", ""), 1);
        assert_eq!(steps_from_sources("codex", "", ""), 0);
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
        let series = parse_collector_series(
            "mpstat",
            "Linux 6.1\n12:00:00 UID PID %usr %system %guest %wait %CPU CPU Command\n12:00:01 1000 42 10.00 2.00 0.00 1.00 12.00 3 agent\n12:00:02 1000 42 20.00 4.00 0.00 0.00 24.00 3 agent\n",
        );
        assert_eq!(series["process.cpu.user_pct"], vec![10.0, 20.0]);
        assert_eq!(series["process.cpu.total_pct"], vec![12.0, 24.0]);
        let io_series = parse_collector_series(
            "iostat",
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
        assert_eq!(metrics["perf.cycles"], 1234.0);
        assert_eq!(metrics["perf.instructions"], 567.0);
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
                "lock-contention"
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
            bpftrace_profile_names().count()
        );
        assert!(
            profiles
                .iter()
                .all(|profile| profile.program.contains("$1"))
        );
        assert!(
            profiles
                .iter()
                .all(|profile| profile.program.contains("interval:s:1"))
        );
        let metrics = parse_bpftrace_metrics(
            "sched-latency",
            "@asb_wakeup_latency_events: 7\n@asb_wakeup_latency_total_ns: 420\n@other: 99\n",
        );
        assert_eq!(metrics["bpftrace.sched-latency.wakeup_latency_events"], 7.0);
        assert_eq!(
            metrics["bpftrace.sched-latency.wakeup_latency_total_ns"],
            420.0
        );
        assert_eq!(metrics.len(), 2);
        let series = parse_collector_series(
            "bpftrace-sched-latency",
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
    fn parses_perfetto_csv_series_with_trace_timestamps() {
        let (timestamps, values) = parse_perfetto_series_output(
            "\"timestamp_s\",\"value\"\n\"12\",\"3\"\n\"13.5\",\"7\"\n",
        );
        assert_eq!(timestamps, vec![12_000.0, 13_500.0]);
        assert_eq!(values, vec![3.0, 7.0]);
    }
}
