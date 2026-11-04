//! Export ASB-owned, agent-scoped observations in AgentSysPerf's record shape.
//!
//! AgentSysPerf's own runner is deliberately not invoked for an ASB benchmark:
//! it would own a second workload/agent lifecycle and its legacy `perf -a`
//! default could observe unrelated host work.  This adapter instead exports
//! already verified ASB process-tree observations for later offline analysis.

use super::super::{CollectorAdapter, ReportGraph, ReportRow};
use crate::measurements::{self, ComponentTiming, Session};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_ANALYSIS_BYTES: usize = 1024 * 1024;
const ANALYSIS_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) struct AgentSysPerfCollector;
pub(super) static ADAPTER: AgentSysPerfCollector = AgentSysPerfCollector;

const DEPENDENCIES: &[&str] = &["agent-memory", "agent-workers", "cpu-accounting", "perf"];

#[derive(Default)]
struct State {
    enabled: bool,
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
    fn description(&self) -> &'static str {
        "AgentSysPerf-compatible agent-scoped measurement records"
    }

    fn id(&self) -> &'static str {
        "agentsysperf:l1-resource"
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

    fn report_graphs(&self, rows: &[ReportRow<'_>]) -> Vec<ReportGraph> {
        bottleneck_graph(rows).into_iter().collect()
    }

    fn configure(&self, session: &mut Session, selected: &[String]) -> Result<(), String> {
        let enabled = crate::collectors::selected_or_environment(self.id(), selected);
        let state = session.state::<State>();
        let mut state = state.borrow_mut();
        state.enabled = enabled;
        state.platform = enabled.then(discover_platform);
        Ok(())
    }

    fn finish(&self, session: &mut Session) -> Result<(), String> {
        if !session.state::<State>().borrow().enabled {
            return Ok(());
        }

        let Some(timing) = session.component_timing("agent.execute") else {
            return write_unavailable(session, "ASB did not observe an agent execution interval.");
        };
        let span_id = format!("asb::{}::agent-execute", session.trace_id());
        let records = match records(session, &span_id, timing) {
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
                "records": records.len(),
                "platform": session.state::<State>().borrow().platform,
                "analysis": analysis["status"],
                "limitations": "This is an ASB-owned process-tree export for offline AgentSysPerf analysis. It does not run AgentSysPerf's benchmark harness or accept system-wide perf attribution. Missing observations are omitted rather than represented as zero."
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
    let stdout_path = session.dir.join("agentsysperf-analysis.stdout.json");
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
        Ok(findings) if findings.is_array() => {
            json!({"schema_version": 1, "status": "observed", "findings": findings})
        }
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
        if analysis.get("status").and_then(Value::as_str) != Some("observed") {
            continue;
        }
        let Some(entries) = analysis.get("findings").and_then(Value::as_array) else {
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

fn discover_platform() -> PlatformManifest {
    let architecture = match std::env::consts::ARCH {
        "aarch64" => "armv8",
        "x86_64" => "x86_64",
        other => other,
    };
    let cpuinfo = fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let family = if architecture == "armv8" {
        "armv8"
    } else if cpuinfo.contains("authenticamd") || cpuinfo.contains("amd") {
        "amd"
    } else if cpuinfo.contains("genuineintel") || cpuinfo.contains("intel") {
        "intel"
    } else {
        "unknown"
    };
    let listing = Command::new("perf")
        .args(["list", "--no-desc"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let mut wanted = vec!["cycles", "instructions", "cache-misses", "branch-misses"];
    match family {
        "intel" => wanted.extend(["topdown-fe-bound", "topdown-be-bound"]),
        // AMD gets the portable hardware counters above unless a future,
        // separately qualified AMD PMU catalog is available.
        "armv8" => wanted.extend(["l1d_cache_refill", "stall_backend", "stall_frontend"]),
        _ => {}
    }
    let perf_events = wanted
        .into_iter()
        .filter(|event| perf_list_advertises(&listing, event))
        .map(str::to_owned)
        .collect();
    PlatformManifest {
        family,
        architecture,
        perf_events,
    }
}

fn perf_list_advertises(listing: &str, event: &str) -> bool {
    listing.lines().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|first| first == event)
    })
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
) -> Result<Vec<MeasurementRecord>, String> {
    let mut l1 = Map::new();
    l1.insert("kind".into(), json!("agent_execute"));
    l1.insert("node_id".into(), json!("asb-agent"));
    l1.insert(
        "duration_us".into(),
        json!(timing.wall_ms.saturating_mul(1_000)),
    );
    if let Some(value) = timing.cpu_user_ms {
        // `cpu_time_s` is CPU time, not just time spent in userspace.  The
        // accounting collector has already validated both optional counters.
        l1.insert(
            "cpu_time_s".into(),
            json!((value + timing.cpu_system_ms.unwrap_or_default()) / 1_000.0),
        );
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
    let perf = read_metrics(&session.dir.join("perf-stat.json"))?;
    if let Some(l3) = l3_record(&perf) {
        result.push(MeasurementRecord {
            span_id: span_id.into(),
            layer: "l3",
            payload: l3,
        });
    }
    Ok(result)
}

fn l3_record(perf: &Map<String, Value>) -> Option<Map<String, Value>> {
    let mut l3 = Map::new();
    for (from, to) in [
        ("perf.observed_cycles_count", "cycles"),
        ("perf.observed_instructions_count", "instructions"),
        ("perf.cache_misses_count", "cache_misses"),
        ("perf.branch_misses_count", "branch_misses"),
        ("perf.observed_l1d_cache_refill_count", "l1d_cache_refill"),
        ("perf.observed_stall_backend_count", "stall_backend"),
        ("perf.observed_stall_frontend_count", "stall_frontend"),
    ] {
        if let Some(value) = metric(perf, from) {
            l3.insert(to.into(), json!(value));
        }
    }
    if let (Some(instructions), Some(cycles)) = (
        l3.get("instructions").and_then(Value::as_f64),
        l3.get("cycles").and_then(Value::as_f64),
    ) && cycles > 0.0
    {
        l3.insert("ipc".into(), json!(instructions / cycles));
    }
    (!l3.is_empty()).then_some(l3)
}

fn read_metrics(path: &std::path::Path) -> Result<Map<String, Value>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => {
            return Err(format!(
                "cannot read collector evidence at {}: {error}",
                path.display()
            ));
        }
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| format!("invalid collector evidence at {}", path.display()))?;
    Ok(value
        .get("metrics")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default())
}

fn read_observed_metrics(
    path: &std::path::Path,
    required_scope: &str,
) -> Result<Map<String, Value>, String> {
    let bytes = fs::read(path).map_err(|error| {
        format!(
            "cannot read required collector evidence at {}: {error}",
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
    fn mapped_l3_records_calculate_ipc_only_from_observed_counts() {
        let mut metrics = Map::new();
        metrics.insert("perf.observed_cycles_count".into(), json!(20.0));
        metrics.insert("perf.observed_instructions_count".into(), json!(30.0));
        let record = l3_record(&metrics).expect("observed hardware counters");
        assert_eq!(record["cycles"], 20.0);
        assert_eq!(record["instructions"], 30.0);
        assert_eq!(record["ipc"], 1.5);
        assert!(!record.contains_key("cache_misses"));
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
        assert_eq!(output["status"], "observed");
        assert_eq!(output["findings"][0]["analyzer"], "fixture");
        assert!(root.join("agentsysperf-analysis.stdout.json").is_file());
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
            json!({"status":"observed","findings":[
                {"analyzer":"breakdown","verdict":"orchestration_heavy","confidence":0.75},
                {"analyzer":"bad","verdict":"ignored","confidence":2.0}
            ]}),
        )]);
        let graph = bottleneck_graph(&[
            ReportRow {
                label: "fixture-agent",
                metrics: &metrics,
                components: None,
                documents: &documents,
            },
            ReportRow {
                label: "unmeasured-agent",
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
}
