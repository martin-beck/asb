//! Workload/provider-neutral agent driver for isolated execution environments.
//!
//! Uses the same adapter registry as native runs. The negotiated connection
//! arrives through a private environment value, never argv or a request file.
use crate::{Error, benchmark::BenchmarkConfig, endpoint::Connection, project};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

const UNPRIVILEGED_UID: u32 = 65_534;
const UNPRIVILEGED_GID: u32 = 65_534;

const REQUEST: &str = "ASB_GUEST_AGENT_REQUEST";
const GATE_RELEASE: &str = "ASB_GATE_RELEASE";
/// Guest artifact bytes are copied by the owning workload controller rather
/// than embedded in its bounded JSON control message.
const MAX_EXPORTED_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;
// Four adapter-owned sidecars plus the three fixed replay sidecars.  The
// aggregate byte budget remains unchanged.
const MAX_EXPORTED_ARTIFACTS: usize = 8;
/// Reserved receipt kinds used to keep replay sidecars out of the bounded
/// workload-controller JSON record.  They are restored only by the owning
/// controller before the recorder sees the completed guest result.
pub(crate) const LOCAL_TOOL_TRANSCRIPT_ARTIFACT: &str = "asb-local-tools";
pub(crate) const DETERMINISTIC_MANIFEST_ARTIFACT: &str = "asb-determinism-manifest";
pub(crate) const NATIVE_TRACE_ARTIFACT: &str = "asb-native-trace";
pub(crate) const NATIVE_TOOL_SIDECAR_ARTIFACT: &str = "asb-native-tool-sidecar";
/// Opaque adapter-owned workspace effects, transferred through guest receipts.
pub(crate) const NATIVE_WORKSPACE_CAPSULE_ARTIFACT: &str = "asb-native-workspace-capsule";
/// Generic, receipt-bound frozen adapter state.  It is restored only after
/// the provider tape has settled, then interpreted by the owning adapter.
pub(crate) const DEFERRED_COLLECTION_ARTIFACT: &str = "asb-deferred-adapter-collection";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuestArtifactReceipt {
    pub kind: String,
    pub filename: String,
    pub bytes: u64,
    pub sha256: String,
}

#[cfg(test)]
#[path = "../../tests/unit/driver_validation.rs"]
mod validation_tests;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkloadOwner {
    pub controller_namespace: u64,
    pub guest_namespace: u64,
    pub token: String,
}

pub(crate) fn namespace_id() -> Result<u64, Error> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self/ns/pid")
            .map(|metadata| metadata.ino())
            .map_err(crate::io_error)
    }
    #[cfg(not(target_os = "linux"))]
    Err(Error::Input(
        "workload process ownership requires Linux PID namespaces".into(),
    ))
}

/// Consume the Docker execution gate capability at the guest boundary.
///
/// The outer shell has already waited for this private marker before it execs
/// the guest driver.  The marker remains as teardown evidence, but nested
/// agent gates must not inherit the outer environment capability.
#[allow(unsafe_code)]
fn consume_gate_release() -> Result<bool, Error> {
    let Some(value) = std::env::var_os(GATE_RELEASE) else {
        return Ok(false);
    };
    // The host supplies this one-shot path through the Docker API. Reject
    // anything but its private /tmp capability shape before waiting on it.
    let path = PathBuf::from(value);
    if !valid_gate_release_path(&path) {
        return Err(Error::Run("invalid private guest gate release path".into()));
    }
    loop {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                unsafe { std::env::remove_var(GATE_RELEASE) }
                return Ok(true);
            }
            Ok(_) => {
                return Err(Error::Run(
                    "guest gate release is not a regular file".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => {
                return Err(Error::Run(format!(
                    "cannot inspect guest gate release: {error}"
                )));
            }
        }
    }
}

/// Publish the direct application principal before consuming its Docker release.
///
/// The workload controller creates the release marker only after it has read
/// this frame and attached the requested collectors.  Keeping this ordering in
/// one helper prevents an application driver from waiting for a marker whose
/// producer is itself waiting for the driver's readiness frame.
fn announce_application_gate_then_consume(
    announce: bool,
    output: &mut dyn Write,
) -> Result<bool, Error> {
    if announce {
        writeln!(output, "__ASB_GATE__:{}", std::process::id()).map_err(crate::io_error)?;
        output.flush().map_err(crate::io_error)?;
    }
    consume_gate_release()
}

fn valid_gate_release_path(path: &Path) -> bool {
    path.parent() == Some(Path::new("/tmp"))
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.len() == ".asb-gate-release-".len() + 32
                    && name.starts_with(".asb-gate-release-")
                    && name[".asb-gate-release-".len()..]
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuestRequest {
    #[serde(default)]
    pub owned_instrumentation: Option<crate::workloads::staging::Ownership>,
    #[serde(default)]
    pub task_identity: Option<crate::workloads::session::TaskIdentity>,
    /// Workload-owned persistent shells keep their actual cwd, not a guessed
    /// container default. The benchmark/project identities remain separate.
    #[serde(default)]
    pub inherit_working_directory: bool,
    #[serde(default)]
    pub process_lifetime: crate::workloads::session::ProcessLifetime,
    #[serde(default)]
    pub workload_owner: Option<WorkloadOwner>,
    pub config: BenchmarkConfig,
    pub connection: Connection,
    pub prompt: String,
    #[serde(default)]
    pub max_steps: Option<u64>,
    pub project_root: PathBuf,
    pub runtime_dir: PathBuf,
    pub run_dir: PathBuf,
    #[serde(default)]
    pub replay: bool,
    /// Hide the snapshot supervisor's private storage mount from the agent
    /// while retaining the independently mounted task workspace.
    #[serde(default)]
    pub snapshot_private_mount: bool,
    #[serde(default)]
    pub snapshot_archive_sha256: Option<String>,
    /// Exact provider-tape path forwarded by the owning runner.  It is never
    /// inferred from the provider endpoint because record and replay are both
    /// routed through loopback services.
    #[serde(default)]
    pub tape_cassette: Option<PathBuf>,
    /// Tool replay is independent of provider replay.  A recorded local-tool
    /// response remains authoritative even if the provider is live.
    #[serde(default)]
    pub tool_replay: bool,
    #[serde(default)]
    pub ignore_tool_mismatch: bool,
    #[serde(default)]
    pub ignore_mcp_mismatch: bool,
    #[serde(default)]
    pub tool_tape_cassette: Option<PathBuf>,
    /// Transparent relay direction for ordinary declared MCP servers.
    #[serde(default)]
    pub mcp_mode: Option<crate::tape::Mode>,
    /// Guest-private selected provider-tape alias with adjacent sealed MCP
    /// relay sidecars. It is never inferred from a loopback provider endpoint.
    #[serde(default)]
    pub mcp_tape_cassette: Option<PathBuf>,
    /// Payload-free authority for the fixed private native sidecar location.
    #[serde(default)]
    pub native_tool_launch: Option<crate::cassette::NativeToolLaunch>,
    /// Opaque capsule direction selected by the owning remote dispatcher.
    /// The driver may carry this bounded intent across the guest boundary but
    /// does not infer paths or interpret adapter tool protocols.
    #[serde(default)]
    pub capsule: GuestCapsule,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: f64,
    #[serde(default)]
    pub namespace_collectors: Vec<String>,
}

/// Opaque request for a capsule owned by the selected adapter.  The remote
/// transport carries this envelope but neither interprets a capsule protocol
/// nor chooses files within one.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuestCapsule {
    #[serde(default)]
    pub mode: GuestCapsuleMode,
    /// A guest-private, relative export target.  It is only a bounded
    /// transport declaration in this slice; later adapter-owned capture will
    /// publish its bytes through the existing receipt channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export: Option<GuestCapsuleExport>,
}

impl GuestCapsule {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if let Some(export) = &self.export {
            export.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GuestCapsuleMode {
    #[default]
    None,
    Record,
    Replay,
}

/// Location class for a future adapter-owned guest capsule export.  The
/// controller supplies only a private relative name; no host path crosses the
/// request boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuestCapsuleExport {
    pub target: String,
}

impl GuestCapsuleExport {
    fn validate(&self) -> Result<(), Error> {
        if self.target.is_empty() || self.target.len() > 128 {
            return Err(Error::Input(
                "guest capsule export target is invalid".into(),
            ));
        }
        let path = Path::new(&self.target);
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::CurDir
                )
            })
        {
            return Err(Error::Input("guest capsule export target is unsafe".into()));
        }
        Ok(())
    }
}

fn default_timeout() -> f64 {
    900.0
}

#[derive(Deserialize, Serialize)]
pub(crate) struct GuestResult {
    /// The Pi snapshot broker settled after consuming its complete tool stream.
    /// This does not attest coverage of arbitrary guest effects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_replay_consumed: Option<bool>,
    #[serde(flatten)]
    pub observation: crate::process_termination::Observation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_instrumentation: Option<crate::workloads::staging::Ownership>,
    /// Exact effective directory selected before launch, not re-resolved later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_workspace: Option<PathBuf>,
    #[serde(default)]
    pub artifact_error: Option<String>,
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub workload_owner: Option<WorkloadOwner>,
    pub schema_version: u32,
    pub exit_code: Option<i32>,
    pub final_response: Option<String>,
    pub token_usage: crate::measurements::TokenUsage,
    pub steps: Option<u64>,
    /// Private, frozen tool transcript returned only to the owning host
    /// recorder. It is sealed into the cassette before the run is published;
    /// reports and command output never render its contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_tool_transcript: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deterministic_manifest: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_trace: Option<Vec<u8>>,
    /// Private complete native-tool stream.  It crosses an isolated guest only
    /// through the receipt-backed artifact channel and is sealed by the host
    /// recorder, never serialized in the controller result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_tool_sidecar: Option<Vec<u8>>,
    /// Generic frozen adapter state exported by an isolated guest for host-side
    /// collection after the outer provider tape settles.  Its payload is never
    /// present in controller JSON or user-facing evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_collection: Option<Vec<u8>>,
    /// Host-only sealed sidecars restored from the controller-owned export
    /// directory after this small result record crosses the guest boundary.
    #[serde(skip)]
    pub sealed_artifacts: Vec<super::SealedArtifact>,
    /// Controller-local snapshot stream archive, never serialized to the guest.
    #[serde(skip)]
    pub snapshot_artifact: Option<PathBuf>,
    /// Small receipts for opaque sidecars. Their bytes remain in the guest
    /// until the workload controller copies each verified file directly to its
    /// private host artifact directory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exported_artifacts: Vec<GuestArtifactReceipt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub adapter_errors: Vec<String>,
    pub execution_failure: Option<String>,
    #[serde(default)]
    pub tree_quiesced: bool,
    /// The isolated client presentation transcript reached the bounded
    /// retention limit. The retained prefix is still useful for adapter
    /// extraction, while controller-side outcome handling must not mistake it
    /// for a complete transcript.
    #[serde(default)]
    pub transcript_truncated: bool,
    #[serde(default)]
    pub namespace_collectors: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Remove enclosing host/guest orchestration state from a benchmark client.
pub(crate) fn isolate_parent_state(command: &mut Command) {
    for name in [
        REQUEST,
        "CODEX_REMOTE_PAYLOAD",
        "CODEX_SESSION_ID",
        "CODEX_THREAD_ID",
        "CODEX_TOOL_CALL_ID",
        "CODEX_VERSION",
        "CODEX_PERMISSION_PROFILE",
        "CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS",
        "CODEX_MANAGED_PACKAGE_ROOT",
        "CODEX_MANAGED_BY_NPM",
        "CODEX_MANAGED_BY_BUN",
        "CODEX_MANAGED_BY_PNPM",
        "CODEX_MANAGED_BY_VITE_PLUS",
        "CODEX_CI",
    ] {
        command.env_remove(name);
    }
}

/// Allocate fresh conventional client state for one repeated native execution.
///
/// No parent files are copied. Adapter preparation may subsequently install its
/// own selected-provider configuration inside this execution's private runtime.
pub(crate) fn prepare_fresh_state(command: &mut Command, root: &Path) -> Result<(), Error> {
    let mut parent = fs::DirBuilder::new();
    parent.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        parent.mode(0o700);
    }
    parent
        .create(
            root.parent()
                .ok_or_else(|| Error::Input("missing agent state parent".into()))?,
        )
        .map_err(crate::io_error)?;
    let mut directory = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    // Existing state is never reused or cleaned on behalf of this attempt.
    directory.create(root).map_err(crate::io_error)?;
    for (variable, name) in [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("TMPDIR", "tmp"),
    ] {
        let path = root.join(name);
        directory.create(&path).map_err(crate::io_error)?;
        command.env(variable, &path);
        if variable == "TMPDIR" {
            command.env("TMP", &path).env("TEMP", &path);
        }
    }
    // Registry owners declare aliases; the generic runner has no agent-name
    // branches. Remove enclosing-agent overrides too, not only the selected
    // client's, before the selected adapter prepares fresh configuration.
    for adapter in super::adapters::REGISTRY {
        for variable in adapter.state_environment_vars() {
            command.env_remove(variable);
        }
    }
    Ok(())
}

fn absolute(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
}

fn execution_workspace(request: &GuestRequest) -> Result<PathBuf, Error> {
    if request.inherit_working_directory {
        std::env::current_dir()
            .map_err(|_| Error::Input("cannot resolve inherited task shell directory".into()))
    } else {
        Ok(PathBuf::from(&request.config.workspace))
    }
}

/// Make an owned private tree accessible to the fixed non-root client
/// identity. Callers grant write access only to task material and per-attempt
/// state; installed tools and project configuration remain read/execute-only.
///
/// The workload container is already private.  We deliberately change mode
/// bits rather than invoking a host utility such as `chown`: minimal benchmark
/// images need not contain that utility, and `symlink_metadata` guarantees
/// that task-controlled links are never followed outside the owned tree.
#[cfg(target_os = "linux")]
fn grant_unprivileged_access(path: &Path, writable: bool) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;

    const MAX_OWNED_NODES: usize = 1_000_000;
    let mut pending = vec![path.to_path_buf()];
    let mut visited = 0usize;
    while let Some(current) = pending.pop() {
        visited += 1;
        if visited > MAX_OWNED_NODES {
            return Err(Error::Run(
                "owned workspace exceeds the safe node limit for unprivileged execution".into(),
            ));
        }
        let metadata = fs::symlink_metadata(&current).map_err(crate::io_error)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        let mode = metadata.permissions().mode();
        let accessible_mode = if metadata.is_dir() {
            mode | if writable { 0o777 } else { 0o555 }
        } else if metadata.is_file() {
            if writable {
                mode | 0o666
            } else {
                mode | 0o444 | if mode & 0o111 != 0 { 0o111 } else { 0 }
            }
        } else {
            continue;
        };
        if accessible_mode != mode {
            fs::set_permissions(&current, fs::Permissions::from_mode(accessible_mode))
                .map_err(crate::io_error)?;
        }
        if metadata.is_dir() {
            for entry in fs::read_dir(&current).map_err(crate::io_error)? {
                pending.push(entry.map_err(crate::io_error)?.path());
            }
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn grant_unprivileged_access(_: &Path, _: bool) -> Result<(), Error> {
    Err(Error::Run(
        "this agent requires an isolated Linux non-root workload identity".into(),
    ))
}

fn configure_execution_identity(
    command: &mut Command,
    identity: crate::agents::ExecutionIdentity,
    workspace: &Path,
    runtime: &Path,
) -> Result<(), Error> {
    if identity != crate::agents::ExecutionIdentity::Unprivileged {
        return Ok(());
    }
    grant_unprivileged_access(workspace, true)?;
    grant_unprivileged_access(runtime, true)?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.uid(UNPRIVILEGED_UID).gid(UNPRIVILEGED_GID);
        Ok(())
    }
    #[cfg(not(unix))]
    Err(Error::Run(
        "this agent requires an isolated Unix non-root workload identity".into(),
    ))
}

/// The staged project prefix is sealed and verified during guest preparation.
/// Its mode may only be relaxed after that verification, immediately before
/// the stopped launch gate is published. The client still receives no write
/// access to this private project/tool tree.
fn activate_unprivileged_project_access(
    identity: crate::agents::ExecutionIdentity,
    project: &Path,
    runtime: &Path,
) -> Result<(), Error> {
    if identity == crate::agents::ExecutionIdentity::Unprivileged {
        grant_unprivileged_access(project, false)?;
        grant_unprivileged_traverse(runtime)?;
        if let Some(launcher) = std::env::var_os("ASB_RUNTIME_EXECUTABLE") {
            grant_unprivileged_bundled_gate_access(Path::new(&launcher))?;
        }
    }
    Ok(())
}

/// The stopped gate uses the same bundled launcher as the guest driver, but
/// runs under the agent identity. Permit traversal without making its runtime
/// writable or exposing unrelated private attempt state.
fn grant_unprivileged_bundled_gate_access(launcher: &Path) -> Result<(), Error> {
    if !launcher.is_absolute() || !launcher.is_file() {
        return Err(Error::Run("invalid bundled ASB runtime launcher".into()));
    }
    let root = launcher
        .parent()
        .ok_or_else(|| Error::Run("bundled ASB runtime has no directory".into()))?;
    grant_unprivileged_traverse(root)?;
    grant_unprivileged_traverse(&root.join("bin"))?;
    grant_unprivileged_traverse(&root.join("lib"))?;
    Ok(())
}

/// Snapshot bookkeeping is an ASB implementation detail, not part of the
/// task filesystem. Keep it out of the agent's view without changing the
/// supervisor's mount namespace or detaching the task's workspace bind.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn hide_snapshot_storage() -> Result<(), Error> {
    use std::ffi::CString;
    let root = CString::new("/").expect("static mount path");
    let storage = CString::new("/asb-snapshot-storage").expect("static mount path");
    // SAFETY: paths are NUL-terminated constants; these calls affect only the
    // current process after unshare, which gives it a private mount namespace.
    unsafe {
        if libc::unshare(libc::CLONE_NEWNS) != 0 {
            return Err(Error::Run(format!(
                "cannot isolate snapshot bookkeeping mount: {}",
                std::io::Error::last_os_error()
            )));
        }
        if libc::mount(
            std::ptr::null(),
            root.as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ) != 0
        {
            return Err(Error::Run(format!(
                "cannot make snapshot mount namespace private: {}",
                std::io::Error::last_os_error()
            )));
        }
        if libc::umount2(storage.as_ptr(), libc::MNT_DETACH) != 0 {
            return Err(Error::Run(format!(
                "cannot hide snapshot bookkeeping mount: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn hide_snapshot_storage() -> Result<(), Error> {
    Err(Error::Run("snapshot mount isolation requires Linux".into()))
}

/// Permit traversal into the already writable `agent-state` child without
/// exposing the sealed runtime prefix for listing, reading, or writing.
#[cfg(target_os = "linux")]
fn grant_unprivileged_traverse(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = fs::symlink_metadata(path).map_err(crate::io_error)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Run(
            "private runtime prefix is not a real directory for unprivileged execution".into(),
        ));
    }
    let mode = metadata.permissions().mode();
    fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o011)).map_err(crate::io_error)
}

#[cfg(not(target_os = "linux"))]
fn grant_unprivileged_traverse(_: &Path) -> Result<(), Error> {
    Err(Error::Run(
        "this agent requires an isolated Linux non-root workload identity".into(),
    ))
}

struct PreparedGuest {
    command: Command,
    agent_route: Option<crate::launch::CommandRoute>,
    auxiliary_plan: Option<super::auxiliary::AuxiliaryPlan>,
    stdin_prompt: Option<Vec<u8>>,
    artifact_owner: super::ArtifactOwner,
    instrumentation: Option<crate::workloads::staging::Guard>,
    execution_identity: crate::agents::ExecutionIdentity,
}
impl std::fmt::Debug for PreparedGuest {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("PreparedGuest { private command, instrumentation and owned artifacts }")
    }
}
impl std::ops::Deref for PreparedGuest {
    type Target = Command;
    fn deref(&self) -> &Command {
        &self.command
    }
}
impl std::ops::DerefMut for PreparedGuest {
    fn deref_mut(&mut self) -> &mut Command {
        &mut self.command
    }
}

fn prepare(request: &GuestRequest) -> Result<PreparedGuest, Error> {
    if !request.timeout_seconds.is_finite() || !(0.001..=86400.0).contains(&request.timeout_seconds)
    {
        return Err(Error::Input(
            "guest agent time budget must be between 0.001 and 86400 seconds".into(),
        ));
    }
    if request.config.concurrency != 1 || request.prompt.len() > 64 * 1024 {
        return Err(Error::Input(
            "guest execution requires one bounded task invocation".into(),
        ));
    }
    let workspace = execution_workspace(request)?;
    let instrumentation = request
        .owned_instrumentation
        .as_ref()
        .map(|owner| {
            if request.runtime_dir != Path::new(crate::workloads::staging::PRIVATE)
                || request.run_dir != Path::new(crate::workloads::staging::PRIVATE).join("attempt")
                || request.project_root != Path::new("/opt/asb-project")
                || !request.task_identity.as_ref().is_some_and(|identity| {
                    identity.attempt_id == owner.attempt
                        && identity.workload == request.config.workload
                })
            {
                return Err(Error::Input(
                    "owned instrumentation does not match this task invocation".into(),
                ));
            }
            crate::workloads::staging::verify(owner)
        })
        .transpose()?;
    let instrumentation_overlap = if instrumentation.is_some() {
        crate::workloads::staging::contains_workspace(&workspace)?
            || crate::workloads::staging::contains_workspace(Path::new(&request.config.workspace))?
    } else {
        false
    };
    if ![
        workspace.as_path(),
        Path::new(&request.config.workspace),
        &request.project_root,
        &request.runtime_dir,
        &request.run_dir,
    ]
    .into_iter()
    .all(absolute)
        || (instrumentation.is_none()
            && (request.runtime_dir.starts_with(&workspace)
                || request.run_dir.starts_with(&workspace)))
        || instrumentation_overlap
        || !request.run_dir.starts_with(&request.runtime_dir)
    {
        return Err(Error::Input("guest workspace and private runtime paths must be absolute, separate and owned by this attempt".into()));
    }
    if !request.project_root.join(".asb/project.toml").is_file() {
        return Err(Error::Input(
            "guest tool project is not initialized; install the selected agent before execution"
                .into(),
        ));
    }
    let adapter = super::find(&request.config.agent).ok_or_else(|| {
        Error::Input("guest execution requires a registered agent adapter".into())
    })?;
    if request
        .max_steps
        .is_some_and(|limit| limit == 0 || limit > 65533)
        || request.max_steps.is_some() && !adapter.supports_max_steps()
    {
        return Err(Error::Input(
            "guest max steps is invalid or unsupported by the selected agent".into(),
        ));
    }
    if !adapter.supported_endpoints().iter().any(|support| {
        support.interface == request.connection.interface
            && support
                .authentication
                .contains(&request.connection.authentication)
    }) {
        return Err(Error::Input(
            "negotiated endpoint is not supported by the selected agent".into(),
        ));
    }
    let (_, project) = project::load(&request.project_root)?;
    let program = super::resolve(
        &request.config.agent,
        &request.project_root,
        &project.tools_dir,
    )
    .ok_or_else(|| {
        Error::Run("selected agent is not installed in the guest tool project".into())
    })?;
    let mut execution_config = request.config.clone();
    execution_config.workspace = workspace
        .to_str()
        .ok_or_else(|| Error::Input("task shell directory must be valid UTF-8".into()))?
        .to_owned();
    let native_tool_path = request.runtime_dir.join("native-tool-sidecar.jsonl");
    let native_tool = request
        .native_tool_launch
        .as_ref()
        .map(|launch| {
            launch
                .validate_staged(&native_tool_path)
                .map_err(|error| Error::Input(error.to_string()))?;
            Ok(super::NativeToolContext {
                launch,
                path: &native_tool_path,
            })
        })
        .transpose()?;
    let specification = adapter.command(&super::CommandContext {
        executable: &program,
        config: &execution_config,
        endpoint: Some(&request.connection.base_url),
        interface: Some(request.connection.interface),
        prompt: &request.prompt,
        max_steps: request.max_steps,
        native_tool_mode: request.tool_tape_cassette.as_ref().map(|_| {
            if request.tool_replay {
                crate::tape::Mode::Replay
            } else {
                crate::tape::Mode::Record
            }
        }),
        native_tool,
    });
    let process_program = if specification.program == adapter.executable() {
        &program
    } else {
        &specification.program
    };
    let mut launch = super::select_process(adapter, process_program, native_tool)?;
    for argument in specification.args {
        launch.arg(argument);
    }
    let stdin_prompt = match specification.prompt_delivery {
        super::PromptDelivery::Argument => {
            launch.arg(&request.prompt);
            None
        }
        super::PromptDelivery::Embedded => None,
        super::PromptDelivery::Stdin => Some(request.prompt.as_bytes().to_vec()),
    };
    let use_agent_gate = cfg!(target_os = "linux");
    let gate = if use_agent_gate {
        Some(crate::process_gate::executable().map_err(crate::io_error)?)
    } else {
        None
    };
    #[cfg(target_os = "linux")]
    let (mut command, route) = if request.process_lifetime
        == crate::workloads::session::ProcessLifetime::ThroughEvaluation
    {
        launch.measured(gate.as_deref().expect("Linux agent gate"))
    } else {
        launch.native(gate.as_deref())
    };
    #[cfg(not(target_os = "linux"))]
    let (mut command, route) = launch.native(gate.as_deref());
    let agent_route = use_agent_gate.then_some(route);
    command
        .current_dir(&workspace)
        .stdin(if stdin_prompt.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolate_parent_state(&mut command);
    // Guest executions need the same fresh conventional client state as native
    // executions.  Adapters then add only their own selected-provider state.
    let state_root = request.runtime_dir.join("agent-state");
    prepare_fresh_state(&mut command, &state_root)?;
    if request.process_lifetime == crate::workloads::session::ProcessLifetime::ThroughEvaluation {
        // The original same-shell evaluator observes the workload user's
        // home. A retained client must write its service configuration there;
        // client-specific XDG state remains private to this attempt.
        let home = std::env::var_os("HOME")
            .ok_or_else(|| Error::Input("retained workload has no home directory".into()))?;
        command.env("HOME", home);
    }
    if request.replay {
        super::mask_replay_credentials(&mut command);
    }
    project::apply_network_proxy(&mut command, &request.project_root)?;
    // Do not replace a benchmark's Python PATH with the agent's interpreter.
    // Agent entry points/wrappers use their own absolute venv interpreter.
    crate::tools::apply_runtime_path(&mut command, &request.project_root, &project.tools_dir)?;
    let mut artifact_specs = adapter.artifact_specs().to_vec();
    if request.ignore_tool_mismatch {
        artifact_specs.push(super::tool_replay_mcp::IGNORED_EVENTS_SPEC);
        artifact_specs.push(super::tool_replay_mcp::IGNORED_PROGRESS_SPEC);
    }
    if request.ignore_mcp_mismatch {
        artifact_specs.push(super::mcp_relay::IGNORED_MCP_SPEC);
    }
    let artifact_owner = super::ArtifactOwner::prepare(&request.run_dir, &artifact_specs)?;
    if request.ignore_mcp_mismatch {
        super::mcp_relay::initialize_ignored_journal(&request.run_dir)?;
    }
    adapter.prepare(&mut super::PreparationContext {
        command: &mut command,
        model: &request.config.model,
        connection: &request.connection,
        workspace: &workspace,
        runtime_dir: &state_root,
        run_dir: &request.run_dir,
        mcp_servers: &request.config.mcp_servers,
        mcp_tape: request.mcp_mode.map(|mode| super::McpTapeContext {
            ignore_mcp_mismatch: request.ignore_mcp_mismatch,
            mode,
            cassette: request.mcp_tape_cassette.as_deref(),
        }),
        tape: request
            .tool_tape_cassette
            .as_deref()
            .map(|cassette| super::TapeContext {
                ignore_tool_mismatch: request.ignore_tool_mismatch,
                mode: if request.tool_replay {
                    crate::tape::Mode::Replay
                } else {
                    crate::tape::Mode::Record
                },
                cassette,
            }),
    })?;
    let auxiliary_plan = adapter.prepare_guest_service(
        &mut command,
        process_program,
        &request.run_dir,
        request.tool_tape_cassette.as_ref().map(|_| {
            if request.tool_replay {
                crate::tape::Mode::Replay
            } else {
                crate::tape::Mode::Record
            }
        }),
        native_tool.map(|native| native.path),
        request.snapshot_archive_sha256.as_deref(),
    )?;
    configure_execution_identity(
        &mut command,
        adapter.execution_identity(),
        &workspace,
        &state_root,
    )?;
    Ok(PreparedGuest {
        command,
        agent_route,
        auxiliary_plan,
        stdin_prompt,
        artifact_owner,
        instrumentation,
        execution_identity: adapter.execution_identity(),
    })
}

fn run(request: GuestRequest) -> Result<u8, Error> {
    // Do not consume the Docker release capability yet.  An application-mode
    // driver is its own measured principal: it must first publish its stopped
    // readiness frame so the workload controller can attach collectors and
    // create that marker.  Consuming it before that frame deadlocks both sides
    // of the protocol (the controller releases only after the frame).
    let archive_gate_pending = std::env::var_os(GATE_RELEASE).is_some();
    request.capsule.validate()?;
    validate_snapshot_marker(
        request.snapshot_private_mount,
        request.tool_tape_cassette.is_some(),
        request.tool_replay,
        std::env::var_os("ASB_FS_MARKER_SOCKET").as_deref(),
        std::env::var_os("ASB_FS_REPLAY_SOCKET").as_deref(),
    )?;
    // Establish this before preparation or collectors can start threads;
    // Linux cannot unshare the filesystem context after it is shared.
    if request.snapshot_private_mount {
        hide_snapshot_storage()?;
    }
    let retained =
        request.process_lifetime == crate::workloads::session::ProcessLifetime::ThroughEvaluation;
    if retained {
        let owner = request
            .workload_owner
            .as_ref()
            .ok_or_else(|| Error::Input("workload lifetime has no namespace owner".into()))?;
        if owner.controller_namespace <= 1
            || owner.guest_namespace != namespace_id()?
            || owner.guest_namespace == owner.controller_namespace
            || owner.token.len() != 32
            || !owner.token.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(Error::Input(
                "workload lifetime is not bound to a distinct owned PID namespace".into(),
            ));
        }
    } else if request.workload_owner.is_some() {
        return Err(Error::Input(
            "quiescent guest execution cannot declare retained ownership".into(),
        ));
    }
    let capsule_replay = match request.capsule.mode {
        GuestCapsuleMode::Replay => {
            let export = request.capsule.export.as_ref().ok_or_else(|| {
                Error::Input("guest native capsule replay has no staged input".into())
            })?;
            let input = request.runtime_dir.join(&export.target);
            let bytes = fs::read(&input).map_err(|_| {
                Error::Run("guest native capsule replay input is unavailable".into())
            })?;
            if bytes.len() > MAX_EXPORTED_ARTIFACT_BYTES {
                return Err(Error::Run(
                    "guest native capsule replay exceeds its bound".into(),
                ));
            }
            Some(bytes)
        }
        GuestCapsuleMode::None | GuestCapsuleMode::Record => None,
    };
    let PreparedGuest {
        mut command,
        mut agent_route,
        auxiliary_plan,
        stdin_prompt,
        mut artifact_owner,
        instrumentation,
        execution_identity,
    } = prepare(&request)?;
    if retained && auxiliary_plan.is_some() {
        return Err(Error::Run(
            "adapter auxiliary recording requires agent cleanup before evaluation".into(),
        ));
    }
    // Keep the same launch identity even if a tool renames/deletes its cwd.
    // Looking up current_dir again after execution can erase a valid outcome.
    let workspace = command
        .get_current_dir()
        .ok_or_else(|| Error::Input("prepared guest command has no task directory".into()))?
        .to_path_buf();
    fs::create_dir_all(&request.run_dir)
        .map_err(|error| Error::Io(format!("cannot create private guest artifacts: {error}")))?;
    let mut capture = if request.namespace_collectors.is_empty() {
        None
    } else {
        let mut session = crate::measurements::Session::start_in_execution_namespace(
            &request.run_dir.join("collector-capture"),
            &request.namespace_collectors,
        )
        .map_err(Error::Run)?;
        let target = agent_route.as_ref().map_or_else(
            || crate::launch::TargetLaunch::direct(&command),
            |route| route.target(&command),
        );
        session.agent_launch_prepared(&target);
        if agent_route.is_none() {
            session.attach_pid(std::process::id()).map_err(Error::Run)?;
        }
        Some(session)
    };
    if let (Some(guard), Some(owner)) = (&instrumentation, &request.owned_instrumentation) {
        guard.revalidate(owner)?;
    }
    activate_unprivileged_project_access(
        execution_identity,
        &request.project_root,
        &request.runtime_dir,
    )?;
    let application_archive_gate = archive_gate_pending && agent_route.is_none();
    let archive_gate_released =
        announce_application_gate_then_consume(application_archive_gate, &mut std::io::stdout())?;
    if agent_route.is_none() && !application_archive_gate {
        println!("__ASB_GATE__:{}", std::process::id());
        std::io::stdout().flush().map_err(crate::io_error)?;
        if !archive_gate_released {
            #[cfg(unix)]
            crate::process_gate::pause_current().map_err(|error| {
                Error::Run(format!("cannot stop prepared guest agent: {error}"))
            })?;
            #[cfg(not(unix))]
            return Err(Error::Run(
                "guest agent gate currently requires Unix signals".into(),
            ));
        }
    }
    let mut auxiliary = auxiliary_plan
        .map(super::auxiliary::AuxiliaryPlan::start)
        .transpose()?;
    #[cfg(target_os = "linux")]
    let retained_pid = std::cell::Cell::new(None);
    #[cfg(target_os = "linux")]
    let held = if retained {
        Some(crate::runner::workload_process::start_with_spawn(
            (&mut command, agent_route.take()),
            stdin_prompt.as_deref(),
            &mut std::io::stdout(),
            &mut std::io::stderr(),
            std::time::Duration::from_secs_f64(request.timeout_seconds),
            |pid| {
                retained_pid.set(Some(pid));
                if let Some(session) = capture.as_mut() {
                    session.attach_pid(pid).map_err(Error::Run)?;
                    session.agent_process_spawned(pid).map_err(Error::Run)?;
                }
                println!("__ASB_GATE__:{pid}");
                std::io::stdout().flush().map_err(crate::io_error)?;
                Ok(())
            },
            |status, elapsed, timed_out| {
                let owner = request
                    .workload_owner
                    .as_ref()
                    .expect("retained owner validated");
                println!(
                    "\n__ASB_AGENT_ENDED__:{}:{}:{}:{}:{}",
                    retained_pid.get().expect("retained principal started"),
                    owner.token,
                    status.code().unwrap_or(1),
                    elapsed,
                    u8::from(timed_out)
                );
                std::io::stdout().flush().map_err(crate::io_error)
            },
        )?)
    } else {
        None
    };
    let (output, transcript_truncated, execution_timed_out, observation) = if retained {
        #[cfg(target_os = "linux")]
        {
            let held = held.as_ref().expect("retained process");
            (
                held.output.clone(),
                held.truncated,
                held.timed_out,
                crate::process_termination::Observation::observed(held.output.status),
            )
        }
        #[cfg(not(target_os = "linux"))]
        return Err(Error::Input(
            "retained workload processes require Linux".into(),
        ));
    } else {
        #[cfg(unix)]
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs_f64(request.timeout_seconds)
            + std::time::Duration::from_secs(30);
        #[cfg(unix)]
        let mut transport_out = crate::runner::GuestTransport::new(&std::io::stdout(), deadline)
            .map_err(crate::io_error)?;
        #[cfg(unix)]
        let mut transport_err = crate::runner::GuestTransport::new(&std::io::stderr(), deadline)
            .map_err(crate::io_error)?;
        let gated_agent = agent_route.is_some();
        let captured = crate::runner::run_guest_process(
            (
                &mut command,
                agent_route.take(),
                auxiliary
                    .as_mut()
                    .map(super::auxiliary::AuxiliaryService::child),
            ),
            stdin_prompt.as_deref(),
            #[cfg(unix)]
            &mut transport_out,
            #[cfg(not(unix))]
            &mut std::io::stdout(),
            #[cfg(unix)]
            &mut transport_err,
            #[cfg(not(unix))]
            &mut std::io::stderr(),
            std::time::Duration::from_secs_f64(request.timeout_seconds),
            &mut |pid| {
                if let Some(session) = capture.as_mut() {
                    if gated_agent {
                        session.attach_pid(pid).map_err(Error::Run)?;
                    }
                    session.agent_process_spawned(pid).map_err(Error::Run)?;
                }
                if gated_agent {
                    println!("__ASB_GATE__:{pid}");
                    std::io::stdout().flush().map_err(crate::io_error)?;
                }
                Ok(())
            },
        );
        let captured = match captured {
            Ok(value) => value,
            Err(error) => {
                if let Some(detail) = auxiliary.as_mut().and_then(|service| service.early_exit()) {
                    return Err(Error::Run(format!("{error}; {detail}")));
                }
                return Err(error);
            }
        };
        (
            captured.output,
            captured.transcript_truncated,
            captured.timed_out,
            captured.observation,
        )
    };
    let snapshot_replay = request.tool_replay
        && request.native_tool_launch.is_some()
        && request.snapshot_archive_sha256.is_some();
    let native_replay_selected = snapshot_replay
        && super::find(&request.config.agent)
            .is_some_and(|adapter| adapter.snapshot_replay_uses_settlement_service());
    let mut native_replay_consumed = match settle_auxiliary(&mut auxiliary, native_replay_selected)
    {
        Ok(value) => value,
        Err(error) if execution_timed_out => {
            // A stopped principal can leave native calls unfinished. Preserve
            // the owner-confirmed timeout while retaining all unsettled raw
            // artifacts; do not publish a complete native replay receipt.
            let _ = artifact_owner.freeze();
            let partial =
                unsettled_timeout_result(observation, cfg!(target_os = "linux") && !retained);
            if let Err(publication) = publish_result(&request.run_dir, &partial) {
                return Err(Error::Run(format!(
                    "agent execution exceeded its time budget; {error}; cannot publish timeout evidence: {publication}"
                )));
            }
            return Err(Error::Run(format!(
                "agent execution exceeded its time budget; {error}"
            )));
        }
        Err(error) => return Err(error),
    };
    let namespace_collectors = capture
        .as_mut()
        .map(|session| session.export_execution_namespace_state())
        .transpose()
        .map_err(Error::Run)?
        .unwrap_or_default();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let adapter = super::find(&request.config.agent).expect("validated agent");
    let frozen = artifact_owner.freeze();
    if snapshot_replay && native_replay_consumed.is_none() {
        native_replay_consumed = adapter.snapshot_replay_consumed(&frozen)?;
    }
    let deferred_collection = frozen.deferred_collection_bytes()?;
    let mut sealed = vec![super::SealedArtifact {
        kind: DEFERRED_COLLECTION_ARTIFACT.into(),
        bytes: deferred_collection,
    }];
    if request.capsule.mode == GuestCapsuleMode::Record {
        sealed.push(super::SealedArtifact {
            kind: NATIVE_WORKSPACE_CAPSULE_ARTIFACT.into(),
            bytes: adapter.capture_native_workspace_capsule(&workspace)?,
        });
    }
    if let Some(bytes) = capsule_replay {
        adapter.apply_native_workspace_capsule(&workspace, &bytes)?;
    }
    let usage = crate::usage::from_sources(
        &request.config.provider,
        &request.config.agent,
        &stdout,
        &stderr,
        None,
    );
    let usage = crate::usage::with_artifact_usage(
        usage,
        adapter.guest_token_usage(
            &request.run_dir,
            crate::providers::anthropic_input_includes_cache(&request.config.provider),
        ),
    );
    // Until the first durable publication transfers the frozen evidence, Drop
    // retains raw artifacts. This interim record never asserts settlement.
    let mut result = GuestResult {
        native_replay_consumed,
        observation,
        artifact_error: Some(
            "agent scratch settlement pending; source or custody may remain".into(),
        ),
        owned_instrumentation: request.owned_instrumentation.clone(),
        execution_workspace: Some(workspace.clone()),
        #[cfg(target_os = "linux")]
        elapsed_ms: held.as_ref().map(|held| held.elapsed_ms),
        #[cfg(not(target_os = "linux"))]
        elapsed_ms: None,
        timed_out: execution_timed_out,
        workload_owner: request.workload_owner,
        schema_version: 1,
        exit_code: match observation.process_termination {
            Some(crate::process_termination::ProcessTermination::Code(code)) => Some(code),
            _ => None,
        },
        final_response: adapter.final_response_with_artifacts(
            &stdout,
            &frozen,
            &workspace,
            !transcript_truncated,
        ),
        token_usage: usage,
        steps: adapter
            .step_count(&stdout, &stderr)
            .or_else(|| adapter.guest_step_count(&request.run_dir)),
        // Large replay sidecars must cross the guest boundary as verified
        // files.  Embedding their byte vectors would base64-expand them into
        // the one-megabyte controller frame.
        local_tool_transcript: None,
        deterministic_manifest: None,
        native_trace: None,
        native_tool_sidecar: None,
        deferred_collection: None,
        sealed_artifacts: Vec::new(),
        snapshot_artifact: None,
        exported_artifacts: Vec::new(),
        adapter_errors: Vec::new(),
        execution_failure: {
            #[cfg(target_os = "linux")]
            let timed_out = held.as_ref().is_some_and(|held| held.timed_out);
            #[cfg(not(target_os = "linux"))]
            let timed_out = false;
            if timed_out {
                Some("agent execution exceeded its time budget".into())
            } else {
                adapter.execution_failure(&stdout, &stderr)
            }
        },
        tree_quiesced: cfg!(target_os = "linux") && !retained,
        transcript_truncated,
        namespace_collectors,
    };
    result.exported_artifacts = export_sealed_artifacts(&request.run_dir, &sealed)?;
    publish_artifacts(&mut result, artifact_owner, frozen.diagnostic(), |result| {
        publish_result(&request.run_dir, result)
    })?;
    #[cfg(target_os = "linux")]
    if let Some(held) = held {
        let owner = result
            .workload_owner
            .as_ref()
            .expect("retained namespace owner");
        println!(
            "\n__ASB_WORKLOAD_READY__:{}:{}:{}",
            retained_pid.get().expect("retained principal started"),
            owner.token,
            output.status.code().unwrap_or(1)
        );
        std::io::stdout().flush().map_err(crate::io_error)?;
        held.hold_for_evaluation();
    }
    Ok(output
        .status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}

// The adapter selects native snapshot interception from these environment
// variables. Losing one must not silently turn a snapshot replay into live
// tool execution. Ordinary non-snapshot runs retain their existing behavior.
fn validate_snapshot_marker(
    snapshot: bool,
    tool_tape: bool,
    replay: bool,
    record_marker: Option<&std::ffi::OsStr>,
    replay_marker: Option<&std::ffi::OsStr>,
) -> Result<(), Error> {
    if !snapshot {
        return Ok(());
    }
    let (expected, unexpected) = if replay {
        (replay_marker, record_marker)
    } else {
        (record_marker, replay_marker)
    };
    if !tool_tape || expected.is_none_or(|value| value.is_empty()) || unexpected.is_some() {
        return Err(Error::Input(
            "snapshot tool capture requires exactly one matching nonempty marker and a tool cassette".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[test]
fn snapshot_guest_requires_the_matching_marker_before_launch() {
    use std::ffi::OsStr;
    let socket = Some(OsStr::new("/opt/asb-private/snapshot.sock"));
    let empty = Some(OsStr::new(""));
    assert!(validate_snapshot_marker(false, false, false, None, None).is_ok());
    for replay in [false, true] {
        assert!(validate_snapshot_marker(true, true, replay, None, None).is_err());
        assert!(validate_snapshot_marker(true, true, replay, empty, empty).is_err());
        assert!(validate_snapshot_marker(true, true, replay, socket, socket).is_err());
        let (record, replay_socket) = if replay {
            (None, socket)
        } else {
            (socket, None)
        };
        assert!(validate_snapshot_marker(true, true, replay, record, replay_socket).is_ok());
        assert!(validate_snapshot_marker(true, false, replay, record, replay_socket).is_err());
        assert!(validate_snapshot_marker(true, true, !replay, record, replay_socket).is_err());
    }
}

#[cfg(test)]
#[test]
fn guest_gate_release_path_is_private_and_fixed_shape() {
    assert!(valid_gate_release_path(Path::new(
        "/tmp/.asb-gate-release-0123456789abcdef0123456789abcdef"
    )));
    for path in [
        "/tmp/asb-gate-release-0123456789abcdef0123456789abcdef",
        "/tmp/.asb-gate-release-0123456789abcdef0123456789abcdeF",
        "/tmp/.asb-gate-release-0123",
        "/var/tmp/.asb-gate-release-0123456789abcdef0123456789abcdef",
        "/tmp/sub/.asb-gate-release-0123456789abcdef0123456789abcdef",
    ] {
        assert!(!valid_gate_release_path(Path::new(path)), "{path}");
    }
}

#[cfg(test)]
#[test]
fn application_gate_announces_before_consuming_the_archive_release() {
    const CASE: &str = "ASB_GUEST_GATE_RELEASE_CASE";
    if std::env::var_os(CASE).is_some() {
        let mut ready = Vec::new();
        assert!(announce_application_gate_then_consume(true, &mut ready).unwrap());
        assert!(
            String::from_utf8(ready)
                .unwrap()
                .starts_with("__ASB_GATE__:")
        );
        assert!(std::env::var_os(GATE_RELEASE).is_none());
        return;
    }
    let release = Path::new("/tmp").join(format!(
        ".asb-gate-release-{:032x}",
        crate::workloads::TaskIdentity::local("guest-archive-release", None)
            .attempt_id
            .as_bytes()
            .iter()
            .fold(0u128, |value, byte| value
                .wrapping_mul(31)
                .wrapping_add(*byte as u128))
    ));
    fs::write(&release, []).unwrap();
    let name = format!(
        "{}::application_gate_announces_before_consuming_the_archive_release",
        module_path!().split_once("::").unwrap().1
    );
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env(CASE, "1")
        .env(GATE_RELEASE, &release)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(release.exists());
    fs::remove_file(release).unwrap();
}

// Successful broker settlement checks the complete selected native stream.
fn settle_auxiliary(
    auxiliary: &mut Option<super::auxiliary::AuxiliaryService>,
    native_replay_selected: bool,
) -> Result<Option<bool>, Error> {
    if native_replay_selected && auxiliary.is_none() {
        return Err(Error::Run(
            "native snapshot replay has no settlement service".into(),
        ));
    }
    if let Some(service) = auxiliary.as_mut() {
        service.settle()?;
    }
    Ok(native_replay_selected.then_some(true))
}

/// Move replay sidecars into receipt-backed files before the result reaches
/// the bounded controller JSON transport.
#[cfg(test)]
fn controller_sidecars(artifacts: &mut super::AgentArtifacts) -> Vec<super::SealedArtifact> {
    let mut sidecars = std::mem::take(&mut artifacts.sealed_artifacts);
    for (kind, bytes) in [
        (
            LOCAL_TOOL_TRANSCRIPT_ARTIFACT,
            artifacts.local_tool_transcript.take(),
        ),
        (
            DETERMINISTIC_MANIFEST_ARTIFACT,
            artifacts.deterministic_manifest.take(),
        ),
        (NATIVE_TRACE_ARTIFACT, artifacts.native_trace.take()),
        (
            NATIVE_TOOL_SIDECAR_ARTIFACT,
            artifacts.native_tool_sidecar.take(),
        ),
    ] {
        if let Some(bytes) = bytes {
            sidecars.push(super::SealedArtifact {
                kind: kind.into(),
                bytes,
            });
        }
    }
    sidecars
}

/// Keep opaque adapter sidecars out of the one-megabyte guest controller
/// record. The owning workload controller later transfers each receipt's one
/// private file directly out of the container and verifies this commitment.
fn export_sealed_artifacts(
    run_dir: &Path,
    artifacts: &[super::SealedArtifact],
) -> Result<Vec<GuestArtifactReceipt>, Error> {
    if artifacts.len() > MAX_EXPORTED_ARTIFACTS {
        return Err(Error::Run(
            "guest exported artifact count exceeds its bound".into(),
        ));
    }
    let total = artifacts.iter().try_fold(0usize, |total, artifact| {
        total.checked_add(artifact.bytes.len())
    });
    if total.is_none_or(|bytes| bytes > MAX_EXPORTED_ARTIFACT_BYTES) {
        return Err(Error::Run(
            "guest exported artifacts exceed their aggregate bound".into(),
        ));
    }
    if artifacts.is_empty() {
        return Ok(Vec::new());
    }
    let directory = run_dir.join("exports");
    fs::create_dir(&directory).map_err(|error| {
        Error::Io(format!(
            "cannot create guest artifact export directory: {error}"
        ))
    })?;
    artifacts
        .iter()
        .enumerate()
        .map(|(index, artifact)| {
            if artifact.kind.is_empty()
                || artifact.kind.len() > 128
                || !artifact
                    .kind
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            {
                return Err(Error::Run("guest artifact has an invalid kind".into()));
            }
            let filename = format!("artifact-{index}");
            let path = directory.join(&filename);
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&path).map_err(crate::io_error)?;
            file.write_all(&artifact.bytes)
                .and_then(|()| file.sync_all())
                .map_err(crate::io_error)?;
            Ok(GuestArtifactReceipt {
                kind: artifact.kind.clone(),
                filename,
                bytes: artifact.bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&artifact.bytes)),
            })
        })
        .collect()
}

fn publish_artifacts(
    result: &mut GuestResult,
    artifact_owner: super::ArtifactOwner,
    snapshot_error: Option<String>,
    mut publish: impl FnMut(&GuestResult) -> Result<(), Error>,
) -> Result<(), Error> {
    if artifact_owner.is_empty() {
        result.artifact_error = snapshot_error;
        return publish(result);
    }
    publish(result)?;
    result.artifact_error = match (snapshot_error, artifact_owner.settle().err()) {
        (Some(snapshot), Some(settlement)) => Some(format!("{snapshot}; {settlement}")),
        (snapshot, settlement) => snapshot.or(settlement),
    };
    // If final publication fails, the earlier durable record truthfully says
    // pending. Continue the existing workload/grade lifetime rather than losing
    // a usable submission or inventing a workload-quiescence failure.
    if publish(result).is_err() {
        result.artifact_error = Some("agent scratch settlement final publication unavailable; inspect pending result and source or custody".into());
    }
    Ok(())
}

fn unsettled_timeout_result(
    observation: crate::process_termination::Observation,
    tree_quiesced: bool,
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "timed_out": true,
        "process_termination": observation.process_termination,
        "process_termination_diagnostic": observation.process_termination_diagnostic,
        "exit_code": observation.process_termination.and_then(|termination| match termination {
            crate::process_termination::ProcessTermination::Code(code) => Some(code),
            _ => None,
        }),
        "final_response": null,
        "token_usage": {},
        "steps": null,
        "tree_quiesced": tree_quiesced,
        "artifact_error": "native tool settlement incomplete; raw artifacts retained",
        "execution_failure": "agent execution exceeded its time budget",
    })
}

fn publish_result(run_dir: &Path, result: &impl Serialize) -> Result<(), Error> {
    let destination = run_dir.join("agent-result.json");
    let temporary = run_dir.join(format!(".agent-result-{}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| Error::Io(error.to_string()))?;
    serde_json::to_writer(&mut file, result)
        .map_err(|_| Error::Io("cannot encode guest result".into()))?;
    file.sync_all()
        .and_then(|()| fs::rename(&temporary, &destination))
        .map_err(|error| Error::Io(error.to_string()))?;
    #[cfg(unix)]
    fs::File::open(run_dir)
        .and_then(|directory| directory.sync_all())
        .map_err(crate::io_error)?;
    Ok(())
}

pub(crate) fn entry() -> std::process::ExitCode {
    let result = (|| {
        let text = std::env::var(REQUEST)
            .map_err(|_| Error::Input("private guest agent request is missing".into()))?;
        if text.len() > 1024 * 1024 {
            return Err(Error::Input("guest request exceeds 1 MiB".into()));
        }
        let request = serde_json::from_str(&text)
            .map_err(|_| Error::Input("invalid guest execution request JSON".into()))?;
        run(request)
    })();
    match result {
        Ok(code) => std::process::ExitCode::from(code),
        Err(error) => {
            eprintln!("ASB guest agent could not execute: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(all(test, unix))]
mod artifact_publication_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn native_consumption_requires_a_successfully_settled_service() {
        assert!(settle_auxiliary(&mut None, true).is_err());
        assert_eq!(settle_auxiliary(&mut None, false).unwrap(), None);
        for (exit, selected, expected) in [(0, true, Some(true)), (0, false, None)] {
            let mut command = std::process::Command::new("sh");
            command.arg("-c").arg(format!(
                "printf 'READY\\n'; read x; test \"$x\" = S || exit 2; exit {exit}"
            ));
            let mut service =
                Some(super::super::auxiliary::AuxiliaryService::start(&mut command).unwrap());
            assert_eq!(settle_auxiliary(&mut service, selected).unwrap(), expected);
        }
        let mut command = std::process::Command::new("sh");
        command.arg("-c").arg("printf 'READY\\n'; read x; exit 1");
        let mut failed =
            Some(super::super::auxiliary::AuxiliaryService::start(&mut command).unwrap());
        assert!(settle_auxiliary(&mut failed, true).is_err());
    }

    #[test]
    fn unsettled_timeout_keeps_timeout_and_never_claims_replay_completion() {
        let value =
            unsettled_timeout_result(crate::process_termination::Observation::default(), true);
        assert_eq!(value["timed_out"], true);
        assert_eq!(value["tree_quiesced"], true);
        assert!(
            value["artifact_error"]
                .as_str()
                .unwrap()
                .contains("incomplete")
        );
        assert!(value.get("native_replay_consumed").is_none());
        assert!(value.get("native_tool_sidecar").is_none());
        assert!(value.get("exported_artifacts").is_none());
        assert_eq!(value["steps"], serde_json::Value::Null);
    }

    #[test]
    fn replay_sidecars_leave_the_controller_result_before_receipt_export() {
        let mut artifacts = super::super::AgentArtifacts {
            local_tool_transcript: Some(vec![b't'; 768 * 1024]),
            deterministic_manifest: Some(b"manifest".to_vec()),
            native_trace: Some(b"trace".to_vec()),
            native_tool_sidecar: Some(b"native-tool".to_vec()),
            ..Default::default()
        };
        let sidecars = controller_sidecars(&mut artifacts);
        assert!(artifacts.local_tool_transcript.is_none());
        assert!(artifacts.deterministic_manifest.is_none());
        assert!(artifacts.native_trace.is_none());
        assert!(artifacts.native_tool_sidecar.is_none());
        assert_eq!(sidecars.len(), 4);
        assert_eq!(sidecars[0].kind, LOCAL_TOOL_TRANSCRIPT_ARTIFACT);
        assert_eq!(sidecars[0].bytes.len(), 768 * 1024);
        assert_eq!(sidecars[1].kind, DETERMINISTIC_MANIFEST_ARTIFACT);
        assert_eq!(sidecars[2].kind, NATIVE_TRACE_ARTIFACT);
        assert_eq!(sidecars[3].kind, NATIVE_TOOL_SIDECAR_ARTIFACT);
    }

    #[test]
    fn replay_sidecars_preserve_the_existing_adapter_sidecar_budget() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/controller-sidecars")
            .join(crate::workloads::TaskIdentity::local("sidecars", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let mut artifacts = super::super::AgentArtifacts {
            sealed_artifacts: (0..4)
                .map(|index| super::super::SealedArtifact {
                    kind: format!("adapter-sidecar-{index}"),
                    bytes: vec![index as u8],
                })
                .collect(),
            local_tool_transcript: Some(b"tool".to_vec()),
            deterministic_manifest: Some(b"manifest".to_vec()),
            native_trace: Some(b"trace".to_vec()),
            native_tool_sidecar: Some(b"native-tool".to_vec()),
            ..Default::default()
        };
        let sidecars = controller_sidecars(&mut artifacts);
        let receipts = export_sealed_artifacts(&root, &sidecars).unwrap();
        assert_eq!(receipts.len(), 8);
        assert_eq!(receipts[6].kind, NATIVE_TRACE_ARTIFACT);
        assert_eq!(receipts[7].kind, NATIVE_TOOL_SIDECAR_ARTIFACT);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn actual_first_and_final_publication_failures_keep_honest_evidence() {
        for first_write_fails in [true, false] {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/agent-artifact-publication")
                .join(crate::workloads::TaskIdentity::local("publication", None).attempt_id);
            fs::create_dir_all(&root).unwrap();
            let mut owner = super::super::ArtifactOwner::prepare(
                &root,
                &[super::super::ArtifactSpec {
                    namespace: "scratch",
                    filename: "evidence.json",
                    format: super::super::ArtifactFormat::Json,
                }],
            )
            .unwrap();
            let raw = root.join("scratch/evidence.json");
            fs::write(&raw, r#"{"answer":"submission","usage":34,"steps":4}"#).unwrap();
            let frozen = owner.freeze();
            let mut result = GuestResult {
                native_replay_consumed: None,
                observation: Default::default(),
                owned_instrumentation: None,
                execution_workspace: None,
                artifact_error: Some(
                    "agent scratch settlement pending; source or custody may remain".into(),
                ),
                elapsed_ms: None,
                timed_out: false,
                workload_owner: None,
                schema_version: 1,
                exit_code: Some(0),
                final_response: Some("submission".into()),
                token_usage: crate::measurements::TokenUsage {
                    input_tokens: Some(13),
                    output_tokens: Some(21),
                    total_tokens: Some(34),
                    ..Default::default()
                },
                steps: Some(4),
                local_tool_transcript: None,
                deterministic_manifest: None,
                native_trace: None,
                native_tool_sidecar: None,
                deferred_collection: None,
                sealed_artifacts: Vec::new(),
                snapshot_artifact: None,
                exported_artifacts: Vec::new(),
                adapter_errors: Vec::new(),
                execution_failure: None,
                tree_quiesced: true,
                transcript_truncated: true,
                namespace_collectors: Default::default(),
            };
            let destination = root.join("agent-result.json");
            if first_write_fails {
                fs::create_dir(&destination).unwrap();
            }
            let mut writes = 0;
            let publication =
                publish_artifacts(&mut result, owner, frozen.diagnostic(), |result| {
                    writes += 1;
                    if writes == 2 {
                        // Real OS failure in the exact publisher, after the first
                        // record has transferred usable evidence. No product hook.
                        fs::create_dir(root.join(format!(".agent-result-{}", std::process::id())))
                            .unwrap();
                    }
                    publish_result(&root, result)
                });
            if first_write_fails {
                assert!(publication.is_err());
                assert_eq!(writes, 1);
                assert_eq!(
                    fs::read_to_string(&raw).unwrap(),
                    r#"{"answer":"submission","usage":34,"steps":4}"#
                );
            } else {
                publication.unwrap();
                assert_eq!(writes, 2);
                let durable: GuestResult =
                    serde_json::from_slice(&fs::read(&destination).unwrap()).unwrap();
                assert!(durable.artifact_error.unwrap().contains("pending"));
                assert_eq!(durable.final_response.as_deref(), Some("submission"));
                assert_eq!(durable.steps, Some(4));
                assert_eq!(durable.token_usage.total_tokens, Some(34));
                assert!(durable.transcript_truncated);
                assert!(
                    durable.tree_quiesced,
                    "scratch publication never rewrites process ownership"
                );
                assert!(
                    result
                        .artifact_error
                        .unwrap()
                        .contains("final publication unavailable")
                );
            }
            fs::remove_dir_all(root).unwrap();
        }
    }
}
