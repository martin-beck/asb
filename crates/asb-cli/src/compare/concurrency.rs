//! Derived report for comparing equivalent benchmark observations by replica concurrency.

use super::*;

pub(super) const REPORT_FILE: &str = "concurrency-report.html";
pub(super) const AGENT_REPORT_FILE: &str = "agent-concurrency-report.html";

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct GroupKey {
    agent: String,
    provider: String,
    model: String,
    workload: String,
    trial: String,
    dataset: String,
    module: String,
}

impl GroupKey {
    fn from_row(row: &BenchmarkData) -> Self {
        let dataset = row
            .task
            .as_ref()
            .and_then(|task| task.dataset.as_ref())
            .or_else(|| {
                row.selection
                    .as_ref()
                    .and_then(|selection| selection.dataset.as_ref())
            });
        Self {
            agent: row.agent.clone(),
            provider: row.provider.clone(),
            model: model_display(row),
            workload: row.workload.clone(),
            trial: trial_label(row).into(),
            dataset: serde_json::to_string(&dataset).expect("dataset identity serializes"),
            module: serde_json::to_string(&row.module).expect("module identity serializes"),
        }
    }

    fn display(&self) -> String {
        format!(
            "{} · {} · {} / {} · {}",
            self.agent, self.workload, self.trial, self.provider, self.model
        )
    }

    fn identity(&self) -> String {
        serde_json::to_string(self).expect("concurrency group serializes")
    }
}

impl Serialize for GroupKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        (
            &self.agent,
            &self.provider,
            &self.model,
            &self.workload,
            &self.trial,
            &self.dataset,
            &self.module,
        )
            .serialize(serializer)
    }
}

#[derive(Clone)]
struct Observation {
    label: String,
    value: f64,
}

#[derive(Clone)]
struct GraphPlan {
    group: GroupKey,
    metric: String,
    is_series_mean: bool,
    filename: String,
    values: BTreeMap<usize, Vec<Observation>>,
}

/// Identity for an apples-to-apples comparison between agents.  The agent is
/// deliberately excluded, while every input that can materially change a
/// workload observation remains part of the key.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct AgentGroupKey {
    provider: String,
    model: String,
    workload: String,
    trial: String,
    dataset: String,
    module: String,
}

impl AgentGroupKey {
    fn from_row(row: &BenchmarkData) -> Self {
        let group = GroupKey::from_row(row);
        Self {
            provider: group.provider,
            model: group.model,
            workload: group.workload,
            trial: group.trial,
            dataset: group.dataset,
            module: group.module,
        }
    }

    fn display(&self) -> String {
        format!(
            "{} · {} / {} · {}",
            self.workload, self.trial, self.provider, self.model
        )
    }

    fn identity(&self) -> String {
        serde_json::to_string(self).expect("agent concurrency group serializes")
    }
}

impl Serialize for AgentGroupKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        (
            &self.provider,
            &self.model,
            &self.workload,
            &self.trial,
            &self.dataset,
            &self.module,
        )
            .serialize(serializer)
    }
}

#[derive(Clone)]
struct AgentGraphPlan {
    group: AgentGroupKey,
    metric: String,
    is_series_mean: bool,
    filename: String,
    values: BTreeMap<String, BTreeMap<usize, Vec<Observation>>>,
}

/// Reserve the derived report and every graph before publication starts. This
/// keeps the report generation atomic with the primary comparison report.
pub(super) fn report_files(prepared: &PreparedReport) -> Vec<String> {
    let plans = graph_plans(&prepared.data.rows);
    let agent_plans = agent_graph_plans(&prepared.data.rows);
    std::iter::once(REPORT_FILE.into())
        .chain(plans.into_iter().map(|plan| plan.filename))
        .chain(std::iter::once(AGENT_REPORT_FILE.into()))
        .chain(agent_plans.into_iter().map(|plan| plan.filename))
        .collect()
}

pub(super) fn render(stage: &Path, prepared: &PreparedReport) -> Result<(), Error> {
    let plans = graph_plans(&prepared.data.rows);
    for plan in &plans {
        write_concurrency_svg(&stage.join(&plan.filename), plan)?;
    }

    let mut by_group = BTreeMap::<GroupKey, Vec<&GraphPlan>>::new();
    for plan in &plans {
        by_group.entry(plan.group.clone()).or_default().push(plan);
    }

    let mut html = String::from(HTML_HEAD);
    html = html.replacen(
        "</style></head>",
        &format!(
            "{}\n{}\n{}\n{}\n{}\n</style></head>",
            INTERACTIVE_STYLE, HIGHLIGHT_STYLE, MODAL_STYLE, INLINE_SVG_STYLE, JSON_DETAILS_STYLE
        ),
        1,
    );
    html.push_str("<main class=\"shell\">");
    html.push_str("<header class=\"hero\"><p class=\"eyebrow\">ASB CONCURRENCY REPORT</p><h1>Concurrency scaling comparison</h1><p>Compare the same agent, trial, provider, model, workload module, and dataset across configured replica counts without rerunning a benchmark.</p></header>");
    html.push_str("<section class=\"explain\"><strong>How to read this report</strong><p>Every graph has configured concurrency on the x-axis and the original metric unit on the y-axis. Each faint point is an individual replica; the connected marker is that concurrency's median. Scalar measurements retain their observed value. Timestamped collector series contribute one per-replica mean, the same summary used by the existing replica-distribution view. Missing values are not zero-filled.</p></section>");
    if plans.is_empty() {
        html.push_str("<section><h2>No comparable concurrency observations</h2><p>ASB found no equivalent agent/trial group with measurements at two or more configured concurrency values. Run the same configuration at another concurrency, then rerun <code>asb compare</code>.</p></section>");
    } else {
        html.push_str("<section class=\"chart-picker\"><div class=\"section-title\"><h2>Chart highlights</h2><p class=\"muted\">Highlight a graph here or from its own toolbar.</p></div><div class=\"table-wrap\"><table><thead><tr><th>Chart</th><th>Highlight</th></tr></thead><tbody>");
        for plan in &plans {
            html.push_str(&format!("<tr data-highlight-row=\"{}\"><td>{} · {}</td><td><label class=\"highlight-toggle\"><input type=\"checkbox\" data-highlight-graph=\"{}\"><span>Highlight chart</span></label></td></tr>", escape(&plan.filename), escape(&plan.group.display()), escape(&metric_display(plan)), escape(&plan.filename)));
        }
        html.push_str("</tbody></table></div></section>");
        for (index, (group, group_plans)) in by_group.into_iter().enumerate() {
            html.push_str(&format!("<section id=\"concurrency-group-{index}\"><div class=\"section-title\"><h2>{}</h2><p class=\"muted\">Comparable observations across configured concurrencies: {}.</p></div><div class=\"charts\">", escape(&group.display()), group_plans.iter().flat_map(|plan| plan.values.keys()).copied().collect::<BTreeSet<_>>().into_iter().map(|value| value.to_string()).collect::<Vec<_>>().join(", ")));
            for plan in group_plans {
                html.push_str(&chart_figure(stage, &plan.filename)?);
            }
            html.push_str("</div></section>");
        }
    }
    html.push_str("<footer>Generated locally by <code>asb compare</code>. No provider or agent was contacted.</footer></main>");
    html.push_str(INTERACTIVE_SCRIPT);
    html.push_str("</body></html>");
    fs::write(stage.join(REPORT_FILE), html).map_err(|error| {
        Error::Run(format!(
            "cannot write concurrency comparison HTML report: {error}"
        ))
    })?;
    render_agent_report(stage, &prepared.data.rows)
}

fn graph_plans(rows: &[BenchmarkData]) -> Vec<GraphPlan> {
    type MetricValues = BTreeMap<(String, bool), BTreeMap<usize, Vec<Observation>>>;
    let mut groups = BTreeMap::<GroupKey, MetricValues>::new();
    for row in rows {
        let group = GroupKey::from_row(row);
        let metrics = groups.entry(group).or_default();
        for (metric, value) in &row.metrics {
            if planning::graphable_metric(metric) && value.is_finite() {
                metrics
                    .entry((metric.clone(), false))
                    .or_default()
                    .entry(row.configured_concurrency)
                    .or_default()
                    .push(Observation {
                        label: attempt_label(row),
                        value: *value,
                    });
            }
        }
        for (metric, series) in &row.series {
            let mut count = 0usize;
            let mut mean = 0.0;
            for value in series
                .values
                .iter()
                .flatten()
                .filter(|value| value.is_finite())
            {
                count += 1;
                mean += (*value - mean) / count as f64;
            }
            if count > 0 && mean.is_finite() && planning::graphable_metric(metric) {
                metrics
                    .entry((metric.clone(), true))
                    .or_default()
                    .entry(row.configured_concurrency)
                    .or_default()
                    .push(Observation {
                        label: format!("{} · series mean", attempt_label(row)),
                        value: mean,
                    });
            }
        }
    }

    let mut names = GraphNames::default();
    let mut plans = Vec::new();
    for (group, metrics) in groups {
        for ((metric, is_series_mean), values) in metrics {
            if values.len() < 2 {
                continue;
            }
            let identity = format!("{}:{metric}:series-mean={is_series_mean}", group.identity());
            plans.push(GraphPlan {
                group: group.clone(),
                metric,
                is_series_mean,
                filename: names.allocate("concurrency", &identity),
                values,
            });
        }
    }
    plans
}

fn agent_graph_plans(rows: &[BenchmarkData]) -> Vec<AgentGraphPlan> {
    type MetricValues =
        BTreeMap<(String, bool), BTreeMap<String, BTreeMap<usize, Vec<Observation>>>>;
    let mut groups = BTreeMap::<AgentGroupKey, MetricValues>::new();
    for row in rows {
        let group = AgentGroupKey::from_row(row);
        let metrics = groups.entry(group).or_default();
        for (metric, value) in &row.metrics {
            if planning::graphable_metric(metric) && value.is_finite() {
                metrics
                    .entry((metric.clone(), false))
                    .or_default()
                    .entry(row.agent.clone())
                    .or_default()
                    .entry(row.configured_concurrency)
                    .or_default()
                    .push(Observation {
                        label: attempt_label(row),
                        value: *value,
                    });
            }
        }
        for (metric, series) in &row.series {
            let mut count = 0usize;
            let mut mean = 0.0;
            for value in series
                .values
                .iter()
                .flatten()
                .filter(|value| value.is_finite())
            {
                count += 1;
                mean += (*value - mean) / count as f64;
            }
            if count > 0 && mean.is_finite() && planning::graphable_metric(metric) {
                metrics
                    .entry((metric.clone(), true))
                    .or_default()
                    .entry(row.agent.clone())
                    .or_default()
                    .entry(row.configured_concurrency)
                    .or_default()
                    .push(Observation {
                        label: format!("{} · series mean", attempt_label(row)),
                        value: mean,
                    });
            }
        }
    }

    let mut names = GraphNames::default();
    let mut plans = Vec::new();
    for (group, metrics) in groups {
        for ((metric, is_series_mean), values) in metrics {
            let concurrencies = values
                .values()
                .flat_map(|by_concurrency| by_concurrency.keys())
                .collect::<BTreeSet<_>>();
            if values.len() < 2 || concurrencies.len() < 2 {
                continue;
            }
            let identity = format!("{}:{metric}:series-mean={is_series_mean}", group.identity());
            plans.push(AgentGraphPlan {
                group: group.clone(),
                metric,
                is_series_mean,
                filename: names.allocate("agent-concurrency", &identity),
                values,
            });
        }
    }
    plans
}

fn render_agent_report(stage: &Path, rows: &[BenchmarkData]) -> Result<(), Error> {
    let plans = agent_graph_plans(rows);
    for plan in &plans {
        write_agent_concurrency_svg(&stage.join(&plan.filename), plan)?;
    }

    let mut by_group = BTreeMap::<AgentGroupKey, Vec<&AgentGraphPlan>>::new();
    for plan in &plans {
        by_group.entry(plan.group.clone()).or_default().push(plan);
    }

    let mut html = String::from(HTML_HEAD);
    html = html.replacen(
        "</style></head>",
        &format!(
            "{}\n{}\n{}\n{}\n{}\n</style></head>",
            INTERACTIVE_STYLE, HIGHLIGHT_STYLE, MODAL_STYLE, INLINE_SVG_STYLE, JSON_DETAILS_STYLE
        ),
        1,
    );
    html.push_str("<main class=\"shell\">");
    html.push_str("<header class=\"hero\"><p class=\"eyebrow\">ASB AGENT CONCURRENCY REPORT</p><h1>Agent concurrency comparison</h1><p>Compare agents on the identical trial, provider, model, workload module, and dataset across configured replica counts without rerunning a benchmark.</p></header>");
    html.push_str("<section class=\"explain\"><strong>How to read this report</strong><p>Each color is one agent. The x-axis is configured concurrency and the y-axis retains the original metric unit. Faint points are individual replicas; connected markers are per-agent medians at each configured concurrency. Scalar measurements retain their observed value. Timestamped collector series contribute one per-replica mean. Missing values are not zero-filled.</p></section>");
    if plans.is_empty() {
        html.push_str("<section><h2>No cross-agent concurrency observations</h2><p>ASB found no equivalent trial/provider/model group with at least two agents and two configured concurrency values. Run matching configurations, then rerun <code>asb compare</code>.</p></section>");
    } else {
        html.push_str("<section class=\"chart-picker\"><div class=\"section-title\"><h2>Chart highlights</h2><p class=\"muted\">Highlight a graph here or from its own toolbar.</p></div><div class=\"table-wrap\"><table><thead><tr><th>Chart</th><th>Highlight</th></tr></thead><tbody>");
        for plan in &plans {
            html.push_str(&format!("<tr data-highlight-row=\"{}\"><td>{} · {}</td><td><label class=\"highlight-toggle\"><input type=\"checkbox\" data-highlight-graph=\"{}\"><span>Highlight chart</span></label></td></tr>", escape(&plan.filename), escape(&plan.group.display()), escape(&agent_metric_display(plan)), escape(&plan.filename)));
        }
        html.push_str("</tbody></table></div></section>");
        for (index, (group, group_plans)) in by_group.into_iter().enumerate() {
            let agents = group_plans
                .iter()
                .flat_map(|plan| plan.values.keys())
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ");
            let concurrencies = group_plans
                .iter()
                .flat_map(|plan| plan.values.values().flat_map(|values| values.keys()))
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            html.push_str(&format!("<section id=\"agent-concurrency-group-{index}\"><div class=\"section-title\"><h2>{}</h2><p class=\"muted\">Agents: {}. Comparable configured concurrencies: {}.</p></div><div class=\"charts\">", escape(&group.display()), escape(&agents), concurrencies));
            for plan in group_plans {
                html.push_str(&chart_figure(stage, &plan.filename)?);
            }
            html.push_str("</div></section>");
        }
    }
    html.push_str("<footer>Generated locally by <code>asb compare</code>. No provider or agent was contacted.</footer></main>");
    html.push_str(INTERACTIVE_SCRIPT);
    html.push_str("</body></html>");
    fs::write(stage.join(AGENT_REPORT_FILE), html).map_err(|error| {
        Error::Run(format!(
            "cannot write agent concurrency comparison HTML report: {error}"
        ))
    })
}

fn agent_metric_display(plan: &AgentGraphPlan) -> String {
    if plan.is_series_mean {
        format!("{} (per-replica series mean)", plan.metric)
    } else {
        plan.metric.clone()
    }
}

fn metric_display(plan: &GraphPlan) -> String {
    if plan.is_series_mean {
        format!("{} (per-replica series mean)", plan.metric)
    } else {
        plan.metric.clone()
    }
}

fn write_concurrency_svg(path: &Path, plan: &GraphPlan) -> Result<(), Error> {
    const WIDTH: f64 = 1_200.0;
    const LEFT: f64 = 100.0;
    const RIGHT: f64 = 1_160.0;
    const TOP: f64 = 78.0;
    const BOTTOM: f64 = 390.0;
    let concurrencies = plan.values.keys().copied().collect::<Vec<_>>();
    let values = plan
        .values
        .values()
        .flatten()
        .map(|observation| observation.value)
        .collect::<Vec<_>>();
    let min = values.iter().copied().fold(0.0_f64, f64::min);
    let mut max = values.iter().copied().fold(0.0_f64, f64::max);
    if max <= min {
        max = min + 1.0;
    }
    let min_concurrency = *concurrencies.first().expect("graph has two concurrencies") as f64;
    let max_concurrency = *concurrencies.last().expect("graph has two concurrencies") as f64;
    let x_for = |concurrency: usize| {
        let fraction = if max_concurrency > min_concurrency {
            (concurrency as f64 - min_concurrency) / (max_concurrency - min_concurrency)
        } else {
            0.0
        };
        LEFT + fraction * (RIGHT - LEFT)
    };
    let title = metric_display(plan);
    let mut svg = svg_header(WIDTH, 470.0, &title);
    svg.push_str("<text x=\"22\" y=\"48\" font-size=\"11\">Faint points are individual replicas; the connected dark line is the median at each configured concurrency.</text>");
    append_y_axis(&mut svg, LEFT, TOP, BOTTOM, RIGHT, (min, max), &plan.metric);
    let baseline = BOTTOM - domain_fraction(0.0, min, max) * (BOTTOM - TOP);
    if min < 0.0 {
        svg.push_str(&format!("<line class=\"zero-baseline\" x1=\"{LEFT}\" y1=\"{baseline:.1}\" x2=\"{RIGHT}\" y2=\"{baseline:.1}\" stroke=\"black\"/>"));
    }
    for concurrency in &concurrencies {
        let x = x_for(*concurrency);
        svg.push_str(&format!("<line class=\"x-axis-tick\" x1=\"{x:.1}\" y1=\"{TOP}\" x2=\"{x:.1}\" y2=\"{BOTTOM}\" stroke=\"#e3e9ef\"/><text class=\"x-axis-value\" x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"11\">{concurrency}</text>", BOTTOM + 22.0));
    }
    svg.push_str(&format!("<text class=\"x-axis-label\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"12\">configured concurrency (replicas)</text>", (LEFT + RIGHT) / 2.0, BOTTOM + 43.0));

    let mut medians = Vec::new();
    for (concurrency, observations) in &plan.values {
        let x = x_for(*concurrency);
        let count = observations.len();
        for (index, observation) in observations.iter().enumerate() {
            let offset = if count <= 1 {
                0.0
            } else {
                (index as f64 / (count - 1) as f64 - 0.5) * 12.0
            };
            let y = BOTTOM - domain_fraction(observation.value, min, max) * (BOTTOM - TOP);
            let label = format!("concurrency {concurrency} · {}", observation.label);
            svg.push_str(&format!("<circle class=\"series-point concurrency-observation\" data-series-index=\"0\" data-label=\"{}\" data-value=\"{}\" data-concurrency=\"{}\" cx=\"{:.1}\" cy=\"{y:.1}\" r=\"4\" fill=\"#7aa6d8\" fill-opacity=\"0.72\" stroke=\"#2d5f91\"><title>{}: {} {}</title></circle>", escape(&label), observation.value, concurrency, x + offset, escape(&label), format_tick(observation.value), escape(metric_unit(&plan.metric))));
        }
        let median = median(observations.iter().map(|observation| observation.value));
        let y = BOTTOM - domain_fraction(median, min, max) * (BOTTOM - TOP);
        medians.push((x, y, *concurrency, observations.len(), median));
    }
    svg.push_str(&format!("<polyline class=\"series concurrency-median\" data-series-index=\"0\" data-label=\"median\" points=\"{}\" fill=\"none\" stroke=\"#12344d\" stroke-width=\"3\"/>", medians.iter().map(|(x, y, _, _, _)| format!("{x:.1},{y:.1}")).collect::<Vec<_>>().join(" ")));
    for (x, y, concurrency, count, value) in medians {
        let label = format!("concurrency {concurrency} median across {count} replica(s)");
        svg.push_str(&format!("<circle class=\"series-point concurrency-median\" data-series-index=\"0\" data-label=\"{}\" data-value=\"{}\" data-concurrency=\"{}\" cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"6\" fill=\"#12344d\" stroke=\"#fff\" stroke-width=\"2\"><title>{}: {} {}</title></circle>", escape(&label), value, concurrency, escape(&label), format_tick(value), escape(metric_unit(&plan.metric))));
    }
    svg.push_str("</svg>\n");
    fs::write(path, svg)
        .map_err(|error| Error::Run(format!("cannot write {}: {error}", path.display())))
}

fn write_agent_concurrency_svg(path: &Path, plan: &AgentGraphPlan) -> Result<(), Error> {
    const WIDTH: f64 = 1_200.0;
    const LEFT: f64 = 100.0;
    const RIGHT: f64 = 1_160.0;
    const TOP: f64 = 110.0;
    const BOTTOM: f64 = 422.0;
    const COLORS: [&str; 10] = [
        "#1769aa", "#bf360c", "#2e7d32", "#6a1b9a", "#00838f", "#ad1457", "#5d4037", "#455a64",
        "#827717", "#283593",
    ];

    let concurrencies = plan
        .values
        .values()
        .flat_map(|values| values.keys())
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let values = plan
        .values
        .values()
        .flat_map(|by_concurrency| by_concurrency.values())
        .flatten()
        .map(|observation| observation.value)
        .collect::<Vec<_>>();
    let min = values.iter().copied().fold(0.0_f64, f64::min);
    let mut max = values.iter().copied().fold(0.0_f64, f64::max);
    if max <= min {
        max = min + 1.0;
    }
    let min_concurrency = *concurrencies.first().expect("graph has two concurrencies") as f64;
    let max_concurrency = *concurrencies.last().expect("graph has two concurrencies") as f64;
    let x_for = |concurrency: usize| {
        let fraction = if max_concurrency > min_concurrency {
            (concurrency as f64 - min_concurrency) / (max_concurrency - min_concurrency)
        } else {
            0.0
        };
        LEFT + fraction * (RIGHT - LEFT)
    };
    let title = agent_metric_display(plan);
    let mut svg = svg_header(WIDTH, 502.0, &title);
    svg.push_str("<text x=\"22\" y=\"48\" font-size=\"11\">Each color is one agent. Faint points are individual replicas; connected markers are per-agent medians.</text>");
    append_y_axis(&mut svg, LEFT, TOP, BOTTOM, RIGHT, (min, max), &plan.metric);
    let baseline = BOTTOM - domain_fraction(0.0, min, max) * (BOTTOM - TOP);
    if min < 0.0 {
        svg.push_str(&format!("<line class=\"zero-baseline\" x1=\"{LEFT}\" y1=\"{baseline:.1}\" x2=\"{RIGHT}\" y2=\"{baseline:.1}\" stroke=\"black\"/>"));
    }
    for concurrency in &concurrencies {
        let x = x_for(*concurrency);
        svg.push_str(&format!("<line class=\"x-axis-tick\" x1=\"{x:.1}\" y1=\"{TOP}\" x2=\"{x:.1}\" y2=\"{BOTTOM}\" stroke=\"#e3e9ef\"/><text class=\"x-axis-value\" x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"11\">{concurrency}</text>", BOTTOM + 22.0));
    }
    svg.push_str(&format!("<text class=\"x-axis-label\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"12\">configured concurrency (replicas)</text>", (LEFT + RIGHT) / 2.0, BOTTOM + 43.0));

    for (series_index, (agent, values)) in plan.values.iter().enumerate() {
        let color = COLORS[series_index % COLORS.len()];
        let mut medians = Vec::new();
        for (concurrency, observations) in values {
            let x = x_for(*concurrency);
            let count = observations.len();
            for (index, observation) in observations.iter().enumerate() {
                let offset = if count <= 1 {
                    0.0
                } else {
                    (index as f64 / (count - 1) as f64 - 0.5) * 10.0
                };
                let y = BOTTOM - domain_fraction(observation.value, min, max) * (BOTTOM - TOP);
                let label = format!(
                    "{agent} · concurrency {concurrency} · {}",
                    observation.label
                );
                svg.push_str(&format!("<circle class=\"series-point concurrency-observation\" data-series-index=\"{series_index}\" data-label=\"{}\" data-value=\"{}\" data-concurrency=\"{}\" cx=\"{:.1}\" cy=\"{y:.1}\" r=\"4\" fill=\"{color}\" fill-opacity=\"0.42\" stroke=\"{color}\"><title>{}: {} {}</title></circle>", escape(&label), observation.value, concurrency, x + offset, escape(&label), format_tick(observation.value), escape(metric_unit(&plan.metric))));
            }
            let value = median(observations.iter().map(|observation| observation.value));
            let y = BOTTOM - domain_fraction(value, min, max) * (BOTTOM - TOP);
            medians.push((x, y, *concurrency, observations.len(), value));
        }
        svg.push_str(&format!("<polyline class=\"series concurrency-median\" data-series-index=\"{series_index}\" data-label=\"{}\" points=\"{}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"3\"/>", escape(agent), medians.iter().map(|(x, y, _, _, _)| format!("{x:.1},{y:.1}")).collect::<Vec<_>>().join(" ")));
        for (x, y, concurrency, count, value) in medians {
            let label =
                format!("{agent} · concurrency {concurrency} median across {count} replica(s)");
            svg.push_str(&format!("<circle class=\"series-point concurrency-median\" data-series-index=\"{series_index}\" data-label=\"{}\" data-value=\"{}\" data-concurrency=\"{}\" cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"6\" fill=\"{color}\" stroke=\"#fff\" stroke-width=\"2\"><title>{}: {} {}</title></circle>", escape(&label), value, concurrency, escape(&label), format_tick(value), escape(metric_unit(&plan.metric))));
        }
        let legend_x = 24.0 + (series_index % 4) as f64 * 286.0;
        let legend_y = 72.0 + (series_index / 4) as f64 * 20.0;
        svg.push_str(&format!("<rect class=\"legend-swatch\" x=\"{legend_x:.1}\" y=\"{:.1}\" width=\"10\" height=\"10\" rx=\"2\" fill=\"{color}\"/><text class=\"legend-item\" data-series-index=\"{series_index}\" x=\"{:.1}\" y=\"{:.1}\" font-size=\"12\" fill=\"{color}\" aria-label=\"{}\">{}</text>", legend_y - 10.0, legend_x + 16.0, legend_y, escape(agent), escape(agent)));
    }
    svg.push_str("</svg>\n");
    fs::write(path, svg)
        .map_err(|error| Error::Run(format!("cannot write {}: {error}", path.display())))
}

fn median(values: impl Iterator<Item = f64>) -> f64 {
    let mut values = values.collect::<Vec<_>>();
    values.sort_by(f64::total_cmp);
    let index = (values.len() - 1) / 2;
    values[index]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(agent: &str, concurrency: usize, metric: f64) -> BenchmarkData {
        BenchmarkData {
            approximate_replay: false,
            replay_selection: None,
            observation: Default::default(),
            native_replay_consumed: None,
            id: RowId {
                configuration: 0,
                batch: None,
                replica: 0,
            },
            execution: None,
            planned_normalization_policy: None,
            normalization_observation: None,
            planning: None,
            effective_budgets: None,
            legacy_evaluation_provenance: None,
            execution_plan_fingerprint: None,
            module: None,
            benchmark_outcome: None,
            result_source: ResultSource::Measurements,
            reconciled: true,
            replica_index: Some(0),
            execution_cohort: None,
            label: format!("{agent}-{concurrency}"),
            directory: PathBuf::from("fixture"),
            agent: agent.into(),
            workload: "bug-fix".into(),
            configured_concurrency: concurrency,
            task: None,
            selection: None,
            evaluation_completed: true,
            workload_evaluation: "fixture".into(),
            provider: "openrouter".into(),
            thinking_effort: None,
            model: "fixture".into(),
            metrics: BTreeMap::from([("execution_seconds".into(), metric)]),
            series: BTreeMap::new(),
            host_metadata: None,
            components: None,
            collector_documents: BTreeMap::new(),
            status: None,
            artifacts: Vec::new(),
            artifact_sources: BTreeMap::new(),
        }
    }

    #[test]
    fn median_uses_the_lower_middle_observation() {
        assert_eq!(median([4.0, 1.0, 3.0, 2.0].into_iter()), 2.0);
    }

    #[test]
    fn agent_plans_compare_matching_agents_across_concurrencies() {
        let rows = vec![
            row("opencode", 32, 10.0),
            row("opencode", 64, 18.0),
            row("pi", 32, 12.0),
            row("pi", 64, 20.0),
        ];

        let plans = agent_graph_plans(&rows);

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].values.len(), 2);
        assert_eq!(plans[0].values["opencode"].len(), 2);
        assert_eq!(plans[0].values["pi"].len(), 2);
        assert!(plans[0].filename.starts_with("agent-concurrency-"));
    }

    #[test]
    fn agent_plans_require_multiple_agents_and_concurrencies() {
        assert!(agent_graph_plans(&[row("pi", 32, 10.0), row("pi", 64, 20.0)]).is_empty());
        assert!(agent_graph_plans(&[row("pi", 32, 10.0), row("opencode", 32, 20.0)]).is_empty());
    }
}
