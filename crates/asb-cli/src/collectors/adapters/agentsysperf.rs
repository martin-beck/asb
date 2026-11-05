//! Export ASB-owned, agent-scoped observations in AgentSysPerf's record shape.
//!
//! AgentSysPerf's own runner is deliberately not invoked for an ASB benchmark:
//! it would own a second workload/agent lifecycle and its legacy `perf -a`
//! default could observe unrelated host work.  This adapter instead exports
//! already verified ASB process-tree observations for later offline analysis.

use super::super::{
    ArtifactDisplay, CollectorAdapter, ReportGraph, ReportRow, SelectionExtension, SelectionOption,
};
use crate::measurements::{self, ComponentTiming, Session};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

mod interpretation;

const MAX_ANALYSIS_BYTES: usize = 1024 * 1024;
const ANALYSIS_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) struct AgentSysPerfCollector;
pub(super) static ADAPTER: AgentSysPerfCollector = AgentSysPerfCollector;

const DEPENDENCIES: &[&str] = &["agent-memory", "agent-workers", "cpu-accounting", "perf"];

#[derive(Default)]
struct State {
    enabled: bool,
    scaling_requested: bool,
    platform: Option<PlatformManifest>,
}

/// Capability evidence for optional processor telemetry.  Event names are
/// accepted only after the local `perf list` advertises them; ASB never copies
/// an Intel encoding onto an AMD or Arm machine.
#[derive(Clone, Serialize)]
struct PlatformManifest {
    family: &'static str,
    architecture: &'static str,
    perf_events: Vec<String>,
}

#[derive(Serialize)]
struct MeasurementRecord {
    span_id: String,
    layer: &'static str,
    payload: Map<String, Value>,
}

impl CollectorAdapter for AgentSysPerfCollector {
    fn default_enabled(&self) -> bool {
        false
    }
    fn description(&self) -> &'static str {
        "AgentSysPerf-compatible agent-scoped measurement records"
    }

    fn id(&self) -> &'static str {
        "agentsysperf:l1-resource"
    }

    fn accepts_selection(&self, selection: &str) -> bool {
        matches!(
            selection,
            "agentsysperf:l1-resource" | "agentsysperf:scaling"
        )
    }

    fn selection_extension(&self) -> Option<SelectionExtension> {
        Some(SelectionExtension {
            tab: "AgentSysPerf modes",
            description: "Choose process-tree analysis and optional offline sweep scaling diagnostics.",
            all_selection: Some("agentsysperf:l1-resource".into()),
            all_description: "agent-process-tree resource analysis",
            required: true,
            options: vec![
                SelectionOption {
                    selection: "agentsysperf:l1-resource".into(),
                    label: "resource analysis",
                    description: "agent-scoped records, Analyzer findings, and bottleneck report",
                    default: true,
                },
                SelectionOption {
                    selection: "agentsysperf:scaling".into(),
                    label: "sweep scaling",
                    description: "offline latency/service-rate curves for sealed compatible sweep cohorts",
                    default: false,
                },
            ],
        })
    }

    fn tool_id(&self) -> Option<&'static str> {
        Some("agentsysperf")
    }

    fn required_tools(&self) -> &'static [&'static str] {
        &["agentsysperf"]
    }

    fn dependencies(&self) -> &'static [&'static str] {
        DEPENDENCIES
    }

    fn prioritizes_artifact(&self, name: &str) -> bool {
        name == "agentsysperf-analysis.json"
    }

    fn display_artifact(&self, name: &str) -> Option<ArtifactDisplay> {
        (name == "agentsysperf-analysis.json").then_some(ArtifactDisplay::Status {
            class: "agentsysperf-interpretation",
            label: "External AgentSysPerf diagnostics",
            suffix: "Analyzer completion does not qualify its diagnoses. Read interpretation_status and interpretation_reason. ASB's normalized hardware/kernel observations retain their own scope and quality.",
        })
    }

    fn report_graphs(&self, rows: &[ReportRow<'_>]) -> Vec<ReportGraph> {
        let mut graphs = bottleneck_graph(rows).into_iter().collect::<Vec<_>>();
        graphs.extend(scaling_graphs(rows));
        graphs
    }

    fn configure(&self, session: &mut Session, selected: &[String]) -> Result<(), String> {
        let enabled = crate::collectors::selected_or_environment(self.id(), selected);
        let state = session.state::<State>();
        let mut state = state.borrow_mut();
        state.enabled = enabled;
        state.scaling_requested = selected.iter().any(|value| value == "agentsysperf:scaling");
        state.platform = enabled.then(|| discover_platform(session));
        Ok(())
    }

    fn after_analysis(
        &self,
        session: &mut Session,
        normalized: Option<&Value>,
    ) -> Result<(), String> {
        if !session.state::<State>().borrow().enabled {
            return Ok(());
        }

        let Some(normalized) =
            normalized.filter(|value| value["engine"] == "asb-resource-analysis-v1")
        else {
            return write_unavailable(
                session,
                "Current normalized analysis is unavailable; raw counters and previous analysis are not substituted.",
            );
        };
        measurements::write_json(
            &session.dir.join("agentsysperf-normalized-input.json"),
            normalized,
        )?;
        let Some(timing) = session.component_timing("agent.execute") else {
            return write_unavailable(session, "ASB did not observe an agent execution interval.");
        };
        let span_id = format!("asb::{}::agent-execute", session.trace_id());
        let records = match records(session, &span_id, timing, normalized) {
            Ok(records) => records,
            Err(reason) => return write_unavailable(session, &reason),
        };
        if records.is_empty() {
            return write_unavailable(
                session,
                "No complete agent-process-tree observations were available for AgentSysPerf export.",
            );
        }
        let directory = session.dir.join("agentsysperf-records");
        fs::create_dir_all(&directory)
            .map_err(|error| format!("cannot create AgentSysPerf record directory: {error}"))?;
        measurements::write_json(&directory.join("measurement_records.json"), &records)?;
        let analysis = analyze(session, &directory);
        measurements::write_json(&session.dir.join("agentsysperf-analysis.json"), &analysis)?;
        measurements::write_json(
            &session.dir.join("agentsysperf.json"),
            &json!({
                "schema_version": 1,
                "collector": "agentsysperf:l1-resource",
                "status": "observed",
                "scope": "selected-agent-process-tree",
                "analysis_input": "agentsysperf-records/measurement_records.json",
                "normalized_input": "agentsysperf-normalized-input.json",
                "records": records.len(),
                "scaling_requested": session.state::<State>().borrow().scaling_requested,
                "platform": session.state::<State>().borrow().platform,
                "analysis": analysis["status"],
                "analysis_interpretation": analysis["interpretation_status"],
                "limitations": "This is an ASB-owned process-tree export for offline AgentSysPerf analysis. It does not run AgentSysPerf's benchmark harness or accept system-wide perf attribution. Missing observations are omitted rather than represented as zero. External analyzer completion does not qualify its causal classifications; see analysis_interpretation and the analysis reason. L3 is exported only for one coherent PMU domain; hybrid domains remain separate in the normalized input and are never summed."
            }),
        )
    }
}

fn analyze(session: &Session, records: &std::path::Path) -> Value {
    #[cfg(unix)]
    {
        analyze_unix(session, records)
    }
    #[cfg(not(unix))]
    {
        let _ = (session, records);
        json!({"schema_version": 1, "status": "unavailable", "reason": "bounded AgentSysPerf analysis currently requires Unix"})
    }
}

#[cfg(unix)]
fn analyze_unix(session: &Session, records: &std::path::Path) -> Value {
    use std::os::unix::process::CommandExt;

    let executable = measurements::resolve_executable(session.tool_directory(), "agentsysperf");
    let Some(executable) = crate::tools::host_executable(&executable) else {
        return json!({"schema_version": 1, "status": "unavailable", "reason": "project-local AgentSysPerf executable is unavailable"});
    };
    // This is raw diagnostic evidence, not a collector document.  The
    // analyzer deliberately emits a human preamble before its JSON array, so
    // a `.json` suffix would make comparison's generic collector discovery
    // reject an otherwise valid completed measurement.
    let stdout_path = session.dir.join("agentsysperf-analysis.stdout.log");
    let stderr_path = session.dir.join("agentsysperf-analysis.stderr.log");
    let stdout = match fs::File::create(&stdout_path) {
        Ok(file) => file,
        Err(error) => {
            return json!({"schema_version": 1, "status": "unavailable", "reason": format!("cannot create AgentSysPerf analysis evidence: {error}")});
        }
    };
    let stderr = match fs::File::create(&stderr_path) {
        Ok(file) => file,
        Err(error) => {
            return json!({"schema_version": 1, "status": "unavailable", "reason": format!("cannot create AgentSysPerf analysis evidence: {error}")});
        }
    };
    let limit = libc::rlimit {
        rlim_cur: MAX_ANALYSIS_BYTES as libc::rlim_t,
        rlim_max: MAX_ANALYSIS_BYTES as libc::rlim_t,
    };
    let mut command = Command::new(executable);
    command
        .args(["analyze"])
        .arg(records)
        .args(["--format", "json"])
        .current_dir(&session.dir)
        .env("AGENTSYSPERF_HOME", session.dir.join("agentsysperf-home"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    // SAFETY: `setrlimit` is async-signal-safe and the copied limit contains
    // no borrowed state. The child may write no more than this bound to either
    // evidence file, even if a damaged external analyzer is noisy.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(move || {
            if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return json!({"schema_version": 1, "status": "unavailable", "reason": format!("cannot start AgentSysPerf analysis: {error}")});
        }
    };
    let deadline = Instant::now() + ANALYSIS_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return json!({"schema_version": 1, "status": "unavailable", "reason": "AgentSysPerf analysis exceeded its 10-second timeout"});
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return json!({"schema_version": 1, "status": "unavailable", "reason": format!("cannot observe AgentSysPerf analysis: {error}")});
            }
        }
    };
    let stdout = read_bounded(&stdout_path);
    let stderr = read_bounded(&stderr_path);
    if stdout.is_none() || stderr.is_none() {
        return json!({"schema_version": 1, "status": "unavailable", "reason": "AgentSysPerf analysis exceeded the bounded evidence size"});
    }
    if !status.success() {
        return json!({"schema_version": 1, "status": "unavailable", "reason": "AgentSysPerf analysis failed", "exit": status.code()});
    }
    match analysis_findings(&stdout.expect("checked above")) {
        Ok(findings) if findings.is_array() => interpretation::document(findings),
        _ => {
            json!({"schema_version": 1, "status": "unavailable", "reason": "AgentSysPerf analysis did not return a JSON finding list"})
        }
    }
}

/// AgentSysPerf currently prints a one-line progress preamble before its JSON
/// report.  Keep that human text in the private bounded stdout artifact, then
/// accept only a complete JSON array beginning at the first array delimiter.
/// This is intentionally not a line scraper: trailing bytes or any non-array
/// JSON remain unavailable rather than becoming a partial diagnosis.
fn analysis_findings(stdout: &[u8]) -> Result<Value, serde_json::Error> {
    let start = stdout
        .iter()
        .position(|byte| *byte == b'[')
        .unwrap_or(stdout.len());
    serde_json::from_slice(&stdout[start..])
}

fn read_bounded(path: &std::path::Path) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take((MAX_ANALYSIS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= MAX_ANALYSIS_BYTES).then_some(bytes)
}

/// Publish only the analyzer's explicit, bounded diagnostic confidence.  A
/// finding is not a benchmark score: absent, malformed, or unavailable
/// analysis never becomes a zero and never contributes a ranking metric.
fn bottleneck_graph(rows: &[ReportRow<'_>]) -> Option<ReportGraph> {
    const MAX_FINDINGS: usize = 64;
    let mut findings = Vec::new();
    for row in rows {
        let Some(analysis) = row.documents.get("agentsysperf-analysis") else {
            continue;
        };
        if analysis.get("status").and_then(Value::as_str) != Some("observed")
            || analysis
                .get("interpretation_status")
                .and_then(Value::as_str)
                != Some("qualified")
        {
            continue;
        }
        let Some(entries) = analysis.get("qualified_findings").and_then(Value::as_array) else {
            continue;
        };
        for finding in entries {
            let Some(analyzer) = finding.get("analyzer").and_then(Value::as_str) else {
                continue;
            };
            let Some(verdict) = finding.get("verdict").and_then(Value::as_str) else {
                continue;
            };
            let Some(confidence) = finding.get("confidence").and_then(Value::as_f64) else {
                continue;
            };
            if !(0.0..=1.0).contains(&confidence) || !confidence.is_finite() {
                continue;
            }
            findings.push((
                bounded_label(&format!("{} · {analyzer}: {verdict}", row.label)),
                confidence,
            ));
            if findings.len() == MAX_FINDINGS {
                break;
            }
        }
        if findings.len() == MAX_FINDINGS {
            break;
        }
    }
    (!findings.is_empty()).then(|| {
        let height = 100.0 + findings.len() as f64 * 24.0;
        let left = 330.0;
        let right = 940.0;
        let mut svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 980 {height:.0}\" role=\"img\" aria-label=\"AgentSysPerf bottleneck finding confidence\"><rect width=\"980\" height=\"{height:.0}\" fill=\"#fff\"/><text x=\"24\" y=\"32\" font-family=\"sans-serif\" font-size=\"18\" font-weight=\"700\">AgentSysPerf bottleneck findings</text><text x=\"24\" y=\"54\" font-family=\"sans-serif\" font-size=\"12\" fill=\"#61707f\">Diagnostic confidence only; findings do not affect ASB grades or rankings.</text>"
        );
        for tick in 0..=4 {
            let value = tick as f64 / 4.0;
            let x = left + (right - left) * value;
            svg.push_str(&format!("<line x1=\"{x:.1}\" y1=\"72\" x2=\"{x:.1}\" y2=\"{:.1}\" stroke=\"#dce4ec\"/><text x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"10\" fill=\"#61707f\">{value:.2}</text>", height - 22.0, height - 7.0));
        }
        for (index, (label, confidence)) in findings.iter().enumerate() {
            let y = 76.0 + index as f64 * 24.0;
            let width = (right - left) * confidence;
            svg.push_str(&format!("<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" font-family=\"sans-serif\" font-size=\"11\">{}</text><rect class=\"bar\" data-label=\"{}\" data-value=\"{confidence:.3}\" x=\"{left}\" y=\"{:.1}\" width=\"{width:.1}\" height=\"16\" rx=\"3\" fill=\"#4472c4\"><title>{}: confidence {confidence:.3}</title></rect>", left - 10.0, y + 12.0, escape(label), escape(label), y, escape(label)));
        }
        svg.push_str("</svg>\n");
        ReportGraph {
            filename: "agentsysperf-bottleneck-findings.svg".into(),
            svg,
        }
    })
}

/// A conservative, offline scaling view for completed ASB dispatch cohorts.
///
/// AgentSysPerf's upstream sweep runner owns a different Harbor lifecycle, so
/// ASB must not invoke it around an ASB sweep.  This collector instead derives
/// a curve from already-sealed ASB rows.  It deliberately uses *summed* elapsed
/// time for service rate: an execution cohort records admission provenance, not
/// proof that starts overlapped, so dividing by the fastest member would invent
/// throughput.  Incomplete cohorts, mixed trials, and repeated cohorts at one
/// point are omitted rather than averaged into a plausible-looking knee.
fn scaling_graphs(rows: &[ReportRow<'_>]) -> Vec<ReportGraph> {
    #[derive(Clone)]
    struct Observation {
        elapsed_ms: f64,
        label: String,
    }
    type Cohorts = BTreeMap<String, Vec<Observation>>;
    type Points = BTreeMap<usize, Cohorts>;
    let mut groups = BTreeMap::<String, Points>::new();
    for row in rows {
        let Some(document) = row.documents.get("agentsysperf") else {
            continue;
        };
        if document.get("status").and_then(Value::as_str) != Some("observed")
            || document.get("scaling_requested").and_then(Value::as_bool) != Some(true)
        {
            continue;
        }
        let Some(cohort) = row.execution_cohort else {
            continue;
        };
        if row.configured_concurrency < 1
            || cohort.selected_replicas != row.configured_concurrency
            || cohort.admitted_slots.len() != row.configured_concurrency
        {
            continue;
        }
        let Some(elapsed_ms) = row.metrics.get("elapsed_ms").copied() else {
            continue;
        };
        if !elapsed_ms.is_finite() || elapsed_ms <= 0.0 {
            continue;
        }
        // A scaling curve reflects completed benchmark work, never a mixture
        // of failed/unresolved slots with successful ones.
        if row.metrics.get("success_rate") != Some(&1.0) {
            continue;
        }
        let key = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            row.agent, row.provider, row.model, row.workload, row.trial
        );
        groups
            .entry(key)
            .or_default()
            .entry(row.configured_concurrency)
            .or_default()
            .entry(cohort.id.clone())
            .or_default()
            .push(Observation {
                elapsed_ms,
                label: row.label.into(),
            });
    }

    const MAX_GRAPHS: usize = 8;
    let mut graphs = Vec::new();
    for (key, points) in groups {
        if graphs.len() == MAX_GRAPHS {
            break;
        }
        let mut curve = Vec::new();
        for (concurrency, cohorts) in points {
            // A repeated sweep point needs an explicit aggregation policy. Do
            // not silently combine it with a different cohort.
            if cohorts.len() != 1 {
                continue;
            }
            let observations = cohorts.into_values().next().unwrap_or_default();
            if observations.len() != concurrency {
                continue;
            }
            let mut elapsed = observations
                .iter()
                .map(|observation| observation.elapsed_ms)
                .collect::<Vec<_>>();
            elapsed.sort_by(f64::total_cmp);
            let p95_index = ((elapsed.len() * 95).saturating_add(99) / 100)
                .saturating_sub(1)
                .min(elapsed.len().saturating_sub(1));
            let summed_ms = elapsed.iter().sum::<f64>();
            if summed_ms <= 0.0 {
                continue;
            }
            curve.push((
                concurrency,
                elapsed[p95_index],
                observations.len() as f64 * 1000.0 / summed_ms,
                observations[0].label.clone(),
            ));
        }
        if curve.len() < 3 {
            continue;
        }
        curve.sort_by_key(|point| point.0);
        let parts = key.split('\u{1f}').collect::<Vec<_>>();
        let title = format!(
            "AgentSysPerf scaling — {} / {} / {} / {} / {}",
            parts[0], parts[1], parts[2], parts[3], parts[4]
        );
        let filename = format!("agentsysperf-scaling-{}.svg", stable_label_id(&key));
        graphs.push(ReportGraph {
            filename,
            svg: scaling_svg(&title, &curve),
        });
    }
    graphs
}

fn stable_label_id(value: &str) -> String {
    // Stable, non-cryptographic presentation identity; this is not used for
    // evidence integrity or a security boundary.
    let hash = value.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    });
    format!("{hash:016x}")
}

fn scaling_svg(title: &str, curve: &[(usize, f64, f64, String)]) -> String {
    let width = 980.0;
    let left = 90.0;
    let right = 940.0;
    let upper_top = 76.0;
    let upper_bottom = 230.0;
    let lower_top = 300.0;
    let lower_bottom = 454.0;
    let max_concurrency = curve.last().map(|point| point.0).unwrap_or(1).max(1) as f64;
    let max_latency = curve
        .iter()
        .map(|point| point.1)
        .fold(0.0f64, f64::max)
        .max(1.0);
    let max_rate = curve
        .iter()
        .map(|point| point.2)
        .fold(0.0f64, f64::max)
        .max(1.0);
    let x = |concurrency: usize| left + (right - left) * concurrency as f64 / max_concurrency;
    let y_latency =
        |latency: f64| upper_bottom - (upper_bottom - upper_top) * latency / max_latency;
    let y_rate = |rate: f64| lower_bottom - (lower_bottom - lower_top) * rate / max_rate;
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width:.0} 500\" role=\"img\" aria-label=\"{}\"><rect width=\"{width:.0}\" height=\"500\" fill=\"#fff\"/><text x=\"24\" y=\"30\" font-family=\"sans-serif\" font-size=\"17\" font-weight=\"700\">{}</text><text x=\"24\" y=\"50\" font-family=\"sans-serif\" font-size=\"11\" fill=\"#61707f\">Only complete, sealed cohorts. Rate is completions divided by summed end-to-end elapsed time; it is not inferred wall-clock throughput.</text>",
        escape(title),
        escape(title)
    );
    for (top, bottom, label, maximum) in [
        (
            upper_top,
            upper_bottom,
            "p95 end-to-end latency (ms)",
            max_latency,
        ),
        (
            lower_top,
            lower_bottom,
            "aggregate service rate (completed/s)",
            max_rate,
        ),
    ] {
        for tick in 0..=4 {
            let fraction = tick as f64 / 4.0;
            let y = bottom - (bottom - top) * fraction;
            svg.push_str(&format!("<line x1=\"{left:.1}\" y1=\"{y:.1}\" x2=\"{right:.1}\" y2=\"{y:.1}\" stroke=\"#dce4ec\"/><text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\" font-family=\"sans-serif\" font-size=\"10\" fill=\"#61707f\">{:.1}</text>", left - 8.0, y + 3.0, maximum * fraction));
        }
        svg.push_str(&format!("<text x=\"{left:.1}\" y=\"{:.1}\" font-family=\"sans-serif\" font-size=\"12\" font-weight=\"700\">{label}</text>", top - 8.0));
    }
    for (index, (concurrency, latency, rate, label)) in curve.iter().enumerate() {
        let px = x(*concurrency);
        if index > 0 {
            let previous = &curve[index - 1];
            svg.push_str(&format!("<path d=\"M {:.1} {:.1} L {:.1} {:.1}\" fill=\"none\" stroke=\"#4472c4\" stroke-width=\"2\"/><path d=\"M {:.1} {:.1} L {:.1} {:.1}\" fill=\"none\" stroke=\"#e07a28\" stroke-width=\"2\"/>", x(previous.0), y_latency(previous.1), px, y_latency(*latency), x(previous.0), y_rate(previous.2), px, y_rate(*rate)));
        }
        for (y, color, value, unit) in [
            (y_latency(*latency), "#4472c4", *latency, "ms"),
            (y_rate(*rate), "#e07a28", *rate, "completed/s"),
        ] {
            svg.push_str(&format!("<circle cx=\"{px:.1}\" cy=\"{y:.1}\" r=\"4\" fill=\"{color}\"><title>{}: concurrency {}, {:.3} {unit}</title></circle>", escape(label), concurrency, value));
        }
        svg.push_str(&format!("<text x=\"{px:.1}\" y=\"482\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"10\" fill=\"#61707f\">{concurrency}</text>"));
    }
    svg.push_str(&format!("<text x=\"{:.1}\" y=\"496\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"11\">configured concurrency</text><text x=\"{left:.1}\" y=\"270\" font-family=\"sans-serif\" font-size=\"11\" fill=\"#4472c4\">● latency</text><text x=\"{:.1}\" y=\"270\" font-family=\"sans-serif\" font-size=\"11\" fill=\"#e07a28\">● service rate</text></svg>\n", width / 2.0, left + 120.0));
    svg
}

fn bounded_label(value: &str) -> String {
    const MAX_LABEL_BYTES: usize = 180;
    if value.len() <= MAX_LABEL_BYTES {
        return value.into();
    }
    let mut end = MAX_LABEL_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Hardware counters that the shared process-tree `perf stat` collector may
/// add to its already-scoped command.  Returning an empty list is a normal
/// unavailable-capability result, never a reason to fall back to `perf -a`.
pub(super) fn telemetry_events(session: &mut Session) -> Vec<String> {
    session
        .state::<State>()
        .borrow()
        .platform
        .as_ref()
        .map(|manifest| manifest.perf_events.clone())
        .unwrap_or_default()
}

fn discover_platform(session: &Session) -> PlatformManifest {
    let architecture = match std::env::consts::ARCH {
        "aarch64" => "armv8",
        "x86_64" => "x86_64",
        other => other,
    };
    let cpuinfo = fs::File::open("/proc/cpuinfo")
        .ok()
        .and_then(|file| {
            let mut bytes = Vec::new();
            file.take(1024 * 1024 + 1).read_to_end(&mut bytes).ok()?;
            (bytes.len() <= 1024 * 1024)
                .then(|| String::from_utf8_lossy(&bytes).to_ascii_lowercase())
        })
        .unwrap_or_default();
    let family = if architecture == "armv8" {
        "armv8"
    } else if cpuinfo.contains("authenticamd") || cpuinfo.contains("amd") {
        "amd"
    } else if cpuinfo.contains("genuineintel") || cpuinfo.contains("intel") {
        "intel"
    } else {
        "unknown"
    };
    let local = session
        .tool_directory()
        .map(|root| crate::tools::local_lookup("perf", root));
    let executable = match local {
        Some(crate::executables::Lookup::Found(path)) => Some(path),
        Some(crate::executables::Lookup::Unusable(_)) => None,
        _ => crate::tools::host_executable(std::path::Path::new("perf")),
    };
    let listing = executable
        .and_then(|path| {
            measurements::bounded_command(
                &path,
                measurements::SnapshotCommand {
                    arguments: vec!["list".into(), "--no-desc".into()],
                    runtime: Duration::from_secs(3),
                    stream_bytes: 1024 * 1024,
                },
            )
        })
        .filter(|(_, stderr)| stderr.is_empty())
        .and_then(|(stdout, _)| String::from_utf8(stdout).ok())
        .unwrap_or_default();
    let perf_events = telemetry_groups(&listing);
    PlatformManifest {
        family,
        architecture,
        perf_events,
    }
}

fn perf_list_advertises(listing: &str, event: &str) -> bool {
    listing.lines().any(|line| {
        line.split('[')
            .next()
            .unwrap_or("")
            .split(" OR ")
            .any(|alias| alias.split_whitespace().next() == Some(event))
    })
}

fn telemetry_groups(listing: &str) -> Vec<String> {
    let mut groups = Vec::new();
    for domain in ["cpu_core", "cpu_atom"] {
        let named = |event| format!("{domain}/{event}/");
        if ["cycles", "instructions"]
            .iter()
            .all(|event| perf_list_advertises(listing, &named(event)))
        {
            let events = ["cycles", "instructions", "cache-misses", "branch-misses"]
                .iter()
                .map(named)
                .filter(|event| perf_list_advertises(listing, event))
                .collect::<Vec<_>>();
            groups.push(format!("{{{}}}", events.join(",")));
        }
    }
    if groups.is_empty()
        && ["cycles", "instructions"]
            .iter()
            .all(|event| perf_list_advertises(listing, event))
    {
        let events = ["cycles", "instructions", "cache-misses", "branch-misses"]
            .iter()
            .filter(|event| perf_list_advertises(listing, event))
            .copied()
            .collect::<Vec<_>>();
        groups.push(format!("{{{}}}", events.join(",")));
    }
    groups
}

fn write_unavailable(session: &Session, reason: &str) -> Result<(), String> {
    measurements::write_json(
        &session.dir.join("agentsysperf.json"),
        &json!({
            "schema_version": 1,
            "collector": "agentsysperf:l1-resource",
            "status": "unavailable",
            "scope": "selected-agent-process-tree",
            "reason": reason,
            "metrics": {}
        }),
    )
}

fn records(
    session: &Session,
    span_id: &str,
    timing: &ComponentTiming,
    normalized: &Value,
) -> Result<Vec<MeasurementRecord>, String> {
    let mut l1 = Map::new();
    l1.insert("kind".into(), json!("agent_execute"));
    l1.insert("node_id".into(), json!("asb-agent"));
    l1.insert(
        "duration_us".into(),
        json!(timing.wall_ms.saturating_mul(1_000)),
    );
    if let Some(value) = normalized_cpu_seconds(normalized) {
        l1.insert("cpu_time_s".into(), json!(value));
    }
    let memory = read_observed_metrics(
        &session.dir.join("agent-memory.json"),
        "selected-agent-process-tree",
    )?;
    if let Some(value) = metric(&memory, "peak_rss_mib") {
        l1.insert("rss_kb_peak".into(), json!(value * 1024.0));
    }
    // Do not translate ASB's process census into AgentSysPerf's thread field:
    // one is an agent process-tree observation and the other is a thread
    // count.  Keeping the dimensions separate avoids a plausible-looking but
    // incorrect bottleneck diagnosis.
    l1.insert("sample_count".into(), json!(1));

    let mut result = vec![MeasurementRecord {
        span_id: span_id.into(),
        layer: "l1",
        payload: l1,
    }];
    if let Some(l3) = l3_record(normalized) {
        result.push(MeasurementRecord {
            span_id: span_id.into(),
            layer: "l3",
            payload: l3,
        });
    }
    Ok(result)
}

fn normalized_cpu_seconds(normalized: &Value) -> Option<f64> {
    let observations = normalized.get("observations")?.as_array()?;
    let value = |id| {
        let mut matches = observations.iter().filter(|o| o["id"] == id);
        let observation = matches.next()?;
        if matches.next().is_some()
            || observation["availability"] != "observed"
            || observation["scope"] != "agent_process_tree"
            || observation["unit"] != "ms"
            || observation["kind"] != "accumulated_thread_time"
        {
            return None;
        }
        observation
            .get("value")?
            .as_f64()
            .filter(|v| v.is_finite() && *v >= 0.0)
    };
    let total = (value("cpu.user_ms")? + value("cpu.system_ms")?) / 1000.0;
    total.is_finite().then_some(total)
}

fn l3_record(normalized: &Value) -> Option<Map<String, Value>> {
    let groups = normalized.get("counter_groups")?.as_array()?;
    // AgentSysPerf's compatibility shape has no PMU-domain dimension. Never
    // sum heterogeneous counters or silently choose one side of a hybrid CPU.
    let [group] = groups.as_slice() else {
        return None;
    };
    if group["scope"] != "agent-process-tree" {
        return None;
    }
    let counts = group.get("coherent_counts")?.as_object()?;
    let mut l3 = Map::new();
    for name in [
        "cycles",
        "instructions",
        "cache_misses",
        "branch_misses",
        "l1d_cache_refill",
        "stall_backend",
        "stall_frontend",
    ] {
        if let Some(value) = counts.get(name).and_then(Value::as_u64) {
            l3.insert(name.into(), json!(value));
        }
    }
    if let Some(ipc) = group
        .get("instructions_per_cycle")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0)
    {
        l3.insert("ipc".into(), json!(ipc));
    }
    (!l3.is_empty()).then_some(l3)
}

fn read_observed_metrics(
    path: &std::path::Path,
    required_scope: &str,
) -> Result<Map<String, Value>, String> {
    let bytes = read_bounded(path).ok_or_else(|| {
        format!(
            "required collector evidence at {} is unreadable or exceeds the bounded input limit",
            path.display()
        )
    })?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| format!("invalid required collector evidence at {}", path.display()))?;
    if value.get("status").and_then(Value::as_str) != Some("observed") {
        return Err(format!(
            "required collector evidence at {} was not observed",
            path.display()
        ));
    }
    if value.get("scope").and_then(Value::as_str) != Some(required_scope) {
        return Err(format!(
            "required collector evidence at {} did not prove {required_scope} scope",
            path.display()
        ));
    }
    Ok(value
        .get("metrics")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default())
}

fn metric(values: &Map<String, Value>, name: &str) -> Option<f64> {
    values
        .get(name)?
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_tree_export_never_samples_host_activity() {
        let root =
            std::env::temp_dir().join(format!("asb-agentsysperf-no-host-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let mut session = Session::start_with_collectors(&root, &[], None).unwrap();
        ADAPTER
            .configure(&mut session, &["agentsysperf:l1-resource".into()])
            .unwrap();
        ADAPTER.start(&mut session).unwrap();
        fs::write(
            root.join("bottlenecks.json"),
            br#"{"engine":"asb-resource-analysis-v1","counter_groups":[]}"#,
        )
        .unwrap();
        fs::write(root.join("perf-stat.json"), br#"{"metrics":{"perf.observed_cycles_count":20,"perf.observed_instructions_count":30}}"#).unwrap();
        ADAPTER.after_analysis(&mut session, None).unwrap();
        assert!(!root.join("agentsysperf-host-telemetry.json").exists());
        let result: Value =
            serde_json::from_slice(&fs::read(root.join("agentsysperf.json")).unwrap()).unwrap();
        assert_eq!(result["status"], "unavailable");
        assert_eq!(result["scope"], "selected-agent-process-tree");
        assert!(!root.join("agentsysperf-records").exists());
        drop(session);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mapped_l3_requires_normalized_groups_and_preserves_exact_counts() {
        let counts =
            json!({"cycles":9_007_199_254_740_993_u64,"instructions":18_014_398_509_481_986_u64});
        let group = json!({"scope":"agent-process-tree","coherent_counts":counts,"instructions_per_cycle":2.0});
        let mut normalized = json!({"counter_groups":[group]});
        let record = l3_record(&normalized).unwrap();
        assert_eq!(record["cycles"].as_u64(), Some(9_007_199_254_740_993));
        assert_eq!(record["ipc"], 2.0);
        assert!(!record.contains_key("cache_misses"));
        normalized["counter_groups"][0]["instructions_per_cycle"] = Value::Null;
        assert!(!l3_record(&normalized).unwrap().contains_key("ipc"));
        let group = normalized["counter_groups"][0].clone();
        normalized["counter_groups"]
            .as_array_mut()
            .unwrap()
            .push(group);
        assert!(
            l3_record(&normalized).is_none(),
            "hybrid domains must not be combined"
        );
        assert!(l3_record(&json!({"metrics":{"perf.observed_cycles_count":20,"perf.observed_instructions_count":30}})).is_none());
    }

    #[test]
    fn normalized_cpu_requires_both_agent_scoped_observations() {
        let observation = |id, value| json!({"id":id,"scope":"agent_process_tree","availability":"observed","unit":"ms","kind":"accumulated_thread_time","value":value});
        let mut n = json!({"observations":[observation("cpu.user_ms",1500.0),observation("cpu.system_ms",500.0)]});
        assert_eq!(normalized_cpu_seconds(&n), Some(2.0));
        for (field, value) in [
            ("scope", json!("unknown")),
            ("availability", json!("unavailable")),
            ("value", Value::Null),
            ("value", json!(-1)),
        ] {
            let mut invalid = n.clone();
            invalid["observations"][1][field] = value;
            assert!(normalized_cpu_seconds(&invalid).is_none());
        }
        n["observations"].as_array_mut().unwrap().pop();
        assert!(
            normalized_cpu_seconds(&n).is_none(),
            "missing system time is not zero"
        );
    }

    #[test]
    fn negative_or_nonfinite_values_never_enter_exported_records() {
        let mut metrics = Map::new();
        metrics.insert("bad".into(), json!(-1.0));
        metrics.insert("missing".into(), Value::Null);
        assert!(metric(&metrics, "bad").is_none());
        assert!(metric(&metrics, "missing").is_none());
    }

    #[test]
    fn scope_evidence_must_be_observed_and_exact() {
        let root =
            std::env::temp_dir().join(format!("asb-agentsysperf-scope-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("agent-memory.json");
        fs::write(
            &path,
            r#"{"status":"observed","scope":"selected-agent-process-tree","metrics":{"peak_rss_mib":2}}"#,
        )
        .unwrap();
        assert_eq!(
            metric(
                &read_observed_metrics(&path, "selected-agent-process-tree").unwrap(),
                "peak_rss_mib"
            ),
            Some(2.0)
        );
        fs::write(
            &path,
            r#"{"status":"incomplete","scope":"selected-agent-process-tree","metrics":{}}"#,
        )
        .unwrap();
        assert!(read_observed_metrics(&path, "selected-agent-process-tree").is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn platform_selection_uses_only_advertised_vendor_events() {
        let arm = "  cycles\n  instructions\n  cache-misses\n  branch-misses\n  l1d_cache_refill OR armv8_pmuv3_0/l1d_cache_refill/\n  stall_backend\n  stall_frontend\n";
        assert!(perf_list_advertises(arm, "l1d_cache_refill"));
        assert!(!perf_list_advertises(arm, "topdown-fe-bound"));
        assert_eq!(
            telemetry_groups(arm),
            ["{cycles,instructions,cache-misses,branch-misses}"]
        );
        assert!(telemetry_groups("cpu_core/cycles/\n instructions\n").is_empty());
        assert!(telemetry_groups("uncore_imc/cycles/\nuncore_imc/instructions/\n").is_empty());
        assert!(!perf_list_advertises(
            "a description about cycles",
            "cycles"
        ));
        assert_eq!(
            telemetry_groups(
                "cycles OR cpu_core/cycles/ [Hardware event]\ninstructions OR cpu_core/instructions/\ncpu_atom/cycles/\ncpu_atom/instructions/\ncpu_atom/branch-misses/\n"
            ),
            [
                "{cpu_core/cycles/,cpu_core/instructions/}",
                "{cpu_atom/cycles/,cpu_atom/instructions/,cpu_atom/branch-misses/}"
            ]
        );
    }

    #[test]
    fn bounded_reader_rejects_oversized_analysis_evidence() {
        let root = std::env::temp_dir().join(format!(
            "asb-agentsysperf-bounded-reader-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("oversized.json");
        fs::write(&path, vec![b'x'; MAX_ANALYSIS_BYTES + 1]).unwrap();
        assert!(read_bounded(&path).is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn analysis_uses_project_local_tool_and_session_local_home() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("asb-agentsysperf-analysis-{}", std::process::id()));
        let tools = root.join("tools");
        let bin = tools.join("agentsysperf/bin");
        fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("agentsysperf");
        fs::write(
            &executable,
            "#!/bin/sh\ntest \"$AGENTSYSPERF_HOME\" = \"$PWD/agentsysperf-home\" || exit 7\nprintf 'Analyzing 1 record...\\n\\n[{\"analyzer\":\"fixture\"}]'\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let session = Session::start_with_collectors(&root, &[], Some(&tools)).unwrap();
        let output = analyze(&session, &root.join("records"));
        assert_eq!(output["status"], "unqualified");
        assert_eq!(output["execution_status"], "completed");
        assert_eq!(output["interpretation_status"], "unqualified");
        assert_eq!(output["findings"][0]["analyzer"], "fixture");
        assert!(root.join("agentsysperf-analysis.stdout.log").is_file());
        assert!(root.join("agentsysperf-analysis.stderr.log").is_file());
        drop(session);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn analysis_parser_accepts_only_a_complete_json_array_after_preamble() {
        assert_eq!(
            analysis_findings(b"Analyzing 1 record...\n\n[{\"analyzer\":\"fixture\"}]").unwrap()[0]
                ["analyzer"],
            "fixture"
        );
        assert!(analysis_findings(b"preamble {\"not\":\"an array\"}").is_err());
        assert!(analysis_findings(b"[{}] trailing").is_err());
    }

    #[test]
    fn bottleneck_graph_uses_only_complete_observed_findings() {
        let metrics = std::collections::BTreeMap::new();
        let documents = std::collections::BTreeMap::from([(
            "agentsysperf-analysis".into(),
            json!({"status":"observed","interpretation_status":"qualified","qualified_findings":[
                {"analyzer":"breakdown","verdict":"orchestration_heavy","confidence":0.75},
                {"analyzer":"bad","verdict":"ignored","confidence":2.0}
            ]}),
        )]);
        let graph = bottleneck_graph(&[
            ReportRow {
                label: "fixture-agent",
                agent: "fixture-agent",
                provider: "fixture-provider",
                model: "fixture-model",
                workload: "fixture-workload",
                trial: "fixture-trial",
                configured_concurrency: 1,
                replica_index: None,
                execution_cohort: None,
                metrics: &metrics,
                components: None,
                documents: &documents,
            },
            ReportRow {
                label: "unmeasured-agent",
                agent: "unmeasured-agent",
                provider: "fixture-provider",
                model: "fixture-model",
                workload: "fixture-workload",
                trial: "fixture-trial",
                configured_concurrency: 1,
                replica_index: None,
                execution_cohort: None,
                metrics: &metrics,
                components: None,
                documents: &std::collections::BTreeMap::new(),
            },
        ])
        .expect("one validated finding");
        assert_eq!(graph.filename, "agentsysperf-bottleneck-findings.svg");
        assert!(graph.svg.contains("orchestration_heavy"));
        assert!(!graph.svg.contains("ignored"));
    }

    #[test]
    fn legacy_and_unqualified_external_diagnostics_do_not_become_confidence_graphs() {
        let metrics = BTreeMap::new();
        for mut analysis in [
            json!({"status":"observed","findings":[{"analyzer":"breakdown","verdict":"orchestration_heavy","confidence":0.75}]}),
            interpretation::document(
                json!([{"analyzer":"memory_bandwidth","verdict":"memory_bottleneck_moderate","confidence":0.6}]),
            ),
        ] {
            // Even a diagnostic accidentally copied into this field cannot
            // bypass the explicit interpretation qualification boundary.
            analysis["qualified_findings"] = analysis["findings"].clone();
            let documents = BTreeMap::from([("agentsysperf-analysis".into(), analysis)]);
            assert!(
                bottleneck_graph(&[ReportRow {
                    label: "fixture",
                    agent: "fixture",
                    provider: "fixture",
                    model: "fixture",
                    workload: "fixture",
                    trial: "fixture",
                    configured_concurrency: 1,
                    replica_index: None,
                    execution_cohort: None,
                    metrics: &metrics,
                    components: None,
                    documents: &documents,
                }])
                .is_none()
            );
        }
        assert!(
            ADAPTER
                .display_artifact("agentsysperf-analysis.json")
                .is_some()
        );
        assert!(ADAPTER.display_artifact("other.json").is_none());
    }

    #[test]
    fn scaling_graph_requires_three_complete_compatible_cohorts() {
        let mut metrics = Vec::new();
        let mut documents = Vec::new();
        let mut cohorts = Vec::new();
        let mut labels = Vec::new();
        for concurrency in 1..=3 {
            for replica in 1..=concurrency {
                metrics.push(std::collections::BTreeMap::from([
                    ("elapsed_ms".into(), (concurrency * 100 + replica) as f64),
                    ("success_rate".into(), 1.0),
                ]));
                documents.push(std::collections::BTreeMap::from([(
                    "agentsysperf".into(),
                    json!({"status":"observed","scaling_requested":true}),
                )]));
                cohorts.push(crate::measurements::ExecutionCohort {
                    id: format!("cohort-{concurrency}"),
                    selected_replicas: concurrency,
                    admitted_slots: (1..=concurrency).collect(),
                    dispatch: None,
                });
                labels.push(format!("run-{concurrency}-{replica}"));
            }
        }
        let rows = (0..metrics.len())
            .map(|index| ReportRow {
                label: &labels[index],
                agent: "fixture-agent",
                provider: "fixture-provider",
                model: "fixture-model",
                workload: "fixture-workload",
                trial: "fixture-trial",
                configured_concurrency: cohorts[index].selected_replicas,
                replica_index: None,
                execution_cohort: Some(&cohorts[index]),
                metrics: &metrics[index],
                components: None,
                documents: &documents[index],
            })
            .collect::<Vec<_>>();
        let graphs = scaling_graphs(&rows);
        assert_eq!(graphs.len(), 1);
        assert!(graphs[0].filename.starts_with("agentsysperf-scaling-"));
        assert!(graphs[0].svg.contains("configured concurrency"));
        assert!(graphs[0].svg.contains("summed end-to-end elapsed time"));

        // A partial cohort cannot become a fabricated scaling point.
        assert!(scaling_graphs(&rows[..rows.len() - 1]).is_empty());
    }
}
