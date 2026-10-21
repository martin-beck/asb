//! Project-local installers for agents and workload prompt files.

use crate::Error;
use crate::agents;
use crate::project;
use crate::suggestions;
use crate::tools;
use crate::workloads;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

const GOOSE_X86_64_URL: &str = "https://github.com/aaif-goose/goose/releases/download/v1.54.0/goose-x86_64-unknown-linux-gnu.tar.gz";
const GOOSE_X86_64_SHA256: &str =
    "ab396c95aebbd28b4d38c175234be8d4a3ea0c96e7ccb24c307e4618c9be57e1";
const GOOSE_AARCH64_URL: &str = "https://github.com/aaif-goose/goose/releases/download/v1.54.0/goose-aarch64-unknown-linux-gnu.tar.gz";
const GOOSE_AARCH64_SHA256: &str =
    "0f174b5118a53888cd4e65b7ca9a4b72dd0b887e89142c8ce41e2409205e40d5";

// Some current agent packages declare an AWS SDK version whose registry
// metadata exists but whose tarball is not published. Keep the repair in the
// project-local npm manifest so installation remains deterministic and does
// not alter the host npm configuration.
const NPM_INSTALL_MANIFEST: &str = r#"{
  "private": true,
  "overrides": {
    "@aws-sdk/client-bedrock-runtime": "3.1030.0",
    "@aws-sdk/credential-provider-http": "3.972.76"
  }
}
"#;

pub(crate) trait LoggedStatus {
    fn logged_status(&mut self) -> io::Result<ExitStatus>;
}

impl LoggedStatus for Command {
    fn logged_status(&mut self) -> io::Result<ExitStatus> {
        project::apply_network_proxy(self, Path::new("."))
            .map_err(|error| io::Error::other(error.to_string()))?;
        let log_path = Path::new(".asb/install.log");
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        let stderr = stdout.try_clone()?;
        self.stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .status()
    }
}

type CustomInstall = fn(&Path, &mut dyn FnMut(u8, &str)) -> Result<PathBuf, Error>;

#[derive(Clone, Copy)]
pub(crate) enum Recipe {
    Npm {
        package: &'static str,
        executable: &'static str,
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
    },
    Goose,
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
            Self::Goose | Self::Host { .. } | Self::Custom { .. } => &[],
        }
    }
}

#[derive(serde::Serialize)]
pub(crate) struct GuestBootstrap {
    runtime_tools: Vec<&'static str>,
    assets: Vec<tools::GuestAsset>,
    public_package_cache: bool,
}

#[cfg(test)]
impl GuestBootstrap {
    /// A protocol fixture is not qualification of a client/runtime installation.
    pub(crate) fn fixture_empty() -> Self {
        Self {
            runtime_tools: Vec::new(),
            assets: Vec::new(),
            public_package_cache: false,
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
    let _network = project::network_project(project_root);
    let mut progress = |_percent: u8, _stage: &str| {};
    let mut plan = GuestBootstrap {
        runtime_tools: recipe.guest_runtimes().to_vec(),
        assets: Vec::new(),
        public_package_cache: matches!(recipe, Recipe::Npm { .. }),
    };
    for id in &plan.runtime_tools {
        let tool = tools::find(id)
            .ok_or_else(|| Error::Input("guest runtime is not registered".into()))?;
        let root = tools_root.join(id);
        // Prefetch archives without replacing any host runtime executables.
        for mut asset in tool.guest_assets(&root)? {
            if let Some((url, checksum)) = asset.distribution {
                tools::runtime::cached_archive(&asset.source, url, checksum, &mut progress)?;
            }
            asset.target = Path::new(id).join(asset.target);
            plan.assets.push(asset);
        }
    }
    if matches!(recipe, Recipe::Goose) {
        let archive = goose_archive(&tools_root.join(id), &mut progress)?;
        plan.assets.push(tools::GuestAsset {
            source: archive,
            target: Path::new(id).join("cache/goose.tar.gz"),
            distribution: None,
        });
    }
    Ok(plan)
}

#[derive(Clone, Copy)]
struct ExternalWorkload {
    name: &'static str,
    source: &'static str,
}

struct BundleSpec {
    archive: &'static str,
    marker: &'static str,
    prompt: &'static str,
}

const EXTERNAL_WORKLOADS: &[ExternalWorkload] = &[
    ExternalWorkload {
        name: "agentbench",
        source: "https://raw.githubusercontent.com/THUDM/AgentBench/d1e4a10db08c87075c78972e48ecc182be03e2d5/README.md",
    },
    ExternalWorkload {
        name: "agentdojo",
        source: "https://raw.githubusercontent.com/ethz-spylab/agentdojo/089ed468cf3ed0322acc66b0211f26d9d90dbf60/README.md",
    },
    ExternalWorkload {
        name: "aider-polyglot",
        source: "https://raw.githubusercontent.com/Aider-AI/aider/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/README.md",
    },
    ExternalWorkload {
        name: "bigcodebench",
        source: "https://raw.githubusercontent.com/bigcode-project/bigcodebench/9059fb84d1188c02edeac4995361656a2fdecbef/README.md",
    },
    ExternalWorkload {
        name: "core-bench",
        source: "https://raw.githubusercontent.com/siegelz/core-bench/e32a2980e72fe6eb04ee04eb749458f570625663/README.md",
    },
    ExternalWorkload {
        name: "evalplus",
        source: "https://raw.githubusercontent.com/evalplus/evalplus/e5d0ed0bab96280b60b637ec7f15b5e4841b0cb2/README.md",
    },
    ExternalWorkload {
        name: "livecodebench",
        source: "https://raw.githubusercontent.com/LiveCodeBench/LiveCodeBench/28fef95ea8c9f7a547c8329f2cd3d32b92c1fa24/README.md",
    },
    ExternalWorkload {
        name: "swe-bench",
        source: "https://raw.githubusercontent.com/SWE-bench/SWE-bench/02e7a74ffd0b707aab73d203fe87bdc7c76afc8e/README.md",
    },
    ExternalWorkload {
        name: "swe-bench-lite",
        source: "https://raw.githubusercontent.com/SWE-bench/SWE-bench/02e7a74ffd0b707aab73d203fe87bdc7c76afc8e/README.md",
    },
    ExternalWorkload {
        name: "swe-bench-pro",
        source: "https://raw.githubusercontent.com/scaleapi/SWE-bench_Pro-os/ca10a60a5fcae51e6948ffe1485d4153d421e6c5/README.md",
    },
    ExternalWorkload {
        name: "swe-bench-verified",
        source: "https://raw.githubusercontent.com/SWE-bench/SWE-bench/02e7a74ffd0b707aab73d203fe87bdc7c76afc8e/README.md",
    },
    ExternalWorkload {
        name: "swe-efficiency",
        source: "https://raw.githubusercontent.com/swefficiency/swefficiency/12d32a2d6800824a7d84bdb6797b5708e7b7957f/README.md",
    },
    ExternalWorkload {
        name: "swe-lancer",
        source: "https://raw.githubusercontent.com/openai/frontier-evals/51052cede8cc608f95bb00346635e03759013e5a/README.md",
    },
    ExternalWorkload {
        name: "swe-perf",
        source: "https://raw.githubusercontent.com/SWE-Perf/SWE-Perf/9e8fed285ed640b51baaf66b13773706dc95aa45/README.md",
    },
    ExternalWorkload {
        name: "swe-rebench",
        source: "https://raw.githubusercontent.com/SWE-rebench/SWE-bench-fork/e4907b7a90eafaa1f0a6428fd04fe31cdd8b4284/README.md",
    },
    ExternalWorkload {
        name: "tau-bench",
        source: "https://raw.githubusercontent.com/sierra-research/tau-bench/59a200c6d575d595120f1cb70fea53cef0632f6b/README.md",
    },
    ExternalWorkload {
        name: "terminal-bench",
        source: "https://raw.githubusercontent.com/harbor-framework/terminal-bench/452bf305c6daa62fc59061d22133a7cbc7c1572e/README.md",
    },
];

fn external_workload(name: &str) -> Option<ExternalWorkload> {
    EXTERNAL_WORKLOADS
        .iter()
        .find(|workload| workload.name == name)
        .copied()
        .or_else(|| {
            workloads::registered_sources()
                .find(|(id, _)| *id == name)
                .map(|(name, source)| ExternalWorkload { name, source })
        })
}

/// Return the pinned external workloads that the TUIs can offer for install.
pub fn external_workload_names() -> impl Iterator<Item = &'static str> {
    EXTERNAL_WORKLOADS
        .iter()
        .map(|workload| workload.name)
        .chain(workloads::registered_sources().map(|(id, _)| id))
}

/// Check whether an external workload has its runnable local bundle and
/// prompt. This is side-effect free and safe to call while rendering a TUI.
pub fn workload_available(name: &str, directory: &Path) -> bool {
    if let Some(available) = workloads::installed_available(name, directory) {
        return available;
    }
    if bundle_spec(name).is_some() {
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

fn bundle_spec(name: &str) -> Option<BundleSpec> {
    match name {
        "terminal-bench" => Some(BundleSpec {
            archive: "https://github.com/harbor-framework/terminal-bench/archive/452bf305c6daa62fc59061d22133a7cbc7c1572e.tar.gz",
            marker: "terminal-bench/terminal-bench@452bf305c6daa62fc59061d22133a7cbc7c1572e",
            prompt: "Use the installed upstream Terminal-Bench checkout in `external-workload`. Inspect its README, task definitions, and local runner instructions, then execute or improve a concrete terminal task using only the installed checkout and run its focused tests. Do not download additional data during the run.\n",
        }),
        _ => None,
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
    let mut progress = |_percent: u8, _stage: &str| {};
    tool_with_progress(options, output, &mut progress)
}

/// Install a project-local tool and report concrete installer stages.
pub fn tool_with_progress(
    options: ToolInstallOptions,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, &str),
) -> Result<(), Error> {
    let ToolInstallOptions {
        name: id,
        source,
        dir,
        dry_run,
    } = options;
    crate::selection_names::validate(&id, "tool").map_err(Error::Input)?;
    if !valid_name(&id) {
        return Err(Error::Input(format!("invalid tool name `{id}`")));
    }
    let (project_root, project_config) = project::load(Path::new("."))?;
    let tools_root = dir
        .map(|value| project::resolve(&project_root, &value.to_string_lossy()))
        .unwrap_or_else(|| project::resolve(&project_root, &project_config.tools_dir));
    progress(2, "preparing");
    let root = tools_root.join(&id);
    let built_in = built_in_tool(&id);
    ensure_collector_dependencies(
        std::slice::from_ref(&id),
        &project_root,
        &tools_root,
        output,
        progress,
        dry_run,
    )?;
    if let Some(source) = source {
        let destination = root
            .join("bin")
            .join(built_in.map_or(id.as_str(), |(_, executable, _)| executable));
        if dry_run {
            writeln!(output, "download {source} -> {}", destination.display())
                .map_err(super::io_error)?;
            return Ok(());
        }
        progress(20, "copying source");
        fs::create_dir_all(destination.parent().expect("bin parent"))
            .map_err(|error| Error::Io(format!("cannot create {}: {error}", root.display())))?;
        install_source(&source, &destination, "tool")?;
        make_executable(&destination)?;
        progress(100, "ready");
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
            )));
        }
    };
    if dry_run {
        writeln!(output, "{}", describe(plan, &root)).map_err(super::io_error)?;
        return Ok(());
    }
    let executable = install_recipe(plan, &root, &project_root, output, progress)?;
    if let Some(tool) = tools::find(&id) {
        tool.post_install(&executable, progress, output)?;
    }
    progress(100, "ready");
    let kind = built_in.expect("known tool has metadata").2;
    writeln!(output, "Installed {kind} {id} at {}", executable.display())
        .map_err(super::io_error)?;
    Ok(())
}

/// Install only collector dependencies, not the selected collectors themselves.
/// Existing project-local binaries are never overwritten by this automatic path.
pub(crate) fn tool_installed_locally(id: &str, tools_root: &Path) -> bool {
    tools::find(id)
        .is_some_and(|tool| executable_file(&executable(tool.recipe(), &tools_root.join(id))))
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub(crate) fn ensure_collector_dependencies(
    selected: &[String],
    project_root: &Path,
    tools_root: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, &str),
    dry_run: bool,
) -> Result<(), Error> {
    for id in crate::collectors::dependency_tools(selected) {
        let tool = tools::find(id).ok_or_else(|| {
            Error::Input(format!("collector dependency `{id}` has no tool installer"))
        })?;
        let recipe = tool.recipe();
        let root = tools_root.join(id);
        let destination = executable(recipe, &root);
        if destination.is_file() {
            if executable_file(&destination) {
                continue;
            }
            return Err(Error::Io(format!(
                "required tool `{id}` at {} is not executable; make it executable or reinstall it, then retry",
                destination.display()
            )));
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
        progress(5, &format!("installing required {id}"));
        let installed = match recipe {
            Recipe::Host { executable } => install_host_dependency(executable, &destination)?,
            _ => install_recipe(recipe, &root, project_root, output, progress)?,
        };
        tool.post_install(&installed, progress, output)?;
        writeln!(
            output,
            "Installed required tool {id} at {}",
            installed.display()
        )
        .map_err(super::io_error)?;
    }
    Ok(())
}

fn install_host_dependency(name: &str, destination: &Path) -> Result<PathBuf, Error> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let source = host_executable(name)?;
    let parent = destination.parent().expect("dependency has a parent");
    fs::create_dir_all(parent)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", parent.display())))?;
    let temporary = parent.join(format!(
        ".asb-{name}-{}-{}",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let mut created = false;
    let result = (|| -> Result<(), Error> {
        let mut input = fs::File::open(&source)
            .map_err(|error| Error::Io(format!("cannot read {}: {error}", source.display())))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| {
                Error::Io(format!("cannot create {}: {error}", temporary.display()))
            })?;
        created = true;
        io::copy(&mut input, &mut output)
            .map_err(|error| Error::Io(format!("cannot install {name}: {error}")))?;
        drop(output);
        fs::set_permissions(
            &temporary,
            input
                .metadata()
                .map_err(|error| Error::Io(error.to_string()))?
                .permissions(),
        )
        .map_err(|error| Error::Io(error.to_string()))?;
        make_executable(&temporary)?;
        match fs::hard_link(&temporary, destination) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && destination.is_file() => {
                Ok(())
            }
            Err(error) => Err(Error::Io(format!(
                "cannot install {name} at {}: {error}",
                destination.display()
            ))),
        }
    })();
    if created {
        let _ = fs::remove_file(&temporary);
    }
    result?;
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
    let local = executable(plan, &tools_root.join(name));
    if local.is_file() {
        return executable_file(&local);
    }
    matches!(plan, Recipe::Host { executable } if host_executable(executable).is_ok_and(|path| executable_file(&path)))
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
    let mut progress = |_percent: u8, _stage: &str| {};
    workload_with_progress(options, output, &mut progress)
}

/// Install a workload and report concrete download/extraction stages.
pub fn workload_with_progress(
    options: WorkloadInstallOptions,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, &str),
) -> Result<(), Error> {
    let WorkloadInstallOptions {
        source: requested,
        name: requested_name,
        dir: requested_dir,
    } = options;
    let (source, default_name, project_root, project_config) = workload_source(&requested)?;
    let name = requested_name.unwrap_or(default_name);
    crate::selection_names::validate(&name, "workload").map_err(Error::Input)?;
    if !valid_name(&name) {
        return Err(Error::Input(
            "workload name must contain only letters, numbers, '.', '_' or '-'".into(),
        ));
    }
    let directory = requested_dir
        .map(|value| project::resolve(&project_root, &value.to_string_lossy()))
        .unwrap_or_else(|| {
            std::env::var_os("ASB_WORKLOAD_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| project::resolve(&project_root, &project_config.workloads_dir))
        });
    progress(2, "preparing");
    fs::create_dir_all(&directory)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", directory.display())))?;
    // A custom prompt with a coincidentally matching destination name is not
    // an upstream dataset installation.
    if external_workload(&requested).is_some() && name == requested {
        let tools_root = project::resolve(&project_root, &project_config.tools_dir);
        if workloads::install_registered(
            &name,
            &workloads::InstallationContext {
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
    if let Some(bundle) = bundle_spec(&name) {
        install_bundle(&name, bundle, &directory, output, progress)?;
        return Ok(());
    }
    progress(30, "downloading reference");
    let reference = directory.join(format!("{name}.reference.md"));
    install_source(&source, &reference, "workload reference")?;
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
    progress(100, "ready");
    writeln!(
        output,
        "Installed workload {name} at {}",
        destination.display()
    )
    .map_err(super::io_error)?;
    Ok(())
}

fn install_bundle(
    name: &str,
    bundle: BundleSpec,
    directory: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, &str),
) -> Result<(), Error> {
    let bundle_dir = directory.join(name);
    let source_dir = bundle_dir.join("source");
    let marker = bundle_dir.join("ASB-BUNDLE");
    if marker.is_file() && source_dir.is_dir() {
        progress(80, "validating existing bundle");
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
            )));
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
        workloads::write_default_trial_set(name, directory)
            .map_err(|error| Error::Io(format!("cannot write {name} trial set: {error}")))?;
        writeln!(
            output,
            "External workload {name} already installed at {}",
            bundle_dir.display()
        )
        .map_err(super::io_error)?;
        progress(100, "ready");
        return Ok(());
    }
    if bundle_dir.exists() {
        return Err(Error::Io(format!(
            "external workload {name} has an incomplete local bundle at {}; remove that directory and retry",
            bundle_dir.display()
        )));
    }
    let archive = directory.join(format!(".{name}.tar.gz"));
    let extract = directory.join(format!(".{name}.extract"));
    if extract.exists() {
        fs::remove_dir_all(&extract).map_err(|error| {
            Error::Io(format!(
                "cannot clear stale extraction {}: {error}",
                extract.display()
            ))
        })?;
    }
    fs::create_dir_all(&extract).map_err(|error| {
        Error::Io(format!(
            "cannot create extraction directory {}: {error}",
            extract.display()
        ))
    })?;
    progress(20, "downloading bundle");
    install_source(bundle.archive, &archive, "external workload bundle")?;
    progress(45, "extracting bundle");
    let status = Command::new("tar")
        .args(["--extract", "--gzip", "--file"])
        .arg(&archive)
        .args(["--directory"])
        .arg(&extract)
        .logged_status()
        .map_err(|error| missing_dependency("tar", "external workload installation", error))?;
    if !status.success() {
        return Err(Error::Io(format!(
            "could not extract external workload {name}"
        )));
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
    progress(100, "ready");
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
        .map(|value| project::resolve(&project_root, &value.to_string_lossy()))
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

fn install_recipe(
    recipe: Recipe,
    root: &Path,
    project_root: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, &str),
) -> Result<PathBuf, Error> {
    fs::create_dir_all(root)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", root.display())))?;
    let status = match recipe {
        Recipe::Npm { package, .. } => {
            ensure_runtime_dependency("node", project_root, root.parent().expect("tool parent"), output, progress)?;
            let manifest = root.join("package.json");
            if !manifest.exists() {
                fs::write(&manifest, NPM_INSTALL_MANIFEST).map_err(|error| {
                    Error::Io(format!("cannot prepare npm install manifest: {error}"))
                })?;
            }
            progress(15, "installing npm package");
            let mut command = Command::new("npm");
            tools::apply_runtime_path(&mut command, project_root, &root.parent().expect("tool parent").to_string_lossy())?;
            command.args(["install", "--prefix"])
                .arg(root)
                .args(["--no-save", package])
                .env("NPM_CONFIG_CACHE", project_root.join(".asb-cache/npm"))
                .logged_status()
        }
        Recipe::Python {
            package,
            dependencies,
            ..
        } => {
            let project_python = crate::python_runtime::ensure(project_root, output)?;
            let venv = root.join("venv");
            if fs::symlink_metadata(&venv).is_ok() && !crate::python_runtime::matches_project(&venv.join("bin/python"), &project_python)
                && let Some(backup) = crate::python_runtime::preserve_environment(&venv)? {
                writeln!(output, "Preserved the previous tool Python environment at {}.", backup.display())
                    .map_err(crate::io_error)?;
            }
            let created = Command::new(&project_python)
                .args(["-m", "venv"]).arg(&venv).logged_status()
                .map_err(|error| Error::Io(format!("cannot create agent environment with {}: {error}", project_python.display())))?;
            if !created.success() {
                return Err(Error::Io("could not create the agent Python environment; see .asb/install.log for details".into()));
            }
            progress(35, "installing Python package");
            let python = venv.join("bin/python");
            let status = Command::new(&python)
                .args(["-m", "pip", "install", package])
                .logged_status()
                .map_err(|error| {
                    Error::Io(format!("cannot install Python package {package}: {error}"))
                })?;
            if !status.success() {
                return Err(Error::Io(format!(
                    "Python package installation failed for {package}; see .asb/install.log for details"
                )));
            }
            for dependency in dependencies {
                progress(45, "installing Python runtime dependency");
                let dependency_status = Command::new(&python)
                    .args(["-m", "pip", "install", dependency])
                    .logged_status()
                    .map_err(|error| {
                        Error::Io(format!(
                            "cannot install Python runtime dependency {dependency}: {error}"
                        ))
                    })?;
                if !dependency_status.success() {
                    return Err(Error::Io(format!(
                        "Python runtime dependency installation failed for {dependency}"
                    )));
                }
            }
            Ok(status)
        }
        Recipe::Host { executable } => {
            progress(55, "copying host tool");
            let source = host_executable(executable)?;
            let destination = root.join("bin").join(executable);
            fs::create_dir_all(destination.parent().expect("host tool parent"))
                .map_err(|error| Error::Io(format!("cannot create {}: {error}", root.display())))?;
            fs::copy(&source, &destination).map_err(|error| {
                Error::Io(format!(
                    "cannot copy host tool {} to {}: {error}",
                    source.display(),
                    destination.display()
                ))
            })?;
            make_executable(&destination)?;
            return Ok(destination);
        }
        Recipe::Custom { install, .. } => return install(root, progress),
        Recipe::Goose => return install_goose(root, progress),
    }
    .map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            let dependency = match recipe {
                Recipe::Npm { .. } => "npm",
                Recipe::Python { .. } => "python3",
                Recipe::Host { .. } => "host measurement binary",
                Recipe::Custom { .. } => "custom installer dependency",
                Recipe::Goose => "curl, tar, and sha256sum",
            };
            missing_dependency(dependency, "agent installation", error)
        } else {
            Error::Io(format!("cannot start agent installer: {error}"))
        }
    })?;
    if !status.success() {
        return Err(Error::Io(
            "agent package installation failed; verify the prerequisite tool and rerun `asb tool install`".into(),
        ));
    }
    let executable = executable(recipe, root);
    Ok(executable)
}

/// Reuse a compatible host runtime or install its registered project-local recipe.
pub(crate) fn ensure_runtime_dependency(
    id: &str,
    project_root: &Path,
    tools_root: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, &str),
) -> Result<(), Error> {
    let tool = tools::find(id)
        .ok_or_else(|| Error::Input(format!("runtime dependency `{id}` is not registered")))?;
    if tool_installed_locally(id, tools_root) || tool.host_runtime_usable() {
        return Ok(());
    }
    let destination = install_recipe(
        tool.recipe(),
        &tools_root.join(id),
        project_root,
        output,
        progress,
    )?;
    tool.post_install(&destination, progress, output)?;
    writeln!(output, "Installed required runtime {id} for this project.")
        .map_err(crate::io_error)?;
    Ok(())
}

fn goose_archive(root: &Path, progress: &mut dyn FnMut(u8, &str)) -> Result<PathBuf, Error> {
    let (url, checksum) = match std::env::consts::ARCH {
        "x86_64" => (GOOSE_X86_64_URL, GOOSE_X86_64_SHA256),
        "aarch64" => (GOOSE_AARCH64_URL, GOOSE_AARCH64_SHA256),
        architecture => {
            return Err(Error::Input(format!(
                "Goose is packaged by ASB only for Linux x86_64 and aarch64; this host is {architecture}"
            )));
        }
    };
    let archive = root.join("cache/goose.tar.gz");
    tools::runtime::cached_archive(&archive, url, checksum, progress)
}

fn install_goose(root: &Path, progress: &mut dyn FnMut(u8, &str)) -> Result<PathBuf, Error> {
    let archive = goose_archive(root, progress)?;
    let extract = root.join(format!("extract-{}", tools::runtime::unique_id()));
    fs::create_dir(&extract)
        .map_err(|error| Error::Io(format!("cannot create Goose extraction directory: {error}")))?;
    progress(60, "extracting Goose binary");
    let status = Command::new("tar")
        .args(["--extract", "--gzip", "--file"])
        .arg(&archive)
        .args(["--directory"])
        .arg(&extract)
        .logged_status()
        .map_err(|error| missing_dependency("tar", "Goose installation", error))?;
    if !status.success() {
        return Err(Error::Io(
            "could not extract the pinned Goose binary".into(),
        ));
    }
    let source = extract.join("goose");
    if !source.is_file() {
        return Err(Error::Io(
            "pinned Goose archive did not contain a goose executable".into(),
        ));
    }
    let destination = root.join("bin").join("goose");
    fs::create_dir_all(destination.parent().expect("Goose bin parent"))
        .map_err(|error| Error::Io(format!("cannot create Goose bin directory: {error}")))?;
    fs::copy(&source, &destination).map_err(|error| {
        Error::Io(format!(
            "cannot install Goose at {}: {error}",
            destination.display()
        ))
    })?;
    make_executable(&destination)?;
    let _ = fs::remove_dir_all(extract);
    progress(100, "ready");
    Ok(destination)
}

pub(crate) fn verify_sha256(
    destination: &Path,
    expected_hash: &str,
    label: &str,
) -> Result<(), Error> {
    verify_sha256_inner(destination, expected_hash, label, true)
}

/// Inspect cached inputs without destroying evidence needed for repair.
pub(crate) fn verify_sha256_preserving(
    destination: &Path,
    expected_hash: &str,
    label: &str,
) -> Result<(), Error> {
    verify_sha256_inner(destination, expected_hash, label, false)
}

fn verify_sha256_inner(
    destination: &Path,
    expected_hash: &str,
    label: &str,
    remove_invalid: bool,
) -> Result<(), Error> {
    let digest = command_output(
        &PathBuf::from("sha256sum"),
        &[destination.to_string_lossy().as_ref()],
        &format!("{label} checksum"),
    )?;
    let actual_hash = digest.split_whitespace().next().unwrap_or_default();
    if actual_hash != expected_hash {
        if remove_invalid {
            let _ = fs::remove_file(destination);
        }
        return Err(Error::Io(format!(
            "{label} checksum mismatch: expected {expected_hash}, got {actual_hash}"
        )));
    }
    Ok(())
}

pub(crate) fn command_output(program: &Path, args: &[&str], label: &str) -> Result<String, Error> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| Error::Io(format!("cannot inspect {label}: {error}")))?;
    if !output.status.success() {
        return Err(Error::Io(format!(
            "cannot inspect {label}: command exited with {}",
            output.status
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if text.is_empty() {
        return Err(Error::Io(format!(
            "cannot inspect {label}: command returned no output"
        )));
    }
    Ok(text)
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
        Recipe::Goose => root.join("bin").join("goose"),
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
        Recipe::Goose => format!(
            "download and verify pinned Goose binary into {}/bin",
            root.display()
        ),
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
        Recipe::Goose => "pinned Goose binary (x86_64/aarch64) -> goose".into(),
    }
}

pub(crate) fn host_executable(name: &str) -> Result<PathBuf, Error> {
    let path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .chain([PathBuf::from("/usr/bin").join(name), PathBuf::from("/bin").join(name)])
        .find(|path| path.is_file())
        .ok_or_else(|| {
            Error::Io(format!(
                "measurement tool `{name}` is not installed on this host; install its distribution package and rerun `asb tool install {name}`"
            ))
        })?;
    Ok(path)
}

fn workload_source(
    requested: &str,
) -> Result<(String, String, PathBuf, project::ProjectConfig), Error> {
    let source = external_workload(requested)
        .map(|workload| workload.source.to_owned())
        .unwrap_or_else(|| requested.to_owned());
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
    let filename = source
        .split('?')
        .next()
        .and_then(|value| value.rsplit('/').next())
        .unwrap_or(&source);
    let default_name = Path::new(filename)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| valid_name(value))
        .ok_or(Error::Input(
            "workload source must have a simple file name".into(),
        ))?
        .to_owned();
    let name = external_workload(requested)
        .map(|workload| workload.name.to_owned())
        .unwrap_or(default_name);
    let (project_root, project_config) = project::load(Path::new("."))?;
    Ok((source, name, project_root, project_config))
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

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

pub(crate) fn install_source(source: &str, destination: &Path, kind: &str) -> Result<(), Error> {
    install_source_with_limit(source, destination, kind, None)
}

pub(crate) fn install_runtime_source(source: &str, destination: &Path) -> Result<(), Error> {
    install_source_with_limit(
        source,
        destination,
        "runtime archive",
        Some(256 * 1024 * 1024),
    )
}

fn install_source_with_limit(
    source: &str,
    destination: &Path,
    kind: &str,
    limit: Option<u64>,
) -> Result<(), Error> {
    if source.starts_with("https://") || source.starts_with("http://") {
        let mut command = Command::new("curl");
        command.args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--output",
        ]);
        command.arg(destination);
        if let Some(limit) = limit {
            command.args(["--max-time", "300", "--max-filesize", &limit.to_string()]);
        }
        let status = command
            .arg(source)
            .logged_status()
            .map_err(|error| missing_dependency("curl", kind, error))?;
        if !status.success() {
            return Err(Error::Io(format!(
                "could not download {kind} from {source}"
            )));
        }
        if let Some(limit) = limit
            && fs::metadata(destination)
                .map_err(|error| Error::Io(error.to_string()))?
                .len()
                > limit
        {
            return Err(Error::Input(format!(
                "{kind} exceeds the installation size limit"
            )));
        }
        return Ok(());
    }
    let source_path = Path::new(source);
    let metadata = fs::metadata(source_path)
        .map_err(|error| Error::Input(format!("cannot read {kind} source {source}: {error}")))?;
    if !metadata.is_file() {
        return Err(Error::Input(format!(
            "{kind} source is not a file: {source}"
        )));
    }
    if limit.is_some_and(|limit| metadata.len() > limit) {
        return Err(Error::Input(format!(
            "{kind} exceeds the installation size limit"
        )));
    }
    fs::copy(source_path, destination).map_err(|error| {
        Error::Io(format!(
            "cannot install {kind} at {}: {error}",
            destination.display()
        ))
    })?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_runtime_requirements_come_from_every_registered_agent_recipe() {
        for id in agents::names() {
            let recipe = agents::find(id).unwrap().recipe();
            let expected: &[&str] = match recipe {
                Recipe::Npm { .. } => &["node"],
                Recipe::Python { .. } => &["uv"],
                Recipe::Goose => &[],
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
        let source = root.join("source");
        let destination = root.join("destination");
        fs::write(&source, b"four").unwrap();
        fs::write(&destination, b"preserved").unwrap();
        assert!(
            install_source_with_limit(source.to_str().unwrap(), &destination, "fixture", Some(3))
                .is_err()
        );
        assert_eq!(fs::read(&destination).unwrap(), b"preserved");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn npm_install_manifest_pins_published_aws_sdk_artifacts() {
        let manifest: serde_json::Value =
            serde_json::from_str(NPM_INSTALL_MANIFEST).expect("valid npm manifest");
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
        let mut progress = |percent: u8, stage: &str| events.push((percent, stage.to_owned()));
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
                .any(|(percent, stage)| *percent == 2 && stage == "preparing")
        );
        assert!(
            events
                .iter()
                .any(|(percent, stage)| *percent == 100 && stage == "ready")
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
        let mut names = std::collections::BTreeSet::new();
        for workload in EXTERNAL_WORKLOADS {
            assert!(valid_name(workload.name));
            assert!(names.insert(workload.name));
            assert!(
                workload
                    .source
                    .starts_with("https://raw.githubusercontent.com/")
            );
            assert!(workload.source.split('/').any(|part| part.len() == 40));
        }
        assert_eq!(
            external_workload("swe-bench").map(|item| item.name),
            Some("swe-bench")
        );
        assert!(bundle_spec("swe-bench").is_none());
        assert!(bundle_spec("terminal-bench").is_some());
        assert!(external_workload("methodology-only").is_none());
        let installed = external_workload_names().collect::<std::collections::BTreeSet<_>>();
        let adapters =
            workloads::external_adapter_names().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            adapters, installed,
            "every external workload needs an adapter"
        );
    }

    #[test]
    fn complete_external_bundle_is_reused_when_already_installed() {
        let directory =
            std::env::temp_dir().join(format!("asb-external-bundle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let bundle_dir = directory.join("terminal-bench");
        fs::create_dir_all(bundle_dir.join("source")).unwrap();
        fs::write(bundle_dir.join("source/README.md"), "local source").unwrap();
        let spec = bundle_spec("terminal-bench").unwrap();
        fs::write(bundle_dir.join("ASB-BUNDLE"), spec.marker).unwrap();
        let mut output = Vec::new();
        let mut progress = |_percent: u8, _stage: &str| {};
        install_bundle(
            "terminal-bench",
            spec,
            &directory,
            &mut output,
            &mut progress,
        )
        .expect("existing bundle is reused");
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("already installed")
        );
        assert_eq!(
            fs::read_to_string(bundle_dir.join("source/README.md")).unwrap(),
            "local source"
        );
        let _ = fs::remove_dir_all(directory);
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
