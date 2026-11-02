//! Render one prepared immutable report into a private staging generation.
use super::*;
use planning::GraphKind;

#[derive(Clone, Copy)]
enum AgentReportKind {
    Summary,
    Distribution,
    Histogram,
}

impl AgentReportKind {
    const ALL: [Self; 3] = [Self::Summary, Self::Distribution, Self::Histogram];

    fn report_file(self) -> &'static str {
        match self {
            Self::Summary => "agent-summary-report.html",
            Self::Distribution => "agent-distribution-report.html",
            Self::Histogram => "agent-histogram-report.html",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Summary => "Agent summary comparison",
            Self::Distribution => "Agent value distributions",
            Self::Histogram => "Agent value histograms",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Summary => {
                "Mean, median, and 95th-percentile observations grouped by agent across this sweep."
            }
            Self::Distribution => {
                "Sorted observations per agent. Each line retains all available values for an agent and metric."
            }
            Self::Histogram => {
                "Binned observations per agent. Histogram bins use a shared metric range so agents remain comparable."
            }
        }
    }

    fn graph_kind(self, statistic: Option<&str>) -> String {
        match (self, statistic) {
            (Self::Summary, Some(statistic)) => format!("agent-summary-{statistic}"),
            (Self::Distribution, _) => "agent-distribution".into(),
            (Self::Histogram, _) => "agent-histogram".into(),
            (Self::Summary, None) => unreachable!("summary graphs always name their statistic"),
        }
    }
}

#[derive(Clone)]
struct AgentReportPlan {
    kind: AgentReportKind,
    report_file: String,
    graphs: Vec<(String, Vec<(String, String)>)>,
}

/// Keep aggregate file names deterministic and predeclared so they are
/// published together with the original report by the same report publisher.
pub(super) fn agent_report_files(prepared: &PreparedReport) -> Vec<String> {
    agent_report_plans(prepared)
        .into_iter()
        .flat_map(|plan| {
            std::iter::once(plan.report_file).chain(
                plan.graphs
                    .into_iter()
                    .flat_map(|(_, graphs)| graphs.into_iter().map(|(_, file)| file)),
            )
        })
        .collect()
}

fn agent_report_plans(prepared: &PreparedReport) -> Vec<AgentReportPlan> {
    let metrics = prepared
        .data
        .rows
        .iter()
        .flat_map(|row| row.metrics.keys())
        .filter(|name| planning::graphable_metric(name))
        .cloned()
        .collect::<BTreeSet<_>>();
    AgentReportKind::ALL
        .into_iter()
        .map(|kind| {
            let mut names = GraphNames::default();
            let graphs = metrics
                .iter()
                .map(|metric| {
                    let files = match kind {
                        AgentReportKind::Summary => ["mean", "median", "p95"]
                            .into_iter()
                            .map(|statistic| {
                                (
                                    statistic.to_owned(),
                                    names.allocate(&kind.graph_kind(Some(statistic)), metric),
                                )
                            })
                            .collect(),
                        AgentReportKind::Distribution | AgentReportKind::Histogram => vec![(
                            "values".into(),
                            names.allocate(&kind.graph_kind(None), metric),
                        )],
                    };
                    (metric.clone(), files)
                })
                .collect();
            AgentReportPlan {
                kind,
                report_file: kind.report_file().into(),
                graphs,
            }
        })
        .collect()
}
pub(super) fn render(stage: &Path, prepared: &PreparedReport) -> Result<(), Error> {
    let benchmark_data = &prepared.data.rows;
    let plan = &prepared.data.request.plan;
    let sweep_path = Path::new(&prepared.data.request.path);
    let single = prepared.data.request.single;
    let parent = &prepared.data.request.parent;
    let sweep_results = &prepared.data.request.sweep_results;
    let comparison_dir = stage;
    let graphs = prepared.graph_names();
    let resolution_groups = &prepared.resolution;
    let evaluation_coverage = &prepared.evaluation;
    let population_coverage = &prepared.populations;
    let base_labels = benchmark_data
        .iter()
        .map(super::chart_label)
        .collect::<Vec<_>>();
    let mut base_counts = BTreeMap::new();
    for label in &base_labels {
        *base_counts.entry(label.clone()).or_insert(0usize) += 1;
    }
    let mut labels = benchmark_data
        .iter()
        .zip(&base_labels)
        .map(|(row, base)| {
            if base_counts[base] == 1 {
                base.clone()
            } else {
                format!("{} / {base}", row.agent)
            }
        })
        .collect::<Vec<_>>();
    let mut label_counts = BTreeMap::new();
    for label in &labels {
        *label_counts.entry(label.clone()).or_insert(0usize) += 1;
    }
    for (index, label) in labels.iter_mut().enumerate() {
        if label_counts[label] > 1 {
            let row = &benchmark_data[index];
            *label = format!(
                "{} / {} / {} / {}",
                row.agent,
                row.provider,
                row.model,
                super::attempt_label(row)
            );
        }
    }
    let chart_labels = benchmark_data
        .iter()
        .zip(labels)
        .map(|(row, label)| (row.label.clone(), label))
        .collect();
    for graph in &prepared.graphs {
        let path = stage.join(&graph.filename);
        match &graph.kind {
            GraphKind::Metric(name) => {
                let values = graph
                    .rows
                    .iter()
                    .map(|index| {
                        let row = &benchmark_data[*index];
                        (row.label.clone(), row.metrics.get(name).copied())
                    })
                    .collect::<Vec<_>>();
                write_bar_svg(&path, name, &values, &chart_labels)?;
            }
            GraphKind::Series { name, timed } => {
                let values = graph
                    .rows
                    .iter()
                    .map(|index| {
                        let row = &benchmark_data[*index];
                        (
                            row.label.clone(),
                            row.series
                                .get(name)
                                .filter(|series| series.timestamps_ms.is_some() == *timed),
                        )
                    })
                    .collect::<Vec<_>>();
                write_series_svg_with_labels(&path, name, &values, &chart_labels)?;
            }
            GraphKind::Collector(index) => {
                let graph = prepared.collector_graphs.get(*index).ok_or_else(|| {
                    Error::Run("prepared collector graph index is unavailable".into())
                })?;
                fs::write(&path, &graph.svg).map_err(|error| {
                    Error::Run(format!(
                        "cannot write collector report graph {}: {error}",
                        graph.filename
                    ))
                })?;
            }
        }
    }
    let benchmark_rows = benchmark_data.iter().map(|benchmark| json!({
            "id": benchmark.id,
            "execution": benchmark.execution,
            "name": benchmark.label,
            "process_termination": benchmark.observation.process_termination,
            "process_termination_diagnostic": benchmark.observation.process_termination_diagnostic,
            "planning": benchmark.planning,
            "effective_budgets": benchmark.effective_budgets,
            "module": benchmark.module,
            "legacy_evaluation_provenance": benchmark.legacy_evaluation_provenance,
            "execution_plan_fingerprint": benchmark.execution_plan_fingerprint,
            "benchmark_outcome": benchmark.benchmark_outcome,
            "directory": benchmark.directory,
            "agent": benchmark.agent,
            "workload": benchmark.workload,
            "task": benchmark.task,
            "replica_index": benchmark.replica_index,
            "execution_cohort": benchmark.execution_cohort,
            "reconciled": benchmark.reconciled,
            "result_source": benchmark.result_source,
            "selection": benchmark.selection,
            "evaluation_completed": benchmark.evaluation_completed,
            "workload_evaluation": benchmark.workload_evaluation,
            "provider": benchmark.provider,
            "model": benchmark.model,
            "metrics": benchmark.metrics,
            "host_metadata": benchmark.host_metadata,
            "components": benchmark.components,
            "series": benchmark.series.iter().map(|(name, series)| (name, &series.values)).collect::<BTreeMap<_, _>>(),
            "series_timestamps": benchmark.series.iter().filter_map(|(name, series)| series.timestamps_ms.as_ref().map(|timestamps| (name, timestamps))).collect::<BTreeMap<_, _>>(),
            "series_rendering": benchmark.series.iter().map(|(name,series)|(name,series.rendering_metadata())).collect::<BTreeMap<_,_>>(),
            "status": benchmark.status,
            "artifacts": benchmark.artifacts,
        })).collect::<Vec<_>>();
    let summary = json!({
        "schema_version": 2,
        "campaign": prepared.data.request.campaign,
        "success_rate_definition": "resolved task attempts / all selected task attempts, including failures; not process exit rate or pass@k; smoke checks excluded",
        "resolution_groups": resolution_groups,
        "selection_plan": plan.as_ref().map(|plan| json!({"run_id":plan.run_id,"configuration_match":true,"execution_mode":plan.execution_mode,"respect_record_timings":plan.respect_record_timings,"scheduling":plan.scheduling,"execution":plan.execution,"replay_seed_scope":plan.execution.as_ref().map(|_|"bundled-configurations")})),
        "evaluation_coverage":evaluation_coverage,
        "population_coverage":population_coverage,
        "metric_groups": prepared.metric_groups,
        "input": sweep_path,
        "input_kind": if prepared.data.request.campaign.is_some() { "campaign" } else if single { "benchmark" } else { "sweep" },
        "sweep": if single { Value::Null } else { json!(sweep_path) },
        "results_directory": if single { json!(parent) } else { json!(sweep_results) },
        "comparison_directory": prepared.data.request.comparison_dir,
        "report": "report.html",
        "agent_reports": agent_report_files(prepared).into_iter().filter(|name| name.ends_with(".html")).collect::<Vec<_>>(),
        "benchmarks": benchmark_rows,
        "graphs": graphs,
    });
    let summary_path = comparison_dir.join("comparison.json");
    let mut file = fs::File::create(&summary_path).map_err(|error| {
        Error::Run(format!("cannot create {}: {error}", summary_path.display()))
    })?;
    serde_json::to_writer_pretty(&mut file, &summary)
        .map_err(|error| Error::Run(format!("cannot write {}: {error}", summary_path.display())))?;
    writeln!(file).map_err(|error| Error::Run(error.to_string()))?;
    write_html_report(stage, prepared)?;
    render_agent_reports(stage, prepared)
}

fn render_agent_reports(stage: &Path, prepared: &PreparedReport) -> Result<(), Error> {
    let values = agent_metric_values(&prepared.data.rows);
    for plan in agent_report_plans(prepared) {
        for (metric, files) in &plan.graphs {
            let agents = values.get(metric).cloned().unwrap_or_default();
            match plan.kind {
                AgentReportKind::Summary => {
                    for (statistic, file) in files {
                        let points = agents
                            .iter()
                            .map(|(agent, values)| {
                                (agent.clone(), summary_value(statistic, values))
                            })
                            .collect::<Vec<_>>();
                        write_bar_svg(
                            &stage.join(file),
                            &format!("{metric} · {statistic}"),
                            &points,
                            &BTreeMap::new(),
                        )?;
                    }
                }
                AgentReportKind::Distribution => {
                    let series = agents
                        .iter()
                        .map(|(agent, values)| {
                            (
                                agent.clone(),
                                TimeSeries {
                                    values: values.iter().copied().map(Some).collect(),
                                    timestamps_ms: None,
                                },
                            )
                        })
                        .collect::<Vec<_>>();
                    let borrowed = series
                        .iter()
                        .map(|(agent, series)| (agent.clone(), Some(series)))
                        .collect::<Vec<_>>();
                    write_series_svg_with_labels(
                        &stage.join(&files[0].1),
                        &format!("{metric} · sorted values"),
                        &borrowed,
                        &BTreeMap::new(),
                    )?;
                }
                AgentReportKind::Histogram => write_histogram_svg(
                    &stage.join(&files[0].1),
                    &format!("{metric} · histogram"),
                    &agents,
                )?,
            }
        }
        write_agent_html_report(stage, &plan, &values)?;
    }
    Ok(())
}

fn agent_metric_values(rows: &[BenchmarkData]) -> BTreeMap<String, BTreeMap<String, Vec<f64>>> {
    let mut grouped = BTreeMap::<String, BTreeMap<String, Vec<f64>>>::new();
    for row in rows {
        for (metric, value) in &row.metrics {
            if value.is_finite() && planning::graphable_metric(metric) {
                grouped
                    .entry(metric.clone())
                    .or_default()
                    .entry(row.agent.clone())
                    .or_default()
                    .push(*value);
            }
        }
    }
    for agents in grouped.values_mut() {
        for values in agents.values_mut() {
            values.sort_by(f64::total_cmp);
        }
    }
    grouped
}

fn summary_value(statistic: &str, values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    match statistic {
        "mean" => Some(values.iter().sum::<f64>() / values.len() as f64),
        "median" => Some(percentile(values, 0.5)),
        "p95" => Some(percentile(values, 0.95)),
        _ => None,
    }
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    let index = fraction.clamp(0.0, 1.0) * sorted.len().saturating_sub(1) as f64;
    let lower = index.floor() as usize;
    let upper = index.ceil() as usize;
    sorted[lower] + (sorted[upper] - sorted[lower]) * (index - lower as f64)
}

fn write_agent_html_report(
    stage: &Path,
    plan: &AgentReportPlan,
    values: &BTreeMap<String, BTreeMap<String, Vec<f64>>>,
) -> Result<(), Error> {
    let agents = values
        .values()
        .flat_map(|metric| metric.keys().cloned())
        .collect::<BTreeSet<_>>();
    let graphs = plan
        .graphs
        .iter()
        .flat_map(|(_, graphs)| graphs.iter().map(|(_, file)| file.clone()))
        .collect::<Vec<_>>();
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
    html.push_str(&format!("<header class=\"hero\"><p class=\"eyebrow\">ASB AGENT REPORT</p><h1>{}</h1><p>{}</p></header>", escape(plan.kind.title()), escape(plan.kind.description())));
    html.push_str("<section class=\"explain\"><strong>Agent-only aggregation</strong><p>These derived views use the same validated sweep records and SVG/HTML interaction pipeline as the benchmark report. They group every finite observation by agent; provider, model, workload, and task detail stay in the original report and comparison.json.</p></section>");
    html.push_str("<section class=\"filters\"><div class=\"section-title\"><h2>Filter agents</h2><div><button type=\"button\" data-action=\"all\">Select all</button><button type=\"button\" data-action=\"none\">Clear all</button></div></div><div class=\"filter-grid\"><fieldset><legend>Agent</legend>");
    for agent in &agents {
        html.push_str(&format!("<label><input type=\"checkbox\" data-group=\"agent\" value=\"{}\" checked><span>{}</span></label>", escape(agent), escape(agent)));
    }
    html.push_str("</fieldset></div><p class=\"filter-count\" id=\"filter-count\"></p></section>");
    html.push_str("<section class=\"chart-picker\"><div class=\"section-title\"><h2>Chart highlights</h2><p class=\"muted\">Highlight a chart here or from its own toolbar.</p></div><div class=\"table-wrap\"><table><thead><tr><th>Chart</th><th>Highlight</th></tr></thead><tbody>");
    for graph in &graphs {
        html.push_str(&format!("<tr data-highlight-row=\"{}\"><td>{}</td><td><label class=\"highlight-toggle\"><input type=\"checkbox\" data-highlight-graph=\"{}\"><span>Highlight chart</span></label></td></tr>", escape(graph), escape(graph.strip_suffix(".svg").unwrap_or(graph)), escape(graph)));
    }
    html.push_str("</tbody></table></div></section>");
    html.push_str("<section><div class=\"section-title\"><h2>Agent observations</h2><p class=\"muted\">Per-agent count, mean, median, and p95 for every collected scalar.</p></div><div class=\"table-wrap\"><table><thead><tr><th>Agent</th><th>Metric</th><th>Samples</th><th>Mean</th><th>Median</th><th>P95</th></tr></thead><tbody id=\"results\">");
    for (metric, by_agent) in values {
        for (agent, samples) in by_agent {
            html.push_str(&format!("<tr data-dimension-benchmark=\"{}\" data-dimension-agent=\"{}\"><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>", escape(agent), escape(agent), escape(agent), escape(metric), samples.len(), summary_value("mean", samples).map_or_else(|| "—".into(), format_tick), summary_value("median", samples).map_or_else(|| "—".into(), format_tick), summary_value("p95", samples).map_or_else(|| "—".into(), format_tick)));
        }
    }
    html.push_str("</tbody></table></div></section><section><div class=\"section-title\"><h2>Charts</h2><p class=\"muted\">Hover values, zoom with the wheel, drag to pan, or enlarge a graph.</p></div><div class=\"charts\">");
    for graph in &graphs {
        html.push_str(&chart_figure(stage, graph)?);
    }
    html.push_str("</div></section><footer>Generated locally by <code>asb compare</code>. No provider or agent was contacted.</footer></main>");
    html.push_str(INTERACTIVE_SCRIPT);
    html.push_str(HTML_SCRIPT);
    fs::write(stage.join(&plan.report_file), html).map_err(|error| {
        Error::Run(format!(
            "cannot write aggregate agent HTML report {}: {error}",
            plan.report_file
        ))
    })
}

fn write_histogram_svg(
    path: &Path,
    title: &str,
    agents: &BTreeMap<String, Vec<f64>>,
) -> Result<(), Error> {
    const WIDTH: f64 = 1_100.0;
    const LEFT: f64 = 88.0;
    const RIGHT: f64 = 1_060.0;
    const TOP: f64 = 108.0;
    const BOTTOM: f64 = 390.0;
    const BINS: usize = 12;
    let all = agents
        .values()
        .flat_map(|values| values.iter().copied())
        .collect::<Vec<_>>();
    let min = all.iter().copied().reduce(f64::min).unwrap_or(0.0);
    let mut max = all.iter().copied().reduce(f64::max).unwrap_or(1.0);
    if max <= min {
        max = min + 1.0;
    }
    let width = (max - min) / BINS as f64;
    let counts = agents
        .iter()
        .map(|(agent, values)| {
            let mut bins = vec![0usize; BINS];
            for value in values {
                let index = (((*value - min) / width).floor() as usize).min(BINS - 1);
                bins[index] += 1;
            }
            (agent.clone(), bins)
        })
        .collect::<Vec<_>>();
    let maximum = counts
        .iter()
        .flat_map(|(_, bins)| bins)
        .copied()
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    let mut svg = svg_header(WIDTH, 470.0, title);
    svg.push_str("<text x=\"20\" y=\"52\" font-size=\"11\">Every color is one agent. Each metric uses a shared numeric range and twelve equal-width bins.</text>");
    append_y_axis(
        &mut svg,
        LEFT,
        TOP,
        BOTTOM,
        RIGHT,
        (0.0, maximum),
        "sample_count",
    );
    let colors = [
        "#4472c4", "#ed7d31", "#70ad47", "#a5a5a5", "#ffc000", "#5b9bd5",
    ];
    let group_width = (RIGHT - LEFT) / BINS as f64;
    let bar_width = (group_width - 8.0) / counts.len().max(1) as f64;
    for (agent_index, (agent, bins)) in counts.iter().enumerate() {
        let color = colors[agent_index % colors.len()];
        svg.push_str(&format!("<g data-benchmark=\"{}\">", escape(agent)));
        for (bin, count) in bins.iter().enumerate() {
            let height = *count as f64 / maximum * (BOTTOM - TOP);
            let x = LEFT + bin as f64 * group_width + 4.0 + agent_index as f64 * bar_width;
            let y = BOTTOM - height;
            let start = min + bin as f64 * width;
            let end = start + width;
            svg.push_str(&format!("<rect class=\"bar\" data-label=\"{}\" data-value=\"{}\" x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{height:.1}\" fill=\"{color}\"><title>{}: {} samples in {}–{}</title></rect>", escape(agent), count, (bar_width - 1.0).max(1.0), escape(agent), count, format_tick(start), format_tick(end)));
        }
        let legend_x = 90.0 + (agent_index % 3) as f64 * 300.0;
        let legend_y = 74.0 + (agent_index / 3) as f64 * 18.0;
        svg.push_str(&format!("<rect x=\"{legend_x:.1}\" y=\"{:.1}\" width=\"10\" height=\"10\" rx=\"2\" fill=\"{color}\"/><text x=\"{:.1}\" y=\"{legend_y:.1}\" font-size=\"12\">{}</text></g>", legend_y - 9.0, legend_x + 16.0, escape(agent)));
    }
    for bin in 0..=BINS {
        let x = LEFT + bin as f64 * group_width;
        let value = min + bin as f64 * width;
        svg.push_str(&format!("<line x1=\"{x:.1}\" y1=\"{BOTTOM}\" x2=\"{x:.1}\" y2=\"{:.1}\" stroke=\"#18212b\"/><text x=\"{x:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"10\">{}</text>", BOTTOM + 5.0, BOTTOM + 22.0, format_tick(value)));
    }
    svg.push_str("<text x=\"574\" y=\"452\" text-anchor=\"middle\" font-size=\"12\">metric value bin</text></svg>\n");
    fs::write(path, svg)
        .map_err(|error| Error::Run(format!("cannot write {}: {error}", path.display())))
}
