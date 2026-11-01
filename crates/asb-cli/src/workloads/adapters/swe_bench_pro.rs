//! Installed catalog for the public SWE-bench Pro source release.
//!
//! The pinned source publishes one Dockerfile directory per official
//! instance. Enumerating those directories after installation gives setup a
//! real, reproducible population without executing downloaded code or making
//! another network request.

use super::{SetupExtension, SourceBundle, WorkloadAdapter};
use crate::workloads::Trial;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const REVISION: &str = "ca10a60a5fcae51e6948ffe1485d4153d421e6c5";
const MARKER: &str = "scaleapi/SWE-bench_Pro-os@ca10a60a5fcae51e6948ffe1485d4153d421e6c5";
const SCHEMA_VERSION: u32 = 1;
const MAX_TASKS: usize = 100_000;

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

pub(super) struct SweBenchProAdapter;
pub(super) static ADAPTER: SweBenchProAdapter = SweBenchProAdapter;

impl WorkloadAdapter for SweBenchProAdapter {
    fn estimated_disk_mib(&self) -> u64 {
        8192
    }
    fn id(&self) -> &'static str {
        "swe-bench-pro"
    }

    fn source(&self) -> Option<&'static str> {
        Some(
            "https://raw.githubusercontent.com/scaleapi/SWE-bench_Pro-os/ca10a60a5fcae51e6948ffe1485d4153d421e6c5/README.md",
        )
    }

    fn source_bundle(&self) -> Option<SourceBundle> {
        Some(SourceBundle {
            archive: "https://github.com/scaleapi/SWE-bench_Pro-os/archive/ca10a60a5fcae51e6948ffe1485d4153d421e6c5.tar.gz",
            marker: MARKER,
            prompt: "Use the selected installed SWE-bench Pro source instance in `external-workload`. Do not download additional data during the run.\n",
        })
    }

    fn setup_extension(&self) -> Option<SetupExtension> {
        Some(SetupExtension::trials())
    }

    fn installed_available(&self, directory: &Path) -> Option<bool> {
        Some(installed_trials(directory).is_ok())
    }

    fn installed_trials(&self, directory: &Path) -> Result<Vec<Trial>, String> {
        installed_trials(directory)
    }

    fn finalize_install(&self, directory: &Path) -> Result<(), String> {
        write_catalog(directory, discover_trials(directory)?)
    }

    fn trial_ids(&self) -> Vec<Trial> {
        Vec::new()
    }

    fn prepare_source(&self, workloads: &Path, workspace: &Path) -> Result<usize, String> {
        super::prepare_external_source(self.id(), workloads, workspace)
    }
}

fn root(directory: &Path) -> std::path::PathBuf {
    directory.join("swe-bench-pro")
}

fn failure(reason: &str) -> String {
    format!(
        "swe-bench-pro has no valid installed official task catalog ({reason}); run `asb workload install swe-bench-pro`"
    )
}

fn installed_trials(directory: &Path) -> Result<Vec<Trial>, String> {
    let root = root(directory);
    if fs::read_to_string(root.join("ASB-BUNDLE"))
        .ok()
        .as_deref()
        .map(str::trim)
        != Some(MARKER)
    {
        return Err(failure("pinned source is not installed"));
    }
    let bytes = fs::read(root.join("catalog.json")).map_err(|_| failure("catalog is missing"))?;
    let catalog: Catalog = serde_json::from_slice(&bytes).map_err(|_| failure("invalid JSON"))?;
    if catalog.schema_version != SCHEMA_VERSION
        || catalog.workload != "swe-bench-pro"
        || catalog.revision != REVISION
        || !valid_tasks(&catalog.tasks)
    {
        return Err(failure("catalog does not match the pinned source"));
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
        return Err(failure("catalog differs from the installed source"));
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
    let root = root(directory);
    if fs::read_to_string(root.join("ASB-BUNDLE"))
        .ok()
        .as_deref()
        .map(str::trim)
        != Some(MARKER)
    {
        return Err(failure("pinned source is not installed"));
    }
    let instances = root.join("source/dockerfiles/base_dockerfile");
    let mut ids = fs::read_dir(&instances)
        .map_err(|_| failure("official instance directory is missing"))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (entry.path().is_dir()
                && name.starts_with("instance_")
                && entry.path().join("Dockerfile").is_file())
            .then(|| name.trim_start_matches("instance_").to_owned())
        })
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    if ids.is_empty() || ids.len() > MAX_TASKS || ids.iter().any(|id| !valid_id(id)) {
        return Err(failure("official instance layout is invalid"));
    }
    Ok(ids
        .into_iter()
        .map(|id| Trial {
            description: format!("SWE-bench Pro official instance {id}"),
            id,
        })
        .collect())
}

fn write_catalog(directory: &Path, trials: Vec<Trial>) -> Result<(), String> {
    let root = root(directory);
    let catalog = Catalog {
        schema_version: SCHEMA_VERSION,
        workload: "swe-bench-pro".into(),
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
        .map_err(|error| format!("cannot serialize SWE-bench Pro catalog: {error}"))?;
    let temporary = root.join(".catalog.json.tmp");
    fs::write(&temporary, bytes)
        .map_err(|error| format!("cannot write SWE-bench Pro catalog: {error}"))?;
    fs::rename(&temporary, root.join("catalog.json"))
        .map_err(|error| format!("cannot publish SWE-bench Pro catalog: {error}"))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 512
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}

fn valid_tasks(tasks: &[CatalogTask]) -> bool {
    !tasks.is_empty()
        && tasks.len() <= MAX_TASKS
        && tasks
            .iter()
            .all(|task| valid_id(&task.id) && !task.description.trim().is_empty())
        && tasks
            .iter()
            .map(|task| &task.id)
            .collect::<BTreeSet<_>>()
            .len()
            == tasks.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "asb-swe-bench-pro-catalog-{}",
            crate::workloads::TaskIdentity::local("swe-pro", None).attempt_id
        ));
        let installed = root.join("swe-bench-pro");
        for name in ["instance_org__repo-a1", "instance_org__repo-b2"] {
            let task = installed
                .join("source/dockerfiles/base_dockerfile")
                .join(name);
            fs::create_dir_all(&task).unwrap();
            fs::write(task.join("Dockerfile"), "FROM scratch\n").unwrap();
        }
        fs::write(installed.join("ASB-BUNDLE"), format!("{MARKER}\n")).unwrap();
        root
    }

    #[test]
    fn installed_source_generates_and_loads_official_instance_catalog() {
        let root = fixture();
        ADAPTER.finalize_install(&root).unwrap();
        let trials = ADAPTER.installed_trials(&root).unwrap();
        assert_eq!(
            trials
                .iter()
                .map(|trial| trial.id.as_str())
                .collect::<Vec<_>>(),
            ["org__repo-a1", "org__repo-b2"]
        );
        assert!(root.join("swe-bench-pro/catalog.json").is_file());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn category_placeholders_are_not_a_valid_catalog() {
        let root = fixture();
        fs::write(
            root.join("swe-bench-pro/catalog.json"),
            r#"{"schema_version":1,"workload":"swe-bench-pro","revision":"ca10a60a5fcae51e6948ffe1485d4153d421e6c5","tasks":[{"id":"bug-fix","description":"fake"}]}"#,
        )
        .unwrap();
        assert!(ADAPTER.installed_trials(&root).is_err());
        let _ = fs::remove_dir_all(root);
    }
}
