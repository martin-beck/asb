//! Common execution/evaluation protocol for workload-owned official engines.
use super::task_protocol::{
    AgentRequest, CompletedExecution, CompletedGrade, EventKind, PrivateInput, ReadyTask, Request,
};
use crate::workloads::controller::{Controller, ControllerCommand, decode_base64};
use crate::workloads::session::{
    BaselineEvidence, ExecutionContext, ExecutionResult, PreparationContext, ResourceDisposition,
    ResourceSettlement, TaskInvocation,
};
use crate::workloads::{Evaluation, TaskIdentity, WorkloadSession};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::time::{Duration, Instant};

const CANCELLATION_TIMEOUT: Duration = Duration::from_secs(240);
const AGENT_TRANSPORT_ALLOWANCE: Duration = Duration::from_secs(1820);
const EVALUATION_TIMEOUT_SECONDS: u64 = crate::planning::DEFAULT_EVALUATION_SECONDS;
const EVALUATION_PROTOCOL_ALLOWANCE: Duration = Duration::from_secs(30);
const INSPECTION_TIMEOUT: Duration = Duration::from_secs(60);
const RETAINED_GRADING_TIMEOUT: Duration = Duration::from_secs(1980);
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(180);
const INITIAL_MATERIAL_TIMEOUT: Duration = Duration::from_secs(600);
const COHORT_IDLE_ALLOWANCE: Duration = Duration::from_secs(120);
const WORKLOAD_READY_TIMEOUT: Duration =
    Duration::from_secs(crate::planning::DEFAULT_ENVIRONMENT_PREPARATION_SECONDS);
const RESUME_ACK_TIMEOUT: Duration = Duration::from_secs(60);
const TARGET_VERIFICATION_RETRY: Duration = Duration::from_secs(30);
const PROGRESS_POLL: Duration = Duration::from_millis(250);
const PROGRESS_TRANSCRIPT_LIMIT: usize = 8 * 1024 * 1024;
const MAX_PRIVATE_REPLAY_INPUT: u64 = 64 * 1024 * 1024;

/// Docker's exec inspection and `/proc` publication are host-global.  Dense
/// cohorts must not make dozens of stopped-target scans race through the same
/// short-lived driver trees.
static STOPPED_TARGET_VERIFICATION: std::sync::OnceLock<std::sync::Mutex<()>> =
    std::sync::OnceLock::new();

/// Keep a paused target's verified identity adjacent to collector attachment.
///
/// A cohort launch rendezvous may wait for many peers. Enter it only after
/// this replica has attached its collectors, while the target is still stopped.
/// The separate collector rendezvous remains the only authority to resume a
/// target.
fn attach_target_before_launch<T>(
    launch: Option<&dyn crate::workloads::session::LaunchReadiness>,
    collector_ready: Option<&dyn crate::workloads::session::LaunchReadiness>,
    attach: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let attached = attach()?;
    if let Some(launch) = launch {
        launch.ready()?;
    }
    // The final collector deadline starts only after every selected peer has
    // reached the shared launch rendezvous with a verified stopped target.
    // `ready` below remains the final Resume barrier.
    if let Some(collector_ready) = collector_ready {
        collector_ready.arm()?;
    }
    Ok(attached)
}

/// A Docker exec target can be announced just before all of its host `/proc`
/// details are coherent.  Retry only the known local-publication observations
/// while the target remains stopped; malformed controller identity is never
/// retried or admitted.
fn verify_stopped_target_until(
    event: &Value,
    deadline: Instant,
    mut verify: impl FnMut(&Value) -> Result<super::target::LocalTarget, String>,
) -> Result<super::target::LocalTarget, String> {
    let retry_deadline = deadline.min(Instant::now() + TARGET_VERIFICATION_RETRY);
    let _verification = STOPPED_TARGET_VERIFICATION
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .map_err(|_| "stopped target verification synchronization failed")?;
    loop {
        match verify(event) {
            Ok(target) => return Ok(target),
            Err(error)
                if super::target::retryable_verification_error(&error)
                    && Instant::now() < retry_deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    }
}

fn preparation_seconds(workload: &str, configured: Option<u64>) -> u64 {
    configured
        .or_else(|| super::preparation_default_seconds(workload))
        .unwrap_or(WORKLOAD_READY_TIMEOUT.as_secs())
}

/// Resolve experimental snapshot tools from the project first, then PATH.
fn snapshot_auxiliary_tools(
    enabled: bool,
    tools_root: &Path,
    search: Option<&std::ffi::OsStr>,
) -> Result<std::collections::BTreeMap<String, PathBuf>, String> {
    let mut tools = std::collections::BTreeMap::new();
    if !enabled {
        return Ok(tools);
    }
    for (name, directory) in [
        ("btrfs", "btrfs"),
        ("mkfs.btrfs", "btrfs"),
        ("rsync", "rsync"),
    ] {
        let local = tools_root.join(directory).join("bin").join(name);
        let executable = if crate::executables::is_executable(&local) {
            Some(local)
        } else {
            crate::tools::host_executable_in_at(Path::new(name), search, tools_root)
        };
        let executable = executable.ok_or_else(|| format!(
            "snapshot workspace needs `{name}`; install btrfs-progs and rsync in the project environment"
        ))?;
        tools.insert(name.into(), executable);
    }
    Ok(tools)
}

fn workload_tool_mode(mode: Option<crate::tape::Mode>) -> &'static str {
    match mode {
        Some(crate::tape::Mode::Record) => "record",
        Some(crate::tape::Mode::Replay) => "replay",
        None => "none",
    }
}

fn controller_command(program: impl AsRef<std::ffi::OsStr>) -> Result<ControllerCommand, String> {
    let mut command = ControllerCommand::new(program)?;
    command.current_dir(crate::project::network_invocation_dir());
    Ok(command)
}

const GUEST_ENVIRONMENT_VARS: &[&str] = &[
    "http_proxy",
    "HTTP_PROXY",
    "https_proxy",
    "HTTPS_PROXY",
    "all_proxy",
    "ALL_PROXY",
    "socks_proxy",
    "SOCKS_PROXY",
    "no_proxy",
    "NO_PROXY",
    "NPM_CONFIG_OFFLINE",
    "ASB_MEASURE_INTERVAL_MS",
];

/// Build the intentionally small environment projection for a workload guest.
/// Adapter declarations add only non-secret invocation controls; credentials
/// continue to travel exclusively in the negotiated connection.
fn guest_environment(
    agent: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> serde_json::Map<String, Value> {
    let adapter_vars = crate::agents::find(agent)
        .map(|adapter| adapter.forwarded_invocation_environment_vars())
        .unwrap_or_default();
    GUEST_ENVIRONMENT_VARS
        .iter()
        .copied()
        .chain(adapter_vars.iter().copied())
        .filter_map(|name| lookup(name).map(|value| (name.into(), Value::String(value))))
        .collect()
}

fn append_progress_transcript(transcript: &mut Vec<u8>, bytes: &[u8]) {
    let remaining = PROGRESS_TRANSCRIPT_LIMIT.saturating_sub(transcript.len());
    transcript.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
}

/// Project paths/proxies cross the workload boundary, host capabilities do not.
fn guest_project_configuration(project: &crate::project::ProjectConfig) -> Result<String, String> {
    #[derive(serde::Serialize)]
    struct GuestProject<'a> {
        tools_dir: &'static str,
        workloads_dir: &'static str,
        workspace_dir: &'static str,
        proxy: &'a crate::project::NetworkProxy,
    }
    toml::to_string(&GuestProject {
        tools_dir: ".asb-tools",
        workloads_dir: "workloads",
        workspace_dir: "workspace",
        proxy: &project.proxy,
    })
    .map_err(|_| "cannot encode guest project configuration".into())
}

/// A workload guest cannot dereference host campaign paths.  Replay aliases
/// are mounted directly beneath this private directory, including each
/// replica's `replay-N.jsonl` and its adjacent local-tool transcript.
fn guest_tape_cassette(path: Option<&Path>) -> Result<Option<PathBuf>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    let name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "tape cassette has no filename for workload guest".to_owned())?;
    Ok(Some(Path::new("/opt/asb-private").join(name)))
}

/// Move an adapter-owned workspace capsule across the guest boundary without
/// interpreting its bytes.  The remote layer only verifies host custody and
/// supplies a fixed private target; the selected adapter owns the format and
/// effects of that payload.
fn stage_native_capsule(
    capsule: Option<crate::workloads::session::NativeCapsule<'_>>,
    artifact_output: &Path,
    private_inputs: &mut Vec<PrivateInput>,
) -> Result<crate::agents::driver::GuestCapsule, String> {
    match capsule {
        Some(crate::workloads::session::NativeCapsule::Record(destination)) => {
            if destination.as_os_str().is_empty() {
                return Err("guest native capsule destination is invalid".into());
            }
            Ok(crate::agents::driver::GuestCapsule {
                mode: crate::agents::driver::GuestCapsuleMode::Record,
                export: Some(crate::agents::driver::GuestCapsuleExport {
                    target: "native-capsule.zip".into(),
                }),
            })
        }
        Some(crate::workloads::session::NativeCapsule::Replay(source)) => {
            let metadata = std::fs::symlink_metadata(source)
                .map_err(|_| "guest native capsule replay is unavailable".to_owned())?;
            if !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() > 16 * 1024 * 1024
            {
                return Err("guest native capsule replay is invalid or exceeds its bound".into());
            }
            let staged = artifact_output.join("native-capsule.zip");
            let mut input = std::fs::File::open(source)
                .map_err(|_| "guest native capsule replay is unavailable".to_owned())?;
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)
                .map_err(|error| format!("cannot stage guest native capsule replay: {error}"))?;
            let mut hash = Sha256::new();
            let mut length = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let read = input
                    .read(&mut buffer)
                    .map_err(|_| "guest native capsule replay is unavailable".to_owned())?;
                if read == 0 {
                    break;
                }
                length += read as u64;
                if length > metadata.len() {
                    return Err("guest native capsule replay changed before staging".into());
                }
                output.write_all(&buffer[..read]).map_err(|error| {
                    format!("cannot stage guest native capsule replay: {error}")
                })?;
                hash.update(&buffer[..read]);
            }
            if length != metadata.len() {
                return Err("guest native capsule replay changed before staging".into());
            }
            private_inputs.push(PrivateInput {
                source: staged,
                target: "native-capsule.zip".into(),
                bytes: length,
                sha256: format!("{:x}", hash.finalize()),
            });
            Ok(crate::agents::driver::GuestCapsule {
                mode: crate::agents::driver::GuestCapsuleMode::Replay,
                export: Some(crate::agents::driver::GuestCapsuleExport {
                    target: "native-capsule.zip".into(),
                }),
            })
        }
        None => Ok(crate::agents::driver::GuestCapsule::default()),
    }
}

fn local_tool_sidecar(path: &Path) -> Result<PathBuf, String> {
    replay_sidecar(path, "local-tools", "jsonl")
}

fn tool_protocol_sidecar(path: &Path) -> Result<PathBuf, String> {
    replay_sidecar(path, "tool-protocol", "json")
}

fn deterministic_manifest_sidecar(path: &Path) -> Result<PathBuf, String> {
    replay_sidecar(path, "determinism-manifest", "json")
}

fn native_trace_sidecar(path: &Path) -> Result<PathBuf, String> {
    replay_sidecar(path, "native-trace", "jsonl")
}

fn replay_sidecar(path: &Path, stem: &str, extension: &str) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "replay cassette has no valid private filename".to_owned())?;
    let suffix = name
        .strip_prefix("replay")
        .and_then(|suffix| suffix.strip_suffix(".jsonl"))
        .ok_or_else(|| "replay cassette has an unexpected private filename".to_owned())?;
    let parent = path
        .parent()
        .ok_or_else(|| "replay cassette has no private directory".to_owned())?;
    Ok(parent.join(format!("{stem}{suffix}.{extension}")))
}

// The adapter derives only the private evidence it needs from the selected
// model track; the generic guest transport does not interpret its contents.
fn stage_adapter_replay_evidence(
    adapter: &dyn crate::agents::AgentAdapter,
    track: Option<&Path>,
    replay: bool,
    artifact_output: &Path,
    inputs: &mut Vec<PrivateInput>,
) -> Result<(), String> {
    if !replay {
        return Ok(());
    }
    let Some(track) = track else {
        return Ok(());
    };
    let Some(bytes) = adapter
        .guest_replay_evidence(track)
        .map_err(|error| format!("cannot prepare adapter replay evidence: {error}"))?
    else {
        return Ok(());
    };
    if bytes.len() as u64 > MAX_PRIVATE_REPLAY_INPUT {
        return Err("adapter replay evidence exceeds its size limit".into());
    }
    let directory = artifact_output.join("adapter-private");
    std::fs::create_dir_all(&directory)
        .map_err(|_| "cannot allocate adapter replay evidence".to_owned())?;
    let source = directory.join("adapter-model-evidence.json");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&source)
        .map_err(|_| "cannot create fresh adapter replay evidence".to_owned())?;
    file.write_all(&bytes)
        .map_err(|_| "cannot write adapter replay evidence".to_owned())?;
    file.sync_all()
        .map_err(|_| "cannot settle adapter replay evidence".to_owned())?;
    inputs.push(PrivateInput {
        source,
        target: "adapter-model-evidence.json".into(),
        bytes: bytes.len() as u64,
        sha256: crate::cassette::digest(&bytes),
    });
    Ok(())
}

fn replay_private_inputs(
    path: Option<&Path>,
    replay: bool,
    mcp_servers: &[crate::benchmark::McpServerConfig],
    mcp_replay: bool,
) -> Result<Vec<PrivateInput>, String> {
    if !replay {
        return Ok(Vec::new());
    }
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let local_tools = local_tool_sidecar(path)?;
    let tool_protocol = tool_protocol_sidecar(path)?;
    let manifest = deterministic_manifest_sidecar(path)?;
    let native_trace = native_trace_sidecar(path)?;
    let mut inputs = Vec::new();
    // Replay extensions are adapter-owned. A sealed record only stages the
    // sidecars it actually published, so a provider-tape-only adapter remains
    // replayable without pretending it supports local-tool, deterministic, or
    // native execution replay.
    let mut stage = |source: PathBuf,
                     description: &str,
                     limit: u64,
                     target_override: Option<&str>|
     -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(&source)
            .map_err(|_| format!("{description} is unavailable"))?;
        if !metadata.file_type().is_file() || metadata.len() > limit {
            return Err(format!(
                "{description} is invalid or exceeds its size limit"
            ));
        }
        let mut file =
            std::fs::File::open(&source).map_err(|_| format!("{description} is unavailable"))?;
        let mut hash = Sha256::new();
        let mut length = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|_| format!("{description} is unavailable"))?;
            if read == 0 {
                break;
            }
            length += read as u64;
            if length > metadata.len() {
                return Err(format!("{description} changed while preparing guest input"));
            }
            hash.update(&buffer[..read]);
        }
        if length != metadata.len() {
            return Err(format!("{description} changed while preparing guest input"));
        }
        let target = target_override.map(str::to_owned).unwrap_or(
            source
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| format!("{description} has no filename"))?
                .to_owned(),
        );
        inputs.push(PrivateInput {
            source,
            target,
            bytes: length,
            sha256: format!("{:x}", hash.finalize()),
        });
        Ok(())
    };
    for (source, description) in [
        (local_tools, "sealed local-tool replay transcript"),
        (tool_protocol, "sealed local-tool protocol identity"),
        (manifest, "sealed deterministic replay manifest"),
    ] {
        match std::fs::symlink_metadata(&source) {
            Ok(_) => stage(source, description, MAX_PRIVATE_REPLAY_INPUT, None)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(format!("{description} is unavailable")),
        }
    }
    match std::fs::symlink_metadata(&native_trace) {
        Ok(_) => {
            let bytes = std::fs::read(&native_trace)
                .map_err(|_| "sealed native replay trace is unavailable".to_owned())?;
            crate::agents::native_trace::canonicalize(&bytes)
                .map_err(|_| "sealed native replay trace is malformed".to_owned())?;
            stage(
                native_trace,
                "sealed native replay trace",
                MAX_PRIVATE_REPLAY_INPUT,
                None,
            )?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("sealed native replay trace is unavailable".into()),
    }
    if mcp_replay {
        for server in mcp_servers {
            let source = crate::agents::mcp_relay::replay_bundle_path(path, &server.id)
                .map_err(|error| error.to_string())?;
            stage(
                source,
                "sealed MCP relay sidecar",
                MAX_PRIVATE_REPLAY_INPUT,
                None,
            )?;
        }
    }
    let snapshots = replay_sidecar(path, "workspace-snapshots", "tar")?;
    match std::fs::symlink_metadata(&snapshots) {
        Ok(_) => stage(
            snapshots,
            "sealed workspace snapshot archive",
            crate::cassette::Limits::default().max_artifact_bytes,
            Some("workspace-snapshots.tar"),
        )?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err("sealed workspace snapshot archive is unavailable".into()),
    }
    Ok(inputs)
}

pub(crate) fn prefetch(context: &super::PreflightContext<'_>) -> Result<(), crate::Error> {
    let tools = crate::project::resolve(context.project_root, &context.project_config.tools_dir);
    crate::install::guest_bootstrap(context.agent, context.project_root, &tools, context.offline)?;
    Ok(())
}

pub(crate) fn verify_retained(
    tool_id: &str,
    module: &str,
    context: &super::recovery::RetainedOutcomeContext<'_>,
) -> Result<(), crate::Error> {
    let recovery = &context.recovery;
    recovery
        .reference
        .validate_location(recovery.project_root)?;
    let tool = crate::project::resolve(recovery.project_root, &recovery.project_config.tools_dir)
        .join(tool_id);
    require_sdk(&tool, &recovery.reference.workload)?;
    let mut command =
        controller_command(tool.join("venv/bin/python")).map_err(crate::Error::Run)?;
    command
        .arg(tool.join("asb_retained.py"))
        .arg("--module")
        .arg(module)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stderr(std::process::Stdio::null());
    let mut controller = Controller::spawn(&mut command).map_err(crate::Error::Run)?;
    let deadline = Instant::now() + INSPECTION_TIMEOUT;
    controller.limit_cleanup_to(deadline);
    controller
        .send(
            &json!({"reference":recovery.reference,"summary":context.summary}),
            deadline,
        )
        .map_err(crate::Error::Run)?;
    let result = controller.receive(deadline).map_err(crate::Error::Run)?;
    if result["ok"] != true
        || result["scope"] != "retained-artifact-consistency-not-historical-attestation"
    {
        let reason = result["reason"]
            .as_str()
            .filter(|reason| reason.len() <= 512 && !reason.chars().any(char::is_control))
            .unwrap_or(
                "retained upstream artifacts do not establish a consistent completed outcome",
            );
        return Err(crate::Error::Input(format!(
            "cannot reconcile retained outcome: {reason}; inspect {} and preserve the original records rather than repeating the task",
            recovery.reference.artifacts.display()
        )));
    }
    controller
        .complete_response(deadline)
        .map_err(crate::Error::Run)
}

pub(crate) fn inspect_grade(
    tool_id: &str,
    module: &str,
    context: &super::recovery::RecoveryContext<'_>,
    runtime: &std::path::Path,
) -> Result<Value, crate::Error> {
    retained_grade_request(tool_id, module, context, runtime, None)
}

pub(crate) fn grade_retained(
    tool_id: &str,
    module: &str,
    context: &super::recovery::RecoveryContext<'_>,
    runtime: &std::path::Path,
    catalog: &std::path::Path,
) -> Result<Value, crate::Error> {
    retained_grade_request(tool_id, module, context, runtime, Some(catalog))
}

fn retained_grade_request(
    tool_id: &str,
    module: &str,
    context: &super::recovery::RecoveryContext<'_>,
    runtime: &std::path::Path,
    grading_catalog: Option<&std::path::Path>,
) -> Result<Value, crate::Error> {
    context.reference.validate_location(context.project_root)?;
    let tool = crate::project::resolve(context.project_root, &context.project_config.tools_dir)
        .join(tool_id);
    require_sdk(&tool, &context.reference.workload)?;
    let mut command =
        controller_command(tool.join("venv/bin/python")).map_err(crate::Error::Run)?;
    command
        .arg(runtime.join("asb_retained.py"))
        .args([
            "--module",
            module,
            if grading_catalog.is_some() {
                "--grade"
            } else {
                "--inspect-grade"
            },
        ])
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stderr(std::process::Stdio::null());
    let mut controller = Controller::spawn(&mut command).map_err(crate::Error::Run)?;
    let deadline = Instant::now()
        + if grading_catalog.is_some() {
            RETAINED_GRADING_TIMEOUT
        } else {
            INSPECTION_TIMEOUT
        };
    controller.limit_cleanup_to(deadline);
    controller
        .send(
            &json!({"reference":context.reference,"catalog":grading_catalog}),
            deadline,
        )
        .map_err(crate::Error::Run)?;
    let report = controller.receive(deadline).map_err(crate::Error::Run)?;
    if report["ok"] == false {
        let error_class = report["error_class"]
            .as_str()
            .filter(|name| {
                !name.is_empty()
                    && name.len() <= 128
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            })
            .unwrap_or("unknown_error");
        let fallback = format!(
            "retained official grade is missing or inconsistent ({error_class}); inspect preserved artifacts"
        );
        let reason = report["reason"]
            .as_str()
            .filter(|reason| reason.len() <= 512 && !reason.chars().any(char::is_control))
            .unwrap_or(&fallback);
        return Err(crate::Error::Input(format!(
            "cannot {} retained grade: {reason}; no agent was run",
            if grading_catalog.is_some() {
                "complete"
            } else {
                "inspect"
            }
        )));
    }
    controller
        .complete_response(deadline)
        .map_err(crate::Error::Run)?;
    validate_inspected_grade(&report, context.reference)?;
    Ok(report)
}

/// Private support copies must not replace an active worker's installed SDK helpers.
pub(crate) struct InspectionRuntime(pub(crate) PathBuf);

impl InspectionRuntime {
    pub(crate) fn new(project_root: &std::path::Path) -> Result<Self, crate::Error> {
        let path = project_root.join(".asb").join(format!(
            "grade-inspection-{}",
            TaskIdentity::local("inspection", None).attempt_id
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).map_err(crate::io_error)?;
        Ok(Self(path))
    }
}

impl Drop for InspectionRuntime {
    fn drop(&mut self) {
        // Only the unique directory successfully created by this guard.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn validate_inspected_grade(
    report: &Value,
    reference: &super::recovery::AttemptReference,
) -> Result<(), crate::Error> {
    let invalid = || {
        crate::Error::Input("retained grade inspection returned an invalid or mismatched report; no outcome was imported".into())
    };
    let task: TaskIdentity =
        serde_json::from_value(report["task"].clone()).map_err(|_| invalid())?;
    let baseline: BaselineEvidence =
        serde_json::from_value(report["workload_baseline"].clone()).map_err(|_| invalid())?;
    let evidence = report["evidence"].as_object().ok_or_else(invalid)?;
    let mut evidence_bytes = 0u64;
    if evidence.is_empty() || evidence.len() > 256 {
        return Err(invalid());
    }
    for (relative, item) in evidence {
        if relative.is_empty()
            || relative.len() > 4096
            || relative.starts_with('/')
            || relative.contains('\\')
            || relative
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || item["sha256"].as_str().is_none_or(|hash| {
                hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err(invalid());
        }
        evidence_bytes = evidence_bytes
            .checked_add(item["bytes"].as_u64().ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        if evidence_bytes > 512 * 1024 * 1024 {
            return Err(invalid());
        }
    }
    if report["schema_version"] != 1
        || report["scope"] != "retained-official-grade-not-execution-recovery"
        || task.workload != reference.workload
        || task.task_id != reference.task_id
        || Some(&reference.dataset) != task.dataset.as_ref()
        || task.attempt_id.len() != 36
        || !task.attempt_id.starts_with("asb-")
        || !task.attempt_id[4..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || !baseline.supports_evaluation(super::SavedEvaluationKind::Known(
            super::KnownEvaluationKind::UpstreamHarness,
        ))
        || report["workload_evaluation"] != super::KnownEvaluationKind::UpstreamHarness.as_str()
        || report["workload_success"].as_bool().is_none()
        || !matches!(
            report["recorded_cleanup"].as_str(),
            Some("removed" | "stopped-preserved")
        )
        || [
            "process_success",
            "exit_code",
            "elapsed_ms",
            "token_usage",
            "steps",
        ]
        .iter()
        .any(|field| !report.get(field).is_some_and(Value::is_null))
    {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn runtime_inputs(
    tool: &str,
    context: &crate::runtime_inputs::Context<'_>,
) -> Result<Vec<crate::runtime_inputs::Dependency>, crate::Error> {
    use crate::runtime_inputs::Dependency;
    use std::path::Path;
    // This controller owns the Python SDK and Docker CLI transport, not
    // benchmark dispatch or the selected agent/provider implementation.
    let mut inputs = vec![
        Dependency::Python {
            path: context.tool_root(tool).join("venv/bin/python"),
            site: true,
        },
        Dependency::program("docker"),
    ];
    if crate::tools::host_executable(Path::new("sudo")).is_some() {
        inputs.push(Dependency::program("sudo"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        for path in [
            ".docker/config.json",
            ".docker/contexts",
            ".docker/cli-plugins",
        ] {
            inputs.push(Dependency::file(Path::new(&home).join(path)));
        }
    }
    for path in [
        "/usr/libexec/docker/cli-plugins",
        "/usr/local/lib/docker/cli-plugins",
        "/usr/lib/docker/cli-plugins",
    ] {
        inputs.push(Dependency::file(path));
    }
    Ok(inputs)
}

struct Session {
    snapshot: bool,
    evaluation_seconds: u64,
    preparation_seconds: u64,
    task_default_execution_seconds: f64,
    process_lifetime: super::session::ProcessLifetime,
    controller: Controller,
    invocation: TaskInvocation,
    baseline: BaselineEvidence,
    initial_material: Result<MaterialIdentities, String>,
    guest_workspace: String,
    agent_timeout: Duration,
    measurement_active: bool,
    preparation_active: bool,
    resume_ack_deadline: Option<Instant>,
    resume_exec_id: Option<Value>,
    resume_target: Option<super::target::LocalTarget>,
    resume_target_sha256: Option<String>,
    cleanup_attempted: bool,
    completed_grade: Option<CompletedGrade>,
    settlement: Option<ResourceSettlement>,
    artifacts: PathBuf,
}

pub(crate) fn require_sdk(root: &std::path::Path, workload: &str) -> Result<(), crate::Error> {
    if !root.join("venv/bin/python").is_file() {
        return Err(crate::Error::Input(format!(
            "the official evaluator for `{workload}` is not installed; run `asb workload install {workload}` before starting the benchmark"
        )));
    }
    Ok(())
}

pub(crate) fn recover(
    tool_id: &str,
    context: &super::recovery::RecoveryContext<'_>,
) -> Result<Value, crate::Error> {
    context.reference.validate_location(context.project_root)?;
    let tools = crate::project::resolve(context.project_root, &context.project_config.tools_dir);
    let tool = tools.join(tool_id);
    require_sdk(&tool, &context.reference.workload)?;
    if !tool.join("asb_resources.py").is_file() {
        return Err(crate::Error::Input("the installed workload recovery helper is missing; reinstall the workload before recovery".into()));
    }
    let mut command =
        controller_command(tool.join("venv/bin/python")).map_err(crate::Error::Run)?;
    command
        .arg(tool.join("asb_resources.py"))
        .arg("--quiesce")
        .arg(&context.reference.artifacts)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stderr(std::process::Stdio::null());
    crate::project::apply_network_proxy(&mut command, context.project_root)?;
    let mut controller = Controller::spawn(&mut command).map_err(crate::Error::Run)?;
    let deadline = Instant::now() + RECOVERY_TIMEOUT;
    controller.limit_cleanup_to(deadline);
    let report = controller.receive(deadline).map_err(crate::Error::Run)?;
    if report["ok"] == false {
        let reason = if report["error_class"] == "RecoveryError" {
            report["reason"]
                .as_str()
                .filter(|reason| reason.len() <= 512 && !reason.contains(['\n', '\r']))
        } else {
            None
        };
        return Err(crate::Error::Run(format!(
            "workload recovery failed: {}",
            reason.unwrap_or(
                "Docker resource inspection or access failed; run `docker info` and inspect the retained attempt journal"
            )
        )));
    }
    controller
        .complete_response(deadline)
        .map_err(crate::Error::Run)?;
    let attempt = report["attempt"].as_str().filter(|attempt| {
        attempt.len() == 36
            && attempt.starts_with("asb-")
            && attempt[4..].bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    if report["schema_version"] != 1
        || attempt.is_none()
        || report["scope"] != "observed-resource-quiescence-not-a-retry-receipt"
        || report["resources"].as_array().is_none_or(|resources| {
            resources.len() > 64
                || resources.iter().any(|resource| {
                    !matches!(
                        resource["status"].as_str(),
                        Some("absent" | "preserved" | "stopped-preserved")
                    ) || !matches!(
                        resource["kind"].as_str(),
                        Some("containers" | "networks" | "volumes" | "images")
                    ) || resource["name"].as_str().is_none_or(|name| {
                        name.len() > 255 || attempt.is_none_or(|attempt| !name.contains(attempt))
                    }) || (resource["kind"] == "images"
                        && resource["status"] != "absent"
                        && resource["image_id"].as_str().is_none_or(|identity| {
                            identity.strip_prefix("sha256:").is_none_or(|hash| {
                                hash.len() != 64
                                    || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                            })
                        }))
                })
        })
    {
        return Err(crate::Error::Run(
            "workload recovery returned an invalid bounded report".into(),
        ));
    }
    Ok(report)
}

pub(crate) fn launch(
    tool_id: &str,
    helper: &str,
    operation: Option<&str>,
    workload: &str,
    dataset: super::session::DatasetIdentity,
    context: &PreparationContext<'_>,
) -> Result<Box<dyn WorkloadSession>, String> {
    launch_inner(
        tool_id, helper, operation, workload, dataset, context, false,
    )
}

/// Adapter opt-in: the workload helper must pass and verify this CPU set for
/// every container it creates before starting task work.
pub(crate) fn launch_with_cpu_allocation(
    tool_id: &str,
    helper: &str,
    operation: Option<&str>,
    workload: &str,
    dataset: super::session::DatasetIdentity,
    context: &PreparationContext<'_>,
) -> Result<Box<dyn WorkloadSession>, String> {
    launch_inner(tool_id, helper, operation, workload, dataset, context, true)
}

fn launch_inner(
    tool_id: &str,
    helper: &str,
    operation: Option<&str>,
    workload: &str,
    dataset: super::session::DatasetIdentity,
    context: &PreparationContext<'_>,
    supports_cpu_allocation: bool,
) -> Result<Box<dyn WorkloadSession>, String> {
    if context.cpu_allocation.is_some() && !supports_cpu_allocation {
        return Err(
            "this workload does not yet support verified managed-container CPU placement".into(),
        );
    }
    let task = context
        .trial
        .ok_or("select an official instance ID in setup before running this benchmark")?;
    if context.trial_set.is_some() {
        crate::workloads::selected_trial(
            workload,
            task,
            context.trial_set,
            context.workload_directory,
        )?;
    }
    let project_root = context.project_root;
    let tool = context.tool_root(tool_id);
    let attempts = project_root.join(".asb/attempts");
    std::fs::create_dir_all(&attempts).map_err(|error| error.to_string())?;
    let preparation_id = TaskIdentity::local(workload, Some(task)).attempt_id;
    let artifacts =
        std::path::absolute(attempts.join(&preparation_id)).map_err(|error| error.to_string())?;
    let reference = super::recovery::AttemptReference {
        schema_version: 1,
        preparation_id,
        workload: workload.into(),
        task_id: task.into(),
        dataset: dataset.clone(),
        artifacts: artifacts.clone(),
    };
    reference
        .validate_location(project_root)
        .map_err(|error| error.to_string())?;
    super::recovery::remember(context.workspace, &reference).map_err(|error| error.to_string())?;
    if let Some(observer) = context.on_allocation {
        observer(&reference)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let error_log = options
        .open(artifacts.with_extension("controller.log"))
        .map_err(|error| error.to_string())?;
    let mut command = controller_command(tool.join("venv/bin/python"))?;
    if let Some(cpus) = context.cpu_allocation {
        let mask = crate::runtime::affinity::AffinityMask::new(cpus)?;
        mask.prepare_command(&mut command)
            .map_err(|error| format!("cannot prepare workload controller CPU affinity: {error}"))?;
        command.env(
            "ASB_CPU_ALLOCATION",
            cpus.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
        );
    } else {
        command.env_remove("ASB_CPU_ALLOCATION");
    }
    command.stderr(error_log);
    if let Some(expected) = context.expected_material_sha256 {
        command.env("ASB_EXPECTED_MATERIAL_SHA256", expected);
    } else {
        command.env_remove("ASB_EXPECTED_MATERIAL_SHA256");
    }
    command.env(
        "ASB_RETAIN_RECORDED_MATERIAL",
        if context.retain_recorded_material {
            "1"
        } else {
            "0"
        },
    );
    command.env(
        "ASB_WORKLOAD_TOOL_MODE",
        workload_tool_mode(context.tool_mode),
    );
    let snapshot = context.native_snapshot_replay
        || context.native_snapshot_record
        || std::env::var_os("ASB_EXPERIMENTAL_SNAPSHOT").as_deref()
            == Some(std::ffi::OsStr::new("1"));
    command.env(
        "ASB_EXPERIMENTAL_SNAPSHOT",
        if snapshot { "1" } else { "0" },
    );
    if let Some(roots) = std::env::var_os("ASB_EXPERIMENTAL_SNAPSHOT_ROOTS") {
        command.env("ASB_EXPERIMENTAL_SNAPSHOT_ROOTS", roots);
    }
    if let Some(root) = std::env::var_os("ASB_EXPERIMENTAL_SNAPSHOT_STORAGE_ROOT") {
        command.env("ASB_EXPERIMENTAL_SNAPSHOT_STORAGE_ROOT", root);
    }
    if std::env::var_os("ASB_EXPERIMENTAL_SNAPSHOT_CUSTODY").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        command.env("ASB_EXPERIMENTAL_SNAPSHOT_CUSTODY", "1");
    }
    command.arg(tool.join(helper));
    if let Some(operation) = operation {
        command.arg(operation);
    }
    command
        .arg("--catalog")
        .arg(
            context
                .workload_directory
                .join(workload)
                .join("catalog.json"),
        )
        .arg("--task")
        .arg(task)
        .arg("--artifacts")
        .arg(&artifacts)
        .env("PYTHONDONTWRITEBYTECODE", "1");
    crate::project::apply_network_proxy(&mut command, project_root)
        .map_err(|error| error.to_string())?;
    let mut controller = Controller::spawn(&mut command)?;
    let preparation_seconds =
        preparation_seconds(workload, context.budgets.environment_preparation_seconds);
    let prepared = (|| -> Result<_, String> {
        let ready =
            controller.receive(Instant::now() + Duration::from_secs(preparation_seconds))?;
        let ready = ReadyTask::decode(&ready, workload, task, &dataset)?;
        let initial_material = if context.verify_initial_material {
            Ok(request_initial_material(&mut controller)?)
        } else {
            Err("initial material identity was not requested for this legacy execution".into())
        };
        Ok((ready, initial_material))
    })();
    let (
        ReadyTask {
            identity,
            prompt,
            guest_workspace,
            baseline,
            agent_timeout,
        },
        initial_material,
    ) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            return Err(match controller.cancel(CANCELLATION_TIMEOUT) {
                Ok(_) => error,
                Err(cleanup) => format!("{error}; workload cancellation cleanup failed: {cleanup}"),
            });
        }
    };
    Ok(Box::new(Session {
        snapshot,
        evaluation_seconds: context
            .budgets
            .evaluation_seconds
            .unwrap_or(EVALUATION_TIMEOUT_SECONDS),
        preparation_seconds,
        task_default_execution_seconds: agent_timeout,
        process_lifetime: super::adapters::find(workload)
            .ok_or("workload adapter disappeared before execution")?
            .process_lifetime(),
        controller,
        invocation: TaskInvocation {
            identity,
            input: prompt.into(),
            workspace: context.workspace.to_path_buf(),
        },
        baseline,
        initial_material,
        guest_workspace,
        agent_timeout: context
            .budgets
            .execution_seconds
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs_f64(agent_timeout)),
        measurement_active: false,
        preparation_active: false,
        resume_ack_deadline: None,
        resume_exec_id: None,
        resume_target: None,
        resume_target_sha256: None,
        cleanup_attempted: false,
        completed_grade: None,
        settlement: None,
        artifacts,
    }))
}

fn request_initial_material(controller: &mut Controller) -> Result<MaterialIdentities, String> {
    request_initial_material_until(controller, Instant::now() + INITIAL_MATERIAL_TIMEOUT)
}

fn request_initial_material_until(
    controller: &mut Controller,
    deadline: Instant,
) -> Result<MaterialIdentities, String> {
    controller.send(
        &Request::InitialMaterial {
            binary: &runtime_binary()?,
        }
        .value()?,
        deadline,
    )?;
    let response = controller.receive(deadline)?;
    if EventKind::of(&response) != Some(EventKind::InitialMaterial) {
        return Err(
            "official workload did not establish prepared material coverage; no agent was started"
                .into(),
        );
    }
    validate_initial_material(&response["material"])
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MaterialIdentities {
    primary: String,
    legacy: Option<String>,
}

fn validate_initial_material(value: &Value) -> Result<MaterialIdentities, String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Material {
        schema_version: u32,
        scope: String,
        sha256: String,
        #[serde(default)]
        legacy_sha256: Option<String>,
    }
    let invalid = || {
        "official prepared material identity has an unsupported schema, scope or digest".to_owned()
    };
    let material: Material = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
    if material.schema_version != 1
        || material.scope != "prepared-workload-material-v1"
        || material.sha256.len() != 64
        || !valid_material_digest(&material.sha256)
        || material
            .legacy_sha256
            .as_deref()
            .is_some_and(|digest| !valid_material_digest(digest))
    {
        return Err(invalid());
    }
    Ok(MaterialIdentities {
        primary: material.sha256,
        legacy: material.legacy_sha256,
    })
}

fn valid_material_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl Session {
    fn cancel(&mut self) -> Result<ResourceDisposition, String> {
        if let Some(settlement) = &self.settlement {
            return Ok(settlement.disposition);
        }
        self.cleanup_attempted = true;
        let disposition = self.controller.cancel(CANCELLATION_TIMEOUT)?;
        self.settlement = Some(ResourceSettlement {
            disposition,
            diagnostic: None,
        });
        Ok(disposition)
    }

    fn begin_agent_preparation(
        &mut self,
        context: &mut ExecutionContext<'_>,
    ) -> Result<(), String> {
        // Launch admission belongs to resume_measured_target: only a verified,
        // still-stopped target may arrive at the shared release barrier.
        // Preparation itself must not consume a launch-gate arrival.
        if let Some(session) = context.measurements.as_deref_mut() {
            session.event(crate::collectors::LifecycleEvent::PhaseBegin(
                crate::collectors::LifecyclePhase::AgentPreparation,
            ))?;
            self.preparation_active = true;
        }
        Ok(())
    }

    fn run(
        &mut self,
        context: &mut ExecutionContext<'_>,
        output: &mut dyn std::io::Write,
    ) -> Result<ExecutionResult, String> {
        let connection = context
            .connection
            .ok_or("isolated execution needs a negotiated API connection")?;
        self.begin_agent_preparation(context)?;
        let prepared = PreparedBootstrap::prepare(context)?;
        self.run_prepared(context, output, connection, prepared)
    }

    fn run_prepared(
        &mut self,
        context: &mut ExecutionContext<'_>,
        output: &mut dyn std::io::Write,
        connection: &crate::endpoint::Connection,
        prepared: PreparedBootstrap,
    ) -> Result<ExecutionResult, String> {
        let preparation = context
            .launch
            .map(|launch| launch.remaining())
            .transpose()?
            .unwrap_or_default();
        let allowance = if context.launch.is_some() {
            Duration::from_secs(20)
        } else {
            AGENT_TRANSPORT_ALLOWANCE
        };
        let now = Instant::now();
        let deadline = now + preparation + self.agent_timeout + allowance;
        let dispatch_deadline = if context.launch.is_some() {
            now + preparation
        } else {
            deadline
        };
        self.dispatch_agent(context, connection, prepared, dispatch_deadline)?;
        self.receive_execution(context, output, deadline)
    }

    fn dispatch_agent(
        &mut self,
        context: &mut ExecutionContext<'_>,
        connection: &crate::endpoint::Connection,
        prepared: PreparedBootstrap,
        deadline: Instant,
    ) -> Result<(), String> {
        let PreparedBootstrap {
            binary,
            tools_root,
            installation: bootstrap,
        } = prepared;
        let mut config = context.config.clone();
        config.workspace = self.guest_workspace.clone();
        config.concurrency = 1;
        let namespace_collectors = context
            .measurements
            .as_deref_mut()
            .map(|session| session.execution_namespace_collectors())
            .unwrap_or_default();
        let environment = guest_environment(&context.config.agent, |name| std::env::var(name).ok());
        let tape_cassette = guest_tape_cassette(context.tape_cassette)?;
        let tool_tape_cassette = guest_tape_cassette(context.tool_tape_cassette)?;
        let mcp_tape_cassette = guest_tape_cassette(context.mcp_tape_cassette)?;
        let artifact_output = self.artifacts.join("guest-artifacts");
        std::fs::create_dir_all(&artifact_output)
            .map_err(|error| format!("cannot allocate guest artifact export directory: {error}"))?;
        let mut private_inputs = replay_private_inputs(
            context.mcp_tape_cassette.or(context.tool_tape_cassette),
            context.tool_replay || context.mcp_mode == Some(crate::tape::Mode::Replay),
            &context.config.mcp_servers,
            context.mcp_mode == Some(crate::tape::Mode::Replay),
        )?;
        let capsule = stage_native_capsule(
            context.native_capsule(),
            &artifact_output,
            &mut private_inputs,
        )?;
        let native_tool_launch = context
            .native_tool()
            .map(|(launch, source)| {
                launch
                    .validate_staged(source)
                    .map_err(|error| format!("sealed native tool sidecar is invalid: {error}"))?;
                let bytes = std::fs::read(source)
                    .map_err(|_| "sealed native tool sidecar is unavailable".to_owned())?;
                private_inputs.push(PrivateInput {
                    source: source.to_path_buf(),
                    target: "native-tool-sidecar.jsonl".into(),
                    bytes: bytes.len() as u64,
                    sha256: crate::cassette::digest(&bytes),
                });
                Ok::<crate::cassette::NativeToolLaunch, String>(launch.clone())
            })
            .transpose()?;
        // Adapter extensions may require a tiny static helper inside the guest
        // runtime.  Stage declared opaque bytes without interpreting their
        // protocol; the adapter alone selects the guest-relative target.
        let adapter = crate::agents::find(&context.config.agent)
            .ok_or_else(|| "selected agent has no installation adapter".to_owned())?;
        stage_adapter_replay_evidence(
            adapter,
            context.tape_cassette,
            context.tool_replay && native_tool_launch.is_some(),
            &artifact_output,
            &mut private_inputs,
        )?;
        for asset in adapter.guest_private_assets() {
            let target = std::path::Path::new(asset.target);
            if target.is_absolute()
                || target.components().any(|part| {
                    matches!(
                        part,
                        std::path::Component::ParentDir | std::path::Component::RootDir
                    )
                })
            {
                return Err("adapter guest asset has an unsafe target".into());
            }
            let source = artifact_output.join("adapter-private").join(target);
            let parent = source
                .parent()
                .ok_or_else(|| "adapter guest asset has no parent".to_owned())?;
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot stage adapter guest asset: {error}"))?;
            std::fs::write(&source, asset.bytes)
                .map_err(|error| format!("cannot stage adapter guest asset: {error}"))?;
            private_inputs.push(PrivateInput {
                source,
                target: asset.target.into(),
                bytes: asset.bytes.len() as u64,
                sha256: crate::cassette::digest(asset.bytes),
            });
        }
        let payload = AgentRequest {
            binary: &binary,
            project_root: context.project_root,
            preparation_seconds: context
                .launch
                .map(|launch| launch.remaining().map(|remaining| remaining.as_secs_f64()))
                .transpose()?,
            timeout: self.agent_timeout.as_secs_f64(),
            idle_timeout_seconds: self
                .preparation_seconds
                .saturating_add(self.agent_timeout.as_secs())
                .saturating_add(COHORT_IDLE_ALLOWANCE.as_secs()),
            tools_root: &tools_root,
            bootstrap: &bootstrap,
            measure_target: context.measurements.is_some(),
            project_configuration: guest_project_configuration(context.project_config)?,
            environment,
            network: std::env::var("ASB_BENCHMARK_NETWORK").ok(),
            artifact_output: &artifact_output,
            private_inputs,
            auxiliary_tools: snapshot_auxiliary_tools(
                self.snapshot && context.tool_tape_cassette.is_some(),
                &tools_root,
                std::env::var_os("PATH").as_deref(),
            )?,
            request: json!({"process_lifetime":self.process_lifetime,"config":config,"connection":connection,"prompt":self.invocation.input.instructions,"timeout_seconds":self.agent_timeout.as_secs_f64(),"max_steps":context.max_steps,
                "project_root":"/opt/asb-project","runtime_dir":"/opt/asb-private","run_dir":"/opt/asb-private/attempt","replay":context.replay,
                "tape_cassette":tape_cassette,
                "tool_replay":context.tool_replay,
                "ignore_tool_mismatch":context.ignore_tool_mismatch,
                "ignore_mcp_mismatch":context.ignore_mcp_mismatch,
                "tool_tape_cassette":tool_tape_cassette,
                "mcp_mode":context.mcp_mode,
                "mcp_tape_cassette":mcp_tape_cassette,
                "native_tool_launch":native_tool_launch,
                "capsule":capsule,
                "namespace_collectors":namespace_collectors}),
        };
        self.controller.send(
            &Request::Agent {
                payload: Box::new(payload),
            }
            .value()?,
            deadline,
        )
    }

    fn receive_execution(
        &mut self,
        context: &mut ExecutionContext<'_>,
        output: &mut dyn std::io::Write,
        mut deadline: Instant,
    ) -> Result<ExecutionResult, String> {
        let mut collector_errors = Vec::new();
        let mut output_error = None;
        let mut launched = false;
        let mut preparation_failed = false;
        let mut progress_stdout = Vec::new();
        loop {
            let response_deadline = self
                .resume_ack_deadline
                .map_or(deadline, |ack| ack.min(deadline));
            let poll_deadline = response_deadline.min(Instant::now() + PROGRESS_POLL);
            let event = self
                .controller
                .receive_until(poll_deadline)
                .map_err(|error| {
                    output_error.as_ref().map_or_else(
                        || error.clone(),
                        |presentation| format!("{error}; {presentation}"),
                    )
                })?;
            let Some(event) = event else {
                if self.resume_ack_deadline.is_some() && Instant::now() >= response_deadline {
                    return Err(
                        "isolated execution did not report accepted release before effective resume verification deadline".into(),
                    );
                }
                if Instant::now() >= deadline {
                    let error = "workload controller exceeded its response deadline";
                    return Err(output_error.as_ref().map_or_else(
                        || error.into(),
                        |presentation| format!("{error}; {presentation}"),
                    ));
                }
                if launched && let Some(progress) = context.progress.as_deref_mut() {
                    progress.update(&progress_stdout, &[]);
                }
                continue;
            };
            match EventKind::of(&event) {
                Some(EventKind::Paused) => {
                    if launched {
                        return Err("isolated execution returned a second launch target".into());
                    }
                    self.resume_measured_target(context, &event, deadline)?;
                    launched = true;
                    // Recording and replay can be silent until the first
                    // provider response. Show the same immediate live frame
                    // as a native run, then let the timed polling refresh it.
                    if let Some(progress) = context.progress.as_deref_mut() {
                        progress.update(&progress_stdout, &[]);
                    }
                }
                Some(EventKind::ReleaseAccepted) => {
                    self.verify_effective_resume(context, &event)?
                }
                Some(EventKind::PreparationFailed) => {
                    preparation_failed = true;
                    // The task emits its bounded error immediately after this
                    // marker. Keep its controller open until that error is
                    // received; aborting peers here closes the controller pipe
                    // and turns the original failure into a disconnect.
                    deadline = deadline.min(Instant::now() + CANCELLATION_TIMEOUT);
                }
                Some(EventKind::Output) => {
                    self.require_resume_acknowledged()?;
                    let bytes = decode_base64(&event["data"])?;
                    append_progress_transcript(&mut progress_stdout, &bytes);
                    if launched && let Some(progress) = context.progress.as_deref_mut() {
                        progress.update(&progress_stdout, &[]);
                    }
                    if context.show_output
                        && output_error.is_none()
                        && let Err(error) = output.write_all(&bytes).and_then(|()| output.flush())
                    {
                        output_error = Some(format!("cannot write agent output: {error}"));
                    }
                }
                Some(EventKind::ExecutionEnded) => {
                    self.require_resume_acknowledged()?;
                    if let Some(session) = context.measurements.as_deref_mut() {
                        if !self.measurement_active {
                            return Err(
                                "isolated execution ended without an active measured target".into(),
                            );
                        }
                        for event in [
                            crate::collectors::LifecycleEvent::AgentExecuteEnd,
                            crate::collectors::LifecycleEvent::AgentExited,
                        ] {
                            if let Err(error) = session.event(event) {
                                collector_errors.push(error);
                            }
                        }
                        self.measurement_active = false;
                    }
                }
                Some(EventKind::Completed) => {
                    self.require_resume_acknowledged()?;
                    if preparation_failed {
                        return Err("isolated preparation failed before shared launch; inspect private task artifacts".into());
                    }
                    if context.launch.is_some() && !launched {
                        return Err("isolated execution omitted shared launch readiness".into());
                    }
                    let mut result = self.completed_execution(context, &event)?;
                    result.collector_errors.extend(collector_errors);
                    result.output_error = output_error;
                    return Ok(result);
                }
                _ => return Err("unexpected isolated execution event".into()),
            }
        }
    }

    fn resume_measured_target(
        &mut self,
        context: &mut ExecutionContext<'_>,
        event: &Value,
        deadline: Instant,
    ) -> Result<(), String> {
        let mut verified_target = None;
        self.resume_target = None;
        if context.launch.is_some() {
            let target = &event["target"];
            let valid_hex = |value: &Value| {
                value.as_str().is_some_and(|text| {
                    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            };
            if !valid_hex(&target["container_id"])
                || !valid_hex(&target["exec_id"])
                || !target["namespace_pid"]
                    .as_u64()
                    .is_some_and(|pid| pid > 1 && pid <= u32::MAX as u64)
                || !matches!(
                    target["pid_scope"].as_str(),
                    Some("docker-daemon-host" | "container-namespace")
                )
            {
                let error = "isolated launch gate returned invalid stopped-target identity";
                if let Some(session) = context.measurements.as_deref_mut() {
                    write_launch_gate_failure(session, error, "target-identity")?;
                }
                return Err(error.into());
            }
        }
        if let Some(session) = context.measurements.as_deref_mut() {
            use crate::collectors::LifecycleEvent as Event;
            if self.measurement_active {
                return Err("isolated execution returned a second measurement target".into());
            }
            let (target, target_sha256) =
                match attach_target_before_launch(context.launch, context.collector_ready, || {
                    let target = match verify_stopped_target_until(
                        &event["target"],
                        deadline,
                        super::target::verify,
                    ) {
                        Ok(target) => target,
                        Err(error) => {
                            write_target_verification_failure(session, &error)?;
                            return Err(error);
                        }
                    };
                    let target_evidence = json!({
                        "schema_version":1,"scope":"verified-local-isolated-process-tree",
                        "host_pid":target.pid,"namespace_pid":target.namespace_pid,
                        "start_time_ticks":target.start_time_ticks,"cgroup":target.cgroup,
                        "cgroup_membership":"preserved"
                    });
                    let target_sha256 = target_evidence_digest(&target_evidence)?;
                    crate::measurements::write_json(
                        &session.dir.join("execution-target.json"),
                        &target_evidence,
                    )?;
                    if let Err(error) =
                        session.attach_isolated_pid(target.pid, target.start_time_ticks)
                    {
                        session.event(Event::AgentAttachFailed)?;
                        return Err(error);
                    }
                    write_cohort_release_receipt(session, &target_sha256, "collector_attached")?;
                    Ok((target, target_sha256))
                }) {
                    Ok(target) => target,
                    Err(error) => {
                        write_launch_gate_failure(session, &error, "target-attachment")?;
                        return Err(error);
                    }
                };
            self.resume_target = Some(target);
            verified_target = Some(target_sha256);
        } else if let Some(launch) = context.launch {
            launch.ready()?;
            if let Some(collector_ready) = context.collector_ready {
                collector_ready.arm()?;
            }
        }
        if let Some(session) = context.measurements.as_deref_mut() {
            use crate::collectors::LifecycleEvent as Event;
            let phase_end = Event::PhaseEnd(crate::collectors::LifecyclePhase::AgentPreparation);
            if let Err(error) = session.event(phase_end) {
                write_launch_gate_failure(session, &error, "lifecycle")?;
                return Err(error);
            }
            self.preparation_active = false;
            for event in [Event::MeasurementsReady, Event::AgentSpawn] {
                if let Err(error) = session.event(event) {
                    write_launch_gate_failure(session, &error, "lifecycle")?;
                    return Err(error);
                }
            }
        }
        // The target remains stopped through collector binding. Resume is the
        // final common release: every admitted peer now has an equivalent
        // verified target and a bound collector set. Capture activation waits
        // for the workload-owned effective-release acknowledgement below.
        if let Some(collector_ready) = context.collector_ready
            && let Err(error) = collector_ready.ready()
        {
            if let Some(session) = context.measurements.as_deref_mut() {
                write_launch_gate_failure(session, &error, "collector-release")?;
            }
            return Err(error);
        }
        if let (Some(session), Some(target_sha256)) = (
            context.measurements.as_deref_mut(),
            verified_target.as_ref(),
        ) {
            write_cohort_release_receipt(session, target_sha256, "collector_released")?;
        }
        if let Err(error) = self.controller.send(
            &Request::Resume {
                exec_id: &event["target"]["exec_id"],
            }
            .value()?,
            deadline,
        ) {
            if let Some(session) = context.measurements.as_deref_mut() {
                write_launch_gate_failure(session, &error, "resume-dispatch")?;
            }
            return Err(error);
        }
        if let (Some(session), Some(target_sha256)) = (
            context.measurements.as_deref_mut(),
            verified_target.as_ref(),
        ) {
            write_cohort_release_receipt(session, target_sha256, "resume_sent")?;
        }
        self.resume_ack_deadline = Some((Instant::now() + RESUME_ACK_TIMEOUT).min(deadline));
        self.resume_exec_id = Some(event["target"]["exec_id"].clone());
        self.resume_target_sha256 = verified_target;
        Ok(())
    }

    fn require_resume_acknowledged(&self) -> Result<(), String> {
        if self.resume_ack_deadline.is_some() {
            return Err(
                "isolated target emitted execution data before effective resume acknowledgement"
                    .into(),
            );
        }
        Ok(())
    }

    fn verify_effective_resume(
        &mut self,
        context: &mut ExecutionContext<'_>,
        event: &Value,
    ) -> Result<(), String> {
        self.verify_effective_resume_with(
            context,
            event,
            super::target::verify_resumed,
            crate::interruption::requested,
        )
    }

    fn verify_effective_resume_with(
        &mut self,
        context: &mut ExecutionContext<'_>,
        event: &Value,
        verify: impl Fn(&super::target::LocalTarget) -> Result<(), String>,
        interrupted: impl Fn() -> bool,
    ) -> Result<(), String> {
        let expected = self
            .resume_exec_id
            .as_ref()
            .ok_or("isolated execution accepted release without a pending target")?;
        if event["exec_id"] != *expected {
            return Err("isolated execution accepted release for a different target".into());
        }
        let deadline = self
            .resume_ack_deadline
            .ok_or("isolated execution accepted release without a pending deadline")?;
        if let Some(target) = self.resume_target.as_ref() {
            loop {
                match verify(target) {
                    Ok(()) => break,
                    Err(_) if interrupted() => {
                        return Err("ASB interruption requested controlled workload cleanup".into());
                    }
                    Err(_error) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        // The controller request alone does not prove that the stopped target
        // has run. Its workload-owned acknowledgement and host identity check
        // above are the execution-release boundary for snapshots, lifecycle
        // timing, kernel-pressure capture and tracing collectors.
        if let Some(session) = context.measurements.as_deref_mut() {
            let target = self
                .resume_target
                .as_ref()
                .ok_or("isolated execution lost its verified target before collector activation")?;
            session.activate_isolated_collectors(target.pid, target.start_time_ticks)?;
            use crate::collectors::LifecycleEvent as Event;
            session.event(Event::AgentExecuteBegin)?;
            self.measurement_active = true;
            session.event(Event::AgentStarted)?;
        }
        self.resume_exec_id.take();
        self.resume_ack_deadline.take();
        self.resume_target.take();
        if let (Some(session), Some(target_sha256)) = (
            context.measurements.as_deref_mut(),
            self.resume_target_sha256.take(),
        ) {
            write_cohort_release_receipt(session, &target_sha256, "resume_acknowledged")?;
        }
        Ok(())
    }

    fn completed_execution(
        &mut self,
        context: &mut ExecutionContext<'_>,
        event: &Value,
    ) -> Result<ExecutionResult, String> {
        if self.measurement_active {
            return Err("isolated execution omitted its completion boundary".into());
        }
        let completed = CompletedExecution::decode_with_artifacts(
            event,
            self.process_lifetime,
            Some(&self.artifacts.join("guest-artifacts")),
        )?;
        let collector_errors = context
            .measurements
            .as_deref_mut()
            .and_then(|session| {
                session
                    .import_execution_namespace_state(&completed.metadata().namespace_collectors)
                    .err()
            })
            .into_iter()
            .collect();
        let mut result = completed.into_execution()?;
        if self.snapshot && context.tool_tape_cassette.is_some() && !context.tool_replay {
            let archive = self.artifacts.join("snapshot-streams.tar");
            if !archive.is_file() {
                return Err("snapshot stream archive is missing from controller artifacts".into());
            }
            result.metadata.snapshot_artifact = Some(archive);
        }
        result.collector_errors = collector_errors;
        Ok(result)
    }

    fn settle_execution(
        &mut self,
        context: &mut ExecutionContext<'_>,
        mut result: Result<ExecutionResult, String>,
    ) -> Result<ExecutionResult, String> {
        if result.is_err() {
            abort_launch_boundaries(context);
        }
        if result.is_err() && self.preparation_active {
            self.preparation_active = false;
            if let Some(session) = context.measurements.as_deref_mut() {
                let _ = session.event(crate::collectors::LifecycleEvent::PhaseFailed(
                    crate::collectors::LifecyclePhase::AgentPreparation,
                ));
            }
        }
        if result.is_err() && self.measurement_active {
            self.measurement_active = false;
            if let Some(session) = context.measurements.as_deref_mut() {
                let _ = session.event(crate::collectors::LifecycleEvent::AgentExecuteEnd);
                let _ = session.event(crate::collectors::LifecycleEvent::AgentWaitFailed);
            }
        }
        if let Err(error) = &mut result
            && let Err(cleanup) = self.cancel()
        {
            error.push_str(&format!(
                "; workload cancellation cleanup failed: {cleanup}"
            ));
        }
        result
    }
}

fn write_cohort_release_receipt(
    session: &crate::measurements::Session,
    target_sha256: &str,
    stage: &'static str,
) -> Result<(), String> {
    crate::measurements::write_json(
        &session.dir.join(format!("cohort-{stage}.json")),
        &json!({
            "schema_version": 1,
            "kind": "cohort_release",
            "stage": stage,
            "scope": "verified-local-isolated-process-tree",
            "execution_target_sha256": target_sha256,
        }),
    )
}

fn target_evidence_digest(target: &Value) -> Result<String, String> {
    crate::evidence::json_bytes(target).map(|bytes| crate::cassette::digest(&bytes))
}

/// Stable, non-sensitive classification for a stopped-target observation.
/// Never persist the verifier's text: it may contain an implementation detail
/// while the category is sufficient to repair admission behavior.
fn target_verification_code(error: &str) -> &'static str {
    match error {
        "isolated target is not proven to belong to this host; remote Docker measurement is not supported"
        | "isolated target driver identity is invalid"
        | "isolated measurement target has invalid identity fields" => "host-identity",
        "isolated target lacks namespace PID evidence"
        | "isolated target has invalid namespace PID evidence"
        | "invalid isolated process start time"
        | "isolated target is stale, not stopped, or does not match this host PID namespace" => {
            "namespace-identity"
        }
        "isolated measurement requires identifiable cgroup-v2 membership"
        | "isolated target does not match its owned container cgroup or changed during verification"
        | "isolated driver does not belong to its owned container" => "container-cgroup",
        "isolated target candidate scan exceeded its deadline"
        | "isolated target candidate scan exceeded its bound"
        | "isolated target process tree exceeds its bound"
        | "isolated target task list is invalid"
        | "isolated target children list is invalid"
        | "isolated stopped target identity is ambiguous"
        | "cannot identify the stopped agent under its owned driver"
        | "isolated driver changed during target resolution" => "candidate-resolution",
        _ => "other",
    }
}

/// Stable, non-sensitive classification for any shared launch-gate rejection.
/// This covers the wire-identity check which precedes target verification and
/// admission failures raised while binding the collector set.
fn launch_gate_failure_code(error: &str) -> &'static str {
    match error {
        "isolated launch gate returned invalid stopped-target identity" => {
            "invalid-target-identity"
        }
        "shared launch preparation deadline exceeded"
        | "replica failed before shared launch readiness" => "shared-readiness",
        _ => target_verification_code(error),
    }
}

fn write_target_verification_failure(
    session: &crate::measurements::Session,
    error: &str,
) -> Result<(), String> {
    crate::measurements::write_json(
        &session.dir.join("target-verification-failure.json"),
        &json!({
            "schema_version": 1,
            "scope": "isolated-target-verification-failure-v1",
            "failure_code": target_verification_code(error),
        }),
    )
}

fn write_launch_gate_failure(
    session: &crate::measurements::Session,
    error: &str,
    stage: &'static str,
) -> Result<(), String> {
    crate::measurements::write_json(
        &session.dir.join("launch-gate-failure.json"),
        &json!({
            "schema_version": 1,
            "scope": "isolated-launch-gate-failure-v1",
            "failure_code": launch_gate_failure_code(error),
            "stage": stage,
        }),
    )
}

fn abort_launch_boundaries(context: &ExecutionContext<'_>) {
    if let Some(launch) = context.launch {
        launch.abort();
    }
    if let Some(collector_ready) = context.collector_ready {
        collector_ready.abort();
    }
}

/// Installation preparation is complete before protocol execution begins.
/// This is private data, never a CLI/configuration bypass for prefetch checks.
struct PreparedBootstrap {
    binary: PathBuf,
    tools_root: PathBuf,
    installation: crate::install::GuestBootstrap,
}

impl PreparedBootstrap {
    fn prepare(context: &ExecutionContext<'_>) -> Result<Self, String> {
        let binary = runtime_binary()?;
        let tools_root =
            crate::project::resolve(context.project_root, &context.project_config.tools_dir);
        let installation = crate::install::guest_bootstrap(
            &context.config.agent,
            context.project_root,
            &tools_root,
            context.replay,
        )
        .map_err(|error| {
            format!("cannot prepare selected agent installation artifacts: {error}")
        })?;
        Ok(Self {
            binary,
            tools_root,
            installation,
        })
    }
}

fn runtime_binary() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("ASB_GUEST_BINARY") {
        return Ok(PathBuf::from(path));
    }
    // Pin the actual running executable, even if an update atomically replaces
    // its pathname while the SDK prepares the task. Numeric PID is essential:
    // /proc/self/exe would refer to the Python controller when it reads it.
    #[cfg(target_os = "linux")]
    return Ok(PathBuf::from(format!("/proc/{}/exe", std::process::id())));
    #[cfg(not(target_os = "linux"))]
    std::env::current_exe().map_err(|_| "cannot find portable ASB runtime".into())
}

impl WorkloadSession for Session {
    fn prepare_snapshot_replay(&self) -> Result<(), String> {
        if self.snapshot {
            Ok(())
        } else {
            Err("the workload session did not provision a snapshot replay backend".into())
        }
    }

    fn uses_isolated_guest(&self) -> bool {
        true
    }

    fn initial_material_sha256(&self) -> Result<String, String> {
        self.initial_material
            .clone()
            .map(|material| material.primary)
    }
    fn legacy_initial_material_sha256(&self) -> Result<Option<String>, String> {
        self.initial_material
            .clone()
            .map(|material| material.legacy)
    }
    fn cancel_prepared(&mut self) -> Result<(), String> {
        self.cancel().map(|_| ())
    }
    fn effective_budgets(&self) -> crate::planning::EffectiveBudgets {
        crate::planning::EffectiveBudgets {
            environment_preparation_seconds: Some(self.preparation_seconds),
            agent_preparation_seconds: None,
            execution_seconds: Some(self.agent_timeout.as_secs_f64()),
            evaluation_seconds: Some(self.evaluation_seconds),
            task_default_execution_seconds: Some(self.task_default_execution_seconds),
        }
    }
    fn checkpoint_evidence(&self) -> Vec<PathBuf> {
        vec![self.artifacts.clone()]
    }
    fn finish(&mut self) -> Result<ResourceSettlement, String> {
        if let Some(settlement) = &self.settlement {
            return Ok(settlement.clone());
        }
        let Some(grade) = self.completed_grade.take() else {
            self.cancel()?;
            return self
                .settlement
                .clone()
                .ok_or("workload settlement was not confirmed".into());
        };
        self.cleanup_attempted = true;
        match self.controller.finish(grade, CANCELLATION_TIMEOUT) {
            Ok(settlement) => {
                self.settlement = Some(settlement.clone());
                Ok(settlement)
            }
            Err(error) => match self.cancel() {
                Ok(disposition) => {
                    let settlement = ResourceSettlement {
                        disposition,
                        diagnostic: Some(error),
                    };
                    self.settlement = Some(settlement.clone());
                    Ok(settlement)
                }
                Err(cleanup) => Err(format!(
                    "{error}; workload cancellation cleanup failed: {cleanup}"
                )),
            },
        }
    }
    fn invocation(&self) -> &TaskInvocation {
        &self.invocation
    }
    fn evaluation_kind(&self) -> super::KnownEvaluationKind {
        super::KnownEvaluationKind::UpstreamHarness
    }
    fn baseline_passed(&self) -> Option<bool> {
        None
    }
    fn baseline_evidence(&self) -> BaselineEvidence {
        self.baseline.clone()
    }
    fn execute(
        &mut self,
        context: &mut ExecutionContext<'_>,
        output: &mut dyn std::io::Write,
    ) -> Option<Result<ExecutionResult, String>> {
        let result = self.run(context, output);
        Some(self.settle_execution(context, result))
    }
    fn evaluate(&mut self, _response: &str) -> Result<Evaluation, String> {
        self.completed_grade = None;
        let event = self.controller.request(
            Request::Evaluate {
                timeout: self.evaluation_seconds,
            }
            .value()?,
            EventKind::Evaluated,
            Duration::from_secs(self.evaluation_seconds) + EVALUATION_PROTOCOL_ALLOWANCE,
        )?;
        let grade = CompletedGrade::decode(&event, &self.invocation.identity)?;
        let evaluation = grade.evaluation();
        self.completed_grade = Some(grade);
        Ok(evaluation)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Preparation can succeed before provider/proxy setup fails. Those
        // callers never reach execute/finish; still give the owner its full
        // bounded teardown budget instead of the controller's kill fallback.
        // Explicit paths report failures; Drop cannot replace a caller error.
        if !self.cleanup_attempted {
            let _ = self.cancel();
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/remote.rs"]
mod coverage_tests;

#[cfg(test)]
#[path = "../../tests/unit/remote_execution.rs"]
mod execution_tests;

#[cfg(test)]
mod tests {
    use crate::workloads::session::LaunchReadiness;

    struct OrderedLaunch<'a> {
        events: &'a std::cell::RefCell<Vec<&'static str>>,
        name: &'static str,
    }

    impl crate::workloads::session::LaunchReadiness for OrderedLaunch<'_> {
        fn ready(&self) -> Result<(), String> {
            self.events.borrow_mut().push(self.name);
            Ok(())
        }

        fn abort(&self) {
            self.events.borrow_mut().push("abort");
        }

        fn arm(&self) -> Result<(), String> {
            self.events.borrow_mut().push("collector-arm");
            Ok(())
        }

        fn remaining(&self) -> Result<std::time::Duration, String> {
            Ok(std::time::Duration::from_secs(1))
        }
    }

    #[test]
    fn paused_target_attaches_collectors_before_shared_launch_rendezvous() {
        let events = std::cell::RefCell::new(Vec::new());
        let launch = OrderedLaunch {
            events: &events,
            name: "launch",
        };
        let collector_ready = OrderedLaunch {
            events: &events,
            name: "collector-ready",
        };
        super::attach_target_before_launch(Some(&launch), Some(&collector_ready), || {
            events.borrow_mut().push("attached");
            Ok(())
        })
        .unwrap();
        collector_ready.ready().unwrap();
        assert_eq!(
            *events.borrow(),
            ["attached", "launch", "collector-arm", "collector-ready"]
        );
    }

    #[test]
    fn stopped_target_verification_retries_only_transient_local_observations() {
        let target = serde_json::json!({"fixture": true});
        let attempts = std::cell::Cell::new(0);
        let result = super::verify_stopped_target_until(
            &target,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            |_| {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err("cannot identify the stopped agent under its owned driver".into())
                } else {
                    Ok(crate::workloads::target::LocalTarget {
                        pid: 2,
                        namespace_pid: 2,
                        start_time_ticks: 1,
                        cgroup: std::path::PathBuf::from("/sys/fs/cgroup/fixture"),
                    })
                }
            },
        )
        .unwrap();
        assert_eq!(attempts.get(), 2);
        assert_eq!(result.pid, 2);

        let attempts = std::cell::Cell::new(0);
        let error = super::verify_stopped_target_until(
            &target,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            |_| {
                attempts.set(attempts.get() + 1);
                Err("isolated measurement target has invalid identity fields".into())
            },
        )
        .unwrap_err();
        assert_eq!(
            error,
            "isolated measurement target has invalid identity fields"
        );
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn stopped_target_verification_failure_codes_are_stable_and_non_sensitive() {
        assert_eq!(
            super::target_verification_code(
                "isolated target does not match its owned container cgroup or changed during verification"
            ),
            "container-cgroup"
        );
        assert_eq!(
            super::target_verification_code("isolated stopped target identity is ambiguous"),
            "candidate-resolution"
        );
        assert_eq!(
            super::target_verification_code("secret detail must not be persisted"),
            "other"
        );
        assert_eq!(
            super::launch_gate_failure_code(
                "isolated launch gate returned invalid stopped-target identity"
            ),
            "invalid-target-identity"
        );
        assert_eq!(
            super::launch_gate_failure_code("replica failed before shared launch readiness"),
            "shared-readiness"
        );
    }

    #[test]
    fn controller_uses_workload_preparation_default_and_explicit_override() {
        assert_eq!(super::preparation_seconds("swe-bench", None), 7_320);
        assert_eq!(
            super::preparation_seconds("swe-bench-verified", None),
            7_320
        );
        assert_eq!(super::preparation_seconds("swe-bench", Some(120)), 120);
        assert_eq!(
            super::preparation_seconds("unknown", None),
            super::WORKLOAD_READY_TIMEOUT.as_secs()
        );
    }
    #[test]
    fn real_claude_replay_evidence_opt_in() {
        let Some(track) = std::env::var_os("ASB_TEST_CLAUDE_TRACK") else {
            return;
        };
        let output = std::env::var_os("ASB_TEST_CLAUDE_EVIDENCE_ROOT")
            .expect("explicit evidence output is required");
        let root = std::path::PathBuf::from(output);
        std::fs::create_dir(&root).expect("evidence output must be fresh");
        let mut inputs = Vec::new();
        super::stage_adapter_replay_evidence(
            crate::agents::find("claude-code").unwrap(),
            Some(std::path::Path::new(&track)),
            true,
            &root,
            &mut inputs,
        )
        .unwrap();
        assert_eq!(inputs.len(), 1);
        assert!(inputs[0].bytes > 2);
    }

    #[test]
    fn stages_only_adapter_owned_replay_evidence() {
        let root = std::env::temp_dir().join(format!(
            "asb-adapter-evidence-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let track = root.join("replay.jsonl");
        let fixture = serde_json::json!({"request":{"body":{"messages":[{"role":"user","content":"\nWeb page content:\n---\nfixture"}],"tools":[]}}});
        std::fs::write(&track, format!("{fixture}\n")).unwrap();
        let claude = crate::agents::find("claude-code").unwrap();
        let other = crate::agents::find("codex").unwrap();
        let mut inputs = Vec::new();
        super::stage_adapter_replay_evidence(claude, Some(&track), false, &root, &mut inputs)
            .unwrap();
        super::stage_adapter_replay_evidence(other, Some(&track), true, &root, &mut inputs)
            .unwrap();
        assert!(inputs.is_empty());
        super::stage_adapter_replay_evidence(claude, Some(&track), true, &root, &mut inputs)
            .unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].target, "adapter-model-evidence.json");
        let bytes = std::fs::read(&inputs[0].source).unwrap();
        assert_eq!(inputs[0].sha256, crate::cassette::digest(&bytes));
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert!(
            super::stage_adapter_replay_evidence(claude, Some(&track), true, &root, &mut inputs)
                .is_err()
        );
        assert_eq!(std::fs::read(&inputs[0].source).unwrap(), bytes);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn all_workload_controller_routes_inherit_the_invocation_directory() {
        let root = std::env::temp_dir().join(format!(
            "asb-controller-cwd-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let _scope = crate::project::network_project_at(&root, &root);
        let mut command = super::controller_command("python3").unwrap();
        command.args([
            "-u",
            "-c",
            "import json,os; print(json.dumps({'cwd':os.getcwd()}),flush=True)",
        ]);
        let mut controller = super::Controller::spawn(&mut command).unwrap();
        let event = controller
            .receive(std::time::Instant::now() + std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(event["cwd"], root.to_str().unwrap());
        drop(controller);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn initial_material_protocol_preserves_only_known_bounded_local_diagnostics() {
        for event in [
            serde_json::json!({"event":"initial-material","material":{"schema_version":1,"scope":"prepared-workload-material-v1","sha256":"a".repeat(64)}}),
            serde_json::json!({"event":"error","error_class":"ExecutionError","reason":"mounted prepared material requires explicit coverage"}),
            serde_json::json!({"event":"error","error_class":"SdkError","reason":"secret-body-must-not-leak"}),
        ] {
            let script = "import json,sys\nassert json.loads(sys.stdin.readline())['op']=='initial-material'\nprint(sys.argv[1],flush=True)\nassert not sys.stdin.readline()\nprint(json.dumps(dict(event='closed',cleanup='stopped-preserved')),flush=True)\n";
            let mut command = ControllerCommand::new("python3").unwrap();
            command.args(["-u", "-c", script, &event.to_string()]);
            let mut controller = super::Controller::spawn(&mut command).unwrap();
            let result = super::request_initial_material(&mut controller);
            if event["event"] == "initial-material" {
                assert_eq!(result.unwrap().primary, "a".repeat(64));
            } else {
                let error = result.unwrap_err();
                if event["error_class"] == "ExecutionError" {
                    assert!(error.contains("mounted prepared material requires explicit coverage"));
                }
                assert!(!error.contains("secret-body-must-not-leak"));
            }
            controller
                .cancel(std::time::Duration::from_secs(3))
                .unwrap();
        }
    }

    #[test]
    fn prepared_material_requires_exact_scope_schema_digest_and_no_unknown_fields() {
        let original = serde_json::json!({"schema_version":1,"scope":"prepared-workload-material-v1","sha256":"a".repeat(64)});
        assert_eq!(
            super::validate_initial_material(&original).unwrap().primary,
            "a".repeat(64)
        );
        for (key, value) in [
            ("schema_version", serde_json::json!(2)),
            ("scope", serde_json::json!("image-only-v1")),
            ("sha256", serde_json::json!("A".repeat(64))),
            ("secret", serde_json::json!("must-not-appear-in-errors")),
        ] {
            let mut changed = original.clone();
            changed[key] = value;
            let error = super::validate_initial_material(&changed).unwrap_err();
            assert!(!error.contains("must-not-appear"));
        }
        let mut with_legacy = original.clone();
        with_legacy["legacy_sha256"] = serde_json::json!("c".repeat(64));
        assert_eq!(
            super::validate_initial_material(&with_legacy)
                .unwrap()
                .legacy,
            Some("c".repeat(64))
        );
        with_legacy["legacy_sha256"] = serde_json::json!("invalid");
        assert!(super::validate_initial_material(&with_legacy).is_err());
        assert!(super::validate_initial_material(&serde_json::Value::Null).is_err());
    }

    use super::*;

    #[cfg(unix)]
    #[test]
    fn snapshot_tools_prefer_project_then_path_and_report_missing() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "asb-snapshot-tools-{}",
            crate::tools::runtime::unique_id()
        ));
        let tools = root.join("tools");
        let host = root.join("host");
        std::fs::create_dir_all(tools.join("btrfs/bin")).unwrap();
        std::fs::create_dir_all(&host).unwrap();
        for path in [
            tools.join("btrfs/bin/btrfs"),
            host.join("btrfs"),
            host.join("mkfs.btrfs"),
            host.join("rsync"),
        ] {
            std::fs::write(&path, b"fixture").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert!(
            snapshot_auxiliary_tools(false, &tools, None)
                .unwrap()
                .is_empty()
        );
        let selected = snapshot_auxiliary_tools(true, &tools, Some(host.as_os_str())).unwrap();
        assert_eq!(selected["btrfs"], tools.join("btrfs/bin/btrfs"));
        assert_eq!(selected["rsync"], host.join("rsync"));
        assert_eq!(selected.len(), 3);
        std::fs::remove_file(host.join("rsync")).unwrap();
        assert!(
            snapshot_auxiliary_tools(true, &tools, Some(host.as_os_str()))
                .unwrap_err()
                .contains("rsync")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workload_tool_intent_covers_all_modes() {
        assert_eq!(workload_tool_mode(None), "none");
        assert_eq!(
            workload_tool_mode(Some(crate::tape::Mode::Record)),
            "record"
        );
        assert_eq!(
            workload_tool_mode(Some(crate::tape::Mode::Replay)),
            "replay"
        );
    }

    #[test]
    fn guest_tape_projection_uses_private_alias_and_preserves_replica_suffix() {
        assert_eq!(
            guest_tape_cassette(Some(Path::new("/host/campaign/batch-1/replay.jsonl"))).unwrap(),
            Some(PathBuf::from("/opt/asb-private/replay.jsonl"))
        );
        assert_eq!(
            guest_tape_cassette(Some(Path::new("/host/campaign/batch-1/replay-3.jsonl"))).unwrap(),
            Some(PathBuf::from("/opt/asb-private/replay-3.jsonl"))
        );
        assert_eq!(guest_tape_cassette(None).unwrap(), None);
        assert!(guest_tape_cassette(Some(Path::new("/"))).is_err());
    }

    #[test]
    fn guest_capsule_staging_copies_and_hashes_multiple_chunks() {
        let root = std::env::temp_dir().join(format!(
            "asb-guest-capsule-stream-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source");
        let artifacts = root.join("artifacts");
        std::fs::create_dir(&artifacts).unwrap();
        let data = vec![43u8; 200_001];
        std::fs::write(&source, &data).unwrap();
        let mut inputs = Vec::new();
        stage_native_capsule(
            Some(crate::workloads::session::NativeCapsule::Replay(&source)),
            &artifacts,
            &mut inputs,
        )
        .unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].bytes, data.len() as u64);
        assert_eq!(inputs[0].sha256, crate::cassette::digest(&data));
        assert_eq!(std::fs::read(&inputs[0].source).unwrap(), data);
        assert!(
            stage_native_capsule(
                Some(crate::workloads::session::NativeCapsule::Replay(&source)),
                &artifacts,
                &mut Vec::new()
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_guest_capsule_replays_stage_independent_copies() {
        let root = std::env::temp_dir().join(format!(
            "asb-guest-capsule-parallel-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source");
        let bytes = vec![37u8; 200_001];
        std::fs::write(&source, &bytes).unwrap();
        let handles: Vec<_> = (0..4)
            .map(|index| {
                let source = source.clone();
                let artifacts = root.join(format!("replica-{index}"));
                std::fs::create_dir(&artifacts).unwrap();
                std::thread::spawn(move || {
                    let mut inputs = Vec::new();
                    stage_native_capsule(
                        Some(crate::workloads::session::NativeCapsule::Replay(&source)),
                        &artifacts,
                        &mut inputs,
                    )
                    .unwrap();
                    inputs.pop().unwrap().source
                })
            })
            .collect();
        let staged: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for path in &staged {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        std::fs::write(&staged[0], b"replica mutation").unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
        for path in &staged[1..] {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_private_input_hashes_multiple_chunks() {
        let root = std::env::temp_dir().join(format!(
            "asb-replay-stream-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let data = vec![42u8; 200_001];
        std::fs::write(root.join("local-tools.jsonl"), &data).unwrap();
        let inputs =
            replay_private_inputs(Some(&root.join("replay.jsonl")), true, &[], false).unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].bytes, data.len() as u64);
        assert_eq!(inputs[0].sha256, crate::cassette::digest(&data));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_snapshot_input_follows_replica_and_is_stream_hashed() {
        let root = std::env::temp_dir().join(format!(
            "asb-snapshot-replay-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let data = vec![42u8; 200_001];
        std::fs::write(root.join("workspace-snapshots-concurrency-3.tar"), &data).unwrap();
        let inputs = replay_private_inputs(
            Some(&root.join("replay-concurrency-3.jsonl")),
            true,
            &[],
            false,
        )
        .unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].target, "workspace-snapshots.tar");
        assert_eq!(inputs[0].sha256, crate::cassette::digest(&data));
        assert!(
            replay_private_inputs(
                Some(&root.join("replay-concurrency-2.jsonl")),
                true,
                &[],
                false
            )
            .unwrap()
            .is_empty()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_private_input_preserves_the_replica_sidecar_identity() {
        let root = std::env::temp_dir().join(format!(
            "asb-replay-private-input-{}",
            crate::tools::runtime::unique_id()
        ));
        let batch = root.join("batch-1");
        std::fs::create_dir_all(&batch).unwrap();
        std::fs::write(batch.join("replay-2.jsonl"), b"provider tape\n").unwrap();
        std::fs::write(batch.join("local-tools-2.jsonl"), b"{}\n").unwrap();
        std::fs::write(batch.join("tool-protocol-2.json"), b"protocol\n").unwrap();
        std::fs::write(batch.join("determinism-manifest-2.json"), b"{}\n").unwrap();
        std::fs::write(batch.join("native-trace-2.jsonl"), b"").unwrap();
        let inputs =
            replay_private_inputs(Some(&batch.join("replay-2.jsonl")), true, &[], false).unwrap();
        assert_eq!(inputs.len(), 4);
        assert_eq!(inputs[0].target, "local-tools-2.jsonl");
        assert_eq!(inputs[0].bytes, 3);
        assert_eq!(inputs[0].sha256, crate::cassette::digest(b"{}\n"));
        assert_eq!(inputs[1].target, "tool-protocol-2.json");
        assert_eq!(inputs[1].sha256, crate::cassette::digest(b"protocol\n"));
        assert_eq!(inputs[2].target, "determinism-manifest-2.json");
        assert_eq!(inputs[3].target, "native-trace-2.jsonl");
        assert_eq!(inputs[3].bytes, 0);
        assert!(
            replay_private_inputs(Some(&batch.join("replay-2.jsonl")), false, &[], false)
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_private_input_allows_recordings_without_determinism_extension() {
        let root = std::env::temp_dir().join(format!(
            "asb-replay-private-input-no-determinism-{}",
            crate::tools::runtime::unique_id()
        ));
        let batch = root.join("batch-1");
        std::fs::create_dir_all(&batch).unwrap();
        std::fs::write(batch.join("replay.jsonl"), b"provider tape\n").unwrap();
        std::fs::write(batch.join("local-tools.jsonl"), b"{}\n").unwrap();

        let inputs =
            replay_private_inputs(Some(&batch.join("replay.jsonl")), true, &[], false).unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].target, "local-tools.jsonl");
        assert!(
            replay_private_inputs(Some(&batch.join("replay.jsonl")), false, &[], false)
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_private_input_stages_each_declared_mcp_relay_sidecar() {
        let root = std::env::temp_dir().join(format!(
            "asb-replay-mcp-input-{}",
            crate::tools::runtime::unique_id()
        ));
        let batch = root.join("batch");
        std::fs::create_dir_all(&batch).unwrap();
        let replay = batch.join("replay.jsonl");
        std::fs::write(&replay, b"provider tape\n").unwrap();
        let relay = crate::agents::mcp_relay::empty_bundle().unwrap();
        std::fs::write(batch.join("mcp-relay-filesystem.jsonl"), &relay).unwrap();
        let servers = [crate::benchmark::McpServerConfig {
            id: "filesystem".into(),
            program: "fixture-mcp".into(),
            args: Vec::new(),
        }];
        let inputs = replay_private_inputs(Some(&replay), true, &servers, true).unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].target, "mcp-relay-filesystem.jsonl");
        assert_eq!(inputs[0].sha256, crate::cassette::digest(&relay));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_private_input_allows_provider_tape_only_recordings() {
        let root = std::env::temp_dir().join(format!(
            "asb-replay-private-input-tape-only-{}",
            crate::tools::runtime::unique_id()
        ));
        let batch = root.join("batch-1");
        std::fs::create_dir_all(&batch).unwrap();
        std::fs::write(batch.join("replay.jsonl"), b"provider tape\n").unwrap();

        assert!(
            replay_private_inputs(Some(&batch.join("replay.jsonl")), true, &[], false)
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn replay_private_input_rejects_symlinked_tool_protocol() {
        let root = std::env::temp_dir().join(format!(
            "asb-replay-private-protocol-symlink-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("identity.json"), b"{}\n").unwrap();
        std::os::unix::fs::symlink(root.join("identity.json"), root.join("tool-protocol.json"))
            .unwrap();
        let error = match replay_private_inputs(Some(&root.join("replay.jsonl")), true, &[], false)
        {
            Ok(_) => panic!("symlinked protocol identity must be rejected"),
            Err(error) => error,
        };
        assert!(error.contains("sealed local-tool protocol identity"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replay_private_input_rejects_malformed_native_trace() {
        let root = std::env::temp_dir().join(format!(
            "asb-replay-private-input-malformed-native-trace-{}",
            crate::tools::runtime::unique_id()
        ));
        let batch = root.join("batch-1");
        std::fs::create_dir_all(&batch).unwrap();
        std::fs::write(batch.join("replay.jsonl"), b"provider tape\n").unwrap();
        std::fs::write(batch.join("native-trace.jsonl"), b"{}\n").unwrap();

        let error = match replay_private_inputs(Some(&batch.join("replay.jsonl")), true, &[], false)
        {
            Ok(_) => panic!("malformed native trace must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error, "sealed native replay trace is malformed");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn guest_environment_forwards_only_selected_adapter_invocation_settings() {
        let values = std::collections::BTreeMap::from([
            ("ASB_CODEX_SANDBOX_MODE", "danger-full-access"),
            ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "65536"),
            ("HTTPS_PROXY", "http://proxy.invalid:8443"),
            ("UNRELATED_HOST_VALUE", "must-not-cross"),
        ]);
        let lookup = |name: &str| values.get(name).map(|value| value.to_string());
        let codex = guest_environment("codex", lookup);
        assert_eq!(
            codex["ASB_CODEX_SANDBOX_MODE"],
            Value::String("danger-full-access".into())
        );
        assert_eq!(
            codex["HTTPS_PROXY"],
            Value::String("http://proxy.invalid:8443".into())
        );
        assert!(codex.get("UNRELATED_HOST_VALUE").is_none());

        let opencode = guest_environment("opencode", lookup);
        assert!(opencode.get("ASB_CODEX_SANDBOX_MODE").is_none());
        assert!(opencode.get("CLAUDE_CODE_MAX_OUTPUT_TOKENS").is_none());
        assert_eq!(opencode["HTTPS_PROXY"], codex["HTTPS_PROXY"]);
        let claude = guest_environment("claude-code", lookup);
        assert_eq!(
            claude["CLAUDE_CODE_MAX_OUTPUT_TOKENS"],
            Value::String("65536".into())
        );
        assert!(claude.get("ASB_CODEX_SANDBOX_MODE").is_none());
    }

    #[test]
    fn guest_project_projection_excludes_host_capabilities_and_survives_proxy_rewrite() {
        use std::io::Write;
        use std::process::Stdio;
        for instances in [
            Vec::new(),
            vec!["asb-host-only-0".into(), "asb-host-only-1".into()],
        ] {
            let host = crate::project::ProjectConfig {
                tools_dir: "host-tools".into(),
                workloads_dir: "host-workloads".into(),
                workspace_dir: "host-workspace".into(),
                cgroup_parent: Some("/host-only/cgroup".into()),
                perfetto_instances: instances.clone(),
                installation: crate::project::InstallationConfig::default(),
                replay: crate::project::ReplayConfig::default(),
                proxy: crate::project::NetworkProxy {
                    http: Some("http://localhost:1234/proxy".into()),
                    ..Default::default()
                },
            };
            let encoded = guest_project_configuration(&host).unwrap();
            let table: toml::Value = toml::from_str(&encoded).unwrap();
            assert_eq!(table.as_table().unwrap().len(), 4);
            assert!(table.get("perfetto_instances").is_none());
            assert!(table.get("cgroup_parent").is_none());
            let source = r#"
import sys,tomllib
from types import SimpleNamespace
sys.path.insert(0,sys.argv[1])
from asb_relay import forward_request
configuration=sys.stdin.read()
targets=[]
def start(values):
    targets.extend(values)
    return SimpleNamespace(ports=[2345])
result=forward_request({'project_configuration':configuration,'environment':{},'request':{'connection':{'base_url':'http://localhost:1234/v1'}}},start)
assert targets == [('localhost',1234)]
assert result['request']['connection']['base_url'] == 'http://localhost:2345/v1'
assert tomllib.loads(result['project_configuration'])['proxy']['http'] == 'http://localhost:2345/proxy'
sys.stdout.write(result['project_configuration'])
"#;
            let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/benchmarks");
            let mut child = Command::new("python3")
                .args(["-I", "-S", "-B", "-c", source])
                .arg(assets)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(encoded.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let guest: crate::project::ProjectConfig =
                toml::from_str(std::str::from_utf8(&output.stdout).unwrap()).unwrap();
            assert_eq!(guest.tools_dir, ".asb-tools");
            assert_eq!(guest.workloads_dir, "workloads");
            assert_eq!(guest.workspace_dir, "workspace");
            assert_eq!(guest.cgroup_parent, None);
            assert!(guest.perfetto_instances.is_empty());
            assert_eq!(host.perfetto_instances, instances);
            assert_eq!(host.cgroup_parent.as_deref(), Some("/host-only/cgroup"));
        }
    }

    #[test]
    fn inspected_grade_requires_bound_identity_and_explicitly_unknown_execution() {
        let reference = super::super::recovery::AttemptReference {
            schema_version: 1,
            preparation_id: "a-b-c".into(),
            workload: "fixture".into(),
            task_id: "original-task".into(),
            dataset: super::super::session::DatasetIdentity {
                source: "fixture/dataset".into(),
                revision: "b".repeat(40),
                split: "test".into(),
            },
            artifacts: "unused".into(),
        };
        let original = json!({"schema_version":1,"scope":"retained-official-grade-not-execution-recovery",
            "task":{"workload":reference.workload,"task_id":reference.task_id,"dataset":reference.dataset,"attempt_id":format!("asb-{}","a".repeat(32))},
            "workload_baseline":{"kind":"upstream-task-definition","base_commit":"b".repeat(40)},
            "workload_evaluation":"upstream-harness","workload_success":false,"recorded_cleanup":"stopped-preserved",
            "evidence":{"task.json":{"sha256":"d".repeat(64),"bytes":100}},
            "process_success":null,"exit_code":null,"elapsed_ms":null,"token_usage":null,"steps":null});
        validate_inspected_grade(&original, &reference).unwrap();
        for evidence in [
            json!(null),
            json!({}),
            json!({"../task.json":{"sha256":"d".repeat(64),"bytes":1}}),
            json!({"/task.json":{"sha256":"d".repeat(64),"bytes":1}}),
            json!({"task.json":{"sha256":"bad","bytes":1}}),
            json!({"task.json":{"sha256":"d".repeat(64),"bytes":-1}}),
            json!({"task.json":{"sha256":"d".repeat(64),"bytes":513 * 1024 * 1024}}),
        ] {
            let mut forged = original.clone();
            forged["evidence"] = evidence;
            assert!(validate_inspected_grade(&forged, &reference).is_err());
        }
        for field in [
            "process_success",
            "exit_code",
            "elapsed_ms",
            "token_usage",
            "steps",
        ] {
            let mut forged = original.clone();
            forged[field] = json!(0);
            assert!(
                validate_inspected_grade(&forged, &reference).is_err(),
                "{field}"
            );
            forged.as_object_mut().unwrap().remove(field);
            assert!(
                validate_inspected_grade(&forged, &reference).is_err(),
                "missing {field}"
            );
        }
        for (field, value) in [
            ("scope", json!("complete")),
            ("workload_success", Value::Null),
            ("recorded_cleanup", json!("pending")),
            ("workload_evaluation", json!("adapter-smoke")),
        ] {
            let mut forged = original.clone();
            forged[field] = value;
            assert!(
                validate_inspected_grade(&forged, &reference).is_err(),
                "{field}"
            );
        }
        let mut forged = original.clone();
        forged["task"]["task_id"] = json!("another-task");
        assert!(validate_inspected_grade(&forged, &reference).is_err());
        forged = original.clone();
        forged["task"]["attempt_id"] = json!("invalid");
        assert!(validate_inspected_grade(&forged, &reference).is_err());
        forged = original;
        forged["workload_baseline"] = json!({"kind":"unavailable"});
        assert!(validate_inspected_grade(&forged, &reference).is_err());
    }

    fn session(results: &[Value], completed: bool) -> Session {
        let script = format!(
            "import json,sys\nresults=json.loads(sys.argv[1])\nprint(json.dumps(dict(event='ready')),flush=True)\nfor result in results:\n request=json.loads(sys.stdin.readline())\n assert request['op']=='evaluate'\n print(json.dumps(dict(event='evaluated',result=result)),flush=True)\nlast=sys.stdin.readline()\nassert bool(last)=={completed}\nif last: assert json.loads(last)['op']=='close'\nprint(json.dumps(dict(event='closed',cleanup='removed' if last else 'stopped-preserved')),flush=True)\n",
            completed = if completed { "True" } else { "False" },
        );
        let mut command = ControllerCommand::new("python3").unwrap();
        command.args([
            "-u",
            "-c",
            &script,
            &serde_json::to_string(results).unwrap(),
        ]);
        let mut controller = Controller::spawn(&mut command).unwrap();
        controller
            .receive(Instant::now() + Duration::from_secs(3))
            .unwrap();
        Session {
            snapshot: false,
            initial_material: Err("fixture has no captured initial material".into()),
            evaluation_seconds: EVALUATION_TIMEOUT_SECONDS,
            preparation_seconds: WORKLOAD_READY_TIMEOUT.as_secs(),
            task_default_execution_seconds: 1.0,
            process_lifetime: super::super::session::ProcessLifetime::Quiesce,
            controller,
            invocation: TaskInvocation {
                identity: TaskIdentity::local("fixture", Some("original-task")),
                input: "fixture issue".into(),
                workspace: "fixture-workspace".into(),
            },
            baseline: BaselineEvidence::UpstreamTaskDefinition {
                base_commit: "a".repeat(40),
            },
            guest_workspace: "/task".into(),
            agent_timeout: Duration::from_secs(1),
            measurement_active: false,
            preparation_active: false,
            resume_ack_deadline: None,
            resume_exec_id: None,
            resume_target: None,
            resume_target_sha256: None,
            cleanup_attempted: false,
            completed_grade: None,
            settlement: None,
            artifacts: "retained-attempt".into(),
        }
    }

    fn result(outcome: &str, resolved: Value) -> Value {
        json!({"workload":"fixture", "task_id":"original-task", "dataset":null,
               "outcome":outcome, "resolved":resolved})
    }

    #[test]
    fn only_matching_completed_grades_allow_destructive_session_finish() {
        for (outcome, resolved) in [
            ("resolved", true),
            ("unresolved", false),
            ("empty_submission", false),
        ] {
            let mut task = session(&[result(outcome, json!(resolved))], true);
            assert!(task.evaluate("").is_ok());
            assert!(task.completed_grade.is_some());
            task.finish().unwrap();
            assert!(task.completed_grade.is_none());
        }
        let mut wrong_task = result("resolved", json!(true));
        wrong_task["task_id"] = json!("another-task");
        for invalid in [
            result("evaluation_error", json!(false)),
            result("resolved", json!(false)),
            result("unresolved", json!(true)),
            result("resolved", Value::Null),
            wrong_task,
        ] {
            let mut task = session(&[invalid], false);
            assert!(task.evaluate("").is_err());
            assert!(task.completed_grade.is_none());
            let settlement = task.finish();
            assert!(
                settlement.is_ok(),
                "confirmed preservation is not uncertain cleanup: {settlement:?}"
            );
            assert!(task.cleanup_attempted);
            let settlement = settlement.unwrap();
            assert_eq!(
                settlement.disposition,
                ResourceDisposition::StoppedPreserved
            );
            assert!(settlement.diagnostic.is_none());
            assert_eq!(
                task.finish().unwrap(),
                settlement,
                "settled finish must not repeat cancellation"
            );
        }
    }

    #[test]
    fn later_evaluator_failure_cannot_reuse_an_earlier_grade_to_delete_work() {
        let mut task = session(
            &[
                result("unresolved", json!(false)),
                result("evaluation_error", json!(false)),
            ],
            false,
        );
        assert!(task.evaluate("").is_ok());
        assert!(task.completed_grade.is_some());
        assert!(task.evaluate("").is_err());
        assert!(task.completed_grade.is_none());
        assert!(
            task.finish().is_ok(),
            "later evaluator failure must preserve without authorizing removal"
        );
    }

    #[test]
    fn failed_close_keeps_its_diagnostic_after_confirmed_cached_preservation() {
        let mut task = session(&[], false);
        let script = "import json,sys\nprint(json.dumps(dict(event='ready')),flush=True)\nassert json.loads(sys.stdin.readline())['op']=='close'\nprint(json.dumps(dict(event='error',error_class='ExecutionError',reason='fixture close failure')),flush=True)\nassert sys.stdin.read()==''\nprint(json.dumps(dict(event='closed',cleanup='stopped-preserved')),flush=True)\nsys.exit(1)\n";
        let mut command = ControllerCommand::new("python3").unwrap();
        command.args(["-u", "-c", script]);
        task.controller = Controller::spawn(&mut command).unwrap();
        task.controller
            .receive(Instant::now() + Duration::from_secs(3))
            .unwrap();
        task.completed_grade = Some(
            CompletedGrade::decode(
                &json!({"event":"evaluated","result":result("unresolved", json!(false))}),
                &task.invocation.identity,
            )
            .unwrap(),
        );
        let settled = task.finish().unwrap();
        assert_eq!(settled.disposition, ResourceDisposition::StoppedPreserved);
        assert!(
            settled
                .diagnostic
                .as_ref()
                .unwrap()
                .contains("fixture close failure")
        );
        assert_eq!(task.finish().unwrap(), settled);
        assert_eq!(
            task.cancel().unwrap(),
            ResourceDisposition::StoppedPreserved
        );
        assert_eq!(task.finish().unwrap(), settled);
    }
}
