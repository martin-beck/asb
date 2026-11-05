//! Shared counter evidence for consumers: coherent groups, exact raw integers,
//! explicit quality. No consumer may rebuild IPC from unqualified scalar sums.
use super::hardware::eligible;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Serialize)]
pub(super) struct Group {
    schema_version: u32,
    pub(super) pmu: String,
    scope: &'static str,
    root: crate::measurements::ProcessIdentity,
    status: &'static str,
    requested_events: Vec<String>,
    raw_integer_counts: BTreeMap<String, u64>,
    /// Only counters with complete quality and the cycle reference runtime.
    coherent_counts: BTreeMap<String, u64>,
    counter_runtime_ns: Option<u64>,
    pub(super) instructions_per_cycle: Option<f64>,
    unavailable_events: Vec<String>,
    unavailable_reason: Option<&'static str>,
    limitations: &'static str,
}
fn event(name: &str) -> Option<(String, &str)> {
    let (pmu, alias) = if let Some((pmu, alias)) = name.split_once('/') {
        let alias = alias.strip_suffix('/')?;
        if !(pmu == "cpu_core" || pmu == "cpu_atom" || pmu.starts_with("armv8_pmuv3"))
            || pmu.len() > 128
            || !pmu.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return None;
        }
        (pmu.to_owned(), alias)
    } else {
        ("generic_cpu".into(), name)
    };
    let canonical = match alias {
        "cycles" | "cpu_cycles" => "cycles",
        "instructions" | "inst_retired" => "instructions",
        "cache-misses" => "cache_misses",
        "branch-misses" | "br_mis_pred" => "branch_misses",
        "l1d_cache_refill" => "l1d_cache_refill",
        "l1d_tlb_refill" => "l1d_tlb_refill",
        "stall_backend" => "stall_backend",
        "stall_frontend" => "stall_frontend",
        _ => return None,
    };
    Some((pmu, canonical))
}

pub(super) fn analyze(documents: &BTreeMap<String, Value>) -> Vec<Group> {
    let Some(plan) = documents.get("perf-stat-plan").filter(|p| {
        p["schema_version"] == 1
            && p["scope"] == "agent-process-tree"
            && p["counter_mode"] == "unscaled-observed"
            && p["inherit"] == true
            && p["system_wide"] == false
            && p["root"]["pid"]
                .as_u64()
                .is_some_and(|v| v > 0 && v <= u32::MAX as u64)
            && p["root"]["start_time_ticks"]
                .as_u64()
                .is_some_and(|v| v > 0)
            && p["excluded_controller_pid"]
                .as_u64()
                .is_some_and(|v| v > 0 && v <= u32::MAX as u64)
            && p["root"]["pid"] != p["excluded_controller_pid"]
            && p["root"]["pid"] != p["excluded_guardian"]["pid"]
    }) else {
        return Vec::new();
    };
    if documents.get("measurement-target").is_some_and(|t| {
        t["schema_version"] != 1
            || t["scope"] != "agent-process-tree"
            || t["root"] != plan["root"]
            || t["root"]["pid"] == t["excluded_guardian"]["pid"]
            || t["root"]["pid"] == t["excluded_controller_pid"]
    }) {
        return Vec::new();
    }
    let Some(groups) = plan["groups"].as_array().filter(|g| g.len() <= 256) else {
        return Vec::new();
    };
    let quality = documents.get("perf-stat-quality").filter(|q| {
        q["schema_version"] == 1
            && q["counter_mode"] == "unscaled-observed"
            && documents
                .get("perf-stat")
                .is_some_and(|p| p["status"] == "completed")
    });
    let mut result = Vec::new();
    for group in groups {
        let Some(text) = group
            .as_str()
            .filter(|g| g.len() <= 2048)
            .and_then(|s| s.strip_prefix('{'))
            .and_then(|s| s.strip_suffix('}'))
        else {
            continue;
        };
        let names = text.split(',').collect::<Vec<_>>();
        if names.len() < 2
            || names.len() > 8
            || names.iter().collect::<BTreeSet<_>>().len() != names.len()
        {
            continue;
        }
        let Some(parsed) = names
            .iter()
            .map(|name| event(name))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let pmu = &parsed[0].0;
        if parsed.iter().any(|(p, _)| p != pmu)
            || parsed
                .iter()
                .map(|(_, alias)| *alias)
                .collect::<BTreeSet<_>>()
                .len()
                != names.len()
        {
            continue;
        }
        let raw = names
            .iter()
            .filter_map(|name| {
                Some((
                    (*name).to_owned(),
                    quality?
                        .get("events")?
                        .get(*name)?
                        .get("observed_integer_count")?
                        .as_u64()?,
                ))
            })
            .collect();
        let entries = names
            .iter()
            .zip(&parsed)
            .map(|(name, (_, alias))| {
                (
                    *name,
                    *alias,
                    quality
                        .and_then(|q| q.get("events")?.get(*name))
                        .and_then(eligible),
                )
            })
            .collect::<Vec<_>>();
        let cycles = entries
            .iter()
            .find(|(_, alias, _)| *alias == "cycles")
            .and_then(|(_, _, v)| *v);
        let instructions = entries
            .iter()
            .find(|(_, alias, _)| *alias == "instructions")
            .and_then(|(_, _, v)| *v);
        let mut coherent_counts = BTreeMap::new();
        let mut unavailable = Vec::new();
        for (name, alias, value) in &entries {
            if let Some((count, _)) = value
                .filter(|(_, runtime)| cycles.is_some_and(|(_, reference)| reference == *runtime))
            {
                coherent_counts.insert((*alias).into(), count);
            } else {
                unavailable.push((*name).into());
            }
        }
        let ipc = cycles
            .zip(instructions)
            .filter(|((n, r), (_, s))| *n > 0 && r == s)
            .map(|((c, _), (i, _))| i as f64 / c as f64);
        let status = if coherent_counts.is_empty() {
            "unavailable"
        } else if unavailable.is_empty() {
            "qualified"
        } else {
            "partial"
        };
        result.push(Group {
            schema_version: 1,
            pmu: pmu.clone(),
            scope: "agent-process-tree",
            root: crate::measurements::ProcessIdentity {
                pid: plan["root"]["pid"].as_u64().expect("validated pid") as u32,
                start_time_ticks: plan["root"]["start_time_ticks"].as_u64().expect("validated generation"),
            },
            status,
            requested_events: names.iter().map(|s| (*s).into()).collect(),
            raw_integer_counts: raw,
            coherent_counts,
            counter_runtime_ns: cycles.map(|(_, runtime)| runtime as u64),
            instructions_per_cycle: ipc,
            unavailable_events: unavailable,
            unavailable_reason: ipc.is_none().then_some("IPC requires grouped, complete exact cycle/instruction counts, matching integral runtime and reported 100% scheduling; a zero cycle denominator is undefined"),
            limitations: "Observed unscaled counts, not whole-run estimates or CPU utilization. Runtime equality and 100% scheduling are reported perf quality, with rounded percentages; they do not prove absence of unreported loss. Distinct PMU domains remain separate. Missing/incompatible optional events cannot become zero or be combined with the reference runtime.",
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn documents() -> BTreeMap<String, Value> {
        let quality = |n, r| json!({"observed_integer_count":n,"observed_rows":2,"unavailable_rows":0,"unknown_quality_rows":0,"known_event_runtime_ns":r,"min_running_pct":100,"max_running_pct":100});
        [("perf-stat-plan".into(),json!({"schema_version":1,"scope":"agent-process-tree","counter_mode":"unscaled-observed","inherit":true,"system_wide":false,"excluded_controller_pid":1,"excluded_guardian":{"pid":41,"start_time_ticks":6},"root":{"pid":42,"start_time_ticks":7},"groups":["{cycles,instructions,cache-misses}"]})),
        ("perf-stat".into(),json!({"status":"completed"})),
        ("perf-stat-quality".into(),json!({"schema_version":1,"counter_mode":"unscaled-observed","events":{"cycles":quality(100,1000),"instructions":quality(200,1000),"cache-misses":quality(3,1000)}}))].into()
    }
    #[test]
    fn counters_and_ipc_share_quality_while_optional_misses_stay_missing() {
        let mut d = documents();
        let out = serde_json::to_value(analyze(&d)).unwrap();
        assert_eq!(out[0]["instructions_per_cycle"], 2.0);
        assert_eq!(out[0]["coherent_counts"]["cache_misses"], 3);
        d.get_mut("perf-stat-quality").unwrap()["events"]["cache-misses"]["unavailable_rows"] =
            json!(1);
        let out = serde_json::to_value(analyze(&d)).unwrap();
        assert_eq!(out[0]["instructions_per_cycle"], 2.0);
        assert!(out[0]["coherent_counts"]["cache_misses"].is_null());
        assert_eq!(out[0]["status"], "partial");
    }
    #[test]
    fn ungrouped_foreign_scope_mismatched_domains_or_root_cannot_qualify() {
        for (path, value) in [
            ("/groups", json!(["cycles", "instructions"])),
            ("/scope", json!("host-context")),
            ("/system_wide", json!(true)),
            ("/inherit", json!(false)),
            ("/excluded_controller_pid", json!(42)),
            ("/excluded_guardian/pid", json!(42)),
            (
                "/groups",
                json!(["{cpu_core/cycles/,cpu_atom/instructions/}"]),
            ),
            (
                "/groups",
                json!(["{uncore_imc/cycles/,uncore_imc/instructions/}"]),
            ),
            ("/groups", json!(["{cycles,cycles,instructions}"])),
        ] {
            let mut d = documents();
            *d.get_mut("perf-stat-plan")
                .unwrap()
                .pointer_mut(path)
                .unwrap() = value;
            assert!(analyze(&d).is_empty(), "{path}");
        }
        let mut d = documents();
        d.insert(
            "measurement-target".into(),
            json!({"root":{"pid":43,"start_time_ticks":7}}),
        );
        assert!(analyze(&d).is_empty());
    }
    #[test]
    fn incomplete_multiplexed_runtime_or_failed_capture_cannot_make_ipc() {
        for (field, value) in [
            ("unavailable_rows", json!(1)),
            ("unknown_quality_rows", json!(1)),
            ("min_running_pct", json!(99)),
            ("known_event_runtime_ns", json!(999)),
            ("known_event_runtime_ns", json!(1.5)),
            ("observed_integer_count", Value::Null),
            ("observed_integer_count", json!(0)),
        ] {
            let mut d = documents();
            d.get_mut("perf-stat-quality").unwrap()["events"]["cycles"][field] = value;
            let out = serde_json::to_value(analyze(&d)).unwrap();
            assert!(out[0]["instructions_per_cycle"].is_null(), "{field}");
        }
        let mut d = documents();
        d.get_mut("perf-stat").unwrap()["status"] = json!("failed");
        assert!(
            serde_json::to_value(analyze(&d)).unwrap()[0]["coherent_counts"]
                .as_object()
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn hybrid_domains_and_arm_counts_are_never_merged() {
        let mut d = documents();
        d.get_mut("perf-stat-plan").unwrap()["groups"] = json!([
            "{cpu_core/cycles/,cpu_core/instructions/}",
            "{cpu_atom/cycles/,cpu_atom/instructions/}",
            "{armv8_pmuv3_0/cpu_cycles/,armv8_pmuv3_0/inst_retired/}"
        ]);
        let original = d["perf-stat-quality"]["events"].clone();
        let e = d.get_mut("perf-stat-quality").unwrap()["events"]
            .as_object_mut()
            .unwrap();
        for (prefix, c, i) in [
            ("cpu_core", "cycles", "instructions"),
            ("cpu_atom", "cycles", "instructions"),
            ("armv8_pmuv3_0", "cpu_cycles", "inst_retired"),
        ] {
            e.insert(format!("{prefix}/{c}/"), original["cycles"].clone());
            e.insert(format!("{prefix}/{i}/"), original["instructions"].clone());
        }
        let out = serde_json::to_value(analyze(&d)).unwrap();
        assert_eq!(out.as_array().unwrap().len(), 3);
        assert_eq!(out[0]["pmu"], "cpu_core");
        assert_eq!(out[1]["pmu"], "cpu_atom");
        assert_eq!(out[2]["pmu"], "armv8_pmuv3_0");
    }
}
