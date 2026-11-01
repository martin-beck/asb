//! Bounded, provider-free selection snapshot, published before execution.
use super::{MAX_CONFIGS, SweepFile};
use crate::benchmark::BenchmarkConfig;
use crate::workloads::{PlanningCatalog, TaskSelection};
use crate::{Error, project, read_config, workloads};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub(crate) const MAX_CONFIG_BYTES: usize = 32 * 1024 * 1024;

mod campaigns;
mod generation;
pub(crate) const MAX_PLAN_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SweepPlan {
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionIntent>,
    pub run_id: String,
    #[serde(default)]
    pub execution_mode: SavedExecutionMode,
    #[serde(default)]
    pub respect_record_timings: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduling: Option<super::scheduler::Scheduling>,
    pub entries: Vec<PlannedBenchmark>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<crate::planning::Resources>,
}

pub(crate) use crate::planning::ExecutionMode as SavedExecutionMode;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionIntent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_reservation: Option<crate::cassette::RecordingReservation>,
    pub repeat: u64,
    /// Shared seed applies to bundled configurations only; legacy stays unknown.
    pub seed: Option<u64>,
    #[serde(default)]
    pub user_seed: bool,
    pub selector: Option<crate::cassette::Selector>,
}

impl ExecutionIntent {
    pub(crate) fn matches_replay(&self, plan: &crate::runner::campaign::Plan) -> bool {
        if plan.mode != SavedExecutionMode::Replay {
            return true;
        }
        if let Some(selection) = &plan.replay_selection {
            Some(selection.seed) == self.seed && selection.selector == self.selector
        } else if self.user_seed {
            false
        } else {
            match self.selector {
                None => plan.replay_inputs.iter().all(|input| input.track.is_none()),
                Some(crate::cassette::Selector::Id(id)) => plan
                    .replay_inputs
                    .iter()
                    .all(|input| input.track == Some(id)),
                _ => false,
            }
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlannedBenchmark {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign: Option<crate::runner::campaign::Reference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<crate::planning::ResolvedPlan>,
    pub reference: String,
    pub configuration: BenchmarkConfig,
    pub selection: TaskSelection,
}

#[cfg(test)]
pub(crate) fn build(parent: &Path, manifest: &SweepFile) -> Result<SweepPlan, Error> {
    build_at(parent, manifest, Path::new("."))
}

pub(crate) fn build_at(
    parent: &Path,
    manifest: &SweepFile,
    base: &Path,
) -> Result<SweepPlan, Error> {
    build_with_catalog(parent, manifest, base, workloads::planning_catalog)
}

fn build_with_catalog(
    parent: &Path,
    manifest: &SweepFile,
    base: &Path,
    mut catalog_loader: impl FnMut(&str, &Path) -> Result<Option<PlanningCatalog>, String>,
) -> Result<SweepPlan, Error> {
    let mut catalogs: BTreeMap<(PathBuf, String), Option<PlanningCatalog>> = BTreeMap::new();
    let mut entries = Vec::with_capacity(manifest.configs.len());
    let mut bytes = 0usize;
    for reference in &manifest.configs {
        let path = parent.join(reference);
        let text = read_config(&path)?;
        bytes = bytes.saturating_add(text.len());
        if bytes > MAX_CONFIG_BYTES {
            return Err(Error::Input(
                "sweep configuration inputs exceed 32 MiB; select a smaller task/model batch"
                    .into(),
            ));
        }
        let value: toml::Value = toml::from_str(&text).map_err(|error| {
            Error::Input(format!(
                "cannot parse benchmark {}: {error}",
                path.display()
            ))
        })?;
        if value.get("configs").is_some() {
            return Err(Error::Input(format!(
                "{} is a nested sweep; entries must be normal benchmark TOMLs",
                path.display()
            )));
        }
        let config: BenchmarkConfig = value.try_into().map_err(|error| {
            Error::Input(format!("invalid benchmark {}: {error}", path.display()))
        })?;
        let (root, settings) = project::load(&path)?;
        let directory = project::environment_path("ASB_WORKLOAD_DIR", base)
            .unwrap_or_else(|| project::resolve(&root, &settings.workloads_dir));
        let key = (
            directory.canonicalize().unwrap_or(directory.clone()),
            config.workload.clone(),
        );
        if !catalogs.contains_key(&key) {
            catalogs.insert(
                key.clone(),
                catalog_loader(&config.workload, &directory).map_err(Error::Input)?,
            );
        }
        let selection = workloads::describe_selection(&config, &directory, catalogs[&key].as_ref())
            .map_err(Error::Input)?;
        let mut policy = crate::planning::resolve(&config)?;
        policy.task_requirements = catalogs[&key]
            .as_ref()
            .and_then(|catalog| catalog.requirements.get(&selection.task_id))
            .cloned();
        entries.push(PlannedBenchmark {
            campaign: None,
            policy: Some(policy),
            reference: reference.clone(),
            configuration: config,
            selection,
        });
    }
    Ok(SweepPlan {
        schema_version: 1,
        execution: None,
        run_id: workloads::TaskIdentity::local("sweep", None).attempt_id,
        execution_mode: SavedExecutionMode::Live,
        respect_record_timings: false,
        scheduling: None,
        entries,
        resources: manifest.resources,
    })
}

impl SweepPlan {
    pub(super) fn bind_execution(&mut self, intent: ExecutionIntent) -> Result<(), Error> {
        self.execution = Some(intent);
        self.schema_version = 2;
        self.validate_campaign_references()
    }

    /// Read a historical source index against its OWN retained configurations,
    /// not today's potentially different replay concurrency/collector choices.
    pub(crate) fn source_snapshot(directory: &Path, parent: &Path) -> Result<Option<Self>, Error> {
        let path = directory.join("plan.json");
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(crate::io_error(error)),
            Ok(_) => (),
        }
        let bytes = crate::runner::campaign::reading::bounded(&path, MAX_PLAN_BYTES as u64)?;
        let snapshot: Self = serde_json::from_slice(&bytes)
            .map_err(|_| Error::Input("invalid historical sweep source index".into()))?;
        let manifest = SweepFile {
            configs: snapshot
                .entries
                .iter()
                .map(|entry| entry.reference.clone())
                .collect(),
            resources: snapshot.resources,
            repeat: None,
        };
        let configurations = snapshot
            .entries
            .into_iter()
            .map(|entry| (parent.join(entry.reference), entry.configuration))
            .collect::<BTreeMap<_, _>>();
        Self::decode(&path, &bytes, parent, &manifest, |path| {
            configurations.get(path).cloned().ok_or_else(|| {
                Error::Input("historical sweep source configuration is missing".into())
            })
        })
        .map(Some)
    }
    pub(crate) fn resource_costs(
        &self,
        capacity: Option<crate::planning::Resources>,
    ) -> Result<Vec<crate::planning::Resources>, Error> {
        self.entries.iter().map(|entry| {
            let reservation = entry.policy.as_ref().and_then(|policy| policy.reservation);
            if capacity.is_some() && reservation.is_none() {
                return Err(Error::Input(format!("resource admission requires an explicit planning.reservation for {}; no task cost was guessed", entry.reference)));
            }
            let cost = reservation.unwrap_or_default().replicas(entry.configuration.concurrency)?;
            if let Some(capacity) = capacity {
                let reservation = reservation.expect("checked admission reservation");
                if capacity == crate::planning::Resources::default()
                    || (capacity.cpu_millis > 0 && reservation.cpu_millis == 0)
                    || (capacity.memory_mib > 0 && reservation.memory_mib == 0)
                    || (capacity.disk_mib > 0 && reservation.disk_mib == 0) {
                    return Err(Error::Input("resource admission cannot use empty reservations for budgeted dimensions".into()));
                }
                if let Some(requirements) = entry.policy.as_ref().and_then(|policy| policy.task_requirements.as_ref())
                    && (requirements.cpu_millis.is_some_and(|n| reservation.cpu_millis < n)
                        || requirements.memory_mib.is_some_and(|n| reservation.memory_mib < n)
                        || requirements.disk_mib.is_some_and(|n| reservation.disk_mib < n)) {
                        return Err(Error::Input("admission reservation is below module-declared task requirements".into()));
                }
            }
            if capacity.is_some_and(|capacity| !cost.fits(capacity)) {
                return Err(Error::Input(format!("replica resource reservation for {} exceeds sweep resources", entry.reference)));
            }
            Ok(cost)
        }).collect()
    }
    fn encode(&self) -> Result<Vec<u8>, Error> {
        let bytes =
            serde_json::to_vec_pretty(self).map_err(|error| Error::Io(error.to_string()))?;
        if bytes.len() > MAX_PLAN_BYTES {
            return Err(Error::Input(
                "sweep selection snapshot exceeds 64 MiB; select a smaller task/model batch".into(),
            ));
        }
        Ok(bytes)
    }

    fn publish(&self, directory: &Path, bytes: &[u8]) -> Result<(), Error> {
        let temporary = directory.join(format!(".plan-{}.json", self.run_id));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| Error::Io(format!("cannot create sweep plan: {error}")))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::rename(&temporary, directory.join("plan.json")))
            .map_err(|error| Error::Io(format!("cannot publish sweep plan: {error}")))
    }

    pub(crate) fn load(
        directory: &Path,
        parent: &Path,
        manifest: &SweepFile,
    ) -> Result<Option<Self>, Error> {
        let path = directory.join("plan.json");
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::Input(format!(
                    "cannot read sweep plan {}: {error}",
                    path.display()
                )));
            }
        };
        let mut bytes = Vec::new();
        file.take(MAX_PLAN_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| Error::Input(error.to_string()))?;
        if bytes.len() > MAX_PLAN_BYTES {
            return Err(Error::Input("sweep plan exceeds 64 MiB".into()));
        }
        Self::decode(&path, &bytes, parent, manifest, |path| {
            toml::from_str(&read_config(path)?).map_err(|error| Error::Input(error.to_string()))
        })
        .map(Some)
    }

    /// Decode one retained plan/configuration snapshot with the same validation
    /// used by ordinary loading, without refreshing caller-owned input bytes.
    pub(crate) fn decode(
        path: &Path,
        bytes: &[u8],
        parent: &Path,
        manifest: &SweepFile,
        mut configuration: impl FnMut(&Path) -> Result<BenchmarkConfig, Error>,
    ) -> Result<Self, Error> {
        if bytes.len() > MAX_PLAN_BYTES {
            return Err(Error::Input("sweep plan exceeds 64 MiB".into()));
        }
        let plan: Self = serde_json::from_slice(bytes).map_err(|error| {
            Error::Input(format!("invalid sweep plan {}: {error}", path.display()))
        })?;
        if !matches!(plan.schema_version, 1 | 2)
            || (plan.respect_record_timings && plan.execution_mode != SavedExecutionMode::Replay)
            || plan.run_id.is_empty()
            || plan.run_id.len() > 128
            || !plan
                .run_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
            || plan.entries.len() > MAX_CONFIGS
            || plan.entries.len() != manifest.configs.len()
        {
            return Err(Error::Input(
                "sweep plan does not match the selected manifest".into(),
            ));
        }
        plan.validate_campaign_references()?;
        for (entry, reference) in plan.entries.iter().zip(&manifest.configs) {
            let path = parent.join(reference);
            let config = configuration(&path)?;
            if !entry.selection.valid()
                || entry.reference != *reference
                || entry.configuration != config
                || entry.selection.workload != config.workload
                || entry.selection.task_id != config.trial.as_deref().unwrap_or(&config.workload)
            {
                return Err(Error::Input(format!(
                    "sweep plan no longer matches {}; use the original configurations or start a fresh sweep",
                    path.display()
                )));
            }
            // Loading historical reports remains possible. Resume separately
            // requires a matching resolved policy, including collector choices.
            if let Some(policy) = &entry.policy
                && (policy.schema_version != 1
                    || policy.input_modalities != ["text"]
                    || !matches!(policy.backend.as_str(), "native" | "local-docker"))
            {
                return Err(Error::Input("invalid saved execution policy".into()));
            }
        }
        if plan.resources != manifest.resources {
            return Err(Error::Input(
                "sweep resource capacity no longer matches saved plan".into(),
            ));
        }
        plan.resource_costs(plan.resources)?;
        if let Some(scheduling) = plan.scheduling {
            scheduling.validate(
                &plan
                    .entries
                    .iter()
                    .map(|entry| entry.configuration.concurrency)
                    .collect::<Vec<_>>(),
            )?;
        }
        Ok(plan)
    }

    /// Validate the whole index before any referenced plan or outcome is read.
    fn validate_campaign_references(&self) -> Result<(), Error> {
        let invalid =
            || Error::Input("invalid sweep campaign execution intent or references".into());
        let Some(intent) = &self.execution else {
            return if self.schema_version == 1
                && self.entries.iter().all(|entry| entry.campaign.is_none())
            {
                Ok(())
            } else {
                Err(invalid())
            };
        };
        if self.schema_version != 2
            || intent.repeat == 0
            || (intent.user_seed && intent.seed.is_none())
            || self.execution_mode == SavedExecutionMode::Unknown
            || (self.execution_mode != SavedExecutionMode::Replay
                && (intent.seed.is_some() || intent.selector.is_some()))
        {
            return Err(invalid());
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut slots = 0u64;
        let mut bytes = 0u64;
        for (index, entry) in self.entries.iter().enumerate() {
            let reference = entry.campaign.as_ref().ok_or_else(invalid)?;
            let name = reference
                .root
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(invalid)?;
            let expected = PathBuf::from(super::execution::result_name(
                index,
                Path::new(&entry.reference),
            ))
            .join("campaigns")
            .join(name);
            if reference.root.as_os_str() != expected.as_os_str()
                || name.is_empty()
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
                || !ids.insert(name.to_owned())
                || reference.bytes == 0
                || reference.bytes > 16 * 1024 * 1024
                || !crate::cassette::valid_digest(&reference.sha256)
            {
                return Err(invalid());
            }
            slots = intent
                .repeat
                .checked_mul(entry.configuration.concurrency as u64)
                .and_then(|count| slots.checked_add(count))
                .filter(|count| *count <= 65_533)
                .ok_or_else(invalid)?;
            bytes = bytes
                .checked_add(reference.bytes)
                .filter(|count| *count <= MAX_PLAN_BYTES as u64)
                .ok_or_else(invalid)?;
        }
        if let Some(capacity) = &intent.recording_reservation {
            if self.execution_mode != SavedExecutionMode::Record || capacity.references != slots {
                return Err(invalid());
            }
            capacity
                .validate(crate::cassette::ProofLimits::default())
                .map_err(|_| invalid())?;
        }
        Ok(())
    }
}

/// Preserve prior attempts, rather than mixing their measurements with a new
/// population. No data is deleted and invalid preflight never reaches here.
pub(crate) fn fresh_results(parent: &Path, stem: &str, plan: &SweepPlan) -> Result<PathBuf, Error> {
    // Reject an oversized snapshot before preserving/moving previous results.
    let bytes = plan.encode()?;
    let results = parent.join(format!("{stem}-results"));
    let report = parent.join("results").join(format!("{stem}-comparison"));
    let history = parent.join(format!("{stem}-history"));
    for path in [&results, &report, &history] {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
                return Err(Error::Input(format!(
                    "refused fresh sweep over non-directory or symlink {}",
                    path.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::Input(format!(
                    "cannot inspect sweep directory {}: {error}",
                    path.display()
                )));
            }
        }
    }
    if results.exists() || report.exists() {
        std::fs::create_dir_all(&history).map_err(|error| Error::Io(error.to_string()))?;
        let backup = history.join(&plan.run_id);
        std::fs::create_dir(&backup).map_err(|error| Error::Io(error.to_string()))?;
        if results.exists() {
            std::fs::rename(&results, backup.join(format!("{stem}-results"))).map_err(|error| {
                Error::Io(format!("cannot preserve prior sweep results: {error}"))
            })?;
        }
        if report.exists() {
            std::fs::create_dir(backup.join("results"))
                .map_err(|error| Error::Io(error.to_string()))?;
            std::fs::rename(
                &report,
                backup.join("results").join(format!("{stem}-comparison")),
            )
            .map_err(|error| {
                Error::Io(format!("cannot preserve prior sweep comparison: {error}"))
            })?;
        }
    }
    std::fs::create_dir_all(&results).map_err(|error| {
        Error::Io(format!(
            "cannot create sweep results {}: {error}",
            results.display()
        ))
    })?;
    plan.publish(&results, &bytes)?;
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    #[ignore = "requires explicit installed original catalogs; no agent, provider or Docker execution"]
    fn installed_original_populations_plan_and_report_all_instances_without_execution() {
        let root = PathBuf::from(
            std::env::var_os("ASB_TEST_SWECATALOG_ROOT").expect("installed original project"),
        );
        let (project_root, settings) = project::load(&root.join("qualification.toml")).unwrap();
        let workloads = project::resolve(&project_root, &settings.workloads_dir);
        let directory =
            root.join(workloads::TaskIdentity::local("full-population-plan", None).attempt_id);
        std::fs::create_dir(&directory).unwrap();
        let populations = [
            ("swe-bench", 2294),
            ("swe-bench-lite", 300),
            ("swe-bench-verified", 500),
            ("terminal-bench-1.0", 80),
            ("terminal-bench-2.0", 89),
            ("terminal-bench-2.1", 89),
        ];
        let mut manifest = SweepFile {
            repeat: None,
            resources: None,
            configs: Vec::new(),
        };
        for (workload, expected) in populations {
            let catalog = workloads::planning_catalog(workload, &workloads)
                .unwrap()
                .unwrap();
            assert_eq!(catalog.task_ids.len(), expected, "{workload}");
            for task in &catalog.task_ids {
                let reference = format!("task-{:05}.toml", manifest.configs.len());
                let config: BenchmarkConfig = toml::from_str(&format!(
                    "agent='pi'\nprovider='openrouter'\nmodel='population-planning-fixture'\nworkload='{workload}'\ntrial={task:?}\n"
                )).unwrap();
                std::fs::write(
                    directory.join(&reference),
                    toml::to_string(&config).unwrap(),
                )
                .unwrap();
                manifest.configs.push(reference);
            }
        }
        assert_eq!(manifest.configs.len(), 3263);
        let manifest_path = directory.join("population.toml");
        std::fs::write(&manifest_path, toml::to_string(&manifest).unwrap()).unwrap();
        let plan = build(&directory, &manifest).unwrap();
        assert_eq!(plan.entries.len(), 3263);
        let results = fresh_results(&directory, "population", &plan).unwrap();
        let loaded = SweepPlan::load(&results, &directory, &manifest)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.entries.len(), 3263);
        crate::compare::run(manifest_path.to_str().unwrap(), &mut Vec::new()).unwrap();
        let report: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.join("results/population-comparison/comparison.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(report["benchmarks"].as_array().unwrap().len(), 3263);
        assert_eq!(report["evaluation_coverage"]["selected_attempts"], 3263);
        assert_eq!(report["evaluation_coverage"]["evaluated_attempts"], 0);
        assert_eq!(report["evaluation_coverage"]["unevaluated_attempts"], 3263);
        let groups = report["population_coverage"].as_object().unwrap();
        assert_eq!(groups.len(), 5);
        for (workload, expected) in populations {
            let entry = groups
                .values()
                .find(|group| group["label"].as_str().unwrap().ends_with(workload))
                .unwrap();
            assert_eq!(entry["population_size"], expected);
            assert_eq!(entry["selected_tasks"], expected);
            assert_eq!(entry["selection_coverage"], 1.0);
            assert_eq!(entry["evaluated_attempts"], 0);
            assert_eq!(entry["complete_population_evaluation"], false);
            assert!(entry["full_population_resolution_rate"].is_null());
        }
        eprintln!(
            "Full original population planning/reporting qualification (no execution): {}",
            directory.display()
        );
    }

    struct Project(PathBuf);
    impl Project {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(workloads::TaskIdentity::local("selection-plan", None).attempt_id);
            std::fs::create_dir_all(root.join(".asb")).unwrap();
            std::fs::write(
                root.join(".asb/project.toml"),
                toml::to_string(&project::ProjectConfig::default()).unwrap(),
            )
            .unwrap();
            Self(root)
        }
        fn manifest(&self) -> SweepFile {
            for task in ["one", "two"] {
                std::fs::write(self.0.join(format!("{task}.toml")), format!("agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='fixture-suite'\ntrial='{task}'\n")).unwrap();
            }
            std::fs::write(
                self.0.join("sweep.toml"),
                "configs=['one.toml','two.toml']\n",
            )
            .unwrap();
            SweepFile {
                repeat: None,
                resources: None,
                configs: vec!["one.toml".into(), "two.toml".into()],
            }
        }
        fn plan(&self, manifest: &SweepFile) -> SweepPlan {
            let mut calls = 0;
            let plan = build_with_catalog(&self.0, manifest, Path::new("."), |name, _| {
                calls += 1;
                assert_eq!(name, "fixture-suite");
                Ok(Some(PlanningCatalog {
                    requirements: Default::default(),
                    dataset:serde_json::from_value(json!({"source":"official-fixture","revision":"a".repeat(40),"split":"test"})).unwrap(),
                    task_ids:["one".into(), "two".into()].into_iter().collect(), evaluation_kind:crate::workloads::KnownEvaluationKind::UpstreamHarness,
                }))
            }).unwrap();
            assert_eq!(
                calls, 1,
                "installed catalogs must be read once per workload, not once per task"
            );
            plan
        }
        fn grade(&self, directory: &Path, task: &str, resolved: bool) {
            std::fs::create_dir_all(directory).unwrap();
            std::fs::write(directory.join("run.json"), serde_json::to_vec(&json!({
                "schema_version":2,"status":"failed","success":false,"process_success":false,
                "agent":"pi","provider":"openrouter","model":"fixture","workload":"fixture-suite",
                "task":{"workload":"fixture-suite","task_id":task,"attempt_id":format!("actual-{task}"),
                    "dataset":{"source":"official-fixture","revision":"a".repeat(40),"split":"test"}},
                "workload_evaluation":"upstream-harness", "workload_success":resolved,
                "workload_baseline":{"kind":"upstream-task-definition","base_commit":"b".repeat(40)}
            })).unwrap()).unwrap();
        }
    }
    impl Drop for Project {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn plans_capture_dataset_identity_and_reject_changed_source_configuration() {
        let project = Project::new();
        let manifest = project.manifest();
        let plan = project.plan(&manifest);
        let results = fresh_results(&project.0, "sweep", &plan).unwrap();
        let loaded = SweepPlan::load(&results, &project.0, &manifest)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.entries[0].selection.task_id, "one");
        assert_eq!(loaded.entries[1].selection.population_size, Some(2));
        let value = serde_json::to_value(&loaded.entries[0].selection).unwrap();
        assert!(value.get("attempt_id").is_none());
        assert!(value.get("baseline").is_none());
        let path = project.0.join("one.toml");
        let changed = std::fs::read_to_string(&path)
            .unwrap()
            .replace("model='fixture'", "model='changed'");
        std::fs::write(path, changed).unwrap();
        assert!(
            SweepPlan::load(&results, &project.0, &manifest)
                .unwrap_err()
                .to_string()
                .contains("no longer matches")
        );
    }

    #[test]
    fn campaign_index_rejects_every_invalid_reference_before_configuration_or_artifact_reads() {
        let project = Project::new();
        let manifest = project.manifest();
        let mut plan = project.plan(&manifest);
        for (index, entry) in plan.entries.iter_mut().enumerate() {
            entry.campaign = Some(crate::runner::campaign::Reference {
                root: PathBuf::from(super::super::execution::result_name(
                    index,
                    Path::new(&entry.reference),
                ))
                .join("campaigns")
                .join(format!("campaign-{index}")),
                bytes: 123,
                sha256: "a".repeat(64),
            });
        }
        plan.bind_execution(ExecutionIntent {
            recording_reservation: None,
            repeat: 3,
            seed: None,
            user_seed: false,
            selector: None,
        })
        .unwrap();
        let original = serde_json::to_value(&plan).unwrap();
        let path = project.0.join("plan.json");
        let bytes = serde_json::to_vec(&original).unwrap();
        assert!(
            SweepPlan::decode(&path, &bytes, &project.0, &manifest, |path| {
                toml::from_str(&read_config(path)?)
                    .map_err(|failure| Error::Input(failure.to_string()))
            })
            .is_ok()
        );
        for mutation in [
            "schema",
            "missing-intent",
            "missing-reference",
            "parent",
            "prefix",
            "duplicate-id",
            "zero-bytes",
            "large-bytes",
            "digest",
            "zero-repeat",
            "overflow",
            "seed",
            "selector",
            "seed-origin",
        ] {
            let mut value = original.clone();
            match mutation {
                "schema" => value["schema_version"] = json!(1),
                "missing-intent" => value["execution"] = Value::Null,
                "missing-reference" => value["entries"][1]["campaign"] = Value::Null,
                "parent" => {
                    value["entries"][0]["campaign"]["root"] =
                        json!("000-one/campaigns/../campaign-0")
                }
                "prefix" => {
                    value["entries"][0]["campaign"]["root"] = json!("001-two/campaigns/campaign-0")
                }
                "duplicate-id" => {
                    value["entries"][1]["campaign"]["root"] = json!("001-two/campaigns/campaign-0")
                }
                "zero-bytes" => value["entries"][0]["campaign"]["bytes"] = json!(0),
                "large-bytes" => {
                    value["entries"][0]["campaign"]["bytes"] = json!(16 * 1024 * 1024 + 1)
                }
                "digest" => value["entries"][0]["campaign"]["sha256"] = json!("Z".repeat(64)),
                "zero-repeat" => value["execution"]["repeat"] = json!(0),
                "overflow" => value["execution"]["repeat"] = json!(u64::MAX),
                "seed" => value["execution"]["seed"] = json!(1),
                "seed-origin" => value["execution"]["user_seed"] = json!(true),
                _ => value["execution"]["selector"] = json!({"id":1}),
            }
            let bytes = serde_json::to_vec(&value).unwrap();
            assert!(SweepPlan::decode(&path, &bytes, &project.0, &manifest, |_| {
                panic!("{mutation} reached configuration/artifact lookup before global index rejection")
            }).is_err(), "{mutation}");
        }
    }

    #[test]
    fn saved_modes_keep_wire_values_legacy_default_and_replay_only_timing() {
        for (mode, wire) in [
            (SavedExecutionMode::Live, "live"),
            (SavedExecutionMode::Record, "record"),
            (SavedExecutionMode::Replay, "replay"),
            (SavedExecutionMode::Unknown, "unknown"),
        ] {
            assert_eq!(mode.as_str(), wire);
            assert_eq!(serde_json::to_value(mode).unwrap(), wire);
            assert_eq!(
                serde_json::from_value::<SavedExecutionMode>(json!(wire)).unwrap(),
                mode
            );
        }
        for value in [json!(null), json!("REPLAY"), json!("not-a-mode"), json!(2)] {
            assert!(serde_json::from_value::<SavedExecutionMode>(value).is_err());
        }
        let project = Project::new();
        let manifest = project.manifest();
        let plan = project.plan(&manifest);
        let results = fresh_results(&project.0, "sweep", &plan).unwrap();
        let mut value = serde_json::to_value(plan).unwrap();
        value.as_object_mut().unwrap().remove("execution_mode");
        std::fs::write(
            results.join("plan.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        assert_eq!(
            SweepPlan::load(&results, &project.0, &manifest)
                .unwrap()
                .unwrap()
                .execution_mode,
            SavedExecutionMode::Unknown
        );
        for mode in ["live", "record", "replay", "unknown"] {
            for timing in [false, true] {
                value["execution_mode"] = json!(mode);
                value["respect_record_timings"] = json!(timing);
                std::fs::write(
                    results.join("plan.json"),
                    serde_json::to_vec(&value).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    SweepPlan::load(&results, &project.0, &manifest).is_ok(),
                    !timing || mode == "replay"
                );
            }
        }
    }

    #[test]
    fn malformed_snapshots_are_rejected_and_legacy_results_do_not_invent_a_plan() {
        let project = Project::new();
        let manifest = project.manifest();
        assert!(
            SweepPlan::load(&project.0.join("sweep-results"), &project.0, &manifest)
                .unwrap()
                .is_none()
        );
        let plan = project.plan(&manifest);
        let results = fresh_results(&project.0, "sweep", &plan).unwrap();
        let original = serde_json::to_value(&plan).unwrap();
        for (pointer, replacement) in [
            ("/schema_version", json!(999)),
            ("/run_id", json!("../not-an-attempt")),
            ("/execution_mode", json!("not-a-mode")),
            ("/entries/0/selection/task_id", json!("different")),
            ("/entries/0/selection/population_size", json!(0)),
            ("/entries/0/selection/dataset", serde_json::Value::Null),
        ] {
            let mut malformed = original.clone();
            *malformed.pointer_mut(pointer).unwrap() = replacement;
            std::fs::write(
                results.join("plan.json"),
                serde_json::to_vec(&malformed).unwrap(),
            )
            .unwrap();
            assert!(
                SweepPlan::load(&results, &project.0, &manifest).is_err(),
                "{pointer}"
            );
        }
        for scheduling in [
            json!({"jobs":0,"max_active_attempts":1}),
            json!({"jobs":33,"max_active_attempts":1}),
            json!({"jobs":2,"max_active_attempts":0}),
            json!({"jobs":1,"max_active_attempts":65}),
        ] {
            let mut malformed = original.clone();
            malformed["scheduling"] = scheduling;
            std::fs::write(
                results.join("plan.json"),
                serde_json::to_vec(&malformed).unwrap(),
            )
            .unwrap();
            assert!(SweepPlan::load(&results, &project.0, &manifest).is_err());
        }
    }

    #[test]
    fn comparison_retains_unattempted_instances_and_never_publishes_partial_full_scores() {
        let project = Project::new();
        let manifest = project.manifest();
        let plan = project.plan(&manifest);
        let results = fresh_results(&project.0, "sweep", &plan).unwrap();
        project.grade(&results.join("000-one/measurements"), "one", true);
        let path = project.0.join("sweep.toml");
        crate::compare::run(path.to_str().unwrap(), &mut Vec::new()).unwrap();
        let report_path = project.0.join("results/sweep-comparison/comparison.json");
        let partial: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
        assert_eq!(
            partial["evaluation_coverage"]["eligible_selected_attempts"],
            2
        );
        assert_eq!(partial["evaluation_coverage"]["evaluated_attempts"], 1);
        assert_eq!(partial["evaluation_coverage"]["unevaluated_attempts"], 1);
        assert!(
            partial["benchmarks"][1]["task"].is_null(),
            "do not invent an actual attempt"
        );
        assert_eq!(partial["benchmarks"][1]["selection"]["task_id"], "two");
        let group = partial["population_coverage"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(group["selected_tasks"], 2);
        assert_eq!(group["selected_resolution_lower_bound"], 0.5);
        assert!(group["full_population_resolution_rate"].is_null());
        let resolution = partial["resolution_groups"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(resolution["task_count"], 2);
        assert_eq!(resolution["resolved_count"], 1);
        let html = std::fs::read_to_string(project.0.join("results/sweep-comparison/report.html"))
            .unwrap();
        assert!(html.contains("1/2 eligible selected attempts evaluated"));
        assert!(html.contains("<h2>Population coverage</h2>"));
        assert!(html.contains("<td>2/2</td><td>1/2</td>"));
        assert!(html.contains("Not available"));
        let first = results.join("000-one/measurements/run.json");
        let mut wrong_grader: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&first).unwrap()).unwrap();
        wrong_grader["workload_evaluation"] = json!("bundled-fixture");
        wrong_grader["workload_baseline"] = json!({"kind":"measured","passed":false});
        std::fs::write(&first, serde_json::to_vec(&wrong_grader).unwrap()).unwrap();
        assert!(
            crate::compare::run(path.to_str().unwrap(), &mut Vec::new())
                .unwrap_err()
                .to_string()
                .contains("planned evaluator")
        );
        project.grade(&results.join("000-one/measurements"), "one", true);
        project.grade(&results.join("001-two/measurements"), "two", false);
        crate::compare::run(path.to_str().unwrap(), &mut Vec::new()).unwrap();
        let complete: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
        let group = complete["population_coverage"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(group["full_population_resolution_rate"], 0.5);
        assert_eq!(group["complete_population_evaluation"], true);
        let second = results.join("001-two/measurements/run.json");
        let mut duplicate: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&second).unwrap()).unwrap();
        duplicate["task"]["attempt_id"] = json!("actual-one");
        std::fs::write(&second, serde_json::to_vec(&duplicate).unwrap()).unwrap();
        assert!(
            crate::compare::run(path.to_str().unwrap(), &mut Vec::new())
                .unwrap_err()
                .to_string()
                .contains("duplicate actual attempt")
        );
        project.grade(
            &results.join("001-two/measurements"),
            "wrong-instance",
            true,
        );
        assert!(
            crate::compare::run(path.to_str().unwrap(), &mut Vec::new())
                .unwrap_err()
                .to_string()
                .contains("planned dataset instance")
        );
    }

    #[test]
    fn new_attempts_preserve_old_results_and_reports_instead_of_mixing_evidence() {
        let project = Project::new();
        let manifest = project.manifest();
        let plan = project.plan(&manifest);
        std::fs::create_dir_all(project.0.join("sweep-results")).unwrap();
        std::fs::write(project.0.join("sweep-results/keep"), "previous results").unwrap();
        std::fs::create_dir_all(project.0.join("results/sweep-comparison")).unwrap();
        std::fs::write(
            project.0.join("results/sweep-comparison/report.html"),
            "<a href='../../sweep-results/keep'>previous report</a>",
        )
        .unwrap();
        let results = fresh_results(&project.0, "sweep", &plan).unwrap();
        let backup = project.0.join("sweep-history").join(&plan.run_id);
        assert_eq!(
            std::fs::read_to_string(backup.join("sweep-results/keep")).unwrap(),
            "previous results"
        );
        assert_eq!(
            std::fs::read_to_string(backup.join("results/sweep-comparison/report.html")).unwrap(),
            "<a href='../../sweep-results/keep'>previous report</a>"
        );
        assert_eq!(
            backup
                .join("results/sweep-comparison/../../sweep-results/keep")
                .canonicalize()
                .unwrap(),
            backup.join("sweep-results/keep").canonicalize().unwrap()
        );
        assert!(results.join("plan.json").is_file());
        assert!(!results.join("keep").exists());
    }

    #[cfg(unix)]
    #[test]
    fn history_symlinks_are_rejected_without_moving_existing_results() {
        let project = Project::new();
        let manifest = project.manifest();
        let plan = project.plan(&manifest);
        std::fs::create_dir_all(project.0.join("sweep-results")).unwrap();
        std::fs::write(project.0.join("sweep-results/keep"), "preserved").unwrap();
        std::fs::create_dir(project.0.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(project.0.join("elsewhere"), project.0.join("sweep-history"))
            .unwrap();
        assert!(fresh_results(&project.0, "sweep", &plan).is_err());
        assert!(project.0.join("sweep-results/keep").is_file());
        assert_eq!(
            std::fs::read_dir(project.0.join("elsewhere"))
                .unwrap()
                .count(),
            0
        );
    }
}
