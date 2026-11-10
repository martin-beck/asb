//! Offline, deterministic kernel/resource diagnostics over typed observations.
//!
//! Rules describe observations, not causal proof. Collector adapters retain
//! ownership of measurement semantics; the engine never inspects the host.

pub(crate) mod cohort;
mod collector_overhead;
mod counter_groups;
mod hardware;
mod kernel;
mod observations;
mod rules;
mod sampling;
#[cfg(test)]
mod tests;

use observations::{Observation, component_observations};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Assessment {
    Observed,
    NotObserved,
    InsufficientEvidence,
}

#[derive(Debug, Serialize)]
struct Finding {
    rule: &'static str,
    rule_version: u32,
    assessment: Assessment,
    summary: &'static str,
    evidence: Vec<&'static str>,
    limitations: &'static str,
}

/// A new rule registers once, declares its input semantics and returns an
/// assessment even when observations are missing. Absence is not a negative.
trait AnalysisRule: Sync {
    fn evaluate(&self, observations: &[Observation]) -> Finding;
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    engine: &'static str,
    observations: Vec<Observation>,
    findings: Vec<Finding>,
    hardware: Vec<hardware::PmuAnalysis>,
    kernel: Vec<kernel::Diagnostic>,
    sampling: Vec<sampling::SamplingAnalysis>,
    collector_overhead: Vec<collector_overhead::CollectorOverhead>,
    counter_groups: Vec<counter_groups::Group>,
    phase_alignments: Vec<super::phase_alignment::Alignment>,
}

fn analyze(timing: Option<&super::ComponentTiming>) -> Report {
    let observations = component_observations(timing);
    let findings = rules::REGISTRY
        .iter()
        .map(|rule| rule.evaluate(&observations))
        .collect();
    Report {
        schema_version: 1,
        engine: "asb-resource-analysis-v1",
        observations,
        findings,
        hardware: Vec::new(),
        kernel: Vec::new(),
        sampling: Vec::new(),
        collector_overhead: Vec::new(),
        counter_groups: Vec::new(),
        phase_alignments: Vec::new(),
    }
}

pub(super) fn publish(
    dir: &Path,
    timing: Option<&super::ComponentTiming>,
) -> Result<serde_json::Value, String> {
    let mut documents = std::collections::BTreeMap::new();
    for name in [
        "execution-window",
        "perf-stat-plan",
        "perf-stat-quality",
        "perf-stat",
        "measurement-target",
        "arm-platform",
        "arm-pmu",
        "arm-pmu-plan",
        "perfetto",
        "perfetto-tracepoints",
        "perfetto-state-summary",
        "bpftrace-futex-wait",
        "bpftrace-lock-contention",
        "kernel-pressure-summary",
        "arm-spe",
        "arm-spe-plan",
        "arm-spe-quality",
    ] {
        use std::io::Read;
        let mut bytes = Vec::new();
        let value = std::fs::File::open(dir.join(format!("{name}.json")))
            .ok()
            .and_then(|file| file.take(1024 * 1024 + 1).read_to_end(&mut bytes).ok())
            .filter(|_| bytes.len() <= 1024 * 1024)
            .and_then(|_| serde_json::from_slice(&bytes).ok());
        if let Some(value) = value {
            documents.insert(name.to_owned(), value);
        }
    }
    let mut report = analyze(timing);
    report.hardware = hardware::analyze(&documents);
    report.kernel = kernel::analyze(&documents);
    report.counter_groups = counter_groups::analyze(&documents);
    report.sampling = sampling::analyze(&documents);
    report.collector_overhead = collector_overhead::analyze(&documents);
    report.phase_alignments = sampling::phase_alignments(&documents);
    let value = serde_json::to_value(report).map_err(|error| error.to_string())?;
    super::write_json(&dir.join("bottlenecks.json"), &value)?;
    Ok(value)
}

/// Recompute from the comparison loader's bounded, identity-bound documents.
/// Historical analysis output is never trusted as an input to new findings.
pub(crate) fn from_documents(
    documents: &std::collections::BTreeMap<String, serde_json::Value>,
) -> serde_json::Value {
    let component = documents
        .get("components")
        .and_then(|d| d.pointer("/components/agent.execute"));
    let timing = component.and_then(|c| {
        Some(super::ComponentTiming {
            wall_ms: c.get("wall_ms")?.as_u64()? as u128,
            cpu_user_ms: c.get("cpu_user_ms").and_then(serde_json::Value::as_f64),
            cpu_system_ms: c.get("cpu_system_ms").and_then(serde_json::Value::as_f64),
            cpu_accounting_source: c
                .get("cpu_accounting_source")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            io_wait_ms: c.get("io_wait_ms").and_then(serde_json::Value::as_f64),
            scheduler_wait_ms: c
                .get("scheduler_wait_ms")
                .and_then(serde_json::Value::as_f64),
            network_wait_ms: None,
        })
    });
    let mut report = analyze(timing.as_ref());
    report.hardware = hardware::analyze(documents);
    report.kernel = kernel::analyze(documents);
    report.counter_groups = counter_groups::analyze(documents);
    report.sampling = sampling::analyze(documents);
    report.collector_overhead = collector_overhead::analyze(documents);
    report.phase_alignments = sampling::phase_alignments(documents);
    serde_json::to_value(report).expect("analysis contains only finite values")
}
