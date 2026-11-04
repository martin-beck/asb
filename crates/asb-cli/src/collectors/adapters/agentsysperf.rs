//! Export ASB-owned, agent-scoped observations in AgentSysPerf's record shape.
//!
//! AgentSysPerf's own runner is deliberately not invoked for an ASB benchmark:
//! it would own a second workload/agent lifecycle and its legacy `perf -a`
//! default could observe unrelated host work.  This adapter instead exports
//! already verified ASB process-tree observations for later offline analysis.

use super::super::CollectorAdapter;
use crate::measurements::{self, ComponentTiming, Session};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;

pub(super) struct AgentSysPerfCollector;
pub(super) static ADAPTER: AgentSysPerfCollector = AgentSysPerfCollector;

const DEPENDENCIES: &[&str] = &["agent-memory", "agent-workers", "cpu-accounting", "perf"];

#[derive(Default)]
struct State {
    enabled: bool,
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

    fn configure(&self, session: &mut Session, selected: &[String]) -> Result<(), String> {
        session.state::<State>().borrow_mut().enabled =
            crate::collectors::selected_or_environment(self.id(), selected);
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
        measurements::write_json(
            &session.dir.join("agentsysperf.json"),
            &json!({
                "schema_version": 1,
                "collector": "agentsysperf:l1-resource",
                "status": "observed",
                "scope": "selected-agent-process-tree",
                "analysis_input": "agentsysperf-records/measurement_records.json",
                "records": records.len(),
                "limitations": "This is an ASB-owned process-tree export for offline AgentSysPerf analysis. It does not run AgentSysPerf's benchmark harness or accept system-wide perf attribution. Missing observations are omitted rather than represented as zero."
            }),
        )
    }
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
}
