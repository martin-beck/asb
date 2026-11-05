//! Descriptive observations by actual admitted width, never an inferred speedup.
use super::{Cohort, Context, ReportGraph};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Serialize)]
pub(super) struct Point {
    cohort_id: String,
    selected_replicas: usize,
    admitted_replicas: usize,
    observed_peak_live_principals_lower_bound: usize,
    phase_completions_per_second_lower_bound: f64,
}
#[derive(Serialize)]
pub(super) struct Group {
    context: Context,
    clock_domain: String,
    points: Vec<Point>,
    limitations: &'static str,
}

pub(super) fn derive(cohorts: &[Cohort]) -> Vec<Group> {
    let mut groups = BTreeMap::<(Context, String), Vec<Point>>::new();
    for c in cohorts.iter().filter(|c| c.status == "observed") {
        if let (Some(domain), Some(rate), Some(peak)) = (
            &c.clock_domain,
            c.completed_agent_phases_per_second_lower_bound,
            c.peak_live_principals_lower_bound,
        ) {
            groups
                .entry((c.context.clone(), domain.clone()))
                .or_default()
                .push(Point {
                    cohort_id: c.id.clone(),
                    selected_replicas: c.selected_replicas,
                    admitted_replicas: c.admitted_replicas,
                    observed_peak_live_principals_lower_bound: peak,
                    phase_completions_per_second_lower_bound: rate,
                });
        }
    }
    groups.into_iter().filter_map(|((context, clock_domain), mut points)| {
        if points.iter().map(|p| p.admitted_replicas).collect::<BTreeSet<_>>().len() < 2 { return None; }
        points.sort_by(|a,b| a.admitted_replicas.cmp(&b.admitted_replicas).then(a.cohort_id.cmp(&b.cohort_id)));
        Some(Group {
            context, clock_domain, points,
            limitations: "Descriptive phase completion-rate lower bounds by actual admitted width. Repeated cohorts remain separate points, never silently averaged. A common boot/clock and configuration names do not establish identical input contents, budgets, instrumentation, host contention or provider behavior. These are not controlled speedups, causal scaling efficiency or end-to-end task throughput.",
        })
    }).collect()
}

pub(super) fn graphs(groups: &[Group]) -> Vec<ReportGraph> {
    groups.iter().take(8).enumerate().map(|(index,g)| {
        let max_width = g.points.iter().map(|p|p.admitted_replicas).max().unwrap_or(1);
        let max_rate = g.points.iter().map(|p|p.phase_completions_per_second_lower_bound).fold(0.0,f64::max) * 1.1;
        let x = |n: usize| 100.0 + 800.0*n as f64/max_width as f64;
        let y = |rate: f64| 370.0 - 280.0*rate/max_rate;
        let mut svg = String::from("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 960 440\" role=\"img\" aria-label=\"Observed phase rates by admitted width\"><rect width=\"960\" height=\"440\" fill=\"white\"/><text x=\"24\" y=\"28\" font-family=\"sans-serif\" font-size=\"18\">Observed phase rates by admitted width</text><text x=\"24\" y=\"49\" font-family=\"sans-serif\" font-size=\"11\">Lower-bound phase completions/s; separate points per cohort. Descriptive observations, not controlled speedup.</text>");
        for tick in 0..=5 {
            let rate = max_rate * tick as f64/5.0;
            let label = if rate > 0.0 && rate < 0.001 { format!("{rate:.2e}") } else { format!("{rate:.4}") };
            svg.push_str(&format!("<line x1=\"100\" y1=\"{:.2}\" x2=\"900\" y2=\"{:.2}\" stroke=\"#e2e8f0\"/><text x=\"90\" y=\"{:.2}\" text-anchor=\"end\" font-family=\"sans-serif\" font-size=\"11\">{label}</text>",y(rate),y(rate),y(rate)+4.0));
        }
        let widths = (0..=5).map(|tick| max_width*tick/5).collect::<BTreeSet<_>>();
        for n in widths {
            svg.push_str(&format!("<text x=\"{:.2}\" y=\"393\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"11\">{n}</text>",x(n)));
        }
        for p in &g.points {
            svg.push_str(&format!("<circle cx=\"{:.2}\" cy=\"{:.2}\" r=\"6\" fill=\"#047857\" stroke=\"white\" stroke-width=\"2\"><title>Cohort {}: admitted {}, observed live peak at least {}, phase rate at least {:.6}/s</title></circle>", x(p.admitted_replicas), y(p.phase_completions_per_second_lower_bound), p.cohort_id, p.admitted_replicas, p.observed_peak_live_principals_lower_bound, p.phase_completions_per_second_lower_bound));
        }
        svg.push_str("<text x=\"500\" y=\"425\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"12\">Actual admitted agent phases (count)</text><text x=\"24\" y=\"230\" transform=\"rotate(-90 24 230)\" text-anchor=\"middle\" font-family=\"sans-serif\" font-size=\"12\">Phase completion rate lower bound (/s)</text></svg>");
        ReportGraph {filename:format!("cohort-scaling-{:04}.svg",index+1),svg}
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::measurements::analysis::cohort::tests::{docs, membership, row};
    #[test]
    fn repeated_widths_stay_separate_and_foreign_clocks_never_form_one_curve() {
        let metrics = BTreeMap::new();
        let a = docs(42, 1, 2, 7, 10);
        let b = docs(43, 2, 4, 9, 11);
        let one = membership(vec![1], 1);
        let mut two = membership(vec![1, 2], 2);
        two.id = "b-123".into();
        let mut repeated = membership(vec![1], 1);
        repeated.id = "c-123".into();
        let report = super::super::analyze(&[
            row(1, &one, &a, &metrics),
            row(1, &two, &a, &metrics),
            row(2, &two, &b, &metrics),
            row(1, &repeated, &a, &metrics),
        ]);
        assert_eq!(report.scaling.len(), 1);
        assert_eq!(report.scaling[0].points.len(), 3);
        assert_eq!(
            report.scaling[0]
                .points
                .iter()
                .map(|p| p.admitted_replicas)
                .collect::<Vec<_>>(),
            vec![1, 1, 2]
        );
        let graph = &graphs(&report.scaling)[0];
        assert!(graph.svg.contains("/s)"));
        assert_eq!(graph.svg.matches("<circle ").count(), 3);
        let mut foreign_a = a.clone();
        let mut foreign_b = b.clone();
        foreign_a.get_mut("execution-window").unwrap()["clock_domain"] =
            serde_json::json!("b".repeat(64));
        foreign_b.get_mut("execution-window").unwrap()["clock_domain"] =
            serde_json::json!("b".repeat(64));
        let report = super::super::analyze(&[
            row(1, &one, &a, &metrics),
            row(1, &two, &foreign_a, &metrics),
            row(2, &two, &foreign_b, &metrics),
        ]);
        assert!(report.scaling.is_empty());
    }
}
