//! Arm counters use the existing process-tree perf producer and its custody.
use crate::collectors::{CollectorAdapter, TargetScope};
use crate::measurements::{self, Session};
use serde::Serialize;
use std::io::Read;

pub(super) struct ArmPmu;
pub(super) static ADAPTER: ArmPmu = ArmPmu;

#[derive(Default, Clone, Serialize)]
struct Plan {
    enabled: bool,
    groups: Vec<String>,
    reason: Option<String>,
}

impl CollectorAdapter for ArmPmu {
    fn id(&self) -> &'static str {
        "arm-pmu"
    }
    fn description(&self) -> &'static str {
        "Arm core counters through the shared process-tree perf session"
    }
    fn dependencies(&self) -> &'static [&'static str] {
        &["arm-platform", "perf"]
    }
    fn target_scope(&self, _: &str) -> TargetScope {
        TargetScope::AgentProcess
    }
    fn configure(&self, session: &mut Session, _: &[String]) -> Result<(), String> {
        let inventory = super::arm_platform::resolved_inventory(session);
        let mut plan = make_plan(&inventory.pmus, Some(&inventory.cpu_identity));
        plan.enabled = std::env::consts::ARCH == "aarch64";
        *session.state::<Plan>().borrow_mut() = plan;
        Ok(())
    }
    fn start(&self, session: &mut Session) -> Result<(), String> {
        let plan = session.state::<Plan>();
        measurements::write_json(
            &session.dir.join("arm-pmu-plan.json"),
            &serde_json::json!({
                "schema_version":1, "collector":"arm-pmu", "scope":"agent-process-tree",
                "plan": &*plan.borrow(),
                "producer":"perf-stat", "result":"perf-stat.json", "quality":"perf-stat-quality.json",
                "limitations":"Event groups are observed unscaled counters. Missing or unschedulable groups remain unavailable. CPU domains retain distinct event identities; shared uncore PMUs are excluded."
            }),
        )
    }
    fn finish(&self, session: &mut Session) -> Result<(), String> {
        let plan = session.state::<Plan>().borrow().clone();
        let read = || -> Option<serde_json::Value> {
            let mut bytes = Vec::new();
            std::fs::File::open(session.dir.join("perf-stat-quality.json"))
                .ok()?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > 1024 * 1024 {
                return None;
            }
            serde_json::from_slice(&bytes).ok()
        };
        measurements::write_json(
            &session.dir.join("arm-pmu.json"),
            &summarize(&plan, read().as_ref()),
        )
    }
}

fn summarize(plan: &Plan, quality: Option<&serde_json::Value>) -> serde_json::Value {
    let mut events = serde_json::Map::new();
    for event in plan
        .groups
        .iter()
        .flat_map(|group| group.trim_matches(['{', '}']).split(','))
    {
        let value = quality
            .and_then(|q| q.get("events"))
            .and_then(|q| q.get(event));
        events.insert(
            event.into(),
            value.cloned().unwrap_or(serde_json::Value::Null),
        );
    }
    let observed = events
        .values()
        .filter(|v| v.get("observed_integer_count").is_some_and(|v| v.is_u64()))
        .count();
    serde_json::json!({
        "schema_version":1, "collector":"arm-pmu", "scope":"agent-process-tree",
        "status":if !plan.enabled || observed == 0 { "unavailable" } else if observed == events.len() { "observed" } else { "partial" },
        "reason":if !plan.enabled || plan.groups.is_empty() { plan.reason.as_deref() } else if observed == 0 { Some("Shared perf session did not publish validated Arm counters") } else { None },
        "events":events, "source_artifact":"perf-stat-quality.json",
        "limitations":"Integer counts are unscaled observations. Per-event runtime and unavailable intervals must be checked before deriving ratios; host-shared uncore counters are excluded."
    })
}

fn make_plan(
    pmus: &[super::arm_platform::Pmu],
    cpus: Option<&crate::cpu_profiles::CpuInventory>,
) -> Plan {
    let mut groups = Vec::new();
    for pmu in pmus.iter().filter(|p| p.domain == "cpu") {
        if !["cpu_cycles", "inst_retired"]
            .iter()
            .all(|event| pmu.events.contains_key(*event))
        {
            continue;
        }
        let mut events = [
            "cpu_cycles",
            "inst_retired",
            "l1d_cache_refill",
            "l1d_tlb_refill",
            "br_mis_pred",
            "stall_backend",
        ]
        .into_iter()
        .filter(|event| pmu.events.contains_key(*event))
        .map(|event| format!("{}/{event}/", pmu.name))
        .collect::<Vec<_>>();
        if cpus.is_some_and(|cpus| {
            crate::cpu_profiles::select(pmu.cpus.as_deref(), cpus).id
                == Some(crate::cpu_profiles::N1)
        }) && pmu.events.contains_key("stall_frontend")
        {
            // N1 has six programmable counters plus the dedicated cycle
            // counter. Preserve the existing six events and add its frontend
            // cycle-accounting event in the same acknowledged group.
            events.push(format!("{}/stall_frontend/", pmu.name));
        }
        groups.push(format!("{{{}}}", events.join(",")));
    }
    Plan {
        enabled: true,
        reason: groups
            .is_empty()
            .then(|| "No Arm CPU PMU with cycles and retired instructions was discovered".into()),
        groups,
    }
}

/// Some(empty) intentionally suppresses unrelated generic hardware events when
/// Arm PMUs are unavailable. Software perf observations remain collected.
pub(super) fn events(session: &mut Session) -> Option<Vec<String>> {
    let plan = session.state::<Plan>();
    let plan = plan.borrow();
    plan.enabled.then(|| plan.groups.clone())
}

pub(super) fn metric_name(event: &str) -> Option<&'static str> {
    let (pmu, event) = event.split_once('/')?;
    if !pmu.starts_with("armv8_pmuv3") {
        return None;
    }
    match event.strip_suffix('/')? {
        "cpu_cycles" => Some("observed_cycles_count"),
        "inst_retired" => Some("observed_instructions_count"),
        "l1d_cache_refill" => Some("observed_l1d_cache_refill_count"),
        "l1d_tlb_refill" => Some("observed_l1d_tlb_refill_count"),
        "br_mis_pred" => Some("observed_branch_mispredictions_count"),
        "stall_backend" => Some("observed_stall_backend_count"),
        "stall_frontend" => Some("observed_stall_frontend_count"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn n1_cycle_accounting_adds_only_an_advertised_frontend_event_in_its_domain() {
        let mut pmu = super::super::arm_platform::Pmu {
            name: "armv8_pmuv3_0".into(),
            domain: "cpu",
            event_type: Some(10),
            cpus: Some("0-1".into()),
            cpumask: None,
            formats: Default::default(),
            events: [
                "cpu_cycles",
                "inst_retired",
                "l1d_cache_refill",
                "l1d_tlb_refill",
                "br_mis_pred",
                "stall_backend",
                "stall_frontend",
            ]
            .into_iter()
            .map(|event| (event.into(), "advertised".into()))
            .collect(),
        };
        let mut cpus = crate::cpu_profiles::CpuInventory {
            schema_version: 1,
            online_cpus: vec![0, 1],
            midr_by_cpu: [(0, 0x410fd0c0), (1, 0x411fd0c1)].into(),
            unavailable_cpus: vec![],
        };
        let plan = make_plan(&[pmu.clone()], Some(&cpus));
        assert_eq!(plan.groups[0].split(',').count(), 7);
        assert!(plan.groups[0].contains("armv8_pmuv3_0/stall_frontend/"));
        assert!(!make_plan(&[pmu.clone()], None).groups[0].contains("stall_frontend"));
        cpus.midr_by_cpu.insert(1, 0x481fd010);
        assert!(!make_plan(&[pmu.clone()], Some(&cpus)).groups[0].contains("stall_frontend"));
        cpus.midr_by_cpu.insert(1, 0x410fd0c0);
        pmu.events.remove("stall_frontend");
        assert!(!make_plan(&[pmu], Some(&cpus)).groups[0].contains("stall_frontend"));
    }

    #[test]
    fn absent_quality_never_becomes_a_zero_counter() {
        let plan = Plan {
            enabled: true,
            groups: vec!["{armv8_pmuv3_0/cpu_cycles/,armv8_pmuv3_0/inst_retired/}".into()],
            reason: None,
        };
        let missing = summarize(&plan, None);
        assert_eq!(missing["status"], "unavailable");
        assert!(missing["events"]["armv8_pmuv3_0/cpu_cycles/"].is_null());
        let quality = serde_json::json!({"events":{"armv8_pmuv3_0/cpu_cycles/":{"observed_integer_count":0}}});
        assert_eq!(summarize(&plan, Some(&quality))["status"], "partial");
    }
    #[test]
    fn planning_uses_only_advertised_core_events_and_preserves_pmu_domains() {
        let pmu = |name: &str, domain: &'static str| super::super::arm_platform::Pmu {
            name: name.into(),
            domain,
            event_type: Some(10),
            cpus: Some("0-3".into()),
            cpumask: None,
            events: [
                ("cpu_cycles".into(), "event=0x11".into()),
                ("inst_retired".into(), "event=0x08".into()),
            ]
            .into(),
            formats: Default::default(),
        };
        let plan = make_plan(
            &[
                pmu("armv8_pmuv3_0", "cpu"),
                pmu("armv8_pmuv3_1", "cpu"),
                pmu("hisi_ddr", "uncore"),
            ],
            None,
        );
        assert_eq!(
            plan.groups,
            [
                "{armv8_pmuv3_0/cpu_cycles/,armv8_pmuv3_0/inst_retired/}",
                "{armv8_pmuv3_1/cpu_cycles/,armv8_pmuv3_1/inst_retired/}"
            ]
        );
        assert!(make_plan(&[], None).reason.is_some());
        assert_eq!(
            metric_name("armv8_pmuv3_0/cpu_cycles/"),
            Some("observed_cycles_count")
        );
        assert_eq!(metric_name("hisi_ddr/cpu_cycles/"), None);
        assert_eq!(metric_name("armv8_pmuv3_0/unsupported/"), None);
    }
}
