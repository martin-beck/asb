//! Installation and validation of pinned LiveCodeBench task catalogs.
use super::LiveCodeBenchAdapter;
use crate::Error;
use crate::install::{self, ProducerDirectory, Purpose};
use crate::workloads::InstallationContext;
use crate::workloads::session::DatasetIdentity;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

const HELPER: &str = include_str!("../../../../assets/benchmarks/asb_livecodebench_catalog.py");
const MAX_CATALOG_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Catalog {
    schema_version: u32,
    workload: String,
    scenario: String,
    pub dataset: DatasetIdentity,
    pub tasks: Vec<Task>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Task {
    pub id: String,
    pub description: String,
    pub prompt: String,
    pub metadata: String,
    pub metadata_sha256: String,
}

fn failure(adapter: &LiveCodeBenchAdapter, directory: &Path, reason: &str) -> String {
    format!(
        "{} task catalog {} is unavailable or invalid ({reason}); run `asb workload install {}`",
        adapter.id,
        directory.join(adapter.id).join("catalog.json").display(),
        adapter.id
    )
}

pub(super) fn read(adapter: &LiveCodeBenchAdapter, directory: &Path) -> Result<Catalog, String> {
    let path = directory.join(adapter.id).join("catalog.json");
    let metadata =
        fs::symlink_metadata(&path).map_err(|_| failure(adapter, directory, "not installed"))?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_CATALOG_BYTES
    {
        return Err(failure(
            adapter,
            directory,
            "catalog is not a bounded regular file",
        ));
    }
    let mut bytes = Vec::new();
    fs::File::open(&path)
        .map_err(|_| failure(adapter, directory, "cannot open"))?
        .take(MAX_CATALOG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failure(adapter, directory, "cannot read"))?;
    if bytes.len() as u64 > MAX_CATALOG_BYTES {
        return Err(failure(adapter, directory, "catalog exceeds 32 MiB"));
    }
    let catalog: Catalog = serde_json::from_slice(&bytes)
        .map_err(|_| failure(adapter, directory, "incorrect JSON schema"))?;
    if catalog.schema_version != 1
        || catalog.workload != adapter.id
        || catalog.scenario != adapter.scenario
        || catalog.dataset.source != adapter.dataset
        || catalog.dataset.revision != adapter.revision
        || catalog.dataset.split != adapter.release.unwrap_or(adapter.scenario)
        || catalog.tasks.is_empty()
        || catalog.tasks.len() > 10_000
    {
        return Err(failure(
            adapter,
            directory,
            "dataset identity or population differs",
        ));
    }
    let mut ids = BTreeSet::new();
    for task in &catalog.tasks {
        if task.id.is_empty()
            || task.id.len() > 256
            || !task
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
            || !ids.insert(&task.id)
            || task.description.is_empty()
            || task.description.len() > 1024
            || task.prompt.is_empty()
            || task.prompt.len() > 64 * 1024
            || !task.metadata.starts_with("evaluation-data/")
            || task.metadata.contains("..")
            || task.metadata_sha256.len() != 64
            || !task
                .metadata_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(failure(
                adapter,
                directory,
                "invalid or duplicate task identity",
            ));
        }
    }
    Ok(catalog)
}

pub(super) fn install(
    adapter: &LiveCodeBenchAdapter,
    context: &InstallationContext<'_>,
    output: &mut dyn Write,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> crate::install::InstallResult<()> {
    if !install::tool_installed_locally("livecodebench", context.tools_root) {
        install::tool_in(
            context.process,
            install::ToolInstallOptions {
                name: "livecodebench".into(),
                source: None,
                dir: Some(context.tools_root.to_path_buf()),
                dry_run: false,
            },
            output,
            progress,
        )?;
    }
    let tool = context.tools_root.join("livecodebench");
    crate::workloads::controller::publish_asset(
        &tool.join("asb_livecodebench_catalog.py"),
        HELPER,
    )?;
    if read(adapter, context.workloads_root).is_err() {
        let owner = ProducerDirectory::open(context.workloads_root)?;
        let mut scope = owner.scope(context.process);
        progress(25, crate::install::ProgressStage::DownloadingAndValidating);
        let python = tool.join("venv/bin/python");
        let mut step = scope.prepare(
            &python,
            Purpose::Download,
            "LiveCodeBench pinned dataset installation",
        )?;
        step.command_mut()
            .arg(tool.join("asb_livecodebench_catalog.py"))
            .arg("catalog")
            .args([
                "--workload",
                adapter.id,
                "--scenario",
                adapter.scenario,
                "--dataset",
                adapter.dataset,
                "--revision",
                adapter.revision,
            ])
            .arg("--output")
            .arg(context.workloads_root.join(adapter.id))
            .arg("--cache")
            .arg(context.project_root.join(".asb-cache/huggingface"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env(
                "HF_HOME",
                context.project_root.join(".asb-cache/huggingface"),
            );
        if let Some(release) = adapter.release {
            step.command_mut().args(["--release", release]);
        }
        if !scope.run(step)?.status.success() {
            return Err(Error::Io(format!(
                "{} official dataset installation failed; inspect .asb/install.log, check network/proxy access and retry `asb workload install {}`",
                adapter.id, adapter.id
            )).into());
        }
        owner.consume(scope.finish()?)?;
    }
    let catalog = read(adapter, context.workloads_root).map_err(Error::Input)?;
    fs::write(
        context.workloads_root.join(format!("{}.txt", adapter.id)),
        format!(
            "Official {} task population. Select an installed instance ID.\n",
            adapter.id
        ),
    )
    .map_err(crate::io_error)?;
    crate::workloads::write_default_trial_set(adapter.id, context.workloads_root)
        .map_err(crate::io_error)?;
    progress(100, crate::install::ProgressStage::Ready);
    writeln!(
        output,
        "Installed {}: {} official instances.",
        adapter.id,
        catalog.tasks.len()
    )
    .map_err(crate::io_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{CODE_EXECUTION, CODE_GENERATION_V6, WorkloadAdapter};
    use super::*;

    #[test]
    fn pinned_catalog_exposes_bounded_official_trials() {
        let root = std::env::temp_dir().join(format!(
            "asb-livecodebench-catalog-{}",
            crate::workloads::TaskIdentity::local("lcb", None).attempt_id
        ));
        let directory = root.join(CODE_EXECUTION.id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("catalog.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "workload": CODE_EXECUTION.id,
                "scenario": CODE_EXECUTION.scenario,
                "dataset": {
                    "source": CODE_EXECUTION.dataset,
                    "revision": CODE_EXECUTION.revision,
                    "split": CODE_EXECUTION.scenario,
                },
                "tasks": [{
                    "id": "sample-1",
                    "description": "Pinned execution instance",
                    "prompt": "Predict the output.",
                    "metadata": "evaluation-data/sample-1.json",
                    "metadata_sha256": "a".repeat(64),
                }],
            }))
            .unwrap(),
        )
        .unwrap();
        let catalog = read(&CODE_EXECUTION, &root).unwrap();
        assert_eq!(catalog.tasks[0].id, "sample-1");
        assert_eq!(catalog.tasks[0].prompt, "Predict the output.");
        let planning = CODE_EXECUTION.planning_catalog(&root).unwrap().unwrap();
        assert_eq!(planning.dataset.source, CODE_EXECUTION.dataset);
        assert!(planning.task_ids.contains("sample-1"));
        assert_eq!(
            planning.requirements["sample-1"].source,
            "livecodebench-official-evaluator"
        );
        assert_eq!(
            planning.evaluation_kind,
            crate::workloads::KnownEvaluationKind::UpstreamHarness
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn catalog_rejects_a_wrong_release_population() {
        let root = std::env::temp_dir().join(format!(
            "asb-livecodebench-release-{}",
            crate::workloads::TaskIdentity::local("lcb", None).attempt_id
        ));
        let directory = root.join(CODE_GENERATION_V6.id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("catalog.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "workload": CODE_GENERATION_V6.id,
                "scenario": CODE_GENERATION_V6.scenario,
                "dataset": {
                    "source": CODE_GENERATION_V6.dataset,
                    "revision": CODE_GENERATION_V6.revision,
                    "split": "release_v5",
                },
                "tasks": [{
                    "id": "release_v5.1",
                    "description": "Wrong release",
                    "prompt": "Prompt",
                    "metadata": "evaluation-data/release_v5.1.json",
                    "metadata_sha256": "a".repeat(64),
                }],
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(read(&CODE_GENERATION_V6, &root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
