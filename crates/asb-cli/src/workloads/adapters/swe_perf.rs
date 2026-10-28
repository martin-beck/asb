//! Pinned official SWE-Perf instance catalog.

use super::{SetupExtension, WorkloadAdapter};
use crate::workloads::{InstallationContext, Trial};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;

const PAGE_ZERO: &str = "https://datasets-server.huggingface.co/rows?dataset=SWE-Perf%2FSWE-Perf&config=default&split=test&offset=0&length=100&revision=6298f128c550e92ac25af17653cf092224046516";
const PAGE_ONE: &str = "https://datasets-server.huggingface.co/rows?dataset=SWE-Perf%2FSWE-Perf&config=default&split=test&offset=100&length=100&revision=6298f128c550e92ac25af17653cf092224046516";
const MARKER: &str = "SWE-Perf/SWE-Perf@6298f128c550e92ac25af17653cf092224046516";
const LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    schema_version: u32,
    marker: String,
    tasks: Vec<Task>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Task {
    id: String,
    description: String,
}

#[derive(Deserialize)]
struct DatasetPage {
    num_rows_total: usize,
    rows: Vec<DatasetRow>,
}

#[derive(Deserialize)]
struct DatasetRow {
    row: Instance,
}

#[derive(Deserialize)]
struct Instance {
    instance_id: String,
}

pub(super) struct SwePerfAdapter;
pub(super) static ADAPTER: SwePerfAdapter = SwePerfAdapter;

impl WorkloadAdapter for SwePerfAdapter {
    fn id(&self) -> &'static str {
        "swe-perf"
    }
    fn source(&self) -> Option<&'static str> {
        Some(PAGE_ZERO)
    }
    fn setup_extension(&self) -> Option<SetupExtension> {
        Some(SetupExtension::trials())
    }
    fn installed_available(&self, directory: &Path) -> Option<bool> {
        Some(read(directory).is_ok())
    }
    fn installed_trials(&self, directory: &Path) -> Result<Vec<Trial>, String> {
        read(directory)
    }
    fn trial_ids(&self) -> Vec<Trial> {
        Vec::new()
    }

    fn install(
        &self,
        context: &InstallationContext<'_>,
        output: &mut dyn Write,
        progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
    ) -> crate::install::InstallResult<bool> {
        let root = context.workloads_root.join(self.id());
        fs::create_dir_all(&root).map_err(|error| crate::Error::Io(error.to_string()))?;
        progress(20, crate::install::ProgressStage::Downloading);
        crate::install::install_source_bounded_in(
            context.process,
            PAGE_ZERO,
            &root.join("rows-000.json"),
            "SWE-Perf official catalog page 1",
            LIMIT,
        )?;
        progress(60, crate::install::ProgressStage::Downloading);
        crate::install::install_source_bounded_in(
            context.process,
            PAGE_ONE,
            &root.join("rows-100.json"),
            "SWE-Perf official catalog page 2",
            LIMIT,
        )?;
        fs::write(root.join("ASB-DATASET"), format!("{MARKER}\n"))
            .map_err(|error| crate::Error::Io(error.to_string()))?;
        write_catalog(context.workloads_root).map_err(crate::Error::Input)?;
        fs::write(
            context.workloads_root.join("swe-perf.txt"),
            "Use the selected installed SWE-Perf instance in `external-workload`.\n",
        )
        .map_err(|error| crate::Error::Io(error.to_string()))?;
        crate::workloads::materialize_external_adapter(self.id(), context.workloads_root)
            .map_err(|error| crate::Error::Io(error.to_string()))?;
        crate::workloads::write_default_trial_set(self.id(), context.workloads_root)
            .map_err(|error| crate::Error::Io(error.to_string()))?;
        writeln!(
            output,
            "Installed SWE-Perf official task catalog at {}",
            root.display()
        )
        .map_err(|error| crate::Error::Io(error.to_string()))?;
        progress(100, crate::install::ProgressStage::Ready);
        Ok(true)
    }

    fn prepare_source(&self, workloads: &Path, workspace: &Path) -> Result<usize, String> {
        super::prepare_external_source(self.id(), workloads, workspace)
    }
}

fn parse_page(path: &Path) -> Result<DatasetPage, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() > LIMIT {
        return Err("dataset catalog page is not a bounded regular file".into());
    }
    serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
        .map_err(|error| format!("invalid official dataset page: {error}"))
}

fn source_tasks(root: &Path) -> Result<Vec<Trial>, String> {
    let first = parse_page(&root.join("rows-000.json"))?;
    let second = parse_page(&root.join("rows-100.json"))?;
    if first.num_rows_total != 140 || second.num_rows_total != first.num_rows_total {
        return Err("official SWE-Perf dataset population changed from its pinned release".into());
    }
    if first.rows.len() != 100 || second.rows.len() != 40 {
        return Err("official SWE-Perf catalog pages are incomplete".into());
    }
    let mut ids = BTreeSet::new();
    let tasks = first
        .rows
        .into_iter()
        .chain(second.rows)
        .map(|entry| entry.row.instance_id)
        .map(|id| Trial {
            description: format!("SWE-Perf official instance {id}"),
            id,
        })
        .collect::<Vec<_>>();
    if tasks.iter().any(|task| !ids.insert(task.id.clone())) {
        return Err("official SWE-Perf dataset has duplicate instance ids".into());
    }
    Ok(tasks)
}

fn write_catalog(directory: &Path) -> Result<(), String> {
    let root = directory.join("swe-perf");
    let tasks = source_tasks(&root)?;
    let bytes = serde_json::to_vec_pretty(&Catalog {
        schema_version: 1,
        marker: MARKER.into(),
        tasks: tasks
            .into_iter()
            .map(|task| Task {
                id: task.id,
                description: task.description,
            })
            .collect(),
    })
    .map_err(|error| error.to_string())?;
    fs::write(root.join("catalog.json"), bytes).map_err(|error| error.to_string())
}

fn read(directory: &Path) -> Result<Vec<Trial>, String> {
    let root = directory.join("swe-perf");
    if fs::read_to_string(root.join("ASB-DATASET"))
        .ok()
        .as_deref()
        .map(str::trim)
        != Some(MARKER)
    {
        return Err(
            "SWE-Perf has no pinned installed dataset; run `asb workload install swe-perf`".into(),
        );
    }
    let catalog: Catalog = serde_json::from_slice(
        &fs::read(root.join("catalog.json")).map_err(|_| "SWE-Perf catalog missing")?,
    )
    .map_err(|_| "SWE-Perf catalog invalid")?;
    let tasks = catalog
        .tasks
        .into_iter()
        .map(|task| Trial {
            id: task.id,
            description: task.description,
        })
        .collect::<Vec<_>>();
    if catalog.schema_version != 1
        || catalog.marker != MARKER
        || tasks.iter().map(|task| &task.id).collect::<Vec<_>>()
            != source_tasks(&root)?
                .iter()
                .map(|task| &task.id)
                .collect::<Vec<_>>()
    {
        return Err("SWE-Perf catalog differs from its installed dataset; reinstall".into());
    }
    Ok(tasks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(total: usize, start: usize, count: usize) -> String {
        let rows = (start..start + count)
            .map(|index| serde_json::json!({ "row": { "instance_id": format!("project__issue-{index}") } }))
            .collect::<Vec<_>>();
        serde_json::json!({ "num_rows_total": total, "rows": rows }).to_string()
    }

    #[test]
    fn local_official_page_fixtures_enumerate_and_load_without_download() {
        let root = std::env::temp_dir().join(format!("asb-swe-perf-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dataset = root.join("swe-perf");
        fs::create_dir_all(&dataset).unwrap();
        fs::write(dataset.join("rows-000.json"), page(140, 0, 100)).unwrap();
        fs::write(dataset.join("rows-100.json"), page(140, 100, 40)).unwrap();
        fs::write(dataset.join("ASB-DATASET"), format!("{MARKER}\n")).unwrap();
        write_catalog(&root).unwrap();
        let tasks = read(&root).unwrap();
        assert_eq!(tasks.len(), 140);
        assert_eq!(tasks[0].id, "project__issue-0");
        assert_eq!(tasks[139].id, "project__issue-139");
        let _ = fs::remove_dir_all(root);
    }
}
