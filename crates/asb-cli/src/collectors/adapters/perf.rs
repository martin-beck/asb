use super::super::CollectorAdapter;
use super::super::PreflightContext;
use super::super::{ProcessContext, ProcessData, SharedArtifact};
use crate::measurements;
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(all(test, target_os = "linux"))]
#[path = "../../../tests/unit/perf_stop_contract.rs"]
mod stop_contract_tests;

/// Build the process-scoped `perf record` arguments.  Attaching to the runner
/// root with inheritance is deliberate: the root may fork a bridge, agent
/// helper, or workload process after attachment, and each descendant must be
/// included without sampling unrelated host processes.
fn record_args(pid: u32) -> Vec<String> {
    vec![
        "record".into(),
        // Do not let perf select a host-specific hardware default.  On hybrid
        // systems that can expand to cpu_core/cycles or cpu_atom/cycles and
        // fail after attaching to a container-owned workload even though the
        // generic host probe succeeded.  cpu-clock is a software event and
        // keeps the stack-sample/flamegraph contract portable.
        "-e".into(),
        "cpu-clock".into(),
        "-g".into(),
        "-F".into(),
        "19".into(),
        "--inherit".into(),
        "-p".into(),
        pid.to_string(),
    ]
}

/// Build the process-scoped interval counter arguments with the same
/// inheritance contract as call-stack recording.
fn stat_args(pid: u32, interval_ms: u64) -> Vec<String> {
    vec![
        "stat".into(),
        "--no-scale".into(),
        "-I".into(),
        interval_ms.to_string(),
        "-x,".into(),
        "-e".into(),
        // These software counters remain available when a workload's cgroup
        // or hybrid-PMU policy rejects hardware cycles/instructions.  Hardware
        // counters are deliberately not estimated or silently substituted.
        "task-clock,context-switches,cpu-migrations,page-faults".into(),
        "--inherit".into(),
        "-p".into(),
        pid.to_string(),
    ]
}

pub(super) struct PerfCollector;
pub(super) static ADAPTER: PerfCollector = PerfCollector;

impl CollectorAdapter for PerfCollector {
    #[cfg(any(target_os = "linux", test))]
    fn host_settings(&self) -> &'static [(&'static str, &'static str, &'static str)] {
        &[(
            "kernel.perf_event_paranoid",
            "-1",
            "perf process-tree counters",
        )]
    }
    fn estimated_disk_mib(&self) -> u64 {
        512
    }
    fn description(&self) -> &'static str {
        "process-tree performance counter samples"
    }
    fn id(&self) -> &'static str {
        "perf"
    }
    fn environment_flag(&self) -> Option<&'static str> {
        Some("ASB_MEASURE_PERF")
    }
    fn tool_id(&self) -> Option<&'static str> {
        Some("perf")
    }
    fn requires_pid(&self) -> bool {
        true
    }
    fn preflight(&self, context: &mut PreflightContext<'_>) -> Result<(), String> {
        preflight(context)
    }
    fn configure(
        &self,
        session: &mut measurements::Session,
        selected: &[String],
    ) -> Result<(), String> {
        session.state::<PerfState>().borrow_mut().enabled =
            super::super::selected_or_environment("perf", selected);
        Ok(())
    }
    fn producer_policy(&self, name: &str) -> measurements::ProducerPolicy {
        measurements::ProducerPolicy {
            stream_bytes: 256 * 1024 * 1024,
            graceful: matches!(name, "perf" | "perf-stat")
                .then_some(std::time::Duration::from_secs(5)),
        }
    }
    fn accepts_termination(
        &self,
        name: &str,
        completion: &measurements::ProducerCompletion,
    ) -> bool {
        matches!(name, "perf" | "perf-stat")
            && accepted_exit(
                completion.exit,
                completion.interruption_requested
                    && completion.interruption_acknowledged
                    && completion.guardian_cleanup_confirmed == Some(true)
                    && completion.capture_complete,
            )
    }
    fn allows_failed_termination(&self, name: &str) -> bool {
        matches!(name, "perf" | "perf-stat")
    }
    fn retain_raw_on_processing_failure(&self) -> bool {
        true
    }
    fn process(
        &self,
        session: &mut measurements::Session,
        context: &ProcessContext<'_>,
    ) -> Result<ProcessData, String> {
        process(session, context)
    }
    fn attach_pid(&self, session: &mut measurements::Session, pid: u32) -> Result<(), String> {
        session.start_perf(pid)
    }
    fn attach_isolated_pid(
        &self,
        session: &mut measurements::Session,
        pid: u32,
    ) -> Result<(), String> {
        // The target is a verified daemon-host PID, but rootless Docker may
        // still reject unprivileged perf_event_open across its user namespace.
        // Prefer an existing non-interactive sudo route before the launch gate
        // resumes; never probe the deliberately stopped target.
        let _ = session.enable_passwordless_sudo_collector("perf");
        session.start_perf(pid)
    }
}

fn process(
    session: &mut measurements::Session,
    context: &ProcessContext<'_>,
) -> Result<ProcessData, String> {
    let mut result = ProcessData::from_output(context);
    if !accepted_exit(context.status, context.controlled_interruption) {
        result.status = "failed".into();
        return Ok(result);
    }
    if context.name == "perf-stat" {
        let file = context
            .stderr_reader()
            .map_err(|error| format!("cannot read complete perf stat output: {error}"))?;
        let parsed = parse_perf_stat(std::io::BufReader::new(file))?;
        if parsed.metrics.is_empty() {
            return Err(
                "perf stat produced no supported numeric counters; no totals published".into(),
            );
        }
        result.metrics = parsed.metrics;
        result.series = parsed.series;
        result.timestamps = parsed.timestamps;
        measurements::write_json(
            &session.dir.join("perf-stat-quality.json"),
            &serde_json::json!({
                "schema_version": 1,
                "counter_mode": "unscaled-observed",
                "whole_run_hardware_estimates": false,
                "events": parsed.quality,
                "limitations": [
                    "Hardware values are observed unscaled counts, potentially incomplete; never estimated whole-run totals.",
                    "Unavailable intervals and multiplexing can omit observations. PMU running percentage is not CPU utilization.",
                    "Hybrid PMU identities and runtime fractions are retained separately. Rounded percentages are never used to reverse scaling."
                ]
            }),
        )?;
        result.artifact = Some("perf-stat-quality.json".into());
    } else {
        let artifact = session
            .artifact(SharedArtifact::StackSamples)
            .filter(|path| path.is_file())
            .ok_or("perf record artifact is missing; cannot validate captured samples")?;
        validate_record(
            session,
            artifact,
            std::time::Duration::from_secs(300),
            256 * 1024 * 1024,
        )?;
        result.artifact = Some(measurements::file_name(artifact));
    }
    result.status = "completed".into();
    Ok(result)
}

// perf v6.12 builtin-stat.c sig_atexit and builtin-record.c record__sig_exit
// restore the signal default and re-raise it AFTER flushing. A signal alone is
// not success: only an acknowledged deliberate interrupt with settled complete
// capture may reach the owning adapter's full-data validation above.
// https://github.com/torvalds/linux/blob/v6.12/tools/perf/builtin-stat.c
// https://github.com/torvalds/linux/blob/v6.12/tools/perf/builtin-record.c
fn accepted_exit(status: Option<std::process::ExitStatus>, controlled: bool) -> bool {
    status.is_some_and(|status| {
        #[cfg(unix)]
        let interrupt = {
            use std::os::unix::process::ExitStatusExt;
            status.signal() == Some(libc::SIGINT) || status.code() == Some(128 + libc::SIGINT)
        };
        #[cfg(not(unix))]
        let interrupt = false;
        status.success() || (controlled && interrupt)
    })
}

/// Same built-in offline decoder used by flamegraphs, but bounded independently
/// and supervised through the producer's exact guardian/capture custody. Never
/// run a script extension, provider, live collection, sudo or network lookup.
fn validate_record(
    session: &measurements::Session,
    artifact: &std::path::Path,
    timeout: std::time::Duration,
    stream_bytes: u64,
) -> Result<(), String> {
    let artifact = artifact
        .canonicalize()
        .map_err(|error| format!("cannot resolve perf record artifact: {error}"))?;
    let executable = measurements::resolve_executable(session.tool_dir.as_deref(), "perf");
    let executable = crate::tools::host_executable(&executable)
        .ok_or("selected perf executable is unavailable for offline record validation")?;
    let mut launch = crate::launch::ProcessLaunch::new(executable);
    launch.arg("script").arg("-i").arg(&artifact);
    let (mut command, route) = measurements::producer_command(&mut launch)?;
    configure_offline_decoder_environment(&mut command, &session.dir);
    let mut captures = [
        crate::evidence::Capture::create(&session.dir.join("perf-validation.stdout"))?,
        crate::evidence::Capture::create(&session.dir.join("perf-validation.stderr"))?,
    ];
    let mut producer = measurements::Producer::spawn_with_dependency(
        &mut command,
        route,
        measurements::ProducerPolicy {
            stream_bytes,
            graceful: None,
        },
        [captures[0].writer()?, captures[1].writer()?],
        None,
    )?;
    let deadline = std::time::Instant::now() + timeout;
    let completion = loop {
        if let Some(completion) = producer.poll()? {
            break completion;
        }
        if std::time::Instant::now() >= deadline {
            let settled = producer.stop();
            return Err(format!(
                "offline perf record validation timed out; cleanup confirmed: {:?}; validation captures retained",
                settled.guardian_cleanup_confirmed
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    };
    if !completion.capture_complete
        || completion.guardian_cleanup_confirmed != Some(true)
        || !completion.exit.is_some_and(|status| status.success())
    {
        return Err(format!(
            "offline perf record decoder failed: exit {:?}; {}; validation captures retained",
            completion.exit,
            completion.error.unwrap_or_default()
        ));
    }
    // Empty decoded output is a valid zero-sample record only when the actual
    // installed decoder successfully consumed it. Never invent a sample count.
    for capture in &mut captures {
        capture.freeze()?;
        capture.consume()?;
    }
    Ok(())
}

/// Keep offline decoding independent from perf configuration without clearing
/// the process environment. The supported project-local perf build can
/// dereference host-home resources during `perf script` initialization and
/// crashes if its ambient home is replaced with an empty private directory.
fn configure_offline_decoder_environment(command: &mut Command, home: &std::path::Path) {
    command
        .env_remove("LD_PRELOAD")
        .env("DEBUGINFOD_URLS", "")
        .env("PERF_CONFIG_NOSYSTEM", "1")
        .env("PERF_CONFIG_NOGLOBAL", "1")
        .env("PERF_PAGER", "cat")
        .env("LC_ALL", "C")
        .current_dir(home);
}

fn preflight(context: &mut PreflightContext<'_>) -> Result<(), String> {
    if !super::super::selected_or_environment("perf", context.selected) {
        return Ok(());
    }
    let executable = measurements::resolve_executable(context.tool_dir, "perf");
    let probes = [
        (
            "stat",
            &[
                "stat",
                "--no-scale",
                "-e",
                "task-clock,context-switches,cpu-migrations,page-faults",
                "--inherit",
                "--",
                "sleep",
                "0.01",
            ][..],
        ),
        (
            "record",
            &["record", "-g", "-F", "19", "-o", "-", "--", "sleep", "0.01"][..],
        ),
    ];
    let mut needs_sudo = false;
    for (name, args) in probes {
        let output = perf_probe(&executable, args, false);
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                if perf_access_denied(&output.stderr) {
                    needs_sudo = true;
                    continue;
                }
                return Err(format!("perf {name} preflight failed ({}). Inspect the host configuration with `perf {name} -- sleep 0.01` and retry.", output.status));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err("perf is required for the selected collector. Run `asb tool install perf` and retry.".into()),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Err(format!("cannot execute perf at {}: {error}. Check parent-directory access and reinstall with `asb tool install perf`.", executable.display())),
            Err(error) => return Err(format!("cannot probe perf collector: {error}; check with `perf --version`")),
        }
    }
    if !needs_sudo {
        return Ok(());
    }
    if !measurements::passwordless_sudo_available() {
        return Err(perf_permission_error());
    }
    for (name, args) in probes {
        match perf_probe(&executable, args, true) {
            Ok(output) if output.status.success() => {}
            Ok(output) if perf_access_denied(&output.stderr) => {
                return Err(format!(
                    "perf is denied even through passwordless sudo during its {name} preflight. The host or container policy does not permit performance monitoring for this execution boundary. If the host policy permits it, run `sudo sysctl -w kernel.perf_event_paranoid=-1`; containers or other security policy can still deny perf. Inspect `sudo -n {} {}` and retry.",
                    executable.display(),
                    args.join(" ")
                ));
            }
            Ok(output) => {
                return Err(format!(
                    "privileged perf {name} preflight failed ({}). Inspect the host configuration with `sudo -n {} {}` and retry.",
                    output.status,
                    executable.display(),
                    args.join(" ")
                ));
            }
            Err(error) => {
                return Err(format!(
                    "cannot execute privileged perf at {}: {error}",
                    executable.display()
                ));
            }
        }
    }
    context.sudo_collectors.insert("perf".into());
    Ok(())
}

fn perf_probe(
    executable: &std::path::Path,
    args: &[&str],
    sudo: bool,
) -> std::io::Result<std::process::Output> {
    let mut command = if sudo {
        let mut command = Command::new("sudo");
        command.args(["-n", "--"]).arg(executable);
        command
    } else {
        Command::new(executable)
    };
    command.args(args).stdout(Stdio::null()).output()
}

fn perf_access_denied(stderr: &[u8]) -> bool {
    let stderr = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    stderr.contains("permission")
        || stderr.contains("perf_event_paranoid")
        || stderr.contains("access to performance monitoring")
        || stderr.contains("operation not permitted")
}

fn reserve_recording_artifact(path: &std::path::Path) -> Result<(), String> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("cannot reserve perf recording artifact: {error}"))?;
    Ok(())
}

use crate::measurements::{Collector, env_true, measurement_interval_ms, write_unavailable};
use std::collections::BTreeMap;
use std::io::{BufRead, Read};

#[derive(Default)]
struct PerfState {
    enabled: bool,
}

impl measurements::Session {
    pub(crate) fn start_perf(&mut self, pid: u32) -> Result<(), String> {
        if !self.state::<PerfState>().borrow().enabled {
            return Ok(());
        }
        let data = self.dir.join("perf.data");
        // A passwordless-sudo fallback may be needed for rootless isolated
        // process trees. Pre-creating the destination keeps its ownership
        // with the benchmark user so later `perf script`/Flamegraph processing
        // can read it without expanding privileged access.
        reserve_recording_artifact(&data)?;
        let stderr_path = self.dir.join("perf.stderr");
        let stdout_path = self.dir.join("perf.stdout");
        let captures = [
            crate::evidence::Capture::create(&stdout_path)?,
            crate::evidence::Capture::create(&stderr_path)?,
        ];
        // Attach to the runner root and inherit counters into every child.
        // Unlike a one-time comma-separated PID snapshot, perf's inheritance
        // follows helpers forked after attachment as well as descendants of
        // those helpers, while remaining scoped to this benchmark tree.
        let record_args = record_args(pid);
        let (mut command, route) = self.collector_command("perf")?;
        command
            // Keep perf data bounded enough for the offline flamegraph
            // renderer while retaining process-scoped call samples.
            .args(&record_args)
            .args(["-o"])
            .arg(&data);
        let mut collector = match Collector::spawn("perf", &ADAPTER, command, route, captures) {
            Ok(collector) => collector,
            Err(error) if !env_true("ASB_MEASURE_PERF") => {
                write_unavailable(&self.dir.join("perf.json"), "perf", error.to_string())?;
                return Ok(());
            }
            Err(error) => {
                return Err(format!(
                    "cannot start perf (disable with ASB_MEASURE_PERF=0): {error}"
                ));
            }
        };
        if let Some(reason) = startup_failure(&mut collector)? {
            write_unavailable(
                &self.dir.join("perf.json"),
                "perf",
                collector.startup_failure(reason),
            )?;
            return Ok(());
        }
        self.set_artifact(SharedArtifact::StackSamples, data);
        self.collectors.push(collector);
        let stat_stdout_path = self.dir.join("perf-stat.stdout");
        let stat_stderr_path = self.dir.join("perf-stat.stderr");
        let captures = [
            crate::evidence::Capture::create(&stat_stdout_path)?,
            crate::evidence::Capture::create(&stat_stderr_path)?,
        ];
        let stat_args = stat_args(pid, measurement_interval_ms());
        let (mut command, route) = self.collector_command("perf")?;
        command.args(&stat_args);
        let stat = Collector::spawn("perf-stat", &ADAPTER, command, route, captures);
        match stat {
            Ok(mut collector) => {
                if let Some(reason) = startup_failure(&mut collector)? {
                    write_unavailable(
                        &self.dir.join("perf-stat.json"),
                        "perf-stat",
                        collector.startup_failure(reason),
                    )?;
                } else {
                    self.collectors.push(collector);
                }
            }
            Err(error) => write_unavailable(
                &self.dir.join("perf-stat.json"),
                "perf-stat",
                error.to_string(),
            )?,
        }
        Ok(())
    }
}

fn startup_failure(collector: &mut Collector) -> Result<Option<String>, String> {
    // perf reports inaccessible PMUs immediately. Give its owned capture
    // worker one scheduling interval to observe that deterministic exit.
    std::thread::sleep(Duration::from_millis(25));
    collector.startup_exit()
}

// These are explicit failure bounds, never silent prefix limits. Diagnostics
// keep their separate 256 KiB bound. A measurement exceeding a bound fails
// without publishing plausible-looking partial totals or compressed timelines.
const MAX_STAT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_STAT_LINE: usize = 16 * 1024;
const MAX_STAT_POINTS: usize = 100_000;

#[derive(Default)]
struct PerfStat {
    metrics: BTreeMap<String, f64>,
    series: BTreeMap<String, Vec<f64>>,
    timestamps: BTreeMap<String, Vec<f64>>,
    quality: BTreeMap<String, EventQuality>,
}

#[derive(Default, serde::Serialize)]
struct EventQuality {
    observed_value: Option<f64>,
    value_unit: &'static str,
    observed_rows: u64,
    unavailable_rows: u64,
    unknown_quality_rows: u64,
    known_event_runtime_ns: f64,
    min_running_pct: Option<f64>,
    max_running_pct: Option<f64>,
}

fn parse_perf_stat(reader: impl BufRead) -> Result<PerfStat, String> {
    parse_perf_stat_bounded(reader, MAX_STAT_BYTES)
}

fn parse_perf_stat_bounded(mut reader: impl BufRead, max_bytes: u64) -> Result<PerfStat, String> {
    let mut parsed = PerfStat::default();
    let mut line = Vec::new();
    let mut bytes = 0_u64;
    loop {
        line.clear();
        let count = reader
            .by_ref()
            .take(MAX_STAT_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("cannot read complete perf stat output: {error}"))?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        if count > MAX_STAT_LINE || bytes > max_bytes {
            return Err(
                "perf stat output exceeds measurement parsing bounds; no partial totals published"
                    .into(),
            );
        }
        if !line.ends_with(b"\n") {
            return Err("unterminated perf stat output; no partial totals published".into());
        }
        let line = std::str::from_utf8(&line)
            .map_err(|_| "perf stat output is not UTF-8; no partial totals published")?;
        let fields = line.split(',').map(str::trim).collect::<Vec<_>>();
        let Some(event_index) = fields
            .iter()
            .position(|field| perf_metric_name(field).is_some())
        else {
            if line.contains(',') && !line.trim_start().starts_with('#') {
                return Err("unrecognized perf stat CSV row; no partial totals published".into());
            }
            continue;
        };
        if !matches!(event_index, 2 | 3) || fields.len() < event_index + 2 {
            return Err("malformed perf stat CSV row; no partial totals published".into());
        }
        let raw_value = event_index
            .checked_sub(2)
            .and_then(|index| fields.get(index))
            .ok_or("malformed perf stat counter; no partial totals published")?;
        let quality = parsed
            .quality
            .entry(fields[event_index].into())
            .or_default();
        quality.value_unit = if fields[event_index] == "task-clock" {
            "ms"
        } else {
            "count"
        };
        if matches!(*raw_value, "<not counted>" | "<not supported>") {
            quality.unavailable_rows += 1;
            continue;
        }
        let runtime = fields
            .get(event_index + 1)
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && *value >= 0.0);
        let running_pct = fields
            .get(event_index + 2)
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite() && (0.0..=100.0).contains(value));
        quality.observed_rows += 1;
        if let Some(runtime) = runtime {
            quality.known_event_runtime_ns += runtime;
            if !quality.known_event_runtime_ns.is_finite() {
                return Err("perf stat runtime overflow; no partial totals published".into());
            }
        }
        if let Some(pct) = running_pct {
            quality.min_running_pct = Some(quality.min_running_pct.map_or(pct, |old| old.min(pct)));
            quality.max_running_pct = Some(quality.max_running_pct.map_or(pct, |old| old.max(pct)));
        }
        if runtime.is_none() || running_pct.is_none() {
            quality.unknown_quality_rows += 1;
        }
        let value: f64 = raw_value
            .parse()
            .ok()
            .filter(|value: &f64| value.is_finite())
            .ok_or("invalid perf stat counter; no partial totals published")?;
        let Some(name) = perf_metric_name(fields[event_index]) else {
            continue;
        };
        if value < 0.0 {
            return Err("negative perf stat counter; no partial totals published".into());
        }
        let observed_value = quality.observed_value.unwrap_or(0.0) + value;
        if !observed_value.is_finite() {
            return Err("perf stat PMU counter overflow; no partial totals published".into());
        }
        quality.observed_value = Some(observed_value);
        let name = format!("perf.{name}");
        let timestamp = if event_index == 3 {
            Some(
                fields[0]
                    .parse::<f64>()
                    .ok()
                    .map(|seconds| seconds * 1_000.0)
                    .filter(|value| value.is_finite() && *value >= 0.0)
                    .ok_or("invalid perf stat interval timestamp; no partial totals published")?,
            )
        } else {
            None
        };
        let total = parsed.metrics.entry(name.clone()).or_default();
        *total += value;
        if !total.is_finite() {
            return Err("perf stat counter overflow; no partial totals published".into());
        }
        let values = parsed.series.entry(name.clone()).or_default();
        let timestamps = parsed.timestamps.entry(name).or_default();
        if let Some(timestamp) = timestamp {
            if timestamps.len() != values.len()
                || timestamps.last().is_some_and(|last| timestamp < *last)
            {
                return Err(
                    "inconsistent perf stat interval timestamps; no partial totals published"
                        .into(),
                );
            }
            // Hybrid CPUs may emit separate core/atom hardware counters for
            // the same interval. Sum those into one observed time point.
            if timestamps.last() == Some(&timestamp) {
                *values.last_mut().expect("timestamp has paired value") += value;
                continue;
            }
            timestamps.push(timestamp);
        } else if !timestamps.is_empty() {
            return Err(
                "mixed interval and aggregate perf stat output; no partial totals published".into(),
            );
        }
        if values.len() >= MAX_STAT_POINTS {
            return Err(
                "perf stat series exceeds measurement point bound; no partial totals published"
                    .into(),
            );
        }
        values.push(value);
    }
    parsed.timestamps.retain(|_, values| !values.is_empty());
    Ok(parsed)
}

#[cfg(test)]
pub(crate) fn parse_perf_stat_series(output: &str) -> BTreeMap<String, Vec<f64>> {
    parse_perf_stat(std::io::Cursor::new(output))
        .map(|parsed| parsed.series)
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn parse_perf_stat_metrics(output: &str) -> BTreeMap<String, f64> {
    // `perf stat -I` emits deltas, not cumulative snapshots. Scalar run
    // counters must sum those same observations used by the interval graphs.
    // A single non-interval observation retains its existing value.
    parse_perf_stat(std::io::Cursor::new(output))
        .map(|parsed| parsed.metrics)
        .unwrap_or_default()
}

fn perf_metric_name(event: &str) -> Option<&'static str> {
    match event {
        "task-clock" => Some("task_clock_ms"),
        "context-switches" => Some("context_switches"),
        "cpu-migrations" => Some("cpu_migrations"),
        "page-faults" => Some("page_faults"),
        "cycles" | "cpu_core/cycles/" | "cpu_atom/cycles/" => Some("observed_cycles_count"),
        "instructions" | "cpu_core/instructions/" | "cpu_atom/instructions/" => {
            Some("observed_instructions_count")
        }
        _ => None,
    }
}

pub(crate) fn perf_permission_error() -> String {
    "perf was denied access to performance events. A restrictive kernel.perf_event_paranoid setting is one possible cause.\nWhat to do: if your administrator approves system-wide perf access, run:\n  sudo sysctl -w kernel.perf_event_paranoid=-1\nTo persist the setting across reboots:\n  printf 'kernel.perf_event_paranoid=-1\\n' | sudo tee /etc/sysctl.d/99-asb-perf.conf\nThis changes access for the whole host; containers, capabilities, security policy or other restrictions may still prevent tracing. ASB does not create sudoers rules for perf.".into()
}

#[cfg(test)]
mod tests {
    use super::{record_args, stat_args};

    #[test]
    fn permission_error_suggests_kernel_policy_not_sudoers() {
        let message = super::perf_permission_error();
        assert!(message.contains("kernel.perf_event_paranoid=-1"));
        assert!(message.contains("sudo sysctl -w kernel.perf_event_paranoid=-1"));
        assert!(message.contains("/etc/sysctl.d/99-asb-perf.conf"));
        assert!(message.contains("does not create sudoers rules"));
        assert!(!message.contains("sudoers.d"));
    }

    #[test]
    fn access_denial_detection_covers_perf_and_kernel_diagnostics() {
        assert!(super::perf_access_denied(
            b"Failure to open event 'cpu-clock': Operation not permitted"
        ));
        assert!(super::perf_access_denied(
            b"Access to performance monitoring is limited; perf_event_paranoid setting is 2"
        ));
        assert!(!super::perf_access_denied(
            b"event is not supported by this PMU"
        ));
    }

    #[test]
    fn recording_artifact_is_reserved_once_by_the_benchmark_user() {
        let directory = std::env::temp_dir().join(format!(
            "asb-perf-reservation-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let artifact = directory.join("perf.data");
        super::reserve_recording_artifact(&artifact).unwrap();
        assert!(artifact.is_file());
        assert!(super::reserve_recording_artifact(&artifact).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn parses_perf_stat_counters_into_stable_metrics() {
        let metrics = super::parse_perf_stat_metrics(
            "12.34,msec,task-clock,100.00,\n8,,context-switches,100.00,\n2,,cpu-migrations,100.00,\n4,,page-faults,100.00,\n1234,,cycles,100.00,\n567,,instructions,100.00,\n",
        );
        assert_eq!(metrics["perf.task_clock_ms"], 12.34);
        assert_eq!(metrics["perf.context_switches"], 8.0);
        assert_eq!(metrics["perf.cpu_migrations"], 2.0);
        assert_eq!(metrics["perf.page_faults"], 4.0);
        assert_eq!(metrics["perf.observed_cycles_count"], 1234.0);
        assert_eq!(metrics["perf.observed_instructions_count"], 567.0);
        let series = super::parse_perf_stat_series(
            "0.000,12.34,msec,task-clock,100.00,\n1.000,20.00,msec,task-clock,100.00,\n0.000,8,,context-switches,100.00,\n",
        );
        assert_eq!(series["perf.task_clock_ms"], vec![12.34, 20.0]);
        assert_eq!(series["perf.context_switches"], vec![8.0]);
    }

    #[test]
    fn complete_perf_stat_file_not_diagnostic_prefix_drives_totals_and_timestamps() {
        use std::io::Write;
        let dir = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("perf-complete", None).attempt_id);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("perf-stat.stderr");
        let mut file = std::fs::File::create(&path).unwrap();
        // More than the former diagnostic cap; 800 observed seconds with a
        // deliberate missing interval must stay 800s, not compress to 799s.
        for interval in 1..=8000 {
            if interval == 500 {
                continue;
            }
            writeln!(
                file,
                "{:.3},2.00,msec,task-clock,100000000,100.00,,",
                interval as f64 / 10.0
            )
            .unwrap();
        }
        drop(file);
        assert!(path.metadata().unwrap().len() > 256 * 1024);
        let mut session =
            crate::measurements::Session::start_with_collectors(&dir, &[], None).unwrap();
        let status = std::process::Command::new("true").status().unwrap();
        let capture = crate::evidence::Capture::open(&path).unwrap();
        let data = super::process(
            &mut session,
            &crate::collectors::ProcessContext::new(
                "perf-stat",
                &status,
                ["", "diagnostic prefix only"],
                [&capture, &capture],
            ),
        )
        .unwrap();
        assert_eq!(data.status, "completed");
        assert_eq!(data.metrics["perf.task_clock_ms"], 15998.0);
        assert_eq!(data.series["perf.task_clock_ms"].len(), 7999);
        assert_eq!(
            data.timestamps["perf.task_clock_ms"].last(),
            Some(&800_000.0)
        );
        assert_eq!(data.timestamps["perf.task_clock_ms"][499], 50_100.0);
        std::fs::remove_file(&path).unwrap();
        // A removed/replaced name cannot redirect the admitted source.
        std::fs::write(&path, "0.1,999,,cycles,1,\n").unwrap();
        let retained = super::process(
            &mut session,
            &crate::collectors::ProcessContext::new(
                "perf-stat",
                &status,
                ["", "1,,cycles,1,"],
                [&capture, &capture],
            ),
        )
        .unwrap();
        assert_eq!(retained.metrics, data.metrics);
        assert_eq!(retained.timestamps, data.timestamps);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "0.1,999,,cycles,1,\n"
        );
        drop(session);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn perf_stat_gaps_and_hybrid_counters_preserve_real_interval_offsets() {
        let parsed = super::parse_perf_stat(std::io::Cursor::new(
            "0.1,4,,cpu_core/cycles/,1,\n0.1,3,,cpu_atom/cycles/,1,\n0.2,<not counted>,,cpu_core/cycles/,0,\n0.5,9,,cpu_core/cycles/,1,\n"
        )).unwrap();
        assert_eq!(parsed.metrics["perf.observed_cycles_count"], 16.0);
        assert_eq!(parsed.series["perf.observed_cycles_count"], [7.0, 9.0]);
        assert_eq!(
            parsed.timestamps["perf.observed_cycles_count"],
            [100.0, 500.0]
        );
    }

    #[test]
    fn raw_hybrid_counts_keep_pmu_quality_without_reversing_rounded_scaling() {
        let parsed = super::parse_perf_stat(std::io::Cursor::new(
            "0.1,400,,cpu_core/cycles/,100000000,99.00,,\n0.1,2,,cpu_atom/cycles/,1000,0.00,,\n0.2,<not counted>,,cpu_atom/cycles/,0,0.00,,\n0.3,9,,instructions,100,,\n"
        )).unwrap();
        assert_eq!(parsed.metrics["perf.observed_cycles_count"], 402.0);
        assert!(!parsed.metrics.contains_key("perf.cycles"));
        let atom = &parsed.quality["cpu_atom/cycles/"];
        assert_eq!(atom.observed_value, Some(2.0));
        assert_eq!(atom.min_running_pct, Some(0.0));
        assert_eq!(atom.known_event_runtime_ns, 1000.0);
        assert_eq!(atom.unavailable_rows, 1);
        assert_eq!(
            parsed.quality["cpu_core/cycles/"].observed_value,
            Some(400.0)
        );
        assert_eq!(parsed.quality["instructions"].unknown_quality_rows, 1);
        let args = stat_args(1234, 100);
        assert!(args.iter().any(|arg| arg == "--no-scale"));
        assert!(!args.iter().any(|arg| arg == "--all-cpus"));
    }

    #[test]
    fn perf_stat_refuses_partial_totals_on_invalid_or_excessive_input() {
        let two_rows = "0.1,2,,cycles,1,\n0.2,3,,cycles,1,\n";
        assert!(super::parse_perf_stat_bounded(std::io::Cursor::new(two_rows), 20).is_err());
        for suffix in [
            "0.2,NaN,,cycles,1,\n",
            "0.2,inf,,cycles,1,\n",
            "0.2,-1,,cycles,1,\n",
            "0.05,3,,cycles,1,\n",
            "not-time,3,,cycles,1,\n",
            "3,,cycles,1,\n",
            "0.2,4,,cpu_core/cyc",
            "0.2,4,,cycles",
            "0.2,4,,unknown-event,1,\n",
            "cycles,1,\n",
            "0.2,4,msec,extra,task-clock,1,\n",
            "0.2,4,,cycles\n",
        ] {
            let input = format!("0.1,2,,cycles,1,\n{suffix}");
            assert!(
                super::parse_perf_stat(std::io::Cursor::new(input)).is_err(),
                "{suffix}"
            );
        }
        assert!(
            super::parse_perf_stat(std::io::Cursor::new(vec![b'x'; super::MAX_STAT_LINE + 1]))
                .is_err()
        );
        assert!(super::parse_perf_stat(std::io::Cursor::new(vec![0xff, b'\n'])).is_err());
        let input = "1,,cycles,1,\n".repeat(super::MAX_STAT_POINTS + 1);
        assert!(super::parse_perf_stat(std::io::Cursor::new(input)).is_err());
        struct Fails;
        impl std::io::Read for Fails {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("fixture read failure"))
            }
        }
        assert!(super::parse_perf_stat(std::io::BufReader::new(Fails)).is_err());
    }

    #[test]
    fn scalar_perf_counters_sum_intervals_instead_of_reporting_the_final_sample() {
        let input = "0.100,12.34,msec,task-clock,100.00,\n0.200,20.00,msec,task-clock,100.00,\n0.100,8,,context-switches,100.00,\n0.200,2,,context-switches,100.00,\n0.200,<not supported>,,cycles,0,\n0.200,<not counted>,,instructions,0,\n";
        let metrics = super::parse_perf_stat_metrics(input);
        assert_eq!(metrics["perf.task_clock_ms"], 32.34);
        assert_eq!(metrics["perf.context_switches"], 10.0);
        assert!(!metrics.contains_key("perf.observed_cycles_count"));
        assert!(!metrics.contains_key("perf.observed_instructions_count"));
        for (name, values) in super::parse_perf_stat_series(input) {
            assert_eq!(metrics[&name], values.iter().sum::<f64>());
        }
        assert!(super::parse_perf_stat_metrics("unavailable\n").is_empty());
        assert!(
            super::parse_perf_stat_metrics("1e308,,cycles,100,\n1e308,,cycles,100,\n").is_empty()
        );
    }

    #[test]
    fn perf_record_is_root_attached_and_inherits_all_descendants() {
        assert_eq!(
            record_args(42),
            vec![
                "record",
                "-e",
                "cpu-clock",
                "-g",
                "-F",
                "19",
                "--inherit",
                "-p",
                "42"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn offline_decoder_environment_preserves_home_but_removes_loader_injection() {
        let home =
            std::env::temp_dir().join(format!("asb-perf-decoder-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let mut command = std::process::Command::new("sh");
        command.args([
            "-c",
            "test \"$HOME\" = \"$1\" && test -z \"${LD_PRELOAD+x}\"",
            "sh",
            home.to_str().unwrap(),
        ]);
        command.env("HOME", &home);
        command.env("LD_PRELOAD", "must-not-leak");
        super::configure_offline_decoder_environment(&mut command, &home);
        assert!(command.status().unwrap().success());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn perf_stat_uses_the_same_process_tree_scope() {
        assert_eq!(
            stat_args(42, 1000),
            vec![
                "stat",
                "--no-scale",
                "-I",
                "1000",
                "-x,",
                "-e",
                "task-clock,context-switches,cpu-migrations,page-faults",
                "--inherit",
                "-p",
                "42"
            ]
        );
    }
}
