//! Pinned public task catalog and project-local official SDK installation.
use super::SweBenchAdapter;
use crate::Error;
use crate::install;
use crate::workloads::InstallationContext;
use crate::workloads::session::DatasetIdentity;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::{Component, Path};

const MATERIALIZER: &str = include_str!("../../../../assets/benchmarks/asb_swebench.py");
const TASK_SESSION: &str = include_str!("../../../../assets/benchmarks/asb_swebench_task.py");
const TASK_SOURCES: &str = include_str!("../../../../assets/benchmarks/asb_swebench_sources.py");
const MAX_CATALOG_BYTES: u64 = 16 * 1024 * 1024;
// Streaming rows are normalized into project-local metadata while the source
// cache is populated, so this is a bounded package materialization rather
// than a short, single-file transfer.
const CATALOG_MATERIALIZATION_PURPOSE: install::Purpose = install::Purpose::Package;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Catalog {
    schema_version: u32,
    workload: String,
    pub dataset: DatasetIdentity,
    pub tasks: Vec<Task>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Task {
    pub id: String,
    pub description: String,
    repo: String,
    base_commit: String,
    metadata: String,
    metadata_sha256: String,
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) fn read(adapter: &SweBenchAdapter, directory: &Path) -> Result<Catalog, String> {
    let path = directory.join(adapter.id).join("catalog.json");
    let failure = |reason: &str| {
        format!(
            "{} task catalog {} is unavailable or invalid ({reason}); run `asb workload install {}`",
            adapter.id,
            path.display(),
            adapter.id,
        )
    };
    if fs::symlink_metadata(&path)
        .map_err(|_| failure("not installed"))?
        .file_type()
        .is_symlink()
    {
        return Err(failure("catalog must not be a symlink"));
    }
    let mut content = Vec::new();
    fs::File::open(&path)
        .map_err(|_| failure("cannot open"))?
        .take(MAX_CATALOG_BYTES + 1)
        .read_to_end(&mut content)
        .map_err(|_| failure("cannot read"))?;
    if content.len() as u64 > MAX_CATALOG_BYTES {
        return Err(failure("exceeds 16 MiB"));
    }
    let catalog: Catalog =
        serde_json::from_slice(&content).map_err(|_| failure("incorrect JSON schema"))?;
    if catalog.schema_version != 1
        || catalog.workload != adapter.id
        || catalog.dataset.source != adapter.dataset
        || catalog.dataset.revision != adapter.revision
        || catalog.dataset.split != "test"
        || catalog.tasks.len() != adapter.expected_tasks
    {
        return Err(failure(
            "dataset, revision, split or task population does not match",
        ));
    }
    let mut ids = BTreeSet::new();
    let installed_root = path
        .parent()
        .expect("catalog parent")
        .canonicalize()
        .map_err(|_| failure("cannot resolve installed dataset directory"))?;
    for task in &catalog.tasks {
        let expected_metadata = format!("evaluation-data/{}/", adapter.revision);
        let relative = Path::new(&task.metadata);
        if task.id.is_empty()
            || task.id.len() > 256
            || !task
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
            || !ids.insert(&task.id)
            || task.description.len() > 1024
            || task.repo.is_empty()
            || task.repo.len() > 256
            || !hex(&task.base_commit, 40)
            || !hex(&task.metadata_sha256, 64)
            || !task.metadata.starts_with(&expected_metadata)
            || relative.components().count() != 3
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            || !task.metadata.ends_with(".json")
        {
            return Err(failure("invalid or duplicate task identity"));
        }
        let metadata_path = installed_root.join(&task.metadata);
        let metadata = fs::symlink_metadata(&metadata_path)
            .map_err(|_| failure("task evaluator metadata is missing"))?;
        if !metadata.is_file()
            || metadata.len() > 4 * 1024 * 1024
            || !metadata_path
                .canonicalize()
                .is_ok_and(|path| path.starts_with(&installed_root))
        {
            return Err(failure(
                "task evaluator metadata is not a bounded local file",
            ));
        }
    }
    Ok(catalog)
}

pub(super) fn install(
    adapter: &SweBenchAdapter,
    context: &InstallationContext<'_>,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> crate::install::InstallResult<()> {
    let owned_context = context
        .process
        .with_destination(&context.tools_root.join(super::ENGINE.tool()))?;
    let context = &crate::workloads::InstallationContext {
        process: &owned_context,
        project_root: context.project_root,
        tools_root: context.tools_root,
        workloads_root: context.workloads_root,
    };
    if !install::tool_installed_locally(super::ENGINE.tool(), context.tools_root) {
        install::tool_in(
            context.process,
            install::ToolInstallOptions {
                name: super::ENGINE.tool().into(),
                source: None,
                dir: Some(context.tools_root.to_path_buf()),
                dry_run: false,
            },
            output,
            progress,
        )?;
    }
    let tool_root = context.tools_root.join(super::ENGINE.tool());
    super::ENGINE.publish(&tool_root)?;
    let helper = tool_root.join(super::ENGINE.script());
    let python = tool_root.join("venv/bin/python");
    let owner = install::ProducerDirectory::open(context.workloads_root)?;
    let mut scope = owner.scope(context.process);
    let status = scope.status(
        &python,
        install::Purpose::Probe,
        "SWE-bench evaluator SDK identity",
        |command| {
            command
                .arg(&helper)
                .arg("identity")
                .env("PYTHONDONTWRITEBYTECODE", "1");
        },
    )?;
    if !status.success() {
        return Err(Error::Input("installed SWE-bench evaluator SDK does not match its recipe or is unusable; run `asb tool install swebench`, then retry the workload installation; inspect installation diagnostics".into()).into());
    }
    let valid = if read(adapter, context.workloads_root).is_ok() {
        scope
            .status(
                &python,
                install::Purpose::Probe,
                "SWE-bench installed catalog verification",
                |command| {
                    command
                        .arg(&helper)
                        .arg("verify")
                        .arg("--catalog")
                        .arg(context.workloads_root.join(adapter.id).join("catalog.json"))
                        .env("PYTHONDONTWRITEBYTECODE", "1");
                },
            )?
            .success()
    } else {
        false
    };
    if !valid {
        let bundle = context.workloads_root.join(adapter.id);
        if fs::symlink_metadata(&bundle).is_ok() {
            if fs::symlink_metadata(&bundle)
                .map_err(|error| Error::Io(error.to_string()))?
                .file_type()
                .is_symlink()
            {
                return Err(Error::Input("official dataset directory must not be a symlink; preserve it and choose a normal project directory".into()).into());
            }
            let backup = context.workloads_root.join(format!(
                "{}.asb-backup-{}",
                adapter.id,
                crate::workloads::TaskIdentity::local(adapter.id, None).attempt_id
            ));
            fs::rename(&bundle, &backup).map_err(|error| {
                Error::Io(format!("cannot preserve incomplete dataset: {error}"))
            })?;
            writeln!(
                output,
                "Preserved the incomplete or changed dataset at {} before reinstalling.",
                backup.display()
            )
            .map_err(crate::io_error)?;
        }
        progress(25, crate::install::ProgressStage::DownloadingAndValidating);
        let catalog_output = context.workloads_root.join(adapter.id);
        let mut step = scope.prepare(
            &python,
            CATALOG_MATERIALIZATION_PURPOSE,
            "official SWE-bench dataset materialization",
        )?;
        step.command_mut()
            .arg(&helper)
            .arg("catalog")
            .args([
                "--workload",
                adapter.id,
                "--dataset",
                adapter.dataset,
                "--revision",
                adapter.revision,
                "--split",
                "test",
            ])
            .arg("--output")
            .arg(&catalog_output)
            .arg("--cache")
            .arg(context.project_root.join(".asb-cache/huggingface"))
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("HF_HUB_ETAG_TIMEOUT", "30")
            .env("HF_HUB_DOWNLOAD_TIMEOUT", "60")
            .env("LITELLM_LOCAL_MODEL_COST_MAP", "True");
        let cache = context.project_root.join(".asb-cache/huggingface");
        let monitor = crate::install::transfer::Cache::start_many(
            &[&cache, &catalog_output],
            "Downloading and materializing SWE-bench dataset",
        );
        let result = scope.run(step);
        let _ = monitor.finish();
        let status = result?.status;
        if !status.success() {
            return Err(Error::Io(format!(
                "{} official dataset installation failed; inspect .asb/install.log, check network/proxy access and retry `asb workload install {}`",
                adapter.id, adapter.id,
            )).into());
        }
    }
    owner.consume(scope.finish()?)?;
    let catalog = read(adapter, context.workloads_root).map_err(Error::Input)?;
    super::sources::install(adapter, context, output, progress)?;
    // The prompt is just a discoverable catalog label, not agent input. Actual
    // issue text must come from the selected, checksum-verified instance.
    fs::write(
        context.workloads_root.join(format!("{}.txt", adapter.id)),
        format!(
            "Official {} test population. Select an installed instance ID.\n",
            adapter.id
        ),
    )
    .map_err(|error| Error::Io(format!("cannot write workload catalog label: {error}")))?;
    publish_selection(adapter, &catalog, context.workloads_root, output)?;
    progress(100, crate::install::ProgressStage::Ready);
    writeln!(
        output,
        "Installed {}: {} official test instances.",
        adapter.id,
        catalog.tasks.len()
    )
    .map_err(crate::io_error)?;
    Ok(())
}

pub(super) fn ensure_helpers(root: &Path) -> Result<(), Error> {
    crate::workloads::controller::publish_support(root)?;
    for (name, source) in [
        ("asb_swebench.py", MATERIALIZER),
        ("asb_swebench_task.py", TASK_SESSION),
        ("asb_swebench_sources.py", TASK_SOURCES),
    ] {
        publish_helper(&root.join(name), source)?;
    }
    Ok(())
}

fn publish_helper(path: &Path, source: &str) -> Result<(), Error> {
    crate::workloads::controller::publish_asset(path, source)
}

fn publish_selection(
    adapter: &SweBenchAdapter,
    catalog: &Catalog,
    directory: &Path,
    output: &mut dyn std::io::Write,
) -> Result<(), Error> {
    let path = directory.join(format!("{}.workload.toml", adapter.id));
    if path.exists() {
        if fs::symlink_metadata(&path)
            .map_err(|error| Error::Io(error.to_string()))?
            .file_type()
            .is_symlink()
        {
            return Err(Error::Input(
                "workload selection must not be a symlink".into(),
            ));
        }
        let mut previous = Vec::new();
        fs::File::open(&path)
            .map_err(|error| Error::Io(error.to_string()))?
            .take(MAX_CATALOG_BYTES + 1)
            .read_to_end(&mut previous)
            .map_err(|error| Error::Io(error.to_string()))?;
        if previous.len() as u64 > MAX_CATALOG_BYTES {
            return Err(Error::Input("existing workload selection exceeds 16 MiB; preserve it and remove it before reinstalling".into()));
        }
        let ids = catalog
            .tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<BTreeSet<_>>();
        let selected = std::str::from_utf8(&previous)
            .ok()
            .and_then(|text| toml::from_str::<crate::workloads::TrialSet>(text).ok());
        if selected.is_some_and(|selected| {
            selected.workload == adapter.id
                && !selected.trials.is_empty()
                && selected
                    .trials
                    .iter()
                    .map(|trial| &trial.id)
                    .collect::<BTreeSet<_>>()
                    .len()
                    == selected.trials.len()
                && selected
                    .trials
                    .iter()
                    .all(|trial| ids.contains(trial.id.as_str()))
        }) {
            return Ok(()); // Preserve the user's valid subset and descriptions.
        }
        // Old synthetic trial selections cannot identify real instances. Keep
        // their exact bytes under an exclusive backup name before replacement.
        let backup = (1..)
            .find_map(|index| {
                let backup = path.with_extension(format!("toml.legacy-{index}"));
                match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&backup)
                {
                    Ok(file) => Some(Ok((backup, file))),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => Some(Err(Error::Io(error.to_string()))),
                }
            })
            .expect("unbounded backup suffix")?;
        let (backup_path, mut backup_file) = backup;
        backup_file
            .write_all(&previous)
            .and_then(|()| backup_file.sync_all())
            .map_err(|error| {
                Error::Io(format!("cannot preserve legacy task selection: {error}"))
            })?;
        writeln!(
            output,
            "Preserved the previous incompatible task selection at {}.",
            backup_path.display()
        )
        .map_err(crate::io_error)?;
    }
    let selected = crate::workloads::TrialSet {
        workload: adapter.id.into(),
        trials: catalog
            .tasks
            .iter()
            .map(|task| crate::workloads::Trial {
                id: task.id.clone(),
                description: task.description.clone(),
            })
            .collect(),
    };
    let text = toml::to_string_pretty(&selected).map_err(|error| Error::Io(error.to_string()))?;
    let temporary = path.with_extension(format!(
        "toml.{}",
        crate::workloads::TaskIdentity::local(adapter.id, None).attempt_id
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| Error::Io(error.to_string()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temporary, &path))
        .map_err(|error| Error::Io(format!("cannot publish official task selection: {error}")))?;
    Ok(())
}

#[cfg(test)]
#[path = "catalog/coverage_tests.rs"]
mod coverage_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workloads::adapters::WorkloadAdapter;
    use serde_json::json;

    fn fixture(root: &Path) -> SweBenchAdapter {
        let adapter = SweBenchAdapter {
            id: "suite",
            dataset: "official/source",
            revision: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            source_sha256: "fcd30fba8098151a003ce953022e7e6db16f53f82298290a285b22722483a5a1",
            expected_tasks: 2,
        };
        fs::create_dir_all(root.join(adapter.id)).unwrap();
        let metadata = root
            .join(adapter.id)
            .join("evaluation-data")
            .join(adapter.revision)
            .join(format!("{}.json", "c".repeat(64)));
        fs::create_dir_all(metadata.parent().unwrap()).unwrap();
        fs::write(metadata, "{}").unwrap();
        let task = |id| {
            json!({"id": id, "description": "Fix the fixture", "repo": "owner/repo",
            "base_commit": "b".repeat(40), "metadata": format!("evaluation-data/{}/{}.json", adapter.revision, "c".repeat(64)),
            "metadata_sha256": "d".repeat(64)})
        };
        fs::write(root.join("suite/catalog.json"), serde_json::to_vec(&json!({
            "schema_version": 1, "workload": adapter.id,
            "dataset": {"source": adapter.dataset, "revision": adapter.revision, "split": "test"},
            "tasks": [task("owner__repo-1"), task("owner__repo-2")],
        })).unwrap()).unwrap();
        adapter
    }

    #[test]
    fn installed_discovery_binds_population_and_dataset_identity() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("catalog", None).attempt_id);
        let adapter = fixture(&root);
        let trials = adapter.installed_trials(&root).unwrap();
        assert_eq!(trials.len(), 2);
        assert_eq!(trials[0].id, "owner__repo-1");
        assert_eq!(adapter.installed_available(&root), Some(true));
        let planning = adapter.planning_catalog(&root).unwrap().unwrap();
        assert_eq!(planning.dataset.source, adapter.dataset);
        assert_eq!(planning.dataset.revision, adapter.revision);
        assert_eq!(planning.task_ids.len(), 2);
        assert!(planning.task_ids.contains("owner__repo-1"));
        assert_eq!(planning.evaluation_kind.as_str(), "upstream-harness");
        let path = root.join("suite/catalog.json");
        let original = fs::read(&path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        for (field, replacement) in [
            ("schema_version", json!(2)),
            ("workload", json!("other")),
            (
                "dataset",
                json!({"source": adapter.dataset, "revision": "main", "split": "test"}),
            ),
            ("tasks", json!([])),
            (
                "tasks",
                json!([value["tasks"][0].clone(), value["tasks"][0].clone()]),
            ),
        ] {
            let mut changed = value.clone();
            changed[field] = replacement;
            fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
            assert!(adapter.installed_trials(&root).is_err(), "{field}");
            assert_eq!(adapter.installed_available(&root), Some(false));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selection_migration_preserves_legacy_and_valid_user_subsets() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("selection", None).attempt_id);
        let adapter = fixture(&root);
        let catalog = read(&adapter, &root).unwrap();
        let path = root.join("suite.workload.toml");
        let legacy = b"workload='suite'\n[[trials]]\nid='bug-fix'\n";
        fs::write(&path, legacy).unwrap();
        let mut output = Vec::new();
        publish_selection(&adapter, &catalog, &root, &mut output).unwrap();
        assert_eq!(
            fs::read(path.with_extension("toml.legacy-1")).unwrap(),
            legacy
        );
        assert_eq!(
            crate::workloads::read_trial_set(&path)
                .unwrap()
                .trials
                .len(),
            2
        );
        let subset = b"# user choice\nworkload='suite'\n[[trials]]\nid='owner__repo-2'\n";
        fs::write(&path, subset).unwrap();
        publish_selection(&adapter, &catalog, &root, &mut output).unwrap();
        assert_eq!(fs::read(&path).unwrap(), subset);
        assert!(!path.with_extension("toml.legacy-2").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
