use super::super::{
    ArtifactDisplay, CollectorAdapter, LifecycleEvent, PreflightContext, ProcessContext,
    ProcessData,
};
use crate::measurements;
pub(super) struct PerfettoCollector;
pub(super) static ADAPTER: PerfettoCollector = PerfettoCollector;

impl CollectorAdapter for PerfettoCollector {
    fn id(&self) -> &'static str {
        "perfetto"
    }
    fn environment_flag(&self) -> Option<&'static str> {
        Some("ASB_MEASURE_PERFETTO")
    }
    fn tool_id(&self) -> Option<&'static str> {
        Some("perfetto")
    }
    fn requires_pid(&self) -> bool {
        true
    }
    fn prioritizes_artifact(&self, name: &str) -> bool {
        name.starts_with("perfetto-metric-")
    }
    fn display_artifact(&self, name: &str) -> Option<ArtifactDisplay> {
        (name.starts_with("perfetto-metric-") && name.ends_with(".svg")).then_some(
            ArtifactDisplay::Svg {
                class: "perfetto-metric",
                label: "Perfetto metric SVG",
            },
        )
    }
    fn preflight(&self, context: &mut PreflightContext<'_>) -> Result<(), String> {
        preflight(context)
    }
    fn configure(
        &self,
        session: &mut measurements::Session,
        selected: &[String],
    ) -> Result<(), String> {
        session.state::<PerfettoState>().borrow_mut().enabled =
            super::super::selected_or_environment("perfetto", selected);
        Ok(())
    }
    fn graceful_shutdown(&self, _: &str) -> bool {
        true
    }
    fn process(
        &self,
        session: &mut measurements::Session,
        context: &ProcessContext<'_>,
    ) -> Result<ProcessData, String> {
        process(session, context)
    }
    fn finish(&self, session: &mut measurements::Session) -> Result<(), String> {
        finish(session)
    }
    fn start(&self, session: &mut measurements::Session) -> Result<(), String> {
        session.start_perfetto()
    }
    fn lifecycle_event(
        &self,
        session: &mut measurements::Session,
        event: LifecycleEvent,
    ) -> Result<(), String> {
        let pid = std::process::id();
        let id = session.trace_id();
        let marker = match event {
            LifecycleEvent::RunStarted => format!("I|{pid}|asb.run.started.{id}"),
            LifecycleEvent::RunFinishing => format!("I|{pid}|asb.run.finishing.{id}"),
            LifecycleEvent::RunFailed => format!("I|{pid}|asb.run.failed.{id}"),
            LifecycleEvent::PhaseBegin(phase) => format!("B|{pid}|asb.{}.{id}", phase.name()),
            LifecycleEvent::PhaseEnd(_) => "E".into(),
            LifecycleEvent::PhaseFailed(phase) => {
                format!("I|{pid}|asb.{}.failed.{id}", phase.name())
            }
            LifecycleEvent::MeasurementsReady => format!("I|{pid}|asb.measurements.ready.{id}"),
            LifecycleEvent::AgentSpawn => format!("I|{pid}|asb.agent.spawn.{id}"),
            LifecycleEvent::AgentExecuteBegin => format!("B|{pid}|asb.agent.execute.{id}"),
            LifecycleEvent::AgentStarted => format!("I|{pid}|asb.agent.started.{id}"),
            LifecycleEvent::AgentSpawnFailed => format!("I|{pid}|asb.agent.spawn_failed.{id}"),
            LifecycleEvent::AgentAttachFailed => format!("I|{pid}|asb.agent.attach_failed.{id}"),
            LifecycleEvent::AgentWaitFailed => format!("I|{pid}|asb.agent.wait_failed.{id}"),
            LifecycleEvent::AgentExecuteEnd => "E".into(),
            LifecycleEvent::AgentExited => format!("I|{pid}|asb.agent.exited.{id}"),
        };
        session.write_trace_marker(&marker)
    }
    fn attach_pid(&self, session: &mut measurements::Session, pid: u32) -> Result<(), String> {
        session.set_perfetto_pid(pid);
        Ok(())
    }
}

fn process(
    session: &mut measurements::Session,
    context: &ProcessContext<'_>,
) -> Result<ProcessData, String> {
    session.verify_perfetto_marker_capture()?;
    let mut result = ProcessData::from_output(context);
    result.metrics = session.perfetto_trace_metrics();
    (result.series, result.timestamps) = session.perfetto_trace_series();
    let state = session.state::<PerfettoState>();
    let mut state = state.borrow_mut();
    state.metrics = result.metrics.clone();
    result.artifact = state
        .trace
        .as_ref()
        .filter(|path| path.is_file())
        .map(|path| measurements::file_name(path));
    if context.status.code().is_none()
        && state
            .trace
            .as_ref()
            .is_some_and(|path| path.metadata().is_ok_and(|metadata| metadata.len() > 0))
    {
        result.status = "completed".into();
    }
    if context.stderr.contains("FtraceController") {
        result.status = "unavailable".into();
    }
    Ok(result)
}

fn finish(session: &mut measurements::Session) -> Result<(), String> {
    let state = session.state::<PerfettoState>();
    let state = state.borrow();
    if !state.enabled {
        return Ok(());
    }
    measurements::write_json(
        &session.dir.join("perfetto-tracepoints.json"),
        &serde_json::json!({
            "schema_version": 1,
            "collector": "perfetto",
            "tracepoint_count": state.tracepoint_count,
            "trace_marker": state.trace_marker.as_ref().map(|path| path.display().to_string()),
            "trace": state.trace.as_ref().map(|path| measurements::file_name(path)),
        }),
    )?;
    write_perfetto_metric_svgs(
        &session.dir,
        state.trace.as_deref(),
        state.tracepoint_count,
        &state.metrics,
    )
}

fn preflight(context: &mut PreflightContext<'_>) -> Result<(), String> {
    if !super::super::selected_or_environment("perfetto", context.selected) {
        return Ok(());
    }
    let executable = measurements::resolve_executable(context.tool_dir, "perfetto");
    if !executable.is_file() {
        return Err("Perfetto is required for the selected collector. Run `asb tool install perfetto` and retry.".into());
    }
    let marker = checked_trace_marker_path()?;
    match probe_trace_marker_io(&marker) {
        Ok(()) => preflight_perfetto(context, &executable),
        Err(error) if marker_probe_allows_fallback(&marker, &error) => {
            // Some container kernels expose trace_marker but reject writes
            // with EBADF. Process-bounded trace collection remains valid;
            // post-processing will use the process lifetime instead of ASB
            // lifecycle markers.
            preflight_perfetto(context, &executable)
        }
        Err(error) => Err(marker_access_error(&marker, &error)),
    }
}

#[cfg(test)]
fn preflight_with_marker(context: &mut PreflightContext<'_>, marker: &Path) -> Result<(), String> {
    let executable = measurements::resolve_executable(context.tool_dir, "perfetto");
    match probe_trace_marker_io(marker) {
        Ok(()) => preflight_perfetto(context, &executable),
        Err(error) if marker_probe_allows_fallback(marker, &error) => {
            preflight_perfetto(context, &executable)
        }
        Err(error) => Err(marker_access_error(marker, &error)),
    }
}

fn preflight_perfetto(context: &mut PreflightContext<'_>, executable: &Path) -> Result<(), String> {
    probe_trace_processor(context.tool_dir)?;
    if !executable.is_file() {
        return Ok(());
    }
    if perfetto_probe(executable, context.measurement_dir, false)? {
        return Ok(());
    }
    if perfetto_probe(executable, context.measurement_dir, true)? {
        context.sudo_collectors.insert("perfetto".into());
        Ok(())
    } else {
        Err(measurements::passwordless_sudo_error(
            "perfetto", executable,
        ))
    }
}

fn probe_trace_processor(tool_dir: Option<&Path>) -> Result<(), String> {
    let Some(processor) = tool_dir.map(|dir| dir.join("perfetto/bin/trace_processor_shell")) else {
        return Err("Perfetto trace processing needs the project-local trace processor. Run `asb tool install perfetto` and retry.".into());
    };
    let mut child = Command::new(&processor)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot execute Perfetto trace processor {}: {error}. Check directory access and run `asb tool install perfetto` to reinstall it.", processor.display()))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(format!(
                    "Perfetto trace processor {} failed its preflight ({status}). Run `asb tool install perfetto` to reinstall it.",
                    processor.display()
                ));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "Perfetto trace processor {} timed out during preflight. Run `asb tool install perfetto` to reinstall it.",
                    processor.display()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "cannot check Perfetto trace processor {}: {error}. Run `asb tool install perfetto` to reinstall it.",
                    processor.display()
                ));
            }
        }
    }
}

use crate::collectors;
use crate::measurements::{
    Collector, bounded, env_true, escape_svg, parse_float, safe_svg_name, write_unavailable,
};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Default)]
struct PerfettoState {
    enabled: bool,
    trace: Option<PathBuf>,
    pid: Option<u32>,
    trace_marker: Option<PathBuf>,
    trace_marker_output: Option<File>,
    tracepoint_count: u64,
    metrics: BTreeMap<String, f64>,
}

impl measurements::Session {
    pub(crate) fn set_perfetto_pid(&mut self, pid: u32) {
        self.state::<PerfettoState>().borrow_mut().pid = Some(pid);
    }

    pub(crate) fn start_perfetto(&mut self) -> Result<(), String> {
        if !self.state::<PerfettoState>().borrow().enabled {
            return Ok(());
        }
        let marker = checked_trace_marker_path()?;
        self.start_perfetto_with_marker(&marker)
    }

    fn start_perfetto_with_marker(&mut self, marker: &Path) -> Result<(), String> {
        let config = self.dir.join("perfetto-scheduling.pbtx");
        fs::write(&config, perfetto_config(None))
            .map_err(|error| format!("cannot write Perfetto trace configuration: {error}"))?;
        let trace = self.dir.join("perfetto-scheduling.perfetto-trace");
        // A sudo-launched Tracebox otherwise creates a root-only 0600 trace.
        // Precreating the artifact keeps its ownership with the benchmark user.
        File::create(&trace)
            .map_err(|error| format!("cannot create Perfetto trace artifact: {error}"))?;
        let stdout_path = self.dir.join("perfetto.stdout");
        let stderr_path = self.dir.join("perfetto.stderr");
        File::create(&stdout_path)
            .map_err(|error| format!("cannot create Perfetto output: {error}"))?;
        let mut marker_output = open_trace_marker(marker)?;
        let child = self
            .command("perfetto")
            .args(["--txt", "--notify-fd", "1", "-c"])
            .arg(&config)
            .args(["-o"])
            .arg(&trace)
            .stdout(Stdio::piped())
            .stderr(Stdio::from(File::create(&stderr_path).map_err(
                |error| format!("cannot create Perfetto error output: {error}"),
            )?))
            .spawn();
        match child {
            Ok(mut child) => {
                let mut pipe = child
                    .stdout
                    .take()
                    .ok_or_else(|| "cannot receive Perfetto readiness notification".to_owned())?;
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                std::thread::spawn(move || {
                    let mut status = [1u8];
                    let result = pipe.read_exact(&mut status).map(|()| status[0]);
                    let _ = tx.send(result);
                });
                let ready = rx.recv_timeout(Duration::from_secs(35));
                if !matches!(ready, Ok(Ok(0))) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "Perfetto did not report that tracing is ready: {ready:?}"
                    ));
                }
                self.collectors.push(Collector {
                    name: "perfetto".into(),
                    adapter: collectors::find("perfetto").expect("registered collector"),
                    child,
                    started: Instant::now(),
                    stdout_path,
                    stderr_path,
                });
                // Register the child before validating the live marker, so
                // Session's drop path stops it if validation fails. A
                // container tracefs instance may reject marker writes with
                // EBADF even after readiness; retain the trace and use the
                // scoped process lifetime in that case.
                if tracing_status(marker) == Some(false) {
                    return Err(format!(
                        "Perfetto reported readiness, but tracing is off in {}. The benchmark was not started. Check the Perfetto collector logs and other tracing sessions; ASB will not change the host-wide tracing state.",
                        marker
                            .parent()
                            .unwrap_or(marker)
                            .join("tracing_on")
                            .display()
                    ));
                }
                let ready_marker =
                    format!("I|{}|asb.trace.ready.{}", std::process::id(), self.trace_id);
                let marker_output =
                    match write_active_marker_io(&mut marker_output, marker, &ready_marker) {
                        Ok(()) => Some(marker_output),
                        Err(error) if marker_probe_allows_fallback(marker, &error) => None,
                        Err(error) => return Err(marker_access_error(marker, &error)),
                    };
                let state = self.state::<PerfettoState>();
                let mut state = state.borrow_mut();
                state.trace = Some(trace);
                state.trace_marker = marker_output.as_ref().map(|_| marker.to_path_buf());
                state.trace_marker_output = marker_output;
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

    pub(crate) fn write_trace_marker(&mut self, event: &str) -> Result<(), String> {
        let state = self.state::<PerfettoState>();
        let mut state = state.borrow_mut();
        let path = state
            .trace_marker
            .clone()
            .unwrap_or_else(|| PathBuf::from("trace_marker"));
        let Some(output) = state.trace_marker_output.as_mut() else {
            return Ok(());
        };
        match write_active_marker_io(output, &path, event) {
            Ok(()) => {
                state.tracepoint_count = state.tracepoint_count.saturating_add(1);
                Ok(())
            }
            Err(error) if marker_probe_allows_fallback(&path, &error) => {
                // A tracefs inode can become unusable after Perfetto starts
                // (notably in containers where tracing_on is controlled by a
                // different namespace). Keep the collector alive and let
                // scoped process bounds provide the time window.
                state.trace_marker = None;
                state.trace_marker_output = None;
                Ok(())
            }
            Err(error) => Err(marker_access_error(&path, &error)),
        }
    }

    fn verify_perfetto_marker_capture(&mut self) -> Result<(), String> {
        let state = self.state::<PerfettoState>();
        let state = state.borrow();
        if state.trace_marker_output.is_none() {
            // Marker writes are optional on kernels that expose an unusable
            // tracefs inode. The trace remains process-scoped and queries
            // fall back to the recorded process lifetime.
            return Ok(());
        }
        let trace = state.trace.as_ref().ok_or_else(|| {
            "Perfetto did not produce a trace to verify lifecycle markers".to_owned()
        })?;
        let tool_dir = self.tool_dir.as_ref().ok_or_else(|| {
            "Perfetto trace processor is unavailable; cannot verify lifecycle markers".to_owned()
        })?;
        verify_trace_marker_capture(
            &tool_dir.join("perfetto/bin/trace_processor_shell"),
            trace,
            &self.trace_id,
        )
    }

    #[cfg(test)]
    pub(crate) fn set_test_marker(&mut self, path: &Path) {
        let state = self.state::<PerfettoState>();
        let mut state = state.borrow_mut();
        state.trace_marker = Some(path.to_path_buf());
        state.trace_marker_output = Some(File::create(path).expect("create test marker"));
    }

    /// Summarize the native trace with the installer-provided trace processor.
    /// The trace itself remains the authoritative artifact; these aggregates
    /// make scheduler activity comparable in ASB's offline SVG and HTML views.
    pub(crate) fn perfetto_trace_metrics(&mut self) -> BTreeMap<String, f64> {
        let mut metrics = BTreeMap::new();
        let state = self.state::<PerfettoState>();
        let state = state.borrow();
        let Some(trace) = state.trace.as_ref().filter(|path| path.is_file()) else {
            return metrics;
        };
        metrics.insert(
            "perfetto.trace_bytes".into(),
            trace.metadata().map_or(0, |metadata| metadata.len()) as f64,
        );
        metrics.insert("perfetto.tracepoints".into(), state.tracepoint_count as f64);
        let Some(tool_dir) = self.tool_dir.as_ref() else {
            return metrics;
        };
        let processor = tool_dir.join("perfetto/bin/trace_processor_shell");
        if !processor.is_file() {
            return metrics;
        }
        for (metric, query) in perfetto_metric_queries(state.pid, &self.trace_id) {
            let Ok(output) = Command::new(&processor)
                .args(["query"])
                .arg(trace)
                .arg(query)
                .output()
            else {
                continue;
            };
            if output.status.success()
                && let Some(value) = bounded(&output.stdout).lines().rev().find_map(parse_float)
            {
                metrics.insert(metric, value);
            }
        }
        metrics
    }

    /// Extract timestamped scheduler samples directly from the Perfetto trace.
    /// The timestamp column is retained in milliseconds so comparison graphs
    /// use trace time rather than synthetic point indexes.
    pub(crate) fn perfetto_trace_series(
        &mut self,
    ) -> (BTreeMap<String, Vec<f64>>, BTreeMap<String, Vec<f64>>) {
        let mut series = BTreeMap::new();
        let mut timestamps = BTreeMap::new();
        let state = self.state::<PerfettoState>();
        let state = state.borrow();
        let Some(trace) = state.trace.as_ref().filter(|path| path.is_file()) else {
            return (series, timestamps);
        };
        let Some(tool_dir) = self.tool_dir.as_ref() else {
            return (series, timestamps);
        };
        let processor = tool_dir.join("perfetto/bin/trace_processor_shell");
        if !processor.is_file() {
            return (series, timestamps);
        }
        for (name, query) in perfetto_series_queries(state.pid, &self.trace_id) {
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
                timestamps.insert(name.clone(), sample_timestamps);
                series.insert(name, values);
            }
        }
        (series, timestamps)
    }
}

pub(crate) fn parse_perfetto_series_output(output: &str) -> (Vec<f64>, Vec<f64>) {
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

/// Write one dependency-free SVG per stable aggregate metric.
pub(crate) fn write_perfetto_metric_svgs(
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

fn checked_trace_marker_path() -> Result<PathBuf, String> {
    let paths = [
        PathBuf::from("/sys/kernel/tracing/trace_marker"),
        PathBuf::from("/sys/kernel/debug/tracing/trace_marker"),
    ];
    let mut denied = None;
    for path in paths {
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                if denied.is_none() {
                    denied = Some((path, error));
                }
            }
            _ => {}
        }
    }
    match denied {
        Some((path, error)) => Err(marker_access_error(&path, &error)),
        None => Err(perfetto_tracefs_error()),
    }
}

fn open_trace_marker(path: &Path) -> Result<File, String> {
    // Opening checks permissions without writing to an inactive trace buffer.
    OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| marker_access_error(path, &error))
}

fn probe_trace_marker_io(path: &Path) -> std::io::Result<()> {
    let mut marker = OpenOptions::new().write(true).open(path)?;
    write!(marker, "I|{}|asb.preflight", std::process::id())
}

fn marker_probe_allows_fallback(path: &Path, error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(9) && path.starts_with("/sys/kernel/")
}

fn tracing_status(path: &Path) -> Option<bool> {
    let value = fs::read_to_string(path.parent()?.join("tracing_on")).ok()?;
    match value.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
fn validate_active_marker(
    output: &mut impl Write,
    path: &Path,
    trace_id: &str,
) -> Result<(), String> {
    if tracing_status(path) == Some(false) {
        return Err(format!(
            "Perfetto reported readiness, but tracing is off in {}. The benchmark was not started. Check the Perfetto collector logs and other tracing sessions; ASB will not change the host-wide tracing state.",
            path.parent().unwrap_or(path).join("tracing_on").display()
        ));
    }
    let marker = format!("I|{}|asb.trace.ready.{trace_id}", std::process::id());
    write_active_marker(output, path, &marker)
}

#[cfg(test)]
fn write_active_marker(output: &mut impl Write, path: &Path, event: &str) -> Result<(), String> {
    write_active_marker_io(output, path, event).map_err(|error| {
        if error.raw_os_error() == Some(9) {
            let state = match tracing_status(path) {
                Some(true) => "on",
                Some(false) => "off",
                None => "unavailable to the current user",
            };
            format!("cannot write ASB lifecycle marker to {}: {error}. Tracefs rejected the write; tracing_on is {state}. The Perfetto trace may be incomplete. Check whether tracing was stopped by another session and inspect {} (for example, `sudo cat '{}'`); retry after resolving the tracing conflict. ASB does not change host-wide tracing state.", path.display(), path.parent().unwrap_or(path).join("tracing_on").display(), path.parent().unwrap_or(path).join("tracing_on").display())
        } else {
            marker_access_error(path, &error)
        }
    })
}

fn write_active_marker_io(
    output: &mut impl Write,
    _path: &Path,
    event: &str,
) -> std::io::Result<()> {
    output.write_all(event.as_bytes())
}

fn verify_trace_marker_capture(
    processor: &Path,
    trace: &Path,
    trace_id: &str,
) -> Result<(), String> {
    // trace_id is generated from a process ID and atomic counter, not user input.
    let query = format!("SELECT COUNT(*) FROM slice WHERE name='asb.trace.ready.{trace_id}'");
    let output = Command::new(processor)
        .arg("query")
        .arg(trace)
        .arg(query)
        .output()
        .map_err(|error| {
            format!(
                "cannot verify Perfetto lifecycle markers with {}: {error}",
                processor.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "cannot verify lifecycle markers in Perfetto trace {}: trace processor failed: {}",
            trace.display(),
            bounded(&output.stderr)
        ));
    }
    let count = bounded(&output.stdout).lines().rev().find_map(parse_float);
    match count {
        Some(n) if n >= 1.0 => Ok(()),
        Some(_) => Err(format!(
            "Perfetto trace {} did not capture the ASB startup marker; its lifecycle timings may be incomplete. Check ftrace/print collection and conflicting tracing sessions, then retry.",
            trace.display()
        )),
        None => Err(format!(
            "cannot verify ASB lifecycle markers in Perfetto trace {}: trace processor returned an invalid result",
            trace.display()
        )),
    }
}

fn marker_access_error(path: &Path, error: &std::io::Error) -> String {
    if error.kind() != std::io::ErrorKind::PermissionDenied {
        return format!(
            "cannot write ASB lifecycle markers to {}: {error}; check the tracefs mount and host security policy before rerunning",
            path.display()
        );
    }
    let quoted = format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    format!(
        "cannot write ASB lifecycle markers to {}: {error}. Tracebox sudo access does not grant ASB marker access.\nWhat to do (systemd Linux; gives the current user ownership and mode 0600, replacing existing access on this marker):\n  printf 'z %s 0600 %s - -\\n' {quoted} \"$(id -u)\" | sudo sh -c 'set -C; cat > \"/etc/tmpfiles.d/asb-perfetto-marker-$1.conf\"' sh \"$(id -u)\" && sudo systemd-tmpfiles --create \"/etc/tmpfiles.d/asb-perfetto-marker-$(id -u).conf\"\nThis installs a boot-time rule and applies it now. If the rule already exists, inspect it before changing it. The user also needs search access to the parent directories; check with `namei -l {quoted}`. Remounting tracefs or a host security policy can still require additional administrator action.",
        path.display()
    )
}

/// Build Perfetto queries that restrict scheduler data to the benchmark
/// process and every descendant it launched.  Tracebox cannot apply a pid
/// filter on all supported versions, so the scope is enforced by the offline
/// trace processor instead of silently reporting host-wide activity.
pub(crate) fn perfetto_metric_queries(pid: Option<u32>, trace_id: &str) -> Vec<(String, String)> {
    let Some(pid) = pid else {
        return Vec::new();
    };
    let scope = perfetto_scope_sql(pid, trace_id);
    vec![
        (
            "perfetto.scheduler_events".into(),
            format!(
                "{scope} SELECT COUNT(*) FROM sched s JOIN thread th ON s.utid=th.utid CROSS JOIN bounds WHERE th.upid IN tree AND s.ts >= bounds.start_ts AND s.ts < bounds.end_ts"
            ),
        ),
        (
            "perfetto.processes".into(),
            format!("{scope} SELECT COUNT(*) FROM process WHERE upid IN tree"),
        ),
        (
            "perfetto.threads".into(),
            format!("{scope} SELECT COUNT(*) FROM thread WHERE upid IN tree"),
        ),
    ]
}

pub(crate) fn perfetto_series_queries(pid: Option<u32>, trace_id: &str) -> Vec<(String, String)> {
    let Some(pid) = pid else {
        return Vec::new();
    };
    let scope = perfetto_scope_sql(pid, trace_id);
    vec![
        (
            "perfetto.scheduler_events_per_s".into(),
            format!(
                "{scope} SELECT CAST((s.ts-bounds.start_ts)/1000000000 AS INTEGER) AS timestamp_s, COUNT(*) AS value FROM sched s JOIN thread th ON s.utid=th.utid CROSS JOIN bounds WHERE th.upid IN tree AND s.ts >= bounds.start_ts AND s.ts < bounds.end_ts GROUP BY timestamp_s ORDER BY timestamp_s"
            ),
        ),
        (
            "perfetto.scheduler_duration_ms_per_s".into(),
            format!(
                "{scope}, relevant AS (SELECT s.ts, s.dur FROM sched s JOIN thread th ON s.utid=th.utid CROSS JOIN bounds WHERE th.upid IN tree AND s.ts < bounds.end_ts AND s.ts+s.dur > bounds.start_ts), bucket_ranges(bucket) AS (SELECT 0 UNION ALL SELECT bucket+1 FROM bucket_ranges CROSS JOIN bounds WHERE bounds.start_ts+(bucket+1)*1000000000 < bounds.end_ts) SELECT bucket_ranges.bucket AS timestamp_s, COALESCE(SUM(MAX(0, MIN(r.ts+r.dur,bounds.start_ts+(bucket_ranges.bucket+1)*1000000000)-MAX(r.ts,bounds.start_ts+bucket_ranges.bucket*1000000000)))/1000000.0,0) AS value FROM bucket_ranges CROSS JOIN bounds LEFT JOIN relevant r ON r.ts < bounds.start_ts+(bucket_ranges.bucket+1)*1000000000 AND r.ts+r.dur > bounds.start_ts+bucket_ranges.bucket*1000000000 GROUP BY bucket_ranges.bucket ORDER BY bucket_ranges.bucket"
            ),
        ),
    ]
}

pub(crate) fn perfetto_scope_sql(pid: u32, trace_id: &str) -> String {
    // Prefer explicit ASB lifecycle markers when tracefs captured them, and
    // fall back to the process tree's lifetime when markers are unavailable.
    format!(
        "WITH RECURSIVE tree(upid) AS (SELECT upid FROM process WHERE pid={pid} UNION ALL SELECT p.upid FROM process p JOIN tree t ON p.parent_upid=t.upid), process_bounds AS (SELECT MIN(start_ts) AS process_start, MAX(COALESCE(end_ts,(SELECT MAX(ts+dur) FROM sched))) AS process_end FROM process WHERE upid IN tree), marker_bounds AS (SELECT MIN(ts) FILTER (WHERE name='asb.agent.started.{trace_id}') AS marker_start, MAX(ts) FILTER (WHERE name='asb.agent.exited.{trace_id}') AS marker_end FROM slice), bounds AS (SELECT COALESCE(marker_start,process_start) AS start_ts, COALESCE(marker_end,process_end) AS end_ts FROM process_bounds CROSS JOIN marker_bounds)",
    )
}

pub(crate) fn perfetto_config(pid: Option<u32>) -> String {
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

/// Probe the exact ftrace configuration used by ASB. `--background-wait`
/// waits for Perfetto to acknowledge every data source, so a successful
/// process launch cannot mask an unavailable tracefs instance.
pub(crate) fn perfetto_probe(
    executable: &Path,
    measurement_dir: &Path,
    sudo: bool,
) -> Result<bool, String> {
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
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output();
    let _ = fs::remove_file(&config);
    let _ = fs::remove_file(&trace);
    match result {
        Ok(output) => {
            if !output.status.success() {
                return Ok(false);
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            let unavailable_markers = [
                "Failed to reset ftrace",
                "Failed to create FtraceController",
                "Data source id=1 not found",
            ];
            Ok(!unavailable_markers
                .iter()
                .any(|marker| stderr.contains(marker)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && sudo => Ok(false),
        Err(error) => Err(format!(
            "cannot execute Perfetto at {}: {error}. Check parent-directory access and reinstall with `asb tool install perfetto`.",
            executable.display()
        )),
    }
}

pub(crate) fn perfetto_tracefs_error() -> String {
    "Perfetto needs the Linux tracefs ftrace interface, but ASB could not find trace_marker under /sys/kernel/tracing or /sys/kernel/debug/tracing. This is a host kernel interface, not an installable project tool. What to do: mount tracefs or enable kernel tracing, then rerun. If mounting tracefs requires privileges, grant the required passwordless sudo access before starting the benchmark; ASB never prompts for a password.".into()
}

#[cfg(test)]
mod marker_tests {
    use super::*;

    #[test]
    fn permission_failure_has_persistent_scoped_remediation() {
        let path = Path::new("/sys/kernel/tracing/trace_marker");
        let error = marker_access_error(
            path,
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        assert!(
            error
                .contains("cannot write ASB lifecycle markers to /sys/kernel/tracing/trace_marker")
        );
        assert!(error.contains("systemd-tmpfiles --create"));
        assert!(error.contains("$(id -u)"));
        assert!(error.contains("mode 0600"));
        assert!(error.contains("parent directories"));
        let command = error
            .lines()
            .find(|line| line.trim_start().starts_with("printf "))
            .unwrap()
            .trim();
        assert!(
            Command::new("sh")
                .args(["-n", "-c", command])
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn non_permission_failure_does_not_suggest_changing_permissions() {
        let error = marker_access_error(Path::new("/dev/full"), &std::io::Error::other("full"));
        assert!(error.contains("check the tracefs mount"));
        assert!(!error.contains("systemd-tmpfiles"));
    }

    #[test]
    fn ebadf_tracefs_marker_uses_process_bounds_fallback() {
        let path = Path::new("/sys/kernel/tracing/trace_marker");
        assert!(marker_probe_allows_fallback(
            path,
            &std::io::Error::from_raw_os_error(9)
        ));
        assert!(!marker_probe_allows_fallback(
            path,
            &std::io::Error::from(std::io::ErrorKind::PermissionDenied)
        ));
    }

    #[test]
    fn ebadf_during_recording_reports_incomplete_trace_not_permissions() {
        struct BadFile;
        impl Write for BadFile {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(9))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let error = write_active_marker(
            &mut BadFile,
            Path::new("/no-such-tracefs/trace_marker"),
            "event",
        )
        .unwrap_err();
        assert!(error.contains("trace may be incomplete"), "{error}");
        assert!(error.contains("tracing_on is unavailable"), "{error}");
        assert!(!error.contains("systemd-tmpfiles"));
    }

    #[test]
    fn permission_marker_failure_keeps_remediation() {
        let path = Path::new("/sys/kernel/tracing/trace_marker");
        let error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(marker_access_error(path, &error).contains("systemd-tmpfiles"));
    }

    #[test]
    fn marker_probe_occurs_only_when_recording_is_ready() {
        let root = std::env::temp_dir().join(format!("asb-marker-ready-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let marker = root.join("trace_marker");
        let tracing_on = root.join("tracing_on");
        fs::write(&marker, "").unwrap();
        fs::write(&tracing_on, "0\n").unwrap();
        let mut output = open_trace_marker(&marker).unwrap();
        assert!(
            fs::read(&marker).unwrap().is_empty(),
            "permission check must not write"
        );
        let error = validate_active_marker(&mut output, &marker, "42-1").unwrap_err();
        assert!(error.contains("tracing is off"), "{error}");
        assert!(fs::read(&marker).unwrap().is_empty());
        fs::write(&tracing_on, "1\n").unwrap();
        validate_active_marker(&mut output, &marker, "42-1").unwrap();
        assert_eq!(
            fs::read_to_string(&marker).unwrap(),
            format!("I|{}|asb.trace.ready.42-1", std::process::id())
        );
        fs::remove_file(&tracing_on).unwrap();
        assert_eq!(tracing_status(&marker), None);
        validate_active_marker(&mut Vec::new(), &marker, "42-2").unwrap();
        fs::remove_file(marker).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn tracebox_readiness_precedes_marker_write_and_failed_start_is_reaped() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("asb-marker-lifecycle-{}", std::process::id()));
        let bin = root.join("tools/perfetto/bin");
        fs::create_dir_all(&bin).unwrap();
        let marker = root.join("trace_marker");
        let tracing_on = root.join("tracing_on");
        let executable = bin.join("tracebox");
        fs::write(&marker, "").unwrap();
        fs::write(&tracing_on, "0\n").unwrap();
        // The fake collector refuses readiness if ASB wrote too early. It
        // then enables the fixture's tracing flag before reporting ready.
        fs::write(&executable, format!("#!/bin/sh\n[ \"$1\" = --version ] && exit 0\n[ ! -s '{}' ] || exit 1\nprintf '1\\n' > '{}'\nprintf '\\000'\nexec sleep 30\n", marker.display(), tracing_on.display())).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let tools = root.join("tools");
        let mut session =
            measurements::Session::start_with_collectors(&root.join("run"), &[], Some(&tools))
                .unwrap();
        session.start_perfetto_with_marker(&marker).unwrap();
        let id = session.trace_id().to_owned();
        session.write_trace_marker("I|1|asb.test.event").unwrap();
        let contents = fs::read_to_string(&marker).unwrap();
        assert!(
            contents.contains(&format!("asb.trace.ready.{id}")),
            "{contents}"
        );
        assert!(contents.contains("asb.test.event"), "{contents}");
        drop(session);

        fs::write(&marker, "").unwrap();
        fs::write(&tracing_on, "0\n").unwrap();
        fs::write(
            &executable,
            "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\nprintf '\\000'\nexec sleep 30\n",
        )
        .unwrap();
        let mut session =
            measurements::Session::start_with_collectors(&root.join("failed"), &[], Some(&tools))
                .unwrap();
        let error = session.start_perfetto_with_marker(&marker).unwrap_err();
        assert!(error.contains("tracing is off"), "{error}");
        let pid = session.collectors[0].child.id();
        drop(session);
        assert!(
            !Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success(),
            "failed collector was not reaped"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn trace_marker_capture_requires_actual_processor_evidence() {
        use std::os::unix::fs::PermissionsExt;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("asb-marker-capture-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let trace = root.join("trace");
        fs::write(&trace, "fixture").unwrap();
        for (name, script, expected) in [
            ("found", "#!/bin/sh\nprintf '\"COUNT(*)\"\\n1\\n'\n", None),
            (
                "missing",
                "#!/bin/sh\nprintf '\"COUNT(*)\"\\n0\\n'\n",
                Some("did not capture"),
            ),
            (
                "invalid",
                "#!/bin/sh\nprintf 'invalid\\n'\n",
                Some("invalid result"),
            ),
            (
                "failed",
                "#!/bin/sh\nexit 1\n",
                Some("trace processor failed"),
            ),
        ] {
            let processor = root.join(name);
            fs::write(&processor, script).unwrap();
            fs::set_permissions(&processor, fs::Permissions::from_mode(0o755)).unwrap();
            let result = verify_trace_marker_capture(&processor, &trace, "10-1");
            if let Some(expected) = expected {
                let error = result.unwrap_err();
                assert!(error.contains(expected), "{error}");
            } else {
                result.unwrap();
            }
            fs::remove_file(processor).unwrap();
        }
        fs::remove_file(trace).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    #[ignore = "requires an installed Perfetto trace processor; no host tracing is used"]
    fn real_trace_processor_verifies_synthetic_marker_capture() {
        let tools = PathBuf::from(
            std::env::var_os("ASB_TEST_PERFETTO_TOOLS").expect("set ASB_TEST_PERFETTO_TOOLS"),
        );
        let processor = tools.join("perfetto/bin/trace_processor_shell");
        let trace =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/perfetto-marker.trace");
        verify_trace_marker_capture(&processor, &trace, "42-1").unwrap();
        let error = verify_trace_marker_capture(&processor, &trace, "42-2").unwrap_err();
        assert!(error.contains("did not capture"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn trace_processor_preflight_checks_missing_and_executable_tools() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("asb-processor-preflight-{}", std::process::id()));
        let bin = root.join("perfetto/bin");
        fs::create_dir_all(&bin).unwrap();
        let error = probe_trace_processor(Some(&root)).unwrap_err();
        assert!(error.contains("asb tool install perfetto"), "{error}");
        let processor = bin.join("trace_processor_shell");
        fs::write(&processor, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&processor, fs::Permissions::from_mode(0o644)).unwrap();
        if Command::new(&processor).arg("--version").spawn().is_err() {
            let error = probe_trace_processor(Some(&root)).unwrap_err();
            assert!(
                error.contains("cannot execute Perfetto trace processor"),
                "{error}"
            );
            assert!(error.contains("asb tool install perfetto"), "{error}");
        }
        fs::set_permissions(&processor, fs::Permissions::from_mode(0o755)).unwrap();
        probe_trace_processor(Some(&root)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn preflight_stops_before_perfetto_when_marker_is_unwritable() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("asb-marker-preflight-{}", std::process::id()));
        fs::create_dir_all(root.join("tools/perfetto/bin")).unwrap();
        let marker = root.join("trace_marker");
        fs::write(&marker, "").unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o400)).unwrap();
        // A privileged test process can legitimately bypass these permissions.
        if OpenOptions::new().write(true).open(&marker).is_err() {
            let executable = root.join("tools/perfetto/bin/perfetto");
            let sentinel = root.join("perfetto-ran");
            fs::write(
                &executable,
                format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
            )
            .unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            let tools = root.join("tools");
            let mut sudo_collectors = std::collections::BTreeSet::new();
            let mut context = PreflightContext {
                measurement_dir: &root,
                selected: &[],
                tool_dir: Some(&tools),
                sudo_collectors: &mut sudo_collectors,
            };
            let error = preflight_with_marker(&mut context, &marker).unwrap_err();
            assert!(
                error.contains("cannot write ASB lifecycle markers"),
                "{error}"
            );
            assert!(
                !sentinel.exists(),
                "collector must not start before the marker check"
            );
        }
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn marker_probe_writes_a_fixed_non_secret_event() {
        let path = std::env::temp_dir().join(format!("asb-marker-probe-{}", std::process::id()));
        fs::write(&path, "").unwrap();
        let mut output = open_trace_marker(&path).unwrap();
        validate_active_marker(&mut output, &path, "123-1").unwrap();
        let data = fs::read_to_string(&path).unwrap();
        assert_eq!(
            data,
            format!("I|{}|asb.trace.ready.123-1", std::process::id())
        );
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn tmpfiles_rule_applies_to_an_isolated_marker_fixture() {
        use std::os::unix::fs::PermissionsExt;
        if Command::new("systemd-tmpfiles")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let root = std::env::temp_dir().join(format!("asb-marker-tmpfiles-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let marker = root.join("marker");
        let rule = root.join("asb.conf");
        fs::write(&marker, "").unwrap();
        fs::set_permissions(&marker, fs::Permissions::from_mode(0o644)).unwrap();
        let uid = Command::new("id").arg("-u").output().unwrap();
        let uid = String::from_utf8(uid.stdout).unwrap();
        fs::write(
            &rule,
            format!("z {} 0600 {} - -\n", marker.display(), uid.trim()),
        )
        .unwrap();
        let output = Command::new("systemd-tmpfiles")
            .arg("--create")
            .arg(&rule)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::metadata(&marker).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }
}
