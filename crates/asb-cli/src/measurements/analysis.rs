//! Offline, deterministic kernel/resource diagnostics over typed observations.
//!
//! Rules describe observations, not causal proof. Collector adapters retain
//! ownership of measurement semantics; the engine never inspects the host.

mod observations;
mod rules;
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
    }
}

pub(super) fn publish(dir: &Path, timing: Option<&super::ComponentTiming>) -> Result<(), String> {
    super::write_json(&dir.join("bottlenecks.json"), &analyze(timing))
}
