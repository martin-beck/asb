//! Project-local installers for agents and workload prompt files.

use crate::Error;
use crate::agents;
use crate::benchmark::BenchmarkConfig;
use crate::project;
use crate::suggestions;
use crate::tools;
use crate::workloads;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::process::Stdio;

pub(crate) mod operation;
pub(crate) mod process;
mod progress;
pub(crate) use progress::ProgressStage;
pub(crate) mod archive;
#[cfg(all(test, target_os = "linux"))]
#[path = "../tests/unit/installer_process_boundary.rs"]
mod process_boundary_red;
mod source;
pub(crate) mod transfer;
pub(crate) use process::{
    InstallFailure, InstallResult, OperationContext, ProcessScope, ProducerDirectory, Purpose,
};
pub(crate) use source::check_version_in;

/// Serialize writers to the same persistent installation destination.
pub(crate) fn destination_key(destination: &Path) -> Result<PathBuf, Error> {
    let parent = destination
        .parent()
        .ok_or_else(|| Error::Io("installation destination has no parent".into()))?;
    let name = destination
        .file_name()
        .ok_or_else(|| Error::Io("installation destination has no name".into()))?;
    fs::create_dir_all(parent).map_err(crate::io_error)?;
    let parent = parent.canonicalize().map_err(crate::io_error)?;
    let mut lock_name = std::ffi::OsString::from(".asb-install-");
    lock_name.push(name);
    lock_name.push(".lock");
    Ok(parent.join(lock_name))
}

pub(crate) fn destination_keys(destination: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut keys = vec![destination_key(destination)?];
    match fs::canonicalize(destination) {
        Ok(resolved) => keys.push(destination_key(&resolved)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(crate::io_error(error)),
    }
    keys.sort();
    keys.dedup();
    Ok(keys)
}

pub(crate) fn destination_key_lock(key: &Path) -> Result<crate::output_lock::OutputLock, Error> {
    crate::output_lock::OutputLock::acquire_retained(
        key,
        "another installation owns this destination; wait for it to finish and retry",
    )
    .map_err(Error::Io)
}

#[cfg(test)]
pub(crate) fn destination_lock(
    destination: &Path,
) -> Result<Vec<crate::output_lock::OutputLock>, Error> {
    destination_keys(destination)?
        .iter()
        .map(|key| destination_key_lock(key))
        .collect()
}

#[cfg(test)]
mod destination_lock_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn inherited_descriptor_retains_lock_when_parent_owner_drops() {
        let root = std::env::temp_dir().join(tools::runtime::unique_id());
        fs::create_dir_all(&root).unwrap();
        let owner = destination_lock(&root.join("tool")).unwrap();
        let inherited = owner[0].cloned_descriptor();
        drop(owner);
        assert!(destination_lock(&root.join("tool")).is_err());
        drop(inherited);
        drop(destination_lock(&root.join("tool")).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn leaf_replaced_by_symlink_cannot_change_ownership_key() {
        let root = std::env::temp_dir().join(tools::runtime::unique_id());
        fs::create_dir_all(root.join("target")).unwrap();
        let owner = destination_lock(&root.join("tool")).unwrap();
        std::os::unix::fs::symlink(root.join("target"), root.join("tool")).unwrap();
        assert!(destination_lock(&root.join("tool")).is_err());
        drop(owner);
        let alias_owner = destination_lock(&root.join("tool")).unwrap();
        assert!(destination_lock(&root.join("target")).is_err());
        drop(alias_owner);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn worker_context_retains_destination_after_caller_drops() {
        let root = std::env::temp_dir().join(tools::runtime::unique_id());
        fs::create_dir_all(&root).unwrap();
        let base = OperationContext::new(&root, &project::ProjectConfig::default()).unwrap();
        let owner = base.with_destination(&root.join("tool")).unwrap();
        let nested = owner.with_destination(&root.join("tool")).unwrap();
        let worker = owner.clone();
        drop(owner);
        assert!(destination_lock(&root.join("tool")).is_err());
        drop(nested);
        drop(worker);
        drop(destination_lock(&root.join("tool")).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_tool_install_cannot_modify_a_destination_owned_by_another_operation() {
        let root = std::env::temp_dir().join(tools::runtime::unique_id());
        fs::create_dir_all(root.join("tools/fixture/bin")).unwrap();
        let executable = root.join("tools/fixture/bin/fixture");
        fs::write(&executable, "previous").unwrap();
        let source = root.join("source");
        fs::write(&source, "replacement").unwrap();
        let owner = destination_lock(&root.join("tools/fixture")).unwrap();
        let context = OperationContext::new(&root, &project::ProjectConfig::default()).unwrap();
        let result = tool_in(
            &context,
            ToolInstallOptions {
                name: "fixture".into(),
                source: Some(source.to_string_lossy().into()),
                dir: Some(root.join("tools")),
                dry_run: false,
            },
            &mut Vec::new(),
            &mut |_, _| {},
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("another installation owns")
        );
        assert_eq!(fs::read_to_string(executable).unwrap(), "previous");
        drop(owner);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_bundle_extraction_cannot_be_removed_by_competing_install() {
        let root = std::env::temp_dir().join(tools::runtime::unique_id());
        fs::create_dir_all(&root).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker_root = root.clone();
        let worker = std::thread::spawn(move || {
            let bundle = workloads::SourceBundle {
                archive: "/missing-asb-fixture",
                marker: "fixture",
                prompt: "fixture",
            };
            install_bundle(
                "fixture",
                bundle,
                &worker_root,
                &mut Vec::new(),
                &mut |_, stage| {
                    if matches!(stage, ProgressStage::Downloading) {
                        fs::write(worker_root.join(".fixture.extract/sentinel"), "preserve")
                            .unwrap();
                        ready_tx.send(()).unwrap();
                        resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    }
                },
            )
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let bundle = workloads::SourceBundle {
            archive: "/missing-asb-fixture",
            marker: "fixture",
            prompt: "fixture",
        };
        let competing = install_bundle("fixture", bundle, &root, &mut Vec::new(), &mut |_, _| {});
        assert!(
            competing
                .unwrap_err()
                .to_string()
                .contains("another installation owns")
        );
        assert_eq!(
            fs::read_to_string(root.join(".fixture.extract/sentinel")).unwrap(),
            "preserve"
        );
        drop(destination_lock(&root.join("unrelated")).unwrap());
        resume_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        drop(destination_lock(&root.join("fixture")).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}

type CustomInstall = fn(
    &OperationContext,
    &Path,
    &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> InstallResult<PathBuf>;
type CustomGuestAssets = fn(
    &OperationContext,
    &Path,
    &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> InstallResult<Vec<tools::GuestAsset>>;

#[derive(Clone, Copy)]
pub(crate) enum Recipe {
    Npm {
        package: &'static str,
        executable: &'static str,
        /// Optional package-manager manifest supplied by the owning adapter.
        manifest: Option<&'static str>,
    },
    Python {
        package: &'static str,
        dependencies: &'static [&'static str],
        executable: &'static str,
    },
    Host {
        executable: &'static str,
    },
    Custom {
        executable: &'static str,
        description: &'static str,
        summary: &'static str,
        install: CustomInstall,
        guest_assets: Option<CustomGuestAssets>,
    },
}

impl Recipe {
    pub(crate) fn runtime_inputs(self, root: &Path) -> Vec<crate::runtime_inputs::Dependency> {
        match self {
            Self::Python { .. } => vec![crate::runtime_inputs::Dependency::Python {
                path: root.join("venv/bin/python"),
                site: true,
            }],
            _ => Vec::new(),
        }
    }
    fn guest_runtimes(self) -> &'static [&'static str] {
        match self {
            Self::Npm { .. } => &["node"],
            Self::Python { .. } => &["uv"],
            Self::Host { .. } | Self::Custom { .. } => &[],
        }
    }

    fn guest_assets(
        self,
        context: &OperationContext,
        root: &Path,
        progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
    ) -> InstallResult<Option<Vec<tools::GuestAsset>>> {
        match self {
            Self::Custom {
                guest_assets: Some(assets),
                ..
            } => assets(context, root, progress).map(Some),
            _ => Ok(None),
        }
    }
}

#[derive(serde::Serialize)]
pub(crate) struct GuestBootstrap {
    runtime_tools: Vec<&'static str>,
    assets: Vec<tools::GuestAsset>,
    public_package_cache: bool,
    npm_package_tree: Option<NpmPackageTree>,
}

/// A bounded, project-owned npm installation that is staged into an isolated
/// workload guest.  The guest validates and extracts this archive itself; it
/// never receives a host directory mount.
#[derive(serde::Serialize)]
pub(crate) struct NpmPackageTree {
    source: PathBuf,
    target: String,
    executable: &'static str,
}

#[cfg(test)]
impl GuestBootstrap {
    /// A protocol fixture is not qualification of a client/runtime installation.
    pub(crate) fn fixture_empty() -> Self {
        Self {
            runtime_tools: Vec::new(),
            assets: Vec::new(),
            public_package_cache: false,
            npm_package_tree: None,
        }
    }
}

/// Prefetch primary-source artifacts on the host, not via a task image's curl.
/// The owning recipes still perform extraction/package installation in guest.
pub(crate) fn guest_bootstrap(
    id: &str,
    project_root: &Path,
    tools_root: &Path,
) -> Result<GuestBootstrap, Error> {
    let context = OperationContext::load(project_root).map_err(InstallFailure::into_public)?;
    guest_bootstrap_in(&context, id, tools_root).map_err(InstallFailure::into_public)
}
fn guest_bootstrap_in(
    context: &OperationContext,
    id: &str,
    tools_root: &Path,
) -> InstallResult<GuestBootstrap> {
    let adapter = agents::find(id)
        .ok_or_else(|| Error::Input("selected agent has no installation adapter".into()))?;
    let recipe = adapter.recipe();
    fs::create_dir_all(tools_root).map_err(|error| Error::Io(error.to_string()))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(tools_root.join(".asb-guest-bootstrap.lock"))
        .map_err(|error| Error::Io(format!("cannot reserve guest bootstrap: {error}")))?;
    lock.lock()
        .map_err(|error| Error::Io(format!("cannot lock project bootstrap cache: {error}")))?;
    let mut progress = |_percent: u8, _stage: crate::install::ProgressStage| {};
    let mut plan = GuestBootstrap {
        runtime_tools: recipe.guest_runtimes().to_vec(),
        assets: Vec::new(),
        public_package_cache: matches!(recipe, Recipe::Npm { .. }),
        npm_package_tree: None,
    };
    for id in &plan.runtime_tools {
        let tool = tools::find(id)
            .ok_or_else(|| Error::Input("guest runtime is not registered".into()))?;
        let root = tools_root.join(id);
        // Prefetch archives without replacing any host runtime executables.
        for mut asset in tool.guest_assets(&root)? {
            if let Some((url, checksum)) = asset.distribution {
                tools::runtime::cached_archive_in(
                    context,
                    &asset.source,
                    url,
                    checksum,
                    &mut progress,
                )?;
            }
            asset.target = Path::new(id).join(asset.target);
            plan.assets.push(asset);
        }
    }
    if let Some(assets) = recipe.guest_assets(context, &tools_root.join(id), &mut progress)? {
        plan.assets.extend(assets.into_iter().map(|mut asset| {
            asset.target = Path::new(id).join(asset.target);
            asset
        }));
    }
    if let Recipe::Npm {
        executable: program,
        ..
    } = recipe
    {
        let root = tools_root.join(id);
        let launcher = executable(recipe, &root);
        if !crate::executables::is_executable(&launcher) {
            return Err(Error::Input(format!(
                "selected npm agent `{id}` is not installed or its executable is unavailable at {}; run `asb tool install {id}` before starting this workload",
                launcher.display()
            ))
            .into());
        }
        let archive = npm_guest_package_tree_in(context, &root)?;
        plan.npm_package_tree = Some(NpmPackageTree {
            source: archive,
            target: id.into(),
            executable: program,
        });
    }
    Ok(plan)
}

fn npm_guest_package_tree_in(context: &OperationContext, root: &Path) -> InstallResult<PathBuf> {
    let node_modules = root.join("node_modules");
    if !node_modules.is_dir() {
        return Err(Error::Input(format!(
            "npm agent installation at {} has no node_modules tree; rerun its project-local installation",
            root.display()
        ))
        .into());
    }
    let cache = root.join("cache");
    fs::create_dir_all(&cache)
        .map_err(|error| Error::Io(format!("cannot create npm guest cache: {error}")))?;
    let archive = cache.join("guest-node-modules.tar");
    let temporary = cache.join(format!(
        ".guest-node-modules-{}.tar",
        tools::runtime::unique_id()
    ));
    let owner = ProducerDirectory::open(root)?;
    let mut scope = owner.scope(context);
    let mut step = scope.prepare("tar", Purpose::Package, "npm guest package archive")?;
    step.command_mut()
        .args(["-C"])
        .arg(root)
        .args(["-cf"])
        .arg(&temporary)
        .arg("node_modules")
        .stdin(Stdio::null());
    let result = scope.run(step)?;
    owner.consume(scope.finish()?)?;
    if !result.status.success() {
        return Err(Error::Io(format!(
            "could not prepare the isolated npm agent payload ({})",
            result.status
        ))
        .into());
    }
    let metadata = fs::metadata(&temporary).map_err(|error| {
        Error::Io(format!(
            "cannot inspect prepared npm agent payload: {error}"
        ))
    })?;
    if !metadata.is_file() || metadata.len() > 512 * 1024 * 1024 {
        let _ = fs::remove_file(&temporary);
        return Err(Error::Input(
            "npm agent payload exceeds the 512 MiB guest bootstrap limit; reduce the selected package tree before retrying".into(),
        )
        .into());
    }
    fs::rename(&temporary, &archive).map_err(|error| {
        Error::Io(format!(
            "cannot publish prepared npm agent payload: {error}"
        ))
    })?;
    Ok(archive)
}

#[derive(Clone, Copy)]
struct ExternalWorkload {
    name: &'static str,
    source: &'static str,
}

fn external_workload(name: &str) -> Option<ExternalWorkload> {
    workloads::registered_sources()
        .find(|(id, _)| *id == name)
        .map(|(name, source)| ExternalWorkload { name, source })
}

/// Return the pinned external workloads that the TUIs can offer for install.
pub fn external_workload_names() -> impl Iterator<Item = &'static str> {
    workloads::registered_sources().map(|(id, _)| id)
}

/// Check whether an external workload has its runnable local bundle and
/// prompt. This is side-effect free and safe to call while rendering a TUI.
pub fn workload_available(name: &str, directory: &Path) -> bool {
    if let Some(available) = workloads::installed_available(name, directory) {
        return available;
    }
    if workloads::source_bundle(name).is_some() {
        directory.join(name).join("ASB-BUNDLE").is_file()
            && directory.join(name).join("source").is_dir()
            && directory
                .join(name)
                .join("asb-trial/ASB-ADAPTER-FIXTURE")
                .is_file()
            && directory.join(format!("{name}.txt")).is_file()
    } else {
        directory.join(format!("{name}.txt")).is_file()
            && (external_workload(name).is_none()
                || directory
                    .join(name)
                    .join("asb-trial/ASB-ADAPTER-FIXTURE")
                    .is_file())
    }
}

pub struct ToolInstallOptions {
    pub name: String,
    pub source: Option<String>,
    pub dir: Option<PathBuf>,
    pub dry_run: bool,
}

pub struct WorkloadInstallOptions {
    pub source: String,
    pub name: Option<String>,
    pub dir: Option<PathBuf>,
}

pub struct WorkloadListOptions {
    pub dir: Option<PathBuf>,
}

pub fn tool(options: ToolInstallOptions, output: &mut dyn std::io::Write) -> Result<(), Error> {
    let mut progress = |_percent: u8, _stage: crate::install::ProgressStage| {};
    tool_with_progress(options, output, &mut progress)
}

/// Install a project-local tool and report concrete installer stages.
pub fn tool_with_progress(
    options: ToolInstallOptions,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> Result<(), Error> {
    let context = OperationContext::load(Path::new(".")).map_err(InstallFailure::into_public)?;
    let result = tool_in(&context, options, output, progress);
    let diagnostic = context.report_diagnostics(output);
    result.map_err(InstallFailure::into_public)?;
    diagnostic
}

pub(crate) fn tool_in(
    context: &OperationContext,
    options: ToolInstallOptions,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, ProgressStage),
) -> InstallResult<()> {
    let ToolInstallOptions {
        name: id,
        source,
        dir,
        dry_run,
    } = options;
    crate::selection_names::validate(&id, "tool").map_err(Error::Input)?;
    if !valid_name(&id) {
        return Err(Error::Input(format!("invalid tool name `{id}`")).into());
    }
    let project_root = context.root();
    let project_config = context.project();
    let tools_root = dir
        .map(|value| resolve_path(project_root, &value))
        .unwrap_or_else(|| project::resolve(project_root, &project_config.tools_dir));
    progress(2, crate::install::ProgressStage::Preparing);
    let root = tools_root.join(&id);
    let built_in = built_in_tool(&id);
    ensure_collector_dependencies_in(
        context,
        std::slice::from_ref(&id),
        project_root,
        &tools_root,
        output,
        progress,
        dry_run,
    )?;
    let owned_context = if dry_run {
        context.clone()
    } else {
        context.with_destination(&root)?
    };
    let context = &owned_context;
    if let Some(source) = source {
        let destination = root
            .join("bin")
            .join(built_in.map_or(id.as_str(), |(_, executable, _)| executable));
        if dry_run {
            writeln!(output, "download {source} -> {}", destination.display())
                .map_err(super::io_error)?;
            return Ok(());
        }
        progress(20, crate::install::ProgressStage::Copying);
        fs::create_dir_all(destination.parent().expect("bin parent"))
            .map_err(|error| Error::Io(format!("cannot create {}: {error}", root.display())))?;
        install_executable_source_in(context, &source, &destination, "tool")?;
        progress(100, crate::install::ProgressStage::Ready);
        writeln!(output, "Installed tool {id} at {}", destination.display())
            .map_err(super::io_error)?;
        return Ok(());
    }
    let plan = match built_in {
        Some((recipe, _, _)) => recipe,
        None => {
            return Err(Error::Input(format!(
                "{}; pass --source URL or PATH",
                suggestions::unknown("tool", &id, &tool_names().collect::<Vec<_>>())
            ))
            .into());
        }
    };
    if dry_run {
        writeln!(output, "{}", describe(plan, &root)).map_err(super::io_error)?;
        return Ok(());
    }
    let executable = install_recipe_in(context, plan, &root, output, progress)?;
    if let Some(tool) = tools::find(&id) {
        tool.post_install(context, &executable, progress, output)?;
    }
    progress(100, crate::install::ProgressStage::Ready);
    let kind = built_in.expect("known tool has metadata").2;
    writeln!(output, "Installed {kind} {id} at {}", executable.display())
        .map_err(super::io_error)?;
    Ok(())
}

/// Install only collector dependencies, not the selected collectors themselves.
/// Existing project-local binaries are never overwritten by this automatic path.
pub(crate) fn tool_installed_locally(id: &str, tools_root: &Path) -> bool {
    local_lookup(id, tools_root).path().is_some()
}

fn local_lookup(id: &str, tools_root: &Path) -> crate::executables::Lookup {
    if agents::find(id).is_some() {
        agents::local_lookup(id, tools_root)
    } else {
        tools::local_lookup(id, tools_root)
    }
}

pub(crate) fn ensure_collector_dependencies(
    selected: &[String],
    project_root: &Path,
    tools_root: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
    dry_run: bool,
) -> Result<(), Error> {
    let context = OperationContext::load(project_root).map_err(InstallFailure::into_public)?;
    let result = ensure_collector_dependencies_in(
        &context,
        selected,
        project_root,
        tools_root,
        output,
        progress,
        dry_run,
    );
    let diagnostic = context.report_diagnostics(output);
    result.map_err(InstallFailure::into_public)?;
    diagnostic
}

/// Prepare every project-local installable selected by an executable benchmark.
/// This is deliberately shared by direct runs and sweeps: record/replay alters
/// transport only: replay still needs the agent, workload, and collectors,
/// but its local tape endpoint does not need a live provider runtime.
pub(crate) fn ensure_execution_requirements(
    config: &BenchmarkConfig,
    config_path: &Path,
    mode: Option<crate::tape::Mode>,
    output: &mut dyn std::io::Write,
) -> Result<(), Error> {
    let (root, project_config) = project::load(config_path)?;
    let tools_root = project::resolve(&root, &project_config.tools_dir);
    let workloads_root = project::resolve(&root, &project_config.workloads_dir);
    let context =
        OperationContext::new(&root, &project_config).map_err(InstallFailure::into_public)?;
    let mut progress = |_percent: u8, _stage: ProgressStage| {};
    let result = (|| -> InstallResult<()> {
        for id in execution_tool_ids(config, mode) {
            let owned_context = context.with_destination(&tools_root.join(&id))?;
            match local_lookup(&id, &tools_root) {
                crate::executables::Lookup::Found(_) => {}
                crate::executables::Lookup::Unusable(path) => {
                    return Err(Error::Input(format!(
                        "selected tool executable {} is not a usable executable; remove or repair it, then run `asb tool install {id}`",
                        path.display()
                    )).into());
                }
                crate::executables::Lookup::Missing => tool_in(
                    &owned_context,
                    ToolInstallOptions {
                        name: id,
                        source: None,
                        dir: Some(tools_root.clone()),
                        dry_run: false,
                    },
                    output,
                    &mut progress,
                )?,
            }
        }
        if external_workload(&config.workload).is_some() {
            let owned_context = context.with_destination(&workloads_root.join(&config.workload))?;
            if !workload_available(&config.workload, &workloads_root) {
                let source = workload_source_in(&config.workload, &root, &project_config)?;
                workload_in(
                    &owned_context,
                    WorkloadInstallOptions {
                        source: config.workload.clone(),
                        name: None,
                        dir: Some(workloads_root.clone()),
                    },
                    source,
                    output,
                    &mut progress,
                )?;
            }
        }
        if let Some(measures) = config.measurements.as_ref().filter(|value| value.enabled) {
            ensure_collector_dependencies_in(
                &context,
                &measures.collectors,
                &root,
                &tools_root,
                output,
                &mut progress,
                false,
            )?;
        }
        Ok(())
    })();
    let diagnostic = context.report_diagnostics(output);
    result.map_err(InstallFailure::into_public)?;
    diagnostic
}

/// The project-local installation inputs for one executable benchmark.
///
/// This is intentionally narrower than `BenchmarkConfig`: a trial, model,
/// output path, workspace, retention policy, and repetition do not change a
/// project-local tool or workload installation.  Sweep preflight uses the key
/// to avoid repeatedly hashing/verifying an unchanged shared archive for each
/// task in a workload family.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ExecutionRequirements {
    tools: Vec<String>,
    workload: String,
    collectors: Vec<String>,
}

pub(crate) fn execution_requirements(
    config: &BenchmarkConfig,
    mode: Option<crate::tape::Mode>,
) -> ExecutionRequirements {
    let mut collectors = config
        .measurements
        .as_ref()
        .filter(|value| value.enabled)
        .map(|value| value.collectors.clone())
        .unwrap_or_default();
    collectors.sort();
    collectors.dedup();
    ExecutionRequirements {
        tools: execution_tool_ids(config, mode),
        workload: config.workload.clone(),
        collectors,
    }
}

fn execution_tool_ids(config: &BenchmarkConfig, mode: Option<crate::tape::Mode>) -> Vec<String> {
    std::iter::once(&config.agent)
        .chain((mode != Some(crate::tape::Mode::Replay)).then_some(&config.provider))
        .filter(|id| agents::find(id).is_some() || tools::find(id).is_some())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn ensure_collector_dependencies_in(
    context: &OperationContext,
    selected: &[String],
    _project_root: &Path,
    tools_root: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, ProgressStage),
    dry_run: bool,
) -> InstallResult<()> {
    for id in crate::collectors::dependency_tools(selected) {
        let tool = tools::find(id).ok_or_else(|| {
            Error::Input(format!("collector dependency `{id}` has no tool installer"))
        })?;
        let recipe = tool.recipe();
        let root = tools_root.join(id);
        let destination = executable(recipe, &root);
        let owned_context = if dry_run {
            context.clone()
        } else {
            context.with_destination(&root)?
        };
        let context = &owned_context;
        match tools::local_lookup(id, tools_root) {
            crate::executables::Lookup::Found(_) => continue,
            crate::executables::Lookup::Unusable(path) => {
                return Err(Error::Io(format!(
                    "required tool `{id}` at {} is not executable; make it executable or reinstall it, then retry",
                    path.display()
                )).into());
            }
            crate::executables::Lookup::Missing => {}
        }
        if dry_run {
            writeln!(
                output,
                "Required dependency {id}: {}",
                describe(recipe, &root)
            )
            .map_err(super::io_error)?;
            continue;
        }
        progress(5, crate::install::ProgressStage::Dependencies);
        let installed = match recipe {
            Recipe::Host { executable } => {
                install_host_dependency_in(context, executable, &destination)?
            }
            _ => install_recipe_in(context, recipe, &root, output, progress)?,
        };
        tool.post_install(context, &installed, progress, output)?;
        writeln!(
            output,
            "Installed required tool {id} at {}",
            installed.display()
        )
        .map_err(super::io_error)?;
    }
    Ok(())
}

#[cfg(test)]
fn install_host_dependency(name: &str, destination: &Path) -> Result<PathBuf, Error> {
    let context = OperationContext::new(
        destination.parent().unwrap_or(Path::new(".")),
        &project::ProjectConfig::default(),
    )
    .map_err(InstallFailure::into_public)?;
    install_host_dependency_in(&context, name, destination).map_err(InstallFailure::into_public)
}

fn install_host_dependency_in(
    context: &OperationContext,
    name: &str,
    destination: &Path,
) -> InstallResult<PathBuf> {
    let source = host_executable(name)?;
    let parent = destination.parent().expect("dependency has a parent");
    fs::create_dir_all(parent)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", parent.display())))?;
    source::install_input_in(
        context,
        &source::Input::pin_local(source)?,
        destination,
        source::Options {
            kind: name,
            limit: source::MAX_SOURCE_BYTES,
            checksum: None,
            executable: true,
        },
        |_, _| Ok(()),
        source::Publication::NoClobber,
    )?;
    Ok(destination.to_path_buf())
}

/// List every built-in tool recipe and its project-local executable.
pub fn tool_list(output: &mut dyn std::io::Write) -> Result<(), Error> {
    writeln!(output, "Available tools:").map_err(super::io_error)?;
    for agent in agents::all() {
        writeln!(
            output,
            "  {:<12} {}",
            agent.id(),
            tool_summary(agent.recipe())
        )
        .map_err(super::io_error)?;
    }
    for tool in tools::all() {
        writeln!(
            output,
            "  {:<12} {}",
            tool.id(),
            tool_summary(tool.recipe())
        )
        .map_err(super::io_error)?;
    }
    writeln!(output, "  custom       --source URL|PATH").map_err(super::io_error)
}

/// Report whether a built-in tool is runnable from the host or this project.
/// This is intentionally side-effect free so TUIs can refresh status on every
/// navigation or after a Ctrl-I installation attempt.
pub fn tool_available(name: &str, tools_root: &Path) -> bool {
    let Some((plan, _, _)) = built_in_tool(name) else {
        return false;
    };
    match local_lookup(name, tools_root) {
        crate::executables::Lookup::Found(_) => true,
        crate::executables::Lookup::Unusable(_) => false,
        crate::executables::Lookup::Missing => {
            matches!(plan, Recipe::Host { executable } if host_executable(executable).is_ok())
        }
    }
}

/// All installable IDs, composed from the agent and non-agent registries.
/// Keeping this here ensures completions, listings, and installation share the
/// same catalogue rather than duplicating agent identifiers.
pub(crate) fn tool_names() -> impl Iterator<Item = &'static str> {
    agents::names().chain(tools::names())
}

fn built_in_tool(id: &str) -> Option<(Recipe, &'static str, &'static str)> {
    if let Some(agent) = agents::find(id) {
        return Some((agent.recipe(), agent.executable(), "agent"));
    }
    let tool = tools::find(id)?;
    let kind = match tool.kind() {
        tools::ToolKind::Collector => "collector",
        tools::ToolKind::Support => "tool",
    };
    Some((tool.recipe(), tool.executable()?, kind))
}

pub fn workload(
    options: WorkloadInstallOptions,
    output: &mut dyn std::io::Write,
) -> Result<(), Error> {
    let mut progress = |_percent: u8, _stage: crate::install::ProgressStage| {};
    workload_with_progress(options, output, &mut progress)
}

/// Install a workload and report concrete download/extraction stages.
pub fn workload_with_progress(
    options: WorkloadInstallOptions,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> Result<(), Error> {
    let resolved = workload_source(&options.source)?;
    let context = OperationContext::new(&resolved.project_root, &resolved.project_config)
        .map_err(InstallFailure::into_public)?;
    let result = workload_in(&context, options, resolved, output, progress);
    let diagnostic = context.report_diagnostics(output);
    result.map_err(InstallFailure::into_public)?;
    diagnostic
}
fn workload_in(
    context: &OperationContext,
    options: WorkloadInstallOptions,
    resolved: ResolvedWorkloadSource,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, ProgressStage),
) -> InstallResult<()> {
    let WorkloadInstallOptions {
        source: requested,
        name: requested_name,
        dir: requested_dir,
    } = options;
    let ResolvedWorkloadSource {
        source,
        default_name,
        project_root,
        project_config,
    } = resolved;
    let name = requested_name.unwrap_or(default_name);
    crate::selection_names::validate(&name, "workload").map_err(Error::Input)?;
    if !valid_name(&name) {
        return Err(Error::Input(
            "workload name must contain only letters, numbers, '.', '_' or '-'".into(),
        )
        .into());
    }
    let directory = requested_dir
        .map(|value| resolve_path(&project_root, &value))
        .unwrap_or_else(|| {
            std::env::var_os("ASB_WORKLOAD_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| project::resolve(&project_root, &project_config.workloads_dir))
        });
    progress(2, crate::install::ProgressStage::Preparing);
    fs::create_dir_all(&directory)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", directory.display())))?;
    let owned_context = context.with_destination(&directory.join(&name))?;
    let context = &owned_context;
    // A custom prompt with a coincidentally matching destination name is not
    // an upstream dataset installation.
    if external_workload(&requested).is_some() && name == requested {
        let tools_root = project::resolve(&project_root, &project_config.tools_dir);
        if workloads::install_registered(
            &name,
            &workloads::InstallationContext {
                process: context,
                project_root: &project_root,
                tools_root: &tools_root,
                workloads_root: &directory,
            },
            output,
            progress,
        )? {
            return Ok(());
        }
    }
    if let Some(bundle) = workloads::source_bundle(&name) {
        install_bundle_in(context, &name, bundle, &directory, output, progress)?;
        return Ok(());
    }
    progress(30, crate::install::ProgressStage::Downloading);
    let reference = directory.join(format!("{name}.reference.md"));
    source::install_input_in(
        context,
        &source,
        &reference,
        source::Options {
            kind: "workload reference",
            limit: source::MAX_SOURCE_BYTES,
            checksum: None,
            executable: false,
        },
        |_, _| Ok(()),
        source::Publication::Replace,
    )?;
    workloads::materialize_external_adapter(&name, &directory)
        .map_err(|error| Error::Io(format!("cannot materialize {name} adapter trial: {error}")))?;
    let destination = directory.join(format!("{name}.txt"));
    if !destination.is_file() {
        fs::copy(&reference, &destination).map_err(|error| {
            Error::Io(format!(
                "cannot create workload prompt {}: {error}",
                destination.display()
            ))
        })?;
    }
    progress(100, crate::install::ProgressStage::Ready);
    writeln!(
        output,
        "Installed workload {name} at {}",
        destination.display()
    )
    .map_err(super::io_error)?;
    Ok(())
}

#[cfg(test)]
fn install_bundle(
    name: &str,
    bundle: workloads::SourceBundle,
    directory: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, ProgressStage),
) -> Result<(), Error> {
    let context = OperationContext::new(directory, &project::ProjectConfig::default())
        .map_err(InstallFailure::into_public)?;
    let context = context.with_destination(&directory.join(name))?;
    let result = install_bundle_in(&context, name, bundle, directory, output, progress);
    let diagnostic = context.report_diagnostics(output);
    result.map_err(InstallFailure::into_public)?;
    diagnostic
}

fn install_bundle_in(
    context: &OperationContext,
    name: &str,
    bundle: workloads::SourceBundle,
    directory: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> InstallResult<()> {
    let bundle_dir = directory.join(name);
    let source_dir = bundle_dir.join("source");
    let marker = bundle_dir.join("ASB-BUNDLE");
    if marker.is_file() && source_dir.is_dir() {
        progress(80, crate::install::ProgressStage::Validating);
        let installed = fs::read_to_string(&marker).map_err(|error| {
            Error::Io(format!(
                "cannot read external workload marker {}: {error}",
                marker.display()
            ))
        })?;
        if installed.trim() != bundle.marker {
            return Err(Error::Io(format!(
                "external workload {name} is installed at a different pinned revision; remove {} and retry",
                bundle_dir.display()
            )).into());
        }
        let prompt = directory.join(format!("{name}.txt"));
        if !prompt.exists() {
            fs::write(&prompt, bundle.prompt).map_err(|error| {
                Error::Io(format!(
                    "cannot write external workload prompt {}: {error}",
                    prompt.display()
                ))
            })?;
        }
        workloads::materialize_external_adapter(name, directory).map_err(|error| {
            Error::Io(format!("cannot materialize {name} adapter trial: {error}"))
        })?;
        workloads::finalize_install(name, directory).map_err(Error::Input)?;
        workloads::write_default_trial_set(name, directory)
            .map_err(|error| Error::Io(format!("cannot write {name} trial set: {error}")))?;
        writeln!(
            output,
            "External workload {name} already installed at {}",
            bundle_dir.display()
        )
        .map_err(super::io_error)?;
        progress(100, crate::install::ProgressStage::Ready);
        return Ok(());
    }
    if bundle_dir.exists() {
        return Err(Error::Io(format!(
            "external workload {name} has an incomplete local bundle at {}; remove that directory and retry",
            bundle_dir.display()
        )).into());
    }
    let archive = directory.join(format!(".{name}.tar.gz"));
    let extract = directory.join(format!(".{name}.extract"));
    if extract.exists() {
        return Err(Error::Io(format!(
            "previous extraction at {} was preserved; inspect it and move it aside before retrying",
            extract.display()
        ))
        .into());
    }
    if archive.exists() {
        return Err(Error::Io(format!(
            "previous download at {} was preserved; inspect it and move it aside before retrying",
            archive.display()
        ))
        .into());
    }
    fs::create_dir_all(&extract).map_err(|error| {
        Error::Io(format!(
            "cannot create extraction directory {}: {error}",
            extract.display()
        ))
    })?;
    progress(20, crate::install::ProgressStage::Downloading);
    install_source_in(
        context,
        bundle.archive,
        &archive,
        "external workload bundle",
    )?;
    progress(45, crate::install::ProgressStage::Extracting);
    let owner = ProducerDirectory::open(&extract)?;
    let mut scope = owner.scope(context);
    let mut step = scope.prepare("tar", Purpose::Extract, "external workload extraction")?;
    step.command_mut()
        .args(["--extract", "--gzip", "--file"])
        .arg(&archive)
        .args(["--directory"])
        .arg(&extract)
        .stdin(Stdio::null());
    let archive_bytes = fs::metadata(&archive).ok().map(|metadata| metadata.len());
    let mut extraction = transfer::Activity::new("Extracting external workload", archive_bytes);
    extraction.update(0);
    let status = scope.run(step)?.status;
    if let Some(bytes) = archive_bytes {
        extraction.update(bytes);
    }
    extraction.finish();
    owner.consume(scope.finish()?)?;
    if !status.success() {
        return Err(Error::Io(format!("could not extract external workload {name}")).into());
    }
    let top = fs::read_dir(&extract)
        .map_err(|error| Error::Io(format!("cannot inspect extracted {name}: {error}")))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .ok_or_else(|| {
            Error::Io(format!(
                "external workload {name} archive has no source directory"
            ))
        })?;
    fs::create_dir_all(&bundle_dir).map_err(|error| {
        Error::Io(format!(
            "cannot create external workload {}: {error}",
            bundle_dir.display()
        ))
    })?;
    fs::rename(&top, &source_dir).map_err(|error| {
        Error::Io(format!(
            "cannot place external workload source {}: {error}",
            source_dir.display()
        ))
    })?;
    fs::write(&marker, bundle.marker).map_err(|error| {
        Error::Io(format!(
            "cannot write external workload marker {}: {error}",
            marker.display()
        ))
    })?;
    let prompt = directory.join(format!("{name}.txt"));
    if !prompt.exists() {
        fs::write(&prompt, bundle.prompt).map_err(|error| {
            Error::Io(format!(
                "cannot write external workload prompt {}: {error}",
                prompt.display()
            ))
        })?;
    }
    workloads::materialize_external_adapter(name, directory)
        .map_err(|error| Error::Io(format!("cannot materialize {name} adapter trial: {error}")))?;
    workloads::finalize_install(name, directory).map_err(Error::Input)?;
    workloads::write_default_trial_set(name, directory)
        .map_err(|error| Error::Io(format!("cannot write {name} trial set: {error}")))?;
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&extract);
    writeln!(
        output,
        "Installed external workload {name} at {} with local source and prompt",
        bundle_dir.display()
    )
    .map_err(super::io_error)?;
    progress(100, crate::install::ProgressStage::Ready);
    Ok(())
}

/// List prompt workloads and configurable external workload trial sets.
pub fn workload_list(
    options: WorkloadListOptions,
    output: &mut dyn std::io::Write,
) -> Result<(), Error> {
    let (project_root, project_config) = project::load(Path::new("."))?;
    let directory = options
        .dir
        .map(|value| resolve_path(&project_root, &value))
        .or_else(|| std::env::var_os("ASB_WORKLOAD_DIR").map(PathBuf::from))
        .unwrap_or_else(|| project::resolve(&project_root, &project_config.workloads_dir));
    let mut names = fs::read_dir(&directory)
        .map_err(|error| Error::Io(format!("cannot read {}: {error}", directory.display())))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|value| value.to_str()) == Some("txt"))
                .then(|| path.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect::<Vec<_>>();
    names.sort();
    for name in &names {
        crate::selection_names::validate(name, "workload").map_err(Error::Input)?;
    }
    let mut trial_sets = fs::read_dir(&directory)
        .map_err(|error| Error::Io(format!("cannot read {}: {error}", directory.display())))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            name.strip_suffix(".workload.toml").map(str::to_owned)
        })
        .collect::<Vec<_>>();
    for name in &trial_sets {
        crate::selection_names::validate(name, "trial set").map_err(Error::Input)?;
    }
    trial_sets.sort();
    writeln!(output, "Available workloads in {}:", directory.display()).map_err(super::io_error)?;
    if names.is_empty() {
        writeln!(output, "  (none)").map_err(super::io_error)?;
    } else {
        for name in names {
            writeln!(output, "  {name}").map_err(super::io_error)?;
        }
    }
    writeln!(output, "Configured workload trial sets:").map_err(super::io_error)?;
    if trial_sets.is_empty() {
        writeln!(output, "  (none)").map_err(super::io_error)?;
    } else {
        for name in trial_sets {
            writeln!(output, "  {name} [trial set]").map_err(super::io_error)?;
        }
    }
    writeln!(output, "Installable external workloads:").map_err(super::io_error)?;
    for workload in external_workload_names() {
        writeln!(output, "  {workload}").map_err(super::io_error)?;
    }
    Ok(())
}

enum PreparedDistribution {
    Host(source::Input),
    Package(Recipe),
    Custom(CustomInstall),
}
struct PreparedRecipe<'a> {
    distribution: PreparedDistribution,
    root: &'a Path,
    context: &'a OperationContext,
    destination: PathBuf,
}
impl<'a> PreparedRecipe<'a> {
    fn resolve(
        context: &'a OperationContext,
        recipe: Recipe,
        root: &'a Path,
    ) -> InstallResult<Self> {
        let distribution = match recipe {
            Recipe::Host { executable } => {
                PreparedDistribution::Host(source::Input::pin_local(host_executable(executable)?)?)
            }
            Recipe::Custom { install, .. } => PreparedDistribution::Custom(install),
            Recipe::Npm { .. } | Recipe::Python { .. } => PreparedDistribution::Package(recipe),
        };
        Ok(Self {
            distribution,
            root,
            context,
            destination: executable(recipe, root),
        })
    }
    fn install(
        self,
        output: &mut dyn io::Write,
        progress: &mut dyn FnMut(u8, ProgressStage),
    ) -> InstallResult<PathBuf> {
        match self.distribution {
            PreparedDistribution::Host(source) => {
                progress(55, ProgressStage::Copying);
                fs::create_dir_all(self.destination.parent().expect("host tool parent")).map_err(
                    |error| Error::Io(format!("cannot create {}: {error}", self.root.display())),
                )?;
                source::install_input_in(
                    self.context,
                    &source,
                    &self.destination,
                    source::Options {
                        kind: "host executable",
                        limit: source::MAX_SOURCE_BYTES,
                        checksum: None,
                        executable: true,
                    },
                    |_, _| Ok(()),
                    source::Publication::Replace,
                )?;
                Ok(self.destination)
            }
            PreparedDistribution::Custom(install) => install(self.context, self.root, progress),
            PreparedDistribution::Package(recipe) => {
                install_package(self.context, recipe, self.root, output, progress)
            }
        }
    }
}

#[cfg(test)]
fn install_recipe(
    recipe: Recipe,
    root: &Path,
    project_root: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, ProgressStage),
) -> Result<PathBuf, Error> {
    let context = OperationContext::load(project_root).map_err(InstallFailure::into_public)?;
    install_recipe_in(&context, recipe, root, output, progress).map_err(InstallFailure::into_public)
}

fn install_recipe_in(
    context: &OperationContext,
    recipe: Recipe,
    root: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, ProgressStage),
) -> InstallResult<PathBuf> {
    fs::create_dir_all(root)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", root.display())))?;
    PreparedRecipe::resolve(context, recipe, root)?.install(output, progress)
}

fn install_package(
    context: &OperationContext,
    recipe: Recipe,
    root: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, ProgressStage),
) -> InstallResult<PathBuf> {
    fs::create_dir_all(root)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", root.display())))?;
    let owner = ProducerDirectory::open(root)?;
    let mut scope = owner.scope(context);
    let status = match recipe {
        Recipe::Npm {
            package,
            manifest: adapter_manifest,
            ..
        } => {
            ensure_runtime_dependency_in(
                context,
                "node",
                root.parent().expect("tool parent"),
                output,
                progress,
            )?;
            let manifest_path = root.join("package.json");
            if let Some(contents) = adapter_manifest.filter(|_| !manifest_path.exists()) {
                fs::write(&manifest_path, contents).map_err(|error| {
                    Error::Io(format!(
                        "cannot prepare adapter npm install manifest: {error}"
                    ))
                })?;
            }
            progress(15, ProgressStage::InstallingPackage);
            #[cfg(target_os = "linux")]
            let (npm, search, cwd) = {
                let inherited = std::env::var_os("PATH");
                let search = tools::runtime_path_override_at(
                    root.parent().expect("tool parent"),
                    inherited.as_deref(),
                )?
                .or(inherited);
                let cwd = std::env::current_dir().map_err(crate::io_error)?;
                let npm = match &search {
                    Some(search) => tools::host_executable_in_at(
                        Path::new("npm"),
                        Some(search),
                        &cwd,
                    )
                    .ok_or_else(|| Error::Io(
                        "agent package installation needs `npm`, but no executable is currently available on its effective project runtime PATH. Install a compatible Node.js/npm runtime (for fnm: `fnm install --lts && fnm use --lts`), then retry.".into(),
                    ))?,
                    // Unknown/unset PATH retains the existing platform lookup;
                    // never guess a default or classify a later127 as missing.
                    None => PathBuf::from("npm"),
                };
                (npm, search, cwd)
            };
            #[cfg(not(target_os = "linux"))]
            let npm = "npm";
            let mut step =
                scope.prepare(npm, Purpose::Package, "agent npm package installation")?;
            #[cfg(target_os = "linux")]
            {
                step.command_mut().current_dir(cwd);
                if let Some(search) = search {
                    step.command_mut().env("PATH", search);
                }
            }
            #[cfg(not(target_os = "linux"))]
            tools::apply_runtime_path_at(step.command_mut(), root.parent().expect("tool parent"))?;
            let cache = context.root().join(".asb-cache/npm");
            step.command_mut()
                .args(["install", "--prefix"])
                .arg(root)
                .args(["--no-save", package])
                .env("NPM_CONFIG_CACHE", &cache)
                .stdin(Stdio::null());
            let monitor = transfer::Cache::start_many(
                &[&cache, root],
                "Downloading and materializing npm package",
            );
            let result = scope.run(step);
            let _ = monitor.finish();
            let result = result?;
            result.status
        }
        Recipe::Python {
            package,
            dependencies,
            ..
        } => {
            let project_python = crate::python_runtime::ensure_in(context, output)?;
            let venv = root.join("venv");
            if fs::symlink_metadata(&venv).is_ok()
                && !crate::python_runtime::matches_project_in(
                    &mut scope,
                    &venv.join("bin/python"),
                    &project_python,
                )?
                && let Some(backup) = crate::python_runtime::preserve_environment(&venv)?
            {
                writeln!(
                    output,
                    "Preserved the previous tool Python environment at {}.",
                    backup.display()
                )
                .map_err(crate::io_error)?;
            }
            let mut step = scope.prepare(
                &project_python,
                Purpose::Configure,
                "agent Python environment creation",
            )?;
            step.command_mut()
                .args(["-m", "venv"])
                .arg(&venv)
                .stdin(Stdio::null());
            let created = scope.run(step)?;
            if !created.status.success() {
                return Err(Error::Io(format!("could not create the agent Python environment ({}); inspect the installation diagnostics", created.status)).into());
            }
            progress(35, ProgressStage::InstallingPackage);
            let python = venv.join("bin/python");
            let cache = context.root().join(".asb-cache/pip");
            let mut step = scope.prepare(
                &python,
                Purpose::Package,
                "agent Python package installation",
            )?;
            step.command_mut()
                .args(["-m", "pip", "install", package])
                .env("PIP_CACHE_DIR", &cache)
                .stdin(Stdio::null());
            let monitor = transfer::Cache::start_many(
                &[&cache, root],
                "Downloading and materializing pip package",
            );
            let installed = scope.run(step);
            let _ = monitor.finish();
            let installed = installed?;
            if !installed.status.success() {
                return Err(Error::Io(format!("Python package installation failed for {package} ({}); inspect the installation diagnostics", installed.status)).into());
            }
            for dependency in dependencies {
                progress(45, ProgressStage::Dependencies);
                let mut step = scope.prepare(
                    &python,
                    Purpose::Package,
                    "Python runtime dependency installation",
                )?;
                step.command_mut()
                    .args(["-m", "pip", "install", dependency])
                    .env("PIP_CACHE_DIR", &cache)
                    .stdin(Stdio::null());
                let monitor = transfer::Cache::start_many(
                    &[&cache, root],
                    "Downloading and materializing pip dependency",
                );
                let result = scope.run(step);
                let _ = monitor.finish();
                let result = result?;
                if !result.status.success() {
                    return Err(Error::Io(format!(
                        "Python runtime dependency installation failed for {dependency} ({})",
                        result.status
                    ))
                    .into());
                }
            }
            installed.status
        }
        Recipe::Host { .. } | Recipe::Custom { .. } => {
            unreachable!("prepared executable recipes bypass package managers")
        }
    };
    owner.consume(scope.finish()?)?;
    if !status.success() {
        return Err(Error::Io(format!("agent package installation failed ({status}); verify the prerequisite tool and rerun `asb tool install`")).into());
    }
    let executable = executable(recipe, root);
    // Package managers can successfully resolve a manifest while omitting a
    // platform-specific optional package.  Returning the expected pathname in
    // that case makes the install look successful and defers the actionable
    // failure until an unrelated runner later tries to resolve the client.
    // Follow the launcher symlink and require the same executable readiness
    // contract that the registry uses before publishing this installation.
    if !crate::executables::is_executable(&executable) {
        return Err(Error::Io(format!(
            "agent package installation completed, but its required executable `{}` was not installed or is not executable; verify that the selected package supports this platform and rerun `asb tool install`",
            executable.display()
        ))
        .into());
    }
    Ok(executable)
}

/// Reuse a compatible host runtime or install its registered project-local recipe.
#[cfg(test)]
pub(crate) fn ensure_runtime_dependency(
    id: &str,
    project_root: &Path,
    tools_root: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> Result<(), Error> {
    let context = OperationContext::load(project_root).map_err(InstallFailure::into_public)?;
    let result = ensure_runtime_dependency_in(&context, id, tools_root, output, progress);
    let diagnostic = context.report_diagnostics(output);
    result.map_err(InstallFailure::into_public)?;
    diagnostic
}

pub(crate) fn ensure_runtime_dependency_in(
    context: &OperationContext,
    id: &str,
    tools_root: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, ProgressStage),
) -> InstallResult<()> {
    let tool = tools::find(id)
        .ok_or_else(|| Error::Input(format!("runtime dependency `{id}` is not registered")))?;
    let owned_context = context.with_destination(&tools_root.join(id))?;
    let context = &owned_context;
    if tool_installed_locally(id, tools_root) || tool.host_runtime_usable(context)? {
        return Ok(());
    }
    let destination = install_recipe_in(
        context,
        tool.recipe(),
        &tools_root.join(id),
        output,
        progress,
    )?;
    tool.post_install(context, &destination, progress, output)?;
    writeln!(output, "Installed required runtime {id} for this project.")
        .map_err(crate::io_error)?;
    Ok(())
}

/// Inspect cached inputs without destroying evidence needed for repair.
pub(crate) fn verify_sha256_preserving(
    destination: &Path,
    expected_hash: &str,
    label: &str,
) -> Result<(), Error> {
    let handle = source::open_named(destination).map_err(crate::io_error)?;
    source::verify_pinned_checksum(
        &handle,
        destination,
        expected_hash,
        label,
        source::MAX_SOURCE_BYTES,
    )
}

pub(crate) fn command_output_in<S: AsRef<std::ffi::OsStr>>(
    scope: &mut ProcessScope,
    program: &Path,
    args: &[S],
    label: &str,
) -> InstallResult<String> {
    let mut step = scope.prepare(program, Purpose::Probe, label)?;
    step.capture(process::Capture::inspection());
    step.command_mut().args(args).stdin(Stdio::null());
    let output = scope.run(step)?;
    if !output.status.success() {
        return Err(Error::Io(format!(
            "cannot inspect {label}: command exited with {}; {}",
            output.status,
            if output.stderr.is_empty() {
                "no diagnostic output"
            } else {
                "diagnostic output was captured privately"
            }
        ))
        .into());
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if text.is_empty() {
        return Err(Error::Io(format!(
            "cannot inspect {label}: command returned no output"
        ))
        .into());
    }
    Ok(text)
}

pub(crate) fn install_source_in(
    context: &OperationContext,
    source: &str,
    destination: &Path,
    kind: &str,
) -> InstallResult<()> {
    install_source_bounded_in(context, source, destination, kind, source::MAX_SOURCE_BYTES)
}

pub(crate) fn install_source_bounded_in(
    context: &OperationContext,
    source: &str,
    destination: &Path,
    kind: &str,
    limit: u64,
) -> InstallResult<()> {
    source::install_input_in(
        context,
        &source::Input::from_os(std::ffi::OsStr::new(source)),
        destination,
        source::Options {
            kind,
            limit,
            checksum: None,
            executable: false,
        },
        |_, _| Ok(()),
        source::Publication::Replace,
    )
}

pub(crate) fn install_executable_source_in(
    context: &OperationContext,
    source: &str,
    destination: &Path,
    kind: &str,
) -> InstallResult<()> {
    source::install_input_in(
        context,
        &source::Input::from_os(std::ffi::OsStr::new(source)),
        destination,
        source::Options {
            kind,
            limit: source::MAX_SOURCE_BYTES,
            checksum: None,
            executable: true,
        },
        |_, _| Ok(()),
        source::Publication::Replace,
    )
}

pub(crate) fn install_verified_source_in(
    context: &OperationContext,
    source: &str,
    destination: &Path,
    kind: &str,
    checksum: &str,
    executable: bool,
    validate: impl FnOnce(&Path, &mut ProcessScope) -> InstallResult<()>,
) -> InstallResult<()> {
    install_verified_source_bounded_in(
        context,
        source,
        destination,
        VerifiedSource {
            kind,
            checksum,
            executable,
            limit: source::MAX_SOURCE_BYTES,
        },
        validate,
    )
}

/// Fixed source properties used for a bounded, checksum-verified download.
pub(crate) struct VerifiedSource<'a> {
    pub(crate) kind: &'a str,
    pub(crate) checksum: &'a str,
    pub(crate) executable: bool,
    pub(crate) limit: u64,
}

/// Install a source with a published checksum while retaining the caller's
/// bounded download budget.  Cache reuse validates this same checksum before
/// an entry is accepted, so a valid cache envelope cannot substitute another
/// payload for a pinned source.
pub(crate) fn install_verified_source_bounded_in(
    context: &OperationContext,
    source: &str,
    destination: &Path,
    verified: VerifiedSource<'_>,
    validate: impl FnOnce(&Path, &mut ProcessScope) -> InstallResult<()>,
) -> InstallResult<()> {
    source::install_input_in(
        context,
        &source::Input::from_os(std::ffi::OsStr::new(source)),
        destination,
        source::Options {
            kind: verified.kind,
            limit: verified.limit,
            checksum: Some(verified.checksum),
            executable: verified.executable,
        },
        validate,
        source::Publication::Replace,
    )
}

pub(crate) fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"unavailable\"".into())
}

fn executable(recipe: Recipe, root: &Path) -> PathBuf {
    match recipe {
        Recipe::Npm { executable, .. } => root.join("node_modules/.bin").join(executable),
        Recipe::Python { executable, .. } => root.join("venv/bin").join(executable),
        Recipe::Host { executable } => root.join("bin").join(executable),
        Recipe::Custom { executable, .. } => root.join("bin").join(executable),
    }
}

fn describe(recipe: Recipe, root: &Path) -> String {
    match recipe {
        Recipe::Npm { package, .. } => format!("npm install --prefix {} {package}", root.display()),
        Recipe::Python {
            package,
            dependencies,
            ..
        } => {
            let extras = if dependencies.is_empty() {
                String::new()
            } else {
                format!(" && pip install {}", dependencies.join(" "))
            };
            format!(
                "create {}/venv with project Python 3.12 or 3.13; install {package}{extras}",
                root.display()
            )
        }
        Recipe::Host { executable } => {
            format!("copy host `{executable}` into {}/bin", root.display())
        }
        Recipe::Custom { description, .. } => format!("{description} into {}/bin", root.display()),
    }
}

fn tool_summary(recipe: Recipe) -> String {
    match recipe {
        Recipe::Npm {
            package,
            executable,
            ..
        } => format!("npm {package} -> {executable}"),
        Recipe::Python {
            package,
            executable,
            ..
        } => format!("python {package} -> {executable}"),
        Recipe::Host { executable } => format!("host binary -> {executable}"),
        Recipe::Custom { summary, .. } => summary.into(),
    }
}

pub(crate) fn host_executable(name: &str) -> Result<PathBuf, Error> {
    let path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .chain([PathBuf::from("/usr/bin").join(name), PathBuf::from("/bin").join(name)])
        .find(|path| crate::executables::is_executable(path))
        .ok_or_else(|| {
            Error::Io(format!(
                "measurement tool `{name}` is not installed on this host; install its distribution package and rerun `asb tool install {name}`"
            ))
        })?;
    Ok(path)
}

#[derive(Debug)]
struct ResolvedWorkloadSource {
    source: source::Input,
    default_name: String,
    project_root: PathBuf,
    project_config: project::ProjectConfig,
}
fn workload_source(requested: &str) -> Result<ResolvedWorkloadSource, Error> {
    if external_workload(requested).is_none()
        && valid_name(requested)
        && !Path::new(requested).is_file()
    {
        let aliases = external_workload_names().collect::<Vec<_>>();
        if let Some(candidate) = suggestions::closest(requested, &aliases) {
            return Err(Error::Input(format!(
                "workload source `{requested}` was not found; did you mean external workload `{candidate}`?"
            )));
        }
    }
    let (project_root, project_config) = project::load(Path::new("."))?;
    workload_source_in(requested, &project_root, &project_config)
}

fn workload_source_in(
    requested: &str,
    project_root: &Path,
    project_config: &project::ProjectConfig,
) -> Result<ResolvedWorkloadSource, Error> {
    let source = external_workload(requested)
        .map(|workload| workload.source.to_owned())
        .unwrap_or_else(|| requested.to_owned());
    let filename = source
        .split('?')
        .next()
        .and_then(|value| value.rsplit('/').next())
        .unwrap_or(&source);
    let default_name = external_workload(requested)
        .map(|workload| workload.name.to_owned())
        .or_else(|| {
            Path::new(filename)
                .file_stem()
                .and_then(|value| value.to_str())
                .filter(|value| valid_name(value))
                .map(str::to_owned)
        })
        .ok_or(Error::Input(
            "workload source must have a simple file name".into(),
        ))?;
    Ok(ResolvedWorkloadSource {
        source: source::Input::from_os(source.as_ref()),
        default_name,
        project_root: project_root.to_path_buf(),
        project_config: project_config.clone(),
    })
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn resolve_path(root: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        root.join(value)
    }
}

#[cfg(test)]
fn missing_dependency(tool: &str, operation: &str, error: std::io::Error) -> Error {
    let command = match tool {
        "npm" => {
            "curl -fsSL https://fnm.vercel.app/install | bash && eval \"$(fnm env --shell bash)\" && fnm install --lts && fnm use --lts"
        }
        "python3" => {
            "curl -LsSf https://astral.sh/uv/install.sh | sh && ~/.local/bin/uv python install 3.12"
        }
        "curl" => "sudo apt-get update && sudo apt-get install -y curl",
        "tar" => "sudo apt-get update && sudo apt-get install -y tar",
        _ => "install the missing prerequisite and rerun the command",
    };
    Error::Io(format!(
        "{operation} needs `{tool}`, but it could not be started: {error}. Install it, then rerun the command. Suggested command:\n  {command}"
    ))
}

#[cfg(test)]
pub(crate) fn install_source(
    root: &Path,
    source: &str,
    destination: &Path,
    kind: &str,
) -> InstallResult<()> {
    install_source_with_limit(root, source, destination, kind, None)
}

#[cfg(test)]
fn install_source_with_limit(
    root: &Path,
    source: &str,
    destination: &Path,
    kind: &str,
    limit: Option<u64>,
) -> InstallResult<()> {
    let context = OperationContext::load(root)?;
    source::install_input_in(
        &context,
        &source::Input::from_os(source.as_ref()),
        destination,
        source::Options {
            kind,
            limit: limit.unwrap_or(source::MAX_SOURCE_BYTES),
            checksum: None,
            executable: false,
        },
        |_, _| Ok(()),
        source::Publication::Replace,
    )
}

pub(crate) fn make_executable(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .map_err(|error| Error::Io(format!("cannot inspect {}: {error}", path.display())))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).map_err(|error| {
            Error::Io(format!(
                "cannot make {} executable: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod coverage_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_requirements_select_agent_and_provider_owned_tools_once() {
        let config: BenchmarkConfig =
            toml::from_str("agent='pi'\nprovider='cli2key'\nmodel='fixture'\nworkload='bug-fix'\n")
                .unwrap();
        assert_eq!(execution_tool_ids(&config, None), ["cli2key", "pi"]);
        assert_eq!(
            execution_tool_ids(&config, Some(crate::tape::Mode::Record)),
            ["cli2key", "pi"]
        );
        assert_eq!(
            execution_tool_ids(&config, Some(crate::tape::Mode::Replay)),
            ["pi"]
        );
        assert_ne!(
            execution_requirements(&config, None),
            execution_requirements(&config, Some(crate::tape::Mode::Replay))
        );

        let endpoint_only: BenchmarkConfig = toml::from_str(
            "agent='pi'\nprovider='openrouter'\nmodel='fixture'\nworkload='bug-fix'\n",
        )
        .unwrap();
        assert_eq!(execution_tool_ids(&endpoint_only, None), ["pi"]);
    }

    #[test]
    fn execution_requirements_deduplicate_trials_but_not_install_inputs() {
        let base: BenchmarkConfig = toml::from_str(
            "agent='pi'\nprovider='cli2key'\nmodel='one'\nworkload='swe-bench-lite'\ntrial='task-a'\nlog_file='first.log'\n[measurements]\nenabled=true\ndirectory='first-measurements'\ncollectors=['perfetto','mpstat']\n",
        )
        .unwrap();
        let same_install: BenchmarkConfig = toml::from_str(
            "agent='pi'\nprovider='cli2key'\nmodel='two'\nworkload='swe-bench-lite'\ntrial='task-b'\nworkspace='other-workspace'\nlog_file='second.log'\n[measurements]\nenabled=true\ndirectory='second-measurements'\ncollectors=['mpstat','perfetto']\n",
        )
        .unwrap();
        let different_workload: BenchmarkConfig = toml::from_str(
            "agent='pi'\nprovider='cli2key'\nmodel='two'\nworkload='terminal-bench-2.0'\n",
        )
        .unwrap();
        let different_collector: BenchmarkConfig = toml::from_str(
            "agent='pi'\nprovider='cli2key'\nmodel='two'\nworkload='swe-bench-lite'\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['iostat']\n",
        )
        .unwrap();

        let replay = execution_requirements(&base, Some(crate::tape::Mode::Replay));
        assert_eq!(replay.tools, ["pi"]);
        assert_eq!(replay.workload, "swe-bench-lite");
        assert_eq!(replay.collectors, ["mpstat", "perfetto"]);

        assert_eq!(
            execution_requirements(&base, None),
            execution_requirements(&same_install, None)
        );
        assert_ne!(
            execution_requirements(&base, None),
            execution_requirements(&different_workload, None)
        );
        assert_ne!(
            execution_requirements(&base, None),
            execution_requirements(&different_collector, None)
        );
    }

    #[test]
    fn guest_runtime_requirements_come_from_every_registered_agent_recipe() {
        for id in agents::names() {
            let recipe = agents::find(id).unwrap().recipe();
            let expected: &[&str] = match recipe {
                Recipe::Npm { .. } => &["node"],
                Recipe::Python { .. } => &["uv"],
                Recipe::Custom { .. } => &[],
                _ => panic!("registered agent {id} needs an explicit guest bootstrap recipe"),
            };
            assert_eq!(recipe.guest_runtimes(), expected);
            for runtime in expected {
                let tool = tools::find(runtime).unwrap();
                let assets = tool
                    .guest_assets(Path::new("/fixture/tools/runtime"))
                    .unwrap();
                assert!(!assets.is_empty());
                for asset in assets {
                    assert!(asset.source.starts_with("/fixture/tools/runtime"));
                    assert!(!asset.target.is_absolute());
                    assert!(asset.target.starts_with("cache"));
                    let (url, hash) = asset
                        .distribution
                        .expect("runtime archive has no primary source");
                    assert!(url.starts_with("https://"));
                    assert_eq!(hash.len(), 64);
                }
            }
        }
    }

    #[test]
    fn bounded_runtime_copy_rejects_oversized_input_without_touching_existing_output() {
        let root =
            std::env::temp_dir().join(format!("asb-copy-bound-{}", tools::runtime::unique_id()));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join(".asb")).unwrap();
        fs::write(
            root.join(".asb/project.toml"),
            "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
        )
        .unwrap();
        let source = root.join("source");
        let destination = root.join("destination");
        fs::write(&source, b"four").unwrap();
        fs::write(&destination, b"preserved").unwrap();
        let error = install_source_with_limit(
            &root,
            source.to_str().unwrap(),
            &destination,
            "fixture",
            Some(3),
        )
        .unwrap_err();
        assert!(error.to_string().contains("size limit"), "{error}");
        assert_eq!(fs::read(&destination).unwrap(), b"preserved");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opencode_adapter_owns_its_npm_compatibility_manifest() {
        let Recipe::Npm {
            manifest: Some(contents),
            ..
        } = agents::find("opencode").unwrap().recipe()
        else {
            panic!("opencode must own its npm compatibility manifest");
        };
        let manifest: serde_json::Value =
            serde_json::from_str(contents).expect("valid npm manifest");
        assert_eq!(
            manifest["overrides"]["@aws-sdk/client-bedrock-runtime"],
            "3.1030.0"
        );
        assert_eq!(
            manifest["overrides"]["@aws-sdk/credential-provider-http"],
            "3.972.76"
        );
    }
    use crate::tools::bpftrace::{
        bpftrace_host_commands, bpftrace_missing_dependency, bpftrace_source_commands,
    };

    #[cfg(unix)]
    #[test]
    fn non_executable_dependency_is_not_reported_as_ready_or_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "asb-dependency-mode-{}-{stamp}",
            std::process::id()
        ));
        let perf = root.join("perf/bin/perf");
        fs::create_dir_all(perf.parent().unwrap()).unwrap();
        fs::write(&perf, b"preserve this tool").unwrap();
        fs::set_permissions(&perf, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!tool_installed_locally("perf", &root));
        assert!(!tool_available("perf", &root));
        let result = ensure_collector_dependencies(
            &["flamegraph".into()],
            &root,
            &root,
            &mut Vec::new(),
            &mut |_, _| {},
            false,
        );
        assert!(result.unwrap_err().to_string().contains("not executable"));
        assert_eq!(fs::read(&perf).unwrap(), b"preserve this tool");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installer_catalogue_is_composed_from_adapter_registries() {
        let names = tool_names().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names.len(), agents::all().len() + tools::all().len());
        for id in &names {
            let (recipe, executable, _) = built_in_tool(id).expect("registered install target");
            assert!(!describe(recipe, Path::new(".asb-tools")).is_empty());
            assert!(!executable.is_empty());
        }
        assert!(agents::all().iter().all(|agent| names.contains(agent.id())));
    }

    #[test]
    fn perfetto_recipe_uses_project_local_tracebox() {
        let recipe = tools::find("perfetto").expect("perfetto recipe").recipe();
        assert_eq!(
            executable(recipe, Path::new(".asb-tools/perfetto")),
            Path::new(".asb-tools/perfetto/bin/tracebox")
        );
        assert_eq!(
            Path::new(".asb-tools/perfetto/bin").join("trace_processor_shell"),
            Path::new(".asb-tools/perfetto/bin/trace_processor_shell")
        );
        assert!(describe(recipe, Path::new(".asb-tools/perfetto")).contains("SHA-256"));
    }

    #[test]
    fn tool_availability_prefers_project_local_installations() {
        let root = std::env::temp_dir().join(format!("asb-tool-status-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        assert!(!tool_available("not-a-tool", &root));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_progress_reports_real_workload_stages() {
        let root =
            std::env::temp_dir().join(format!("asb-install-progress-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let source = root.join("prompt.txt");
        fs::write(&source, "prompt").unwrap();
        let mut events = Vec::new();
        let mut progress = |percent: u8, stage: crate::install::ProgressStage| {
            events.push((percent, stage.to_owned()))
        };
        let mut output = Vec::new();
        workload_with_progress(
            WorkloadInstallOptions {
                source: source.to_string_lossy().into_owned(),
                name: None,
                dir: Some(root.join("workloads")),
            },
            &mut output,
            &mut progress,
        )
        .expect("local workload installation succeeds");
        assert!(
            events
                .iter()
                .any(|(percent, stage)| *percent == 2 && *stage == ProgressStage::Preparing)
        );
        assert!(
            events
                .iter()
                .any(|(percent, stage)| *percent == 100 && *stage == ProgressStage::Ready)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tool_list_includes_builtins_and_source_fallback() {
        let mut output = Vec::new();
        tool_list(&mut output).expect("tool list succeeds");
        let text = String::from_utf8(output).expect("utf8");
        assert!(!text.contains("codeagent"));
        assert!(text.contains("pi"));
        assert!(text.contains("openjiuwen"));
        assert!(!text.contains("llmtape"));
        assert!(text.contains("mpstat"));
        assert!(text.contains("bpftrace"));
        assert!(text.contains("custom"));
        assert!(tools::find("llmtape").is_none());
    }

    #[test]
    fn missing_prerequisite_messages_include_copyable_repair_commands() {
        let npm = missing_dependency(
            "npm",
            "agent installation",
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )
        .to_string();
        assert!(npm.contains("fnm install --lts"));
        let python = missing_dependency(
            "python3",
            "Python agent installation",
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )
        .to_string();
        assert!(python.contains("uv python install 3.12"));
        let curl = missing_dependency(
            "curl",
            "model discovery",
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )
        .to_string();
        assert!(curl.contains("apt-get install -y curl"));
    }

    #[test]
    fn bpftrace_repair_messages_cover_supported_distributions() {
        let host = bpftrace_host_commands();
        assert!(host.contains("Red Hat/Fedora:"));
        assert!(host.contains("sudo dnf install -y bpftrace"));
        assert!(host.contains("sudo apt-get update && sudo apt-get install -y bpftrace"));
        assert!(host.contains("openEuler:"));

        let source = bpftrace_source_commands();
        assert!(source.contains("bcc-devel"));
        assert!(source.contains("libbpfcc-dev"));
        assert!(source.contains("libbpf-dev"));
        assert!(source.contains("cmake"));
        assert!(source.contains("gcc-c++"));

        let cmake = bpftrace_missing_dependency(
            "cmake",
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )
        .to_string();
        assert!(cmake.contains("Red Hat/Fedora:"));
        assert!(cmake.contains("Debian/Ubuntu:"));
        assert!(cmake.contains("openEuler:"));
        assert!(cmake.contains("rerun `asb tool install bpftrace`"));
    }

    #[test]
    fn workload_list_enumerates_prompt_files() {
        let directory =
            std::env::temp_dir().join(format!("asb-workload-list-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("directory");
        fs::write(directory.join("zeta.txt"), "z").expect("prompt");
        fs::write(directory.join("alpha.txt"), "a").expect("prompt");
        fs::write(directory.join("ignored.md"), "ignored").expect("other file");
        let mut output = Vec::new();
        workload_list(
            WorkloadListOptions {
                dir: Some(directory.clone()),
            },
            &mut output,
        )
        .expect("workload list succeeds");
        let text = String::from_utf8(output).expect("utf8");
        assert!(text.find("alpha").expect("alpha") < text.find("zeta").expect("zeta"));
        assert!(!text.contains("ignored"));
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn external_workload_aliases_are_short_unique_and_pinned() {
        use sha2::{Digest, Sha256};
        let sources = workloads::registered_sources().collect::<Vec<_>>();
        assert_eq!(
            external_workload_names().collect::<Vec<_>>(),
            vec![
                "agentbench",
                "agentdojo",
                "aider-polyglot",
                "bigcodebench",
                "core-bench",
                "evalplus",
                "livecodebench-code-generation-v1",
                "livecodebench-code-generation-v2",
                "livecodebench-code-generation-v3",
                "livecodebench-code-generation-v4",
                "livecodebench-code-generation-v5",
                "livecodebench-code-generation-v6",
                "livecodebench-self-repair-v1",
                "livecodebench-self-repair-v2",
                "livecodebench-self-repair-v3",
                "livecodebench-self-repair-v4",
                "livecodebench-self-repair-v5",
                "livecodebench-self-repair-v6",
                "livecodebench",
                "livecodebench-test-output-prediction",
                "livecodebench-code-execution",
                "swe-bench",
                "swe-bench-lite",
                "swe-bench-pro",
                "swe-bench-verified",
                "swe-efficiency",
                "swe-lancer",
                "swe-perf",
                "swe-rebench",
                "tau-bench",
                "terminal-bench-1.0",
                "terminal-bench-2.0",
                "terminal-bench-2.1",
            ]
        );
        // Snapshot the exact public installation inventory and source URLs.
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&sources).unwrap())
            ),
            "d54b48513f72dd34cbc6428d045db839cbb14b0130724a6dd91d69584069180e",
            "installation names/order/URLs changed from the original registry"
        );
        let mut names = std::collections::BTreeSet::new();
        for (name, source) in workloads::registered_sources() {
            assert!(valid_name(name));
            assert!(
                names.insert(name),
                "duplicate workload registration: {name}"
            );
            assert_eq!(external_workload(name).unwrap().source, source);
            assert!(
                source.starts_with("https://raw.githubusercontent.com/")
                    || source.starts_with("https://github.com/")
                    || source.starts_with("https://huggingface.co/")
                    || source.starts_with("https://datasets-server.huggingface.co/")
            );
            assert!(
                source.contains("/releases/download/v")
                    || source.split(['/', '=', '&', '?']).any(|part| {
                        let revision = part.strip_suffix(".tar.gz").unwrap_or(part);
                        revision.len() == 40
                            && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
                    }),
                "unversioned source URL for {name}: {source}"
            );
        }
        assert_eq!(
            external_workload("swe-bench").map(|item| item.name),
            Some("swe-bench")
        );
        assert!(workloads::source_bundle("swe-bench").is_none());
        assert!(external_workload("methodology-only").is_none());
        assert!(workloads::source_bundle("methodology-only").is_none());
        assert!(external_workload("bug-fix").is_none());
        assert!(workloads::source_bundle("bug-fix").is_none());
        let installed = external_workload_names().collect::<std::collections::BTreeSet<_>>();
        let adapters =
            workloads::external_adapter_names().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            adapters, installed,
            "every external workload needs an adapter"
        );
    }

    #[test]
    fn unknown_agent_install_suggests_the_closest_builtin() {
        let error = tool(
            ToolInstallOptions {
                name: "opencod".into(),
                source: None,
                dir: None,
                dry_run: false,
            },
            &mut Vec::new(),
        )
        .expect_err("typo rejected");
        assert!(error.to_string().contains("did you mean `opencode`?"));
    }
}
