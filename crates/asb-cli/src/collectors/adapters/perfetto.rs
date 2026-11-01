use super::super::{
    ArtifactDisplay, CollectorAdapter, LifecycleEvent, PreflightContext, ProcessContext,
    ProcessData, ReportArtifactLink, ReportGraph, ReportPrimaryLink, ReportRow,
};
use crate::measurements;
mod bootstrap;
mod project;
mod service;
mod state_summary;
use service::Service;
use std::sync::Arc;
pub(super) struct PerfettoCollector;
pub(super) static ADAPTER: PerfettoCollector = PerfettoCollector;

impl CollectorAdapter for PerfettoCollector {
    fn private_service_aliases(&self) -> &'static [&'static str] {
        &["__asb-perfetto-probes"]
    }
    fn private_service_entry(&self, args: &[std::ffi::OsString]) -> Option<std::process::ExitCode> {
        Some(bootstrap::entry(args))
    }
    fn configure_project(
        &self,
        root: &Path,
        output: &mut dyn std::io::Write,
        yes: bool,
        config: &mut crate::project::ProjectConfig,
    ) -> Result<(), crate::Error> {
        let instances = project::configure(root, output, yes)?;
        if !instances.is_empty() {
            config.perfetto_instances = instances;
        }
        Ok(())
    }
    fn lock_project_reset(&self, root: &Path) -> Result<Vec<std::fs::File>, crate::Error> {
        project::lock_for_reset(root)
    }

    fn estimated_disk_mib(&self) -> u64 {
        1024
    }
    fn description(&self) -> &'static str {
        "timestamped execution trace for Perfetto"
    }
    fn id(&self) -> &'static str {
        "perfetto"
    }
    fn runtime_inputs(
        &self,
        context: &crate::runtime_inputs::Context<'_>,
    ) -> Result<Vec<crate::runtime_inputs::Dependency>, crate::Error> {
        let mut inputs = crate::tools::find("perfetto")
            .expect("registered Perfetto tool")
            .runtime_inputs(context)?;
        inputs.push(crate::runtime_inputs::Dependency::command(&Command::new(
            "unshare",
        )));
        Ok(inputs)
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
    fn stop_at_execution_end(&self) -> bool {
        // Capture the entire typed lifecycle; scheduler queries still select
        // only the agent's uniquely identified execution interval and tree.
        false
    }
    fn prioritizes_artifact(&self, name: &str) -> bool {
        name.starts_with("perfetto-metric-") || name == "perfetto-state-timeline.svg"
    }
    fn display_artifact(&self, name: &str) -> Option<ArtifactDisplay> {
        ((name.starts_with("perfetto-metric-") || name == "perfetto-state-timeline.svg")
            && name.ends_with(".svg"))
        .then_some(ArtifactDisplay::Svg {
            class: "perfetto-metric",
            label: "Perfetto metric SVG",
        })
    }
    fn report_graphs(&self, rows: &[ReportRow<'_>]) -> Vec<ReportGraph> {
        execution_composition_graph(rows).into_iter().collect()
    }
    fn report_artifact_links(&self, name: &str, source: &Path) -> Vec<ReportArtifactLink> {
        perfetto_ui_url(name, source)
            .map(|href| ReportArtifactLink {
                label: "Perfetto UI",
                href,
                primary: Some(ReportPrimaryLink {
                    heading: "Perfetto trace",
                    text: "Open Perfetto UI and upload this trace",
                    class: "perfetto-link",
                    download_label: "Perfetto trace download",
                    file_label: "Perfetto trace file",
                }),
            })
            .into_iter()
            .collect()
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
    fn producer_policy(&self, _: &str) -> measurements::ProducerPolicy {
        measurements::ProducerPolicy {
            stream_bytes: 256 * 1024 * 1024,
            graceful: Some(Duration::from_secs(5)),
        }
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
    let mut result = ProcessData::from_output(context);
    if let Err(error) = session.verify_perfetto_marker_capture() {
        // Preserve diagnostics without cancelling an independently graded task.
        // The coverage check below suppresses metrics from incomplete capture.
        session.state::<PerfettoState>().borrow_mut().marker_warning = Some(error);
    } else {
        session
            .state::<PerfettoState>()
            .borrow_mut()
            .marker_verified = true;
    }
    let analysis_warning = session
        .state::<PerfettoState>()
        .borrow()
        .trace
        .as_deref()
        .and_then(analysis_budget_warning);
    {
        let state = session.state::<PerfettoState>();
        let mut state = state.borrow_mut();
        state.analysis_warning = analysis_warning;
        if state.analysis_warning.is_some() {
            // The trace was closed and retained, but neither its coverage nor
            // its derived scope can be safely established without loading it
            // in the upstream processor. Do not publish either as known.
            state.capture_warning = Some(
                "Perfetto capture completeness was not verified because this trace exceeds the safe post-processing input limit."
                    .into(),
            );
            state.capture_complete = false;
        } else {
            state.capture_warning = match (session.tool_dir.as_ref(), state.trace.as_ref()) {
                (Some(tools), Some(trace)) => verify_trace_coverage(
                    &tools.join("perfetto/bin/trace_processor_shell"),
                    trace,
                    session.trace_id(),
                )
                .err(),
                _ => Some(
                    "Perfetto capture coverage cannot be verified: trace or processor is missing."
                        .into(),
                ),
            };
            state.capture_complete = state.capture_warning.is_none();
        }
    }
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
    if context.stderr.contains("FtraceController") {
        result.status = "unavailable".into();
    } else if !state.capture_complete {
        result.status = "failed".into();
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
            "marker_verified": state.marker_verified,
            "marker_warning": state.marker_warning,
            "scope_warning": state.scope_warning,
            "capture_complete": state.capture_complete,
            "capture_warning": state.capture_warning,
            "analysis_warning": state.analysis_warning,
        }),
    )?;
    write_perfetto_metric_svgs(
        &session.dir,
        state.trace.as_deref(),
        state.tracepoint_count,
        &state.metrics,
    )?;
    if let (Some(trace), Some(tools), Some(pid)) = (
        state.trace.as_deref(),
        session.tool_dir.as_deref(),
        state.pid,
    ) {
        if let Some(warning) = state.analysis_warning.as_deref() {
            write_timeline_notice(&session.dir, &format!("Timeline unavailable: {warning}"))?;
        } else if !state.capture_complete {
            write_timeline_notice(
                &session.dir,
                "Timeline unavailable: incomplete or lossy Perfetto capture; inspect perfetto-tracepoints.json.",
            )?;
        } else {
            write_state_timeline_svg(
                &session.dir,
                &tools.join("perfetto/bin/trace_processor_shell"),
                trace,
                pid,
                session.trace_id(),
            )?;
        }
    }
    Ok(())
}

fn preflight(context: &mut PreflightContext<'_>) -> Result<(), String> {
    if !super::super::selected_or_environment("perfetto", context.selected) {
        return Ok(());
    }
    let executable = measurements::resolve_executable(context.tool_dir, "perfetto");
    if !executable.is_file() {
        return Err("Perfetto is required for the selected collector. Run `asb tool install perfetto` and retry.".into());
    }
    preflight_perfetto(context, &executable)
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
    let service = Service::acquire(
        executable,
        context
            .project_root
            .ok_or("Perfetto requires the benchmark project's explicit authority")?,
    )?;
    if perfetto_probe(&service, context.measurement_dir)? {
        return Ok(());
    }
    Err("Perfetto could not record its delegated tracefs instance. Inspect .asb/perfetto service logs and rerun `asb init --configure-host` to repair delegation; no host-wide tracing reset or sudo fallback was attempted.".into())
}

fn probe_trace_processor(tool_dir: Option<&Path>) -> Result<(), String> {
    let Some(processor) = tool_dir.map(|dir| dir.join("perfetto/bin/trace_processor_shell")) else {
        return Err("Perfetto trace processing needs the project-local trace processor. Run `asb tool install perfetto` and retry.".into());
    };
    let mut command = Command::new(&processor);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = retry_executable_busy(|| command.spawn())
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

use crate::measurements::{
    Collector, bounded, env_true, escape_svg, parse_float, safe_svg_name, write_unavailable,
};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Perfetto's scheduler-state composition is specific to its trace schema.
/// Comparison merely publishes this adapter-owned SVG alongside other graphs.
fn execution_composition_graph(source_rows: &[ReportRow<'_>]) -> Option<ReportGraph> {
    let mut rows = Vec::new();
    for row in source_rows {
        let state = |name| {
            row.metrics
                .get(&format!("perfetto-state-summary.{name}"))
                .copied()
                .unwrap_or(0.0)
        };
        let values = [
            state("running_ms"),
            state("runnable_ms"),
            state("interruptible_sleep_ms"),
            state("uninterruptible_sleep_ms"),
            row.metrics
                .get("perfetto-state-summary.io_wait_observed_ms")
                .copied()
                .unwrap_or_else(|| state("io_wait_ms")),
            state("other_scheduler_state_ms"),
        ];
        let has_states = row
            .metrics
            .keys()
            .any(|key| key.starts_with("perfetto-state-summary.") && key.ends_with("_ms"));
        let wall = row
            .components
            .and_then(|value| value.pointer("/components/agent.execute"))
            .and_then(|value| value.get("wall_ms"))
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        if values.iter().sum::<f64>() > 0.0 || wall > 0.0 {
            rows.push((row.label, values, wall, has_states));
        }
    }
    if rows.is_empty() {
        return None;
    }
    let colors = [
        "#1976d2", "#c62828", "#78909c", "#6a1b9a", "#ef6c00", "#b0bec5",
    ];
    let labels = [
        "running",
        "runnable",
        "interruptible sleep",
        "uninterruptible (I/O may be unknown)",
        "I/O wait (observed)",
        "other scheduler state",
    ];
    let height = 140 + rows.len() * 60;
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"980\" height=\"{height}\" viewBox=\"0 0 980 {height}\"><title>Agent execution time composition</title><rect width=\"100%\" height=\"100%\" fill=\"#fff\"/><text x=\"24\" y=\"30\" font-family=\"sans-serif\" font-size=\"18\" font-weight=\"bold\">Observed scheduler thread-time composition</text><text x=\"24\" y=\"51\" font-family=\"sans-serif\" font-size=\"12\" fill=\"#52606d\">Concurrent thread-time totals are not additive wall-time categories. Elapsed wall is a separate reference, never a residual.</text>"
    );
    for (index, label) in labels.iter().enumerate() {
        let x = 24 + (index % 3) * 310;
        let y = 68 + (index / 3) * 20;
        svg.push_str(&format!("<rect x=\"{x}\" y=\"{y}\" width=\"10\" height=\"10\" fill=\"{}\"/><text x=\"{}\" y=\"{}\" font-family=\"sans-serif\" font-size=\"11\">{}</text>", colors[index], x + 14, y + 10, escape_svg(label)));
    }
    for (index, (name, values, wall, has_states)) in rows.iter().enumerate() {
        let y = 118 + index * 60;
        let total: f64 = values.iter().sum();
        svg.push_str(&format!("<g data-benchmark=\"{}\"><text x=\"24\" y=\"{}\" font-family=\"sans-serif\" font-size=\"12\">{}</text><rect x=\"250\" y=\"{y}\" width=\"690\" height=\"22\" rx=\"3\" fill=\"#edf1f4\"/>", escape_svg(name), y + 15, escape_svg(name)));
        let text = if *has_states {
            format!("Observed thread-time: {total:.2} ms")
        } else {
            "Scheduler thread-time unavailable".into()
        };
        svg.push_str(&format!("<text x=\"250\" y=\"{}\" font-family=\"sans-serif\" font-size=\"11\">{text}; elapsed wall reference: {wall:.2} ms</text>", y + 38));
        if total > 0.0 {
            let mut x = 250.0;
            for (i, value) in values.iter().enumerate() {
                if *value > 0.0 {
                    let width = 690.0 * value / total;
                    svg.push_str(&format!("<rect class=\"bar\" data-label=\"{}\" data-value=\"{value:.2} ms\" x=\"{x:.2}\" y=\"{y}\" width=\"{width:.2}\" height=\"22\" fill=\"{}\"><title>{}: {:.2} ms</title></rect>", escape_svg(labels[i]), colors[i], labels[i], value));
                    x += width;
                }
            }
        }
        svg.push_str("</g>");
    }
    svg.push_str("</svg>\n");
    Some(ReportGraph {
        filename: "perfetto-execution-composition.svg".into(),
        svg,
    })
}

/// Return the bare official Perfetto UI URL only for a nonempty trace.
///
/// Perfetto deliberately does not receive a trace path here: users upload the
/// separately published trace themselves, whether it is a project-local file
/// or an HTTP download.
fn perfetto_ui_url(name: &str, trace: &Path) -> Option<String> {
    if !name.ends_with(".perfetto-trace") || !trace.is_file() || trace.metadata().ok()?.len() == 0 {
        return None;
    }
    Some("https://ui.perfetto.dev/".into())
}

#[cfg(test)]
mod report_tests {
    use super::*;

    #[test]
    fn report_links_use_the_bare_perfetto_ui_for_a_nonempty_trace() {
        let root = std::env::temp_dir().join(format!(
            "asb perfetto trace {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("trace directory");
        let trace = root.join("capture #1.perfetto-trace");
        fs::write(&trace, b"trace").expect("trace");
        let url = perfetto_ui_url("capture #1.perfetto-trace", &trace).expect("usable trace URL");
        assert_eq!(url, "https://ui.perfetto.dev/");
        assert!(perfetto_ui_url("metrics.json", &trace).is_none());
        fs::write(&trace, b"").expect("empty trace");
        assert!(perfetto_ui_url("capture.perfetto-trace", &trace).is_none());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn report_composition_keeps_missing_scheduler_state_unavailable() {
        let metrics = BTreeMap::new();
        let components = serde_json::json!({"components":{"agent.execute":{"wall_ms":12}}});
        let graph = execution_composition_graph(&[ReportRow {
            label: "no-perfetto",
            metrics: &metrics,
            components: Some(&components),
            documents: &BTreeMap::new(),
        }])
        .expect("wall reference remains useful");
        assert_eq!(graph.filename, "perfetto-execution-composition.svg");
        assert!(
            graph
                .svg
                .contains("Scheduler thread-time unavailable; elapsed wall reference: 12.00 ms")
        );
        assert!(!graph.svg.contains("Observed thread-time: 0.00"));
    }
}

#[derive(Default)]
struct PerfettoState {
    service: Option<Arc<Service>>,
    enabled: bool,
    trace: Option<PathBuf>,
    pid: Option<u32>,
    trace_marker: Option<PathBuf>,
    trace_marker_output: Option<File>,
    marker_verified: bool,
    marker_warning: Option<String>,
    scope_warning: Option<String>,
    capture_warning: Option<String>,
    analysis_warning: Option<String>,
    capture_complete: bool,
    tracepoint_count: u64,
    metrics: BTreeMap<String, f64>,
}

// Trace Processor builds an in-memory database for the entire capture before
// it can apply ASB's process-tree scope.  A busy shared host can therefore
// turn a modest-looking trace into multi-gigabyte post-processing pressure.
// Keep the native trace (the authoritative collector artifact), but bound the
// optional derived metrics, series, and SVG timeline before invoking it.
const MAX_TRACE_PROCESSOR_INPUT_BYTES: u64 = 128 * 1024 * 1024;

fn analysis_budget_warning(trace: &Path) -> Option<String> {
    let size = trace.metadata().ok()?.len();
    (size > MAX_TRACE_PROCESSOR_INPUT_BYTES).then(|| {
        format!(
            "Derived Perfetto analysis was skipped because the retained trace is {} MiB, above the {} MiB post-processing limit. The complete native trace is retained for Perfetto UI inspection.",
            size / (1024 * 1024),
            MAX_TRACE_PROCESSOR_INPUT_BYTES / (1024 * 1024),
        )
    })
}

fn wait_for_capture_ready(collector: &mut Collector, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(completion) = collector.producer.poll()? {
            return Err(format!(
                "capture exited before readiness: {:?}; {}",
                completion.exit,
                completion.error.unwrap_or_default()
            ));
        }
        // Positional inspection preserves every notification/output byte for
        // the final capture, rather than taking a blocking reader thread.
        let notification = collector.captures[0].startup_diagnostic(1)?;
        match notification.as_bytes().first() {
            Some(0) => return Ok(()),
            Some(byte) => return Err(format!("unexpected readiness notification {byte}")),
            None => (),
        }
        if Instant::now() >= deadline {
            return Err("readiness notification deadline exceeded".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl measurements::Session {
    pub(crate) fn set_perfetto_pid(&mut self, pid: u32) {
        self.state::<PerfettoState>().borrow_mut().pid = Some(pid);
    }

    pub(crate) fn start_perfetto(&mut self) -> Result<(), String> {
        if !self.state::<PerfettoState>().borrow().enabled {
            return Ok(());
        }
        let executable = measurements::resolve_executable(self.tool_dir.as_deref(), "perfetto");
        let service = Service::acquire(
            &executable,
            self.project_root
                .as_deref()
                .ok_or("Perfetto requires the benchmark project's explicit authority")?,
        )?;
        let marker = service.marker.clone();
        self.start_perfetto_with_marker(&marker, Some(service))
    }

    fn start_perfetto_with_marker(
        &mut self,
        marker: &Path,
        service: Option<Arc<Service>>,
    ) -> Result<(), String> {
        let config = self.dir.join("perfetto-scheduling.pbtx");
        let configuration = service.as_ref().map_or_else(
            || perfetto_config(None),
            |service| perfetto_instance_config(&service.instance),
        );
        fs::write(&config, configuration)
            .map_err(|error| format!("cannot write Perfetto trace configuration: {error}"))?;
        let trace = self.dir.join("perfetto-scheduling.perfetto-trace");
        // A sudo-launched Tracebox otherwise creates a root-only 0600 trace.
        // Precreating the artifact keeps its ownership with the benchmark user.
        File::create(&trace)
            .map_err(|error| format!("cannot create Perfetto trace artifact: {error}"))?;
        let stdout_path = self.dir.join("perfetto.stdout");
        let stderr_path = self.dir.join("perfetto.stderr");
        let captures = [
            crate::evidence::Capture::create(&stdout_path)?,
            crate::evidence::Capture::create(&stderr_path)?,
        ];
        let mut marker_output = open_trace_marker(marker)?;
        let (mut command, route) = if let Some(service) = &service {
            service.collector_command("perfetto")?
        } else {
            let (mut command, route) = self.collector_command("perfetto")?;
            command.arg("perfetto");
            (command, route)
        };
        command
            .args(["--txt", "--notify-fd", "1", "-c"])
            .arg(&config)
            .args(["-o"])
            .arg(&trace);
        let dependency = service
            .as_ref()
            .map(|service| Arc::clone(service) as Arc<dyn Send + Sync>);
        match Collector::spawn_with_dependency(
            "perfetto", &ADAPTER, command, route, captures, dependency,
        ) {
            Ok(mut collector) => {
                if let Err(error) = wait_for_capture_ready(&mut collector, Duration::from_secs(35))
                {
                    return Err(collector.startup_failure(format!(
                        "Perfetto did not report that tracing is ready: {error}"
                    )));
                }
                self.collectors.push(collector);
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
                state.service = service;
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
        if state.analysis_warning.is_some() {
            return metrics;
        }
        if !state.capture_complete {
            return metrics;
        }
        let Some(tool_dir) = self.tool_dir.as_ref() else {
            return metrics;
        };
        let processor = tool_dir.join("perfetto/bin/trace_processor_shell");
        if !processor.is_file() {
            return metrics;
        }
        let valid = state.pid.and_then(|pid| {
            let query = format!("{} SELECT COUNT(*) FROM root CROSS JOIN bounds WHERE bounds.start_ts IS NOT NULL AND bounds.end_ts>bounds.start_ts", perfetto_scope_sql(pid, &self.trace_id));
            processor_query(&processor, trace, &query).ok().filter(|output| output.status.success())
                .and_then(|output| bounded(&output.stdout).lines().rev().find_map(parse_float))
        }) == Some(1.0);
        if !valid {
            drop(state);
            self.state::<PerfettoState>().borrow_mut().scope_warning = Some("Cannot uniquely identify the benchmark process lifetime and interval in this trace; scheduler metrics are unavailable rather than unscoped or zero. Inspect the retained trace and lifecycle markers.".into());
            return metrics;
        }
        for (metric, query) in perfetto_metric_queries(state.pid, &self.trace_id) {
            let Ok(output) = processor_query(&processor, trace, &query) else {
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
        if state.analysis_warning.is_some() {
            return (series, timestamps);
        }
        if !state.capture_complete {
            return (series, timestamps);
        }
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
            let Ok(output) = processor_query(&processor, trace, &query) else {
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

#[derive(Debug)]
struct StateSpan {
    utid: u32,
    tid: u32,
    ts: u64,
    dur: u64,
    state: String,
    io_wait: Option<bool>,
}

/// Render exact scheduler intervals.  Each lane is a traced thread, so
/// simultaneous states are shown as a stack rather than collapsed into a
/// misleading single state for the entire process tree.
fn write_state_timeline_svg(
    dir: &Path,
    processor: &Path,
    trace: &Path,
    pid: u32,
    trace_id: &str,
) -> Result<(), String> {
    if !processor.is_file() || !trace.is_file() {
        return write_timeline_notice(
            dir,
            "Timeline unavailable: native trace or trace processor is missing.",
        );
    }
    let summary = match state_summary::write(dir, processor, trace, pid, trace_id) {
        Ok(summary) => summary,
        Err(error) => {
            return write_timeline_notice(
                dir,
                &format!(
                    "Timeline unavailable: {error}. No partial scheduler-state totals published."
                ),
            );
        }
    };
    let query = format!(
        "{} SELECT utid,tid,ts,dur,state,COALESCE(io_wait,-1) FROM clipped WHERE utid IN (SELECT DISTINCT utid FROM clipped ORDER BY utid LIMIT 64) ORDER BY utid,ts LIMIT 4096",
        state_summary::timeline_scope(pid, trace_id)
    );
    let output = processor_query(processor, trace, &query)
        .map_err(|e| format!("cannot extract Perfetto state timeline: {e}"))?;
    if !output.status.success() {
        return write_timeline_notice(
            dir,
            "Timeline unavailable: this trace processor cannot extract thread-state intervals.",
        );
    }
    let mut spans = Vec::new();
    for line in bounded(&output.stdout).lines() {
        let fields: Vec<_> = line
            .split(',')
            .map(|s| s.trim().trim_matches('"'))
            .collect();
        if fields.len() != 6 {
            continue;
        }
        let (Ok(utid), Ok(tid), Ok(ts), Ok(dur)) = (
            fields[0].parse(),
            fields[1].parse(),
            fields[2].parse(),
            fields[3].parse(),
        ) else {
            continue;
        };
        spans.push(StateSpan {
            utid,
            tid,
            ts,
            dur,
            state: fields[4].into(),
            io_wait: match fields[5] {
                "1" => Some(true),
                "0" => Some(false),
                _ => None,
            },
        });
    }
    if spans.is_empty() {
        return write_timeline_notice(
            dir,
            "Timeline unavailable: the trace contains no process-tree scheduler intervals.",
        );
    }
    let (start, end) = (summary.start, summary.end);
    let mut tids = spans.iter().map(|s| (s.utid, s.tid)).collect::<Vec<_>>();
    tids.sort_unstable();
    tids.dedup();
    tids.truncate(64);
    let height = 150 + tids.len() * 24;
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"980\" height=\"{height}\" viewBox=\"0 0 980 {height}\"><title>Bounded process-tree scheduler-state timeline</title><rect width=\"100%\" height=\"100%\" fill=\"#fff\"/><text x=\"18\" y=\"25\" font-family=\"sans-serif\" font-size=\"17\" font-weight=\"bold\">Observed scheduler-state timeline</text><text x=\"18\" y=\"45\" font-family=\"sans-serif\" font-size=\"11\" fill=\"#52606d\">Execution-relative milliseconds; one lane per unique thread lifetime. I/O classification can be unknown.</text>"
    );
    svg.push_str(&format!("<text x=\"18\" y=\"62\" font-family=\"sans-serif\" font-size=\"11\">Showing {} of {} intervals, {} of {} observed threads; {} unfinished states excluded. Full known-duration totals are independent of this view.</text>",spans.len(),summary.intervals,tids.len(),summary.threads,summary.unfinished));
    for (index, (label, color)) in [
        ("running", "#1976d2"),
        ("runnable", "#c62828"),
        ("I/O wait", "#ef6c00"),
        ("uninterruptible", "#6a1b9a"),
        ("sleep", "#78909c"),
    ]
    .iter()
    .enumerate()
    {
        let x = 100 + index * 160;
        svg.push_str(&format!("<rect x=\"{x}\" y=\"76\" width=\"9\" height=\"9\" fill=\"{color}\"/><text x=\"{}\" y=\"85\" font-family=\"sans-serif\" font-size=\"10\">{label}</text>", x + 12));
    }
    for (row, (utid, tid)) in tids.iter().enumerate() {
        let y = 102 + row * 24;
        svg.push_str(&format!("<text x=\"18\" y=\"{}\" font-family=\"monospace\" font-size=\"11\"><title>unique thread {utid}</title>tid {tid}</text><rect x=\"100\" y=\"{}\" width=\"850\" height=\"16\" fill=\"#edf1f4\"/>", y + 12, y));
        for s in spans.iter().filter(|s| s.utid == *utid) {
            let x = 100.0 + 850.0 * (s.ts.saturating_sub(start) as f64 / (end - start) as f64);
            let w = 850.0 * s.dur as f64 / (end - start) as f64;
            let (label, color) = state_style(&s.state, s.io_wait);
            svg.push_str(&format!("<rect x=\"{x:.2}\" y=\"{y}\" width=\"{w:.2}\" height=\"16\" fill=\"{color}\"><title>{label}: {} ns</title></rect>", s.dur));
        }
    }
    let axis_y = height - 16;
    for tick in 0..=5 {
        let x = 100 + 170 * tick;
        let ms = (end - start) as f64 * tick as f64 / 5_000_000.0;
        svg.push_str(&format!("<text x=\"{x}\" y=\"{axis_y}\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"11\">{ms:.1} ms</text>"));
    }
    svg.push_str("</svg>\n");
    fs::write(dir.join("perfetto-state-timeline.svg"), svg)
        .map_err(|e| format!("cannot write Perfetto state timeline SVG: {e}"))
}

fn write_timeline_notice(dir: &Path, message: &str) -> Result<(), String> {
    fs::write(dir.join("perfetto-state-timeline.svg"), format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"980\" height=\"100\" viewBox=\"0 0 980 100\"><title>Perfetto state timeline unavailable</title><rect width=\"100%\" height=\"100%\" fill=\"#fff4e5\"/><text x=\"20\" y=\"40\" font-family=\"sans-serif\" font-size=\"18\">Perfetto state timeline unavailable</text><text x=\"20\" y=\"70\" font-family=\"sans-serif\" font-size=\"13\">{}</text></svg>\n", escape_svg(message))).map_err(|e| format!("cannot write Perfetto state timeline notice: {e}"))
}

fn state_style(state: &str, io_wait: Option<bool>) -> (&'static str, &'static str) {
    if matches!(state, "D" | "Uninterruptible Sleep") && io_wait == Some(true) {
        return ("I/O wait", "#ef6c00");
    }
    match state {
        "Running" => ("running", "#1976d2"),
        "Runnable" | "R" | "R+" => ("runnable", "#c62828"),
        "D" | "Uninterruptible Sleep" => ("uninterruptible sleep", "#6a1b9a"),
        "S" | "I" | "Sleeping" => ("interruptible sleep", "#78909c"),
        _ => ("other scheduler state", "#b0bec5"),
    }
}

fn open_trace_marker(path: &Path) -> Result<File, String> {
    // Opening checks permissions without writing to an inactive trace buffer.
    OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| marker_access_error(path, &error))
}

#[cfg(test)]
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

/// Concurrent fork/exec can briefly retain another thread's writable file
/// descriptor to a freshly installed executable. Retry only ETXTBSY, before
/// execution has occurred; never retry a failed query or another launch error.
fn retry_executable_busy<T>(mut launch: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        match launch() {
            Err(error) if error.raw_os_error() == Some(26) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            result => return result,
        }
    }
}

fn processor_query(
    processor: &Path,
    trace: &Path,
    query: &str,
) -> std::io::Result<std::process::Output> {
    retry_executable_busy(|| {
        Command::new(processor)
            .arg("query")
            .arg(trace)
            .arg(query)
            .output()
    })
}

fn verify_trace_marker_capture(
    processor: &Path,
    trace: &Path,
    trace_id: &str,
) -> Result<(), String> {
    // trace_id is generated from a process ID and atomic counter, not user input.
    let query = format!("SELECT COUNT(*) FROM slice WHERE name='asb.trace.ready.{trace_id}'");
    let output = processor_query(processor, trace, &query).map_err(|error| {
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

/// A bounded file/duration limit must not silently turn a partial trace into
/// whole-execution scheduler metrics. Check both ends and reported data loss.
fn verify_trace_coverage(processor: &Path, trace: &Path, trace_id: &str) -> Result<(), String> {
    let query = format!(
        "WITH marks AS (SELECT COUNT(*) FILTER (WHERE name='asb.trace.ready.{trace_id}') AS ready, MIN(ts) FILTER (WHERE name='asb.trace.ready.{trace_id}') AS ready_ts, COUNT(*) FILTER (WHERE name='asb.run.finishing.{trace_id}') AS finishing, MIN(ts) FILTER (WHERE name='asb.run.finishing.{trace_id}') AS finishing_ts, COUNT(*) FILTER (WHERE name='asb.run.failed.{trace_id}') AS failed, MIN(ts) FILTER (WHERE name='asb.run.failed.{trace_id}') AS failed_ts FROM slice) SELECT ready, CASE WHEN (finishing=1 AND failed<=1 AND finishing_ts>ready_ts AND (failed=0 OR (failed_ts>ready_ts AND failed_ts<=finishing_ts))) OR (finishing=0 AND failed=1 AND failed_ts>ready_ts) THEN 1 ELSE 0 END, EXISTS(SELECT 1 FROM stats WHERE value>0 AND (name IN ('traced_buf_bytes_overwritten','traced_buf_chunks_discarded','traced_buf_patches_failed','traced_buf_trace_writer_packet_loss','traced_buf_sequence_packet_loss','ftrace_cpu_overrun_delta','ftrace_cpu_commit_overrun_delta','ftrace_cpu_dropped_events_delta') OR name GLOB 'traced_buf_data_loss_*')) FROM marks"
    );
    let output = processor_query(processor, trace, &query).map_err(|_| {
        "Cannot verify Perfetto capture coverage: trace processor could not start.".to_owned()
    })?;
    if !output.status.success() {
        return Err(
            "Cannot verify Perfetto capture coverage: trace processor query failed.".into(),
        );
    }
    let values = bounded(&output.stdout)
        .lines()
        .find_map(|line| {
            let fields = line
                .split(',')
                .map(|field| field.trim().parse::<u64>())
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            (fields.len() == 3).then_some(fields)
        })
        .ok_or_else(|| {
            "Cannot verify Perfetto capture coverage: invalid processor result.".to_owned()
        })?;
    if values == [1, 1, 0] {
        return Ok(());
    }
    Err(format!(
        "Incomplete Perfetto capture: startup markers={}, terminal boundary valid={}, data loss={}. Scheduler metrics are suppressed. Inspect native trace stats for buffer overruns/overwrites and check whether the 1 GiB file or one-hour duration limit was reached; reduce tracing load before a fresh qualification.",
        values[0],
        values[1] != 0,
        values[2] != 0
    ))
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
                "{scope} SELECT COUNT(*) FROM sched s JOIN thread th ON s.utid=th.utid CROSS JOIN bounds WHERE th.upid IN tree AND s.ts >= bounds.start_ts AND s.ts < bounds.end_ts HAVING {VALID_SCOPE}"
            ),
        ),
        (
            "perfetto.processes".into(),
            format!("{scope} SELECT COUNT(*) FROM process WHERE upid IN tree HAVING {VALID_SCOPE}"),
        ),
        (
            "perfetto.threads".into(),
            format!("{scope} SELECT COUNT(*) FROM thread WHERE upid IN tree HAVING {VALID_SCOPE}"),
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
                "{scope}, relevant AS (SELECT s.ts, s.dur FROM sched s JOIN thread th ON s.utid=th.utid CROSS JOIN bounds WHERE th.upid IN tree AND s.ts < bounds.end_ts AND s.ts+s.dur > bounds.start_ts), bucket_ranges(bucket) AS (SELECT 0 WHERE {VALID_SCOPE} UNION ALL SELECT bucket+1 FROM bucket_ranges CROSS JOIN bounds WHERE bounds.start_ts+(bucket+1)*1000000000 < bounds.end_ts) SELECT bucket_ranges.bucket AS timestamp_s, COALESCE(SUM(MAX(0, MIN(r.ts+r.dur,bounds.end_ts,bounds.start_ts+(bucket_ranges.bucket+1)*1000000000)-MAX(r.ts,bounds.start_ts+bucket_ranges.bucket*1000000000)))/1000000.0,0) AS value FROM bucket_ranges CROSS JOIN bounds LEFT JOIN relevant r ON r.ts < bounds.start_ts+(bucket_ranges.bucket+1)*1000000000 AND r.ts+r.dur > bounds.start_ts+bucket_ranges.bucket*1000000000 GROUP BY bucket_ranges.bucket ORDER BY bucket_ranges.bucket"
            ),
        ),
    ]
}

const VALID_SCOPE: &str = "(SELECT COUNT(*) FROM root CROSS JOIN bounds WHERE bounds.start_ts IS NOT NULL AND bounds.end_ts>bounds.start_ts)=1";

pub(crate) fn perfetto_scope_sql(pid: u32, trace_id: &str) -> String {
    // A numeric PID may have several UPIDs in a long trace. Select only the
    // unique generation alive at the agent-start marker. Without a marker,
    // a unique PID generation is required; ambiguity must not become zero data.
    format!(
        "WITH RECURSIVE tree(upid) AS (SELECT upid FROM root UNION ALL SELECT p.upid FROM process p JOIN tree t ON p.parent_upid=t.upid), marker_bounds AS (SELECT MIN(ts) FILTER (WHERE name='asb.agent.started.{trace_id}') AS marker_start, MAX(ts) FILTER (WHERE name='asb.agent.exited.{trace_id}') AS marker_end FROM slice), root_candidates AS (SELECT p.upid,p.start_ts,p.end_ts,COALESCE(p.start_ts,(SELECT MIN(s.ts) FROM sched s JOIN thread th ON s.utid=th.utid WHERE th.upid=p.upid)) AS first_seen FROM process p WHERE p.pid={pid}), eligible_roots AS (SELECT r.upid FROM root_candidates r CROSS JOIN marker_bounds m WHERE (m.marker_start IS NOT NULL AND r.first_seen <= m.marker_start AND (r.end_ts IS NULL OR r.end_ts >= m.marker_start)) OR (m.marker_start IS NULL AND (SELECT COUNT(*) FROM root_candidates)=1)), root AS (SELECT upid FROM eligible_roots WHERE (SELECT COUNT(*) FROM eligible_roots)=1), process_bounds AS (SELECT MIN(start_ts) AS process_start, MAX(COALESCE(end_ts,(SELECT MAX(ts+dur) FROM sched))) AS process_end FROM process WHERE upid IN tree), bounds AS (SELECT COALESCE(marker_start,process_start) AS start_ts, COALESCE(marker_end,process_end) AS end_ts FROM process_bounds CROSS JOIN marker_bounds)",
    )
}

pub(crate) fn perfetto_config(pid: Option<u32>) -> String {
    // Tracebox's bundled schema rejects FtraceConfig.pid_filter. Process
    // scoping is applied during post-processing using ASB lifecycle markers.
    let _ = pid;
    // Collect only events consumed by scheduler/process-lifetime queries.
    // Redundant sched_stat events multiply traffic on a busy shared host.
    r#"duration_ms: 3600000
write_into_file: true
file_write_period_ms: 1000
max_file_size_bytes: 1073741824
compression { zstd { level: 1 } }
buffers { size_kb: 131072 fill_policy: RING_BUFFER }
data_sources {
  config {
    name: "linux.ftrace"
    ftrace_config {
      buffer_size_kb: 4096
      buffer_size_lower_bound: true
      drain_period_ms: 50
      drain_buffer_percent: 50
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

fn perfetto_instance_config(instance: &str) -> String {
    perfetto_config(None).replacen(
        "ftrace_config {",
        &format!("ftrace_config {{\n      instance_name: \"{instance}\""),
        1,
    )
}

/// Read-only primary tracefs format metadata needed by Tracebox's decoder.
/// No event enable/filter, raw pipe, or trace buffer permissions are included.
pub(crate) fn required_trace_metadata() -> Vec<PathBuf> {
    let root = if Path::new("/sys/kernel/tracing/events").is_dir() {
        Path::new("/sys/kernel/tracing/events")
    } else {
        Path::new("/sys/kernel/debug/tracing/events")
    };
    let mut paths = vec![root.join("header_page"), root.join("header_event")];
    for line in perfetto_config(None).lines() {
        if let Some(event) = line
            .trim()
            .strip_prefix("ftrace_events: \"")
            .and_then(|event| event.strip_suffix('"'))
        {
            paths.extend([root.join(event).join("format"), root.join(event).join("id")]);
        }
    }
    paths.into_iter().filter(|path| path.is_file()).collect()
}

/// Probe the owned instance without autostart, global reset, or sudo fallback.
fn perfetto_probe(service: &Service, measurement_dir: &Path) -> Result<bool, String> {
    let config = measurement_dir.join(".perfetto-preflight.pbtx");
    let trace = measurement_dir.join(".perfetto-preflight.perfetto-trace");
    fs::write(
        &config,
        perfetto_instance_config(&service.instance).replacen(
            "duration_ms: 3600000",
            "duration_ms: 250",
            1,
        ),
    )
    .map_err(|error| format!("cannot write Perfetto preflight configuration: {error}"))?;
    let stderr_path = measurement_dir.join(".perfetto-preflight.stderr");
    let result = (|| -> std::io::Result<bool> {
        let mut child = service
            .command("perfetto")
            .args(["--txt", "-c"])
            .arg(&config)
            .args(["-o"])
            .arg(&trace)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(&stderr_path)?))
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let success = loop {
            match child.try_wait()? {
                Some(status) => break status.success(),
                None if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
                None => std::thread::sleep(Duration::from_millis(25)),
            }
        };
        let mut bytes = Vec::new();
        File::open(&stderr_path)?
            .take(256 * 1024)
            .read_to_end(&mut bytes)?;
        let stderr = String::from_utf8_lossy(&bytes);
        Ok(success
            && ![
                "Failed to reset ftrace",
                "Failed to create FtraceController",
                "Data source id=1 not found",
            ]
            .iter()
            .any(|marker| stderr.contains(marker)))
    })();
    let _ = fs::remove_file(&config);
    let _ = fs::remove_file(&trace);
    let _ = fs::remove_file(&stderr_path);
    result.map_err(|error| format!("cannot execute owned Perfetto preflight client: {error}. Check .asb/perfetto service logs and reinstall with `asb tool install perfetto` if needed."))
}

#[cfg(test)]
#[path = "perfetto/coverage_tests.rs"]
mod coverage_tests;

#[cfg(test)]
mod marker_tests {
    use super::*;

    #[test]
    fn metrics_produce_distinct_escaped_svg_artifacts() {
        let dir = std::env::temp_dir().join(format!("asb-metric-svg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let metrics = BTreeMap::from([
            ("perfetto.sched_switch".to_owned(), 3.0),
            ("perfetto.wait<unsafe>".to_owned(), 12.0),
        ]);
        write_perfetto_metric_svgs(&dir, None, 2, &metrics).unwrap();
        let first = std::fs::read_to_string(dir.join("perfetto-metric-sched_switch.svg")).unwrap();
        let second = std::fs::read_to_string(dir.join("perfetto-metric-wait-unsafe-.svg")).unwrap();
        assert!(first.contains("sched_switch"));
        assert!(second.contains("wait&lt;unsafe&gt;"));
        assert!(!second.contains("wait<unsafe>"));
        assert!(second.contains("native trace unavailable"));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn without_metrics_reports_tracepoints_as_a_fallback() {
        let dir = std::env::temp_dir().join(format!("asb-metric-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_perfetto_metric_svgs(&dir, None, 9, &Default::default()).unwrap();
        let svg = std::fs::read_to_string(dir.join("perfetto-metric-tracepoints.svg")).unwrap();
        assert!(svg.contains("lifecycle markers: 9"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn svg_write_errors_report_the_artifact_type() {
        let absent = std::env::temp_dir().join(format!(
            "asb-no-such-perfetto-metric-dir-{}",
            std::process::id()
        ));
        assert!(
            write_perfetto_metric_svgs(&absent, None, 0, &Default::default())
                .unwrap_err()
                .contains("Perfetto metric SVG")
        );
    }

    #[test]
    fn config_collects_scheduler_process_and_trace_marker_events() {
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
    fn configuration_uses_supported_tracebox_schema() {
        let config = perfetto_config(Some(42));
        assert!(!config.contains("pid_filter"));
        assert!(config.contains("scan_all_processes_on_start: false"));
    }

    #[test]
    fn project_local_install_uses_the_tracebox_binary_name() {
        assert_eq!(
            crate::measurements::tool_binary_name("perfetto"),
            "tracebox"
        );
        assert_eq!(crate::measurements::tool_binary_name("perf"), "perf");
    }

    #[test]
    fn queries_scope_to_the_target_process_tree() {
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
    fn parses_csv_series_with_trace_timestamps() {
        let (timestamps, values) = parse_perfetto_series_output(
            "\"timestamp_s\",\"value\"\n\"12\",\"3\"\n\"13.5\",\"7\"\n",
        );
        assert_eq!(timestamps, vec![12_000.0, 13_500.0]);
        assert_eq!(values, vec![3.0, 7.0]);
    }

    #[test]
    fn capture_configuration_streams_with_bounded_storage_and_kernel_draining() {
        for config in [
            perfetto_config(None),
            perfetto_instance_config("asb-fixture-0"),
        ] {
            for field in [
                "write_into_file: true",
                "file_write_period_ms: 1000",
                "max_file_size_bytes: 1073741824",
                "compression { zstd { level: 1 } }",
                "buffers { size_kb: 131072",
                "buffer_size_kb: 4096",
                "buffer_size_lower_bound: true",
                "drain_period_ms: 50",
                "drain_buffer_percent: 50",
            ] {
                assert!(config.contains(field), "missing {field}");
            }
            assert!(
                !config.contains("output_path:"),
                "the client must pass its precreated artifact fd"
            );
            assert!(!config.contains("sched_stat_"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn coverage_rejects_missing_boundaries_data_loss_and_invalid_processor_results() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("asb-trace-coverage-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for (name, row, expected) in [
            ("complete", "1,1,0", None),
            ("startup_missing", "0,1,0", Some("startup markers=0")),
            (
                "tail_missing",
                "1,0,0",
                Some("terminal boundary valid=false"),
            ),
            ("duplicate", "2,1,0", Some("startup markers=2")),
            ("loss", "1,1,1", Some("data loss=true")),
            ("invalid", "not-a-row", Some("invalid processor result")),
        ] {
            let processor = root.join(name);
            fs::write(&processor, format!("#!/bin/sh\nprintf '%s\\n' '{row}'\n")).unwrap();
            fs::set_permissions(&processor, fs::Permissions::from_mode(0o755)).unwrap();
            let result = verify_trace_coverage(&processor, &root.join("trace"), "42-1");
            if let Some(expected) = expected {
                assert!(result.unwrap_err().contains(expected));
            } else {
                result.unwrap();
            }
        }
        assert!(
            verify_trace_coverage(&root.join("absent"), &root.join("trace"), "42-1")
                .unwrap_err()
                .contains("could not start")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires installed Perfetto trace processor; no tracing or provider"]
    fn real_processor_validates_success_failure_and_duplicate_terminal_order() {
        let tools = PathBuf::from(std::env::var_os("ASB_TEST_PERFETTO_TOOLS").unwrap());
        let root = std::env::temp_dir().join(format!("asb-coverage-order-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for (index, (names, valid)) in [
            (vec!["trace.ready", "run.finishing"], true),
            (vec!["trace.ready", "run.failed", "run.finishing"], true),
            (vec!["trace.ready", "run.failed"], true),
            (vec!["trace.ready", "run.finishing", "run.failed"], false),
            (vec!["run.finishing", "trace.ready"], false),
            (vec!["trace.ready", "run.finishing", "run.finishing"], false),
            (vec!["trace.ready", "run.failed", "run.failed"], false),
        ]
        .into_iter()
        .enumerate()
        {
            let trace = root.join(format!("case-{index}.json"));
            let events = names.iter().enumerate().map(|(i, name)| serde_json::json!({"name":format!("asb.{name}.42-1"),"ph":"i","s":"t","pid":42,"tid":42,"ts":(i+1)*1000})).collect::<Vec<_>>();
            fs::write(
                &trace,
                serde_json::to_vec(&serde_json::json!({"traceEvents":events})).unwrap(),
            )
            .unwrap();
            let result = verify_trace_coverage(
                &tools.join("perfetto/bin/trace_processor_shell"),
                &trace,
                "42-1",
            );
            assert_eq!(result.is_ok(), valid, "case{index}: {result:?}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn perfetto_declares_namespace_launcher_and_detects_path_shadowing() {
        use crate::runtime_inputs::{Context, Dependency};
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(
            crate::workloads::TaskIdentity::local("perfetto-runtime-inputs", None).attempt_id,
        );
        let project = crate::project::ProjectConfig::default();
        let bin = root.join(&project.tools_dir).join("perfetto/bin");
        fs::create_dir_all(&bin).unwrap();
        for name in ["tracebox", "trace_processor_shell"] {
            let path = bin.join(name);
            fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let inputs = ADAPTER
            .runtime_inputs(&Context {
                root: &root,
                project: &project,
            })
            .unwrap();
        assert!(inputs.iter().any(
            |input| matches!(input, Dependency::Program { path, .. } if path.ends_with("tracebox"))
        ));
        assert!(inputs.iter().any(|input| matches!(input, Dependency::Program { path, .. } if path.ends_with("trace_processor_shell"))));
        let mut launcher = inputs.into_iter().find(|input| matches!(input, Dependency::Program { path, search: None } if path == Path::new("unshare"))).expect("same inherited PATH lookup as launch");
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::copy("/bin/true", second.join("unshare")).unwrap();
        if let Dependency::Program { search, .. } = &mut launcher {
            *search = Some(
                std::env::join_paths([&first, &second, Path::new("/usr/bin"), Path::new("/bin")])
                    .unwrap()
                    .into_string()
                    .unwrap(),
            );
        }
        let before =
            crate::runtime_inputs::discover(Path::new("python3"), &[launcher.clone()]).unwrap();
        assert!(before.contains(&second.join("unshare")));
        fs::copy("/bin/true", first.join("unshare")).unwrap();
        let after = crate::runtime_inputs::discover(Path::new("python3"), &[launcher]).unwrap();
        assert!(after.contains(&first.join("unshare")));
        assert_ne!(
            before, after,
            "a new earlier PATH candidate must invalidate the declared runtime closure"
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn scope_fixture_query(query: &str, scenario: &str) -> serde_json::Value {
        let script = r#"import json,sqlite3,sys
c=sqlite3.connect(':memory:')
c.executescript('''CREATE TABLE process(upid INTEGER,pid INTEGER,parent_upid INTEGER,start_ts INTEGER,end_ts INTEGER);
CREATE TABLE thread(utid INTEGER,upid INTEGER);
CREATE TABLE sched(utid INTEGER,ts INTEGER,dur INTEGER);
CREATE TABLE slice(name TEXT,ts INTEGER);
INSERT INTO process VALUES(1,42,NULL,0,50),(2,42,NULL,60,150),(3,42,NULL,170,300),(4,77,2,80,140),(5,88,1,10,40);
INSERT INTO thread VALUES(1,1),(2,2),(3,3),(4,4),(5,5);
INSERT INTO sched VALUES(1,20,5),(2,90,1),(2,110,10),(3,180,10),(4,120,10),(5,25,2);
INSERT INTO slice VALUES('asb.agent.started.fixture',100),('asb.agent.exited.fixture',200);''')
case=sys.argv[2]
if case in ('no-markers','unique-legacy'): c.execute('DELETE FROM slice')
if case=='unique-legacy': c.execute('DELETE FROM process WHERE upid!=2')
if case=='ambiguous': c.execute('UPDATE process SET end_ts=NULL WHERE upid=1')
if case=='unknown-start': c.execute('UPDATE process SET start_ts=NULL WHERE upid IN (2,3)')
if case=='invalid-bounds': c.execute("UPDATE slice SET ts=80 WHERE name='asb.agent.exited.fixture'")
print(json.dumps(c.execute(sys.argv[1]).fetchall()))
"#;
        let output = Command::new("python3")
            .args(["-c", script, query, scenario])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    #[test]
    fn reused_perfetto_pids_select_only_the_unique_generation_at_agent_start() {
        let scope = perfetto_scope_sql(42, "fixture");
        for scenario in ["normal", "unknown-start"] {
            assert_eq!(
                scope_fixture_query(
                    &format!("{scope} SELECT upid FROM tree ORDER BY upid"),
                    scenario
                ),
                serde_json::json!([[2], [4]])
            );
            for (_, query) in perfetto_metric_queries(Some(42), "fixture") {
                assert_eq!(
                    scope_fixture_query(&query, scenario),
                    serde_json::json!([[2]])
                );
            }
        }
        for scenario in ["no-markers", "ambiguous", "invalid-bounds"] {
            for (_, query) in perfetto_metric_queries(Some(42), "fixture") {
                assert_eq!(
                    scope_fixture_query(&query, scenario),
                    serde_json::json!([]),
                    "ambiguous lifetime must be missing, not zero: {scenario}"
                );
            }
            for (_, query) in perfetto_series_queries(Some(42), "fixture") {
                assert_eq!(scope_fixture_query(&query, scenario), serde_json::json!([]));
            }
        }
        assert_eq!(
            scope_fixture_query(&format!("{scope} SELECT upid FROM root"), "unique-legacy"),
            serde_json::json!([[2]])
        );
    }

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
        session.start_perfetto_with_marker(&marker, None).unwrap();
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
        let error = session
            .start_perfetto_with_marker(&marker, None)
            .unwrap_err();
        assert!(error.contains("tracing is off"), "{error}");
        let pid = session.collectors[0].producer.id().unwrap();
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
    fn executable_busy_retry_is_bounded_and_other_errors_are_immediate() {
        let mut calls = 0;
        let result = retry_executable_busy(|| {
            calls += 1;
            if calls < 3 {
                Err(std::io::Error::from_raw_os_error(26))
            } else {
                Ok(42)
            }
        })
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(calls, 3);
        calls = 0;
        let result: std::io::Result<()> = retry_executable_busy(|| {
            calls += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        });
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(calls, 1);
        let result: std::io::Result<()> =
            retry_executable_busy(|| Err(std::io::Error::from_raw_os_error(26)));
        assert_eq!(result.unwrap_err().raw_os_error(), Some(26));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trace_processor_waits_for_a_writable_executable_to_close() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("asb-marker-busy-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let processor = root.join("processor");
        let mut writer = File::create(&processor).unwrap();
        writer
            .write_all(b"#!/bin/sh\nprintf '\"COUNT(*)\"\\n1\\n'\n")
            .unwrap();
        fs::set_permissions(&processor, fs::Permissions::from_mode(0o755)).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            drop(writer);
        });
        let result = verify_trace_marker_capture(&processor, &root.join("trace"), "10-1");
        release.join().unwrap();
        result.unwrap();
        fs::remove_dir_all(root).unwrap();
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
        let scoped = processor_query(
            &processor,
            &trace,
            &format!(
                "{} SELECT COUNT(*) FROM root",
                perfetto_scope_sql(42, "42-1")
            ),
        )
        .unwrap();
        assert!(
            scoped.status.success(),
            "{}",
            String::from_utf8_lossy(&scoped.stderr)
        );
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
                project_root: None,
                states: &mut crate::collectors::CollectorStates::default(),
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
