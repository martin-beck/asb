use super::super::ComponentTiming;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Scope {
    Execution,
    AgentProcessTree,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Kind {
    ElapsedWallTime,
    AccumulatedThreadTime,
}

#[derive(Debug, Serialize)]
pub(super) struct Observation {
    pub id: &'static str,
    pub unit: &'static str,
    pub kind: Kind,
    pub scope: Scope,
    pub value: Option<f64>,
    pub availability: &'static str,
    pub source_artifact: &'static str,
    pub source_field: &'static str,
    pub accounting_source: Option<String>,
}

pub(super) fn component_observations(timing: Option<&ComponentTiming>) -> Vec<Observation> {
    let cpu_scope = match timing.and_then(|t| t.cpu_accounting_source.as_deref()) {
        Some("cgroup-v2" | "proc-sampling") => Scope::AgentProcessTree,
        _ => Scope::Unknown,
    };
    [
        (
            "execution.wall_ms",
            "wall_ms",
            Kind::ElapsedWallTime,
            Scope::Execution,
            timing.map(|t| t.wall_ms as f64),
        ),
        (
            "cpu.user_ms",
            "cpu_user_ms",
            Kind::AccumulatedThreadTime,
            cpu_scope,
            timing.and_then(|t| t.cpu_user_ms),
        ),
        (
            "cpu.system_ms",
            "cpu_system_ms",
            Kind::AccumulatedThreadTime,
            cpu_scope,
            timing.and_then(|t| t.cpu_system_ms),
        ),
        (
            "scheduler.wait_ms",
            "scheduler_wait_ms",
            Kind::AccumulatedThreadTime,
            Scope::AgentProcessTree,
            timing.and_then(|t| t.scheduler_wait_ms),
        ),
        (
            "io.wait_ms",
            "io_wait_ms",
            Kind::AccumulatedThreadTime,
            Scope::AgentProcessTree,
            timing.and_then(|t| t.io_wait_ms),
        ),
    ]
    .into_iter()
    .map(|(id, field, kind, scope, value)| {
        let value = value.filter(|v| v.is_finite() && *v >= 0.0);
        Observation {
            id,
            unit: "ms",
            kind,
            scope,
            value,
            availability: if value.is_some() {
                "observed"
            } else {
                "unavailable"
            },
            source_artifact: "components.json",
            source_field: field,
            accounting_source: timing.and_then(|t| t.cpu_accounting_source.clone()),
        }
    })
    .collect()
}
