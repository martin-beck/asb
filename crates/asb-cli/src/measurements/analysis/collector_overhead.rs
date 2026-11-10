//! Validated collector self-observation, kept separate from workload findings.
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Serialize)]
pub(super) struct CollectorOverhead {
    collector: &'static str,
    status: String,
    scope: String,
    scope_source: String,
    sample_count: u64,
    sample_read_us_total: u64,
    sample_read_us_max: u64,
    interval_jitter_us_max: u64,
    schedule_lateness_us_max: u64,
    missed_scheduled_intervals: u64,
    configured_interval_us: u64,
    limitations: &'static str,
}

/// Report only a complete, typed overhead record.  A collector's own sampling
/// cost is methodology evidence, not a workload bottleneck or an inferred
/// attribution of the measured pressure.
pub(super) fn analyze(documents: &BTreeMap<String, Value>) -> Vec<CollectorOverhead> {
    let Some(summary) = documents.get("kernel-pressure-summary") else {
        return Vec::new();
    };
    if summary.get("schema_version").and_then(Value::as_u64) != Some(1)
        || !matches!(
            summary.get("status").and_then(Value::as_str),
            Some("completed" | "partial")
        )
    {
        return Vec::new();
    }
    let Some(scope) = summary.get("scope").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(scope_source) = summary.get("scope_source").and_then(Value::as_str) else {
        return Vec::new();
    };
    if !matches!(
        (scope, scope_source),
        ("agent-process-tree", "exclusive-owned-cgroup")
            | (
                "owned-container-execution",
                "verified-owned-container-cgroup"
            )
    ) {
        return Vec::new();
    }
    let Some(overhead) = summary.get("collector_overhead") else {
        return Vec::new();
    };
    let value = |name| overhead.get(name).and_then(Value::as_u64);
    let (
        Some(sample_count),
        Some(sample_read_us_total),
        Some(sample_read_us_max),
        Some(interval_jitter_us_max),
        Some(schedule_lateness_us_max),
        Some(missed_scheduled_intervals),
        Some(configured_interval_us),
    ) = (
        value("sample_count"),
        value("sample_read_us_total"),
        value("sample_read_us_max"),
        value("interval_jitter_us_max"),
        value("schedule_lateness_us_max"),
        value("missed_scheduled_intervals"),
        value("configured_interval_us"),
    )
    else {
        return Vec::new();
    };
    if sample_count == 0 || configured_interval_us == 0 || sample_read_us_max > sample_read_us_total
    {
        return Vec::new();
    }
    vec![CollectorOverhead {
        collector: "kernel-pressure",
        status: summary["status"].as_str().unwrap_or_default().into(),
        scope: scope.into(),
        scope_source: scope_source.into(),
        sample_count,
        sample_read_us_total,
        sample_read_us_max,
        interval_jitter_us_max,
        schedule_lateness_us_max,
        missed_scheduled_intervals,
        configured_interval_us,
        limitations: "Collector self-observation reports sampler timing only. It does not measure total ASB overhead, prove zero perturbation, or make pressure values agent-only when the retained scope is owned-container execution.",
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn documents() -> BTreeMap<String, Value> {
        [("kernel-pressure-summary".into(), serde_json::json!({
            "schema_version":1,"status":"completed",
            "scope":"owned-container-execution","scope_source":"verified-owned-container-cgroup",
            "collector_overhead":{"sample_count":18,"sample_read_us_total":22746,"sample_read_us_max":1378,"interval_jitter_us_max":41,"schedule_lateness_us_max":146,"missed_scheduled_intervals":0,"configured_interval_us":1000000}
        }))].into()
    }

    #[test]
    fn preserves_valid_scoped_sampler_observations() {
        let value = serde_json::to_value(analyze(&documents())).unwrap();
        assert_eq!(value[0]["collector"], "kernel-pressure");
        assert_eq!(value[0]["sample_read_us_total"], 22746);
        assert_eq!(value[0]["scope"], "owned-container-execution");
    }

    #[test]
    fn rejects_unscoped_or_incoherent_overhead() {
        let mut invalid = documents();
        invalid.get_mut("kernel-pressure-summary").unwrap()["scope"] = Value::from("host-context");
        assert!(analyze(&invalid).is_empty());
        let mut invalid = documents();
        invalid.get_mut("kernel-pressure-summary").unwrap()["collector_overhead"]["sample_read_us_max"] =
            Value::from(22747);
        assert!(analyze(&invalid).is_empty());
        let mut invalid = documents();
        invalid.get_mut("kernel-pressure-summary").unwrap()["collector_overhead"]["sample_count"] =
            Value::from(0);
        assert!(analyze(&invalid).is_empty());
    }
}
