//! Common-clock cohort observations. Dispatch membership alone proves no overlap.
mod scaling;
use crate::collectors::{ReportGraph, ReportRow};
use serde::Serialize;
#[cfg(test)]
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
struct Context {
    agent: String,
    provider: String,
    model: String,
    workload: String,
    trial: String,
}
impl Context {
    fn of(row: &ReportRow<'_>) -> Self {
        Self {
            agent: row.agent.into(),
            provider: row.provider.into(),
            model: row.model.into(),
            workload: row.workload.into(),
            trial: row.trial.into(),
        }
    }
}
#[derive(Debug, Serialize)]
struct Window {
    replica: usize,
    pid: u64,
    start_time_ticks: u64,
    release_ns: u64,
    start_ns: u64,
    end_ns: u64,
    exit_ns: u64,
}
#[derive(Debug, Serialize)]
struct Cohort {
    id: String,
    context: Context,
    selected_replicas: usize,
    admitted_replicas: usize,
    status: &'static str,
    unavailable_reason: Option<&'static str>,
    clock_domain: Option<String>,
    peak_live_principals_lower_bound: Option<usize>,
    multiple_principals_overlap_lower_bound_ms: Option<f64>,
    all_principals_overlap_lower_bound_ms: Option<f64>,
    agent_phase_span_upper_bound_ms: Option<f64>,
    completed_agent_phases_per_second_lower_bound: Option<f64>,
    windows: Vec<Window>,
}
#[derive(Serialize)]
pub(crate) struct Report {
    schema_version: u32,
    clock: &'static str,
    rows_without_cohort: usize,
    cohorts: Vec<Cohort>,
    scaling: Vec<scaling::Group>,
    limitations: &'static str,
}

pub(crate) fn analyze(rows: &[ReportRow<'_>]) -> Report {
    let mut groups = BTreeMap::<&str, Vec<&ReportRow<'_>>>::new();
    let mut missing = 0;
    for row in rows {
        if let Some(cohort) = row.execution_cohort {
            groups.entry(&cohort.id).or_default().push(row);
        } else {
            missing += 1;
        }
    }
    let cohorts: Vec<Cohort> = groups
        .into_iter()
        .map(|(id, rows)| {
            let first = rows[0];
            let membership = first.execution_cohort.expect("grouped cohort");
            let mut result = Cohort {
                id: id.into(),
                context: Context::of(first),
                selected_replicas: membership.selected_replicas,
                admitted_replicas: membership.admitted_slots.len(),
                status: "unavailable",
                unavailable_reason: None,
                clock_domain: None,
                peak_live_principals_lower_bound: None,
                multiple_principals_overlap_lower_bound_ms: None,
                all_principals_overlap_lower_bound_ms: None,
                agent_phase_span_upper_bound_ms: None,
                completed_agent_phases_per_second_lower_bound: None,
                windows: Vec::new(),
            };
            if let Err(reason) = fill(&mut result, &rows) {
                result.unavailable_reason = Some(reason);
                result.windows.clear();
                result.clock_domain = None;
            }
            result
        })
        .collect();
    Report {
        schema_version: 1,
        clock: "CLOCK_MONOTONIC",
        rows_without_cohort: missing,
        scaling: scaling::derive(&cohorts),
        cohorts,
        limitations: "Generation-verified native principal lifetimes include sleeping/stopped time and do not prove simultaneous CPU residency or a causal bottleneck. Interior windows exclude unsampled tails and descendants after root exit. Overlap and peak are lower bounds. Release-to-owned-exit envelopes include observation overhead, so phase completion rates are lower bounds, not end-to-end task throughput or a scaling speedup. Compare only complete admitted memberships in one boot/time-namespace clock domain. Missing or incompatible evidence is unavailable, never zero.",
    }
}

fn parse_window(row: &ReportRow<'_>, replica: usize) -> Option<(String, Window)> {
    let w = row.documents.get("execution-window")?;
    let container_phase = w["schema_version"] == 1
        && w["status"] == "observed"
        && w["clock"] == "CLOCK_MONOTONIC"
        && w["scope"] == "controller-observed-execution-phase"
        && w["root"].is_null()
        && w["live_observations"] == 0
        && row
            .documents
            .get("kernel-pressure-summary")
            .is_some_and(|summary| {
                summary["status"] == "completed"
                    && summary["scope"] == "owned-container-execution"
                    && summary["scope_source"] == "verified-owned-container-cgroup"
            });
    if container_phase {
        let domain = w["clock_domain"].as_str()?;
        if domain.len() != 64 || !domain.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let release = w["release_bound_ns"].as_u64()?;
        let begin = w["start_bound_ns"].as_u64()?;
        let end = w["end_bound_ns"].as_u64()?;
        let exit = w["exit_bound_ns"].as_u64()?;
        if release != begin || begin >= end || end != exit {
            return None;
        }
        return Some((
            domain.into(),
            Window {
                replica,
                pid: 0,
                start_time_ticks: 0,
                release_ns: release,
                start_ns: begin,
                end_ns: end,
                exit_ns: exit,
            },
        ));
    }
    let target = row.documents.get("measurement-target")?;
    if w["schema_version"] != 1
        || w["status"] != "observed"
        || w["clock"] != "CLOCK_MONOTONIC"
        || w["scope"] != "verified-native-agent-principal-lifetime"
        || target["schema_version"] != 1
        || target["scope"] != "agent-process-tree"
        || target["source"] != "owned-stopped-native-principal"
        || target["root"] != w["root"]
        || w["live_observations"].as_u64()? < 2
    {
        return None;
    }
    let domain = w["clock_domain"].as_str()?;
    if domain.len() != 64 || !domain.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let window = Window {
        replica,
        pid: w["root"]["pid"].as_u64()?,
        start_time_ticks: w["root"]["start_time_ticks"].as_u64()?,
        release_ns: w["release_bound_ns"].as_u64()?,
        start_ns: w["start_bound_ns"].as_u64()?,
        end_ns: w["end_bound_ns"].as_u64()?,
        exit_ns: w["exit_bound_ns"].as_u64()?,
    };
    let guardian = target["excluded_guardian"]["pid"].as_u64()?;
    let controller = target["excluded_controller_pid"].as_u64()?;
    if window.pid == 0
        || window.pid > u32::MAX as u64
        || window.start_time_ticks == 0
        || guardian == 0
        || guardian > u32::MAX as u64
        || controller == 0
        || controller > u32::MAX as u64
        || guardian == controller
        || target["excluded_guardian"]["start_time_ticks"].as_u64()? == 0
        || window.pid == guardian
        || window.pid == controller
        || window.release_ns == 0
        || window.release_ns > window.start_ns
        || window.start_ns >= window.end_ns
        || window.end_ns > window.exit_ns
    {
        return None;
    }
    Some((domain.into(), window))
}

fn fill(result: &mut Cohort, rows: &[&ReportRow<'_>]) -> Result<(), &'static str> {
    let reference = rows[0].execution_cohort.ok_or("missing_cohort")?;
    if rows.len() != reference.admitted_slots.len() {
        return Err("incomplete_admitted_membership");
    }
    let mut slots = BTreeSet::new();
    let mut roots = BTreeSet::new();
    for row in rows {
        let cohort = row.execution_cohort.ok_or("missing_cohort")?;
        let slot = row.replica_index.ok_or("missing_replica_index")?;
        if cohort.selected_replicas != reference.selected_replicas
            || cohort.admitted_slots != reference.admitted_slots
            || Context::of(row) != result.context
            || !cohort.admits(slot, row.configured_concurrency)
        {
            return Err("incompatible_cohort_member");
        }
        if !slots.insert(slot) {
            return Err("duplicate_replica_index");
        }
        let (domain, window) = parse_window(row, slot).ok_or("missing_or_invalid_agent_window")?;
        if result
            .clock_domain
            .as_ref()
            .is_some_and(|expected| expected != &domain)
        {
            return Err("different_clock_domains");
        }
        if window.pid != 0 && !roots.insert((window.pid, window.start_time_ticks)) {
            return Err("duplicate_principal_generation");
        }
        result.clock_domain = Some(domain);
        result.windows.push(window);
    }
    if slots.iter().copied().collect::<Vec<_>>() != reference.admitted_slots {
        return Err("incomplete_admitted_membership");
    }
    result.windows.sort_by_key(|w| w.replica);
    let windows = &result.windows;
    let release = windows
        .iter()
        .map(|w| w.release_ns)
        .min()
        .ok_or("empty_cohort")?;
    let exit = windows
        .iter()
        .map(|w| w.exit_ns)
        .max()
        .ok_or("empty_cohort")?;
    let span = exit
        .checked_sub(release)
        .filter(|n| *n > 0)
        .ok_or("invalid_phase_envelope")?;
    let mut events = BTreeMap::<u64, i32>::new();
    for w in windows {
        *events.entry(w.start_ns).or_default() += 1;
        *events.entry(w.end_ns).or_default() -= 1;
    }
    let mut active = 0i32;
    let mut peak = 0i32;
    let mut previous = 0u64;
    let mut overlap = 0u64;
    for (time, delta) in events {
        if active >= 2 {
            overlap = overlap
                .checked_add(time - previous)
                .ok_or("overlap_overflow")?;
        }
        active += delta;
        peak = peak.max(active);
        previous = time;
    }
    let common_start = windows
        .iter()
        .map(|w| w.start_ns)
        .max()
        .ok_or("empty_cohort")?;
    let common_end = windows
        .iter()
        .map(|w| w.end_ns)
        .min()
        .ok_or("empty_cohort")?;
    result.peak_live_principals_lower_bound = Some(peak as usize);
    result.multiple_principals_overlap_lower_bound_ms = Some(overlap as f64 / 1_000_000.0);
    result.all_principals_overlap_lower_bound_ms =
        Some(common_end.saturating_sub(common_start) as f64 / 1_000_000.0);
    result.agent_phase_span_upper_bound_ms = Some(span as f64 / 1_000_000.0);
    result.completed_agent_phases_per_second_lower_bound =
        Some(windows.len() as f64 * 1_000_000_000.0 / span as f64);
    result.status = "observed";
    Ok(())
}

impl Report {
    pub(crate) fn graphs(&self) -> Vec<ReportGraph> {
        let mut graphs = self.cohorts.iter().filter(|c| c.status == "observed").take(16).enumerate().map(|(index, c)| {
            let first = c.windows.iter().map(|w| w.release_ns).min().unwrap_or(0);
            let last = c.windows.iter().map(|w| w.exit_ns).max().unwrap_or(first);
            let duration = (last - first) as f64;
            let height = 130 + 30 * c.windows.len();
            let container_phase = c.windows.iter().any(|window| window.pid == 0);
            let title = if container_phase { "Observed owned-container execution phases" } else { "Observed agent principal lifetimes" };
            let detail = if container_phase { "Solid: controller-observed execution phase for a verified owned container. It is not an agent PID lifetime or CPU residency." } else { "Solid: verified interior lifetime. Dashed: enclosing agent phase. Sleeping is included; CPU overlap is not inferred." };
            let mut svg = format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 960 {height}\" role=\"img\" aria-label=\"{title}\"><rect width=\"960\" height=\"{height}\" fill=\"white\"/><text x=\"24\" y=\"28\" font-family=\"sans-serif\" font-size=\"18\">{title}</text><text x=\"24\" y=\"49\" font-family=\"sans-serif\" font-size=\"11\">{detail}</text>");
            let x = |ns: u64| 110.0 + 810.0 * (ns - first) as f64 / duration;
            for (i, w) in c.windows.iter().enumerate() {
                let y = 77 + 30 * i;
                let scope = if w.pid == 0 { "owned-container execution phase" } else { "verified lifetime" };
                let identity = if w.pid == 0 { String::new() } else { format!(", PID {}", w.pid) };
                svg.push_str(&format!("<text x=\"20\" y=\"{}\" font-family=\"sans-serif\" font-size=\"12\">Replica {}</text><line x1=\"{:.2}\" y1=\"{y}\" x2=\"{:.2}\" y2=\"{y}\" stroke=\"#64748b\" stroke-dasharray=\"4 3\"/><rect x=\"{:.2}\" y=\"{}\" width=\"{:.2}\" height=\"16\" rx=\"3\" fill=\"#047857\"><title>Replica {}: {} {:.3} ms{}</title></rect>", y + 4, w.replica, x(w.release_ns), x(w.exit_ns), x(w.start_ns), y - 8, x(w.end_ns) - x(w.start_ns), w.replica, scope, (w.end_ns - w.start_ns) as f64 / 1_000_000.0, identity));
            }
            for tick in 0..=5 {
                let at = 110.0 + tick as f64 * 810.0 / 5.0;
                svg.push_str(&format!("<text x=\"{at:.2}\" y=\"{}\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"11\">{:.3}</text>", height - 32, duration * tick as f64 / 5_000_000_000.0));
            }
            svg.push_str(&format!("<text x=\"515\" y=\"{}\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"12\">Time since earliest observed release bound (s)</text></svg>", height - 12));
            ReportGraph { filename: format!("cohort-lifetimes-{:04}.svg", index + 1), svg }
        }).collect::<Vec<_>>();
        graphs.extend(scaling::graphs(&self.scaling));
        graphs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::measurements::ExecutionCohort;
    use serde_json::json;
    pub(super) fn docs(
        pid: u64,
        release: u64,
        start: u64,
        end: u64,
        exit: u64,
    ) -> BTreeMap<String, Value> {
        let root = json!({"pid":pid,"start_time_ticks":1});
        [("execution-window".into(), json!({"schema_version":1,"status":"observed","clock":"CLOCK_MONOTONIC","clock_domain":"a".repeat(64),"scope":"verified-native-agent-principal-lifetime","root":root,"live_observations":3,"release_bound_ns":release*1_000_000_000,"start_bound_ns":start*1_000_000_000,"end_bound_ns":end*1_000_000_000,"exit_bound_ns":exit*1_000_000_000})),
         ("measurement-target".into(),json!({"schema_version":1,"scope":"agent-process-tree","source":"owned-stopped-native-principal","root":root,"excluded_guardian":{"pid":900,"start_time_ticks":1},"excluded_controller_pid":901}))].into()
    }
    fn container_docs(begin: u64, end: u64) -> BTreeMap<String, Value> {
        [("execution-window".into(), json!({"schema_version":1,"status":"observed","clock":"CLOCK_MONOTONIC","clock_domain":"a".repeat(64),"scope":"controller-observed-execution-phase","root":null,"live_observations":0,"release_bound_ns":begin*1_000_000_000,"start_bound_ns":begin*1_000_000_000,"end_bound_ns":end*1_000_000_000,"exit_bound_ns":end*1_000_000_000})),
         ("kernel-pressure-summary".into(), json!({"status":"completed","scope":"owned-container-execution","scope_source":"verified-owned-container-cgroup"}))].into()
    }
    pub(super) fn row<'a>(
        slot: usize,
        cohort: &'a ExecutionCohort,
        documents: &'a BTreeMap<String, Value>,
        metrics: &'a BTreeMap<String, f64>,
    ) -> ReportRow<'a> {
        ReportRow {
            label: "fixture",
            agent: "pi",
            provider: "cli2key",
            model: "test-model",
            workload: "bug-fix",
            trial: "default",
            configured_concurrency: cohort.selected_replicas,
            replica_index: Some(slot),
            execution_cohort: Some(cohort),
            metrics,
            components: None,
            documents,
        }
    }
    pub(super) fn membership(slots: Vec<usize>, selected: usize) -> ExecutionCohort {
        ExecutionCohort {
            id: "a-123".into(),
            selected_replicas: selected,
            admitted_slots: slots,
            dispatch: None,
        }
    }
    #[test]
    fn live_overlap_and_completion_envelope_have_opposite_bound_directions() {
        let m = membership(vec![1, 2], 2);
        let a = docs(42, 1, 2, 7, 10);
        let b = docs(43, 2, 4, 9, 11);
        let metrics = BTreeMap::new();
        let report = analyze(&[row(1, &m, &a, &metrics), row(2, &m, &b, &metrics)]);
        let c = &report.cohorts[0];
        assert_eq!(c.status, "observed");
        assert_eq!(c.peak_live_principals_lower_bound, Some(2));
        assert_eq!(c.multiple_principals_overlap_lower_bound_ms, Some(3000.0));
        assert_eq!(c.all_principals_overlap_lower_bound_ms, Some(3000.0));
        assert_eq!(c.agent_phase_span_upper_bound_ms, Some(10000.0));
        assert_eq!(c.completed_agent_phases_per_second_lower_bound, Some(0.2));
        let graph = &report.graphs()[0];
        assert!(graph.svg.contains("(s)</text>"));
        assert!(graph.svg.contains("Replica 1"));
        assert!(graph.svg.contains("0.000</text>"));
        assert_eq!(graph.filename, "cohort-lifetimes-0001.svg");
    }

    #[test]
    fn verified_owned_container_phases_form_a_clearly_scoped_cohort() {
        let membership = membership(vec![1, 2], 2);
        let first = container_docs(1, 6);
        let second = container_docs(3, 8);
        let metrics = BTreeMap::new();
        let report = analyze(&[
            row(1, &membership, &first, &metrics),
            row(2, &membership, &second, &metrics),
        ]);
        let cohort = &report.cohorts[0];
        assert_eq!(cohort.status, "observed");
        assert_eq!(cohort.peak_live_principals_lower_bound, Some(2));
        assert_eq!(
            cohort.multiple_principals_overlap_lower_bound_ms,
            Some(3000.0)
        );
        let graph = &report.graphs()[0].svg;
        assert!(graph.contains("owned-container execution phase"));
        assert!(!graph.contains("PID 0"));
    }
    #[test]
    fn sequential_and_touching_lifetimes_have_observed_zero_overlap() {
        let m = membership(vec![1, 2], 2);
        let a = docs(42, 1, 2, 4, 6);
        let metrics = BTreeMap::new();
        for start in [4, 5] {
            let b = docs(43, 3, start, 7, 8);
            let report = analyze(&[row(1, &m, &a, &metrics), row(2, &m, &b, &metrics)]);
            let c = &report.cohorts[0];
            assert_eq!(c.status, "observed");
            assert_eq!(c.peak_live_principals_lower_bound, Some(1));
            assert_eq!(c.multiple_principals_overlap_lower_bound_ms, Some(0.0));
            assert_eq!(c.all_principals_overlap_lower_bound_ms, Some(0.0));
        }
    }
    #[test]
    fn sparse_actual_membership_and_three_way_overlap_do_not_infer_selected_width() {
        let m = membership(vec![1, 3, 4], 4);
        let metrics = BTreeMap::new();
        let a = docs(42, 1, 2, 6, 9);
        let b = docs(43, 2, 3, 8, 10);
        let c = docs(44, 4, 5, 7, 9);
        let report = analyze(&[
            row(4, &m, &c, &metrics),
            row(1, &m, &a, &metrics),
            row(3, &m, &b, &metrics),
        ]);
        let c = &report.cohorts[0];
        assert_eq!(c.admitted_replicas, 3);
        assert_eq!(c.selected_replicas, 4);
        assert_eq!(c.peak_live_principals_lower_bound, Some(3));
        assert_eq!(c.multiple_principals_overlap_lower_bound_ms, Some(4000.0));
        assert_eq!(c.all_principals_overlap_lower_bound_ms, Some(1000.0));
        assert_eq!(
            c.windows.iter().map(|w| w.replica).collect::<Vec<_>>(),
            vec![1, 3, 4]
        );
    }
    #[test]
    fn missing_duplicate_or_mixed_membership_does_not_make_a_scaling_point() {
        let m = membership(vec![1, 2], 2);
        let metrics = BTreeMap::new();
        let a = docs(42, 1, 2, 7, 10);
        let b = docs(43, 2, 4, 9, 11);
        let mut cases = vec![
            vec![row(1, &m, &a, &metrics)],
            vec![row(1, &m, &a, &metrics), row(1, &m, &b, &metrics)],
            vec![row(1, &m, &a, &metrics), row(2, &m, &a, &metrics)],
        ];
        let mut foreign = row(2, &m, &b, &metrics);
        foreign.agent = "other";
        cases.push(vec![row(1, &m, &a, &metrics), foreign]);
        let mut missing = row(2, &m, &b, &metrics);
        missing.replica_index = None;
        cases.push(vec![row(1, &m, &a, &metrics), missing]);
        for rows in cases {
            let report = analyze(&rows);
            let c = &report.cohorts[0];
            assert_eq!(c.status, "unavailable");
            assert!(c.unavailable_reason.is_some());
            assert!(c.peak_live_principals_lower_bound.is_none());
            assert!(c.completed_agent_phases_per_second_lower_bound.is_none());
            assert!(c.windows.is_empty());
            assert!(report.graphs().is_empty());
        }
    }
    #[test]
    fn malformed_foreign_scope_clock_or_generation_never_becomes_zero() {
        let m = membership(vec![1, 2], 2);
        let metrics = BTreeMap::new();
        let a = docs(42, 1, 2, 7, 10);
        for (document, path, value) in [
            ("execution-window", "/status", json!("failed")),
            ("execution-window", "/scope", json!("host-context")),
            ("execution-window", "/clock_domain", json!("b".repeat(64))),
            ("execution-window", "/clock_domain", Value::Null),
            ("execution-window", "/root/start_time_ticks", json!(2)),
            (
                "execution-window",
                "/release_bound_ns",
                json!(900_000_000_000u64),
            ),
            ("execution-window", "/exit_bound_ns", Value::Null),
            ("execution-window", "/end_bound_ns", json!(-1)),
            (
                "measurement-target",
                "/source",
                json!("container-controller"),
            ),
            ("measurement-target", "/excluded_guardian/pid", json!(43)),
        ] {
            let mut b = docs(43, 2, 4, 9, 11);
            *b.get_mut(document).unwrap().pointer_mut(path).unwrap() = value;
            let report = analyze(&[row(1, &m, &a, &metrics), row(2, &m, &b, &metrics)]);
            let c = &report.cohorts[0];
            assert_eq!(c.status, "unavailable", "{document}{path}");
            assert!(c.multiple_principals_overlap_lower_bound_ms.is_none());
            assert!(c.completed_agent_phases_per_second_lower_bound.is_none());
        }
    }
}
