//! Derived report for comparing equivalent benchmark observations by replica concurrency.

use super::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum FlameStatistic {
    Mean,
    Median,
    P95,
}

impl FlameStatistic {
    const ALL: [Self; 3] = [Self::Mean, Self::Median, Self::P95];

    fn label(self) -> &'static str {
        match self {
            Self::Mean => "mean",
            Self::Median => "median",
            Self::P95 => "p95",
        }
    }

    /// Compute a statistic over observed values plus implicit zero samples.
    ///
    /// Folded stacks are sparse: a stack absent from a replica contributes a
    /// zero, but retaining every one of those zeros for every stack makes a
    /// large, heterogeneous sweep quadratic in memory.  Values are strictly
    /// positive when parsed, so implicit zeros sort before all observations.
    fn sparse_value(self, observed: &mut [f64], sample_count: usize) -> f64 {
        debug_assert!(sample_count >= observed.len());
        debug_assert!(sample_count > 0);
        match self {
            Self::Mean => observed.iter().sum::<f64>() / sample_count as f64,
            Self::Median | Self::P95 => {
                observed.sort_by(f64::total_cmp);
                let rank = match self {
                    Self::Median => (sample_count - 1) / 2,
                    Self::P95 => (sample_count * 95).div_ceil(100).saturating_sub(1),
                    Self::Mean => unreachable!("mean does not use a rank"),
                };
                let zero_count = sample_count - observed.len();
                if rank < zero_count {
                    0.0
                } else {
                    observed[rank - zero_count]
                }
            }
        }
    }
}

#[derive(Clone)]
struct FlamegraphProfile {
    group: GroupKey,
    concurrency: usize,
    statistic: FlameStatistic,
    folded: String,
    svg: String,
    inputs: Vec<PathBuf>,
    source_svg: Option<PathBuf>,
}

#[derive(Clone)]
struct FlamegraphDiff {
    group: GroupKey,
    statistic: FlameStatistic,
    low_concurrency: usize,
    high_concurrency: usize,
    low_folded: String,
    high_folded: String,
    folded: String,
    svg: String,
}

#[derive(Default)]
struct FlamegraphPlans {
    profiles: Vec<FlamegraphProfile>,
    diffs: Vec<FlamegraphDiff>,
}

/// Exact, project-local Brendan Gregg FlameGraph programs. Aggregate diffs use
/// their standard three-column `difffolded.pl` output so the SVG retains the
/// canonical width, color, hover text, and search behavior.
struct FlamegraphTools {
    diff: PathBuf,
    render: PathBuf,
}

impl FlamegraphTools {
    fn for_request(request: &source::Request) -> Result<Self, Error> {
        let output_root = request
            .comparison_dir
            .parent()
            .and_then(Path::parent)
            .unwrap_or(&request.parent);
        let mut missing = None;
        for start in [&request.parent, output_root] {
            let Ok((root, project)) = crate::project::load(start) else {
                continue;
            };
            let bin = crate::project::resolve(&root, &project.tools_dir).join("flamegraph/bin");
            let diff = bin.join("difffolded.pl");
            let render = bin.join("flamegraph");
            if crate::executables::is_executable(&diff)
                && crate::executables::is_executable(&render)
            {
                return Ok(Self { diff, render });
            }
            missing = Some((diff, render));
        }
        if let Some((diff, render)) = missing {
            return Err(Error::Input(format!(
                "aggregate FlameGraph diffs require {} and {}; install or repair them with `asb tool install flamegraph`",
                diff.display(),
                render.display()
            )));
        }
        Err(Error::Input(
            "aggregate FlameGraph diffs require an initialized project with the project-local FlameGraph tools; run `asb tool install flamegraph` and retry"
                .into(),
        ))
    }
}

/// Reserve the derived report and every graph before publication starts. This
/// keeps the report generation atomic with the primary comparison report.
pub(super) fn report_files(prepared: &PreparedReport) -> Vec<String> {
    let plans = graph_plans(&prepared.data.rows);
    let agent_plans = agent_graph_plans(&prepared.data.rows);
    let flamegraphs = flamegraph_plans(&prepared.data.rows);
    std::iter::once(REPORT_FILE.into())
        .chain(plans.into_iter().map(|plan| plan.filename))
        .chain(std::iter::once(AGENT_REPORT_FILE.into()))
        .chain(agent_plans.into_iter().map(|plan| plan.filename))
        .chain(
            flamegraphs
                .profiles
                .into_iter()
                .flat_map(|plan| [plan.folded, plan.svg]),
        )
        .chain(
            flamegraphs
                .diffs
                .into_iter()
                .flat_map(|plan| [plan.folded, plan.svg]),
        )
        .collect()
}

pub(super) fn render(stage: &Path, prepared: &PreparedReport) -> Result<(), Error> {
    let plans = graph_plans(&prepared.data.rows);
    let flamegraphs = flamegraph_plans(&prepared.data.rows);
    for plan in &plans {
        write_concurrency_svg(&stage.join(&plan.filename), plan)?;
    }
    let tools = (!flamegraphs.diffs.is_empty())
        .then(|| FlamegraphTools::for_request(&prepared.data.request))
        .transpose()?;
    render_flamegraphs(stage, &flamegraphs, tools.as_ref())?;

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
    html.push_str("<section class=\"explain\"><strong>How to read this report</strong><p>Every graph has configured concurrency on the x-axis and a human-scaled metric unit on the y-axis. Each faint point is an individual replica; the connected marker is that concurrency's median. Scalar measurements retain their observed value. Timestamped collector series contribute one per-replica mean, the same summary used by the existing replica-distribution view. Missing values are not zero-filled.</p></section>");
    html.push_str(&collector_commands_html(&prepared.data.rows));
    if plans.is_empty() {
        html.push_str("<section><h2>No comparable concurrency observations</h2><p>ASB found no equivalent agent/trial group with measurements at two or more configured concurrency values. Run the same configuration at another concurrency, then rerun <code>asb compare</code>.</p></section>");
    } else {
        let mut picker = String::from(
            "<div class=\"table-wrap\"><table><thead><tr><th>Chart</th><th>Highlight</th></tr></thead><tbody>",
        );
        for plan in &plans {
            let title = graph_presentation(stage, &plan.filename)?.title;
            picker.push_str(&chart_picker_row(&plan.filename, &title));
        }
        picker.push_str("</tbody></table></div>");
        html.push_str("<section class=\"chart-picker\"><div class=\"section-title\"><h2>Chart highlights</h2><p class=\"muted\">Highlight a graph here or from its own toolbar.</p></div>");
        html.push_str(&folded_table_html("Chart highlights", picker));
        html.push_str("</section>");
        for (index, (group, group_plans)) in by_group.into_iter().enumerate() {
            let graphs = group_plans
                .iter()
                .map(|plan| plan.filename.clone())
                .collect::<Vec<_>>();
            html.push_str(&format!("<section id=\"concurrency-group-{index}\"><div class=\"section-title\"><h2>{}</h2><p class=\"muted\">Comparable observations across configured concurrencies: {}.</p></div>", escape(&group.display()), group_plans.iter().flat_map(|plan| plan.values.keys()).copied().collect::<BTreeSet<_>>().into_iter().map(|value| value.to_string()).collect::<Vec<_>>().join(", ")));
            html.push_str(&collector_chart_groups_html(
                stage,
                &graphs,
                std::slice::from_ref(&group.agent),
            )?);
            html.push_str("</section>");
        }
    }
    html.push_str(&derived_flamegraph_section(stage, &flamegraphs)?);
    html.push_str("<footer>Generated locally by <code>asb compare</code>. No provider or agent was contacted.</footer></main>");
    html.push_str(INTERACTIVE_SCRIPT);
    html.push_str("</body></html>");
    fs::write(stage.join(REPORT_FILE), html).map_err(|error| {
        Error::Run(format!(
            "cannot write concurrency comparison HTML report: {error}"
        ))
    })?;
    render_agent_report(stage, &prepared.data.rows, &flamegraphs)
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

fn render_agent_report(
    stage: &Path,
    rows: &[BenchmarkData],
    flamegraphs: &FlamegraphPlans,
) -> Result<(), Error> {
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
    html.push_str("<section class=\"explain\"><strong>How to read this report</strong><p>Each color is one agent. The x-axis is configured concurrency and the y-axis uses a human-scaled metric unit. Faint points are individual replicas; connected markers are per-agent medians at each configured concurrency. Scalar measurements retain their observed value. Timestamped collector series contribute one per-replica mean. Missing values are not zero-filled.</p></section>");
    html.push_str(&collector_commands_html(rows));
    if plans.is_empty() {
        html.push_str("<section><h2>No cross-agent concurrency observations</h2><p>ASB found no equivalent trial/provider/model group with at least two agents and two configured concurrency values. Run matching configurations, then rerun <code>asb compare</code>.</p></section>");
    } else {
        let mut picker = String::from(
            "<div class=\"table-wrap\"><table><thead><tr><th>Chart</th><th>Highlight</th></tr></thead><tbody>",
        );
        for plan in &plans {
            let title = graph_presentation(stage, &plan.filename)?.title;
            picker.push_str(&chart_picker_row(&plan.filename, &title));
        }
        picker.push_str("</tbody></table></div>");
        html.push_str("<section class=\"chart-picker\"><div class=\"section-title\"><h2>Chart highlights</h2><p class=\"muted\">Highlight a graph here or from its own toolbar.</p></div>");
        html.push_str(&folded_table_html("Chart highlights", picker));
        html.push_str("</section>");
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
            let graphs = group_plans
                .iter()
                .map(|plan| plan.filename.clone())
                .collect::<Vec<_>>();
            html.push_str(&format!("<section id=\"agent-concurrency-group-{index}\"><div class=\"section-title\"><h2>{}</h2><p class=\"muted\">Agents: {}. Comparable configured concurrencies: {}.</p></div>", escape(&group.display()), escape(&agents), concurrencies));
            html.push_str(&collector_chart_groups_html(
                stage,
                &graphs,
                &group_plans
                    .iter()
                    .flat_map(|plan| plan.values.keys().cloned())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>(),
            )?);
            html.push_str("</section>");
        }
    }
    html.push_str(&derived_flamegraph_section(stage, flamegraphs)?);
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
        format!(
            "{} (per-replica series mean)",
            display_metric_title(&plan.metric)
        )
    } else {
        display_metric_title(&plan.metric)
    }
}

fn metric_display(plan: &GraphPlan) -> String {
    if plan.is_series_mean {
        format!(
            "{} (per-replica series mean)",
            display_metric_title(&plan.metric)
        )
    } else {
        display_metric_title(&plan.metric)
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
    let axis = numeric_axis((min, max), &plan.metric);
    let mut svg = svg_header(WIDTH, 470.0, &title);
    svg.push_str("<text x=\"22\" y=\"48\" font-size=\"11\">Faint points are individual replicas; the connected dark line is the median at each configured concurrency.</text>");
    append_y_axis(&mut svg, LEFT, TOP, BOTTOM, RIGHT, &axis);
    let baseline = BOTTOM - domain_fraction(0.0, axis.min, axis.max) * (BOTTOM - TOP);
    if axis.min < 0.0 {
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
            let y =
                BOTTOM - domain_fraction(observation.value, axis.min, axis.max) * (BOTTOM - TOP);
            let label = format!("concurrency {concurrency} · {}", observation.label);
            svg.push_str(&format!("<circle class=\"series-point concurrency-observation\" data-series-index=\"0\" data-label=\"{}\" data-value=\"{}\" data-concurrency=\"{}\" cx=\"{:.1}\" cy=\"{y:.1}\" r=\"4\" fill=\"#7aa6d8\" fill-opacity=\"0.72\" stroke=\"#2d5f91\"><title>{}: {} {}</title></circle>", escape(&label), observation.value, concurrency, x + offset, escape(&label), format_tick(observation.value), escape(metric_unit(&plan.metric))));
        }
        let median = median(observations.iter().map(|observation| observation.value));
        let y = BOTTOM - domain_fraction(median, axis.min, axis.max) * (BOTTOM - TOP);
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
    let axis = numeric_axis((min, max), &plan.metric);
    let mut svg = svg_header(WIDTH, 502.0, &title);
    svg.push_str("<text x=\"22\" y=\"48\" font-size=\"11\">Each color is one agent. Faint points are individual replicas; connected markers are per-agent medians.</text>");
    append_y_axis(&mut svg, LEFT, TOP, BOTTOM, RIGHT, &axis);
    let baseline = BOTTOM - domain_fraction(0.0, axis.min, axis.max) * (BOTTOM - TOP);
    if axis.min < 0.0 {
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
                let y = BOTTOM
                    - domain_fraction(observation.value, axis.min, axis.max) * (BOTTOM - TOP);
                let label = format!(
                    "{agent} · concurrency {concurrency} · {}",
                    observation.label
                );
                svg.push_str(&format!("<circle class=\"series-point concurrency-observation\" data-series-index=\"{series_index}\" data-label=\"{}\" data-value=\"{}\" data-concurrency=\"{}\" cx=\"{:.1}\" cy=\"{y:.1}\" r=\"4\" fill=\"{color}\" fill-opacity=\"0.42\" stroke=\"{color}\"><title>{}: {} {}</title></circle>", escape(&label), observation.value, concurrency, x + offset, escape(&label), format_tick(observation.value), escape(metric_unit(&plan.metric))));
            }
            let value = median(observations.iter().map(|observation| observation.value));
            let y = BOTTOM - domain_fraction(value, axis.min, axis.max) * (BOTTOM - TOP);
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
        svg.push_str(&format!("<rect class=\"legend-swatch\" data-series-index=\"{series_index}\" role=\"button\" tabindex=\"0\" x=\"{legend_x:.1}\" y=\"{:.1}\" width=\"10\" height=\"10\" rx=\"2\" fill=\"{color}\"/><text class=\"legend-item\" data-series-index=\"{series_index}\" role=\"button\" tabindex=\"0\" x=\"{:.1}\" y=\"{:.1}\" font-size=\"12\" fill=\"{color}\" aria-label=\"{}\">{}</text>", legend_y - 10.0, legend_x + 16.0, legend_y, escape(agent), escape(agent)));
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

fn flamegraph_plans(rows: &[BenchmarkData]) -> FlamegraphPlans {
    let mut groups = BTreeMap::<GroupKey, BTreeMap<usize, Vec<&BenchmarkData>>>::new();
    for row in rows {
        if row
            .artifact_sources
            .keys()
            .any(|name| name.rsplit('/').next() == Some("flamegraph.folded"))
        {
            groups
                .entry(GroupKey::from_row(row))
                .or_default()
                .entry(row.configured_concurrency)
                .or_default()
                .push(row);
        }
    }

    let mut names = GraphNames::default();
    let mut plans = FlamegraphPlans::default();
    let mut by_group_statistic =
        BTreeMap::<(GroupKey, FlameStatistic), Vec<FlamegraphProfile>>::new();
    for (group, by_concurrency) in groups {
        for (concurrency, rows) in by_concurrency {
            let inputs = rows
                .iter()
                .filter_map(|row| folded_source(row))
                .collect::<Vec<_>>();
            if inputs.is_empty() {
                continue;
            }
            let source_svg = (inputs.len() == 1)
                .then(|| rows.iter().find_map(|row| svg_source(row)))
                .flatten();
            for statistic in FlameStatistic::ALL {
                let identity = format!("{}:{concurrency}:{}", group.identity(), statistic.label());
                let svg = names.allocate("concurrency-flamegraph", &identity);
                let folded = svg.trim_end_matches(".svg").to_owned() + ".folded";
                let profile = FlamegraphProfile {
                    group: group.clone(),
                    concurrency,
                    statistic,
                    folded,
                    svg,
                    inputs: inputs.clone(),
                    source_svg: source_svg.clone(),
                };
                by_group_statistic
                    .entry((group.clone(), statistic))
                    .or_default()
                    .push(profile.clone());
                plans.profiles.push(profile);
            }
        }
    }
    for ((group, statistic), mut profiles) in by_group_statistic {
        profiles.sort_by_key(|profile| profile.concurrency);
        let (Some(low), Some(high)) = (profiles.first(), profiles.last()) else {
            continue;
        };
        if low.concurrency == high.concurrency {
            continue;
        }
        let identity = format!(
            "{}:{}:c{}-c{}",
            group.identity(),
            statistic.label(),
            low.concurrency,
            high.concurrency
        );
        let svg = names.allocate("concurrency-flamegraph-diff", &identity);
        let folded = svg.trim_end_matches(".svg").to_owned() + ".folded";
        plans.diffs.push(FlamegraphDiff {
            group,
            statistic,
            low_concurrency: low.concurrency,
            high_concurrency: high.concurrency,
            low_folded: low.folded.clone(),
            high_folded: high.folded.clone(),
            folded,
            svg,
        });
    }
    plans
}

fn folded_source(row: &BenchmarkData) -> Option<PathBuf> {
    // A timed-out post-processing pipeline intentionally retains its partial
    // folded stream for diagnosis. It is not a completed FlameGraph and must
    // not become an input to a derived aggregate or diff report.
    if row
        .collector_documents
        .get("flamegraph")
        .is_some_and(|document| {
            document.get("status").and_then(Value::as_str) != Some("completed")
                || document.get("artifact").and_then(Value::as_str) != Some("flamegraph.svg")
        })
    {
        return None;
    }
    row.artifact_sources
        .iter()
        .find(|(name, _)| name.rsplit('/').next() == Some("flamegraph.folded"))
        .map(|(_, path)| path.clone())
}

fn svg_source(row: &BenchmarkData) -> Option<PathBuf> {
    row.artifact_sources
        .iter()
        .find(|(name, _)| name.rsplit('/').next() == Some("flamegraph.svg"))
        .map(|(_, path)| path.clone())
}

fn render_flamegraphs(
    stage: &Path,
    plans: &FlamegraphPlans,
    tools: Option<&FlamegraphTools>,
) -> Result<(), Error> {
    for profile in &plans.profiles {
        let folded = aggregate_folded(&profile.inputs, profile.statistic)?;
        let folded_text = if profile.inputs.len() == 1 {
            source::read_text(&profile.inputs[0], "flamegraph folded stacks")?
        } else {
            folded_text(&folded)
        };
        fs::write(stage.join(&profile.folded), folded_text).map_err(crate::io_error)?;
        if profile.inputs.len() == 1 {
            if let Some(source) = &profile.source_svg {
                fs::write(
                    stage.join(&profile.svg),
                    source::read_svg(source, &profile.svg)?,
                )
                .map_err(crate::io_error)?;
            } else {
                fs::write(
                    stage.join(&profile.svg),
                    folded_svg(&folded, &profile_title(profile)),
                )
                .map_err(crate::io_error)?;
            }
        } else {
            fs::write(
                stage.join(&profile.svg),
                folded_svg(&folded, &profile_title(profile)),
            )
            .map_err(crate::io_error)?;
        }
    }
    for diff in &plans.diffs {
        render_flamegraph_diff(
            tools.expect("diff plans require resolved FlameGraph tools"),
            stage,
            diff,
        )?;
    }
    Ok(())
}

fn render_flamegraph_diff(
    tools: &FlamegraphTools,
    stage: &Path,
    diff: &FlamegraphDiff,
) -> Result<(), Error> {
    let folded = stage.join(&diff.folded);
    run_flamegraph_command(
        Command::new(&tools.diff)
            .arg(stage.join(&diff.low_folded))
            .arg(stage.join(&diff.high_folded))
            .stdin(Stdio::null())
            .stdout(fs::File::create(&folded).map_err(|error| {
                Error::Run(format!("cannot create {}: {error}", folded.display()))
            })?)
            .stderr(Stdio::null()),
        "FlameGraph differential preparation",
    )?;
    run_flamegraph_command(
        Command::new(&tools.render)
            .arg("--title")
            .arg(diff_title(diff))
            .arg("--subtitle")
            .arg("Red: higher concurrency; blue: lower concurrency")
            .arg("--countname")
            .arg("samples")
            .arg(&folded)
            .stdin(Stdio::null())
            .stdout(fs::File::create(stage.join(&diff.svg)).map_err(|error| {
                Error::Run(format!(
                    "cannot create {}: {error}",
                    stage.join(&diff.svg).display()
                ))
            })?)
            .stderr(Stdio::null()),
        "FlameGraph differential rendering",
    )?;
    source::validate_external_svg(&stage.join(&diff.svg), &diff.svg)
}

fn run_flamegraph_command(command: &mut Command, label: &str) -> Result<(), Error> {
    const TIMEOUT: Duration = Duration::from_secs(300);
    let mut child = command
        .spawn()
        .map_err(|error| Error::Run(format!("cannot start {label}: {error}")))?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(Error::Run(format!("{label} failed with {status}")));
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Run(format!(
                    "{label} exceeded its five-minute limit"
                )));
            }
            Err(error) => return Err(Error::Run(format!("cannot observe {label}: {error}"))),
        }
    }
}

fn aggregate_folded(
    inputs: &[PathBuf],
    statistic: FlameStatistic,
) -> Result<BTreeMap<String, f64>, Error> {
    let profiles = inputs
        .iter()
        .map(|path| parse_folded(path))
        .collect::<Result<Vec<_>, _>>()?;
    let mut values = BTreeMap::<String, Vec<f64>>::new();
    for profile in &profiles {
        for (stack, value) in profile {
            values.entry(stack.clone()).or_default().push(*value);
        }
    }
    Ok(values
        .into_iter()
        .filter_map(|(stack, mut values)| {
            let value = statistic.sparse_value(&mut values, profiles.len());
            (value > 0.0).then_some((stack, value))
        })
        .collect())
}

fn parse_folded(path: &Path) -> Result<BTreeMap<String, f64>, Error> {
    let mut parsed = BTreeMap::new();
    for (line_number, line) in source::read_text(path, "flamegraph folded stacks")?
        .lines()
        .enumerate()
    {
        let (stack, value) = line.rsplit_once(' ').ok_or_else(|| {
            Error::Input(format!(
                "invalid flamegraph folded stack at {} line {}",
                path.display(),
                line_number + 1
            ))
        })?;
        let value = value
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value > 0.0)
            .ok_or_else(|| {
                Error::Input(format!(
                    "invalid flamegraph folded value at {} line {}",
                    path.display(),
                    line_number + 1
                ))
            })?;
        if stack.is_empty() || stack.contains('\n') || stack.contains('\r') {
            return Err(Error::Input(format!(
                "invalid flamegraph stack at {} line {}",
                path.display(),
                line_number + 1
            )));
        }
        *parsed.entry(stack.to_owned()).or_insert(0.0) += value;
    }
    if parsed.is_empty() {
        return Err(Error::Input(format!(
            "flamegraph folded stacks {} are empty",
            path.display()
        )));
    }
    Ok(parsed)
}

fn folded_text(values: &BTreeMap<String, f64>) -> String {
    values
        .iter()
        .map(|(stack, value)| format!("{stack} {value:.6}\n"))
        .collect()
}

fn profile_title(profile: &FlamegraphProfile) -> String {
    format!(
        "{} flamegraph · concurrency {} · {} across {} replica(s)",
        profile.group.display(),
        profile.concurrency,
        profile.statistic.label(),
        profile.inputs.len()
    )
}

fn diff_title(diff: &FlamegraphDiff) -> String {
    format!(
        "{} flamegraph diff · {} · concurrency {} to {}",
        diff.group.display(),
        diff.statistic.label(),
        diff.low_concurrency,
        diff.high_concurrency
    )
}

fn folded_svg(values: &BTreeMap<String, f64>, title: &str) -> String {
    let tree = FlameTree::from_folded(values);
    let frame_height = 24.0;
    let height = 110.0 + tree.depth().max(1) as f64 * frame_height + 26.0;
    let mut svg = svg_header(1200.0, height, title);
    svg.push_str(&format!("<text x=\"24\" y=\"34\" font-size=\"15\" font-weight=\"700\">{}</text><text x=\"24\" y=\"55\" font-size=\"11\">Aggregate folded stacks; frame widths are aggregate sample weight.</text>", escape(title)));
    let mut color_index = 0;
    render_flame_children(
        &mut svg,
        &tree,
        24.0,
        1152.0,
        0,
        height - 34.0,
        frame_height,
        &mut color_index,
    );
    svg.push_str(&format!("<text x=\"24\" y=\"{:.1}\" font-size=\"11\">Full stack weights are available in the adjacent folded-stack download.</text></svg>\n", height - 10.0));
    svg
}

#[derive(Default)]
struct FlameTree {
    magnitude: f64,
    children: BTreeMap<String, FlameTree>,
}

impl FlameTree {
    fn from_folded(values: &BTreeMap<String, f64>) -> Self {
        let mut root = Self::default();
        for (stack, value) in values {
            root.magnitude += value;
            let mut node = &mut root;
            for frame in stack.split(';') {
                node = node.children.entry(frame.into()).or_default();
                node.magnitude += value;
            }
        }
        root
    }

    fn depth(&self) -> usize {
        self.children
            .values()
            .map(|child| 1 + child.depth())
            .max()
            .unwrap_or(0)
    }
}

#[allow(clippy::too_many_arguments)]
fn render_flame_children(
    svg: &mut String,
    node: &FlameTree,
    x: f64,
    width: f64,
    depth: usize,
    bottom: f64,
    frame_height: f64,
    color_index: &mut usize,
) {
    let mut cursor = x;
    for (frame, child) in &node.children {
        let child_width = width * child.magnitude / node.magnitude.max(1.0);
        let y = bottom - (depth + 1) as f64 * frame_height;
        let colors = ["#e77d5b", "#f6bd60", "#84a59d", "#f28482", "#90be6d"];
        let color = colors[*color_index % colors.len()];
        *color_index += 1;
        svg.push_str(&format!("<g><rect x=\"{cursor:.2}\" y=\"{y:.2}\" width=\"{child_width:.2}\" height=\"{:.2}\" fill=\"{color}\" stroke=\"#fff\"><title>{}: {:.3}</title></rect>", frame_height - 1.0, escape(frame), child.magnitude));
        if child_width > 70.0 {
            svg.push_str(&format!(
                "<text x=\"{:.2}\" y=\"{:.2}\" font-size=\"11\" fill=\"#18212b\">{}</text>",
                cursor + 4.0,
                y + 16.0,
                escape(frame)
            ));
        }
        svg.push_str("</g>");
        render_flame_children(
            svg,
            child,
            cursor,
            child_width,
            depth + 1,
            bottom,
            frame_height,
            color_index,
        );
        cursor += child_width;
    }
}

fn derived_flamegraph_section(stage: &Path, plans: &FlamegraphPlans) -> Result<String, Error> {
    if plans.profiles.is_empty() {
        return Ok(String::new());
    }
    let mut groups = Vec::new();
    let mut diffs = String::new();
    for diff in &plans.diffs {
        diffs.push_str(&derived_flamegraph_figure(
            stage,
            &diff_title(diff),
            &diff.svg,
            &diff.folded,
        )?);
    }
    if !diffs.is_empty() {
        groups.push(("Diffs", diffs));
    }
    for statistic in FlameStatistic::ALL {
        let mut figures = String::new();
        for profile in plans
            .profiles
            .iter()
            .filter(|profile| profile.statistic == statistic)
        {
            figures.push_str(&derived_flamegraph_figure(
                stage,
                &profile_title(profile),
                &profile.svg,
                &profile.folded,
            )?);
        }
        if !figures.is_empty() {
            groups.push((
                match statistic {
                    FlameStatistic::Mean => "Mean",
                    FlameStatistic::Median => "Median",
                    FlameStatistic::P95 => "P95",
                },
                figures,
            ));
        }
    }
    let figures = groups
        .into_iter()
        .map(|(label, figures)| {
            format!(
                "<details class=\"metric-graph-subgroup\"><summary>{label} ({} graphs)</summary><div class=\"charts metric-subgroup-charts\">{figures}</div></details>",
                figures.matches("<figure class=\"flamegraph-chart\"").count(),
            )
        })
        .collect::<String>();
    Ok(format!(
        "<section class=\"flamegraph-section\"><details><summary>Aggregate flamegraphs</summary><p class=\"muted\">Each statistic includes every completed repeat and replica at that configured concurrency. Diff graphs compare the smallest and largest configured concurrencies for the same agent, trial, provider, and model.</p><div class=\"charts\">{figures}</div></details></section>"
    ))
}

fn derived_flamegraph_figure(
    stage: &Path,
    title: &str,
    svg: &str,
    folded: &str,
) -> Result<String, Error> {
    let svg_path = stage.join(svg);
    let (body, download) = match source::read_flamegraph_preview(&svg_path, svg)? {
        Some(svg_document) => {
            let embedded = flamegraph_data_uri(&svg_document);
            (
                format!(
                    "<iframe title=\"{}\" sandbox=\"allow-scripts\" src=\"{}\"></iframe>",
                    escape(title),
                    escape(&embedded)
                ),
                format!(
                    "<a href=\"{}\" download=\"{}\">Download embedded SVG</a>",
                    escape(&embedded),
                    escape(svg),
                ),
            )
        }
        None => {
            source::validate_external_svg(&svg_path, svg)?;
            (
                format!(
                    "<iframe title=\"{}\" sandbox=\"allow-scripts\" src=\"{}\" loading=\"lazy\"></iframe>",
                    escape(title),
                    escape(svg)
                ),
                format!("<a href=\"{}\" download>Download SVG</a>", escape(svg)),
            )
        }
    };
    Ok(format!(
        "<figure class=\"flamegraph-chart\" data-collector=\"Flamegraph aggregate\"><div class=\"flamegraph-toolbar\"><strong>{}</strong><button type=\"button\" class=\"flamegraph-expand\">Enlarge</button><button type=\"button\" class=\"flamegraph-close\" hidden>Close</button></div>{}{download} · <a href=\"{}\" download>Download folded stacks</a><details class=\"chart-description\"><summary>What this measures</summary><p>Aggregated sampled process-tree call stacks for the selected population. Diff colors show relative sampled-stack growth or shrinkage, not an exact wall-time breakdown.</p></details></figure>",
        escape(title),
        body,
        escape(folded)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    fn differential_tools(root: &Path) -> FlamegraphTools {
        let bin = root.join("tools");
        fs::create_dir_all(&bin).unwrap();
        let diff = bin.join("difffolded.pl");
        let render = bin.join("flamegraph");
        fs::write(
            &diff,
            "#!/bin/sh\nprintf '%s\\n' \"$1\" \"$2\" > \"$0.args\"\nprintf 'root;growth 3 12\\nroot;shrink 12 3\\n'\n",
        )
        .unwrap();
        fs::write(
            &render,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nprintf '%s\\n' '<svg xmlns=\"http://www.w3.org/2000/svg\"><text id=\"title\">Upstream Flame Graph</text><g id=\"frames\"><title>root;growth (12 samples, 75%; +56.25%)</title><rect fill=\"rgb(255,0,0)\"/></g></svg>'\n",
        )
        .unwrap();
        for path in [&diff, &render] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        FlamegraphTools { diff, render }
    }

    fn flame_row(
        agent: &str,
        concurrency: usize,
        batch: usize,
        replica: usize,
        folded: PathBuf,
        svg: PathBuf,
    ) -> BenchmarkData {
        let mut result = row(agent, concurrency, 1.0);
        result.id.batch = Some(batch as u64);
        result.id.replica = replica;
        result.replica_index = Some(replica);
        result.artifacts = vec![
            format!("{agent}-{concurrency}-{batch}-{replica}/flamegraph.folded"),
            format!("{agent}-{concurrency}-{batch}-{replica}/flamegraph.svg"),
        ];
        result.artifact_sources = BTreeMap::from([
            (result.artifacts[0].clone(), folded),
            (result.artifacts[1].clone(), svg),
        ]);
        result.collector_documents.insert(
            "flamegraph".into(),
            serde_json::json!({"status":"completed", "artifact":"flamegraph.svg"}),
        );
        result
    }

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

    #[cfg(unix)]
    #[test]
    fn aggregate_flamegraphs_preserve_evidence_for_rebuilt_sweep_continuation() {
        let root = std::env::temp_dir().join(format!(
            "asb-concurrency-flamegraphs-{}-{}",
            std::process::id(),
            crate::tools::runtime::unique_id()
        ));
        let sources = root.join("campaign");
        let stage = root.join("comparison");
        fs::create_dir_all(&sources).unwrap();
        fs::create_dir_all(&stage).unwrap();
        let mut rows = Vec::new();
        for (concurrency, batch, replica, folded) in [
            (1, 0, 0, "root;single 7\nroot;other 3\n"),
            (2, 0, 0, "root;single 2\nroot;other 8\n"),
            (2, 0, 1, "root;single 10\nroot;other 4\n"),
            (2, 1, 0, "root;single 4\nroot;other 12\n"),
            (2, 1, 1, "root;single 8\nroot;other 16\n"),
        ] {
            let prefix = format!("c{concurrency}-b{batch}-r{replica}");
            let folded_path = sources.join(format!("{prefix}.folded"));
            let svg_path = sources.join(format!("{prefix}.svg"));
            fs::write(&folded_path, folded).unwrap();
            fs::write(&svg_path, format!("<svg><title>{prefix}</title></svg>\n")).unwrap();
            rows.push(flame_row(
                "pi",
                concurrency,
                batch,
                replica,
                folded_path,
                svg_path,
            ));
        }
        let before = rows
            .iter()
            .flat_map(|row| row.artifact_sources.values())
            .map(|path| (path.clone(), fs::read(path).unwrap()))
            .collect::<BTreeMap<_, _>>();
        let plans = flamegraph_plans(&rows);
        assert_eq!(
            plans.profiles.len(),
            6,
            "three statistics at two concurrencies"
        );
        assert_eq!(plans.diffs.len(), 3, "mean, median, and p95 diffs");
        let c2_mean = plans
            .profiles
            .iter()
            .find(|plan| plan.concurrency == 2 && plan.statistic == FlameStatistic::Mean)
            .unwrap();
        assert_eq!(
            aggregate_folded(&c2_mean.inputs, FlameStatistic::Mean).unwrap(),
            BTreeMap::from([("root;other".into(), 10.0), ("root;single".into(), 6.0)])
        );
        let c2_median = plans
            .profiles
            .iter()
            .find(|plan| plan.concurrency == 2 && plan.statistic == FlameStatistic::Median)
            .unwrap();
        assert_eq!(
            aggregate_folded(&c2_median.inputs, FlameStatistic::Median).unwrap(),
            BTreeMap::from([("root;other".into(), 8.0), ("root;single".into(), 4.0)])
        );
        let c2_p95 = plans
            .profiles
            .iter()
            .find(|plan| plan.concurrency == 2 && plan.statistic == FlameStatistic::P95)
            .unwrap();
        assert_eq!(
            aggregate_folded(&c2_p95.inputs, FlameStatistic::P95).unwrap(),
            BTreeMap::from([("root;other".into(), 16.0), ("root;single".into(), 10.0)])
        );

        let sparse_paths = [
            sources.join("sparse-1.folded"),
            sources.join("sparse-2.folded"),
            sources.join("sparse-3.folded"),
        ];
        fs::write(&sparse_paths[0], "root;shared 7\nroot;only 9\n").unwrap();
        fs::write(&sparse_paths[1], "root;shared 5\n").unwrap();
        fs::write(&sparse_paths[2], "root;shared 1\n").unwrap();
        assert_eq!(
            aggregate_folded(&sparse_paths, FlameStatistic::Mean).unwrap(),
            BTreeMap::from([
                ("root;only".into(), 3.0),
                ("root;shared".into(), 13.0 / 3.0)
            ])
        );
        assert_eq!(
            aggregate_folded(&sparse_paths, FlameStatistic::Median).unwrap(),
            BTreeMap::from([("root;shared".into(), 5.0)])
        );
        assert_eq!(
            aggregate_folded(&sparse_paths, FlameStatistic::P95).unwrap(),
            BTreeMap::from([("root;only".into(), 9.0), ("root;shared".into(), 7.0)])
        );

        let tools = differential_tools(&root);
        render_flamegraphs(&stage, &plans, Some(&tools)).unwrap();
        for statistic in FlameStatistic::ALL {
            let c1 = plans
                .profiles
                .iter()
                .find(|plan| plan.concurrency == 1 && plan.statistic == statistic)
                .unwrap();
            assert_eq!(
                fs::read(stage.join(&c1.folded)).unwrap(),
                fs::read(&c1.inputs[0]).unwrap(),
                "C1 {statistic:?} keeps the original folded stacks"
            );
            assert_eq!(
                fs::read(stage.join(&c1.svg)).unwrap(),
                fs::read(c1.source_svg.as_ref().unwrap()).unwrap(),
                "C1 {statistic:?} keeps the original flamegraph"
            );
        }
        for diff in &plans.diffs {
            let svg = fs::read_to_string(stage.join(&diff.svg)).unwrap();
            assert!(svg.contains("id=\"title\""));
            assert!(svg.contains("id=\"frames\""));
            assert!(svg.contains("rgb(255,0,0)"));
            assert!(!svg.contains("Red grows at the higher concurrency"));
            let folded = fs::read_to_string(stage.join(&diff.folded)).unwrap();
            assert_eq!(folded, "root;growth 3 12\nroot;shrink 12 3\n");
        }
        let args = fs::read_to_string(tools.render.with_extension("args")).unwrap();
        assert!(args.contains("--title"));
        assert!(args.contains("--subtitle"));
        assert!(args.contains("Red: higher concurrency; blue: lower concurrency"));
        assert!(args.contains("--countname"));
        assert!(args.contains("samples"));
        let diff_args = fs::read_to_string(format!("{}.args", tools.diff.display())).unwrap();
        assert!(diff_args.contains("concurrency-flamegraph"));
        let after = rows
            .iter()
            .flat_map(|row| row.artifact_sources.values())
            .map(|path| (path.clone(), fs::read(path).unwrap()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            before, after,
            "post-processing leaves continuation evidence unchanged"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn aggregate_flamegraphs_ignore_retained_output_from_unavailable_collectors() {
        let root = std::env::temp_dir().join(format!(
            "asb-concurrency-unavailable-flamegraph-{}-{}",
            std::process::id(),
            crate::tools::runtime::unique_id()
        ));
        let sources = root.join("campaign");
        let stage = root.join("comparison");
        fs::create_dir_all(&sources).unwrap();
        fs::create_dir_all(&stage).unwrap();

        let c1_folded = sources.join("c1.folded");
        let c1_svg = sources.join("c1.svg");
        let c2_folded = sources.join("c2.folded");
        let c2_svg = sources.join("c2.svg");
        let partial_folded = sources.join("partial.folded");
        let partial_svg = sources.join("partial.svg");
        fs::write(&c1_folded, "root;c1 1\n").unwrap();
        fs::write(&c1_svg, "<svg><title>c1</title></svg>\n").unwrap();
        fs::write(&c2_folded, "root;c2 2\n").unwrap();
        fs::write(&c2_svg, "<svg><title>c2</title></svg>\n").unwrap();
        fs::write(&partial_folded, "root;partial").unwrap();
        fs::write(&partial_svg, "<svg><title>partial</title></svg>\n").unwrap();

        let mut unavailable = flame_row("pi", 2, 0, 1, partial_folded.clone(), partial_svg);
        unavailable.collector_documents.insert(
            "flamegraph".into(),
            serde_json::json!({
                "status":"unavailable",
                "artifact":null,
                "reason":"perf script timed out"
            }),
        );
        let rows = vec![
            flame_row("pi", 1, 0, 0, c1_folded, c1_svg),
            flame_row("pi", 2, 0, 0, c2_folded.clone(), c2_svg),
            unavailable,
        ];
        let plans = flamegraph_plans(&rows);
        let c2 = plans
            .profiles
            .iter()
            .find(|profile| profile.concurrency == 2 && profile.statistic == FlameStatistic::Mean)
            .unwrap();
        assert_eq!(c2.inputs, vec![c2_folded]);

        render_flamegraphs(&stage, &plans, Some(&differential_tools(&root))).unwrap();
        assert!(stage.join(&c2.svg).is_file());
        assert_eq!(fs::read(&partial_folded).unwrap(), b"root;partial");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn derived_flamegraphs_are_self_contained() {
        let root = std::env::temp_dir().join(format!(
            "asb-large-derived-flamegraph-{}-{}",
            std::process::id(),
            crate::tools::runtime::unique_id()
        ));
        fs::create_dir_all(&root).unwrap();
        let svg = "aggregate.svg";
        let mut document = String::from("<svg>");
        document.push_str(&" ".repeat(1024 * 1024));
        document.push_str("</svg>");
        fs::write(root.join(svg), document).unwrap();

        let html = derived_flamegraph_figure(&root, "aggregate", svg, "aggregate.folded").unwrap();
        assert!(html.contains("sandbox=\"allow-scripts\""), "{html}");
        assert!(html.contains("src=\"data:image/svg+xml;base64,"), "{html}");
        assert!(html.contains("data:image/svg+xml;base64,"), "{html}");
        assert!(html.contains("download=\"aggregate.svg\""), "{html}");
        assert!(!html.contains("allow-same-origin"), "{html}");

        fs::remove_dir_all(root).unwrap();
    }
}
