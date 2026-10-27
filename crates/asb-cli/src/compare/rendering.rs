//! Render one prepared immutable report into a private staging generation.
use super::*;
use planning::GraphKind;
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
    let chart_labels = benchmark_data
        .iter()
        .map(|row| (row.label.clone(), super::chart_label(row)))
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
                        (row.label.clone(), row.metrics[name])
                    })
                    .collect::<Vec<_>>();
                write_bar_svg(&path, name, &values, &chart_labels)?;
            }
            GraphKind::Series(name) => {
                let values = graph
                    .rows
                    .iter()
                    .map(|index| {
                        let row = &benchmark_data[*index];
                        (row.label.clone(), &row.series[name])
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
    write_html_report(stage, prepared)
}
