//! Per-configuration distributions of observed replica measurements.
use super::*;

#[derive(Debug, PartialEq)]
struct Summary {
    q1: f64,
    median: f64,
    q3: f64,
    low: f64,
    high: f64,
}

fn quantile(sorted: &[f64], p: f64) -> f64 {
    let at = (sorted.len() - 1) as f64 * p;
    let lo = at.floor() as usize;
    let hi = at.ceil() as usize;
    sorted[lo] * (1.0 - at.fract()) + sorted[hi] * at.fract()
}

fn summary(values: &[f64]) -> Option<Summary> {
    let mut sorted = values
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .collect::<Vec<_>>();
    sorted.sort_by(f64::total_cmp);
    if sorted.is_empty() {
        return None;
    }
    let q1 = quantile(&sorted, 0.25);
    let median = quantile(&sorted, 0.5);
    let q3 = quantile(&sorted, 0.75);
    let iqr = q3 - q1;
    let low = sorted
        .iter()
        .copied()
        .find(|v| *v >= q1 - 1.5 * iqr)
        .unwrap_or(sorted[0]);
    let high = sorted
        .iter()
        .copied()
        .rfind(|v| *v <= q3 + 1.5 * iqr)
        .unwrap_or(*sorted.last().unwrap());
    Some(Summary {
        q1,
        median,
        q3,
        low,
        high,
    })
}

fn svg(metric: &str, values: &[(String, f64)]) -> String {
    let nums = values.iter().map(|(_, v)| *v).collect::<Vec<_>>();
    let Some(s) = summary(&nums) else {
        return String::new();
    };
    let min = nums.iter().copied().fold(f64::INFINITY, f64::min);
    let max = nums.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let position = |v: f64| {
        if min == max {
            260.0
        } else {
            70.0 + domain_fraction(v, min, max) * 380.0
        }
    };
    let spread = ((values.len().saturating_sub(1)) as f64 * 3.2).max(20.0);
    let center = spread + 15.0;
    let top = center - 20.0;
    let bottom = center + 20.0;
    let whisker_top = center - 14.0;
    let whisker_bottom = center + 14.0;
    let axis = center + spread + 16.0;
    let axis_labels = axis + 17.0;
    let unit_label = axis + 36.0;
    let height = axis + 45.0;
    let mut out = format!(
        "<svg class=\"replica-boxplot\" viewBox=\"0 0 520 {height:.2}\" role=\"img\" aria-label=\"{} distribution, {} observations\"><line x1=\"{:.2}\" y1=\"{center:.2}\" x2=\"{:.2}\" y2=\"{center:.2}\" stroke=\"#274765\" stroke-width=\"2\"/><line x1=\"{:.2}\" y1=\"{whisker_top:.2}\" x2=\"{:.2}\" y2=\"{whisker_bottom:.2}\" stroke=\"#274765\"/><line x1=\"{:.2}\" y1=\"{whisker_top:.2}\" x2=\"{:.2}\" y2=\"{whisker_bottom:.2}\" stroke=\"#274765\"/><rect x=\"{:.2}\" y=\"{top:.2}\" width=\"{:.2}\" height=\"40\" fill=\"#b5d5ed\" stroke=\"#274765\"/><line x1=\"{:.2}\" y1=\"{top:.2}\" x2=\"{:.2}\" y2=\"{bottom:.2}\" stroke=\"#142d42\" stroke-width=\"3\"/>",
        escape(metric),
        values.len(),
        position(s.low),
        position(s.high),
        position(s.low),
        position(s.low),
        position(s.high),
        position(s.high),
        position(s.q1),
        (position(s.q3) - position(s.q1)).max(0.5),
        position(s.median),
        position(s.median)
    );
    for (i, (label, value)) in values.iter().enumerate() {
        let y = center + (i as f64 - (values.len() - 1) as f64 / 2.0) * 6.4;
        out.push_str(&format!("<circle class=\"replica-point\" cx=\"{:.2}\" cy=\"{y:.2}\" r=\"3\" fill=\"#c44d23\" fill-opacity=\"0.8\"><title>{}: {} {}</title></circle>", position(*value), escape(label), value, escape(metric_unit(metric))));
    }
    out.push_str(&format!("<line x1=\"70\" y1=\"{axis:.2}\" x2=\"450\" y2=\"{axis:.2}\" stroke=\"#274765\"/><text x=\"70\" y=\"{axis_labels:.2}\" font-size=\"11\">{}</text><text x=\"450\" y=\"{axis_labels:.2}\" text-anchor=\"end\" font-size=\"11\">{}</text><text x=\"260\" y=\"{unit_label:.2}\" text-anchor=\"middle\" font-size=\"11\">{}</text></svg>", axis_value(min), axis_value(max), escape(metric_unit(metric))));
    out
}

fn axis_value(value: f64) -> String {
    if value != 0.0 && (value.abs() < 0.01 || value.abs() >= 1e9) {
        format!("{value:.8e}")
    } else {
        value.to_string()
    }
}

/// Group matching trial and agent observations, preserving provider, model and dataset identity.
pub(super) fn html(rows: &[BenchmarkData]) -> String {
    let mut groups: BTreeMap<_, Vec<&BenchmarkData>> = BTreeMap::new();
    for row in rows {
        let dataset = row
            .task
            .as_ref()
            .and_then(|task| task.dataset.as_ref())
            .or_else(|| {
                row.selection
                    .as_ref()
                    .and_then(|selection| selection.dataset.as_ref())
            });
        let key = (
            &row.agent,
            &row.workload,
            trial_label(row),
            &row.provider,
            &row.model,
            serde_json::to_string(&dataset).expect("dataset identity serializes"),
            serde_json::to_string(&row.module).expect("module identity serializes"),
        );
        groups.entry(key).or_default().push(row);
    }
    let mut html = String::from(
        "<section id=\"replica-distributions\"><h2>Concurrency distributions</h2><p>Each point is one observed replica. Boxes show the first and third quartiles and median; whiskers extend to the most extreme observations within 1.5 × IQR. Missing measurements are omitted, not zero. Time-series plots summarize each replica by the mean of its available samples; sample timing remains available in comparison.json. Matching agent, trial, provider, model, dataset and workload module observations are grouped across configurations and batches. Filters show or hide entire distributions and do not recalculate their statistics.</p><nav aria-label=\"Agent and trial distributions\"><ul>",
    );
    for (config, group) in groups.values().enumerate() {
        let first = group[0];
        html.push_str(&format!(
            "<li><a href=\"#distribution-{config}\">{} · {} · {} · {} / {}</a></li>",
            escape(&first.agent),
            escape(&first.workload),
            escape(trial_label(first)),
            escape(&first.provider),
            escape(&first.model)
        ));
    }
    html.push_str("</ul></nav></section>");
    for (config, group) in groups.into_values().enumerate() {
        let first = group[0];
        let mut measurements: BTreeMap<(String, bool), Vec<(String, f64)>> = BTreeMap::new();
        for row in &group {
            for (name, value) in &row.metrics {
                if planning::graphable_metric(name) && value.is_finite() {
                    measurements
                        .entry((name.clone(), false))
                        .or_default()
                        .push((attempt_label(row), *value));
                }
            }
            for (name, series) in &row.series {
                let mut count = 0usize;
                let mut mean = 0.0;
                for value in series.values.iter().flatten().filter(|v| v.is_finite()) {
                    count += 1;
                    mean += (*value - mean) / count as f64;
                }
                if count > 0 && mean.is_finite() {
                    measurements
                        .entry((name.clone(), true))
                        .or_default()
                        .push((attempt_label(row), mean));
                }
            }
        }
        let members =
            serde_json::to_string(&group.iter().map(|row| &row.label).collect::<Vec<_>>())
                .expect("labels serialize");
        let concurrencies = group
            .iter()
            .map(|row| row.configured_concurrency.to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(", ");
        html.push_str(&format!("<section id=\"distribution-{config}\" class=\"distribution-group\" data-members=\"{}\"><h2>{} · {} · {}</h2><p>{} / {} · {} planned rows · configured concurrency {}</p><div class=\"charts\">", escape(&members), escape(&first.agent), escape(&first.workload), escape(trial_label(first)), escape(&first.provider), escape(&first.model), group.len(), concurrencies));
        if measurements.is_empty() {
            html.push_str("<p>No numeric replica measurements available.</p>");
        }
        for ((metric, series), values) in measurements {
            html.push_str(&format!("<figure class=\"distribution-chart\"><figcaption>{}{} · n={} of {}</figcaption>{}</figure>", escape(&metric), if series { " (per-replica series mean)" } else { "" }, values.len(), group.len(), svg(&metric, &values)));
        }
        html.push_str("</div></section>");
    }
    html
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tukey_whiskers_leave_outliers_as_points() {
        let s = summary(&[1.0, 2.0, 3.0, 4.0, 100.0]).unwrap();
        assert_eq!(
            s,
            Summary {
                q1: 2.0,
                median: 3.0,
                q3: 4.0,
                low: 1.0,
                high: 4.0
            }
        );
        let graph = svg(
            "elapsed_ms",
            &[("replica 1".into(), 1.0), ("replica 2".into(), 100.0)],
        );
        assert_eq!(graph.matches("class=\"replica-point\"").count(), 2);
        assert!(graph.contains("replica 2: 100 ms"));
    }
    #[test]
    fn empty_and_constant_distributions_are_safe() {
        assert!(summary(&[]).is_none());
        assert!(summary(&[f64::NAN]).is_none());
        let graph = svg("elapsed_ms", &[("one".into(), 0.0), ("two".into(), 0.0)]);
        assert_eq!(graph.matches("class=\"replica-point\"").count(), 2);
        assert!(!graph.contains("NaN"));
        assert_ne!(axis_value(1000.1), axis_value(1000.2));
    }

    #[test]
    fn sixty_four_equal_replicas_have_distinct_visible_points() {
        let values = (0..64)
            .map(|i| (format!("replica {i}"), 5.0))
            .collect::<Vec<_>>();
        let graph = svg("elapsed_ms", &values);
        let positions = graph
            .split("<circle ")
            .skip(1)
            .map(|circle| {
                circle
                    .split("cy=\"")
                    .nth(1)
                    .unwrap()
                    .split('"')
                    .next()
                    .unwrap()
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(positions.len(), 64);
    }
}
