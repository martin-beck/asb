//! Project-local configuration shared by every command.

use crate::Error;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "linux")]
mod host_setup;
pub(crate) mod perfetto;

thread_local! {
    static NETWORK_PROJECT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Restrict implicit network lookups to the benchmark's project for this
/// thread, including discovery and provider-owned subprocesses.
pub struct NetworkProjectGuard(Option<PathBuf>, std::marker::PhantomData<std::rc::Rc<()>>);

impl Drop for NetworkProjectGuard {
    fn drop(&mut self) {
        NETWORK_PROJECT.with(|project| *project.borrow_mut() = self.0.take());
    }
}

pub fn network_project(root: &Path) -> NetworkProjectGuard {
    NetworkProjectGuard(
        NETWORK_PROJECT.with(|project| project.borrow_mut().replace(root.to_path_buf())),
        std::marker::PhantomData,
    )
}

const CONFIG_DIR: &str = ".asb";
const CONFIG_FILE: &str = "project.toml";

#[cfg(target_os = "linux")]
const HOST_SETTINGS: &[(&str, &str, &str)] = &[
    (
        "kernel.perf_event_paranoid",
        "-1",
        "perf process-tree counters",
    ),
    (
        "kernel.sched_schedstats",
        "1",
        "bpftrace io-wait scheduler statistics",
    ),
];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectConfig {
    pub tools_dir: String,
    pub workloads_dir: String,
    pub workspace_dir: String,
    /// Optional project-owned delegated cgroup parent for process accounting.
    /// This is populated by `asb init` only when the host permits safe setup.
    #[serde(default)]
    pub cgroup_parent: Option<String>,
    /// Explicitly delegated tracefs instances for concurrent Perfetto capture.
    #[serde(default)]
    pub perfetto_instances: Vec<String>,
    #[serde(default)]
    pub proxy: NetworkProxy,
    /// Project-local installer execution budgets; frozen for each operation.
    #[serde(default)]
    pub installation: InstallationConfig,
}

mod installation;
pub use installation::InstallationConfig;

/// Network proxies are separate from ASB's record/replay proxy adapters.
/// Credentials must stay in the environment, not in project files.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct NetworkProxy {
    pub http: Option<String>,
    pub https: Option<String>,
    pub socks: Option<String>,
    pub no_proxy: Option<String>,
}

impl NetworkProxy {
    fn detect(get: &impl Fn(&str) -> Option<String>) -> (Self, bool) {
        let mut omitted = false;
        let mut save = |names: &[&str]| {
            let value = names.iter().find_map(|name| get(name));
            value.and_then(|value| {
                if value.is_empty() {
                    None
                } else if value.contains(['@', '?', '#', '\n', '\r']) {
                    omitted = true;
                    None
                } else {
                    Some(value)
                }
            })
        };
        // curl ignores HTTP_PROXY in CGI environments to prevent HTTPoxy.
        let http_names: &[&str] = if get("REQUEST_METHOD").is_some() {
            &["http_proxy"]
        } else {
            &["http_proxy", "HTTP_PROXY"]
        };
        let config = Self {
            http: save(http_names),
            https: save(&["https_proxy", "HTTPS_PROXY"]),
            socks: save(&["all_proxy", "ALL_PROXY", "socks_proxy", "SOCKS_PROXY"]),
            no_proxy: save(&["no_proxy", "NO_PROXY"]),
        };
        (config, omitted)
    }

    pub(crate) fn apply(&self, command: &mut Command, get: &impl Fn(&str) -> Option<String>) {
        let select = |names: &[&str], configured: &Option<String>| {
            names
                .iter()
                .find_map(|name| get(name))
                .or_else(|| configured.clone())
        };
        let http_names: &[&str] = if get("REQUEST_METHOD").is_some() {
            &["http_proxy"]
        } else {
            &["http_proxy", "HTTP_PROXY"]
        };
        let http = select(http_names, &self.http);
        if get("REQUEST_METHOD").is_some() {
            command.env_remove("HTTP_PROXY");
        }
        let https = select(&["https_proxy", "HTTPS_PROXY"], &self.https);
        let socks = select(
            &["all_proxy", "ALL_PROXY", "socks_proxy", "SOCKS_PROXY"],
            &self.socks,
        );
        let no_proxy = select(&["no_proxy", "NO_PROXY"], &self.no_proxy);
        for (lower, upper, value) in [
            ("http_proxy", "HTTP_PROXY", &http),
            ("https_proxy", "HTTPS_PROXY", &https),
            ("all_proxy", "ALL_PROXY", &socks),
        ] {
            if let Some(value) = value {
                command.env(lower, value);
                if upper != "HTTP_PROXY" || get("REQUEST_METHOD").is_none() {
                    command.env(upper, value);
                } else {
                    command.env_remove(upper);
                }
            }
        }
        // Recent Node versions require an explicit opt-in for their standard
        // HTTP(S) proxy variables. Preserve an explicit user choice and do not
        // claim this enables Node's unsupported SOCKS/all-protocol fallback.
        if get("NODE_USE_ENV_PROXY").is_none()
            && [&http, &https].iter().any(|value| {
                value.as_ref().is_some_and(|proxy| {
                    proxy.starts_with("http://") || proxy.starts_with("https://")
                })
            })
        {
            command.env("NODE_USE_ENV_PROXY", "1");
        }
        // Local agents, replay and provider bridges must not be sent to a
        // corporate proxy, even if the existing bypass list is incomplete.
        if http.is_some() || https.is_some() || socks.is_some() || no_proxy.is_some() {
            let mut bypass = no_proxy.unwrap_or_default();
            for host in ["localhost", "127.0.0.1", "::1"] {
                if !bypass
                    .split(',')
                    .any(|item| item.trim().eq_ignore_ascii_case(host))
                {
                    if !bypass.is_empty() {
                        bypass.push(',');
                    }
                    bypass.push_str(host);
                }
            }
            command.env("no_proxy", &bypass).env("NO_PROXY", &bypass);
        }
    }
}

/// Apply the invoking project's settings to one child, without modifying the
/// process environment or other concurrently running projects.
pub fn apply_network_proxy(command: &mut Command, start: &Path) -> Result<(), Error> {
    let scoped = NETWORK_PROJECT.with(|project| project.borrow().clone());
    let (_, config) = load(scoped.as_deref().unwrap_or(start))?;
    config
        .proxy
        .apply(command, &|name| std::env::var(name).ok());
    Ok(())
}

pub(crate) fn apply_python_path(
    command: &mut Command,
    root: &Path,
    program: &Path,
) -> Result<(), Error> {
    crate::python_runtime::add_to_path(command, root, program)
}

/// Let ASB-owned Node sidecars terminate their outbound subprocesses before
/// forcing termination, without signalling unrelated process groups.
pub fn stop_network_child(child: &mut std::process::Child) -> std::io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    let signalled = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if signalled {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    let _ = child.kill();
    child.wait().map(|_| ())
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            tools_dir: ".asb-tools".into(),
            workloads_dir: "workloads".into(),
            workspace_dir: "asb-workspace".into(),
            cgroup_parent: None,
            perfetto_instances: Vec::new(),
            proxy: NetworkProxy::default(),
            installation: InstallationConfig::default(),
        }
    }
}

pub fn init(
    root: Option<PathBuf>,
    reset: bool,
    yes: bool,
    output: &mut dyn std::io::Write,
) -> Result<(), Error> {
    let root = root.unwrap_or_else(|| PathBuf::from("."));
    if is_asb_source_checkout(&root) {
        return Err(Error::Usage(
            "refusing to initialize the ASB source checkout; choose a separate benchmark project directory",
        ));
    }
    let path = root.join(CONFIG_DIR).join(CONFIG_FILE);
    if path.is_file() {
        if !reset {
            writeln!(
                output,
                "Warning: this project is already initialized at {}.\nNo files were changed. Rerun `asb init --reset` to remove managed state and initialize fresh.",
                path.display()
            )
            .map_err(super::io_error)?;
            return Err(Error::Usage(
                "project already initialized; use `asb init --reset` for a fresh initialization",
            ));
        }
        reset_project(&root, &path)?;
    }
    fs::create_dir_all(path.parent().expect("config parent"))
        .map_err(|error| Error::Io(format!("cannot create project configuration: {error}")))?;
    let (proxy, omitted_credentials) = NetworkProxy::detect(&|name| std::env::var(name).ok());
    let cgroup_parent = configure_host_capabilities(output, yes)?;
    let perfetto_instances = perfetto::configure(&root, output, yes)?;
    let config = ProjectConfig {
        proxy,
        cgroup_parent,
        perfetto_instances,
        ..ProjectConfig::default()
    };
    fs::create_dir_all(resolve(&root, &config.tools_dir))
        .map_err(|error| Error::Io(format!("cannot create tools directory: {error}")))?;
    fs::create_dir_all(resolve(&root, &config.workloads_dir))
        .map_err(|error| Error::Io(format!("cannot create workloads directory: {error}")))?;
    let bundled = crate::workloads::populate(&resolve(&root, &config.workloads_dir))
        .map_err(|error| Error::Io(format!("cannot install bundled workloads: {error}")))?;
    fs::create_dir_all(resolve(&root, &config.workspace_dir))
        .map_err(|error| Error::Io(format!("cannot create workspace directory: {error}")))?;
    let fixtures = crate::workloads::populate_fixtures(&root.join(CONFIG_DIR).join("fixtures"))
        .map_err(|error| Error::Io(format!("cannot install workload fixtures: {error}")))?;
    let text = toml::to_string_pretty(&config).map_err(|error| Error::Input(error.to_string()))?;
    fs::write(&path, text)
        .map_err(|error| Error::Io(format!("cannot write {}: {error}", path.display())))?;
    let installation = crate::install::OperationContext::new(&root, &config)
        .map_err(crate::install::InstallFailure::into_public)?;
    let python = crate::python_runtime::ensure_in(&installation, output);
    let diagnostics = installation.report_diagnostics(output);
    match python {
        Ok(_) => {}
        Err(crate::install::InstallFailure::Ordinary(error)) => {
            writeln!(output, "Warning: project Python is not ready: {error}\nPython-based tool installation will retry setup; other ASB commands remain available.")
                .map_err(super::io_error)?;
        }
        Err(crate::install::InstallFailure::Abort(abort)) => {
            return Err(crate::install::InstallFailure::Abort(abort).into_public());
        }
    }
    diagnostics?;
    writeln!(
        output,
        "Initialized ASB project at {}\nBundled workloads available: {bundled}\nFixture files available: {fixtures}",
        path.display()
    )
    .map_err(super::io_error)?;
    let detected = [
        ("HTTP", config.proxy.http.is_some()),
        ("HTTPS", config.proxy.https.is_some()),
        ("SOCKS/all-protocol", config.proxy.socks.is_some()),
    ]
    .into_iter()
    .filter_map(|(name, present)| present.then_some(name))
    .collect::<Vec<_>>();
    if detected.is_empty() {
        writeln!(output, "No credential-free network proxies detected. Configure [proxy] in .asb/project.toml if needed.").map_err(super::io_error)?;
    } else {
        writeln!(
            output,
            "Saved detected network proxy settings: {}.",
            detected.join(", ")
        )
        .map_err(super::io_error)?;
    }
    if omitted_credentials {
        writeln!(output, "Proxy settings containing credentials or query data were not saved. Keep the corresponding proxy environment variables set when running ASB.").map_err(super::io_error)?;
    }
    #[cfg(target_os = "linux")]
    writeln!(output, "For complete CPU accounting across short-lived child processes, use writable cgroup-v2 delegation. On systemd Linux, run:\n  systemd-run --user --scope -p Delegate=yes -- asb run CONFIG.toml\nFor a sweep, replace `asb run CONFIG.toml` with `asb sweep SWEEP.toml`. Other delegated setups can set ASB_CGROUP_PARENT to a writable cgroup-v2 directory. Without delegation, ASB uses sampled process accounting.").map_err(super::io_error)?;
    Ok(())
}

/// Recheck optional host capabilities without resetting an existing project.
pub fn configure_host(
    root: Option<PathBuf>,
    yes: bool,
    output: &mut dyn std::io::Write,
) -> Result<(), Error> {
    let start = root.unwrap_or_else(|| PathBuf::from("."));
    let (root, mut config) = load(&start)?;
    let path = initialized_config_path(&root)?;
    config.cgroup_parent = configure_host_capabilities(output, yes)?.or(config.cgroup_parent);
    let instances = perfetto::configure(&root, output, yes)?;
    if !instances.is_empty() {
        config.perfetto_instances = instances;
    }
    let text = toml::to_string_pretty(&config).map_err(|error| Error::Input(error.to_string()))?;
    fs::write(&path, text)
        .map_err(|error| Error::Io(format!("cannot update project capabilities: {error}")))?;
    writeln!(output, "Updated host capability configuration; existing tools, workloads, configurations and results were preserved.").map_err(super::io_error)
}

// Kept separate so missing-project tests never need to discover or configure
// an unrelated real ancestor project. Production uses the exact same guard.
fn initialized_config_path(root: &Path) -> Result<PathBuf, Error> {
    let path = root.join(CONFIG_DIR).join(CONFIG_FILE);
    if !path.is_file() {
        return Err(Error::Input(
            "no initialized project found; run `asb init` first".into(),
        ));
    }
    Ok(path)
}

/// Apply optional host prerequisites through one consent/capability policy.
fn configure_host_capabilities(
    output: &mut dyn std::io::Write,
    yes: bool,
) -> Result<Option<String>, Error> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (output, yes);
        Ok(None)
    }
    #[cfg(target_os = "linux")]
    {
        let mut reader = std::io::stdin().lock();
        let mut executor = host_setup::SystemExecutor;
        let mut policy = host_setup::Policy::new(&mut reader, &mut executor);
        configure_host_capabilities_with(output, yes, &mut policy, |setting| {
            let proc_name = setting.strip_prefix("kernel.").unwrap_or(setting);
            fs::read_to_string(format!("/proc/sys/kernel/{proc_name}")).ok()
        })
    }
}

#[cfg(target_os = "linux")]
fn configure_host_capabilities_with(
    output: &mut dyn std::io::Write,
    yes: bool,
    policy: &mut host_setup::Policy<'_>,
    current: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, Error> {
    use host_setup::{FailurePolicy, Operation, Request, Requirement};
    let mut pending = Vec::new();
    for &(setting, wanted, purpose) in HOST_SETTINGS {
        if current(setting).is_some_and(|value| value.trim() == wanted) {
            writeln!(
                output,
                "Host capability setup: {setting}={wanted} already enabled ({purpose})."
            )
            .map_err(super::io_error)?;
        } else {
            pending.push((setting, wanted, purpose));
        }
    }
    if !pending.is_empty() {
        let request = Request {
            heading: "Host capability setup needs to change these system-wide kernel tracing settings:",
            explanation: "These changes affect the whole host. Enter y to approve; n or Enter leaves them unchanged.",
            operations: pending
                .iter()
                .map(|(setting, wanted, _)| {
                    Operation::command(&["sysctl", "-w", &format!("{setting}={wanted}")], true)
                })
                .collect(),
            requirement: Requirement::AllowInteractive,
            failure: FailurePolicy::Continue,
        };
        if let host_setup::Decision::Approved(approved) = policy.approve(output, yes, request)? {
            for ((setting, wanted, purpose), success) in
                pending.iter().zip(policy.execute(approved))
            {
                if success {
                    writeln!(
                        output,
                        "Host capability setup: changed {setting}={wanted} ({purpose})."
                    )
                    .map_err(super::io_error)?;
                } else {
                    writeln!(output, "Host capability setup: could not change {setting}={wanted}; continuing without changing that setting ({purpose}).")
                        .map_err(super::io_error)?;
                }
            }
        } else {
            writeln!(
                output,
                "Host capability setup: system-wide tracing settings unchanged by user request."
            )
            .map_err(super::io_error)?;
        }
    }
    // Preserve the prior group policy: interactive approval for sysctls never
    // escalates cgroup setup. It requires the original passwordless capability
    // and a separate consent, even if interactive sudo refreshed credentials.
    if policy.passwordless(output)? {
        configure_cgroup_parent_with(output, yes, policy)
    } else {
        writeln!(output, "Host capability setup: passwordless sudo is unavailable; no delegated project cgroup was created.")
            .map_err(super::io_error)?;
        Ok(None)
    }
}

#[cfg(target_os = "linux")]
fn request_sudo_consent(output: &mut dyn std::io::Write, yes: bool) -> Result<bool, Error> {
    host_setup::consent(&mut std::io::stdin().lock(), output, yes)
}

#[cfg(all(test, target_os = "linux"))]
fn parse_host_settings_confirmation(answer: &str) -> Option<bool> {
    host_setup::parse_confirmation(answer)
}

#[cfg(all(test, target_os = "linux"))]
fn configure_cgroup_parent(
    output: &mut dyn std::io::Write,
    yes: bool,
) -> Result<Option<String>, Error> {
    let mut reader = std::io::stdin().lock();
    let mut executor = host_setup::SystemExecutor;
    let mut policy = host_setup::Policy::new(&mut reader, &mut executor);
    configure_cgroup_parent_with(output, yes, &mut policy)
}

#[cfg(target_os = "linux")]
fn configure_cgroup_parent_with(
    output: &mut dyn std::io::Write,
    yes: bool,
    policy: &mut host_setup::Policy<'_>,
) -> Result<Option<String>, Error> {
    use std::os::unix::fs::MetadataExt;

    let relative = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("0::"))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| PathBuf::from(value.trim_start_matches('/')))
        });
    let Some(relative) = relative else {
        writeln!(output, "Host capability setup: unified cgroup-v2 membership was not found; no delegated project cgroup was created.")
            .map_err(super::io_error)?;
        return Ok(None);
    };
    let Some(session_name) = relative.file_name().and_then(|name| name.to_str()) else {
        writeln!(output, "Host capability setup: the current cgroup layout is not systemd-delegatable; no delegated project cgroup was created.")
            .map_err(super::io_error)?;
        return Ok(None);
    };
    if !session_name.ends_with(".scope") {
        writeln!(output, "Host capability setup: the current cgroup is not a systemd session scope; no delegated project cgroup was created.")
            .map_err(super::io_error)?;
        return Ok(None);
    }
    let parent_relative = relative.parent().map(Path::to_path_buf).unwrap_or_default();
    if parent_relative.as_os_str().is_empty()
        || parent_relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        writeln!(output, "Host capability setup: the current cgroup path is unsafe to delegate; no project cgroup was created.")
            .map_err(super::io_error)?;
        return Ok(None);
    }
    let parent = Path::new("/sys/fs/cgroup").join(parent_relative);
    let project_parent = parent.join("asb.slice");
    let uid = fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .map_err(|error| {
            Error::Io(format!(
                "cannot determine current user id for cgroup setup: {error}"
            ))
        })?;
    let gid = fs::metadata("/proc/self")
        .map(|metadata| metadata.gid())
        .map_err(|error| {
            Error::Io(format!(
                "cannot determine current group id for cgroup setup: {error}"
            ))
        })?;
    let owner = format!("{uid}:{gid}");
    let path = project_parent.to_string_lossy().into_owned();
    let controller_file = parent.join("cgroup.subtree_control");
    let enable_cpu = controller_file.is_file()
        && !read_trimmed_path(&controller_file)
            .is_some_and(|value| value.split_whitespace().any(|item| item == "cpu"));
    apply_cgroup_parent(
        output,
        yes,
        policy,
        &path,
        &owner,
        &controller_file.to_string_lossy(),
        enable_cpu,
    )
}

#[cfg(target_os = "linux")]
fn apply_cgroup_parent(
    output: &mut dyn std::io::Write,
    yes: bool,
    policy: &mut host_setup::Policy<'_>,
    path: &str,
    owner: &str,
    controller_path: &str,
    enable_cpu: bool,
) -> Result<Option<String>, Error> {
    use host_setup::{FailurePolicy, Operation, Request, Requirement};
    let mut operations = vec![
        Operation::command(&["mkdir", "-p", path], false),
        Operation::command(&["chown", owner, path], false),
        Operation::command(&["chmod", "0750", path], false),
    ];
    if enable_cpu {
        operations.push(Operation::write(controller_path, b"+cpu\n"));
    }
    let request = Request {
        heading: "Host capability setup may run these privileged commands:",
        explanation: "These commands create a user-owned delegated cgroup and enable CPU accounting; they do not change resource limits or sudoers.",
        operations,
        requirement: Requirement::Passwordless,
        failure: FailurePolicy::Stop,
    };
    let decision = policy.approve(output, yes, request)?;
    let host_setup::Decision::Approved(approved) = decision else {
        writeln!(
            output,
            "Host capability setup: delegated cgroup commands were not run; sampled process accounting remains available."
        )
        .map_err(super::io_error)?;
        return Ok(None);
    };
    let results = policy.execute(approved);
    let created = results.len() >= 3 && results[..3].iter().all(|success| *success);
    if !created {
        writeln!(output, "Host capability setup: passwordless sudo could not create or delegate {path}; sampled process accounting remains available.")
            .map_err(super::io_error)?;
        return Ok(None);
    }
    if enable_cpu && results.get(3) != Some(&true) {
        writeln!(output, "Host capability setup: created {path}, but could not enable the CPU controller; sampled process accounting remains available.")
            .map_err(super::io_error)?;
        return Ok(None);
    }
    writeln!(output, "Host capability setup: created writable delegated cgroup parent {path} for process-tree CPU accounting.")
        .map_err(super::io_error)?;
    Ok(Some(path.into()))
}

#[cfg(target_os = "linux")]
fn read_trimmed_path(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
}

#[cfg(all(test, target_os = "linux"))]
fn passwordless_sudo_available() -> bool {
    use host_setup::Executor;
    host_setup::SystemExecutor.passwordless()
}
#[cfg(all(test, target_os = "linux"))]
fn run_sysctl(assignment: &str, passwordless: bool) -> bool {
    use host_setup::{Executor, Operation, SudoMode};
    host_setup::SystemExecutor.execute(
        if passwordless {
            SudoMode::Passwordless
        } else {
            SudoMode::Interactive
        },
        &Operation::command(&["sysctl", "-w", assignment], true),
    )
}
#[cfg(all(test, target_os = "linux"))]
fn run_sudo_status(args: &[&str]) -> bool {
    use host_setup::{Executor, Operation, SudoMode};
    host_setup::SystemExecutor.execute(SudoMode::Passwordless, &Operation::command(args, false))
}
#[cfg(all(test, target_os = "linux"))]
fn run_sudo_write(path: &str, contents: &[u8]) -> bool {
    use host_setup::{Executor, Operation, SudoMode};
    host_setup::SystemExecutor.execute(SudoMode::Passwordless, &Operation::write(path, contents))
}

fn reset_project(root: &Path, config_path: &Path) -> Result<(), Error> {
    let state = root.join(CONFIG_DIR);
    reject_symlink(&state)?;
    reject_symlink(config_path)?;
    // Stable admission coordination excludes new slots throughout deletion;
    // existing instance leases additionally exclude active/crash-cleanup services.
    let _tracing_leases = perfetto::lock_for_reset(root)?;
    let existing = fs::read_to_string(config_path)
        .map_err(|error| Error::Io(format!("cannot read {}: {error}", config_path.display())))?;
    let config: ProjectConfig = toml::from_str(&existing).map_err(|error| {
        Error::Input(format!(
            "cannot parse {} before reset: {error}",
            config_path.display()
        ))
    })?;
    // Only direct children can be removed safely with std::fs::remove_dir_all:
    // it does not follow the final symlink, but does follow intermediate ones.
    // Validate everything before changing anything, including the default paths
    // that initialization will recreate after the reset.
    let defaults = ProjectConfig::default();
    for directory in [
        &config.tools_dir,
        &config.workloads_dir,
        &config.workspace_dir,
        &defaults.tools_dir,
        &defaults.workloads_dir,
        &defaults.workspace_dir,
    ] {
        if !is_safe_project_relative_path(directory) {
            return Err(Error::Input(format!(
                "cannot reset configured directory {directory:?}: reset requires a direct child of the project; move its contents manually and update .asb/project.toml before retrying"
            )));
        }
        reject_symlink(&root.join(directory))?;
    }
    if state.exists() {
        fs::remove_dir_all(&state).map_err(|error| {
            Error::Io(format!("cannot remove {}: {error}", config_path.display()))
        })?;
    }
    for directory in [
        &config.tools_dir,
        &config.workloads_dir,
        &config.workspace_dir,
    ] {
        let path = root.join(directory);
        if path.is_dir() {
            fs::remove_dir_all(&path)
                .map_err(|error| Error::Io(format!("cannot remove {}: {error}", path.display())))?;
        }
    }
    Ok(())
}

fn is_safe_project_relative_path(value: &str) -> bool {
    let mut components = Path::new(value).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

fn reject_symlink(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::Input(format!(
            "cannot reset: {} is a symbolic link; move or remove the link before retrying",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(format!(
            "cannot inspect {}: {error}",
            path.display()
        ))),
    }
}

fn is_asb_source_checkout(root: &Path) -> bool {
    root.join("Cargo.toml").is_file()
        && root.join("crates/asb-cli/Cargo.toml").is_file()
        && root.join("workloads").is_dir()
}

pub fn load(start: &Path) -> Result<(PathBuf, ProjectConfig), Error> {
    // Relative paths such as `.` cannot be walked into their parent with
    // PathBuf::pop; anchor them first so subdirectories find the project.
    let start = if start.is_absolute() {
        start.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| Error::Io(format!("cannot locate current directory: {error}")))?
            .join(start)
    };
    let mut directory = if start.is_dir() {
        start
    } else {
        start
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    };
    loop {
        let path = directory.join(CONFIG_DIR).join(CONFIG_FILE);
        if path.is_file() {
            let text = fs::read_to_string(&path).map_err(|error| {
                Error::Input(format!("cannot read {}: {error}", path.display()))
            })?;
            let config: ProjectConfig = toml::from_str(&text).map_err(|error| {
                Error::Input(format!("cannot parse {}: {error}", path.display()))
            })?;
            config
                .installation
                .validate()
                .map_err(|error| Error::Input(format!("invalid {}: {error}", path.display())))?;
            return Ok((directory, config));
        }
        if !directory.pop() {
            break;
        }
    }
    Ok((PathBuf::from("."), ProjectConfig::default()))
}

pub fn resolve(root: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

#[cfg(test)]
#[path = "../tests/unit/project_capabilities.rs"]
mod coverage_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(values: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn proxy_detection_respects_precedence_and_does_not_persist_credentials() {
        let (proxy, omitted) = NetworkProxy::detect(&vars(&[
            ("http_proxy", "http://preferred:8080"),
            ("HTTP_PROXY", "http://ignored:8080"),
            ("HTTPS_PROXY", "http://user:secret@proxy:8080"),
            ("SOCKS_PROXY", "socks5h://socks:1080"),
            ("NO_PROXY", "internal.example"),
        ]));
        assert!(omitted);
        assert_eq!(proxy.http.as_deref(), Some("http://preferred:8080"));
        assert_eq!(proxy.https, None);
        assert_eq!(proxy.socks.as_deref(), Some("socks5h://socks:1080"));
        let encoded = toml::to_string(&proxy).unwrap();
        assert!(!encoded.contains("secret"));
        assert_eq!(proxy.no_proxy.as_deref(), Some("internal.example"));
    }

    #[test]
    fn proxy_apply_respects_environment_empty_values_and_loopback() {
        let config = NetworkProxy {
            http: Some("http://configured:8080".into()),
            https: Some("http://configured:8081".into()),
            socks: Some("socks5h://configured:1080".into()),
            no_proxy: Some("internal.example".into()),
        };
        let mut command = Command::new("unused");
        config.apply(
            &mut command,
            &vars(&[("http_proxy", ""), ("HTTPS_PROXY", "http://override:8000")]),
        );
        let env: std::collections::HashMap<_, _> = command.get_envs().collect();
        assert_eq!(env[std::ffi::OsStr::new("http_proxy")].unwrap(), "");
        assert_eq!(
            env[std::ffi::OsStr::new("https_proxy")].unwrap(),
            "http://override:8000"
        );
        assert_eq!(
            env[std::ffi::OsStr::new("all_proxy")].unwrap(),
            "socks5h://configured:1080"
        );
        assert_eq!(
            env[std::ffi::OsStr::new("NODE_USE_ENV_PROXY")].unwrap(),
            "1"
        );
        let bypass = env[std::ffi::OsStr::new("no_proxy")]
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(bypass, "internal.example,localhost,127.0.0.1,::1");
    }

    #[test]
    fn cgi_http_proxy_is_ignored_and_removed() {
        let env = [
            ("REQUEST_METHOD", "GET"),
            ("HTTP_PROXY", "http://attacker:80"),
        ];
        let (config, _) = NetworkProxy::detect(&vars(&env));
        assert!(config.http.is_none());
        let mut command = Command::new("unused");
        config.apply(&mut command, &vars(&env));
        assert!(
            command
                .get_envs()
                .any(|(name, value)| name == "HTTP_PROXY" && value.is_none())
        );
    }

    #[test]
    fn node_proxy_opt_in_respects_explicit_choice_and_does_not_claim_socks_support() {
        let http = NetworkProxy {
            http: Some("http://proxy:8080".into()),
            ..NetworkProxy::default()
        };
        let mut command = Command::new("unused");
        http.apply(&mut command, &vars(&[("NODE_USE_ENV_PROXY", "0")]));
        assert!(
            !command
                .get_envs()
                .any(|(name, _)| name == "NODE_USE_ENV_PROXY")
        );
        let socks = NetworkProxy {
            socks: Some("socks5h://proxy:1080".into()),
            ..NetworkProxy::default()
        };
        let mut command = Command::new("unused");
        socks.apply(&mut command, &vars(&[]));
        assert!(
            !command
                .get_envs()
                .any(|(name, _)| name == "NODE_USE_ENV_PROXY")
        );
    }

    #[test]
    fn network_project_scope_is_nested_and_thread_local() {
        let first = Path::new("/project-one");
        let second = Path::new("/project-two");
        let initial = NETWORK_PROJECT.with(|project| project.borrow().clone());
        {
            let _first = network_project(first);
            assert_eq!(
                NETWORK_PROJECT.with(|project| project.borrow().clone()),
                Some(first.to_path_buf())
            );
            {
                let _second = network_project(second);
                assert_eq!(
                    NETWORK_PROJECT.with(|project| project.borrow().clone()),
                    Some(second.to_path_buf())
                );
                assert!(
                    std::thread::spawn(
                        || NETWORK_PROJECT.with(|project| project.borrow().is_none())
                    )
                    .join()
                    .unwrap()
                );
            }
            assert_eq!(
                NETWORK_PROJECT.with(|project| project.borrow().clone()),
                Some(first.to_path_buf())
            );
        }
        assert_eq!(
            NETWORK_PROJECT.with(|project| project.borrow().clone()),
            initial
        );
    }

    #[test]
    fn old_project_configuration_loads_without_proxy_section() {
        let old = "tools_dir = '.asb-tools'\nworkloads_dir = 'workloads'\nworkspace_dir = 'asb-workspace'\n";
        let config: ProjectConfig = toml::from_str(old).unwrap();
        assert_eq!(config.proxy, NetworkProxy::default());
        assert_eq!(config.cgroup_parent, None);
    }

    #[test]
    fn init_writes_config_and_directories() {
        let root = std::env::temp_dir().join(format!("asb-project-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut output = Vec::new();
        init(Some(root.clone()), false, false, &mut output).expect("init succeeds");
        #[cfg(target_os = "linux")]
        assert!(
            String::from_utf8_lossy(&output)
                .contains("systemd-run --user --scope -p Delegate=yes -- asb run CONFIG.toml")
        );
        let (loaded_root, config) = load(&root).expect("config loads");
        assert_eq!(loaded_root, root);
        assert!(root.join(".asb/project.toml").is_file());
        assert!(resolve(&root, &config.tools_dir).is_dir());
        assert!(resolve(&root, &config.workloads_dir).is_dir());
        assert!(resolve(&root, &config.workspace_dir).is_dir());
        assert!(root.join("workloads/bug-fix.txt").is_file());
        assert!(root.join(".asb/fixtures/bug-fix/src/parser.py").is_file());
        assert!(
            root.join(".asb/fixtures/test-generation/tests/test_temperature.py")
                .is_file()
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn host_settings_confirmation_accepts_only_single_letter_answers_or_default() {
        assert_eq!(parse_host_settings_confirmation("y\n"), Some(true));
        assert_eq!(parse_host_settings_confirmation("N\n"), Some(false));
        assert_eq!(parse_host_settings_confirmation("\n"), Some(false));
        assert_eq!(parse_host_settings_confirmation("yes\n"), None);
    }

    #[test]
    fn init_rejects_the_asb_source_checkout_layout() {
        let root = std::env::temp_dir().join(format!("asb-source-checkout-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("crates/asb-cli")).unwrap();
        fs::create_dir_all(root.join("workloads")).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(root.join("crates/asb-cli/Cargo.toml"), "[package]\n").unwrap();
        let error = init(Some(root.clone()), false, false, &mut Vec::new())
            .expect_err("source checkout must be rejected");
        assert!(error.to_string().contains("source checkout"));
        assert!(!root.join(".asb").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn init_warns_without_reset_and_preserves_existing_state() {
        let root = std::env::temp_dir().join(format!("asb-init-warning-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut output = Vec::new();
        init(Some(root.clone()), false, false, &mut output).expect("initial init succeeds");
        fs::write(root.join("workloads/custom.txt"), "keep").unwrap();
        let error = init(Some(root.clone()), false, false, &mut output)
            .expect_err("second init requires reset");
        assert!(error.to_string().contains("already initialized"));
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("Warning:"));
        assert!(text.contains("asb init --reset"));
        assert_eq!(
            fs::read_to_string(root.join("workloads/custom.txt")).unwrap(),
            "keep"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn init_reset_removes_managed_state_before_recreating_it() {
        let root = std::env::temp_dir().join(format!("asb-init-reset-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        init(Some(root.clone()), false, false, &mut Vec::new()).expect("initial init succeeds");
        fs::write(root.join("workloads/custom.txt"), "remove").unwrap();
        let mut output = Vec::new();
        init(Some(root.clone()), true, false, &mut output).expect("reset succeeds");
        assert!(!root.join("workloads/custom.txt").exists());
        assert!(root.join("workloads/bug-fix.txt").is_file());
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Bundled workloads available")
        );
        let _ = fs::remove_dir_all(root);
    }
}
