//! Public selection identity is available before any task environment starts.
use super::session::{DatasetIdentity, TaskIdentity};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

pub(crate) struct PlanningCatalog {
    pub requirements: std::collections::BTreeMap<String, crate::planning::TaskRequirements>,
    pub dataset: DatasetIdentity,
    pub task_ids: BTreeSet<String>,
    pub evaluation_kind: super::KnownEvaluationKind,
}

/// A selection is not an execution attempt or evidence of a passing baseline.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskSelection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<super::contracts::ModuleDescriptor>,
    pub workload: String,
    pub task_id: String,
    pub dataset: Option<DatasetIdentity>,
    pub evaluation_kind: String,
    pub population_size: Option<usize>,
}

impl TaskSelection {
    pub(crate) fn valid(&self) -> bool {
        let bounded = |value: &str, limit| !value.is_empty() && value.len() <= limit;
        bounded(&self.workload, 256)
            && self.module.as_ref().is_none_or(|module| {
                module.valid()
                    && module.id == self.workload
                    && module.dataset == self.dataset
                    && match module.support {
                        super::contracts::SupportLevel::OfficialAdapter => {
                            super::SavedEvaluationKind::classify(&self.evaluation_kind)
                                == super::SavedEvaluationKind::Known(
                                    super::KnownEvaluationKind::UpstreamHarness,
                                )
                        }
                        _ => self.evaluation_kind == module.protocol.id,
                    }
            })
            && bounded(&self.task_id, 256)
            && bounded(&self.evaluation_kind, 128)
            && self.dataset.is_some() == self.population_size.is_some()
            && self
                .population_size
                .is_none_or(|count| count > 0 && count <= 1_000_000)
            && self.dataset.as_ref().is_none_or(|dataset| {
                bounded(&dataset.source, 1024)
                    && bounded(&dataset.revision, 256)
                    && bounded(&dataset.split, 256)
            })
    }

    pub(crate) fn matches(&self, actual: &TaskIdentity) -> bool {
        self.workload == actual.workload
            && self.task_id == actual.task_id
            && self.dataset == actual.dataset
    }
}

pub(crate) fn describe(
    workload: &str,
    trial: Option<&str>,
    trial_set: Option<&str>,
    directory: &Path,
    catalog: Option<&PlanningCatalog>,
) -> Result<TaskSelection, String> {
    if let Some(catalog) = catalog {
        // A reference-smoke adapter may expose an installed task population
        // without claiming an official dataset/module contract.  Bind the
        // catalog for selection in that case, while retaining the strict
        // dataset identity check for modules that do declare one.
        if super::descriptor(workload)
            .and_then(|module| module.dataset)
            .is_some_and(|dataset| dataset != catalog.dataset)
        {
            return Err("installed dataset differs from the owning module descriptor".into());
        }
        let task = trial.ok_or_else(|| {
            format!("select an installed instance ID for `{workload}` before starting the sweep")
        })?;
        if !catalog.task_ids.contains(task) {
            return Err(format!(
                "selected instance `{task}` is absent from the installed `{workload}` catalog"
            ));
        }
    }
    if trial_set.is_some()
        && let Some(trial) = trial
    {
        super::selected_trial(workload, trial, trial_set, directory)?;
    }
    let selection = TaskSelection {
        module: super::descriptor(workload)
            .filter(|module| module.dataset.as_ref() == catalog.map(|catalog| &catalog.dataset)),
        workload: workload.into(),
        task_id: trial.unwrap_or(workload).into(),
        dataset: catalog.map(|catalog| catalog.dataset.clone()),
        evaluation_kind: catalog.map_or_else(
            || super::evaluation_kind(workload, directory).as_str().into(),
            |catalog| catalog.evaluation_kind.as_str().into(),
        ),
        population_size: catalog.map(|catalog| catalog.task_ids.len()),
    };
    if !selection.valid() {
        return Err(format!(
            "invalid or oversized selection identity for `{workload}`"
        ));
    }
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_module_binds_protocol_and_legacy_selection_remains_readable() {
        let current = describe("bug-fix", None, None, Path::new("."), None).unwrap();
        assert!(current.module.is_some());
        let mut changed = current.clone();
        changed.module.as_mut().unwrap().protocol.revision += 1;
        assert_ne!(current, changed);
        changed.module.as_mut().unwrap().id = "other".into();
        assert!(!changed.valid());
        changed = current.clone();
        changed.evaluation_kind = "upstream-harness".into();
        assert!(!changed.valid());
        let mut old = serde_json::to_value(&current).unwrap();
        old.as_object_mut().unwrap().remove("module");
        let old: TaskSelection = serde_json::from_value(old).unwrap();
        assert!(old.valid());
        assert!(old.module.is_none());
        assert_ne!(current, old);
    }

    #[test]
    fn official_catalog_mismatch_cannot_discard_the_protocol_binding() {
        let profile = super::super::descriptor("swe-bench-lite").unwrap();
        let mut catalog = PlanningCatalog {
            requirements: Default::default(),
            dataset: profile.dataset.unwrap(),
            task_ids: ["task-1".into()].into_iter().collect(),
            evaluation_kind: crate::workloads::KnownEvaluationKind::UpstreamHarness,
        };
        let selection = describe(
            "swe-bench-lite",
            Some("task-1"),
            None,
            Path::new("."),
            Some(&catalog),
        )
        .unwrap();
        assert!(selection.module.is_some());
        catalog.dataset.revision = "different".into();
        assert!(
            describe(
                "swe-bench-lite",
                Some("task-1"),
                None,
                Path::new("."),
                Some(&catalog)
            )
            .unwrap_err()
            .contains("descriptor")
        );
    }

    #[test]
    fn reference_smoke_catalogs_remain_selectable_without_an_official_binding() {
        let catalog = PlanningCatalog {
            requirements: Default::default(),
            dataset: DatasetIdentity {
                source: "sierra-research/tau-bench".into(),
                revision: "a".repeat(40),
                split: "airline+retail".into(),
            },
            task_ids: ["retail-36".into()].into_iter().collect(),
            evaluation_kind: crate::workloads::KnownEvaluationKind::AdapterSmoke,
        };
        let selection = describe(
            "tau-bench",
            Some("retail-36"),
            None,
            Path::new("."),
            Some(&catalog),
        )
        .unwrap();
        assert!(selection.module.is_none());
        assert_eq!(selection.dataset, Some(catalog.dataset));
        assert_eq!(selection.task_id, "retail-36");
    }

    #[test]
    fn selected_instances_are_validated_without_inventing_attempts_or_baselines() {
        let catalog = PlanningCatalog {
            requirements: Default::default(),
            dataset: DatasetIdentity {
                source: "official-source".into(),
                revision: "a".repeat(40),
                split: "test".into(),
            },
            task_ids: ["issue-1".into(), "issue-2".into()].into_iter().collect(),
            evaluation_kind: crate::workloads::KnownEvaluationKind::UpstreamHarness,
        };
        assert!(describe("suite", None, None, Path::new("."), Some(&catalog)).is_err());
        assert!(
            describe(
                "suite",
                Some("unknown"),
                None,
                Path::new("."),
                Some(&catalog)
            )
            .is_err()
        );
        let selection = describe(
            "suite",
            Some("issue-1"),
            None,
            Path::new("."),
            Some(&catalog),
        )
        .unwrap();
        assert_eq!(selection.population_size, Some(2));
        let mut task = TaskIdentity::local("suite", Some("issue-1"));
        task.dataset = Some(catalog.dataset);
        assert!(selection.matches(&task));
        task.attempt_id = "a different actual attempt".into();
        assert!(selection.matches(&task));
        task.dataset.as_mut().unwrap().revision = "b".repeat(40);
        assert!(!selection.matches(&task));
        let record = serde_json::to_value(selection).unwrap();
        assert!(record.get("attempt_id").is_none());
        assert!(record.get("baseline").is_none());
    }
}
