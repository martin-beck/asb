//! SPE's sampled classifications stay separate from architectural event counts.
use super::hardware::Metric;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Serialize)]
pub(super) struct SamplingAnalysis {
    pmu: &'static str,
    rule_version: u32,
    scope: &'static str,
    metrics: Vec<Metric>,
    limitations: &'static str,
}

pub(super) fn analyze(documents: &BTreeMap<String, Value>) -> Vec<SamplingAnalysis> {
    if !documents.contains_key("arm-spe-plan") {
        return Vec::new();
    }
    let quality = validated_quality(documents);
    let mut metrics = Vec::new();
    let count = |name| quality?.get("sample_counts")?.get(name)?.as_u64();
    let memory = count("memory").filter(|n| *n > 0);
    for (id, event) in [
        ("sampled_l1d_misses_per_1000_memory_samples", "l1d_miss"),
        ("sampled_tlb_misses_per_1000_memory_samples", "tlb_miss"),
        ("sampled_llc_misses_per_1000_memory_samples", "llc_miss"),
    ] {
        let value = count(event)
            .zip(memory)
            .filter(|(n, d)| n <= d)
            .map(|(n, d)| n as f64 / d as f64 * 1000.0);
        metrics.push(Metric{id,unit:"classifications/1000 sampled memory operations",value,
            unavailable_reason:value.is_none().then_some("requires complete validated SPE decoding with an observed memory population and explicit classification count"),
            evidence:vec!["arm-spe-quality.json".into(),format!("sample_counts.{event}"),"sample_counts.memory".into()]});
    }
    let latency = quality.and_then(|q| {
        let denominator = memory?;
        if q.get("memory_latency_known_samples")?.as_u64()? != denominator {
            return None;
        }
        let weight = q
            .get("memory_latency_weight_sum_cycles")?
            .as_str()?
            .parse::<u128>()
            .ok()?;
        if weight < denominator as u128 || weight > denominator as u128 * u64::MAX as u128 {
            return None;
        }
        let value = weight as f64 / denominator as f64;
        (value.is_finite() && value > 0.0).then_some(value)
    });
    metrics.push(Metric {
        id: "sampled_memory_latency_cycles_mean",
        unit: "cycles",
        value: latency,
        unavailable_reason: latency
            .is_none()
            .then_some("requires a known positive latency weight for every observed memory sample"),
        evidence: vec![
            "arm-spe-quality.json".into(),
            "memory_latency_weight_sum_cycles".into(),
            "memory_latency_known_samples".into(),
        ],
    });
    vec![SamplingAnalysis {
        pmu: "arm_spe",
        rule_version: 1,
        scope: "agent-process-tree",
        metrics,
        limitations: "Classification counts are normalized by decoded memory samples, not cache accesses or retired instructions. SPE filters and sample population are implementation-dependent and sampling bias remains possible. Latency is sampled operation weight in cycles, not isolated DRAM service time or elapsed wall time. Missing classifications/weights remain unavailable. This does not implement a CPU-independent Topdown model or prove a kernel bottleneck.",
    }]
}

fn validated_quality(documents: &BTreeMap<String, Value>) -> Option<&Value> {
    documents.get("arm-spe-quality").filter(|q| {
        q.get("schema_version").and_then(Value::as_u64) == Some(1)
            && q.get("status").and_then(Value::as_str) == Some("observed")
            && q.get("scope").and_then(Value::as_str) == Some("agent-process-tree")
            && q.pointer("/raw_record_quality/aux_records")
                .and_then(Value::as_u64)
                .is_some_and(|n| n > 0)
            && documents
                .get("arm-spe")
                .and_then(|d| d.get("status"))
                .and_then(Value::as_str)
                == Some("completed")
    })
}

pub(super) fn phase_alignments(
    documents: &BTreeMap<String, Value>,
) -> Vec<crate::measurements::phase_alignment::Alignment> {
    if !documents.contains_key("arm-spe-plan") {
        return Vec::new();
    }
    let null = Value::Null;
    vec![crate::measurements::phase_alignment::spe(
        validated_quality(documents).unwrap_or(&null),
        documents.get("execution-window").unwrap_or(&null),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn docs() -> BTreeMap<String, Value> {
        [("arm-spe-plan".into(),serde_json::json!({})),("arm-spe".into(),serde_json::json!({"status":"completed"})),("arm-spe-quality".into(),serde_json::json!({"schema_version":1,"status":"observed","scope":"agent-process-tree","raw_record_quality":{"aux_records":1},"sample_counts":{"memory":100,"l1d_miss":2},"memory_latency_known_samples":100,"memory_latency_weight_sum_cycles":"2000"}))].into()
    }
    #[test]
    fn sampled_classification_and_latency_are_not_full_hardware_counts() {
        let output = serde_json::to_value(analyze(&docs())).unwrap();
        assert_eq!(output[0]["metrics"][0]["value"], 20.0);
        assert!(output[0]["metrics"][1]["value"].is_null());
        assert_eq!(output[0]["metrics"][3]["value"], 20.0);
        assert!(
            output[0]["limitations"]
                .as_str()
                .unwrap()
                .contains("not cache accesses")
        );
    }
    #[test]
    fn foreign_scope_failed_decode_or_partial_weights_do_not_qualify() {
        let mut input = docs();
        input.get_mut("arm-spe-quality").unwrap()["scope"] = Value::from("host-context");
        assert!(serde_json::to_value(analyze(&input)).unwrap()[0]["metrics"][0]["value"].is_null());
        let mut input = docs();
        input.get_mut("arm-spe").unwrap()["status"] = Value::from("failed");
        assert!(serde_json::to_value(analyze(&input)).unwrap()[0]["metrics"][0]["value"].is_null());
        let mut input = docs();
        input.get_mut("arm-spe-quality").unwrap()["memory_latency_known_samples"] = Value::from(99);
        assert!(serde_json::to_value(analyze(&input)).unwrap()[0]["metrics"][3]["value"].is_null());
        for weight in [
            "0",
            "not-an-integer",
            "340282366920938463463374607431768211455",
        ] {
            let mut invalid = docs();
            invalid.get_mut("arm-spe-quality").unwrap()["memory_latency_weight_sum_cycles"] =
                Value::from(weight);
            assert!(
                serde_json::to_value(analyze(&invalid)).unwrap()[0]["metrics"][3]["value"]
                    .is_null()
            );
        }
    }
}
