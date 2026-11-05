//! Ratios over simultaneously scheduled Arm event groups. No CPU-independent
//! bottleneck thresholds: a Kunpeng is not assumed to implement Neoverse metrics.
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Serialize)]
pub(super) struct Metric {
    pub(super) id: &'static str,
    pub(super) unit: &'static str,
    pub(super) value: Option<f64>,
    pub(super) unavailable_reason: Option<&'static str>,
    pub(super) evidence: Vec<String>,
}

#[derive(Serialize)]
pub(super) struct PmuAnalysis {
    pmu: String,
    rule_version: u32,
    scope: &'static str,
    raw_integer_counts: BTreeMap<String, u64>,
    metrics: Vec<Metric>,
    cpu_profile: crate::cpu_profiles::Selection,
    limitations: &'static str,
}

struct Ratio {
    id: &'static str,
    numerator: &'static str,
    denominator: &'static str,
    scale: f64,
    unit: &'static str,
}

const RATIOS: &[Ratio] = &[
    Ratio {
        id: "instructions_per_cycle",
        numerator: "inst_retired",
        denominator: "cpu_cycles",
        scale: 1.0,
        unit: "instructions/cycle",
    },
    Ratio {
        id: "l1d_refills_per_ki",
        numerator: "l1d_cache_refill",
        denominator: "inst_retired",
        scale: 1000.0,
        unit: "refills/1000 instructions",
    },
    Ratio {
        id: "l1d_tlb_refills_per_ki",
        numerator: "l1d_tlb_refill",
        denominator: "inst_retired",
        scale: 1000.0,
        unit: "refills/1000 instructions",
    },
    Ratio {
        id: "branch_mispredictions_per_ki",
        numerator: "br_mis_pred",
        denominator: "inst_retired",
        scale: 1000.0,
        unit: "mispredictions/1000 instructions",
    },
    Ratio {
        id: "backend_stall_cycles_pct",
        numerator: "stall_backend",
        denominator: "cpu_cycles",
        scale: 100.0,
        unit: "%",
    },
    Ratio {
        id: "frontend_stall_cycles_pct",
        numerator: "stall_frontend",
        denominator: "cpu_cycles",
        scale: 100.0,
        unit: "%",
    },
];

fn eligible(value: &Value) -> Option<(u64, f64)> {
    if value.get("unavailable_rows")?.as_u64()? != 0
        || value.get("unknown_quality_rows")?.as_u64()? != 0
        || value.get("observed_rows")?.as_u64()? == 0
    {
        return None;
    }
    let runtime = value.get("known_event_runtime_ns")?.as_f64()?;
    let min = value.get("min_running_pct")?.as_f64()?;
    let max = value.get("max_running_pct")?.as_f64()?;
    // The shared perf record currently stores runtime as f64. Reject values
    // outside exact integral-nanosecond comparison rather than admitting two
    // different u64 runtimes that round to the same floating-point value.
    if !runtime.is_finite()
        || runtime <= 0.0
        || runtime.fract() != 0.0
        || runtime > 9_007_199_254_740_991.0
        || min != 100.0
        || max != 100.0
    {
        return None;
    }
    Some((value.get("observed_integer_count")?.as_u64()?, runtime))
}

pub(super) fn analyze(documents: &BTreeMap<String, Value>) -> Vec<PmuAnalysis> {
    let Some(record) = documents.get("arm-pmu").filter(|r| {
        r.get("schema_version").and_then(Value::as_u64) == Some(1)
            && r.get("scope").and_then(Value::as_str) == Some("agent-process-tree")
            && matches!(
                r.get("status").and_then(Value::as_str),
                Some("observed" | "partial")
            )
    }) else {
        return Vec::new();
    };
    let groups = documents
        .get("arm-pmu-plan")
        .filter(|p| {
            p.get("schema_version").and_then(Value::as_u64) == Some(1)
                && p.pointer("/plan/enabled").and_then(Value::as_bool) == Some(true)
        })
        .and_then(|p| p.pointer("/plan/groups"))
        .and_then(Value::as_array);
    let Some(groups) = groups.filter(|g| g.len() <= 256) else {
        return Vec::new();
    };
    let Some(events) = record.get("events").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut output = Vec::new();
    for group in groups {
        let Some(group) = group
            .as_str()
            .filter(|g| g.len() <= 2048)
            .and_then(|g| g.strip_prefix('{'))
            .and_then(|g| g.strip_suffix('}'))
        else {
            continue;
        };
        let names = group.split(',').collect::<Vec<_>>();
        if names.len() > 7 || names.is_empty() {
            continue;
        }
        let Some(pmu) = names[0]
            .split('/')
            .next()
            .filter(|p| p.starts_with("armv8_pmuv3") && p.len() <= 128)
        else {
            continue;
        };
        if names
            .iter()
            .any(|name| !name.starts_with(&format!("{pmu}/")))
        {
            continue;
        }
        let mut raw = BTreeMap::new();
        for name in &names {
            if let Some(count) = events
                .get(*name)
                .and_then(|v| v.get("observed_integer_count"))
                .and_then(Value::as_u64)
            {
                raw.insert((*name).to_owned(), count);
            }
        }
        let platform = documents.get("arm-platform").filter(|p| {
            p["schema_version"] == 1
                && p["architecture"] == "aarch64"
                && p["status"] == "discovered"
                && p["scope"] == "host_context"
        });
        let cpus = platform
            .and_then(|p| p.get("cpu_identity"))
            .and_then(crate::cpu_profiles::decode)
            .unwrap_or_default();
        let domain = platform
            .and_then(|p| p.get("pmus"))
            .and_then(Value::as_array)
            .and_then(|pmus| {
                pmus.iter()
                    .find(|p| p["name"] == pmu && p["domain"] == "cpu")
            });
        let cpu_profile =
            crate::cpu_profiles::select(domain.and_then(|p| p["cpus"].as_str()), &cpus);
        let metrics = RATIOS.iter().map(|ratio| {
            let numerator = format!("{pmu}/{}/",ratio.numerator);
            let denominator = format!("{pmu}/{}/",ratio.denominator);
            let pair = if names.contains(&numerator.as_str()) && names.contains(&denominator.as_str()) {
                events.get(&numerator).and_then(eligible).zip(events.get(&denominator).and_then(eligible))
            } else { None };
            let (value, reason) = match pair {
                Some(((n,n_runtime),(d,d_runtime))) if d>0 && n_runtime==d_runtime => {
                    let value = n as f64 / d as f64 * ratio.scale;
                    if cpu_profile.id==Some(crate::cpu_profiles::N1) && ratio.unit=="%" && n>d {
                        (None,Some("CPU-specific cycle accounting exceeds the matched cycle count"))
                    } else if value.is_finite() { (Some(value), None) } else { (None,Some("nonfinite ratio")) }
                }
                Some((_,(0,_))) => (None,Some("zero denominator; ratio is undefined")),
                _ => (None,Some("missing, multiplexed, incomplete, or differently scheduled event group")),
            };
            Metric { id:ratio.id, unit:ratio.unit, value, unavailable_reason:reason, evidence:vec![numerator,denominator] }
        }).collect();
        output.push(PmuAnalysis {
            pmu:pmu.into(), rule_version:1, scope:"agent-process-tree",
            raw_integer_counts:raw, metrics, cpu_profile,
            limitations:"Ratios describe observed, unscaled event groups with matching runtimes and reported 100% running time. Percentages are rounded by perf; no whole-run completeness, critical-path contribution, kernel-only attribution or CPU-specific Topdown classification is claimed.",
        });
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    fn docs() -> BTreeMap<String, Value> {
        let quality = |count| serde_json::json!({"observed_integer_count":count,"observed_rows":2,"unavailable_rows":0,"unknown_quality_rows":0,"known_event_runtime_ns":1000,"min_running_pct":100,"max_running_pct":100});
        [
            ("arm-pmu".into(),serde_json::json!({"schema_version":1,"scope":"agent-process-tree","status":"observed","events":{"armv8_pmuv3_0/cpu_cycles/":quality(100),"armv8_pmuv3_0/inst_retired/":quality(200)}})),
            ("arm-pmu-plan".into(),serde_json::json!({"schema_version":1,"plan":{"enabled":true,"groups":["{armv8_pmuv3_0/cpu_cycles/,armv8_pmuv3_0/inst_retired/}"]}})),
        ].into()
    }
    #[test]
    fn cpu_specific_cycle_accounting_requires_full_identity_and_coherent_counts() {
        let mut input = docs();
        let events = input.get_mut("arm-pmu").unwrap()["events"]
            .as_object_mut()
            .unwrap();
        for (event, count) in [("stall_frontend", 25), ("stall_backend", 50)] {
            let mut quality = events["armv8_pmuv3_0/cpu_cycles/"].clone();
            quality["observed_integer_count"] = serde_json::json!(count);
            events.insert(format!("armv8_pmuv3_0/{event}/"), quality);
        }
        input.get_mut("arm-pmu-plan").unwrap()["plan"]["groups"] = serde_json::json!([
            "{armv8_pmuv3_0/cpu_cycles/,armv8_pmuv3_0/inst_retired/,armv8_pmuv3_0/stall_frontend/,armv8_pmuv3_0/stall_backend/}"
        ]);
        input.insert("arm-platform".into(),serde_json::json!({"schema_version":1,"architecture":"aarch64","status":"discovered","scope":"host_context","pmus":[{"name":"armv8_pmuv3_0","domain":"cpu","cpus":"0-1"}],
            "cpu_identity":{"schema_version":1,"online_cpus":[0,1],"midr_by_cpu":{"0":0x410fd0c0u64,"1":0x411fd0c1u64},"unavailable_cpus":[]}}));
        let output = serde_json::to_value(analyze(&input)).unwrap();
        assert_eq!(output[0]["cpu_profile"]["id"], crate::cpu_profiles::N1);
        assert_eq!(output[0]["metrics"][4]["value"], 50.0);
        assert_eq!(output[0]["metrics"][5]["value"], 25.0);
        let root =
            crate::fixture_namespace::allocate(&std::env::temp_dir(), "asb-cpu-method-").unwrap();
        for (name, value) in &input {
            std::fs::write(root.join(format!("{name}.json")), value.to_string()).unwrap();
        }
        super::super::publish(&root, None).unwrap();
        let published: Value =
            serde_json::from_slice(&std::fs::read(root.join("bottlenecks.json")).unwrap()).unwrap();
        assert_eq!(published, super::super::from_documents(&input));
        std::fs::remove_dir_all(root).unwrap();

        input.get_mut("arm-pmu").unwrap()["events"]["armv8_pmuv3_0/stall_frontend/"]["observed_integer_count"] =
            serde_json::json!(150);
        let output = serde_json::to_value(analyze(&input)).unwrap();
        assert!(output[0]["metrics"][5]["value"].is_null());
        assert!(
            output[0]["metrics"][5]["unavailable_reason"]
                .as_str()
                .unwrap()
                .contains("exceeds")
        );
        input.get_mut("arm-platform").unwrap()["cpu_identity"]["midr_by_cpu"]["1"] =
            serde_json::json!(0x481fd010u64);
        let output = serde_json::to_value(analyze(&input)).unwrap();
        assert_eq!(output[0]["cpu_profile"]["status"], "unsupported");
        assert!(output[0]["cpu_profile"]["id"].is_null());
    }

    #[test]
    fn ratios_require_same_group_runtime_and_complete_observations() {
        let mut input = docs();
        let report = serde_json::to_value(analyze(&input)).unwrap();
        assert_eq!(report[0]["metrics"][0]["value"], 2.0);
        assert!(report[0]["metrics"][1]["value"].is_null());
        for (key, value) in [
            ("unavailable_rows", Value::from(1)),
            ("unknown_quality_rows", Value::from(1)),
            ("min_running_pct", Value::from(90)),
            ("known_event_runtime_ns", Value::from(999)),
        ] {
            input = docs();
            input.get_mut("arm-pmu").unwrap()["events"]["armv8_pmuv3_0/inst_retired/"][key] = value;
            assert!(
                serde_json::to_value(analyze(&input)).unwrap()[0]["metrics"][0]["value"].is_null()
            );
        }
    }
    #[test]
    fn fractional_or_rounded_large_runtime_cannot_prove_group_coherence() {
        for runtime in [
            serde_json::json!(1.5),
            serde_json::json!(9_007_199_254_740_992u64),
            serde_json::json!(9_007_199_254_740_993u64),
        ] {
            let mut input = docs();
            input.get_mut("arm-pmu").unwrap()["events"]["armv8_pmuv3_0/cpu_cycles/"]["known_event_runtime_ns"] =
                runtime.clone();
            input.get_mut("arm-pmu").unwrap()["events"]["armv8_pmuv3_0/inst_retired/"]["known_event_runtime_ns"] =
                runtime;
            assert!(
                serde_json::to_value(analyze(&input)).unwrap()[0]["metrics"][0]["value"].is_null()
            );
        }
    }

    #[test]
    fn missing_plan_foreign_scope_and_zero_denominator_are_not_valid_ratios() {
        let mut input = docs();
        input.remove("arm-pmu-plan");
        assert!(analyze(&input).is_empty());
        input = docs();
        input.get_mut("arm-pmu").unwrap()["scope"] = Value::from("host-context");
        assert!(analyze(&input).is_empty());
        input = docs();
        input.get_mut("arm-pmu").unwrap()["events"]["armv8_pmuv3_0/cpu_cycles/"]["observed_integer_count"] =
            Value::from(0);
        assert!(serde_json::to_value(analyze(&input)).unwrap()[0]["metrics"][0]["value"].is_null());
    }
}
