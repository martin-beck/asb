//! Resource pressure reports symptoms over exclusive agent cgroups, not causes.
use super::*;

pub(super) struct Pressure {
    rule: &'static str,
    metric: &'static str,
    counter: &'static str,
    unit: &'static str,
    limitations: &'static str,
}
impl Rule for Pressure {
    fn evaluate(&self, docs: &Documents) -> Diagnostic {
        let value = docs
            .get("kernel-pressure-summary")
            .filter(|d| {
                d.get("schema_version").and_then(Value::as_u64) == Some(1)
                    && d.get("scope").and_then(Value::as_str) == Some("agent-process-tree")
                    && d.get("scope_source").and_then(Value::as_str)
                        == Some("exclusive-owned-cgroup")
                    && d.get("duration_us")
                        .and_then(Value::as_u64)
                        .is_some_and(|v| v > 0)
                    && matches!(
                        d.get("status").and_then(Value::as_str),
                        Some("completed" | "partial")
                    )
                    && d.get("unavailable")
                        .and_then(Value::as_object)
                        .is_some_and(|u| !u.contains_key(self.counter))
            })
            .and_then(|d| d.get("observations"))
            .and_then(|m| m.get(self.metric))
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 0.0 && (self.unit != "%" || *v <= 100.0));
        Diagnostic {
            rule: self.rule, rule_version:1,
            assessment: match value { Some(v) if v > 0.0 => "observed", Some(_) => "not_observed", None => "insufficient_evidence" },
            value, unit: self.unit,
            unavailable_reason: value.is_none().then_some("requires a valid exclusive agent-cgroup execution interval and the specific observed pressure counter"),
            evidence: vec!["kernel-pressure-summary.json", self.metric],
            limitations: self.limitations,
        }
    }
}
pub(super) static CPU: Pressure = Pressure {
    rule: "pressure.cpu_some_pct",
    metric: "cpu.some_stall_pct",
    counter: "cpu.some_total_us",
    unit: "%",
    limitations: "Cgroup wall-time share with at least one CPU-stalled task. It is not runnable-thread time, proof of a kernel bottleneck, or the identity of a competing process. Resource stalls overlap.",
};
pub(super) static MEMORY: Pressure = Pressure {
    rule: "pressure.memory_some_pct",
    metric: "memory.some_stall_pct",
    counter: "memory.some_total_us",
    unit: "%",
    limitations: "Cgroup wall-time share with at least one task stalled on memory. Reclaim, limits and shared host contention need further evidence; not DRAM latency or additive wall-time loss.",
};
pub(super) static IO: Pressure = Pressure {
    rule: "pressure.io_some_pct",
    metric: "io.some_stall_pct",
    counter: "io.some_total_us",
    unit: "%",
    limitations: "Cgroup wall-time share with at least one task stalled on I/O. It does not distinguish device service from queueing, filesystem contention or other I/O causes; not network wait.",
};
pub(super) static THROTTLE: Pressure = Pressure {
    rule: "pressure.cpu_throttled_ms",
    metric: "cpu.throttled_ms",
    counter: "cpu.throttled_total_us",
    unit: "ms",
    limitations: "Observed CPU-bandwidth throttling duration can accumulate across CPUs and exceed elapsed wall time. It is separate from scheduler contention and cannot be subtracted from wall time.",
};
pub(super) static OOM: Pressure = Pressure {
    rule: "pressure.oom_kill_count",
    metric: "memory.oom_kill_count",
    counter: "memory.oom_kill_count",
    unit: "count",
    limitations: "Observed cgroup OOM kills during execution. This does not identify a particular killed task or prove that its benchmark failure was caused by OOM.",
};
pub(super) static MEMORY_HIGH: Pressure = Pressure {
    rule: "pressure.memory_high_count",
    metric: "memory.high_count",
    counter: "memory.high_count",
    unit: "count",
    limitations: "Observed memory.high events in the exclusive cgroup. These are limit-pressure events, not elapsed reclaim time, memory allocation counts or proof of hardware bandwidth saturation.",
};

#[cfg(test)]
mod tests {
    use super::*;
    fn docs() -> Documents {
        Documents::from([(
            "kernel-pressure-summary".into(),
            serde_json::json!({
                "schema_version":1,"scope":"agent-process-tree","scope_source":"exclusive-owned-cgroup",
                "status":"completed","duration_us":1000,"unavailable":{},
                "observations":{"cpu.some_stall_pct":5,"memory.some_stall_pct":0,"cpu.throttled_ms":3,"memory.oom_kill_count":2}
            }),
        )])
    }
    #[test]
    fn pressure_rules_preserve_missing_zero_and_accumulated_throttle_semantics() {
        assert_eq!(CPU.evaluate(&docs()).value, Some(5.0));
        assert_eq!(MEMORY.evaluate(&docs()).assessment, "not_observed");
        assert_eq!(IO.evaluate(&docs()).assessment, "insufficient_evidence");
        assert_eq!(THROTTLE.evaluate(&docs()).value, Some(3.0));
        assert_eq!(OOM.evaluate(&docs()).value, Some(2.0));
        assert!(
            THROTTLE
                .evaluate(&docs())
                .limitations
                .contains("exceed elapsed")
        );
    }
    #[test]
    fn host_container_incomplete_or_invalid_counters_cannot_produce_findings() {
        for (key, value) in [
            ("scope", Value::from("host-context")),
            ("scope", Value::from("container")),
            ("scope_source", Value::from("container-cgroup")),
            ("duration_us", Value::from(0)),
            ("status", Value::from("unavailable")),
            ("schema_version", Value::from(2)),
        ] {
            let mut docs = docs();
            docs.get_mut("kernel-pressure-summary").unwrap()[key] = value;
            assert!(CPU.evaluate(&docs).value.is_none(), "{key}");
        }
        let mut lost = docs();
        lost.get_mut("kernel-pressure-summary").unwrap()["unavailable"]["cpu.some_total_us"] =
            Value::from("counter reset");
        assert!(CPU.evaluate(&lost).value.is_none());
        let mut invalid = docs();
        invalid.get_mut("kernel-pressure-summary").unwrap()["observations"]["cpu.some_stall_pct"] =
            Value::from(101);
        assert!(CPU.evaluate(&invalid).value.is_none());
    }
}
