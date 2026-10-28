//! Pinned BigCodeBench dataset catalog.

use super::{SetupExtension, WorkloadAdapter};
use crate::workloads::{InstallationContext, Trial};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

const URL: &str = "https://github.com/bigcode-project/bigcodebench-annotation/releases/download/v0.1.4/BigCodeBench.jsonl.gz";
const MARKER: &str = "bigcodebench-annotation/BigCodeBench@v0.1.4";
const LIMIT: u64 = 256 * 1024 * 1024;

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

pub(super) struct BigCodeBenchAdapter;
pub(super) static ADAPTER: BigCodeBenchAdapter = BigCodeBenchAdapter;

impl WorkloadAdapter for BigCodeBenchAdapter {
    fn id(&self) -> &'static str {
        "bigcodebench"
    }
    fn source(&self) -> Option<&'static str> {
        Some(URL)
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
        fs::create_dir_all(&root).map_err(|e| crate::Error::Io(e.to_string()))?;
        progress(25, crate::install::ProgressStage::Downloading);
        crate::install::install_source_bounded_in(
            context.process,
            URL,
            &root.join("dataset.jsonl.gz"),
            "BigCodeBench official dataset",
            LIMIT,
        )?;
        fs::write(root.join("ASB-DATASET"), format!("{MARKER}\n"))
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        write_catalog(context.workloads_root).map_err(crate::Error::Input)?;
        fs::write(
            context.workloads_root.join("bigcodebench.txt"),
            "Use the selected installed BigCodeBench task definition in `external-workload`.\n",
        )
        .map_err(|e| crate::Error::Io(e.to_string()))?;
        crate::workloads::materialize_external_adapter(self.id(), context.workloads_root)
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        crate::workloads::write_default_trial_set(self.id(), context.workloads_root)
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        writeln!(
            output,
            "Installed BigCodeBench official task catalog at {}",
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

fn parse(path: &Path) -> Result<Vec<Trial>, String> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut input = GzDecoder::new(file).take(LIMIT + 1);
    let mut text = String::new();
    input.read_to_string(&mut text).map_err(|e| e.to_string())?;
    if text.len() as u64 > LIMIT {
        return Err("dataset exceeds bounded decompressed size".into());
    }
    let mut tasks = Vec::new();
    let mut ids = BTreeSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        let upstream = value
            .get("task_id")
            .and_then(|v| v.as_str())
            .ok_or("dataset task has no task_id")?;
        let id = format!(
            "task-{}",
            upstream
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        if !ids.insert(id.clone()) {
            return Err("dataset has duplicate task ids".into());
        }
        tasks.push(Trial {
            description: format!("BigCodeBench official task {upstream}"),
            id,
        });
    }
    if tasks.is_empty() || tasks.len() > 100_000 {
        return Err("dataset has an invalid task population".into());
    }
    Ok(tasks)
}
fn write_catalog(directory: &Path) -> Result<(), String> {
    let root = directory.join("bigcodebench");
    let tasks = parse(&root.join("dataset.jsonl.gz"))?;
    let bytes = serde_json::to_vec_pretty(&Catalog {
        schema_version: 1,
        marker: MARKER.into(),
        tasks: tasks
            .into_iter()
            .map(|t| Task {
                id: t.id,
                description: t.description,
            })
            .collect(),
    })
    .map_err(|e| e.to_string())?;
    fs::write(root.join("catalog.json"), bytes).map_err(|e| e.to_string())
}
fn read(directory: &Path) -> Result<Vec<Trial>, String> {
    let root = directory.join("bigcodebench");
    if fs::read_to_string(root.join("ASB-DATASET"))
        .ok()
        .as_deref()
        .map(str::trim)
        != Some(MARKER)
    {
        return Err(
            "BigCodeBench has no pinned installed dataset; run `asb workload install bigcodebench`"
                .into(),
        );
    }
    let catalog: Catalog = serde_json::from_slice(
        &fs::read(root.join("catalog.json")).map_err(|_| "BigCodeBench catalog missing")?,
    )
    .map_err(|_| "BigCodeBench catalog invalid")?;
    let tasks = catalog
        .tasks
        .into_iter()
        .map(|t| Trial {
            id: t.id,
            description: t.description,
        })
        .collect::<Vec<_>>();
    if catalog.schema_version != 1
        || catalog.marker != MARKER
        || tasks.iter().map(|t| &t.id).collect::<Vec<_>>()
            != parse(&root.join("dataset.jsonl.gz"))?
                .iter()
                .map(|t| &t.id)
                .collect::<Vec<_>>()
    {
        return Err("BigCodeBench catalog differs from its installed dataset; reinstall".into());
    }
    Ok(tasks)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gzip_fixture_enumerates_without_download() {
        let root = std::env::temp_dir().join(format!("asb-bigcode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let file = fs::File::create(root.join("x.gz")).unwrap();
        let mut out = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        out.write_all(b"{\"task_id\":\"BigCodeBench/1\"}\n")
            .unwrap();
        out.finish().unwrap();
        assert_eq!(
            parse(&root.join("x.gz")).unwrap()[0].id,
            "task-426967436f646542656e63682f31"
        );
        let _ = fs::remove_dir_all(root);
    }
}
