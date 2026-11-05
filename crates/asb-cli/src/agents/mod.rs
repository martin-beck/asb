//! Extensible adapters for benchmark agents.
//!
//! The runner owns only process lifecycle and output capture. Every command
//! shape and provider-specific adjustment lives in the owning adapter module.

mod adapters;
mod artifacts;
pub(crate) mod auxiliary;
#[allow(
    dead_code,
    reason = "capsule staging follows the validated manifest contract"
)]
pub(crate) mod capsule;
pub(crate) mod determinism;
pub(crate) mod mcp_framing;
pub(crate) mod mcp_relay;
pub(crate) mod mcp_transcript;
mod native_guest;
pub(crate) mod native_trace;
pub(crate) mod tool_replay;
pub(crate) mod tool_replay_mcp;
pub(crate) mod tool_tracing;
pub(crate) use artifacts::{ArtifactFormat, ArtifactOwner, ArtifactSpec, FrozenArtifacts};
pub(crate) mod driver;
use adapters::REGISTRY;

use crate::Error;
use crate::benchmark::BenchmarkConfig;
use crate::endpoint::{Interface, Support};
use crate::install::Recipe;
use crate::project;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Opaque adapter-owned code/data staged into an isolated guest runtime.
/// The remote dispatcher validates paths and bytes but never interprets the
/// adapter protocol carried by an asset.
#[derive(Clone, Copy)]
pub(crate) struct GuestPrivateAsset {
    pub target: &'static str,
    pub bytes: &'static [u8],
}

/// Dispatch the adapter-private OpenHands native bridge without exposing the
/// adapter registry itself to unrelated CLI modules.
pub(crate) fn agent_bridge_entry(args: &[std::ffi::OsString]) -> std::process::ExitCode {
    let Some((id, rest)) = args.split_first() else {
        eprintln!("asb: agent bridge requires a registered agent");
        return std::process::ExitCode::from(2);
    };
    let Some(adapter) = id.to_str().and_then(find) else {
        eprintln!("asb: agent bridge requires a registered agent");
        return std::process::ExitCode::from(2);
    };
    adapter.bridge_entry(rest).unwrap_or_else(|| {
        eprintln!("asb: selected agent has no private bridge");
        std::process::ExitCode::from(2)
    })
}

/// Keep adapter-declared private aliases usable by already-running launchers.
pub(crate) fn legacy_bridge_entry(args: &[std::ffi::OsString]) -> Option<std::process::ExitCode> {
    let (name, rest) = args.split_first()?;
    let name = name.to_str()?;
    all()
        .iter()
        .find(|adapter| adapter.bridge_aliases().contains(&name))?
        .bridge_entry(rest)
}

/// Versioned delivery of task inputs into an isolated coding-agent invocation.
/// Bump when incompatible input/session semantics change, not for binary updates.
pub(crate) const REPLAY_INPUT_CONTRACT: &str = "asb-agent-input-and-session-v1";

pub(crate) trait AgentAdapter: Sync {
    fn bridge_aliases(&self) -> &'static [&'static str] {
        &[]
    }
    fn bridge_entry(&self, _args: &[std::ffi::OsString]) -> Option<std::process::ExitCode> {
        None
    }
    /// Conservative per-attempt storage estimate for this agent's artifacts.
    fn estimated_disk_mib(&self) -> u64 {
        512
    }
    /// Invocation inputs actually supported by this ASB adapter's transport.
    fn input_modalities(&self) -> &'static [&'static str] {
        &["text"]
    }
    fn id(&self) -> &'static str;
    fn executable(&self) -> &'static str;
    /// Project-local installation recipe for this exact adapter executable.
    fn recipe(&self) -> Recipe;
    /// Adapter-owned package entry points, after an explicit local bin override.
    fn preferred_executables(&self, _installation_root: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
    fn runtime_inputs(
        &self,
        context: &crate::runtime_inputs::Context<'_>,
    ) -> Result<Vec<crate::runtime_inputs::Dependency>, Error> {
        // Native execution accepts the adapter command from PATH when no
        // project-local installation is selected. The optional input seal must
        // declare that same command instead of making fresh sweeps warn solely
        // because the user chose a system-installed agent.
        let executable = execution_lookup_at(
            self.id(),
            context.root,
            &context.project.tools_dir,
            context.invocation_dir,
        )
        .path()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| self.executable().into());
        let mut command = self.process(&executable)?.direct();
        project::apply_python_path(&mut command, context.root, Path::new(&executable))?;
        crate::tools::apply_runtime_path(&mut command, context.root, &context.project.tools_dir)?;
        let mut inputs = self.recipe().runtime_inputs(&context.tool_root(self.id()));
        inputs.push(crate::runtime_inputs::Dependency::command(&command));
        if let Some(path) = std::env::var_os("ASB_TOOL_DIR") {
            inputs.push(crate::runtime_inputs::Dependency::file(
                context.invocation_dir.join(path),
            ));
        }
        Ok(inputs)
    }
    /// Static adapter-private guest inputs required before client launch.
    fn guest_private_assets(&self) -> &'static [GuestPrivateAsset] {
        &[]
    }
    /// Optional adapter-owned compact evidence derived from a selected model
    /// replay track, for clients whose native tools make model subrequests.
    fn guest_replay_evidence(&self, _track: &Path) -> Result<Option<Vec<u8>>, Error> {
        Ok(None)
    }
    /// Supported wire interfaces, in preference order.
    fn supported_endpoints(&self) -> &'static [Support];
    fn command(&self, context: &CommandContext<'_>) -> CommandSpec;
    /// Whether this adapter turns `--max-steps` into a client-enforced limit.
    fn supports_max_steps(&self) -> bool {
        false
    }
    fn prepare(&self, context: &mut PreparationContext<'_>) -> Result<(), Error>;
    fn prepare_guest_service(
        &self,
        _command: &mut Command,
        _executable: &str,
        _run_dir: &Path,
        _mode: Option<crate::tape::Mode>,
        _native_tool: Option<&Path>,
        _snapshot_sha256: Option<&str>,
    ) -> Result<Option<auxiliary::AuxiliaryPlan>, Error> {
        Ok(None)
    }
    fn supports_native_tool_transport(&self) -> bool {
        false
    }
    /// Whether this adapter provides a tool replay preparation hook.
    fn has_tool_replay_hook(&self) -> bool {
        false
    }
    /// Bind an adapter-private pending native stream to settled workspace data.
    fn finalize_native_recording(
        &self,
        _pending: &[u8],
        _workspace_sha256: &str,
    ) -> Result<Vec<u8>, Error> {
        Err(Error::Run(
            "agent does not support deferred native recording".into(),
        ))
    }
    /// Adapter-owned consistency check over a sealed native stream and snapshot archive.
    fn verify_native_snapshot(
        &self,
        _native: &[u8],
        _snapshot: &mut std::fs::File,
        _digest: &str,
    ) -> Result<(), Error> {
        Ok(())
    }
    /// Whether this adapter owns a bounded guest-workspace effect capsule.
    /// Remote orchestration may only transfer the opaque bytes; capture and
    /// application remain adapter responsibilities.
    fn supports_native_workspace_capsule(&self) -> bool {
        false
    }
    fn capture_native_workspace_capsule(&self, _workspace: &Path) -> Result<Vec<u8>, Error> {
        Err(Error::Run(
            "agent does not implement native workspace capsule capture".into(),
        ))
    }
    fn apply_native_workspace_capsule(
        &self,
        _workspace: &Path,
        _bytes: &[u8],
    ) -> Result<(), Error> {
        Err(Error::Run(
            "agent does not implement native workspace capsule replay".into(),
        ))
    }
    fn process(&self, executable: &str) -> Result<crate::launch::ProcessLaunch, Error> {
        Ok(crate::launch::ProcessLaunch::new(executable))
    }
    /// The identity used for the client process inside an owned workload
    /// container.  Most clients accept the workload default; clients which
    /// refuse unattended operation as root opt into the isolated non-root
    /// identity below.
    fn execution_identity(&self) -> ExecutionIdentity {
        ExecutionIdentity::WorkloadDefault
    }
    fn replay_credential_vars(&self) -> &'static [&'static str] {
        &[]
    }
    /// Optional adapter-owned opaque provider-wire canonicalizer. The shared
    /// tape transport invokes this extension without interpreting agent
    /// protocol fields.
    fn tape_wire_extension(&self) -> Option<&'static str> {
        None
    }
    /// Optional adapter-owned replay normalizer.  Unlike a wire extension it
    /// receives only the already decoded provider request and must preserve
    /// every semantic field it does not explicitly recognize.  The shared
    /// tape transport materializes this source as a separate module and never
    /// interprets its agent-specific protocol rules.
    fn tape_normalization_scripts(&self, _workload: &str) -> Vec<TapeNormalizationScript> {
        Vec::new()
    }
    /// Whether this adapter declares a tool recording or replay transport.
    fn supports_tool_replay(&self) -> bool {
        false
    }
    /// Whether this adapter can materialize declared ordinary stdio MCP
    /// servers without treating them as ASB semantic local-tool replay.
    fn supports_mcp_servers(&self) -> bool {
        false
    }
    /// Whether declared MCP servers can be substituted by ASB's transparent
    /// relay. This is deliberately narrower than ordinary MCP configuration:
    /// a requested recording must never silently start an unrecorded server.
    fn supports_mcp_relay(&self) -> bool {
        false
    }
    /// Mutable client-state overrides, distinct from provider credentials.
    fn state_environment_vars(&self) -> &'static [&'static str] {
        &[]
    }
    /// Non-secret invocation settings that must cross an isolated workload
    /// boundary. Adapters opt in one name at a time; the workload transport
    /// never copies an arbitrary host environment into a benchmark guest.
    fn forwarded_invocation_environment_vars(&self) -> &'static [&'static str] {
        &[]
    }
    fn collect(&self, _context: &CollectionContext<'_>) -> AgentArtifacts {
        AgentArtifacts::default()
    }
    fn artifact_specs(&self) -> &'static [ArtifactSpec] {
        &[]
    }
    /// `Some` means the adapter owns parsing of its output, even if usage is unavailable.
    fn output_usage(
        &self,
        _stdout: &str,
        _stderr: &str,
        _input_includes_cache: bool,
    ) -> Option<crate::measurements::TokenUsage> {
        None
    }
    /// Extract the agent's structured final answer from machine-readable
    /// output.  Intermediate tool events and echoed prompts are never treated
    /// as a final response.
    fn final_response(&self, _output: &str, _workspace: &Path) -> Option<String> {
        None
    }
    #[cfg(test)]
    fn has_owned_final_response(&self) -> bool {
        false
    }
    /// Whether the adapter cross-checks its answer against frozen evidence.
    fn uses_artifact_final_response(&self) -> bool {
        false
    }
    /// Interpret a final response using the frozen per-run artifacts.
    fn final_response_with_artifacts(
        &self,
        output: &str,
        _frozen: &FrozenArtifacts,
        workspace: &Path,
        _complete: bool,
    ) -> Option<String> {
        self.final_response(output, workspace)
    }
    /// Confirm a guest answer against the guest's restored frozen evidence.
    fn validate_guest_artifact_answer(&self, _answer: &str, _frozen: &FrozenArtifacts) -> bool {
        false
    }
    /// A complete run's authoritative counter, not a count of JSON/text matches.
    fn step_count(&self, _stdout: &str, _stderr: &str) -> Option<u64> {
        None
    }
    /// Count adapter-observed completed tool calls from a complete client stream.
    fn tool_call_count(&self, _stdout: &str, _stderr: &str) -> Option<usize> {
        None
    }
    /// Extract an adapter-owned counter before an isolated guest is removed.
    fn guest_step_count(&self, _run_dir: &Path) -> Option<u64> {
        None
    }
    /// A terminal client/protocol failure that a zero process exit can hide.
    fn execution_failure(&self, _stdout: &str, _stderr: &str) -> Option<String> {
        None
    }
    /// Validate a terminal answer independently of the native/guest launcher.
    fn final_response_failure(&self, _response: Option<&str>) -> Option<String> {
        None
    }
}

pub(crate) struct PreparationContext<'a> {
    pub command: &'a mut Command,
    pub model: &'a str,
    pub connection: &'a crate::endpoint::Connection,
    /// Canonical workload workspace.  Adapter-owned local tools must receive
    /// this explicitly rather than inferring it from a process cwd.
    pub workspace: &'a Path,
    /// Private mutable configuration for this agent execution.
    pub runtime_dir: &'a Path,
    /// A unique path for artifacts belonging to this individual run.
    pub run_dir: &'a Path,
    /// Declared ordinary stdio MCP servers.  Adapters that support normal MCP
    /// configuration receive these typed declarations directly; the generic
    /// runner never interprets tool names or command arguments.
    pub mcp_servers: &'a [crate::benchmark::McpServerConfig],
    /// Explicit transparent MCP recording/replay intent.  This remains
    /// separate from adapter-local semantic tool replay.
    pub mcp_tape: Option<McpTapeContext<'a>>,
    /// Provider-tape identity for an execution whose agent-local tools must
    /// share the same record/replay boundary.  This is explicit rather than
    /// inferred from the provider URL: record and replay both use loopback
    /// endpoints.
    pub tape: Option<TapeContext<'a>>,
}

/// Immutable provider-tape identity made available to agent adapters.
///
/// Adapters that own an interceptable tool transport use this to place their
/// own causal tool transcript beside the provider cassette.  The transport is
/// deliberately adapter-owned; the generic runner does not know tool names or
/// result formats.
#[derive(Clone, Copy)]
pub(crate) struct TapeContext<'a> {
    pub mode: crate::tape::Mode,
    pub cassette: &'a Path,
}

/// Mode for the protocol-level MCP relay.  The relay has no tool semantics;
/// adapter configuration supplies it in place of an ordinary stdio server.
#[derive(Clone, Copy)]
pub(crate) struct McpTapeContext<'a> {
    pub mode: crate::tape::Mode,
    /// Selected replay payload. Recording owns a new private relay directory;
    /// replay needs the exact staged sidecar that supplied its sealed bundle.
    pub cassette: Option<&'a Path>,
}

impl PreparationContext<'_> {
    /// Describe only protocol metadata: credentials and endpoint URLs may contain secrets.
    fn unsupported_endpoint(&self, agent: &str) -> Error {
        Error::Run(format!(
            "agent `{agent}` cannot configure endpoint interface {:?} with {:?} authentication; choose a compatible agent/provider endpoint",
            self.connection.interface, self.connection.authentication
        ))
    }
}

pub(crate) struct CommandContext<'a> {
    /// Resolved project-local agent executable. Adapter-owned alternate SDK
    /// launchers may derive their matching interpreter from this wrapper.
    pub executable: &'a str,
    pub config: &'a BenchmarkConfig,
    pub endpoint: Option<&'a str>,
    pub interface: Option<Interface>,
    pub prompt: &'a str,
    pub max_steps: Option<u64>,
    /// Requested adapter-owned native terminal tape direction.
    pub native_tool_mode: Option<crate::tape::Mode>,
    #[allow(
        dead_code,
        reason = "future native-capable adapters own launch configuration"
    )]
    pub native_tool: Option<NativeToolContext<'a>>,
}

#[derive(Clone, Copy)]
pub(crate) struct NativeToolContext<'a> {
    #[allow(
        dead_code,
        reason = "future native-capable adapters own launch configuration"
    )]
    pub launch: &'a crate::cassette::NativeToolLaunch,
    #[allow(
        dead_code,
        reason = "future native-capable adapters receive this private runtime path"
    )]
    pub path: &'a Path,
}

pub(crate) fn select_process(
    adapter: &dyn AgentAdapter,
    executable: &str,
    native_tool: Option<NativeToolContext<'_>>,
) -> Result<crate::launch::ProcessLaunch, Error> {
    if native_tool.is_some() && !adapter.supports_native_tool_transport() {
        return Err(Error::Run(
            "selected agent does not support native tool replay transport".into(),
        ));
    }
    adapter.process(executable)
}

pub(crate) struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub prompt_delivery: PromptDelivery,
}

/// How the runner supplies a workload's canonical instruction document.
///
/// This belongs to the adapter contract: runners must never infer it from an
/// agent name or from incidental command-line syntax.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PromptDelivery {
    /// The runner appends the document as one argv argument.
    Argument,
    /// The adapter has already embedded the document in its own arguments.
    Embedded,
    /// The runner writes the exact document to stdin and closes the pipe.
    Stdin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExecutionIdentity {
    WorkloadDefault,
    Unprivileged,
}
#[derive(Default)]
pub(crate) struct AgentArtifacts {
    pub final_response: Option<String>,
    pub token_usage: Option<crate::measurements::TokenUsage>,
    /// Agent-reported completed model/tool calls, when the adapter exposes a
    /// bounded structured usage artifact.
    pub steps: Option<u64>,
    /// Aggregate tool calls observed in an adapter-owned completed session.
    pub tool_call_count: Option<usize>,
    /// A validated JSONL transcript of client-local tool turns.  The generic
    /// runner treats this only as a sealed auxiliary cassette payload; tool
    /// semantics remain in the adapter-owned replay transport.
    pub local_tool_transcript: Option<Vec<u8>>,
    /// Private extension evidence sealed beside provider and local-tool
    /// recordings. It is not rendered in reports or command output.
    pub deterministic_manifest: Option<Vec<u8>>,
    /// Compact native interaction index.  Referenced payloads remain private
    /// capture blobs and are never copied into user-facing output.
    pub native_trace: Option<Vec<u8>>,
    /// A complete adapter-owned native-tool exchange stream.  The runner
    /// validates and seals it as the cassette track's private native sidecar;
    /// it never enters result JSON, logs, or reports.
    pub native_tool_sidecar: Option<Vec<u8>>,
    /// Adapter-owned, bounded private sidecars retained only in the selected
    /// cassette track.  They are never rendered in run output or reports.
    pub sealed_artifacts: Vec<SealedArtifact>,
    /// Fail-closed adapter collection errors. They are retained only as the
    /// run diagnostic; an incomplete relay recording is never publishable.
    pub errors: Vec<String>,
}

/// A named private sidecar supplied by an adapter after an execution settles.
///
/// The recorder seals its digest and bytes atomically with the provider track.
/// Adapters must use stable lowercase kinds; cassette validation rejects any
/// malformed, duplicate, or oversized artifact before publication.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct SealedArtifact {
    pub kind: String,
    pub bytes: Vec<u8>,
}

pub(crate) struct CollectionContext<'a> {
    pub frozen: &'a FrozenArtifacts,
    /// One recorder-owned provider capture handle. Generic lifecycle code
    /// never reads or interprets its protocol bytes; an adapter may do so for
    /// its own post-hook correlation.
    pub provider_capture: Option<&'a Path>,
    pub run_dir: &'a Path,
    pub interface: Option<Interface>,
    pub anthropic_input_includes_cache: bool,
    pub mcp_servers: &'a [crate::benchmark::McpServerConfig],
    pub mcp_tape: Option<McpTapeContext<'a>>,
}

pub(super) fn read_usage_document(path: &Path) -> Option<serde_json::Value> {
    const MAX_USAGE_DOCUMENT_BYTES: u64 = 16 * 1024 * 1024;
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_USAGE_DOCUMENT_BYTES {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_USAGE_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_USAGE_DOCUMENT_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

/// Read a counter from the last final-result envelope, never from intermediate
/// snapshots, nested tool payloads, or a previous result with a newer partial
/// result after it. Adapters choose the field and its client-defined meaning.
pub(super) fn final_result_counter(stdout: &str, field: &str) -> Option<u64> {
    fn result(value: &serde_json::Value) -> Option<&serde_json::Value> {
        match value {
            serde_json::Value::Array(events) => events.iter().rev().find_map(result),
            serde_json::Value::Object(event)
                if event.get("type").and_then(|v| v.as_str()) == Some("result") =>
            {
                Some(value)
            }
            _ => None,
        }
    }
    let document = serde_json::from_str::<serde_json::Value>(stdout)
        .ok()
        .or_else(|| {
            let mut latest = None;
            for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
                let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
                if result(&value).is_some() {
                    latest = Some(value);
                }
            }
            latest
        })?;
    result(&document)?.get(field)?.as_u64()
}

type CommandBuilder = for<'a> fn(&CommandContext<'a>) -> CommandSpec;
type PreparationHook = for<'a> fn(&mut PreparationContext<'a>) -> Result<(), Error>;
type GuestServiceHook = fn(
    &mut Command,
    &str,
    &Path,
    Option<crate::tape::Mode>,
    Option<&Path>,
    Option<&str>,
) -> Result<Option<auxiliary::AuxiliaryPlan>, Error>;
type DeferredNativeHook = fn(&[u8], &str) -> Result<Vec<u8>, Error>;
type GuestReplayEvidenceHook = fn(&Path) -> Result<Vec<u8>, Error>;
type NativeSnapshotHook = fn(&[u8], &mut std::fs::File, &str) -> Result<(), Error>;
type ToolReplayHook = for<'a> fn(&mut PreparationContext<'a>, TapeContext<'a>) -> Result<(), Error>;
type CollectionHook = for<'a> fn(&CollectionContext<'a>) -> AgentArtifacts;
/// An adapter-selected extension which may attach its own bounded replay
/// evidence to the opaque artifact envelope.  Static dispatch deliberately
/// does not interpret adapter filenames or replay formats.
type ArtifactCollectionHook = for<'a> fn(&CollectionContext<'a>, &mut AgentArtifacts);
type UsageHook = fn(&str, &str, bool) -> crate::measurements::TokenUsage;
type BridgeHook = fn(&[std::ffi::OsString]) -> std::process::ExitCode;
type FinalResponseHook = fn(&str, &Path) -> Option<String>;
type ArtifactFinalResponseHook = fn(&str, &FrozenArtifacts, &Path, bool) -> Option<String>;
type GuestArtifactAnswerHook = fn(&str, &FrozenArtifacts) -> bool;
type ProcessHook = fn(&str) -> Result<crate::launch::ProcessLaunch, Error>;
type StepCountHook = fn(&str, &str) -> Option<u64>;
type ToolCallCountHook = fn(&str, &str) -> Option<usize>;
type GuestStepCountHook = fn(&Path) -> Option<u64>;
type FailureHook = fn(&str, &str) -> Option<String>;
type PreferredExecutablesHook = fn(&Path) -> Vec<PathBuf>;
type ResponseFailureHook = fn(Option<&str>) -> Option<String>;
type WorkspaceCapsuleCaptureHook = fn(&Path) -> Result<Vec<u8>, Error>;
type WorkspaceCapsuleApplyHook = fn(&Path, &[u8]) -> Result<(), Error>;

/// A narrowly-scoped cassette normalizer owned by an agent adapter.
///
/// `workloads` is an explicit allow-list, with `"*"` meaning that the
/// agent's protocol emits the incidental value for every workload.  Keeping
/// this selection data next to the adapter prevents the tape core from
/// guessing from request contents or benchmark names.
#[derive(Clone, Copy)]
pub(crate) struct TapeNormalizationScript {
    pub(crate) id: &'static str,
    pub(crate) workloads: &'static [&'static str],
    pub(crate) source: &'static str,
}

impl TapeNormalizationScript {
    fn applies_to(self, workload: &str) -> bool {
        let mut index = 0;
        while index < self.workloads.len() {
            let candidate = self.workloads[index];
            if candidate == "*" || candidate == workload {
                return true;
            }
            index += 1;
        }
        false
    }
}

/// Trait-object implementation whose behavior is supplied by individual files.
pub(super) struct StaticAdapter {
    bridge_entry: Option<BridgeHook>,
    bridge_aliases: &'static [&'static str],
    id: &'static str,
    estimated_disk_mib: u64,
    executable: &'static str,
    recipe: Recipe,
    supported_endpoints: &'static [Support],
    command: CommandBuilder,
    prepare: PreparationHook,
    tool_replay: Option<ToolReplayHook>,
    mcp_servers: bool,
    mcp_relay: bool,
    native_tool_transport: bool,
    guest_service: Option<GuestServiceHook>,
    finalize_native_recording: Option<DeferredNativeHook>,
    native_snapshot_check: Option<NativeSnapshotHook>,
    max_steps: bool,
    native_workspace_capsule: Option<(WorkspaceCapsuleCaptureHook, WorkspaceCapsuleApplyHook)>,
    collect: Option<CollectionHook>,
    replay_artifacts: Option<ArtifactCollectionHook>,
    lifecycle_artifacts: Option<ArtifactCollectionHook>,
    artifact_specs: &'static [ArtifactSpec],
    deterministic_extension: bool,
    output_usage: Option<UsageHook>,
    final_response: Option<FinalResponseHook>,
    artifact_final_response: Option<ArtifactFinalResponseHook>,
    guest_artifact_answer: Option<GuestArtifactAnswerHook>,
    process: Option<ProcessHook>,
    step_count: Option<StepCountHook>,
    tool_call_count: Option<ToolCallCountHook>,
    guest_step_count: Option<GuestStepCountHook>,
    failure: Option<FailureHook>,
    response_failure: Option<ResponseFailureHook>,
    replay_credential_vars: &'static [&'static str],
    tape_wire_extension: Option<&'static str>,
    tape_normalization_scripts: &'static [TapeNormalizationScript],
    state_environment_vars: &'static [&'static str],
    forwarded_invocation_environment_vars: &'static [&'static str],
    preferred_executables: Option<PreferredExecutablesHook>,
    guest_private_assets: &'static [GuestPrivateAsset],
    guest_replay_evidence: Option<GuestReplayEvidenceHook>,
    execution_identity: ExecutionIdentity,
}
impl StaticAdapter {
    pub(super) const fn with_estimated_disk_mib(mut self, mib: u64) -> Self {
        self.estimated_disk_mib = mib;
        self
    }
    pub(super) const fn new(
        id: &'static str,
        executable: &'static str,
        recipe: Recipe,
        supported_endpoints: &'static [Support],
        command: CommandBuilder,
        prepare: PreparationHook,
    ) -> Self {
        Self {
            id,
            estimated_disk_mib: 512,
            executable,
            recipe,
            supported_endpoints,
            command,
            prepare,
            tool_replay: None,
            mcp_servers: false,
            mcp_relay: false,
            native_tool_transport: false,
            guest_service: None,
            finalize_native_recording: None,
            native_snapshot_check: None,
            max_steps: false,
            native_workspace_capsule: None,
            collect: None,
            replay_artifacts: None,
            lifecycle_artifacts: Some(native_trace::collect_lifecycle_artifact),
            artifact_specs: &[],
            deterministic_extension: false,
            output_usage: None,
            bridge_entry: None,
            bridge_aliases: &[],
            final_response: None,
            artifact_final_response: None,
            guest_artifact_answer: None,
            process: None,
            step_count: None,
            tool_call_count: None,
            guest_step_count: None,
            failure: None,
            response_failure: None,
            replay_credential_vars: &[],
            tape_wire_extension: None,
            tape_normalization_scripts: &[],
            state_environment_vars: &[],
            forwarded_invocation_environment_vars: &[],
            preferred_executables: None,
            guest_private_assets: &[],
            guest_replay_evidence: None,
            execution_identity: ExecutionIdentity::WorkloadDefault,
        }
    }

    pub(super) const fn with_collection(mut self, collect: CollectionHook) -> Self {
        self.collect = Some(collect);
        self
    }
    pub(super) const fn with_guest_service(mut self, hook: GuestServiceHook) -> Self {
        self.guest_service = Some(hook);
        self
    }

    pub(super) const fn with_deferred_native_recording(
        mut self,
        finalize: DeferredNativeHook,
    ) -> Self {
        self.finalize_native_recording = Some(finalize);
        self
    }

    pub(super) const fn with_native_snapshot_check(mut self, check: NativeSnapshotHook) -> Self {
        self.native_snapshot_check = Some(check);
        self
    }

    /// Adapter-owned interception of local tool execution during a tape-backed
    /// record or replay.  The runner provides only cassette identity and mode;
    /// the adapter owns the client protocol and tool semantics.
    pub(super) const fn with_tool_replay(mut self, hook: ToolReplayHook) -> Self {
        self.tool_replay = Some(hook);
        self.replay_artifacts = Some(adapters::replay_artifacts::collect_local_tool_transcript);
        self
    }

    /// Declares that the adapter has a normal stdio MCP configuration ingress.
    /// This is intentionally distinct from complete ASB local-tool replay.
    pub(super) const fn with_mcp_servers(mut self) -> Self {
        self.mcp_servers = true;
        self
    }

    /// Declares a tested adapter route for substituting each ordinary MCP
    /// server with ASB's transport-only relay.
    pub(super) const fn with_mcp_relay(mut self) -> Self {
        self.mcp_relay = true;
        self
    }

    /// Declares an adapter-owned, tested consumer for a sealed native sidecar.
    pub(super) const fn with_native_tool_transport(mut self) -> Self {
        self.native_tool_transport = true;
        self
    }

    /// Declares that the adapter maps ASB's requested step budget to a
    /// client-enforced turn limit.
    pub(super) const fn with_max_steps(mut self) -> Self {
        self.max_steps = true;
        self
    }

    /// Declares adapter-owned bounded workspace-effect capture/replay for an
    /// isolated guest. Remote orchestration only transfers the returned bytes.
    pub(super) const fn with_native_workspace_capsule(
        mut self,
        capture: WorkspaceCapsuleCaptureHook,
        apply: WorkspaceCapsuleApplyHook,
    ) -> Self {
        self.native_workspace_capsule = Some((capture, apply));
        self
    }

    pub(super) const fn with_preferred_executables(
        mut self,
        hook: PreferredExecutablesHook,
    ) -> Self {
        self.preferred_executables = Some(hook);
        self
    }

    pub(super) const fn with_guest_replay_evidence(
        mut self,
        hook: GuestReplayEvidenceHook,
    ) -> Self {
        self.guest_replay_evidence = Some(hook);
        self
    }

    pub(super) const fn with_guest_private_assets(
        mut self,
        assets: &'static [GuestPrivateAsset],
    ) -> Self {
        self.guest_private_assets = assets;
        self
    }

    pub(super) const fn with_guest_artifact_answer(
        mut self,
        hook: GuestArtifactAnswerHook,
    ) -> Self {
        self.guest_artifact_answer = Some(hook);
        self
    }

    pub(super) const fn with_artifact_final_response(
        mut self,
        hook: ArtifactFinalResponseHook,
    ) -> Self {
        self.artifact_final_response = Some(hook);
        self
    }

    pub(super) const fn with_output_usage(mut self, hook: UsageHook) -> Self {
        self.output_usage = Some(hook);
        self
    }

    pub(super) const fn with_bridge_entry(mut self, hook: BridgeHook) -> Self {
        self.bridge_entry = Some(hook);
        self
    }

    pub(super) const fn with_bridge_aliases(mut self, aliases: &'static [&'static str]) -> Self {
        self.bridge_aliases = aliases;
        self
    }

    pub(super) const fn with_step_count(mut self, hook: StepCountHook) -> Self {
        self.step_count = Some(hook);
        self
    }
    pub(super) const fn with_tool_call_count(mut self, hook: ToolCallCountHook) -> Self {
        self.tool_call_count = Some(hook);
        self
    }
    pub(super) const fn with_guest_step_count(mut self, hook: GuestStepCountHook) -> Self {
        self.guest_step_count = Some(hook);
        self
    }

    pub(super) const fn with_final_response(mut self, hook: FinalResponseHook) -> Self {
        self.final_response = Some(hook);
        self
    }
    pub(super) const fn with_artifact_specs(mut self, specs: &'static [ArtifactSpec]) -> Self {
        self.artifact_specs = specs;
        self
    }

    /// Select ASB's portable deterministic execution extension for this adapter.
    /// The adapter owns this choice; the runner remains unaware of individual
    /// client names and records the resulting manifest as sealed evidence.
    pub(super) const fn with_deterministic_extension(mut self) -> Self {
        self.deterministic_extension = true;
        self
    }

    pub(super) const fn with_execution_failure(mut self, hook: FailureHook) -> Self {
        self.failure = Some(hook);
        self
    }

    pub(super) const fn with_final_response_failure(mut self, hook: ResponseFailureHook) -> Self {
        self.response_failure = Some(hook);
        self
    }

    pub(super) const fn with_process(mut self, hook: ProcessHook) -> Self {
        self.process = Some(hook);
        self
    }

    pub(super) const fn with_replay_credential_vars(
        mut self,
        vars: &'static [&'static str],
    ) -> Self {
        self.replay_credential_vars = vars;
        self
    }

    /// Supplies an adapter-private request canonicalizer to the generic tape
    /// transport. The transport passes opaque request data and does not know
    /// the extension's protocol vocabulary.
    pub(super) const fn with_tape_wire_extension(mut self, source: &'static str) -> Self {
        self.tape_wire_extension = Some(source);
        self
    }

    /// Select adapter-owned normalizers only for explicitly listed workloads.
    /// The tape layer materializes these sources but does not interpret their
    /// agent or workload semantics.
    pub(super) const fn with_tape_normalization_scripts(
        mut self,
        scripts: &'static [TapeNormalizationScript],
    ) -> Self {
        self.tape_normalization_scripts = scripts;
        self
    }

    pub(super) const fn with_state_environment_vars(
        mut self,
        vars: &'static [&'static str],
    ) -> Self {
        self.state_environment_vars = vars;
        self
    }

    pub(super) const fn with_forwarded_invocation_environment_vars(
        mut self,
        vars: &'static [&'static str],
    ) -> Self {
        self.forwarded_invocation_environment_vars = vars;
        self
    }

    pub(super) const fn with_unprivileged_execution(mut self) -> Self {
        self.execution_identity = ExecutionIdentity::Unprivileged;
        self
    }
}
impl AgentAdapter for StaticAdapter {
    fn bridge_aliases(&self) -> &'static [&'static str] {
        self.bridge_aliases
    }
    fn bridge_entry(&self, args: &[std::ffi::OsString]) -> Option<std::process::ExitCode> {
        self.bridge_entry.map(|hook| hook(args))
    }
    fn estimated_disk_mib(&self) -> u64 {
        self.estimated_disk_mib
    }
    fn guest_private_assets(&self) -> &'static [GuestPrivateAsset] {
        self.guest_private_assets
    }
    fn guest_replay_evidence(&self, track: &Path) -> Result<Option<Vec<u8>>, Error> {
        self.guest_replay_evidence
            .map(|hook| hook(track))
            .transpose()
    }
    fn state_environment_vars(&self) -> &'static [&'static str] {
        self.state_environment_vars
    }
    fn id(&self) -> &'static str {
        self.id
    }
    fn executable(&self) -> &'static str {
        self.executable
    }
    fn recipe(&self) -> Recipe {
        self.recipe
    }
    fn preferred_executables(&self, root: &Path) -> Vec<PathBuf> {
        self.preferred_executables
            .map(|hook| hook(root))
            .unwrap_or_default()
    }
    fn supported_endpoints(&self) -> &'static [Support] {
        self.supported_endpoints
    }
    fn command(&self, context: &CommandContext<'_>) -> CommandSpec {
        (self.command)(context)
    }
    fn prepare(&self, context: &mut PreparationContext<'_>) -> Result<(), Error> {
        // Validate deserialized guest connections as well as normally negotiated ones,
        // before an adapter can publish configuration or mutate the child environment.
        if !self.supported_endpoints.iter().any(|support| {
            support.interface == context.connection.interface
                && support
                    .authentication
                    .contains(&context.connection.authentication)
        }) {
            return Err(context.unsupported_endpoint(self.id));
        }
        if let (Some(hook), Some(tape)) = (self.tool_replay, context.tape) {
            hook(context, tape)?;
        }
        if self.deterministic_extension {
            determinism::prepare(self.id, context)?;
        }
        (self.prepare)(context)
    }
    fn process(&self, executable: &str) -> Result<crate::launch::ProcessLaunch, Error> {
        self.process.map_or_else(
            || Ok(crate::launch::ProcessLaunch::new(executable)),
            |hook| hook(executable),
        )
    }
    fn execution_identity(&self) -> ExecutionIdentity {
        self.execution_identity
    }
    fn replay_credential_vars(&self) -> &'static [&'static str] {
        self.replay_credential_vars
    }
    fn tape_wire_extension(&self) -> Option<&'static str> {
        self.tape_wire_extension
    }
    fn tape_normalization_scripts(&self, workload: &str) -> Vec<TapeNormalizationScript> {
        self.tape_normalization_scripts
            .iter()
            .copied()
            .filter(|script| script.applies_to(workload))
            .collect()
    }
    fn supports_tool_replay(&self) -> bool {
        self.tool_replay.is_some() || self.native_tool_transport
    }
    fn supports_max_steps(&self) -> bool {
        self.max_steps
    }
    fn supports_mcp_servers(&self) -> bool {
        self.mcp_servers
    }
    fn supports_mcp_relay(&self) -> bool {
        self.mcp_relay
    }
    fn supports_native_tool_transport(&self) -> bool {
        self.native_tool_transport
    }
    fn has_tool_replay_hook(&self) -> bool {
        self.tool_replay.is_some()
    }
    fn prepare_guest_service(
        &self,
        command: &mut Command,
        executable: &str,
        run_dir: &Path,
        mode: Option<crate::tape::Mode>,
        native_tool: Option<&Path>,
        snapshot_sha256: Option<&str>,
    ) -> Result<Option<auxiliary::AuxiliaryPlan>, Error> {
        self.guest_service.map_or(Ok(None), |hook| {
            hook(
                command,
                executable,
                run_dir,
                mode,
                native_tool,
                snapshot_sha256,
            )
        })
    }
    fn finalize_native_recording(
        &self,
        pending: &[u8],
        workspace_sha256: &str,
    ) -> Result<Vec<u8>, Error> {
        self.finalize_native_recording
            .ok_or_else(|| Error::Run("agent does not support deferred native recording".into()))?(
            pending,
            workspace_sha256,
        )
    }
    fn verify_native_snapshot(
        &self,
        native: &[u8],
        snapshot: &mut std::fs::File,
        digest: &str,
    ) -> Result<(), Error> {
        self.native_snapshot_check
            .map_or(Ok(()), |check| check(native, snapshot, digest))
    }
    fn supports_native_workspace_capsule(&self) -> bool {
        self.native_workspace_capsule.is_some()
    }
    fn capture_native_workspace_capsule(&self, workspace: &Path) -> Result<Vec<u8>, Error> {
        self.native_workspace_capsule
            .ok_or_else(|| {
                Error::Run("agent does not implement native workspace capsule capture".into())
            })?
            .0(workspace)
    }
    fn apply_native_workspace_capsule(&self, workspace: &Path, bytes: &[u8]) -> Result<(), Error> {
        self.native_workspace_capsule
            .ok_or_else(|| {
                Error::Run("agent does not implement native workspace capsule replay".into())
            })?
            .1(workspace, bytes)
    }
    fn forwarded_invocation_environment_vars(&self) -> &'static [&'static str] {
        self.forwarded_invocation_environment_vars
    }
    fn collect(&self, context: &CollectionContext<'_>) -> AgentArtifacts {
        let mut artifacts = self
            .collect
            .map(|collect| collect(context))
            .unwrap_or_default();
        if let Some(collect) = self.replay_artifacts {
            collect(context, &mut artifacts);
        }
        if let Some(collect) = self.lifecycle_artifacts {
            collect(context, &mut artifacts);
        }
        artifacts
    }
    fn artifact_specs(&self) -> &'static [ArtifactSpec] {
        self.artifact_specs
    }
    fn output_usage(
        &self,
        stdout: &str,
        stderr: &str,
        input_includes_cache: bool,
    ) -> Option<crate::measurements::TokenUsage> {
        self.output_usage
            .map(|hook| hook(stdout, stderr, input_includes_cache))
    }
    fn final_response(&self, output: &str, workspace: &Path) -> Option<String> {
        self.final_response.and_then(|hook| hook(output, workspace))
    }
    #[cfg(test)]
    fn has_owned_final_response(&self) -> bool {
        self.final_response.is_some() || self.artifact_final_response.is_some()
    }
    fn uses_artifact_final_response(&self) -> bool {
        self.artifact_final_response.is_some()
    }
    fn validate_guest_artifact_answer(&self, answer: &str, frozen: &FrozenArtifacts) -> bool {
        self.guest_artifact_answer
            .is_some_and(|hook| hook(answer, frozen))
    }
    fn final_response_with_artifacts(
        &self,
        output: &str,
        frozen: &FrozenArtifacts,
        workspace: &Path,
        complete: bool,
    ) -> Option<String> {
        self.artifact_final_response.map_or_else(
            || self.final_response(output, workspace),
            |hook| hook(output, frozen, workspace, complete),
        )
    }
    fn step_count(&self, stdout: &str, stderr: &str) -> Option<u64> {
        self.step_count.and_then(|hook| hook(stdout, stderr))
    }
    fn tool_call_count(&self, stdout: &str, stderr: &str) -> Option<usize> {
        self.tool_call_count.and_then(|hook| hook(stdout, stderr))
    }
    fn guest_step_count(&self, run_dir: &Path) -> Option<u64> {
        self.guest_step_count.and_then(|hook| hook(run_dir))
    }
    fn execution_failure(&self, stdout: &str, stderr: &str) -> Option<String> {
        self.failure.and_then(|hook| hook(stdout, stderr))
    }
    fn final_response_failure(&self, response: Option<&str>) -> Option<String> {
        self.response_failure.and_then(|hook| hook(response))
    }
}
pub(crate) fn prepare(agent: &str, context: &mut PreparationContext<'_>) -> Result<(), Error> {
    let Some(adapter) = find(agent) else {
        return Ok(());
    };
    adapter.prepare(context)
}

/// Mask known agent-owned credential aliases in the inherited replay environment.
pub(crate) fn mask_replay_credentials(command: &mut Command) {
    for adapter in REGISTRY {
        for variable in adapter.replay_credential_vars() {
            command.env_remove(variable);
        }
    }
}

pub(crate) fn all() -> &'static [&'static dyn AgentAdapter] {
    REGISTRY
}
pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    REGISTRY.iter().map(|adapter| adapter.id())
}
pub(crate) fn find(id: &str) -> Option<&'static dyn AgentAdapter> {
    REGISTRY.iter().copied().find(|adapter| adapter.id() == id)
}

/// Return an adapter-owned provider-wire extension without exposing its
/// protocol semantics to the shared tape runner.
pub(crate) fn tape_wire_extension(id: &str) -> Option<&'static str> {
    find(id).and_then(AgentAdapter::tape_wire_extension)
}

/// Return an adapter-owned replay normalizer without exposing its protocol
/// semantics to the shared tape runner.
pub(crate) fn tape_normalization_scripts(id: &str, workload: &str) -> Vec<TapeNormalizationScript> {
    find(id)
        .map(|adapter| adapter.tape_normalization_scripts(workload))
        .unwrap_or_default()
}

/// Read explicit usage fields only; never infer token counts from text.
pub(super) fn usage_fields(value: &serde_json::Value) -> crate::measurements::TokenUsage {
    let number = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            value
                .get(key)
                .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        })
    };
    crate::measurements::TokenUsage {
        input_tokens: number(&[
            "input_tokens",
            "prompt_tokens",
            "inputTokens",
            "promptTokens",
        ]),
        output_tokens: number(&[
            "output_tokens",
            "completion_tokens",
            "outputTokens",
            "completionTokens",
        ]),
        cached_tokens: number(&[
            "cache_read_input_tokens",
            "cache_read_tokens",
            "cacheReadTokens",
            "cached_tokens",
            "cached_input_tokens",
            "cachedTokens",
            "cached",
            "cache_tokens",
        ]),
        total_tokens: number(&["total_tokens", "totalTokens", "total"]),
        source: None,
    }
}

#[cfg(test)]
pub(crate) fn final_response(id: &str, output: &str, workspace: &Path) -> Option<String> {
    find(id).and_then(|adapter| adapter.final_response(output, workspace))
}

pub(crate) fn local_lookup(id: &str, tools_root: &Path) -> crate::executables::Lookup {
    let Some(adapter) = find(id) else {
        return crate::executables::Lookup::Missing;
    };
    crate::executables::project_lookup(
        tools_root,
        adapter.id(),
        adapter.executable(),
        adapter.preferred_executables(&tools_root.join(adapter.id())),
    )
}

pub(crate) fn execution_lookup(
    id: &str,
    project_root: &Path,
    tools_dir: &str,
) -> crate::executables::Lookup {
    execution_lookup_at(id, project_root, tools_dir, Path::new("."))
}

pub(crate) fn execution_lookup_at(
    id: &str,
    project_root: &Path,
    tools_dir: &str,
    invocation_dir: &Path,
) -> crate::executables::Lookup {
    let Some(adapter) = find(id) else {
        return crate::executables::Lookup::Missing;
    };
    if let Some(directory) = std::env::var_os("ASB_TOOL_DIR") {
        match crate::executables::lookup([invocation_dir
            .join(directory)
            .join(adapter.executable())])
        {
            crate::executables::Lookup::Missing => {}
            other => return other,
        }
    }
    local_lookup(id, &project::resolve(project_root, tools_dir))
}

pub(crate) fn resolve(id: &str, project_root: &Path, tools_dir: &str) -> Option<String> {
    execution_lookup(id, project_root, tools_dir)
        .path()
        .map(|path| path.to_string_lossy().into_owned())
}

#[cfg(test)]
#[path = "../../tests/unit/agent_contract.rs"]
mod contract_tests;

#[cfg(test)]
pub(crate) fn goose_archive_spec() -> crate::install::archive::ArchiveSpec {
    adapters::goose::archive_spec()
}

#[cfg(test)]
mod tests {
    #[test]
    fn normalizer_selection_requires_the_matching_agent_and_workload() {
        let scripts = [super::TapeNormalizationScript {
            id: "fixture",
            workloads: &["swe-bench"],
            source: "export function normalizeRequest(request) { return request.body; }",
        }];
        assert!(scripts[0].applies_to("swe-bench"));
        assert!(!scripts[0].applies_to("terminal-bench-1.0"));
        assert!(
            super::tape_normalization_scripts("codex", "swe-bench")
                .iter()
                .any(|script| script.id == "codex-runtime-envelopes")
        );
        assert!(super::tape_normalization_scripts("not-an-agent", "swe-bench").is_empty());
    }

    #[test]
    fn final_result_counter_uses_the_latest_native_value_without_summing() {
        for (output, expected) in [
            (r#"{"type":"result","turns":0}"#, Some(0)),
            (
                r#"[{"type":"assistant","turns":99},{"type":"result","turns":3},{"type":"result","turns":3}]"#,
                Some(3),
            ),
            (
                "{\"type\":\"result\",\"turns\":2}\n{\"type\":\"result\",\"turns\":4}",
                Some(4),
            ),
            (r#"[{"type":"result","turns":3},{"type":"result"}]"#, None),
            (r#"{"type":"result","turns":-1}"#, None),
            (r#"{"type":"result","turns":1.5}"#, None),
            (r#"{"type":"result","turns":"3"}"#, None),
            (
                r#"{"type":"tool","result":{"type":"result","turns":3}}"#,
                None,
            ),
            ("broken", None),
            (
                "{\"type\":\"result\",\"turns\":3}\n{\"type\":\"result\",\"turns\":",
                None,
            ),
            ("{\"type\":\"result\",\"turns\":3}\nbroken", None),
        ] {
            assert_eq!(
                super::final_result_counter(output, "turns"),
                expected,
                "{output}"
            );
        }
    }

    #[test]
    fn replay_agent_configs_use_the_tape_endpoint_without_live_credentials() {
        let root =
            std::env::temp_dir().join(format!("asb-agent-replay-config-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for (agent, config) in [
            ("pi", "pi/config/models.json"),
            ("cline", "cline/config/data/settings/providers.json"),
            ("continue", "continue/config/config.yaml"),
        ] {
            let runtime = root.join(agent);
            let mut command = std::process::Command::new("unused");
            let adapter = super::find(agent).unwrap();
            let connection = crate::endpoint::negotiate(
                adapter.id(),
                adapter.supported_endpoints(),
                "openrouter",
                "test-model",
                None,
                Some((crate::tape::Mode::Replay, "http://127.0.0.1:4242/v1")),
            )
            .unwrap();
            super::prepare(
                agent,
                &mut super::PreparationContext {
                    command: &mut command,
                    model: "test-model",
                    connection: &connection,
                    workspace: &runtime,
                    runtime_dir: &runtime,
                    run_dir: &runtime,
                    mcp_servers: &[],
                    mcp_tape: None,
                    tape: None,
                },
            )
            .unwrap();
            let contents = std::fs::read_to_string(runtime.join(config)).unwrap();
            assert!(
                contents.contains("http://127.0.0.1:4242/v1"),
                "{agent}: {contents}"
            );
            assert!(
                !contents.contains("https://openrouter.ai"),
                "{agent}: {contents}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    use super::*;

    #[test]
    fn every_registered_adapter_builds_a_noninteractive_command() {
        let config = BenchmarkConfig {
            planning: None,
            agent: "unused".into(),
            workload: "bug-fix".into(),
            provider: "cli2key".into(),
            model: "test-model".into(),
            concurrency: 1,
            trial_set: None,
            trial: None,
            workspace: "workspace".into(),
            log_file: "run.log".into(),
            measurements: None,
            retention: Default::default(),
            mcp_servers: Vec::new(),
        };
        for adapter in all() {
            let command = adapter.command(&CommandContext {
                executable: adapter.executable(),
                config: &config,
                endpoint: Some("http://127.0.0.1:3000/v1"),
                interface: Some(Interface::AnthropicMessages),
                prompt: "repair",
                max_steps: None,
                native_tool_mode: None,
                native_tool: None,
            });
            assert!(!command.program.is_empty(), "{}", adapter.id());
            assert!(!command.args.is_empty(), "{}", adapter.id());
            assert!(!adapter.executable().is_empty(), "{}", adapter.id());
        }
    }

    #[test]
    fn registry_has_unique_stable_ids() {
        let ids = names().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), all().len());
        assert!(ids.contains("codex"));
        assert!(ids.contains("continue"));
    }

    #[test]
    fn every_registered_agent_can_use_its_declared_tool_transport() {
        for adapter in all() {
            assert!(
                adapter.supports_tool_replay(),
                "{} has no tool transport",
                adapter.id()
            );
        }
        assert!(!find("openjiuwen").unwrap().has_tool_replay_hook());
        assert!(find("codex").unwrap().has_tool_replay_hook());
        assert!(find("goose").unwrap().has_tool_replay_hook());
        assert!(find("codex").unwrap().supports_mcp_servers());
        assert!(!find("pi").unwrap().supports_mcp_servers());
    }

    #[test]
    fn native_tool_transport_requires_an_explicit_adapter_capability() {
        let capable = all()
            .iter()
            .filter(|adapter| adapter.supports_native_tool_transport())
            .map(|adapter| adapter.id())
            .collect::<Vec<_>>();
        assert_eq!(
            capable,
            vec![
                "codex",
                "pi",
                "opencode",
                "workbuddy",
                "qwen-code",
                "openhands",
                "openjiuwen",
                "claude-code",
                "goose",
                "hermes",
                "cline",
                "continue"
            ]
        );
        assert!(find("pi").unwrap().supports_native_tool_transport());
        assert!(find("goose").unwrap().supports_native_tool_transport());
        let adapter = find("codex").unwrap();
        assert!(select_process(adapter, "not-executed", None).is_ok());
    }

    #[test]
    fn tool_replay_adapters_declare_a_replay_sidecar() {
        let local_sidecar = |adapter: &dyn AgentAdapter| {
            adapter.artifact_specs().iter().any(|spec| {
                spec.namespace == "tool-replay"
                    && spec.filename == "local-tools.jsonl"
                    && spec.format == ArtifactFormat::JsonLines
            })
        };
        let native_sidecar = |adapter: &dyn AgentAdapter| {
            adapter.artifact_specs().iter().any(|spec| {
                spec.filename.contains("native-tool") && spec.format == ArtifactFormat::JsonLines
            })
        };
        for adapter in all()
            .iter()
            .copied()
            .filter(|adapter| adapter.supports_tool_replay())
        {
            let available_sidecar = if adapter.supports_native_tool_transport() {
                native_sidecar(adapter)
            } else {
                local_sidecar(adapter)
            };
            assert!(
                available_sidecar,
                "{} declares a tool transport without its replay sidecar",
                adapter.id()
            );
        }
        // Negative coverage: non-native adapters cannot satisfy the contract
        // merely by declaring an unrelated native artifact, while native
        // adapters are not required to pretend their transport is MCP/local.
        assert!(local_sidecar(find("pi").unwrap()));
        assert!(native_sidecar(find("pi").unwrap()));
        assert!(native_sidecar(find("openhands").unwrap()));
        assert!(!local_sidecar(find("openhands").unwrap()));
    }

    #[test]
    fn final_response_uses_structured_events_not_prompt_or_tool_text() {
        let root = std::env::temp_dir().join(format!("asb-final-response-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let echoed = "Trace src/cli.py src/registry.py lookup\nASB_FINAL_RESPONSE: echoed prompt\n{\"type\":\"tool_call\",\"command\":\"cat src/registry.py\"}";
        assert!(final_response("codex", echoed, &root).is_none());
        let structured = "{\"type\":\"tool_call\",\"command\":\"cat src/registry.py\"}\n{\"type\":\"response.output_text.done\",\"text\":\"The call path is src/cli.py -> src/registry.py through lookup.\"}";
        assert!(final_response("codex", structured, &root).is_none());

        for event in [
            r#"{"type":"message","role":"tool","content":"I can't run tests"}"#,
            r#"{"type":"message","role":"user","content":"I can't run tests"}"#,
            r#"{"type":"message","content":"I can't run tests"}"#,
            r#"{"type":"agent_end","messages":[{"role":"tool","content":"I can't run tests"}]}"#,
            r#"{"type":"agent_end","messages":[{"role":"assistant","content":[{"type":"tool_result","content":"I can't run tests"}]}]}"#,
            r#"{"type":"agent_end","messages":[{"role":"assistant","content":[{"type":"thinking","text":"I can't run tests"}]}]}"#,
            r#"{"type":"agent_end","messages":[{"role":"assistant","content":"old answer"},{"role":"tool","content":"I can't run tests"}]}"#,
            r#"{"type":"agent_end","messages":[{"role":"assistant","stopReason":"toolUse","content":"I can't run tests"}]}"#,
        ] {
            assert!(final_response("pi", event, &root).is_none(), "{event}");
        }
        let mixed = r#"{"type":"agent_end","messages":[{"role":"assistant","content":[{"type":"text","text":"assistant final"},{"type":"tool_result","content":"I can't run tests"}]}]}"#;
        assert_eq!(
            final_response("pi", mixed, &root).as_deref(),
            Some("assistant final")
        );
        let wrapped =
            r#"{"type":"message","message":{"role":"assistant","content":"wrapped assistant"}}"#;

        for intermediate in [
            wrapped,
            r#"{"type":"message_update","assistantMessageEvent":{"type":"text","text":"I can't run tests"}}"#,
            r#"{"type":"turn_end","message":{"role":"assistant","content":"I can't run tests"}}"#,
        ] {
            assert!(
                final_response("pi", intermediate, &root).is_none(),
                "{intermediate}"
            );
        }

        let codex =
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"codex final"}}"#;
        assert!(final_response("codex", codex, &root).is_none());
        let opencode = concat!(
            "{\"type\":\"text\",\"part\":{\"type\":\"text\",\"messageID\":\"m1\",\"text\":\"opencode final\"}}\n",
            "{\"type\":\"step_finish\",\"part\":{\"messageID\":\"m1\",\"reason\":\"stop\"}}"
        );
        assert_eq!(
            final_response("opencode", opencode, &root).as_deref(),
            Some("opencode final")
        );
        let pi = r#"{"type":"agent_end","messages":[{"role":"assistant","content":[{"type":"text","text":"pi final"}]}]}"#;
        assert_eq!(final_response("pi", pi, &root).as_deref(), Some("pi final"));
        let mini_root = root.join("mini");
        std::fs::create_dir_all(&mini_root).unwrap();
        std::fs::write(
            mini_root.join(".asb-mini-swe.traj.json"),
            "{\n  \"messages\": [\n    {\n      \"role\": \"assistant\",\n      \"content\": [{\"type\": \"text\", \"text\": \"mini final\"}]\n    }\n  ],\n  \"exit\": \"submitted\"\n}\n",
        )
        .unwrap();
        assert_eq!(final_response("mini-swe", "", &mini_root).as_deref(), None,);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_agent_rejects_unadvertised_endpoint_pairs_before_side_effects() {
        use crate::endpoint::{Authentication, Connection, Interface};
        let root = std::env::temp_dir().join(format!(
            "asb-agent-rejected-endpoints-{}",
            std::process::id()
        ));
        assert!(!root.exists());
        let mut rejected = 0;
        for adapter in all() {
            for interface in [
                Interface::OpenAiResponses,
                Interface::OpenAiChat,
                Interface::AnthropicMessages,
                Interface::Gemini,
            ] {
                for authentication in [
                    Authentication::Bearer,
                    Authentication::ApiKey,
                    Authentication::None,
                ] {
                    if adapter.supported_endpoints().iter().any(|support| {
                        support.interface == interface
                            && support.authentication.contains(&authentication)
                    }) {
                        continue;
                    }
                    let connection = Connection {
                        interface,
                        authentication,
                        base_url: "https://secret-host.invalid/private?key=secret-query".into(),
                        credential: Some("secret-credential".into()),
                    };
                    let mut command = Command::new("must-not-execute");
                    command.arg("unchanged").env("FIXTURE", "unchanged");
                    let before = format!("{command:?}");
                    let error = adapter
                        .prepare(&mut PreparationContext {
                            command: &mut command,
                            model: "unused",
                            connection: &connection,
                            workspace: &root,
                            runtime_dir: &root,
                            run_dir: &root,
                            mcp_servers: &[],
                            mcp_tape: None,
                            tape: None,
                        })
                        .expect_err("unadvertised pair must fail without panicking")
                        .to_string();
                    assert!(error.contains(adapter.id()), "{error}");
                    assert!(error.contains(&format!("{interface:?}")), "{error}");
                    assert!(error.contains(&format!("{authentication:?}")), "{error}");
                    assert!(!error.contains("secret"), "{error}");
                    assert_eq!(format!("{command:?}"), before);
                    assert!(!root.exists(), "{} wrote configuration", adapter.id());
                    rejected += 1;
                }
            }
        }
        assert!(rejected > 0);
    }

    #[test]
    fn every_agent_declares_and_prepares_every_advertised_interface() {
        let root =
            std::env::temp_dir().join(format!("asb-agent-interface-matrix-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for adapter in all() {
            assert!(
                !adapter.supported_endpoints().is_empty(),
                "{}",
                adapter.id()
            );
            for (index, interface) in adapter.supported_endpoints().iter().enumerate() {
                assert!(!interface.authentication.is_empty(), "{}", adapter.id());
                for authentication in interface.authentication {
                    let runtime = root
                        .join(adapter.id())
                        .join(format!("{index}-{authentication:?}"));
                    let connection = crate::endpoint::Connection {
                        interface: interface.interface,
                        base_url: "http://127.0.0.1:4242/v1".into(),
                        credential: Some("asb-replay".into()),
                        authentication: *authentication,
                    };
                    let mut command = std::process::Command::new("unused");
                    adapter
                        .prepare(&mut PreparationContext {
                            command: &mut command,
                            model: "example/model",
                            connection: &connection,
                            workspace: &runtime,
                            runtime_dir: &runtime,
                            run_dir: &runtime,
                            mcp_servers: &[],
                            mcp_tape: None,
                            tape: None,
                        })
                        .unwrap_or_else(|error| panic!("{} {interface:?}: {error}", adapter.id()));
                }
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
