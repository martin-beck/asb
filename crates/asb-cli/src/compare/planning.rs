//! Pure aggregation and graph identities. No report files are read or written.
use super::*;
pub(super) enum GraphKind {
    Metric(String),
    Series(String),
    Collector(usize),
}
pub(super) struct GraphSpec {
    pub filename: String,
    pub kind: GraphKind,
    pub rows: Vec<usize>,
}
pub(super) struct PreparedReport {
    pub data: ValidatedReport,
    pub graphs: Vec<GraphSpec>,
    pub collector_graphs: Vec<crate::collectors::ReportGraph>,
    pub evaluation: coverage::EvaluationCoverage,
    pub populations: BTreeMap<String, coverage::PopulationCoverage>,
    pub resolution: BTreeMap<String, crate::workloads::ResolutionSummary>,
    pub metric_groups: BTreeMap<String, crate::workloads::outcome::MetricSummary>,
    pub dimensions: BTreeMap<&'static str, Vec<String>>,
    pub dispatch_groups: BTreeMap<String, usize>,
}
impl PreparedReport {
    pub(super) fn prepare(data: ValidatedReport) -> Result<Self, Error> {
        let evaluation = coverage::evaluation_summary(&data.rows);
        let populations = coverage::populations(&data.rows)?;
        let metric_groups = coverage::metric_groups(&data.rows)?;
        let mut metrics: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut series: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        let mut resolution: BTreeMap<String, crate::workloads::ResolutionSummary> = BTreeMap::new();
        let mut dimensions: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
        let mut dispatch_groups = BTreeMap::new();
        for (index, row) in data.rows.iter().enumerate() {
            for (dimension, value) in [
                ("benchmark", &row.label),
                ("agent", &row.agent),
                ("provider", &row.provider),
                ("model", &row.model),
                ("workload", &row.workload),
            ] {
                dimensions.entry(dimension).or_default().push(value.clone());
            }
            if let Some(cohort) = &row.execution_cohort {
                let next = dispatch_groups.len() + 1;
                dispatch_groups.entry(cohort.id.clone()).or_insert(next);
            }
            for name in row.metrics.keys() {
                metrics.entry(name.clone()).or_default().push(index);
            }
            for name in row.series.keys() {
                series.entry(name.clone()).or_default().push(index);
            }
            if row.benchmark_outcome.as_ref().is_some_and(|outcome| {
                outcome.policy != crate::workloads::outcome::MetricPolicy::default()
                    || (outcome.evaluation.completed() && outcome.evaluation.resolution().is_none())
            }) {
                continue;
            }
            for fingerprint in coverage::fingerprints(row, &data.rows)? {
                let key = serde_json::to_string(&(
                    &row.agent,
                    &row.provider,
                    &row.model,
                    &row.workload,
                    coverage::group_kind(row),
                    &row.module,
                    &fingerprint,
                    row.task
                        .as_ref()
                        .and_then(|task| task.dataset.as_ref())
                        .or_else(|| {
                            row.selection
                                .as_ref()
                                .and_then(|selection| selection.dataset.as_ref())
                        }),
                ))
                .map_err(|error| Error::Run(error.to_string()))?;
                resolution.entry(key).or_default().observe(
                    coverage::group_kind(row),
                    row.metrics
                        .get("success_rate")
                        .is_some_and(|value| *value == 1.0),
                );
            }
        }
        for values in dimensions.values_mut() {
            values.sort();
            values.dedup();
        }
        let mut names = GraphNames::default();
        let mut graphs = Vec::new();
        for (name, rows) in metrics {
            if !graphable_metric(&name) {
                continue;
            }
            graphs.push(GraphSpec {
                filename: names.allocate("metric", &name),
                kind: GraphKind::Metric(name),
                rows,
            });
        }
        for (name, rows) in series {
            let (timed, indexed): (Vec<_>, Vec<_>) = rows
                .into_iter()
                .partition(|index| data.rows[*index].series[&name].timestamps_ms.is_some());
            let mixed = !timed.is_empty() && !indexed.is_empty();
            if !timed.is_empty() {
                graphs.push(GraphSpec {
                    filename: names.allocate("series", &name),
                    kind: GraphKind::Series(name.clone()),
                    rows: timed,
                });
            }
            if !indexed.is_empty() {
                graphs.push(GraphSpec {
                    filename: names.allocate(if mixed { "series-index" } else { "series" }, &name),
                    kind: GraphKind::Series(name),
                    rows: indexed,
                });
            }
        }
        let report_rows = data
            .rows
            .iter()
            .map(|row| crate::collectors::ReportRow {
                label: &row.label,
                metrics: &row.metrics,
                components: row.components.as_ref(),
                documents: &row.collector_documents,
            })
            .collect::<Vec<_>>();
        let collector_graphs = crate::collectors::report_graphs(&report_rows);
        let mut filenames = graphs
            .iter()
            .map(|graph| graph.filename.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for (index, graph) in collector_graphs.iter().enumerate() {
            if !graph.filename.ends_with(".svg")
                || graph.filename.contains('/')
                || graph.filename.contains('\\')
                || !filenames.insert(graph.filename.clone())
            {
                return Err(Error::Run(format!(
                    "collector report graph has an invalid or duplicate filename: {}",
                    graph.filename
                )));
            }
            graphs.push(GraphSpec {
                filename: graph.filename.clone(),
                kind: GraphKind::Collector(index),
                rows: Vec::new(),
            });
        }
        Ok(Self {
            data,
            graphs,
            collector_graphs,
            evaluation,
            populations,
            metric_groups,
            resolution,
            dimensions,
            dispatch_groups,
        })
    }
    pub(super) fn graph_names(&self) -> Vec<String> {
        self.graphs
            .iter()
            .map(|graph| graph.filename.clone())
            .collect()
    }
}

fn graphable_metric(name: &str) -> bool {
    // Per-destination CPU migration counters are useful retained evidence but
    // produce one low-value chart per core. Keep the aggregate migration plot.
    !name.contains("cpu_migration_destination[")
}

#[cfg(test)]
mod tests {
    use super::graphable_metric;

    #[test]
    fn per_core_cpu_migration_destinations_do_not_create_graphs() {
        assert!(!graphable_metric(
            "bpftrace-cpu-migrations.bpftrace.cpu-migrations.cpu_migration_destination[12]"
        ));
        assert!(graphable_metric(
            "bpftrace-cpu-migrations.bpftrace.cpu-migrations.cpu_migrations"
        ));
    }
}
