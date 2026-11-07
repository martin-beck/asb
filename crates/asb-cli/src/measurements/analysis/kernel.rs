//! Kernel diagnostics use source-specific semantics and require capture validity.
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
mod pressure;

#[derive(Serialize)]
pub(super) struct Diagnostic {
    rule: &'static str,
    rule_version: u32,
    assessment: &'static str,
    value: Option<f64>,
    unit: &'static str,
    unavailable_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'static str>,
    evidence: Vec<&'static str>,
    limitations: &'static str,
}

type Documents = BTreeMap<String, Value>;
trait Rule: Sync {
    fn evaluate(&self, documents: &Documents) -> Diagnostic;
}
struct RunnablePressure;
impl Rule for RunnablePressure {
    fn evaluate(&self, docs: &Documents) -> Diagnostic {
        let calculate = || -> Option<f64> {
            let status = docs.get("perfetto")?.get("status")?.as_str()?;
            let trace = docs.get("perfetto-tracepoints")?;
            if status != "completed"
                || trace.get("scope_verified")?.as_bool() != Some(true)
                || trace.get("capture_complete")?.as_bool() != Some(true)
            {
                return None;
            }
            let summary = docs.get("perfetto-state-summary")?;
            if summary.get("schema_version")?.as_u64()? != 2
                || summary.get("observed_intervals")?.as_u64()? == 0
                || summary.get("unfinished_intervals_excluded")?.as_u64()? != 0
                || summary.get("execution_end_ns")?.as_u64()?
                    <= summary.get("execution_start_ns")?.as_u64()?
            {
                return None;
            }
            let metrics = summary.get("metrics")?.as_object()?;
            // A complete closed-set state aggregate omits states with no rows.
            let state = |key| -> Option<f64> {
                match metrics.get(key) {
                    None => Some(0.0),
                    Some(v) => v.as_f64().filter(|v| v.is_finite() && *v >= 0.0),
                }
            };
            let running = state("running_ms")?;
            let waiting = state("runnable_ms")?;
            let total = running + waiting;
            (total > 0.0 && total.is_finite()).then_some(waiting / total * 100.0)
        };
        let value = calculate();
        Diagnostic {
            rule: "scheduler.runnable_wait_share_pct",
            rule_version: 1,
            assessment: match value {
                Some(v) if v > 0.0 => "observed",
                Some(_) => "not_observed",
                None => "insufficient_evidence",
            },
            value,
            unit: "%",
            unavailable_reason: value.is_none().then_some(
                "requires complete loss-free scoped scheduler states with execution bounds",
            ),
            scope: None,
            evidence: vec![
                "perfetto.json",
                "perfetto-tracepoints.json",
                "perfetto-state-summary.json",
            ],
            limitations: "Runnable wait / (running + runnable wait) is an observed thread-time share, not elapsed-wall loss. It does not identify competing processes, quota throttling, kernel defects or remote provider latency.",
        }
    }
}

struct BpfTime {
    rule: &'static str,
    artifact: &'static str,
    metric: &'static str,
    limitations: &'static str,
}
impl Rule for BpfTime {
    fn evaluate(&self, docs: &Documents) -> Diagnostic {
        let value = docs
            .get(self.artifact)
            .filter(|d| d.get("status").and_then(Value::as_str) == Some("completed"))
            .and_then(|d| d.get("metrics"))
            .and_then(|m| m.get(self.metric))
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| v / 1_000_000.0);
        Diagnostic {
            rule: self.rule,
            rule_version: 1,
            assessment: match value {
                Some(v) if v > 0.0 => "observed",
                Some(_) => "not_observed",
                None => "insufficient_evidence",
            },
            value,
            unit: "ms",
            unavailable_reason: value
                .is_none()
                .then_some("requires completed scoped BPF collector and numeric observation"),
            scope: None,
            evidence: vec![self.artifact, self.metric],
            limitations: self.limitations,
        }
    }
}
static RUNNABLE: RunnablePressure = RunnablePressure;
static LOCKS: BpfTime = BpfTime {
    rule: "kernel.lock_contention_ms",
    artifact: "bpftrace-lock-contention",
    metric: "bpftrace.lock-contention.lock_contention_total_ns",
    limitations: "Sum of completed observed kernel contention intervals over the collector lifetime. Concurrent waits overlap; missing boundary pairs and startup/teardown coverage limit completeness. Not elapsed-wall delay or a lock-owner diagnosis.",
};
static FUTEX: BpfTime = BpfTime {
    rule: "synchronization.futex_syscall_ms",
    artifact: "bpftrace-futex-wait",
    metric: "bpftrace.futex-wait.futex_wait_total_ns",
    limitations: "The current futex profile measures all observed futex syscall durations, including wake operations and descheduling. This is not exact blocked-lock time or evidence that every futex call waited.",
};
static RULES: &[&dyn Rule] = &[
    &RUNNABLE,
    &LOCKS,
    &FUTEX,
    &pressure::CPU,
    &pressure::MEMORY,
    &pressure::IO,
    &pressure::THROTTLE,
    &pressure::OOM,
    &pressure::MEMORY_HIGH,
];
pub(super) fn analyze(documents: &Documents) -> Vec<Diagnostic> {
    RULES.iter().map(|rule| rule.evaluate(documents)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scheduler_ratio_requires_scope_capture_and_complete_interval_evidence() {
        let docs = Documents::from([
            ("perfetto".into(), serde_json::json!({"status":"completed"})),
            (
                "perfetto-tracepoints".into(),
                serde_json::json!({"scope_verified":true,"capture_complete":true}),
            ),
            (
                "perfetto-state-summary".into(),
                serde_json::json!({"schema_version":2,"observed_intervals":2,"unfinished_intervals_excluded":0,"execution_start_ns":10,"execution_end_ns":100,"metrics":{"running_ms":30,"runnable_ms":10}}),
            ),
        ]);
        assert_eq!(analyze(&docs)[0].value, Some(25.0));
        for (source, key, value) in [
            ("perfetto", "status", Value::from("failed")),
            ("perfetto-tracepoints", "scope_verified", Value::from(false)),
            (
                "perfetto-tracepoints",
                "capture_complete",
                Value::from(false),
            ),
            (
                "perfetto-state-summary",
                "unfinished_intervals_excluded",
                Value::from(1),
            ),
        ] {
            let mut broken = docs.clone();
            broken.get_mut(source).unwrap()[key] = value;
            assert_eq!(analyze(&broken)[0].assessment, "insufficient_evidence");
        }
    }
    #[test]
    fn missing_and_failed_bpf_are_unknown_and_futex_labels_do_not_claim_blocked_time() {
        let docs = Documents::from([(
            "bpftrace-futex-wait".into(),
            serde_json::json!({"status":"completed","metrics":{"bpftrace.futex-wait.futex_wait_total_ns":2_000_000}}),
        )]);
        let report = analyze(&docs);
        assert!(report[1].value.is_none());
        assert_eq!(report[2].value, Some(2.0));
        assert!(report[2].limitations.contains("wake operations"));
        let mut failed = docs;
        failed.get_mut("bpftrace-futex-wait").unwrap()["status"] = Value::from("failed");
        assert!(analyze(&failed)[2].value.is_none());
    }
}
