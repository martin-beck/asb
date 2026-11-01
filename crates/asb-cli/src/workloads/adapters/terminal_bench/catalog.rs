//! Immutable source catalogs for the two original Terminal-Bench versions.
use super::TerminalBenchAdapter;
use crate::Error;
use crate::install;
use crate::workloads::session::DatasetIdentity;
use crate::workloads::{InstallationContext, TaskIdentity, Trial, TrialSet};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

#[cfg(test)]
#[path = "catalog/download_tests.rs"]
mod download_tests;

const HELPER: &str = include_str!("../../../../assets/benchmarks/asb_terminal_catalog.py");
const ORIGINAL_TASKS: &str =
    include_str!("../../../../assets/benchmarks/terminal_bench_1_tasks.json");
const LIMIT: u64 = 16 * 1024 * 1024;

#[cfg(test)]
mod tests;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Catalog {
    schema_version: u32,
    workload: String,
    pub dataset: DatasetIdentity,
    sdk: Sdk,
    source_sha256: String,
    archive_sha256: String,
    pub tasks: Vec<Task>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Sdk {
    package: String,
    version: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Task {
    pub id: String,
    pub description: String,
    instruction: String,
    definition: String,
}

pub(super) fn requirements(
    adapter: &TerminalBenchAdapter,
    directory: &Path,
    catalog: &Catalog,
) -> Result<std::collections::BTreeMap<String, crate::planning::TaskRequirements>, String> {
    let mut requirements = std::collections::BTreeMap::new();
    for task in &catalog.tasks {
        // The pinned original SDK owns YAML semantics. Do not guess those or
        // Compose's multi-service limits from scalar text; effective controller
        // budgets are recorded after preparation for that harness.
        let path = directory
            .join(adapter.id)
            .join(&task.definition)
            .join("task.toml");
        if !path.exists() {
            continue;
        }
        let mut text = String::new();
        fs::File::open(&path)
            .map_err(|error| error.to_string())?
            .take(64 * 1024 + 1)
            .read_to_string(&mut text)
            .map_err(|error| error.to_string())?;
        if text.len() > 64 * 1024 {
            return Err("task requirements exceed 64 KiB".into());
        }
        let value: toml::Value =
            toml::from_str(&text).map_err(|error| format!("invalid task requirements: {error}"))?;
        requirements.insert(task.id.clone(), requirements_from_toml(&value)?);
    }
    Ok(requirements)
}

fn requirements_from_toml(
    value: &toml::Value,
) -> Result<crate::planning::TaskRequirements, String> {
    let number = |section: &str, field: &str| -> Result<Option<f64>, String> {
        value
            .get(section)
            .and_then(|v| v.get(field))
            .map(|v| {
                let n = v
                    .as_float()
                    .or_else(|| v.as_integer().map(|n| n as f64))
                    .ok_or("invalid numeric task requirement")?;
                if !n.is_finite() || n <= 0.0 || n > 1_000_000_000.0 {
                    return Err("task requirement is outside supported bounds");
                }
                Ok(n)
            })
            .transpose()
            .map_err(str::to_owned)
    };
    Ok(crate::planning::TaskRequirements {
        source: "pinned-task-definition".into(),
        cpu_millis: number("environment", "cpus")?.map(|n| (n * 1000.0).ceil() as u64),
        memory_mib: number("environment", "memory_mb")?.map(|n| n.ceil() as u64),
        disk_mib: number("environment", "storage_mb")?.map(|n| n.ceil() as u64),
        execution_seconds: number("agent", "timeout_sec")?,
        evaluation_seconds: number("verifier", "timeout_sec")?,
    })
}

pub(super) fn read(adapter: &TerminalBenchAdapter, directory: &Path) -> Result<Catalog, String> {
    let root = directory.join(adapter.id);
    let path = root.join("catalog.json");
    let failure = || {
        format!(
            "{} has no valid official task catalog; run `asb workload install {}`",
            adapter.id, adapter.id
        )
    };
    if fs::symlink_metadata(&path)
        .map_err(|_| failure())?
        .file_type()
        .is_symlink()
    {
        return Err(failure());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|_| failure())?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failure())?;
    if bytes.len() as u64 > LIMIT {
        return Err(failure());
    }
    let catalog: Catalog = serde_json::from_slice(&bytes).map_err(|_| failure())?;
    let hash =
        |value: &str| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit());
    if catalog.schema_version != 1
        || catalog.workload != adapter.id
        || catalog.dataset.source != adapter.source
        || catalog.dataset.revision != adapter.revision
        || catalog.dataset.split != adapter.split
        || catalog.sdk.package != adapter.engine.tool()
        || catalog.sdk.version != adapter.version
        || catalog.tasks.len() != adapter.expected_tasks
        || !hash(&catalog.source_sha256)
        || !hash(&catalog.archive_sha256)
    {
        return Err(failure());
    }
    let canonical = root.canonicalize().map_err(|_| failure())?;
    let manifest =
        fs::symlink_metadata(root.join("source-manifest.json")).map_err(|_| failure())?;
    if !manifest.is_file() || manifest.len() > LIMIT {
        return Err(failure());
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
            || task.description.len() > 1024
            || task.instruction.trim().is_empty()
            || task.instruction.len() > 64 * 1024
            || task.definition != task_definition(adapter, &task.id)
            || !root.join(&task.definition).is_dir()
            || !root
                .join(&task.definition)
                .canonicalize()
                .is_ok_and(|path| path.starts_with(&canonical))
        {
            return Err(failure());
        }
    }
    Ok(catalog)
}

pub(super) fn task_definition(adapter: &TerminalBenchAdapter, id: &str) -> String {
    if adapter.tasks_root.is_empty() {
        format!("source/{id}")
    } else {
        format!("source/{}/{id}", adapter.tasks_root)
    }
}

fn publish(path: &Path, content: &str) -> Result<(), Error> {
    crate::workloads::controller::publish_asset(path, content)
}

pub(super) fn ensure_helpers(root: &Path) -> Result<(), Error> {
    crate::workloads::controller::publish_support(root)?;
    publish(&root.join("asb_terminal_catalog.py"), HELPER)?;
    publish(&root.join("terminal_bench_1_tasks.json"), ORIGINAL_TASKS)?;
    publish(
        &root.join("asb_terminal_bench_1.py"),
        include_str!("../../../../assets/benchmarks/asb_terminal_bench_1.py"),
    )?;
    publish(
        &root.join("asb_harbor.py"),
        include_str!("../../../../assets/benchmarks/asb_harbor.py"),
    )
}

pub(super) fn runtime_available(
    adapter: &TerminalBenchAdapter,
    root: &Path,
) -> Result<bool, Error> {
    let context =
        install::OperationContext::load(root).map_err(install::InstallFailure::into_public)?;
    let owner =
        install::ProducerDirectory::open(root).map_err(install::InstallFailure::into_public)?;
    let mut scope = owner.scope(&context);
    let available = runtime_available_in(&mut scope, &context, adapter, root);
    owner
        .consume(
            scope
                .finish()
                .map_err(install::InstallFailure::into_public)?,
        )
        .map_err(install::InstallFailure::into_public)?;
    available.map_err(install::InstallFailure::into_public)
}
fn runtime_available_in(
    scope: &mut install::ProcessScope,
    context: &install::OperationContext,
    adapter: &TerminalBenchAdapter,
    root: &Path,
) -> install::InstallResult<bool> {
    let python = root.join("venv/bin/python");
    #[cfg(target_os = "linux")]
    let python = {
        let python = std::path::absolute(python).map_err(crate::io_error)?;
        context.check_before_admission("Terminal-Bench evaluator runtime", &python)?;
        let selected = crate::tools::host_executable(&python);
        context.check_before_admission("Terminal-Bench evaluator runtime", &python)?;
        selected.ok_or_else(|| {
            Error::Io(format!(
                "cannot inspect installed evaluator runtime {}: it is not currently executable; run `asb tool install {}`",
                python.display(), adapter.engine.tool()
            ))
        })?
    };
    #[cfg(not(target_os = "linux"))]
    let _ = context; // Preserve native direct/PATHEXT execution on other hosts.
    Ok(scope
        .status(
            python,
            install::Purpose::Probe,
            "Terminal-Bench evaluator runtime",
            |command| {
                command
                    .arg(root.join("asb_terminal_catalog.py"))
                    .args(["runtime", "--workload", adapter.id, "--directory"])
                    .arg(root)
                    .env("PYTHONDONTWRITEBYTECODE", "1");
            },
        )?
        .success())
}

pub(super) fn install(
    adapter: &TerminalBenchAdapter,
    context: &InstallationContext<'_>,
    output: &mut dyn Write,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> install::InstallResult<()> {
    let owned_context = context
        .process
        .with_destination(&context.tools_root.join(adapter.engine.tool()))?;
    let context = &crate::workloads::InstallationContext {
        process: &owned_context,
        project_root: context.project_root,
        tools_root: context.tools_root,
        workloads_root: context.workloads_root,
    };
    if !install::tool_installed_locally(adapter.engine.tool(), context.tools_root) {
        install::tool_in(
            context.process,
            install::ToolInstallOptions {
                name: adapter.engine.tool().into(),
                source: None,
                dir: Some(context.tools_root.to_path_buf()),
                dry_run: false,
            },
            output,
            progress,
        )?;
    }
    let tool = context.tools_root.join(adapter.engine.tool());
    let helper = tool.join("asb_terminal_catalog.py");
    ensure_helpers(&tool)?;
    let owner = install::ProducerDirectory::open(context.workloads_root)?;
    let mut scope = owner.scope(context.process);
    if !runtime_available_in(&mut scope, context.process, adapter, &tool)? {
        progress(10, crate::install::ProgressStage::Dependencies);
        install::tool_in(
            context.process,
            install::ToolInstallOptions {
                name: adapter.engine.tool().into(),
                source: None,
                dir: Some(context.tools_root.to_path_buf()),
                dry_run: false,
            },
            output,
            progress,
        )?;
        if !runtime_available_in(&mut scope, context.process, adapter, &tool)? {
            return Err(Error::Input(format!(
                "installed evaluator runtime is unusable; run `asb tool install {}` and inspect .asb/install.log",
                adapter.engine.tool()
            )).into());
        }
    }
    let directory = context.workloads_root.join(adapter.id);
    let mut diagnostic = None;
    let mut run = |scope: &mut install::ProcessScope,
                   operation: &str,
                   archive: Option<&Path>|
     -> install::InstallResult<bool> {
        let mut step = scope.prepare(
            tool.join("venv/bin/python"),
            install::Purpose::Configure,
            "official Terminal-Bench catalog operation",
        )?;
        step.command_mut()
            .arg(&helper)
            .arg(operation)
            .arg("--workload")
            .arg(adapter.id)
            .arg("--directory")
            .arg(&directory)
            .env("PYTHONDONTWRITEBYTECODE", "1");
        if let Some(archive) = archive {
            step.command_mut().arg("--archive").arg(archive);
        }
        let completed = scope.run(step)?;
        diagnostic = completed.log.path;
        Ok(completed.status.success())
    };
    if read(adapter, context.workloads_root).is_err() || !run(&mut scope, "verify", None)? {
        let cache = context.project_root.join(".asb-cache/workload-sources");
        fs::create_dir_all(&cache).map_err(|error| Error::Io(error.to_string()))?;
        let archive = cache.join(format!("{}-{}.tar.gz", adapter.id, adapter.revision));
        if archive.exists() && !run(&mut scope, "archive", Some(&archive))? {
            let backup = archive.with_extension(format!(
                "damaged-{}",
                TaskIdentity::local(adapter.id, None).attempt_id
            ));
            fs::rename(&archive, &backup).map_err(|error| Error::Io(error.to_string()))?;
            writeln!(
                output,
                "Preserved damaged source archive at {} before downloading it again.",
                backup.display()
            )
            .map_err(crate::io_error)?;
        }
        if !archive.is_file() {
            progress(20, crate::install::ProgressStage::Downloading);
            let temporary = archive.with_extension(format!(
                "part-{}",
                TaskIdentity::local(adapter.id, None).attempt_id
            ));
            install::install_source_bounded_in(
                context.process,
                adapter.archive,
                &temporary,
                "official Terminal-Bench task source",
                256 * 1024 * 1024,
            )?;
            if !run(&mut scope, "archive", Some(&temporary))? {
                return Err(Error::Io("downloaded task archive is incomplete or unsafe; retained evidence in the project cache; retry the installation".into()).into());
            }
            fs::rename(&temporary, &archive).map_err(|error| Error::Io(error.to_string()))?;
        }
        progress(50, crate::install::ProgressStage::Validating);
        if !run(&mut scope, "install", Some(&archive))? {
            let log = diagnostic.as_ref().map_or_else(
                || "the installation diagnostics".to_owned(),
                |path| path.display().to_string(),
            );
            return Err(Error::Input(format!(
                "{} dataset validation failed; inspect {log}; reinstall `{}` with `asb tool install {}` if its SDK is incompatible",
                adapter.id,
                adapter.engine.tool(),
                adapter.engine.tool()
            )).into());
        }
    }
    owner.consume(scope.finish()?)?;
    let catalog = read(adapter, context.workloads_root).map_err(Error::Input)?;
    fs::write(
        context.workloads_root.join(format!("{}.txt", adapter.id)),
        format!(
            "Official {} task catalog. Select an installed instance.\n",
            adapter.id
        ),
    )
    .map_err(|error| Error::Io(error.to_string()))?;
    selection(adapter.id, &catalog.tasks, context.workloads_root, output)?;
    progress(100, crate::install::ProgressStage::Ready);
    writeln!(
        output,
        "Installed {}: {} official tasks.",
        adapter.id,
        catalog.tasks.len()
    )
    .map_err(crate::io_error)?;
    Ok(())
}

fn selection(
    id: &str,
    tasks: &[Task],
    directory: &Path,
    output: &mut dyn Write,
) -> Result<(), Error> {
    let path = directory.join(format!("{id}.workload.toml"));
    if path.exists() {
        let metadata = fs::symlink_metadata(&path).map_err(|error| Error::Io(error.to_string()))?;
        if !metadata.is_file() || metadata.len() > LIMIT {
            return Err(Error::Input(
                "existing task selection must be a bounded regular file".into(),
            ));
        }
        let previous = fs::read_to_string(&path).map_err(|error| Error::Io(error.to_string()))?;
        let available = tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<BTreeSet<_>>();
        if toml::from_str::<TrialSet>(&previous).is_ok_and(|selected| {
            selected.workload == id
                && !selected.trials.is_empty()
                && selected
                    .trials
                    .iter()
                    .all(|trial| available.contains(trial.id.as_str()))
                && selected
                    .trials
                    .iter()
                    .map(|trial| &trial.id)
                    .collect::<BTreeSet<_>>()
                    .len()
                    == selected.trials.len()
        }) {
            return Ok(());
        }
        let backup = path.with_extension(format!(
            "toml.backup-{}",
            TaskIdentity::local(id, None).attempt_id
        ));
        fs::rename(&path, &backup).map_err(|error| Error::Io(error.to_string()))?;
        writeln!(
            output,
            "Preserved the previous incompatible task selection at {}.",
            backup.display()
        )
        .map_err(crate::io_error)?;
    }
    let selected = TrialSet {
        workload: id.into(),
        trials: tasks
            .iter()
            .map(|task| Trial {
                id: task.id.clone(),
                description: task.description.clone(),
            })
            .collect(),
    };
    publish(
        &path,
        &toml::to_string_pretty(&selected).map_err(|error| Error::Io(error.to_string()))?,
    )
}

#[cfg(test)]
mod planning_tests {
    use super::*;
    #[test]
    fn task_metadata_preserves_unknowns_fractional_timeouts_and_declared_resources() {
        let value = toml::from_str("[environment]\ncpus=1.5\nmemory_mb=2048\n[agent]\ntimeout_sec=0.5\n[verifier]\ntimeout_sec=300").unwrap();
        let plan = requirements_from_toml(&value).unwrap();
        assert_eq!(plan.cpu_millis, Some(1500));
        assert_eq!(plan.memory_mib, Some(2048));
        assert_eq!(plan.disk_mib, None);
        assert_eq!(plan.execution_seconds, Some(0.5));
        assert_eq!(plan.evaluation_seconds, Some(300.0));
        let invalid = toml::from_str("[agent]\ntimeout_sec=-1").unwrap();
        assert!(requirements_from_toml(&invalid).is_err());
    }
}
