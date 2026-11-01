//! Private stdio MCP endpoint for tape-backed local agent tools.
//!
//! The endpoint is launched only by adapter-generated, run-private
//! configuration. It records canonical calls in record mode and applies only
//! the pre-recorded effects in replay mode.

use super::{
    PreparationContext, TapeContext, mcp_framing,
    tool_replay::{self, Effect, Event, SCHEMA_VERSION, Transcript},
};
use crate::Error;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_COMMAND_ARGUMENTS: usize = 32;
const MAX_COMMAND_ARGUMENT_BYTES: usize = 4096;
// A command result is represented twice in the MCP response: as compatible
// text and as structured content.  Keep each captured stream well below the
// 1 MiB framing/adapter capture boundary so a record-mode command that emits
// a large result becomes a sealed, replayable tool failure instead of causing
// the Pi extension's synchronous child to overflow and abandon its lease.
const MAX_COMMAND_OUTPUT: usize = 128 * 1024;
/// Command effects share the local-tool transcript's 1 MiB event ceiling.
/// The snapshot itself retains metadata only; file content is read only for a
/// changed regular UTF-8 file that must be reconstructed during replay.
const MAX_COMMAND_EFFECT_BYTES: usize = 768 * 1024;
const MAX_COMMAND_EFFECT_ENTRIES: usize = 256;
const MAX_COMMAND_SNAPSHOT_ENTRIES: usize = 4096;
const MAX_COMMAND_SNAPSHOT_BYTES: u64 = 16 * 1024 * 1024;
// Pi records every tool result in several structured output events.  Keep one
// confined file read comfortably below the outer agent-output capture bound,
// while exposing truncation to the model so it can use search or a narrower
// command instead of silently reasoning from partial source.
const MAX_MCP_READ_BYTES: usize = 48 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(45);
const STAGED_RUNTIME_EXECUTABLE: &str = "/opt/asb-runtime/asb";
/// Bump only when a provider-visible local-tool definition or its execution
/// semantics changes. A replay must use the same protocol that shaped its
/// recorded provider requests; this is not a compatibility normalizer.
const LOCAL_TOOL_PROTOCOL: &str = "asb-local-tools-v2-bounded-commands";
const TOOL_PROTOCOL_ARTIFACT: &str = "tool-protocol";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ToolProtocol {
    schema_version: u32,
    protocol: String,
}

pub(crate) fn entry(args: &[OsString]) -> std::process::ExitCode {
    match run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // This process is an adapter-private stdio server.  Its stdout is
            // reserved exclusively for JSON-RPC, but stderr is inherited by
            // the adapter and is the only safe way to explain a nonzero exit
            // to clients such as Pi.
            eprintln!("ASB local-tool service: {error}");
            std::process::ExitCode::from(2)
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    Record,
    Replay,
}

struct Options {
    mode: Mode,
    workspace: PathBuf,
    transcript: PathBuf,
    per_call_state: Option<PathBuf>,
    /// Optional run-private protocol progress trace.  It deliberately stores
    /// only method, request ordinal, ID shape, framing and response state;
    /// request parameters, tool arguments and tool results never enter it.
    metadata_trace: Option<PathBuf>,
    allow_commuted_observations: bool,
}

/// A restarted client may briefly overlap its old and new stdio MCP children.
/// The transcript and cursor are one coupled, run-private state machine, so a
/// second process must not read either halfway through the first process's
/// publication.  This create-new lease is deliberately fail-closed instead of
/// trying to merge two independently executed tool calls.
#[derive(Debug)]
struct PersistentStateLease {
    path: PathBuf,
}

impl PersistentStateLease {
    fn acquire(state: &Path) -> Result<Self, Error> {
        let path = state.with_extension("lock");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| Error::Run("local-tool persistent transcript is already in use".into()))?;
        if let Err(error) = file
            .write_all(b"asb-local-tool-state\n")
            .and_then(|_| file.sync_all())
        {
            let _ = fs::remove_file(&path);
            return Err(crate::io_error(error));
        }
        Ok(Self { path })
    }
}

impl Drop for PersistentStateLease {
    fn drop(&mut self) {
        // This is a private, create-new lease.  If removal fails, retain it so
        // a later process fails closed rather than overwriting uncertain state.
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PerCallState {
    schema_version: u32,
    cursor: usize,
    events: usize,
}

const PER_CALL_STATE_SCHEMA_VERSION: u32 = 1;

/// The adapter-facing, private stdio command for a single tape-backed run.
/// It is deliberately constructed here so every adapter gets the same sealed
/// transcript naming and replay fail-closed behavior.
pub(crate) struct Launch {
    pub program: std::path::PathBuf,
    pub args: Vec<std::ffi::OsString>,
}

/// Enable only for clients that dispatch independent MCP requests concurrently.
/// Effects remain strictly ordered by the transcript implementation.
pub(crate) fn allow_commuted_observations(mut launch: Launch) -> Launch {
    launch.args.push("--allow-commuted-observations".into());
    launch
}

pub(crate) fn launch(
    context: &PreparationContext<'_>,
    tape: TapeContext<'_>,
) -> Result<Launch, Error> {
    let transcript = match tape.mode {
        crate::tape::Mode::Record => {
            let transcript = context.run_dir.join("tool-replay/local-tools.jsonl");
            let parent = transcript
                .parent()
                .expect("local-tool transcript has a parent");
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::Run(format!(
                    "cannot create local-tool transcript directory: {error}"
                ))
            })?;
            // Some clients launch a configured MCP server lazily. Publish a
            // valid empty transcript before the client starts so an otherwise
            // tool-free recording has sealed replay evidence too.
            std::fs::write(&transcript, Transcript::default().encode()?).map_err(|error| {
                Error::Run(format!("cannot initialize local-tool transcript: {error}"))
            })?;
            transcript
        }
        crate::tape::Mode::Replay => replay_transcript(context, tape.cassette)?,
    };
    if tape.mode == crate::tape::Mode::Replay && !transcript.is_file() {
        return Err(Error::Run(
            "this replay requires its sealed local-tool transcript; record the benchmark again before replaying it".into(),
        ));
    }
    let program = runtime_program(std::env::var_os("ASB_RUNTIME_EXECUTABLE"))?;
    Ok(Launch {
        program,
        args: vec![
            "__asb-tool-replay".into(),
            "--mode".into(),
            match tape.mode {
                crate::tape::Mode::Record => "record".into(),
                crate::tape::Mode::Replay => "replay".into(),
            },
            "--workspace".into(),
            context.workspace.as_os_str().to_owned(),
            "--transcript".into(),
            transcript.into_os_string(),
        ],
    })
}

/// Bind an adapter's recorded provider request shape to the exact local-tool
/// protocol before the adapter is allowed to start. This makes a changed
/// tools/list definition a direct replay admission failure, rather than a
/// sequence of provider-side 409 mismatches.
pub(crate) fn prepare_protocol_artifact(
    context: &PreparationContext<'_>,
    tape: TapeContext<'_>,
) -> Result<(), Error> {
    let path = protocol_artifact_path(context, tape)?;
    match tape.mode {
        crate::tape::Mode::Record => {
            let bytes = protocol_bytes()?;
            std::fs::write(&path, bytes).map_err(|error| {
                Error::Run(format!(
                    "cannot publish local-tool protocol identity: {error}"
                ))
            })
        }
        crate::tape::Mode::Replay => {
            let bytes = std::fs::read(&path).map_err(|_| {
                Error::Run(
                    "this replay requires its sealed local-tool protocol identity; record the benchmark again before replaying it".into(),
                )
            })?;
            verify_protocol_bytes(&bytes)
        }
    }
}

pub(crate) fn verify_protocol_bytes(bytes: &[u8]) -> Result<(), Error> {
    let manifest: ToolProtocol = serde_json::from_slice(bytes)
        .map_err(|_| Error::Run("sealed local-tool protocol identity is malformed".into()))?;
    if manifest.schema_version != 1 || manifest.protocol != LOCAL_TOOL_PROTOCOL {
        return Err(Error::Run(
            "sealed local-tool protocol differs from this ASB build; replay requires the recorded tool definition and execution semantics".into(),
        ));
    }
    Ok(())
}

fn protocol_bytes() -> Result<Vec<u8>, Error> {
    serde_json::to_vec(&ToolProtocol {
        schema_version: 1,
        protocol: LOCAL_TOOL_PROTOCOL.into(),
    })
    .map_err(|_| Error::Run("cannot encode local-tool protocol identity".into()))
}

fn protocol_artifact_path(
    context: &PreparationContext<'_>,
    tape: TapeContext<'_>,
) -> Result<PathBuf, Error> {
    match tape.mode {
        crate::tape::Mode::Record => Ok(context
            .run_dir
            .join("tool-replay")
            .join(format!("{TOOL_PROTOCOL_ARTIFACT}.json"))),
        crate::tape::Mode::Replay => {
            let name = tape
                .cassette
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    Error::Run("replay cassette has no valid private filename".into())
                })?;
            let suffix = name
                .strip_prefix("replay")
                .and_then(|suffix| suffix.strip_suffix(".jsonl"))
                .ok_or_else(|| {
                    Error::Run("replay cassette has an invalid private filename".into())
                })?;
            let parent = tape
                .cassette
                .parent()
                .ok_or_else(|| Error::Run("replay cassette has no private directory".into()))?;
            Ok(parent.join(format!("{TOOL_PROTOCOL_ARTIFACT}{suffix}.json")))
        }
    }
}

/// Add a run-private, payload-free MCP progress trace to an otherwise normal
/// launch.  This is for adapter compatibility diagnosis only: it is neither a
/// cassette artifact nor user-facing evidence.
pub(crate) fn with_metadata_trace(mut launch: Launch, path: &Path) -> Launch {
    launch
        .args
        .extend(["--metadata-trace".into(), path.as_os_str().to_owned()]);
    launch
}

/// Persist a bounded per-call transcript cursor under the owning adapter's
/// selected state filename when its client restarts the private MCP service.
pub(crate) fn per_call_launch(
    context: &PreparationContext<'_>,
    tape: TapeContext<'_>,
    state_name: &str,
) -> Result<Launch, Error> {
    if !valid_progress_filename(state_name) {
        return Err(Error::Run("local-tool progress filename is invalid".into()));
    }
    let mut launch = launch(context, tape)?;
    let state = per_call_state_path(context.run_dir, state_name);
    let transcript = launch
        .args
        .windows(2)
        .find(|pair| pair[0] == "--transcript")
        .map(|pair| PathBuf::from(&pair[1]))
        .ok_or_else(|| Error::Run("local-tool launch has no transcript".into()))?;
    let events = Transcript::decode(
        &std::fs::read(&transcript)
            .map_err(|_| Error::Run("local-tool transcript is unavailable".into()))?,
    )?
    .len();
    publish_per_call_state(&state, 0, events)?;
    launch
        .args
        .extend(["--per-call-state".into(), state.into_os_string()]);
    Ok(launch)
}

fn valid_progress_filename(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(parts.next(), Some(std::path::Component::Normal(_))) && parts.next().is_none()
}

fn per_call_state_path(run_dir: &Path, name: &str) -> PathBuf {
    run_dir.join("tool-replay").join(name)
}

/// Verify a frozen per-call receipt after an isolated guest has returned its
/// adapter-owned artifacts to the host.  The caller owns which receipt it
/// requests; this utility validates only the common bounded cursor contract.
pub(crate) fn verify_persistent_per_call_completion_receipt(
    receipt: Option<&serde_json::Value>,
    transcript: Option<&[u8]>,
) -> Result<(), Error> {
    let receipt = receipt.map(read_per_call_state_value).transpose()?;
    verify_per_call_completion_receipt(receipt, transcript)
}

fn verify_per_call_completion_receipt(
    receipt: Option<PerCallState>,
    transcript: Option<&[u8]>,
) -> Result<(), Error> {
    match (receipt, transcript) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(Error::Run("local-tool progress is missing".into())),
        (Some(state), None) => (state.cursor == state.events)
            .then_some(())
            .ok_or_else(|| Error::Run("local-tool replay left recorded events unconsumed".into())),
        (Some(state), Some(bytes)) => {
            let transcript = Transcript::decode(bytes)?;
            if state.events == transcript.len() && state.cursor == transcript.len() {
                Ok(())
            } else {
                Err(Error::Run(
                    "local-tool replay left recorded events unconsumed".into(),
                ))
            }
        }
    }
}

/// Guest workloads stage an executable launcher with its matching dynamic
/// loader under `/opt/asb-runtime`. Child tool servers must use that launcher
/// rather than the raw executable path: the latter can lack its staged loader
/// and never answer the client's stdio request.
pub(crate) fn runtime_program(staged: Option<std::ffi::OsString>) -> Result<PathBuf, Error> {
    if staged.as_deref() == Some(std::ffi::OsStr::new(STAGED_RUNTIME_EXECUTABLE)) {
        return Ok(PathBuf::from(STAGED_RUNTIME_EXECUTABLE));
    }
    std::env::current_exe().map_err(|error| {
        Error::Run(format!(
            "cannot locate the ASB local-tool replay executable: {error}"
        ))
    })
}

fn replay_transcript(context: &PreparationContext<'_>, cassette: &Path) -> Result<PathBuf, Error> {
    let name = cassette
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Run("replay cassette has no valid private filename".into()))?;
    let suffix = name
        .strip_prefix("replay")
        .and_then(|suffix| suffix.strip_suffix(".jsonl"));
    if suffix.is_none() {
        // Historical provider-only JSONL input has no local-tool evidence.
        // Keep it usable only with an explicit empty transcript: any tool call
        // then fails causally instead of escaping to the host.
        let transcript = context.runtime_dir.join("legacy-empty-local-tools.jsonl");
        std::fs::write(&transcript, Transcript::default().encode()?).map_err(crate::io_error)?;
        return Ok(transcript);
    }
    let suffix = suffix.expect("checked above");
    let parent = cassette
        .parent()
        .ok_or_else(|| Error::Run("replay cassette has no private directory".into()))?;
    Ok(parent.join(format!("local-tools{suffix}.jsonl")))
}

fn run(args: &[OsString]) -> Result<(), Error> {
    let options = options(args)?;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut input = BufReader::new(stdin.lock());
    run_with_io(options, &mut input, &mut output)
}

/// Run one sealed local-tool session over already-bounded stdio framing.
/// Keeping the session body independent of process-global stdio lets tests
/// prove that Pi's short-lived per-call service persists both a failure event
/// and its progress sidecar before reporting the failure to the extension.
fn run_with_io(
    options: Options,
    input: &mut impl std::io::BufRead,
    output: &mut impl Write,
) -> Result<(), Error> {
    let _lease = options
        .per_call_state
        .as_deref()
        .map(PersistentStateLease::acquire)
        .transpose()?;
    let mut metadata_trace = options
        .metadata_trace
        .as_deref()
        .map(MetadataTrace::open)
        .transpose()?;
    let mut transcript = match options.mode {
        Mode::Record if options.per_call_state.is_some() => Transcript::decode(
            &std::fs::read(&options.transcript)
                .map_err(|_| Error::Run("local-tool record transcript is unavailable".into()))?,
        )?,
        Mode::Record => Transcript::default(),
        Mode::Replay => Transcript::decode(
            &std::fs::read(&options.transcript)
                .map_err(|_| Error::Run("local-tool replay transcript is unavailable".into()))?,
        )?,
    };
    if let Some(state) = &options.per_call_state {
        let state = read_per_call_state(state)?;
        if state.events != transcript.len() {
            return Err(Error::Run(
                "local-tool per-call progress does not bind its transcript".into(),
            ));
        }
        if options.mode == Mode::Replay {
            transcript.set_cursor(state.cursor)?;
        }
    }
    let operation = (|| {
        while let Some(request) = mcp_framing::read(input, "local-tool MCP")? {
            let request_trace = metadata_trace
                .as_mut()
                .map(|trace| {
                    trace.request(
                        request.value.get("method").and_then(Value::as_str),
                        request.value.get("id"),
                        request
                            .value
                            .get("params")
                            .and_then(|params| params.get("name"))
                            .and_then(Value::as_str),
                        request.framing,
                    )
                })
                .transpose()?;
            let response = match respond(&options, &mut transcript, &request.value) {
                Ok(response) => response,
                Err(error) => {
                    if let (Some(trace), Some(sequence)) = (metadata_trace.as_mut(), request_trace)
                    {
                        trace.response(sequence, "error", None)?;
                    }
                    return Err(error);
                }
            };
            if let Some(response) = response {
                // A client can terminate a short-lived service immediately
                // after receiving a response. Persist the corresponding
                // transcript and cursor before exposing any response bytes.
                if options.mode == Mode::Record {
                    publish_record_transcript(&options.transcript, &transcript.encode()?)?;
                }
                if let Some(state) = &options.per_call_state {
                    let cursor = match options.mode {
                        Mode::Record => transcript.len(),
                        Mode::Replay => transcript.cursor(),
                    };
                    publish_per_call_state(state, cursor, transcript.len())?;
                }
                mcp_framing::write(output, &response, request.framing, "local-tool MCP")?;
                if let (Some(trace), Some(sequence)) = (metadata_trace.as_mut(), request_trace) {
                    trace.response(sequence, "result", Some(&response))?;
                }
            } else if let (Some(trace), Some(sequence)) = (metadata_trace.as_mut(), request_trace) {
                trace.response(sequence, "notification", None)?;
            }
        }
        Ok(())
    })();
    match options.mode {
        Mode::Record => {
            let bytes = transcript.encode()?;
            publish_record_transcript(&options.transcript, &bytes)?;
        }
        Mode::Replay if options.per_call_state.is_none() => transcript.finish()?,
        Mode::Replay => {}
    };
    if let Some(state) = &options.per_call_state {
        let cursor = match options.mode {
            // Recording appends one event per short-lived service process;
            // its progress is the sealed transcript length rather than the
            // replay cursor, which intentionally stays at zero in record mode.
            Mode::Record => transcript.len(),
            Mode::Replay => transcript.cursor(),
        };
        publish_per_call_state(state, cursor, transcript.len())?;
    }
    operation
}

fn read_per_call_state(path: &Path) -> Result<PerCallState, Error> {
    let bytes = std::fs::read(path)
        .map_err(|_| Error::Run("local-tool per-call progress is unavailable".into()))?;
    if bytes.len() > 1024 {
        return Err(Error::Run(
            "local-tool per-call progress exceeds its bound".into(),
        ));
    }
    let state: PerCallState = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Run("local-tool per-call progress is malformed".into()))?;
    if state.schema_version != PER_CALL_STATE_SCHEMA_VERSION {
        return Err(Error::Run(
            "local-tool per-call progress has an unsupported schema".into(),
        ));
    }
    Ok(state)
}

fn read_per_call_state_value(value: &serde_json::Value) -> Result<PerCallState, Error> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| Error::Run("local-tool per-call progress is malformed".into()))?;
    if bytes.len() > 1024 {
        return Err(Error::Run(
            "local-tool per-call progress exceeds its bound".into(),
        ));
    }
    let state: PerCallState = serde_json::from_value(value.clone())
        .map_err(|_| Error::Run("local-tool per-call progress is malformed".into()))?;
    if state.schema_version != PER_CALL_STATE_SCHEMA_VERSION {
        return Err(Error::Run(
            "local-tool per-call progress has an unsupported schema".into(),
        ));
    }
    Ok(state)
}

fn publish_per_call_state(path: &Path, cursor: usize, events: usize) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Run("local-tool progress has no private namespace".into()))?;
    std::fs::create_dir_all(parent)
        .map_err(|_| Error::Run("cannot create local-tool progress namespace".into()))?;
    let bytes = serde_json::to_vec(&PerCallState {
        schema_version: PER_CALL_STATE_SCHEMA_VERSION,
        cursor,
        events,
    })
    .map_err(|_| Error::Run("cannot render local-tool per-call progress".into()))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, bytes).map_err(crate::io_error)?;
    std::fs::File::open(&temporary)
        .and_then(|file| file.sync_all())
        .map_err(crate::io_error)?;
    std::fs::rename(&temporary, path).map_err(crate::io_error)?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(crate::io_error)
}

fn publish_record_transcript(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Run("local-tool transcript has no private namespace".into()))?;
    if !parent.is_dir() {
        return Err(Error::Run(
            "local-tool transcript namespace is unavailable".into(),
        ));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| Error::Run("local-tool transcript was not initialized".into()))?;
    if !metadata.file_type().is_file() {
        return Err(Error::Run(
            "local-tool transcript is not a regular file".into(),
        ));
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|_| Error::Run("cannot publish local-tool transcript".into()))?;
    file.write_all(bytes).map_err(crate::io_error)?;
    file.sync_all().map_err(crate::io_error)?;
    std::fs::rename(&temporary, path)
        .map_err(|_| Error::Run("cannot publish local-tool transcript".into()))?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(crate::io_error)
}

fn options(args: &[OsString]) -> Result<Options, Error> {
    let mut mode = None;
    let mut workspace = None;
    let mut transcript = None;
    let mut per_call_state = None;
    let mut metadata_trace = None;
    let mut allow_commuted_observations = false;
    let mut values = args.iter();
    while let Some(flag) = values.next() {
        if flag == "--allow-commuted-observations" {
            allow_commuted_observations = true;
            continue;
        }
        let value = values
            .next()
            .ok_or_else(|| Error::Input("invalid local-tool MCP arguments".into()))?;
        match flag.to_string_lossy().as_ref() {
            "--mode" => {
                mode = Some(match value.to_string_lossy().as_ref() {
                    "record" => Mode::Record,
                    "replay" => Mode::Replay,
                    _ => return Err(Error::Input("invalid local-tool replay mode".into())),
                })
            }
            "--workspace" => workspace = Some(PathBuf::from(value)),
            "--transcript" => transcript = Some(PathBuf::from(value)),
            "--per-call-state" => per_call_state = Some(PathBuf::from(value)),
            "--metadata-trace" => metadata_trace = Some(PathBuf::from(value)),
            _ => return Err(Error::Input("invalid local-tool MCP arguments".into())),
        }
    }
    Ok(Options {
        mode: mode.ok_or_else(|| Error::Input("local-tool MCP mode is required".into()))?,
        workspace: workspace
            .ok_or_else(|| Error::Input("local-tool MCP workspace is required".into()))?,
        transcript: transcript
            .ok_or_else(|| Error::Input("local-tool MCP transcript is required".into()))?,
        per_call_state,
        metadata_trace,
        allow_commuted_observations,
    })
}

struct MetadataTrace {
    file: std::fs::File,
    next_sequence: u64,
}

impl MetadataTrace {
    fn open(path: &Path) -> Result<Self, Error> {
        let parent = path
            .parent()
            .ok_or_else(|| Error::Run("local-tool metadata trace has no parent".into()))?;
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::Run(format!("cannot create local-tool metadata trace: {error}"))
        })?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path).map_err(|error| {
            Error::Run(format!("cannot create local-tool metadata trace: {error}"))
        })?;
        Ok(Self {
            file,
            next_sequence: 1,
        })
    }

    fn request(
        &mut self,
        method: Option<&str>,
        id: Option<&Value>,
        tool: Option<&str>,
        framing: mcp_framing::Framing,
    ) -> Result<u64, Error> {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        // A trace-write failure is surfaced before we execute a tool, avoiding
        // a misleading partial diagnostic record.
        self.write(json!({
            "schema_version": 1,
            "sequence": sequence,
            "event": "request",
            "method": method.unwrap_or("<missing>"),
            "tool": tool,
            "id_shape": id_shape(id),
            "framing": framing,
        }))?;
        Ok(sequence)
    }

    fn response(
        &mut self,
        sequence: u64,
        status: &str,
        response: Option<&Value>,
    ) -> Result<(), Error> {
        self.write(json!({
            "schema_version": 1,
            "sequence": sequence,
            "event": "response",
            "status": status,
            "response_shape": response_shape(response),
        }))
    }

    fn write(&mut self, value: Value) -> Result<(), Error> {
        let bytes = serde_json::to_vec(&value)
            .map_err(|_| Error::Run("cannot encode local-tool metadata trace".into()))?;
        self.file
            .write_all(&bytes)
            .and_then(|()| self.file.write_all(b"\n"))
            .and_then(|()| self.file.sync_data())
            .map_err(|error| Error::Run(format!("cannot write local-tool metadata trace: {error}")))
    }
}

/// Retain only fixed protocol-envelope facts after a successful write+flush.
/// Values, content text, tool arguments and results are intentionally absent.
fn response_shape(response: Option<&Value>) -> &'static str {
    let Some(result) = response
        .and_then(Value::as_object)
        .and_then(|value| value.get("result"))
        .and_then(Value::as_object)
    else {
        return "other";
    };
    let has_content = result
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| !content.is_empty());
    let structured = result
        .get("structuredContent")
        .is_some_and(Value::is_object);
    let successful = result.get("isError").and_then(Value::as_bool) == Some(false);
    match (has_content, structured, successful) {
        (true, true, true) => "content_structured_success",
        (true, false, _) => "content",
        _ => "other",
    }
}

fn id_shape(id: Option<&Value>) -> &'static str {
    match id {
        None => "absent",
        Some(Value::Null) => "null",
        Some(Value::String(_)) => "string",
        Some(Value::Number(_)) => "number",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

fn respond(
    options: &Options,
    transcript: &mut Transcript,
    request: &Value,
) -> Result<Option<Value>, Error> {
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return Err(Error::Run("local-tool MCP request has no method".into()));
    };
    let Some(id) = request.get("id") else {
        return Ok(None);
    };
    let result = match method {
        "initialize" => {
            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"asb-local-tools","version":"1"}})
        }
        "tools/list" => json!({"tools":[
            {"name":"list_directory","description":"List direct non-symlink entries inside the benchmark workspace.","inputSchema":{"type":"object","properties":{"path":{"type":"string"}}}},
            {"name":"search_text","description":"Search bounded UTF-8 source text inside the benchmark workspace.","inputSchema":{"type":"object","required":["query"],"properties":{"path":{"type":"string"},"query":{"type":"string"}}}},
            {"name":"run_command","description":"Run a bounded workspace command. Results and workspace effects are sealed for replay.","inputSchema":{"type":"object","required":["argv"],"properties":{"argv":{"type":"array","items":{"type":"string"}}}}},
            {"name":"read_file","description":"Read a UTF-8 file inside the benchmark workspace.","inputSchema":{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}},
            {"name":"write_file","description":"Create or replace a UTF-8 file inside the benchmark workspace.","inputSchema":{"type":"object","required":["path","content"],"properties":{"path":{"type":"string"},"content":{"type":"string"}}}},
            {"name":"create_directory","description":"Create a directory inside the benchmark workspace.","inputSchema":{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}},
            {"name":"remove_file","description":"Remove a file inside the benchmark workspace.","inputSchema":{"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}}
        ]}),
        "tools/call" => match call(
            options,
            transcript,
            request.get("params").cloned().unwrap_or(Value::Null),
        ) {
            Ok(result) => result,
            // A tool can legitimately fail (for example, the first test run
            // of a bug-fix task).  Preserve that failure in the sealed
            // transcript, but return it through MCP rather than terminating
            // the server.  Clients such as WorkBuddy otherwise lose their
            // stdio connection and cannot issue the follow-up inspection and
            // repair calls that make a record/replay attempt useful.
            Err(error) => {
                return Ok(Some(json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "error":{"code":-32000,"message":error.to_string()},
                })));
            }
        },
        _ => {
            return Ok(Some(
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method unavailable"}}),
            ));
        }
    };
    Ok(Some(json!({"jsonrpc":"2.0","id":id,"result":result})))
}

fn call(options: &Options, transcript: &mut Transcript, params: Value) -> Result<Value, Error> {
    let tool = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Run("local-tool MCP call has no name".into()))?;
    let request = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let request = canonical_request(tool, request)?;
    let result = if options.mode == Mode::Replay {
        transcript.replay_with_commuted_observations(
            &options.workspace,
            tool,
            &request,
            options.allow_commuted_observations,
        )?
    } else {
        let recorded = match execute(&options.workspace, tool, &request) {
            Ok((result, effects)) => Event {
                schema_version: SCHEMA_VERSION,
                sequence: transcript.len() as u64 + 1,
                tool: tool.into(),
                request: request.clone(),
                result,
                failure: None,
                effects,
            },
            Err(error) => Event {
                schema_version: SCHEMA_VERSION,
                sequence: transcript.len() as u64 + 1,
                tool: tool.into(),
                request: request.clone(),
                result: Value::Null,
                failure: Some(recordable_failure(&error)?),
                effects: Vec::new(),
            },
        };
        let failure = recorded.failure.clone();
        let result = recorded.result.clone();
        transcript.record(recorded)?;
        if let Some(failure) = failure {
            return Err(Error::Run(failure));
        }
        result
    };
    let text = serde_json::to_string(&result)
        .map_err(|_| Error::Run("cannot render local-tool result".into()))?;
    // `isError` is optional in MCP, but explicit success matters to clients
    // which keep a tool turn pending until its terminal outcome is declared.
    // Keep the historical text content for broadly compatible clients while
    // also publishing the bounded value in the standard structured field.
    Ok(json!({
        "content":[{"type":"text","text":text}],
        "structuredContent": result,
        "isError": false,
    }))
}

/// Failure turns are sealed only when they are already small, single-line
/// local diagnostics.  This preserves causal replay without turning a tool
/// transcript into a carrier for arbitrary process output.
fn recordable_failure(error: &Error) -> Result<String, Error> {
    let failure = error.to_string();
    if failure.is_empty()
        || failure.len() > 512
        || failure.contains(['\n', '\r'])
        || failure.chars().any(char::is_control)
    {
        return Err(Error::Run(
            "local-tool failure is not safe for replay recording".into(),
        ));
    }
    Ok(failure)
}

/// Retain only fields defined by the local-tool protocol.  Some agent SDKs
/// decorate tool arguments with transport metadata; that metadata has no
/// effect on execution and must not make an otherwise identical replay fail.
fn canonical_request(tool: &str, request: Value) -> Result<Value, Error> {
    let object = request
        .as_object()
        .ok_or_else(|| Error::Run("local-tool arguments must be an object".into()))?;
    let fields: &[&str] = match tool {
        "list_directory" => &["path"],
        "search_text" => &["path", "query"],
        "run_command" => &["argv"],
        "read_file" => &["path"],
        "write_file" => &["path", "content"],
        "create_directory" | "remove_file" => &["path"],
        _ => return Err(Error::Run("local-tool is unsupported".into())),
    };
    Ok(Value::Object(
        fields
            .iter()
            .filter_map(|field| {
                object
                    .get(*field)
                    .cloned()
                    .map(|value| ((*field).into(), value))
            })
            .collect(),
    ))
}

fn execute(root: &Path, tool: &str, request: &Value) -> Result<(Value, Vec<Effect>), Error> {
    match tool {
        "list_directory" => {
            let path = request.get("path").and_then(Value::as_str).unwrap_or(".");
            let entries = tool_replay::list_directory(root, path)?
                .into_iter()
                .map(|(name, directory)| json!({"name":name,"directory":directory}))
                .collect::<Vec<_>>();
            Ok((json!({"entries":entries}), Vec::new()))
        }
        "search_text" => {
            let path = request.get("path").and_then(Value::as_str).unwrap_or(".");
            let query = request
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Run("local-tool search needs a query".into()))?;
            let matches = tool_replay::search_text(root, path, query)?
                .into_iter()
                .map(|(path, line, text)| json!({"path":path,"line":line,"text":text}))
                .collect::<Vec<_>>();
            Ok((json!({"matches":matches}), Vec::new()))
        }
        "run_command" => {
            let before = command_snapshot(root)?;
            let result = run_command(root, request)?;
            let after = command_snapshot(root)?;
            Ok((result, command_effects(&before, &after, root)?))
        }
        "read_file" => Ok((
            bounded_read_result(tool_replay::read_text(root, required_path(request)?)?),
            Vec::new(),
        )),
        "write_file" => {
            let effects = vec![Effect::WriteText {
                path: required_path(request)?.into(),
                content: request
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::Run("local-tool write needs content".into()))?
                    .into(),
            }];
            for effect in &effects {
                tool_replay::apply(root, effect)?;
            }
            Ok((json!({"ok":true}), effects))
        }
        "create_directory" => {
            let effects = vec![Effect::CreateDirectory {
                path: required_path(request)?.into(),
            }];
            for effect in &effects {
                tool_replay::apply(root, effect)?;
            }
            Ok((json!({"ok":true}), effects))
        }
        "remove_file" => {
            let effects = vec![Effect::RemoveFile {
                path: required_path(request)?.into(),
            }];
            for effect in &effects {
                tool_replay::apply(root, effect)?;
            }
            Ok((json!({"ok":true}), effects))
        }
        _ => Err(Error::Run("local-tool is unsupported".into())),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CommandEntry {
    Directory,
    File { bytes: u64, sha256: [u8; 32] },
}

type CommandSnapshot = BTreeMap<String, CommandEntry>;

/// A deliberately small workspace identity for a locally executed command.
/// It follows neither symlinks nor build/cache trees. It identifies source
/// changes without retaining workspace copies or adding syscall/kernel tracing.
fn command_snapshot(root: &Path) -> Result<CommandSnapshot, Error> {
    let root = root
        .canonicalize()
        .map_err(|_| Error::Run("local-tool workspace is unavailable".into()))?;
    let mut pending = vec![(root, PathBuf::new())];
    let mut entries = CommandSnapshot::new();
    let mut snapshot_bytes = 0_u64;
    while let Some((directory, relative)) = pending.pop() {
        let mut children = fs::read_dir(&directory)
            .map_err(|_| Error::Run("cannot snapshot local-tool workspace".into()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| Error::Run("cannot snapshot local-tool workspace".into()))?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let name = child
                .file_name()
                .into_string()
                .map_err(|_| Error::Run("local-tool workspace has a non-UTF-8 path".into()))?;
            if matches!(
                name.as_str(),
                ".git" | "target" | "node_modules" | "__pycache__" | ".pytest_cache"
            ) {
                continue;
            }
            if entries.len() >= MAX_COMMAND_SNAPSHOT_ENTRIES {
                return Err(Error::Run(
                    "local-tool command workspace exceeds snapshot bound".into(),
                ));
            }
            let path = child.path();
            let relative = relative.join(&name);
            let relative_text = relative
                .to_str()
                .ok_or_else(|| Error::Run("local-tool workspace has a non-UTF-8 path".into()))?
                .to_owned();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| Error::Run("cannot inspect local-tool workspace".into()))?;
            if metadata.file_type().is_symlink() {
                return Err(Error::Run(
                    "local-tool command workspace contains a symlink".into(),
                ));
            }
            if metadata.is_dir() {
                entries.insert(relative_text, CommandEntry::Directory);
                pending.push((path, relative));
            } else if metadata.is_file() {
                let sha256 = command_file_identity(&path, metadata.len(), &mut snapshot_bytes)?;
                entries.insert(
                    relative_text,
                    CommandEntry::File {
                        bytes: metadata.len(),
                        sha256,
                    },
                );
            } else {
                return Err(Error::Run(
                    "local-tool command workspace contains an unsupported file".into(),
                ));
            }
        }
    }
    Ok(entries)
}

/// Hash each admissible source file once per snapshot.  The digest catches a
/// same-length rewrite even on a filesystem with coarse or restored mtimes,
/// while the shared byte ceiling keeps command replay lighter than a workspace
/// copy or syscall-level recorder.
fn command_file_identity(path: &Path, bytes: u64, consumed: &mut u64) -> Result<[u8; 32], Error> {
    *consumed = consumed.checked_add(bytes).ok_or_else(|| {
        Error::Run("local-tool command workspace exceeds snapshot byte bound".into())
    })?;
    if *consumed > MAX_COMMAND_SNAPSHOT_BYTES {
        return Err(Error::Run(
            "local-tool command workspace exceeds snapshot byte bound".into(),
        ));
    }
    let mut file = fs::File::open(path)
        .map_err(|_| Error::Run("cannot read local-tool workspace source".into()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut read = 0_u64;
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| Error::Run("cannot read local-tool workspace source".into()))?;
        if count == 0 {
            break;
        }
        read = read.checked_add(count as u64).ok_or_else(|| {
            Error::Run("local-tool command workspace exceeds snapshot byte bound".into())
        })?;
        if read > bytes {
            return Err(Error::Run(
                "local-tool workspace source changed during snapshot".into(),
            ));
        }
        hasher.update(&buffer[..count]);
    }
    if read != bytes {
        return Err(Error::Run(
            "local-tool workspace source changed during snapshot".into(),
        ));
    }
    Ok(hasher.finalize().into())
}

/// Emit only changed source state. Replay applies removals first, then
/// directories and files, which also handles a bounded file-kind replacement.
fn command_effects(
    before: &CommandSnapshot,
    after: &CommandSnapshot,
    root: &Path,
) -> Result<Vec<Effect>, Error> {
    let mut effects = Vec::new();
    let mut removed = BTreeSet::new();
    for path in before.keys().filter(|path| !after.contains_key(*path)) {
        removed.insert(path.clone());
    }
    for (path, entry) in before {
        if let Some(after_entry) = after.get(path)
            && std::mem::discriminant(entry) != std::mem::discriminant(after_entry)
        {
            removed.insert(path.clone());
        }
    }
    for path in removed.iter().filter(|path| {
        Path::new(path)
            .parent()
            .and_then(Path::to_str)
            .is_none_or(|parent| !removed.contains(parent))
    }) {
        match before.get(path) {
            Some(CommandEntry::Directory) => {
                effects.push(Effect::RemoveDirectory { path: path.clone() });
            }
            Some(CommandEntry::File { .. }) => {
                effects.push(Effect::RemoveFile { path: path.clone() })
            }
            None => {
                return Err(Error::Run(
                    "local-tool command ledger is inconsistent".into(),
                ));
            }
        }
    }
    for (path, entry) in after {
        if matches!(entry, CommandEntry::Directory) && before.get(path) != Some(entry) {
            effects.push(Effect::CreateDirectory { path: path.clone() });
        }
    }
    let mut content_bytes = 0usize;
    for (path, entry) in after {
        if before.get(path) != Some(entry) && matches!(entry, CommandEntry::File { .. }) {
            let content = tool_replay::read_text(root, path)?;
            content_bytes = content_bytes
                .checked_add(content.len())
                .ok_or_else(|| Error::Run("local-tool command effects exceed bound".into()))?;
            if content_bytes > MAX_COMMAND_EFFECT_BYTES {
                return Err(Error::Run("local-tool command effects exceed bound".into()));
            }
            effects.push(Effect::WriteText {
                path: path.clone(),
                content,
            });
        }
    }
    if effects.len() > MAX_COMMAND_EFFECT_ENTRIES {
        return Err(Error::Run(
            "local-tool command effects exceed entry bound".into(),
        ));
    }
    Ok(effects)
}

fn bounded_read_result(text: String) -> Value {
    if text.len() <= MAX_MCP_READ_BYTES {
        return json!({"text": text, "truncated": false});
    }
    let mut end = MAX_MCP_READ_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    json!({
        "text": &text[..end],
        "truncated": true,
        "limit_bytes": MAX_MCP_READ_BYTES,
    })
}

fn run_command(root: &Path, request: &Value) -> Result<Value, Error> {
    let argv = request
        .get("argv")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Run("local-tool command needs argv".into()))?;
    if argv.is_empty() || argv.len() > MAX_COMMAND_ARGUMENTS {
        return Err(Error::Run(
            "local-tool command argument count is invalid".into(),
        ));
    }
    let argv = argv
        .iter()
        .map(|value| {
            value.as_str().filter(|value| {
                !value.is_empty()
                    && value.len() <= MAX_COMMAND_ARGUMENT_BYTES
                    && !value.as_bytes().contains(&0)
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| Error::Run("local-tool command arguments are invalid".into()))?;
    let workspace = root
        .canonicalize()
        .map_err(|_| Error::Run("local-tool workspace is unavailable".into()))?;
    let mut command = Command::new(argv[0]);
    command
        .args(&argv[1..])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| Error::Run("cannot start local-tool command".into()))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let read = |mut reader: std::process::ChildStdout| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            reader
                .by_ref()
                .take((MAX_COMMAND_OUTPUT + 1) as u64)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        })
    };
    let stdout_reader = read(stdout);
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        BufReader::new(stderr)
            .by_ref()
            .take((MAX_COMMAND_OUTPUT + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Run(
                    "local-tool command exceeded its time bound".into(),
                ));
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| Error::Run("cannot capture local-tool command output".into()))?
        .map_err(crate::io_error)?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| Error::Run("cannot capture local-tool command output".into()))?
        .map_err(crate::io_error)?;
    if stdout.len() > MAX_COMMAND_OUTPUT || stderr.len() > MAX_COMMAND_OUTPUT {
        return Err(Error::Run("local-tool command output exceeds bound".into()));
    }
    Ok(json!({
        "exit_code": status.code(),
        "success": status.success(),
        "stdout": String::from_utf8_lossy(&stdout),
        "stderr": String::from_utf8_lossy(&stderr),
    }))
}

fn required_path(request: &Value) -> Result<&str, Error> {
    request
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Run("local-tool request needs a path".into()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn adapter_progress_names_are_single_local_components() {
        for name in [
            "pi-progress.json",
            "persistent-progress.json",
            "another-adapter.json",
        ] {
            assert!(super::valid_progress_filename(name), "{name}");
        }
        for name in ["", ".", "..", "../state", "nested/state", "/absolute"] {
            assert!(!super::valid_progress_filename(name), "{name}");
        }
    }
    use super::*;
    use std::io::{BufRead, Read};

    struct InputThatChecksPublishedPrefix {
        bytes: Vec<u8>,
        cursor: usize,
        transcript: std::path::PathBuf,
        checked: bool,
    }

    impl Read for InputThatChecksPublishedPrefix {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            let available = self.fill_buf()?;
            let count = available.len().min(output.len());
            output[..count].copy_from_slice(&available[..count]);
            self.consume(count);
            Ok(count)
        }
    }

    impl BufRead for InputThatChecksPublishedPrefix {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            if self.cursor < self.bytes.len() {
                return Ok(&self.bytes[self.cursor..]);
            }
            if !self.checked {
                self.checked = true;
                assert_eq!(
                    Transcript::decode(&std::fs::read(&self.transcript).unwrap())
                        .unwrap()
                        .len(),
                    1,
                    "the completed tool response must be durable before another request or EOF"
                );
            }
            Ok(&[])
        }

        fn consume(&mut self, amount: usize) {
            self.cursor = self.cursor.saturating_add(amount).min(self.bytes.len());
        }
    }

    #[test]
    fn record_publishes_each_framed_tool_response_before_stdio_eof() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-published-prefix-{}",
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        let transcript = root.join("run/tool-replay/local-tools.jsonl");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(workspace.join("source.txt"), "available\n").unwrap();
        std::fs::write(&transcript, Transcript::default().encode().unwrap()).unwrap();
        let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"source.txt\"}}}\n";
        let mut input = InputThatChecksPublishedPrefix {
            bytes: request.to_vec(),
            cursor: 0,
            transcript: transcript.clone(),
            checked: false,
        };
        let mut output = Vec::new();
        run_with_io(
            Options {
                mode: Mode::Record,
                workspace,
                transcript,
                per_call_state: None,
                metadata_trace: None,
                allow_commuted_observations: false,
            },
            &mut input,
            &mut output,
        )
        .unwrap();
        assert!(input.checked);
        assert!(!output.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn metadata_trace_retains_protocol_progress_without_payloads() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-metadata-trace-{}",
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        let transcript = root.join("tool-replay/local-tools.jsonl");
        let trace = root.join("tool-replay/metadata.jsonl");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, Transcript::default().encode().unwrap()).unwrap();
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"initialize\",\"params\":{\"secret\":\"not-retained\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\",\"params\":{\"secret\":\"not-retained\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"tools/call\",\"params\":{\"name\":\"list_directory\",\"arguments\":{}}}\n"
        );
        let mut output = Vec::new();
        run_with_io(
            Options {
                mode: Mode::Record,
                workspace,
                transcript,
                per_call_state: None,
                metadata_trace: Some(trace.clone()),
                allow_commuted_observations: false,
            },
            &mut BufReader::new(input.as_bytes()),
            &mut output,
        )
        .unwrap();
        let entries = std::fs::read_to_string(&trace).unwrap();
        assert!(!entries.contains("not-retained"));
        assert!(!entries.contains("params"));
        let entries = entries
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 6);
        assert_eq!(entries[0]["method"], "initialize");
        assert_eq!(entries[0]["id_shape"], "number");
        assert_eq!(entries[1]["status"], "result");
        assert_eq!(entries[2]["method"], "notifications/initialized");
        assert_eq!(entries[2]["id_shape"], "absent");
        assert_eq!(entries[3]["status"], "notification");
        assert_eq!(entries[4]["method"], "tools/call");
        assert_eq!(entries[4]["tool"], "list_directory");
        assert_eq!(entries[5]["status"], "result");
        assert_eq!(entries[5]["response_shape"], "content_structured_success");
        assert!(!entries.iter().any(|entry| entry.get("result").is_some()));
        assert!(!String::from_utf8(output).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn private_mcp_accepts_and_answers_content_length_jsonrpc() {
        let body = br#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#;
        let request = [
            format!("Content-Length: {}\r\nX-Trace: fixture\r\n\r\n", body.len()).into_bytes(),
            body.to_vec(),
        ]
        .concat();
        let mut input = BufReader::new(&request[..]);
        let request = mcp_framing::read(&mut input, "local-tool MCP")
            .unwrap()
            .unwrap();
        assert_eq!(request.framing, mcp_framing::Framing::ContentLength);
        assert_eq!(request.value["method"], "initialize");

        let mut output = Vec::new();
        mcp_framing::write(
            &mut output,
            &json!({"jsonrpc":"2.0","id":7,"result":{}}),
            request.framing,
            "local-tool MCP",
        )
        .unwrap();
        let divider = output
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap();
        assert_eq!(&output[..divider], b"Content-Length: 36");
        assert_eq!(
            serde_json::from_slice::<Value>(&output[divider + 4..]).unwrap()["id"],
            7
        );
    }

    #[test]
    fn private_mcp_keeps_json_line_framing_for_existing_clients() {
        let mut input =
            BufReader::new(&b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n"[..]);
        let request = mcp_framing::read(&mut input, "local-tool MCP")
            .unwrap()
            .unwrap();
        assert_eq!(request.framing, mcp_framing::Framing::JsonLines);
        assert_eq!(request.value["id"], 1);
        let mut output = Vec::new();
        mcp_framing::write(
            &mut output,
            &json!({"id":1}),
            request.framing,
            "local-tool MCP",
        )
        .unwrap();
        assert_eq!(&output, b"{\"id\":1}\n");
    }

    #[test]
    fn guest_tool_service_uses_the_staged_runtime_launcher() {
        assert_eq!(
            runtime_program(Some(STAGED_RUNTIME_EXECUTABLE.into())).unwrap(),
            PathBuf::from(STAGED_RUNTIME_EXECUTABLE)
        );
    }

    #[test]
    fn private_file_reads_are_utf8_safe_bounded_and_explicitly_marked() {
        let source = format!("{}é", "x".repeat(MAX_MCP_READ_BYTES));
        let result = bounded_read_result(source);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["limit_bytes"], MAX_MCP_READ_BYTES);
        assert_eq!(result["text"].as_str().unwrap().len(), MAX_MCP_READ_BYTES);
        assert!(std::str::from_utf8(result["text"].as_str().unwrap().as_bytes()).is_ok());

        let complete = bounded_read_result("small source".into());
        assert_eq!(complete, json!({"text":"small source","truncated":false}));
    }

    #[test]
    fn record_tools_apply_only_confined_declared_effects() {
        let root = std::env::temp_dir().join(format!("asb-mcp-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (_, effects) = execute(
            &root,
            "write_file",
            &json!({"path":"answer.txt","content":"recorded"}),
        )
        .unwrap();
        assert_eq!(effects.len(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("answer.txt")).unwrap(),
            "recorded"
        );
        assert!(
            execute(
                &root,
                "write_file",
                &json!({"path":"../escape","content":"no"})
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn options_refuse_unknown_or_partial_arguments() {
        assert!(options(&[OsString::from("--mode"), OsString::from("record")]).is_err());
        assert!(options(&[OsString::from("--unknown"), OsString::from("value")]).is_err());
    }

    #[test]
    fn per_call_completion_requires_every_sealed_event() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-per-call-progress-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(root.join("tool-replay")).unwrap();
        let mut transcript = Transcript::default();
        transcript
            .record(Event {
                schema_version: SCHEMA_VERSION,
                sequence: 1,
                tool: "read_file".into(),
                request: json!({"path":"answer.txt"}),
                result: json!({"text":"answer","truncated":false}),
                failure: None,
                effects: Vec::new(),
            })
            .unwrap();
        let bytes = transcript.encode().unwrap();
        let state = per_call_state_path(&root, "pi-progress.json");
        publish_per_call_state(&state, 0, 1).unwrap();
        let receipt = serde_json::to_value(read_per_call_state(&state).unwrap()).unwrap();
        assert!(
            verify_persistent_per_call_completion_receipt(Some(&receipt), Some(&bytes)).is_err()
        );
        publish_per_call_state(&state, 1, 1).unwrap();
        let receipt = serde_json::to_value(read_per_call_state(&state).unwrap()).unwrap();
        verify_persistent_per_call_completion_receipt(Some(&receipt), Some(&bytes)).unwrap();
        // Replay consumes the sealed cassette outside the new run directory;
        // the bound event count allows collection to validate completion
        // without re-copying that private transcript.
        verify_persistent_per_call_completion_receipt(Some(&receipt), None).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn directory_listing_is_recorded_as_an_observation() {
        let root = std::env::temp_dir().join(format!("asb-mcp-directory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("answer.txt"), "answer").unwrap();
        let (result, effects) = execute(&root, "list_directory", &json!({})).unwrap();
        assert_eq!(effects, Vec::<Effect>::new());
        assert_eq!(result["entries"][0]["name"], "answer.txt");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn text_search_is_recorded_as_an_observation() {
        let root = std::env::temp_dir().join(format!("asb-mcp-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("answer.txt"), "answer\n").unwrap();
        let (result, effects) = execute(&root, "search_text", &json!({"query":"answer"})).unwrap();
        assert_eq!(effects, Vec::<Effect>::new());
        assert_eq!(result["matches"][0]["path"], "answer.txt");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn commands_are_bounded_and_workspace_effects_are_sealed() {
        let root = std::env::temp_dir().join(format!("asb-mcp-command-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let (result, effects) = execute(
            &root,
            "run_command",
            &json!({"argv":["sh","-c","printf repaired > changed.txt"]}),
        )
        .unwrap();
        assert!(result["success"].as_bool().unwrap());
        assert_eq!(
            effects,
            vec![Effect::WriteText {
                path: "changed.txt".into(),
                content: "repaired".into(),
            }]
        );
        assert_eq!(
            std::fs::read_to_string(root.join("changed.txt")).unwrap(),
            "repaired"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn replay_returns_a_recorded_command_result_without_starting_a_process() {
        let root =
            std::env::temp_dir().join(format!("asb-mcp-command-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let request = json!({"name":"run_command","arguments":{"argv":["pwd"]}});
        let record = Options {
            mode: Mode::Record,
            workspace: root.clone(),
            transcript: root.join("record.jsonl"),
            per_call_state: None,
            metadata_trace: None,
            allow_commuted_observations: false,
        };
        let mut transcript = Transcript::default();
        let recorded = call(&record, &mut transcript, request.clone()).unwrap();
        let mut replay = Transcript::decode(&transcript.encode().unwrap()).unwrap();
        let replay_options = Options {
            mode: Mode::Replay,
            workspace: root.join("removed-workspace"),
            transcript: root.join("replay.jsonl"),
            per_call_state: None,
            metadata_trace: None,
            allow_commuted_observations: false,
        };
        let replayed = call(&replay_options, &mut replay, request).unwrap();
        assert_eq!(recorded, replayed);
        assert_eq!(replayed["isError"], false);
        assert!(replayed["structuredContent"].is_object());
        assert!(replayed["content"][0]["text"].is_string());
        replay.finish().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replay_returns_a_recorded_tool_failure_without_touching_the_workspace() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-failure-replay-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let request = json!({
            "name":"write_file",
            "arguments":{"path":"../escape.txt","content":"no"}
        });
        let record = Options {
            mode: Mode::Record,
            workspace: root.clone(),
            transcript: root.join("record.jsonl"),
            per_call_state: None,
            metadata_trace: None,
            allow_commuted_observations: false,
        };
        let mut transcript = Transcript::default();
        let recorded = call(&record, &mut transcript, request.clone()).unwrap_err();
        assert!(!recorded.to_string().is_empty());
        let bytes = transcript.encode().unwrap();
        assert!(
            String::from_utf8(bytes.clone())
                .unwrap()
                .contains("failure")
        );

        let replay_root = root.join("never-opened");
        let replay = Options {
            mode: Mode::Replay,
            workspace: replay_root,
            transcript: root.join("replay.jsonl"),
            per_call_state: None,
            metadata_trace: None,
            allow_commuted_observations: false,
        };
        let mut replayed = Transcript::decode(&bytes).unwrap();
        let replay_error = call(&replay, &mut replayed, request).unwrap_err();
        assert_eq!(replay_error.to_string(), recorded.to_string());
        replayed.finish().unwrap();
        assert!(!root.join("escape.txt").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn recorded_tool_failure_returns_an_mcp_error_and_keeps_the_service_usable() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-pi-failure-publication-{}",
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        let run_dir = root.join("run");
        let transcript = run_dir.join("tool-replay/local-tools.jsonl");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, Transcript::default().encode().unwrap()).unwrap();
        // A sparse file reaches the command snapshot's byte ceiling without
        // retaining a large test fixture.  The first snapshot must fail before
        // the command is executed, exactly like the Pi qualification symptom.
        std::fs::File::create(workspace.join("oversized-source"))
            .unwrap()
            .set_len(MAX_COMMAND_SNAPSHOT_BYTES + 1)
            .unwrap();
        std::fs::write(workspace.join("still-readable.txt"), "available\n").unwrap();
        let state = per_call_state_path(&run_dir, "pi-progress.json");
        publish_per_call_state(&state, 0, 0).unwrap();
        let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"run_command\",\"arguments\":{\"argv\":[\"pwd\"]}}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"still-readable.txt\"}}}\n";
        let record = Options {
            mode: Mode::Record,
            workspace: workspace.clone(),
            transcript: transcript.clone(),
            per_call_state: Some(state.clone()),
            metadata_trace: None,
            allow_commuted_observations: false,
        };
        let mut output = Vec::new();
        run_with_io(record, &mut BufReader::new(&request[..]), &mut output).unwrap();
        let responses = String::from_utf8(output.clone()).unwrap();
        assert!(responses.contains("\"id\":1"));
        assert!(responses.contains("\"id\":2"));
        assert!(responses.contains("local-tool command workspace exceeds snapshot byte bound"));
        assert!(responses.contains("available"), "{responses}");
        let bytes = std::fs::read(&transcript).unwrap();
        let recorded = Transcript::decode(&bytes).unwrap();
        assert_eq!(recorded.len(), 2);
        assert_eq!(
            serde_json::from_slice::<Value>(bytes.split(|byte| *byte == b'\n').next().unwrap())
                .unwrap()["failure"],
            "local-tool command workspace exceeds snapshot byte bound"
        );
        let recorded_state = read_per_call_state(&state).unwrap();
        assert_eq!((recorded_state.cursor, recorded_state.events), (2, 2));

        // Replay initializes a fresh run-private cursor while retaining the
        // sealed event count from the recording.
        publish_per_call_state(&state, 0, 2).unwrap();
        let replay = Options {
            mode: Mode::Replay,
            workspace: root.join("workspace-is-not-opened-during-replay"),
            transcript,
            per_call_state: Some(state.clone()),
            metadata_trace: None,
            allow_commuted_observations: false,
        };
        let mut replay_output = Vec::new();
        run_with_io(
            replay,
            &mut BufReader::new(&request[..]),
            &mut replay_output,
        )
        .unwrap();
        assert_eq!(replay_output, output);
        let receipt = serde_json::to_value(read_per_call_state(&state).unwrap()).unwrap();
        verify_persistent_per_call_completion_receipt(Some(&receipt), None).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn oversized_command_output_is_sealed_before_a_follow_up_per_call_service() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-command-output-bound-{}",
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        let run_dir = root.join("run");
        let transcript = run_dir.join("tool-replay/local-tools.jsonl");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(workspace.join("still-readable.txt"), "available\n").unwrap();
        std::fs::write(&transcript, Transcript::default().encode().unwrap()).unwrap();
        let state = per_call_state_path(&run_dir, "pi-progress.json");
        publish_per_call_state(&state, 0, 0).unwrap();
        let request = format!(
            "{}\n{}\n",
            json!({
                "jsonrpc":"2.0",
                "id":1,
                "method":"tools/call",
                "params":{"name":"run_command","arguments":{"argv":["sh","-c",format!("head -c {} /dev/zero", MAX_COMMAND_OUTPUT + 1)]}},
            }),
            json!({
                "jsonrpc":"2.0",
                "id":2,
                "method":"tools/call",
                "params":{"name":"read_file","arguments":{"path":"still-readable.txt"}},
            }),
        );
        let record = Options {
            mode: Mode::Record,
            workspace,
            transcript: transcript.clone(),
            per_call_state: Some(state.clone()),
            metadata_trace: None,
            allow_commuted_observations: false,
        };
        let mut output = Vec::new();
        run_with_io(record, &mut BufReader::new(request.as_bytes()), &mut output).unwrap();
        let responses = String::from_utf8(output).unwrap();
        assert!(responses.contains("local-tool command output exceeds bound"));
        assert!(responses.contains("available"));
        assert_eq!(
            Transcript::decode(&std::fs::read(&transcript).unwrap())
                .unwrap()
                .len(),
            2
        );
        assert_eq!(read_per_call_state(&state).unwrap().events, 2);
        assert!(!state.with_extension("lock").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn persistent_mcp_lifecycles_append_and_replay_every_event() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-persistent-lifecycles-{}",
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        let run_dir = root.join("run");
        let transcript = run_dir.join("tool-replay/local-tools.jsonl");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(workspace.join("first.txt"), "first\n").unwrap();
        std::fs::write(workspace.join("second.txt"), "second\n").unwrap();
        std::fs::write(&transcript, Transcript::default().encode().unwrap()).unwrap();
        let state = per_call_state_path(&run_dir, "persistent-progress.json");
        publish_per_call_state(&state, 0, 0).unwrap();
        let first = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"first.txt\"}}}\n";
        let second = b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"second.txt\"}}}\n";

        let record = |request: &[u8]| {
            let mut output = Vec::new();
            run_with_io(
                Options {
                    mode: Mode::Record,
                    workspace: workspace.clone(),
                    transcript: transcript.clone(),
                    per_call_state: Some(state.clone()),
                    metadata_trace: None,
                    allow_commuted_observations: false,
                },
                &mut BufReader::new(request),
                &mut output,
            )
            .unwrap();
            output
        };
        let first_recorded = record(first);
        let second_recorded = record(second);
        let sealed = std::fs::read(&transcript).unwrap();
        assert_eq!(Transcript::decode(&sealed).unwrap().len(), 2);
        assert_eq!(read_per_call_state(&state).unwrap().events, 2);

        // A replay starts from the sealed event count but consumes it through
        // the same two independently restarted MCP service lifecycles.
        publish_per_call_state(&state, 0, 2).unwrap();
        let replay = |request: &[u8]| {
            let mut output = Vec::new();
            run_with_io(
                Options {
                    mode: Mode::Replay,
                    workspace: root.join("replay-workspace-is-not-opened"),
                    transcript: transcript.clone(),
                    per_call_state: Some(state.clone()),
                    metadata_trace: None,
                    allow_commuted_observations: false,
                },
                &mut BufReader::new(request),
                &mut output,
            )
            .unwrap();
            output
        };
        assert_eq!(replay(first), first_recorded);
        assert_eq!(replay(second), second_recorded);
        let receipt = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
        verify_persistent_per_call_completion_receipt(Some(&receipt), Some(&sealed)).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn persistent_mcp_state_is_durable_before_client_receives_response() {
        struct InspectOutput {
            transcript: PathBuf,
            state: PathBuf,
            expected_cursor: usize,
            observed: bool,
        }
        impl std::io::Write for InspectOutput {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let transcript =
                    Transcript::decode(&std::fs::read(&self.transcript).unwrap()).unwrap();
                let state = read_per_call_state(&self.state).unwrap();
                assert_eq!(transcript.len(), 1, "transcript must precede response");
                assert_eq!(state.events, 1, "event count must precede response");
                assert_eq!(
                    state.cursor, self.expected_cursor,
                    "cursor must precede response"
                );
                self.observed = true;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-publication-order-{}",
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        let transcript = root.join("local-tools.jsonl");
        let state = root.join("progress.json");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("source.txt"), "fixture").unwrap();
        std::fs::write(&transcript, Transcript::default().encode().unwrap()).unwrap();
        publish_per_call_state(&state, 0, 0).unwrap();
        let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"source.txt\"}}}\n";
        for (mode, expected_cursor) in [(Mode::Record, 1), (Mode::Replay, 1)] {
            if mode == Mode::Replay {
                publish_per_call_state(&state, 0, 1).unwrap();
            }
            let mut output = InspectOutput {
                transcript: transcript.clone(),
                state: state.clone(),
                expected_cursor,
                observed: false,
            };
            run_with_io(
                Options {
                    mode,
                    workspace: workspace.clone(),
                    transcript: transcript.clone(),
                    per_call_state: Some(state.clone()),
                    metadata_trace: None,
                    allow_commuted_observations: false,
                },
                &mut BufReader::new(request.as_slice()),
                &mut output,
            )
            .unwrap();
            assert!(output.observed);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persistent_mcp_lifecycle_refuses_an_overlapping_service() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-persistent-lock-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let state = root.join("persistent-progress.json");
        let lease = PersistentStateLease::acquire(&state).unwrap();
        let error = PersistentStateLease::acquire(&state)
            .unwrap_err()
            .to_string();
        assert!(error.contains("already in use"), "{error}");
        drop(lease);
        PersistentStateLease::acquire(&state).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replay_reconstructs_bounded_command_source_effects() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-command-effects-{}",
            crate::tools::runtime::unique_id()
        ));
        let replay_root = root.join("replay");
        std::fs::create_dir_all(&root).unwrap();
        let source = "alpha\n";
        std::fs::write(root.join("source.txt"), source).unwrap();
        // `sed -i` is an allowed, deterministic local command.  Unlike a
        // formatter, this does not depend on an optional toolchain component
        // or version-specific formatting choices.
        let request = json!({
            "name":"run_command",
            "arguments":{"argv":["sed", "-i", "s/alpha/bravo/", "source.txt"]}
        });
        let record = Options {
            mode: Mode::Record,
            workspace: root.clone(),
            transcript: root.join("record.jsonl"),
            per_call_state: None,
            metadata_trace: None,
            allow_commuted_observations: false,
        };
        let mut transcript = Transcript::default();
        let recorded = call(&record, &mut transcript, request.clone()).unwrap();
        let rewritten = std::fs::read_to_string(root.join("source.txt")).unwrap();
        assert_eq!(rewritten, "bravo\n");
        assert!(
            String::from_utf8(transcript.encode().unwrap())
                .unwrap()
                .contains("source.txt")
        );

        std::fs::create_dir_all(&replay_root).unwrap();
        std::fs::write(replay_root.join("source.txt"), source).unwrap();
        let mut replay = Transcript::decode(&transcript.encode().unwrap()).unwrap();
        let replayed = call(
            &Options {
                mode: Mode::Replay,
                workspace: replay_root.clone(),
                transcript: replay_root.join("replay.jsonl"),
                per_call_state: None,
                metadata_trace: None,
                allow_commuted_observations: false,
            },
            &mut replay,
            request,
        )
        .unwrap();
        assert_eq!(recorded, replayed);
        assert_eq!(
            std::fs::read_to_string(replay_root.join("source.txt")).unwrap(),
            rewritten
        );
        replay.finish().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn command_ledger_detects_same_length_source_rewrite() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-command-identity-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.txt");
        std::fs::write(&source, "alpha").unwrap();
        let before = command_snapshot(&root).unwrap();
        std::fs::write(&source, "bravo").unwrap();
        let after = command_snapshot(&root).unwrap();
        assert_eq!(
            before.get("source.txt").and_then(|entry| match entry {
                CommandEntry::File { bytes, .. } => Some(*bytes),
                CommandEntry::Directory => None,
            }),
            after.get("source.txt").and_then(|entry| match entry {
                CommandEntry::File { bytes, .. } => Some(*bytes),
                CommandEntry::Directory => None,
            })
        );
        assert_ne!(before, after);
        assert_eq!(
            command_effects(&before, &after, &root).unwrap(),
            vec![Effect::WriteText {
                path: "source.txt".into(),
                content: "bravo".into(),
            }]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replay_ignores_non_semantic_tool_argument_metadata() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-metadata-replay-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let record = Options {
            mode: Mode::Record,
            workspace: root.clone(),
            transcript: root.join("record.jsonl"),
            per_call_state: None,
            metadata_trace: None,
            allow_commuted_observations: false,
        };
        let mut transcript = Transcript::default();
        let recorded = call(
            &record,
            &mut transcript,
            json!({"name":"run_command","arguments":{"argv":["pwd"],"_meta":{"client":"fixture"}}}),
        )
        .unwrap();
        let mut replay = Transcript::decode(&transcript.encode().unwrap()).unwrap();
        let replayed = call(
            &Options {
                mode: Mode::Replay,
                workspace: root.join("removed-workspace"),
                transcript: root.join("replay.jsonl"),
                per_call_state: None,
                metadata_trace: None,
                allow_commuted_observations: false,
            },
            &mut replay,
            json!({"name":"run_command","arguments":{"argv":["pwd"],"_meta":{"client":"changed"}}}),
        )
        .unwrap();
        assert_eq!(recorded, replayed);
        replay.finish().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn record_launch_publishes_a_valid_empty_local_tool_transcript() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-empty-transcript-{}",
            crate::tools::runtime::unique_id()
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let cassette = root.join("recording.jsonl");
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            base_url: "http://127.0.0.1:1/v1".into(),
            credential: None,
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let mut command = std::process::Command::new("unused");
        let launch = launch(
            &PreparationContext {
                command: &mut command,
                model: "fixture",
                connection: &connection,
                workspace: &workspace,
                runtime_dir: &root,
                run_dir: &root,
                mcp_servers: &[],
                mcp_tape: None,
                tape: None,
            },
            TapeContext {
                mode: crate::tape::Mode::Record,
                cassette: &cassette,
            },
        )
        .unwrap();
        let transcript = root.join("tool-replay/local-tools.jsonl");
        assert!(transcript.is_file());
        assert!(std::fs::read(&transcript).unwrap().is_empty());
        Transcript::decode(&std::fs::read(&transcript).unwrap()).unwrap();
        assert!(launch.args.iter().any(|argument| argument == "record"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn record_publication_replaces_the_initialized_local_tool_transcript() {
        let root = std::env::temp_dir().join(format!(
            "asb-mcp-record-publication-{}",
            crate::tools::runtime::unique_id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let transcript = root.join("local-tools.jsonl");
        std::fs::write(&transcript, Transcript::default().encode().unwrap()).unwrap();
        let mut expected_transcript = Transcript::default();
        expected_transcript
            .record(Event {
                schema_version: SCHEMA_VERSION,
                sequence: 1,
                tool: "read_file".into(),
                request: json!({"name":"read_file","arguments":{"path":"answer.txt"}}),
                result: json!({"content":"recorded"}),
                failure: None,
                effects: Vec::new(),
            })
            .unwrap();
        let expected = expected_transcript.encode().unwrap();
        publish_record_transcript(&transcript, &expected).unwrap();
        assert_eq!(std::fs::read(&transcript).unwrap(), expected);
        let link = root.join("link.jsonl");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&transcript, &link).unwrap();
        #[cfg(unix)]
        assert!(publish_record_transcript(&link, b"unexpected").is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn replay_launch_refuses_an_unsealed_local_tool_track() {
        let root = std::env::temp_dir().join(format!("asb-mcp-launch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let replay = root.join("replay.jsonl");
        std::fs::write(&replay, "{}\n").unwrap();
        let connection = crate::endpoint::Connection {
            interface: crate::endpoint::Interface::OpenAiChat,
            base_url: "http://127.0.0.1:1/v1".into(),
            credential: None,
            authentication: crate::endpoint::Authentication::Bearer,
        };
        let mut command = std::process::Command::new("unused");
        let error = launch(
            &PreparationContext {
                command: &mut command,
                model: "fixture",
                connection: &connection,
                workspace: &workspace,
                runtime_dir: &root,
                run_dir: &root,
                mcp_servers: &[],
                mcp_tape: None,
                tape: None,
            },
            TapeContext {
                mode: crate::tape::Mode::Replay,
                cassette: &replay,
            },
        )
        .err()
        .expect("unsealed cassette must not enable host tools")
        .to_string();
        assert!(error.contains("sealed local-tool transcript"), "{error}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn protocol_identity_rejects_changed_provider_visible_tool_definitions() {
        verify_protocol_bytes(&protocol_bytes().unwrap()).unwrap();
        let error = verify_protocol_bytes(
            br#"{"schema_version":1,"protocol":"asb-local-tools-v1-read-only"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("differs from this ASB build"), "{error}");
    }
}
