//! Installed τ-bench catalogue.  The pinned upstream benchmark addresses a
//! task by an environment and its one-based position, so those two values form
//! the stable ASB trial identifier.
use super::{SetupExtension, SourceBundle, WorkloadAdapter};
use crate::workloads::{PreparationContext, Trial, WorkloadSession};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const REVISION: &str = "59a200c6d575d595120f1cb70fea53cef0632f6b";
const AIRLINE_TASKS: usize = 50;
const RETAIL_TASKS: usize = 115;
const SOURCE_LIMIT: u64 = 2 * 1024 * 1024;
const CATALOG_SCHEMA_VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    schema_version: u32,
    workload: String,
    revision: String,
    tasks: Vec<CatalogTask>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CatalogTask {
    id: String,
    description: String,
}

pub(super) struct TauBenchAdapter;
pub(super) static ADAPTER: TauBenchAdapter = TauBenchAdapter;

impl WorkloadAdapter for TauBenchAdapter {
    fn id(&self) -> &'static str {
        "tau-bench"
    }

    fn setup_extension(&self) -> Option<SetupExtension> {
        Some(SetupExtension::trials())
    }

    fn source(&self) -> Option<&'static str> {
        Some(
            "https://raw.githubusercontent.com/sierra-research/tau-bench/59a200c6d575d595120f1cb70fea53cef0632f6b/README.md",
        )
    }

    fn source_bundle(&self) -> Option<SourceBundle> {
        Some(SourceBundle {
            archive: "https://github.com/sierra-research/tau-bench/archive/59a200c6d575d595120f1cb70fea53cef0632f6b.tar.gz",
            marker: "tau-bench/tau-bench@59a200c6d575d595120f1cb70fea53cef0632f6b",
            prompt: "Use the installed τ-bench source in `external-workload`. Inspect the selected airline or retail task and its local environment. Do not download additional data during the run.\n",
        })
    }

    fn installed_available(&self, directory: &Path) -> Option<bool> {
        Some(installed_trials(directory).is_ok())
    }

    fn installed_trials(&self, directory: &Path) -> Result<Vec<Trial>, String> {
        installed_trials(directory)
    }

    fn planning_catalog(
        &self,
        directory: &Path,
    ) -> Result<Option<crate::workloads::PlanningCatalog>, String> {
        let tasks = installed_trials(directory)?;
        Ok(Some(crate::workloads::PlanningCatalog {
            requirements: Default::default(),
            dataset: dataset_identity(),
            task_ids: tasks.into_iter().map(|task| task.id).collect(),
            evaluation_kind: crate::workloads::KnownEvaluationKind::AdapterSmoke,
        }))
    }

    fn finalize_install(&self, directory: &Path) -> Result<(), String> {
        // Enumerate the actual pinned source once, then publish the immutable
        // installed catalog consumed by setup, CLI completion, and planning.
        // This is deliberately local-only: installation owns the download;
        // enumeration never makes a second network request.
        write_catalog(directory, discover_trials(directory)?)?;
        remove_legacy_trial_set(directory)
    }

    fn trial_ids(&self) -> Vec<Trial> {
        Vec::new()
    }

    fn prepare_task(
        &self,
        context: &PreparationContext<'_>,
    ) -> Result<Box<dyn WorkloadSession>, String> {
        self.prepare_source(context.workload_directory, context.workspace)?;
        let trial = context
            .trial
            .ok_or_else(|| "tau-bench requires an installed trial selection".to_owned())?;
        let selected = installed_trials(context.workload_directory)?
            .into_iter()
            .find(|candidate| candidate.id == trial)
            .ok_or_else(|| format!("selected tau-bench trial `{trial}` is not installed"))?;
        let prompt = format!(
            "{}\n\nSelected installed τ-bench trial: {}\n{}\n",
            super::super::load(context.workload, context.workload_directory)?,
            selected.id,
            selected.description
        );
        Ok(Box::new(
            super::super::session::FileSession::new(
                context,
                prompt,
                self.evaluator(),
                self.evaluation_kind(context.workload),
            )
            .with_dataset(dataset_identity()),
        ))
    }

    fn prepare_source(&self, workloads: &Path, workspace: &Path) -> Result<usize, String> {
        super::prepare_external_source(self.id(), workloads, workspace)
    }
}

/// Early ASB releases exposed two synthetic τ-bench choices. Replace only
/// that exact generated set, never a user's subset, so the generic installer
/// can publish the real installed catalogue immediately afterward.
fn remove_legacy_trial_set(directory: &Path) -> Result<(), String> {
    let path = directory.join("tau-bench.workload.toml");
    let Ok(trial_set) = crate::workloads::read_trial_set(&path) else {
        return Ok(());
    };
    if trial_set.workload == "tau-bench"
        && trial_set
            .trials
            .iter()
            .map(|trial| trial.id.as_str())
            .eq(["smoke", "standard"])
    {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn installed_trials(directory: &Path) -> Result<Vec<Trial>, String> {
    let root = directory.join("tau-bench");
    let marker = root.join("ASB-BUNDLE");
    let expected_marker = format!("tau-bench/tau-bench@{REVISION}");
    if fs::read_to_string(&marker).ok().as_deref().map(str::trim) != Some(expected_marker.as_str())
    {
        return Err(
            "tau-bench has no valid installed task catalog; run `asb workload install tau-bench`"
                .into(),
        );
    }
    let path = root.join("catalog.json");
    let bytes = fs::read(&path).map_err(|_| {
        "tau-bench has no valid installed task catalog; run `asb workload install tau-bench`"
            .to_owned()
    })?;
    let catalog: Catalog = serde_json::from_slice(&bytes).map_err(|_| {
        "tau-bench has no valid installed task catalog; run `asb workload install tau-bench`"
            .to_owned()
    })?;
    let expected_revision = REVISION;
    if catalog.schema_version != CATALOG_SCHEMA_VERSION
        || catalog.workload != "tau-bench"
        || catalog.revision != expected_revision
        || !valid_catalog_tasks(&catalog.tasks)
    {
        return Err(
            "tau-bench installed task catalog does not match the pinned source; reinstall with `asb workload install tau-bench`".into(),
        );
    }
    let discovered = discover_trials(directory)?;
    if catalog
        .tasks
        .iter()
        .map(|task| task.id.as_str())
        .collect::<Vec<_>>()
        != discovered
            .iter()
            .map(|trial| trial.id.as_str())
            .collect::<Vec<_>>()
    {
        return Err(
            "tau-bench installed task catalog differs from the pinned source; reinstall with `asb workload install tau-bench`".into(),
        );
    }
    Ok(catalog
        .tasks
        .into_iter()
        .map(|task| Trial {
            id: task.id,
            description: task.description,
        })
        .collect())
}

fn discover_trials(directory: &Path) -> Result<Vec<Trial>, String> {
    let root = directory.join("tau-bench");
    let marker = root.join("ASB-BUNDLE");
    let expected_marker = format!("tau-bench/tau-bench@{REVISION}");
    if fs::read_to_string(&marker).ok().as_deref().map(str::trim) != Some(expected_marker.as_str())
    {
        return Err(
            "tau-bench has no valid installed task source; run `asb workload install tau-bench`"
                .into(),
        );
    }
    let mut trials = Vec::with_capacity(AIRLINE_TASKS + RETAIL_TASKS);
    for (domain, count) in [("airline", AIRLINE_TASKS), ("retail", RETAIL_TASKS)] {
        let path = root
            .join("source/tau_bench/envs")
            .join(domain)
            .join("tasks.py");
        let source = fs::read_to_string(&path).map_err(|_| {
            "tau-bench has no valid installed task catalog; run `asb workload install tau-bench`"
                .to_owned()
        })?;
        if source.len() as u64 > SOURCE_LIMIT || count_tasks(&source) != count {
            return Err("tau-bench installed source does not match the pinned task catalog; reinstall with `asb workload install tau-bench`".into());
        }
        trials.extend((1..=count).map(|number| Trial {
            id: format!("{domain}-{number}"),
            description: format!("τ-bench {domain} task {number}"),
        }));
    }
    Ok(trials)
}

fn write_catalog(directory: &Path, trials: Vec<Trial>) -> Result<(), String> {
    let root = directory.join("tau-bench");
    let catalog = Catalog {
        schema_version: CATALOG_SCHEMA_VERSION,
        workload: "tau-bench".into(),
        revision: REVISION.into(),
        tasks: trials
            .into_iter()
            .map(|trial| CatalogTask {
                id: trial.id,
                description: trial.description,
            })
            .collect(),
    };
    let bytes = serde_json::to_vec_pretty(&catalog)
        .map_err(|error| format!("cannot serialize tau-bench task catalog: {error}"))?;
    let temporary = root.join(".catalog.json.tmp");
    fs::write(&temporary, bytes).map_err(|error| {
        format!(
            "cannot write tau-bench task catalog {}: {error}",
            temporary.display()
        )
    })?;
    fs::rename(&temporary, root.join("catalog.json"))
        .map_err(|error| format!("cannot publish tau-bench task catalog: {error}"))
}

fn valid_catalog_tasks(tasks: &[CatalogTask]) -> bool {
    if tasks.len() != AIRLINE_TASKS + RETAIL_TASKS {
        return false;
    }
    let expected = [("airline", AIRLINE_TASKS), ("retail", RETAIL_TASKS)]
        .into_iter()
        .flat_map(|(domain, count)| (1..=count).map(move |number| format!("{domain}-{number}")))
        .collect::<BTreeSet<_>>();
    let actual = tasks
        .iter()
        .map(|task| task.id.clone())
        .collect::<BTreeSet<_>>();
    actual == expected && tasks.iter().all(|task| !task.description.trim().is_empty())
}

fn dataset_identity() -> crate::workloads::session::DatasetIdentity {
    crate::workloads::session::DatasetIdentity {
        source: "sierra-research/tau-bench".into(),
        revision: REVISION.into(),
        split: "airline+retail".into(),
    }
}

/// The pinned source represents every task as a four-space-indented mapping
/// in its top-level `tasks` list.  We intentionally validate that precise
/// source layout rather than executing a downloaded Python module.
fn count_tasks(source: &str) -> usize {
    source.lines().filter(|line| line == &"    {").count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(count: usize) -> String {
        std::iter::repeat_n("    {\n", count).collect()
    }

    #[test]
    fn installed_catalog_enumerates_every_pinned_domain_task() {
        let root = std::env::temp_dir().join(format!(
            "asb-tau-catalog-{}",
            crate::workloads::TaskIdentity::local("tau", None).attempt_id
        ));
        let tau = root.join("tau-bench");
        fs::create_dir_all(tau.join("source/tau_bench/envs/airline")).unwrap();
        fs::create_dir_all(tau.join("source/tau_bench/envs/retail")).unwrap();
        fs::write(
            tau.join("ASB-BUNDLE"),
            format!("tau-bench/tau-bench@{REVISION}\n"),
        )
        .unwrap();
        fs::write(
            tau.join("source/tau_bench/envs/airline/tasks.py"),
            source(AIRLINE_TASKS),
        )
        .unwrap();
        fs::write(
            tau.join("source/tau_bench/envs/retail/tasks.py"),
            source(RETAIL_TASKS),
        )
        .unwrap();
        ADAPTER.finalize_install(&root).unwrap();
        let trials = installed_trials(&root).unwrap();
        assert_eq!(trials.len(), AIRLINE_TASKS + RETAIL_TASKS);
        assert_eq!(trials.first().unwrap().id, "airline-1");
        assert_eq!(trials.last().unwrap().id, "retail-115");
        let catalog = ADAPTER.planning_catalog(&root).unwrap().unwrap();
        assert_eq!(catalog.task_ids.len(), AIRLINE_TASKS + RETAIL_TASKS);
        assert!(catalog.task_ids.contains("retail-115"));
        assert!(tau.join("catalog.json").is_file());
        assert_eq!(ADAPTER.setup_extension().unwrap().id, "trial");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_pinned_source_is_not_enumerated() {
        let root = std::env::temp_dir().join(format!(
            "asb-tau-invalid-{}",
            crate::workloads::TaskIdentity::local("tau", None).attempt_id
        ));
        let tau = root.join("tau-bench");
        fs::create_dir_all(tau.join("source/tau_bench/envs/airline")).unwrap();
        fs::create_dir_all(tau.join("source/tau_bench/envs/retail")).unwrap();
        fs::write(
            tau.join("ASB-BUNDLE"),
            format!("tau-bench/tau-bench@{REVISION}\n"),
        )
        .unwrap();
        fs::write(
            tau.join("source/tau_bench/envs/airline/tasks.py"),
            source(AIRLINE_TASKS - 1),
        )
        .unwrap();
        fs::write(
            tau.join("source/tau_bench/envs/retail/tasks.py"),
            source(RETAIL_TASKS),
        )
        .unwrap();
        assert!(ADAPTER.finalize_install(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn exact_legacy_generated_trial_set_is_replaced_but_a_user_subset_is_kept() {
        let root = std::env::temp_dir().join(format!(
            "asb-tau-legacy-{}",
            crate::workloads::TaskIdentity::local("tau", None).attempt_id
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("tau-bench.workload.toml");
        fs::write(
            &path,
            "workload='tau-bench'\n[[trials]]\nid='smoke'\n[[trials]]\nid='standard'\n",
        )
        .unwrap();
        remove_legacy_trial_set(&root).unwrap();
        assert!(!path.exists());
        fs::write(&path, "workload='tau-bench'\n[[trials]]\nid='retail-1'\n").unwrap();
        remove_legacy_trial_set(&root).unwrap();
        assert!(path.exists());
        let _ = fs::remove_dir_all(root);
    }
}
