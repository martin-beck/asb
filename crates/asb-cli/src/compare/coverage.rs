//! Coverage is separate from measured success and from process exit status.
use super::BenchmarkData;
use crate::Error;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// Missing slots inherit compatible observed protocol groups for denominators;
/// actual observations with absent dispatch remain a distinct unknown group.
#[derive(Clone, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(super) struct ProtocolGroup {
    fingerprint: Option<String>,
    dispatch: Option<crate::planning::Dispatch>,
}
impl ProtocolGroup {
    fn observed(row: &BenchmarkData) -> Self {
        Self {
            fingerprint: row.execution_plan_fingerprint.clone(),
            dispatch: row
                .execution_cohort
                .as_ref()
                .and_then(|cohort| cohort.dispatch),
        }
    }
}

fn metric_scope(benchmark: &BenchmarkData) -> Result<String, Error> {
    metric_scope_with_fingerprint(benchmark, &ProtocolGroup::observed(benchmark))
}

fn metric_scope_with_fingerprint(
    benchmark: &BenchmarkData,
    fingerprint: &ProtocolGroup,
) -> Result<String, Error> {
    serde_json::to_string(&(
        &benchmark.agent,
        &benchmark.provider,
        &benchmark.model,
        &benchmark.workload,
        benchmark
            .selection
            .as_ref()
            .and_then(|s| s.dataset.as_ref())
            .or_else(|| benchmark.task.as_ref().and_then(|t| t.dataset.as_ref())),
        &benchmark.module,
        fingerprint,
        group_kind(benchmark),
    ))
    .map_err(|e| Error::Run(e.to_string()))
}

/// Missing slots have no observed protocol identity. Count them conservatively
/// in each compatible observed cohort, preferring their own configuration's
/// replicas. Never relabel an actual legacy record with a known identity.
pub(super) fn fingerprints(
    benchmark: &BenchmarkData,
    benchmarks: &[BenchmarkData],
) -> Result<BTreeSet<ProtocolGroup>, Error> {
    if benchmark.result_source != super::ResultSource::Missing {
        return Ok(BTreeSet::from([ProtocolGroup::observed(benchmark)]));
    }
    let scope = metric_scope_with_fingerprint(benchmark, &ProtocolGroup::default())?;
    let mut local = BTreeSet::new();
    let mut compatible = BTreeSet::new();
    for peer in benchmarks
        .iter()
        .filter(|row| row.result_source != super::ResultSource::Missing)
    {
        if metric_scope_with_fingerprint(peer, &ProtocolGroup::default())? == scope {
            compatible.insert(ProtocolGroup::observed(peer));
            if peer.directory == benchmark.directory {
                local.insert(ProtocolGroup::observed(peer));
            }
        }
    }
    Ok(if !local.is_empty() {
        local
    } else if !compatible.is_empty() {
        compatible
    } else {
        BTreeSet::from([ProtocolGroup::default()])
    })
}

pub(super) fn metric_groups(
    benchmarks: &[BenchmarkData],
) -> Result<BTreeMap<String, crate::workloads::outcome::MetricSummary>, Error> {
    use crate::workloads::outcome::{EvaluationOutcome, MetricPolicy};
    let mut groups = BTreeMap::<String, (MetricPolicy, Vec<(String, EvaluationOutcome)>)>::new();
    let mut observed = BTreeMap::<String, Vec<MetricPolicy>>::new();
    for benchmark in benchmarks {
        if let Some(outcome) = &benchmark.benchmark_outcome {
            let policies = observed.entry(metric_scope(benchmark)?).or_default();
            if !policies.contains(&outcome.policy) {
                policies.push(outcome.policy.clone());
            }
        }
    }
    for benchmark in benchmarks {
        if !crate::workloads::resolution_eligible(group_kind(benchmark)) {
            continue;
        }
        let policy = benchmark
            .benchmark_outcome
            .as_ref()
            .map(|o| o.policy.clone())
            .unwrap_or_default();
        for fingerprint in fingerprints(benchmark, benchmarks)? {
            let scope = metric_scope_with_fingerprint(benchmark, &fingerprint)?;
            let task = benchmark
                .task
                .as_ref()
                .map(|t| t.task_id.clone())
                .or_else(|| benchmark.selection.as_ref().map(|s| s.task_id.clone()))
                .unwrap_or_else(|| benchmark.label.clone());
            let evaluation = benchmark
                .evaluation_completed
                .then_some(benchmark)
                .and_then(|benchmark| {
                    benchmark
                        .benchmark_outcome
                        .as_ref()
                        .map(|o| o.evaluation.clone())
                })
                .unwrap_or_else(|| {
                    if benchmark.evaluation_completed {
                        EvaluationOutcome::binary(
                            benchmark.metrics.get("success_rate") == Some(&1.0),
                        )
                    } else {
                        EvaluationOutcome::Pending {
                            reason: "no completed evaluation recorded".into(),
                        }
                    }
                });
            let mut policies = vec![policy.clone()];
            // An absent attempt still belongs in every observed policy's coverage.
            // Never make a custom score look complete by dropping missing rows.
            if benchmark.benchmark_outcome.is_none()
                && !benchmark.evaluation_completed
                && let Some(known) = observed.get(&scope)
            {
                policies = known.clone();
            }
            for policy in policies {
                let key = serde_json::to_string(&(&scope, &policy))
                    .map_err(|e| Error::Run(e.to_string()))?;
                groups
                    .entry(key)
                    .or_insert_with(|| (policy, Vec::new()))
                    .1
                    .push((task.clone(), evaluation.clone()));
            }
        }
    }
    Ok(groups
        .into_iter()
        .map(|(key, (policy, rows))| (key, crate::workloads::outcome::aggregate(policy, &rows)))
        .collect())
}

pub(super) fn metric_html(
    groups: &BTreeMap<String, crate::workloads::outcome::MetricSummary>,
) -> String {
    let mut html = String::from(
        "<section><h2>Benchmark metric policies</h2><p>Each policy declares whether missing rewards contribute zero or are excluded. Imputation does not change evaluator status; incomplete coverage is not an official population score. Uncertainty is the standard error over independent task means under that policy, unavailable with fewer than two tasks or unequal repetition counts for attempt means. Repetitions are not pass@k.</p><table><tr><th>Group</th><th>Policy / reward</th><th>Reward coverage</th><th>Tasks</th><th>Estimate</th><th>Task standard error</th></tr>",
    );
    for (key, group) in groups {
        let missing = match (&group.policy.missing_rewards, &group.policy.aggregation) {
            (
                crate::workloads::outcome::MissingRewardPolicy::Zero,
                crate::workloads::outcome::Aggregation::AttemptMean,
            ) => "missing rewards contribute zero; selected-attempt denominator",
            (
                crate::workloads::outcome::MissingRewardPolicy::Zero,
                crate::workloads::outcome::Aggregation::TaskMean,
            ) => "missing rewards contribute zero within selected task means",
            (crate::workloads::outcome::MissingRewardPolicy::Exclude, _) => {
                "observed-only rewards; missing excluded"
            }
        };
        let unit = group
            .policy
            .rewards
            .get(&group.policy.reward)
            .map_or("unknown", |spec| spec.unit.as_str());
        html.push_str(&format!("<tr><td>{}</td><td>{} v{} / {} ({})<br>{}</td><td>{}/{}</td><td>{}</td><td>{}</td><td>{}</td></tr>", super::escape(key), super::escape(&group.policy.id), group.policy.version, super::escape(&group.policy.reward), super::escape(unit), missing, group.rewarded_attempts, group.selected_attempts, group.unique_tasks, group.estimate.map_or_else(|| "Unavailable".into(), |v| format!("{v:.6}")), group.task_cluster_standard_error.map_or_else(|| "Unavailable".into(), |v| format!("{v:.6}"))));
    }
    html.push_str("</table></section>");
    html
}

pub(super) fn group_kind(benchmark: &BenchmarkData) -> &str {
    benchmark
        .selection
        .as_ref()
        .filter(|selection| selection.dataset.is_some())
        .map_or(&benchmark.workload_evaluation, |selection| {
            &selection.evaluation_kind
        })
}

#[derive(Default, Serialize)]
pub(super) struct EvaluationCoverage {
    pub selected_attempts: usize,
    pub eligible_selected_attempts: usize,
    pub evaluated_attempts: usize,
    pub unevaluated_attempts: usize,
    pub unscored_attempts: usize,
}

pub(super) fn evaluation_summary(benchmarks: &[BenchmarkData]) -> EvaluationCoverage {
    let mut summary = EvaluationCoverage {
        selected_attempts: benchmarks.len(),
        ..Default::default()
    };
    for benchmark in benchmarks {
        if crate::workloads::resolution_eligible(group_kind(benchmark)) {
            summary.eligible_selected_attempts += 1;
            summary.evaluated_attempts += usize::from(benchmark.evaluation_completed);
        } else {
            summary.unscored_attempts += 1;
        }
    }
    summary.unevaluated_attempts = summary.eligible_selected_attempts - summary.evaluated_attempts;
    summary
}

#[derive(Default, Serialize)]
pub(super) struct PopulationCoverage {
    label: String,
    selected_attempts: usize,
    evaluated_attempts: usize,
    resolved_attempts: usize,
    selected_tasks: usize,
    evaluated_tasks: usize,
    resolved_tasks: usize,
    population_size: usize,
    selection_coverage: f64,
    evaluation_coverage: f64,
    selected_resolution_lower_bound: f64,
    complete_population_evaluation: bool,
    full_population_resolution_rate: Option<f64>,
    #[serde(skip)]
    nonbinary: bool,
    #[serde(skip)]
    selected: BTreeSet<String>,
    #[serde(skip)]
    evaluated: BTreeSet<String>,
    #[serde(skip)]
    resolved: BTreeSet<String>,
}

impl PopulationCoverage {
    fn observe(
        &mut self,
        task: &str,
        population: usize,
        evaluated: bool,
        resolved: bool,
    ) -> Result<(), Error> {
        if population == 0 || (self.selected_attempts > 0 && self.population_size != population) {
            return Err(Error::Input(
                "inconsistent population size in sweep selection snapshot".into(),
            ));
        }
        self.population_size = population;
        self.selected_attempts += 1;
        self.selected.insert(task.into());
        if evaluated {
            self.evaluated_attempts += 1;
            self.evaluated.insert(task.into());
            if resolved {
                self.resolved_attempts += 1;
                self.resolved.insert(task.into());
            }
        }
        self.selected_tasks = self.selected.len();
        self.evaluated_tasks = self.evaluated.len();
        self.resolved_tasks = self.resolved.len();
        if self.selected_tasks > population {
            return Err(Error::Input(
                "selected task count exceeds the installed population in sweep snapshot".into(),
            ));
        }
        self.selection_coverage = self.selected_tasks as f64 / population as f64;
        self.evaluation_coverage = self.evaluated_attempts as f64 / self.selected_attempts as f64;
        self.selected_resolution_lower_bound =
            self.resolved_attempts as f64 / self.selected_attempts as f64;
        self.complete_population_evaluation =
            self.selected_tasks == population && self.evaluated_attempts == self.selected_attempts;
        // Repeated attempts are not pass@1 or an official one-attempt/task score.
        self.full_population_resolution_rate = (!self.nonbinary
            && self.complete_population_evaluation
            && self.selected_attempts == self.selected_tasks)
            .then_some(self.resolved_tasks as f64 / population as f64);
        Ok(())
    }
}

pub(super) fn populations(
    benchmarks: &[BenchmarkData],
) -> Result<BTreeMap<String, PopulationCoverage>, Error> {
    let mut result: BTreeMap<String, PopulationCoverage> = BTreeMap::new();
    for benchmark in benchmarks {
        let Some(selection) = benchmark.selection.as_ref() else {
            continue;
        };
        let (Some(dataset), Some(population)) = (&selection.dataset, selection.population_size)
        else {
            continue;
        };
        if benchmark.benchmark_outcome.as_ref().is_some_and(|o| {
            o.policy != crate::workloads::outcome::MetricPolicy::default()
                || (o.evaluation.completed() && o.evaluation.resolution().is_none())
        }) {
            continue;
        }
        for fingerprint in fingerprints(benchmark, benchmarks)? {
            let key = serde_json::to_string(&(
                &benchmark.agent,
                &benchmark.provider,
                &benchmark.model,
                &selection.workload,
                dataset,
                &benchmark.module,
                &fingerprint,
            ))
            .map_err(|error| Error::Run(error.to_string()))?;
            let group = result.entry(key).or_insert_with(|| PopulationCoverage {
                label: format!(
                    "{} / {} / {} / {}",
                    benchmark.agent, benchmark.provider, benchmark.model, selection.workload
                ),
                ..Default::default()
            });
            group.nonbinary |= benchmark
                .benchmark_outcome
                .as_ref()
                .is_some_and(|o| o.evaluation.completed() && o.evaluation.resolution().is_none());
            group.observe(
                &selection.task_id,
                population,
                benchmark.evaluation_completed,
                benchmark
                    .metrics
                    .get("success_rate")
                    .is_some_and(|rate| *rate == 1.0),
            )?;
        }
    }
    Ok(result)
}

pub(super) fn html_table(groups: &BTreeMap<String, PopulationCoverage>) -> String {
    if groups.is_empty() {
        return String::new();
    }
    let mut html = String::from(
        "<section><h2>Population coverage</h2><p>Counts describe the entire saved selection, independent of display filters.</p><div class=\"table-wrap\"><table><thead><tr><th>Agent / provider / model / workload</th><th>Selected tasks / population</th><th>Evaluated / selected attempts</th><th>Resolved unique tasks</th><th>Selected-attempt lower bound</th><th>Complete one-attempt population score</th></tr></thead><tbody>",
    );
    for group in groups.values() {
        let full = group.full_population_resolution_rate.map_or_else(
            || "Not available".into(),
            |rate| format!("{:.1}%", rate * 100.0),
        );
        html.push_str(&format!(
            "<tr><td>{}</td><td>{}/{}</td><td>{}/{}</td><td>{}</td><td>{:.1}%</td><td>{}</td></tr>",
            super::escape(&group.label),
            group.selected_tasks,
            group.population_size,
            group.evaluated_attempts,
            group.selected_attempts,
            group.resolved_tasks,
            group.selected_resolution_lower_bound * 100.0,
            full
        ));
    }
    html.push_str("</tbody></table></div></section>");
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(missing: bool) -> BenchmarkData {
        use crate::workloads::outcome::*;
        let task = crate::workloads::TaskIdentity::local("bug-fix", Some("case"));
        BenchmarkData {
            execution: None,
            observation: Default::default(),
            id: crate::compare::RowId {
                configuration: 0,
                batch: None,
                replica: if missing { 2 } else { 1 },
            },
            artifact_sources: BTreeMap::new(),
            planning: None,
            effective_budgets: None,
            legacy_evaluation_provenance: None,
            execution_plan_fingerprint: (!missing).then(|| "plan-P".into()),
            module: None,
            benchmark_outcome: (!missing).then(|| BenchmarkOutcome {
                schema_version: 1,
                execution_success: Some(true),
                evaluation: EvaluationOutcome::binary(true),
                cleanup_confirmed: Some(true),
                evidence: vec![],
                policy: MetricPolicy::default(),
                lineage: EvaluationLineage {
                    execution_attempt: task.attempt_id.clone(),
                    evaluation_id: "evaluation".into(),
                    supersedes: None,
                },
            }),
            result_source: if missing {
                crate::compare::ResultSource::Missing
            } else {
                crate::compare::ResultSource::Measurements
            },
            reconciled: false,
            replica_index: None,
            execution_cohort: None,
            label: "case".into(),
            directory: "same-configuration".into(),
            agent: "fixture".into(),
            workload: "bug-fix".into(),
            task: (!missing).then_some(task),
            selection: None,
            evaluation_completed: !missing,
            workload_evaluation: "bundled-fixture".into(),
            provider: "fixture".into(),
            model: "fixture".into(),
            metrics: if missing {
                BTreeMap::new()
            } else {
                BTreeMap::from([("success_rate".into(), 1.0)])
            },
            series: BTreeMap::new(),
            host_metadata: None,
            components: None,
            collector_documents: BTreeMap::new(),
            status: None,
            artifacts: vec![],
        }
    }

    #[test]
    fn runtime_full_and_partial_dispatch_populations_never_pool_scores() {
        let config = toml::from_str::<crate::benchmark::BenchmarkConfig>("agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='bug-fix'\nconcurrency=4\n").unwrap();
        let policy = crate::planning::resolve(&config).unwrap();
        let caps = crate::planning::EffectiveBudgets {
            execution_seconds: Some(900.0),
            ..Default::default()
        };
        let mut rows = Vec::new();
        for (index, admitted) in [4, 2, 2].into_iter().enumerate() {
            let mut next = row(false);
            next.id.configuration = index;
            next.execution_cohort = Some(crate::measurements::ExecutionCohort {
                id: format!("a{index}"),
                selected_replicas: 4,
                admitted_slots: if admitted == 4 {
                    vec![1, 2, 3, 4]
                } else {
                    vec![2, 4]
                },
                dispatch: Some(crate::planning::Dispatch::new(4, admitted).unwrap()),
            });
            next.execution_plan_fingerprint = Some(
                crate::planning::protocol_fingerprint_with_dispatch(
                    &config,
                    &policy,
                    caps,
                    4,
                    None,
                    Some(crate::planning::Dispatch::new(4, admitted).unwrap()),
                )
                .unwrap()
                .as_str()
                .into(),
            );
            rows.push(next);
        }
        let groups = metric_groups(&rows).unwrap();
        assert_eq!(groups.len(), 2);
        let mut counts: Vec<_> = groups
            .values()
            .map(|group| group.selected_attempts)
            .collect();
        counts.sort_unstable();
        assert_eq!(counts, vec![1, 2]);
        let mut unknown = row(false);
        unknown.execution_plan_fingerprint = rows[1].execution_plan_fingerprint.clone();
        // Even the exact same string cannot silently promote unknown metadata
        // into the known P=2 population.
        rows.push(unknown);
        assert_eq!(metric_groups(&rows).unwrap().len(), 3);
        let mut missing = row(true);
        missing.directory = rows[1].directory.clone();
        let before = serde_json::to_value(&rows[1].execution_cohort).unwrap();
        assert_eq!(fingerprints(&missing, &rows).unwrap().len(), 3);
        rows.push(missing);
        let groups = metric_groups(&rows).unwrap();
        let mut counts: Vec<_> = groups
            .values()
            .map(|group| group.selected_attempts)
            .collect();
        counts.sort_unstable();
        assert_eq!(counts, vec![2, 2, 3]);
        assert_eq!(
            serde_json::to_value(&rows[1].execution_cohort).unwrap(),
            before
        );
    }

    #[test]
    fn unattributed_reward_cannot_contribute_a_grade() {
        let mut unbound = row(false);
        unbound.evaluation_completed = false;
        let groups = metric_groups(&[unbound]).unwrap();
        let group = groups.values().next().unwrap();
        assert_eq!(group.rewarded_attempts, 0);
        assert_eq!(group.evaluated_attempts, 0);
        assert!(!group.complete);
    }

    #[test]
    fn missing_replica_stays_in_known_protocol_coverage_without_relabeling_legacy() {
        let rows = [row(false), row(true)];
        let groups = metric_groups(&rows).unwrap();
        assert_eq!(groups.len(), 1);
        let group = groups.values().next().unwrap();
        assert_eq!(group.selected_attempts, 2);
        assert_eq!(group.rewarded_attempts, 1);
        assert_eq!(group.estimate, Some(0.5));
        assert!(!group.complete);
        assert!(rows[1].execution_plan_fingerprint.is_none());
        let mut legacy = row(false);
        legacy.execution_plan_fingerprint = None;
        let groups = metric_groups(&[row(false), legacy]).unwrap();
        assert_eq!(
            groups.len(),
            2,
            "actual legacy evidence keeps unknown identity"
        );
    }

    #[test]
    fn partial_populations_and_replicas_never_become_complete_one_attempt_scores() {
        let mut group = PopulationCoverage::default();
        group.observe("one", 3, true, true).unwrap();
        group.observe("two", 3, false, false).unwrap();
        assert_eq!(group.selected_resolution_lower_bound, 0.5);
        assert!(group.full_population_resolution_rate.is_none());
        group.observe("three", 3, true, false).unwrap();
        assert_eq!(group.selected_tasks, 3);
        assert!(!group.complete_population_evaluation);
        let mut repeated = PopulationCoverage::default();
        repeated.observe("one", 1, true, false).unwrap();
        repeated.observe("one", 1, true, true).unwrap();
        assert_eq!(repeated.selected_tasks, 1);
        assert_eq!(repeated.selected_attempts, 2);
        assert_eq!(repeated.resolved_tasks, 1);
        assert!(repeated.complete_population_evaluation);
        assert!(repeated.full_population_resolution_rate.is_none());
    }

    #[test]
    fn complete_original_style_population_uses_official_task_outcomes_not_exit_status() {
        let mut group = PopulationCoverage::default();
        group.observe("one", 2, true, true).unwrap();
        group.observe("two", 2, true, false).unwrap();
        assert_eq!(group.full_population_resolution_rate, Some(0.5));
        assert!(group.observe("third", 2, true, true).is_err());
        assert!(
            PopulationCoverage::default()
                .observe("one", 0, false, false)
                .is_err()
        );
    }
}
