//! Transparent, bounded stdio MCP relay recording.
//!
//! The relay forwards validated original JSON bytes in both directions. It does
//! not know MCP tool names or alter agent configuration; a later adapter step
//! supplies its ingress in place of a real MCP server. Recordings remain
//! private, content-addressed, and are published only after both streams and
//! the upstream process close cleanly.

use super::{mcp_framing, mcp_transcript};
use crate::Error;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Instant;

const SERVICE: &str = "MCP relay";
/// One relay records only enough opaque MCP traffic for a bounded benchmark
/// tool interaction.  Larger captures amplify disk and kernel write pressure
/// without making replay more representative.
const MAX_PAYLOAD_BYTES: u64 = 4 * 1024 * 1024;
const CHANNEL_CAPACITY: usize = 4;
const BUNDLE_SCHEMA_VERSION: u32 = 1;
// JSON string escaping can expand an otherwise valid MCP payload.  Keep the
// durable representation small enough for the existing auxiliary-artifact
// path rather than allowing a single server to dominate a run's disk budget.
const MAX_BUNDLE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const IGNORED_MCP_FILENAME: &str = "ignored-mcp.jsonl";
pub(crate) const IGNORED_MCP_SPEC: super::ArtifactSpec = super::ArtifactSpec {
    namespace: "mcp-replay-evidence",
    filename: IGNORED_MCP_FILENAME,
    format: super::ArtifactFormat::JsonLines,
};

pub(crate) fn initialize_ignored_journal(run_dir: &Path) -> Result<(), Error> {
    let path = run_dir
        .join("mcp-replay-evidence")
        .join(IGNORED_MCP_FILENAME);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| Error::Run("cannot initialize MCP replay divergence journal".into()))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum McpIgnoredKind {
    Started,
    Finished,
    Fatal,
    Request {
        sequence: u64,
        expected_sha256: String,
        actual_sha256: String,
    },
    Unconsumed {
        next_sequence: u64,
        client_messages: usize,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpIgnoredEvent {
    pub server: String,
    pub session: String,
    pub event: McpIgnoredKind,
}

struct McpJournal {
    path: PathBuf,
    server: String,
    session: String,
}
impl McpJournal {
    fn write(&self, event: McpIgnoredKind) -> Result<(), Error> {
        let mut options = OpenOptions::new();
        options.append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let mut file = options
            .open(&self.path)
            .map_err(|_| Error::Run("cannot open MCP replay divergence journal".into()))?;
        file.lock()
            .map_err(|_| Error::Run("cannot lock MCP replay divergence journal".into()))?;
        let metadata = file.metadata().map_err(crate::io_error)?;
        let event = McpIgnoredEvent {
            server: self.server.clone(),
            session: self.session.clone(),
            event,
        };
        let mut bytes = serde_json::to_vec(&event)
            .map_err(|_| Error::Run("cannot encode MCP replay divergence".into()))?;
        bytes.push(b'\n');
        if !metadata.is_file() || metadata.len() + bytes.len() as u64 > 256 * 1024 {
            return Err(Error::Run(
                "MCP replay divergence journal is invalid or full".into(),
            ));
        }
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| Error::Run("cannot persist MCP replay divergence".into()))
    }
}

/// Validate per-session evidence after agent shutdown. A start without a
/// completion is fatal even when mismatches are ignorable. A server that
/// never reported a start cannot prove why its recorded requests were absent.
pub(crate) fn settle_ignored_journal(
    bytes: &[u8],
    cassette: &Path,
    declarations: &[crate::benchmark::McpServerConfig],
) -> Result<Vec<McpIgnoredEvent>, Error> {
    if bytes.len() > 256 * 1024 {
        return Err(Error::Run(
            "MCP replay divergence journal exceeds its bound".into(),
        ));
    }
    let mut sessions = BTreeMap::<String, (String, bool)>::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut ignored = Vec::new();
    let mut recordings = BTreeMap::new();
    for declaration in declarations {
        let path = replay_bundle_path(cassette, &declaration.id)?;
        recordings.insert(
            declaration.id.as_str(),
            decode_bundle(&read_bundle(&path)?)?,
        );
    }
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let event: McpIgnoredEvent = serde_json::from_slice(line)
            .map_err(|_| Error::Run("MCP replay divergence journal is malformed".into()))?;
        if !declarations.iter().any(|decl| decl.id == event.server)
            || event.session.is_empty()
            || event.session.len() > 128
        {
            return Err(Error::Run(
                "MCP replay divergence journal has an invalid identity".into(),
            ));
        }
        match &event.event {
            McpIgnoredKind::Started => {
                if sessions
                    .insert(event.session.clone(), (event.server.clone(), false))
                    .is_some()
                {
                    return Err(Error::Run("duplicate MCP replay session".into()));
                }
                seen.insert(event.server.clone());
            }
            McpIgnoredKind::Finished | McpIgnoredKind::Fatal => {
                let (server, finished) = sessions
                    .get_mut(&event.session)
                    .ok_or_else(|| Error::Run("MCP replay session was not started".into()))?;
                if server != &event.server || *finished {
                    return Err(Error::Run("MCP replay session is inconsistent".into()));
                }
                *finished = true;
                if matches!(event.event, McpIgnoredKind::Fatal) {
                    return Err(Error::Run(
                        "MCP replay could not safely supply its recorded response".into(),
                    ));
                }
            }
            McpIgnoredKind::Request { .. } | McpIgnoredKind::Unconsumed { .. } => {
                if !sessions
                    .get(&event.session)
                    .is_some_and(|(server, finished)| server == &event.server && !finished)
                {
                    return Err(Error::Run(
                        "MCP replay divergence has no active session".into(),
                    ));
                }
                let recording = &recordings[event.server.as_str()].transcript;
                match &event.event {
                    McpIgnoredKind::Request {
                        sequence,
                        expected_sha256,
                        actual_sha256,
                    } => {
                        let actual_valid = actual_sha256.len() == 64
                            && actual_sha256.bytes().all(|byte| byte.is_ascii_hexdigit());
                        if !actual_valid
                            || !recording.events().iter().any(|recorded| {
                                recorded.sequence == *sequence
                                    && recorded.direction
                                        == mcp_transcript::Direction::ClientToServer
                                    && recorded.message.sha256 == *expected_sha256
                            })
                        {
                            return Err(Error::Run(
                                "MCP replay divergence does not bind its recording".into(),
                            ));
                        }
                    }
                    McpIgnoredKind::Unconsumed {
                        next_sequence,
                        client_messages,
                    } => {
                        let tail = recording
                            .events()
                            .iter()
                            .skip_while(|recorded| recorded.sequence < *next_sequence)
                            .collect::<Vec<_>>();
                        if *client_messages == 0
                            || !tail.first().is_some_and(|recorded| {
                                recorded.sequence == *next_sequence
                                    && recorded.direction
                                        == mcp_transcript::Direction::ClientToServer
                            })
                            || tail
                                .iter()
                                .filter(|recorded| {
                                    recorded.direction == mcp_transcript::Direction::ClientToServer
                                })
                                .count()
                                != *client_messages
                        {
                            return Err(Error::Run(
                                "MCP replay unconsumed count differs from its recording".into(),
                            ));
                        }
                    }
                    _ => unreachable!(),
                }
                ignored.push(event);
            }
        }
    }
    if sessions.values().any(|(_, finished)| !finished) {
        return Err(Error::Run("MCP replay session did not settle".into()));
    }
    for declaration in declarations {
        if !seen.contains(&declaration.id) {
            return Err(Error::Run(
                "MCP replay server did not report a settled session; replay cannot prove its outcome".into(),
            ));
        }
    }
    Ok(ignored)
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum BundleRecord {
    Header {
        schema_version: u32,
    },
    Event {
        schema_version: u32,
        event: mcp_transcript::Event,
    },
    Payload {
        schema_version: u32,
        blob: mcp_transcript::Blob,
        /// Valid UTF-8 JSON preserved byte-for-byte inside a JSON string.
        payload: String,
    },
}

struct Bundle {
    transcript: mcp_transcript::Transcript,
    payloads: BTreeMap<String, Vec<u8>>,
}

/// Adapter-neutral launch description for a declared MCP server.  A client
/// adapter owns its configuration syntax; this module owns the only decision
/// about whether that configuration addresses the declared server or ASB's
/// bounded record/replay relay.
pub(crate) struct ServerLaunch {
    pub(crate) id: String,
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
}

/// Resolve every declared MCP server for one attempt without interpreting its
/// protocol or tool names.  Keeping this selection here ensures every adapter
/// gets the same fail-closed replay behavior and never starts an upstream
/// server for an MCP replay.
pub(crate) fn server_launches(
    run_dir: &Path,
    declarations: &[crate::benchmark::McpServerConfig],
    tape: Option<super::McpTapeContext<'_>>,
) -> Result<Vec<ServerLaunch>, Error> {
    declarations
        .iter()
        .map(|declaration| {
            let launch = match tape {
                Some(super::McpTapeContext {
                    mode: crate::tape::Mode::Record,
                    ..
                }) => record_launch(run_dir, declaration)?,
                Some(super::McpTapeContext {
                    mode: crate::tape::Mode::Replay,
                    cassette: Some(cassette),
                    ignore_mcp_mismatch,
                }) => replay_launch(run_dir, declaration, cassette, ignore_mcp_mismatch)?,
                Some(super::McpTapeContext {
                    mode: crate::tape::Mode::Replay,
                    cassette: None,
                    ..
                }) => {
                    return Err(Error::Run(
                        "MCP replay needs its selected sealed relay bundle".into(),
                    ));
                }
                None => super::tool_replay_mcp::Launch {
                    program: declaration.program.clone().into(),
                    args: declaration.args.iter().cloned().map(Into::into).collect(),
                },
            };
            Ok(ServerLaunch {
                id: declaration.id.clone(),
                program: launch.program.into(),
                args: launch.args,
            })
        })
        .collect()
}

/// Seal the completed relay recordings for an adapter that opted into MCP
/// recording.  Relay publication is transport-wide rather than client-specific:
/// a missing or malformed recording remains a collection error instead of
/// producing a cassette which could later replay against a live upstream.
pub(crate) fn collect_artifacts(context: &super::CollectionContext<'_>) -> super::AgentArtifacts {
    let mut artifacts = super::AgentArtifacts::default();
    if context.mcp_tape.map(|tape| tape.mode) != Some(crate::tape::Mode::Record) {
        return artifacts;
    }
    for declaration in context.mcp_servers {
        let recording = context.run_dir.join("mcp-relay").join(&declaration.id);
        let bundle = if recording.exists() {
            bundle_recording(&recording)
        } else {
            empty_bundle()
        };
        match bundle {
            Ok(bytes) => artifacts.sealed_artifacts.push(super::SealedArtifact {
                kind: format!("mcp-relay-{}", declaration.id),
                bytes,
            }),
            Err(error) => artifacts.errors.push(format!(
                "cannot seal MCP relay recording for `{}`: {error}",
                declaration.id
            )),
        }
    }
    artifacts
}

/// Adapter-facing command which substitutes one ordinary MCP server during a
/// recording. The target directory remains absent until the relay owns the
/// upstream process, so an interrupted launch cannot be mistaken for a
/// completed empty recording.
pub(crate) fn record_launch(
    run_dir: &Path,
    declaration: &crate::benchmark::McpServerConfig,
) -> Result<super::tool_replay_mcp::Launch, Error> {
    let recording = run_dir.join("mcp-relay").join(&declaration.id);
    let parent = recording
        .parent()
        .expect("MCP relay recording has a parent");
    fs::create_dir_all(parent).map_err(|error| {
        Error::Run(format!(
            "cannot create MCP relay recording directory: {error}"
        ))
    })?;
    if recording.exists() {
        return Err(Error::Run(
            "MCP relay recording destination already exists".into(),
        ));
    }
    let program =
        super::tool_replay_mcp::runtime_program(std::env::var_os("ASB_RUNTIME_EXECUTABLE"))?;
    let mut args = vec![
        "__asb-mcp-relay".into(),
        "--record-dir".into(),
        recording.into_os_string(),
        "--".into(),
        declaration.program.clone().into(),
    ];
    args.extend(declaration.args.iter().cloned().map(Into::into));
    Ok(super::tool_replay_mcp::Launch { program, args })
}

/// Adapter-facing command which serves a sealed relay bundle without starting
/// the declared upstream program.  The selected bundle is staged beside the
/// chosen provider replay payload, then materialized beneath this attempt's
/// private run directory.
pub(crate) fn replay_launch(
    run_dir: &Path,
    declaration: &crate::benchmark::McpServerConfig,
    cassette: &Path,
    ignore_mcp_mismatch: bool,
) -> Result<super::tool_replay_mcp::Launch, Error> {
    let bundle = replay_bundle_path(cassette, &declaration.id)?;
    let bytes = read_bundle(&bundle)?;
    let recording = run_dir.join("mcp-replay").join(&declaration.id);
    let parent = recording
        .parent()
        .expect("MCP replay recording has a parent");
    fs::create_dir_all(parent).map_err(|error| {
        Error::Run(format!(
            "cannot create MCP replay recording directory: {error}"
        ))
    })?;
    materialize_bundle(&bytes, &recording).map_err(|error| {
        Error::Run(format!(
            "cannot stage sealed MCP relay recording for `{}`: {error}",
            declaration.id
        ))
    })?;
    let program =
        super::tool_replay_mcp::runtime_program(std::env::var_os("ASB_RUNTIME_EXECUTABLE"))?;
    let mut launch = super::tool_replay_mcp::Launch {
        program,
        args: vec![
            "__asb-mcp-relay".into(),
            "--replay-dir".into(),
            recording.into_os_string(),
        ],
    };
    if ignore_mcp_mismatch {
        launch.args.extend([
            "--ignored-events".into(),
            run_dir
                .join("mcp-replay-evidence")
                .join("ignored-mcp.jsonl")
                .into_os_string(),
            "--server-id".into(),
            declaration.id.clone().into(),
        ]);
    }
    Ok(launch)
}

pub(crate) fn replay_bundle_path(cassette: &Path, id: &str) -> Result<PathBuf, Error> {
    let name = cassette
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::Run("MCP replay cassette has no valid filename".into()))?;
    let suffix = name
        .strip_prefix("replay")
        .and_then(|suffix| suffix.strip_suffix(".jsonl"))
        .ok_or_else(|| Error::Run("MCP replay cassette has an invalid filename".into()))?;
    let parent = cassette
        .parent()
        .ok_or_else(|| Error::Run("MCP replay cassette has no private directory".into()))?;
    Ok(parent.join(format!("mcp-relay-{id}{suffix}.jsonl")))
}

pub(crate) fn entry(args: &[OsString]) -> std::process::ExitCode {
    match run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // stdout is reserved for the downstream MCP client.
            eprintln!("ASB MCP relay: {error}");
            std::process::ExitCode::from(2)
        }
    }
}

struct RecordOptions {
    record_dir: PathBuf,
    program: OsString,
    args: Vec<OsString>,
}

enum Options {
    Record(RecordOptions),
    Replay {
        directory: PathBuf,
        ignored_events: Option<(PathBuf, String)>,
    },
    Bundle {
        directory: PathBuf,
        output: PathBuf,
    },
    Materialize {
        bundle: PathBuf,
        destination: PathBuf,
    },
}

fn run(args: &[OsString]) -> Result<(), Error> {
    match options(args)? {
        Options::Record(options) => run_record(options),
        Options::Replay {
            directory,
            ignored_events,
        } => run_replay(&directory, ignored_events),
        Options::Bundle { directory, output } => write_bundle(&directory, &output),
        Options::Materialize {
            bundle,
            destination,
        } => materialize_bundle(&read_bundle(&bundle)?, &destination),
    }
}

fn run_record(options: RecordOptions) -> Result<(), Error> {
    let mut upstream = Command::new(&options.program)
        .args(&options.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|_| Error::Run("cannot start MCP relay upstream server".into()))?;
    let upstream_input = upstream
        .stdin
        .take()
        .ok_or_else(|| Error::Run("MCP relay upstream input is unavailable".into()))?;
    let upstream_output = upstream
        .stdout
        .take()
        .ok_or_else(|| Error::Run("MCP relay upstream output is unavailable".into()))?;
    let recorder = Recorder::new(&options.record_dir)?;
    let result = relay_record(
        std::io::stdin(),
        std::io::stdout().lock(),
        upstream_input,
        upstream_output,
        recorder,
    );
    if result.is_err() {
        let _ = upstream.kill();
    }
    let status = upstream
        .wait()
        .map_err(|_| Error::Run("cannot wait for MCP relay upstream server".into()))?;
    result?;
    if !status.success() {
        return Err(Error::Run("MCP relay upstream server failed".into()));
    }
    Ok(())
}

fn run_replay(directory: &Path, ignored_events: Option<(PathBuf, String)>) -> Result<(), Error> {
    let mut journal = ignored_events.map(|(path, server)| McpJournal {
        path,
        server,
        session: crate::tools::runtime::unique_id(),
    });
    if let Some(journal) = journal.as_ref() {
        journal.write(McpIgnoredKind::Started)?;
    }
    let result = (|| {
        validate_recording(directory)?;
        let replay = Replayer::load(directory)?;
        replay_messages_with(
            std::io::stdin(),
            std::io::stdout().lock(),
            &replay,
            journal.as_mut(),
        )
    })();
    if let Some(journal) = journal.as_ref() {
        journal.write(if result.is_ok() {
            McpIgnoredKind::Finished
        } else {
            McpIgnoredKind::Fatal
        })?;
    }
    result
}

fn options(args: &[OsString]) -> Result<Options, Error> {
    if let [flag, directory, output_flag, output] = args {
        if flag == "--bundle-dir" && output_flag == "--output" {
            return Ok(Options::Bundle {
                directory: PathBuf::from(directory),
                output: PathBuf::from(output),
            });
        }
        if flag == "--materialize-bundle" && output_flag == "--output" {
            return Ok(Options::Materialize {
                bundle: PathBuf::from(directory),
                destination: PathBuf::from(output),
            });
        }
    }
    if args.len() == 2 && args[0] == "--replay-dir" {
        return Ok(Options::Replay {
            directory: PathBuf::from(&args[1]),
            ignored_events: None,
        });
    }
    if let [flag, directory, journal_flag, journal, server_flag, server] = args
        && flag == "--replay-dir"
        && journal_flag == "--ignored-events"
        && server_flag == "--server-id"
    {
        let server = server
            .to_str()
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or_else(|| Error::Input("invalid MCP server ID".into()))?;
        return Ok(Options::Replay {
            directory: PathBuf::from(directory),
            ignored_events: Some((PathBuf::from(journal), server.to_owned())),
        });
    }
    let mut record_dir = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].to_str() {
            Some("--record-dir") => {
                index += 1;
                record_dir = args.get(index).map(PathBuf::from);
                if record_dir.is_none() {
                    return Err(Error::Input(
                        "MCP relay record directory is required".into(),
                    ));
                }
            }
            Some("--") => {
                let program = args
                    .get(index + 1)
                    .cloned()
                    .ok_or_else(|| Error::Input("MCP relay upstream program is required".into()))?;
                return Ok(Options::Record(RecordOptions {
                    record_dir: record_dir.ok_or_else(|| {
                        Error::Input("MCP relay record directory is required".into())
                    })?,
                    program,
                    args: args[index + 2..].to_vec(),
                }));
            }
            _ => return Err(Error::Input("invalid MCP relay arguments".into())),
        }
        index += 1;
    }
    Err(Error::Input(
        "MCP relay upstream program is required".into(),
    ))
}

fn write_bundle(directory: &Path, output: &Path) -> Result<(), Error> {
    write_bundle_with(directory, output, || {})
}

fn write_bundle_with(
    directory: &Path,
    output: &Path,
    before_publish: impl FnOnce(),
) -> Result<(), Error> {
    write_bundle_with_hooks(directory, output, before_publish, || {})
}

fn write_bundle_with_hooks(
    directory: &Path,
    output: &Path,
    before_publish: impl FnOnce(),
    after_publish: impl FnOnce(),
) -> Result<(), Error> {
    if output.exists() {
        return Err(Error::Run(
            "MCP relay bundle destination already exists".into(),
        ));
    }
    let parent = output
        .parent()
        .filter(|parent| parent.is_dir())
        .ok_or_else(|| Error::Run("MCP relay bundle parent is unavailable".into()))?;
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| Error::Run("MCP relay bundle destination is invalid".into()))?;
    let bytes = bundle_recording(directory)?;
    let (stage, mut owner) = reserve_file(parent, name, "mcp-bundle")?;
    (|| {
        owner.write_all(&bytes).map_err(crate::io_error)?;
        owner.sync_all().map_err(crate::io_error)?;
        before_publish();
        let (detached, quarantine) = detach_stage(&stage, &owner)?;
        fs::hard_link(&detached, output)
            .map_err(|error| publication_error("MCP relay bundle", output, &detached, error))?;
        after_publish();
        fs::remove_file(&detached).map_err(|error| {
            Error::Run(format!(
                "MCP relay bundle was published at {}, but staging cleanup failed: {error}; inspect retained staging {}",
                output.display(), detached.display()
            ))
        })?;
        fs::remove_dir(&quarantine).map_err(|error| {
            published_cleanup_error("MCP relay bundle", output, &quarantine, error)
        })
    })()
}

fn read_bundle(path: &Path) -> Result<Vec<u8>, Error> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| Error::Run("MCP relay bundle is unavailable".into()))?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_BUNDLE_BYTES as u64
    {
        return Err(Error::Run(
            "MCP relay bundle is invalid or exceeds its bound".into(),
        ));
    }
    fs::read(path).map_err(crate::io_error)
}

struct Replayer {
    root: PathBuf,
    transcript: mcp_transcript::Transcript,
}

impl Replayer {
    fn load(root: &Path) -> Result<Self, Error> {
        let metadata = fs::symlink_metadata(root)
            .map_err(|_| Error::Run("MCP replay recording directory is unavailable".into()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Error::Run(
                "MCP replay recording directory is invalid".into(),
            ));
        }
        let index = root.join("index.jsonl");
        let metadata = fs::symlink_metadata(&index)
            .map_err(|_| Error::Run("MCP replay transcript is unavailable".into()))?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(Error::Run("MCP replay transcript is invalid".into()));
        }
        Ok(Self {
            root: root.to_path_buf(),
            transcript: mcp_transcript::Transcript::decode(
                &fs::read(index).map_err(crate::io_error)?,
            )?,
        })
    }

    fn payload(&self, reference: &mcp_transcript::Blob) -> Result<Vec<u8>, Error> {
        let path = self
            .root
            .join("payloads")
            .join(format!("{}.json", reference.sha256));
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| Error::Run("MCP replay payload is unavailable".into()))?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() != reference.bytes
        {
            return Err(Error::Run("MCP replay payload is invalid".into()));
        }
        let bytes = fs::read(path).map_err(crate::io_error)?;
        if mcp_transcript::payload_ref(&bytes) != *reference {
            return Err(Error::Run(
                "MCP replay payload differs from its transcript".into(),
            ));
        }
        Ok(bytes)
    }
}

/// Verify a completed relay recording before another ASB layer adopts it.
/// This reads only its bounded transcript and referenced payload objects; it
/// never starts an upstream MCP server or an agent process.
pub(crate) fn validate_recording(root: &Path) -> Result<(), Error> {
    let replay = Replayer::load(root)?;
    for event in replay.transcript.events() {
        replay.payload(&event.message)?;
    }
    Ok(())
}

/// Encode a completed relay directory as one bounded, cassette-safe JSONL
/// payload.  The filesystem layout remains private to the relay; callers get
/// one canonical byte stream suitable for the existing auxiliary-artifact
/// cassette contract.
pub(crate) fn bundle_recording(root: &Path) -> Result<Vec<u8>, Error> {
    let replay = Replayer::load(root)?;
    let mut payloads = BTreeMap::new();
    for event in replay.transcript.events() {
        payloads
            .entry(event.message.sha256.clone())
            .or_insert(replay.payload(&event.message)?);
    }
    encode_bundle(&replay.transcript, &payloads)
}

/// Canonical evidence for a declared server which an agent never launched.
/// Replaying this bundle rejects the first client request rather than allowing
/// an unrecorded host server to start.
pub(crate) fn empty_bundle() -> Result<Vec<u8>, Error> {
    encode_bundle(&mcp_transcript::Transcript::default(), &BTreeMap::new())
}

/// Validate a bundled relay artifact and reconstruct its private directory.
/// The destination must not already exist, so replay can never overwrite or
/// merge host MCP evidence with a cassette-selected recording.
pub(crate) fn materialize_bundle(bytes: &[u8], destination: &Path) -> Result<(), Error> {
    materialize_bundle_with(bytes, destination, || {})
}

fn materialize_bundle_with(
    bytes: &[u8],
    destination: &Path,
    before_publish: impl FnOnce(),
) -> Result<(), Error> {
    let bundle = decode_bundle(bytes)?;
    if destination.exists() {
        return Err(Error::Run(
            "MCP replay recording destination already exists".into(),
        ));
    }
    let parent = destination
        .parent()
        .filter(|parent| parent.is_dir())
        .ok_or_else(|| Error::Run("MCP replay recording parent is unavailable".into()))?;
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| Error::Run("MCP replay recording name is invalid".into()))?;
    let stage = reserve_stage(parent, name)?;
    let owner = open_stage(&stage).map_err(crate::io_error)?;
    if !same_stage(&owner, &stage) {
        return Err(Error::Run(format!(
            "MCP replay stage changed while opening {}; preserve existing evidence",
            stage.display()
        )));
    }
    let owned_root = owned_directory_path(&owner, &stage);
    (|| {
        fs::create_dir(owned_root.join("payloads")).map_err(crate::io_error)?;
        let payload_owner = open_stage(&owned_root.join("payloads")).map_err(crate::io_error)?;
        let owned_payloads = owned_directory_path(&payload_owner, &stage.join("payloads"));
        let index = mcp_transcript::canonicalize(&bundle.transcript.encode()?)?;
        write_new(&owned_root.join("index.jsonl"), &index)?;
        for (digest, payload) in &bundle.payloads {
            write_new(&owned_payloads.join(format!("{digest}.json")), payload)?;
        }
        before_publish();
        let (detached, quarantine) = detach_stage(&stage, &owner)?;
        rename_directory_noreplace(&detached, destination).map_err(|error| {
            publication_error("MCP replay recording", destination, &detached, error)
        })?;
        fs::remove_dir(&quarantine).map_err(|error| {
            published_cleanup_error("MCP replay recording", destination, &quarantine, error)
        })?;
        Ok(())
    })()
}

fn encode_bundle(
    transcript: &mcp_transcript::Transcript,
    payloads: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<u8>, Error> {
    let mut output = Vec::new();
    append_bundle_record(
        &mut output,
        &BundleRecord::Header {
            schema_version: BUNDLE_SCHEMA_VERSION,
        },
    )?;
    let mut referenced = BTreeMap::new();
    for event in transcript.events() {
        referenced.insert(event.message.sha256.clone(), event.message.clone());
        append_bundle_record(
            &mut output,
            &BundleRecord::Event {
                schema_version: BUNDLE_SCHEMA_VERSION,
                event: event.clone(),
            },
        )?;
    }
    if payloads.len() != referenced.len() || !payloads.keys().eq(referenced.keys()) {
        return Err(Error::Run(
            "MCP relay bundle payloads do not match its transcript".into(),
        ));
    }
    for (digest, blob) in referenced {
        let payload = payloads
            .get(&digest)
            .expect("payload keys were compared before encoding");
        if mcp_transcript::payload_ref(payload) != blob {
            return Err(Error::Run(
                "MCP relay bundle payload differs from its transcript".into(),
            ));
        }
        let text = std::str::from_utf8(payload)
            .map_err(|_| Error::Run("MCP relay payload is not UTF-8 JSON".into()))?;
        append_bundle_record(
            &mut output,
            &BundleRecord::Payload {
                schema_version: BUNDLE_SCHEMA_VERSION,
                blob,
                payload: text.into(),
            },
        )?;
    }
    Ok(output)
}

fn append_bundle_record(output: &mut Vec<u8>, record: &BundleRecord) -> Result<(), Error> {
    let line = serde_json::to_vec(record)
        .map_err(|_| Error::Io("cannot encode MCP relay bundle".into()))?;
    let next = output
        .len()
        .checked_add(line.len() + 1)
        .filter(|bytes| *bytes <= MAX_BUNDLE_BYTES)
        .ok_or_else(|| Error::Run("MCP relay bundle exceeds aggregate bound".into()))?;
    output.reserve(next - output.len());
    output.extend(line);
    output.push(b'\n');
    Ok(())
}

fn decode_bundle(bytes: &[u8]) -> Result<Bundle, Error> {
    if bytes.len() > MAX_BUNDLE_BYTES {
        return Err(Error::Run(
            "MCP relay bundle exceeds aggregate bound".into(),
        ));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::Run("MCP relay bundle is not UTF-8 JSONL".into()))?;
    let mut records = text.lines();
    match records.next().map(decode_bundle_record).transpose()? {
        Some(BundleRecord::Header { schema_version })
            if schema_version == BUNDLE_SCHEMA_VERSION => {}
        _ => return Err(Error::Run("MCP relay bundle header is invalid".into())),
    }
    let mut transcript = mcp_transcript::Transcript::default();
    let mut payloads = BTreeMap::new();
    let mut payload_bytes = 0u64;
    for line in records {
        match decode_bundle_record(line)? {
            BundleRecord::Event {
                schema_version,
                event,
            } if schema_version == BUNDLE_SCHEMA_VERSION => transcript.append(event)?,
            BundleRecord::Payload {
                schema_version,
                blob,
                payload,
            } if schema_version == BUNDLE_SCHEMA_VERSION => {
                let payload = payload.into_bytes();
                if mcp_transcript::payload_ref(&payload) != blob
                    || payload_bytes
                        .checked_add(blob.bytes)
                        .filter(|total| *total <= MAX_PAYLOAD_BYTES)
                        .is_none()
                    || payloads.insert(blob.sha256.clone(), payload).is_some()
                {
                    return Err(Error::Run("MCP relay bundle payload is invalid".into()));
                }
                payload_bytes += blob.bytes;
            }
            _ => return Err(Error::Run("MCP relay bundle record is invalid".into())),
        }
    }
    let expected = transcript
        .events()
        .iter()
        .map(|event| event.message.sha256.clone())
        .collect::<std::collections::BTreeSet<_>>();
    if payloads.len() != expected.len() || !payloads.keys().cloned().eq(expected) {
        return Err(Error::Run(
            "MCP relay bundle payloads do not match its transcript".into(),
        ));
    }
    Ok(Bundle {
        transcript,
        payloads,
    })
}

fn decode_bundle_record(line: &str) -> Result<BundleRecord, Error> {
    serde_json::from_str(line)
        .map_err(|_| Error::Run("MCP relay bundle record is malformed".into()))
}

fn publication_error(
    label: &str,
    destination: &Path,
    stage: &Path,
    error: std::io::Error,
) -> Error {
    let outcome = if error.kind() == std::io::ErrorKind::AlreadyExists {
        "was not published because the destination already exists"
    } else {
        "publication outcome is unconfirmed"
    };
    Error::Run(format!(
        "{label} {outcome} at {}: {error}; inspect retained staging {} before retrying",
        destination.display(),
        stage.display()
    ))
}

fn published_cleanup_error(
    label: &str,
    destination: &Path,
    stage: &Path,
    error: std::io::Error,
) -> Error {
    Error::Run(format!(
        "{label} was published at {}, but staging cleanup failed: {error}; inspect retained staging {}",
        destination.display(),
        stage.display()
    ))
}

fn reserve_stage(parent: &Path, name: &str) -> Result<PathBuf, Error> {
    for attempt in 0..128_u16 {
        let stage = parent.join(format!(
            ".{name}.mcp-replay-{0}-{attempt}",
            std::process::id()
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&stage) {
            Ok(()) => return Ok(stage),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(crate::io_error(error)),
        }
    }
    Err(Error::Run(
        "cannot reserve MCP replay recording destination".into(),
    ))
}

/// Detach the exposed stage into a fresh private namespace before checking its
/// identity. A substituted entry is restored without replacement when possible.
fn detach_stage(stage: &Path, owner: &fs::File) -> Result<(PathBuf, PathBuf), Error> {
    let parent = stage
        .parent()
        .ok_or_else(|| Error::Run("MCP stage has no parent".into()))?;
    let quarantine = parent.join(format!(
        ".asb-mcp-detached-{}",
        crate::tools::runtime::unique_id()
    ));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&quarantine).map_err(crate::io_error)?;
    let detached = quarantine.join("stage");
    if let Err(error) = fs::rename(stage, &detached) {
        let _ = fs::remove_dir(&quarantine);
        return Err(Error::Run(format!(
            "cannot detach MCP stage; existing evidence preserved: {error}"
        )));
    }
    if !same_stage(owner, &detached) {
        let restored = rename_directory_noreplace(&detached, stage).is_ok();
        if restored {
            let _ = fs::remove_dir(&quarantine);
        }
        return Err(Error::Run(if restored {
            format!(
                "MCP stage identity changed; replacement restored at {}; preserve existing evidence",
                stage.display()
            )
        } else {
            format!(
                "MCP stage identity changed; replacement retained at {}; preserve existing evidence",
                detached.display()
            )
        }));
    }
    Ok((detached, quarantine))
}

#[cfg(unix)]
fn same_stage(owner: &fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    owner
        .metadata()
        .ok()
        .zip(fs::symlink_metadata(path).ok())
        .is_some_and(|(a, b)| {
            a.dev() == b.dev() && a.ino() == b.ino() && a.file_type() == b.file_type()
        })
}

#[cfg(unix)]
fn open_stage(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
}

#[cfg(not(any(windows, unix)))]
fn open_stage(path: &Path) -> std::io::Result<fs::File> {
    fs::File::open(path)
}

#[cfg(windows)]
fn open_stage(path: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const BACKUP_SEMANTICS: u32 = 0x02000000;
    const OPEN_REPARSE_POINT: u32 = 0x00200000;
    OpenOptions::new()
        .read(true)
        .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn same_stage(owner: &fs::File, path: &Path) -> bool {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct Info {
        attributes: u32,
        creation: [u32; 2],
        access: [u32; 2],
        write: [u32; 2],
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(handle: *mut std::ffi::c_void, info: *mut Info) -> i32;
    }
    fn identity(file: &fs::File) -> Option<(u32, u32, u32)> {
        let mut info = std::mem::MaybeUninit::<Info>::uninit();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some((info.volume, info.index_high, info.index_low))
    }
    open_stage(path)
        .ok()
        .and_then(|file| identity(&file))
        .zip(identity(owner))
        .is_some_and(|(actual, expected)| actual == expected)
}

#[cfg(not(any(unix, windows)))]
fn same_stage(owner: &fs::File, path: &Path) -> bool {
    owner
        .metadata()
        .ok()
        .zip(fs::symlink_metadata(path).ok())
        .is_some_and(|(a, b)| {
            a.file_type() == b.file_type()
                && a.len() == b.len()
                && a.created().ok() == b.created().ok()
                && a.modified().ok() == b.modified().ok()
        })
}

#[cfg(target_os = "linux")]
fn owned_directory_path(owner: &fs::File, fallback: &Path) -> PathBuf {
    use std::os::fd::AsRawFd;
    let pinned = PathBuf::from(format!("/proc/self/fd/{}", owner.as_raw_fd()));
    if pinned.is_dir() {
        pinned
    } else {
        fallback.to_path_buf()
    }
}

#[cfg(not(target_os = "linux"))]
fn owned_directory_path(_: &fs::File, fallback: &Path) -> PathBuf {
    fallback.to_path_buf()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(unsafe_code)]
fn rename_directory_noreplace(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("invalid staging path"))?;
    let target = std::ffi::CString::new(target.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("invalid destination path"))?;
    // SAFETY: both pointers refer to live, NUL-terminated filesystem paths.
    #[cfg(target_os = "linux")]
    let status = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let status = unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn rename_directory_noreplace(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileW(source: *const u16, target: *const u16) -> i32;
    }
    let mut source: Vec<u16> = source.as_os_str().encode_wide().collect();
    let mut target: Vec<u16> = target.as_os_str().encode_wide().collect();
    if source.contains(&0) || target.contains(&0) {
        return Err(std::io::Error::other("path contains NUL"));
    }
    source.push(0);
    target.push(0);
    // SAFETY: both buffers are NUL-terminated and stay alive during the call.
    if unsafe { MoveFileW(source.as_ptr(), target.as_ptr()) } != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn rename_directory_noreplace(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace directory publication is unavailable",
    ))
}

fn reserve_file(parent: &Path, name: &str, purpose: &str) -> Result<(PathBuf, fs::File), Error> {
    for attempt in 0..128_u16 {
        let stage = parent.join(format!(
            ".{name}.{purpose}-{0}-{attempt}",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&stage) {
            Ok(file) => return Ok((stage, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(crate::io_error(error)),
        }
    }
    Err(Error::Run(
        "cannot reserve MCP relay bundle destination".into(),
    ))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(crate::io_error)?;
    file.write_all(bytes).map_err(crate::io_error)?;
    file.sync_all().map_err(crate::io_error)
}

#[cfg(test)]
fn replay_messages<R: Read, W: Write>(
    reader: R,
    writer: W,
    replay: &Replayer,
) -> Result<(), Error> {
    replay_messages_with(reader, writer, replay, None)
}

fn replay_messages_with<R: Read, W: Write>(
    reader: R,
    mut writer: W,
    replay: &Replayer,
    journal: Option<&mut McpJournal>,
) -> Result<(), Error> {
    let mut observation = super::tool_tracing::Trace::from_environment();
    let mut reader = BufReader::new(reader);
    let mut ids = BTreeMap::<String, serde_json::Value>::new();
    let mut pending = BTreeMap::<String, Option<u64>>::new();
    let mut outstanding = std::collections::BTreeSet::<String>::new();
    for (index, event) in replay.transcript.events().iter().enumerate() {
        let recorded = replay.payload(&event.message)?;
        match event.direction {
            mcp_transcript::Direction::ClientToServer => {
                let actual = match mcp_framing::read(&mut reader, SERVICE)? {
                    Some(actual) => actual,
                    None if journal.is_some() && outstanding.is_empty() => {
                        journal
                            .as_ref()
                            .expect("checked")
                            .write(McpIgnoredKind::Unconsumed {
                                next_sequence: event.sequence,
                                client_messages: replay.transcript.events()[index..]
                                    .iter()
                                    .filter(|event| {
                                        event.direction == mcp_transcript::Direction::ClientToServer
                                    })
                                    .count(),
                            })?;
                        return Ok(());
                    }
                    None => {
                        return Err(Error::Run(
                            "MCP replay ended before its recorded client message".into(),
                        ));
                    }
                };
                if actual.framing != event.framing {
                    return Err(Error::Run(
                        "MCP replay client framing differs from recording".into(),
                    ));
                }
                let expected: serde_json::Value = serde_json::from_slice(&recorded)
                    .map_err(|_| Error::Run("MCP replay payload is not JSON".into()))?;
                match event.kind {
                    mcp_transcript::MessageKind::Request => {
                        let expected_id = expected.get("id").cloned().ok_or_else(|| {
                            Error::Run("MCP replay request has no recorded ID".into())
                        })?;
                        let actual_id = actual.value.get("id").cloned().ok_or_else(|| {
                            Error::Run("MCP replay request has no client ID".into())
                        })?;
                        if journal.is_some() && !actual_id.is_string() && !actual_id.is_number() {
                            return Err(Error::Run("MCP replay client ID is invalid".into()));
                        }
                        if without_id(&expected) != without_id(&actual.value) {
                            let compatible = compatible_request(&expected, &actual.value);
                            if compatible && let Some(journal) = journal.as_ref() {
                                journal.write(McpIgnoredKind::Request {
                                    sequence: event.sequence,
                                    expected_sha256: mcp_transcript::payload_ref(&recorded).sha256,
                                    actual_sha256: mcp_transcript::payload_ref(&actual.bytes)
                                        .sha256,
                                })?;
                            } else {
                                return Err(Error::Run(
                                    "MCP replay request differs from recording".into(),
                                ));
                            }
                        }
                        let recorded_id = event.id.as_ref().ok_or_else(|| {
                            Error::Run("MCP replay request transcript has no ID".into())
                        })?;
                        if mcp_transcript::payload_ref(
                            &serde_json::to_vec(&expected_id).expect("JSON encodes"),
                        ) != *recorded_id
                        {
                            return Err(Error::Run(
                                "MCP replay request ID differs from transcript".into(),
                            ));
                        }
                        if journal.is_some()
                            && (outstanding.contains(&recorded_id.sha256)
                                || outstanding.iter().any(|id| ids.get(id) == Some(&actual_id)))
                        {
                            return Err(Error::Run(
                                "MCP replay request ID is already outstanding".into(),
                            ));
                        }
                        ids.insert(recorded_id.sha256.clone(), actual_id);
                        outstanding.insert(recorded_id.sha256.clone());
                        if actual
                            .value
                            .get("method")
                            .and_then(serde_json::Value::as_str)
                            == Some("tools/call")
                        {
                            pending.insert(recorded_id.sha256.clone(), observation.begin("mcp"));
                        }
                    }
                    mcp_transcript::MessageKind::Notification => {
                        if expected != actual.value {
                            if compatible_request(&expected, &actual.value)
                                && let Some(journal) = journal.as_ref()
                            {
                                journal.write(McpIgnoredKind::Request {
                                    sequence: event.sequence,
                                    expected_sha256: mcp_transcript::payload_ref(&recorded).sha256,
                                    actual_sha256: mcp_transcript::payload_ref(&actual.bytes)
                                        .sha256,
                                })?;
                            } else {
                                return Err(Error::Run(
                                    "MCP replay notification differs from recording".into(),
                                ));
                            }
                        }
                    }
                    _ => {
                        return Err(Error::Run(
                            "MCP replay transcript direction is invalid".into(),
                        ));
                    }
                }
            }
            mcp_transcript::Direction::ServerToClient => {
                let bytes = match event.kind {
                    mcp_transcript::MessageKind::Response | mcp_transcript::MessageKind::Error => {
                        let recorded_id = event.id.as_ref().ok_or_else(|| {
                            Error::Run("MCP replay response transcript has no ID".into())
                        })?;
                        let actual_id = ids.get(&recorded_id.sha256).ok_or_else(|| {
                            Error::Run("MCP replay response has no matching client request".into())
                        })?;
                        let was_outstanding = outstanding.remove(&recorded_id.sha256);
                        if journal.is_some() && !was_outstanding {
                            return Err(Error::Run(
                                "MCP replay response has no outstanding request".into(),
                            ));
                        }
                        let mut value: serde_json::Value = serde_json::from_slice(&recorded)
                            .map_err(|_| Error::Run("MCP replay payload is not JSON".into()))?;
                        value["id"] = actual_id.clone();
                        let failed = event.kind == mcp_transcript::MessageKind::Error
                            || value.get("error").is_some()
                            || value
                                .get("result")
                                .and_then(|result| result.get("isError"))
                                .and_then(serde_json::Value::as_bool)
                                == Some(true);
                        observation.finish(pending.remove(&recorded_id.sha256).flatten(), failed);
                        serde_json::to_vec(&value)
                            .map_err(|_| Error::Run("cannot encode MCP replay response".into()))?
                    }
                    mcp_transcript::MessageKind::Notification => recorded,
                    mcp_transcript::MessageKind::Request => {
                        return Err(Error::Run(
                            "MCP replay transcript direction is invalid".into(),
                        ));
                    }
                };
                mcp_framing::write_bytes(&mut writer, &bytes, event.framing)?;
            }
        }
    }
    if mcp_framing::read(&mut reader, SERVICE)?.is_some() {
        return Err(Error::Run(
            "MCP replay received an unrecorded client message".into(),
        ));
    }
    Ok(())
}

fn compatible_request(expected: &serde_json::Value, actual: &serde_json::Value) -> bool {
    if expected.get("jsonrpc") != Some(&serde_json::json!("2.0"))
        || actual.get("jsonrpc") != Some(&serde_json::json!("2.0"))
    {
        return false;
    }
    let Some(method) = expected.get("method").and_then(serde_json::Value::as_str) else {
        return false;
    };
    if actual.get("method").and_then(serde_json::Value::as_str) != Some(method)
        || expected.get("id").is_some() != actual.get("id").is_some()
    {
        return false;
    }
    method != "tools/call"
        || expected
            .pointer("/params/name")
            .and_then(serde_json::Value::as_str)
            .zip(
                actual
                    .pointer("/params/name")
                    .and_then(serde_json::Value::as_str),
            )
            .is_some_and(|(a, b)| a == b)
}

fn without_id(value: &serde_json::Value) -> serde_json::Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("id");
    }
    value
}

enum Incoming {
    Message {
        direction: mcp_transcript::Direction,
        message: mcp_framing::Message,
    },
    End(mcp_transcript::Direction),
    Error(Error),
}

/// Relay both stdio directions concurrently while recording only committed,
/// bounded private evidence. The caller must give distinct read/write handles
/// for each full-duplex endpoint.
fn relay_record<ClientRead, ClientWrite, ServerWrite, ServerRead>(
    client_read: ClientRead,
    client_write: ClientWrite,
    server_write: ServerWrite,
    server_read: ServerRead,
    mut recorder: Recorder,
) -> Result<(), Error>
where
    ClientRead: Read + Send + 'static,
    ClientWrite: Write,
    ServerWrite: Write,
    ServerRead: Read + Send + 'static,
{
    let (send, receive) = mpsc::sync_channel(CHANNEL_CAPACITY);
    let client_send = send.clone();
    let client_reader = std::thread::Builder::new()
        .name("asb-mcp-client-reader".into())
        .spawn(move || {
            read_messages(
                client_read,
                mcp_transcript::Direction::ClientToServer,
                client_send,
            )
        })
        .map_err(crate::io_error)?;
    let server_reader = std::thread::Builder::new()
        .name("asb-mcp-server-reader".into())
        .spawn(move || read_messages(server_read, mcp_transcript::Direction::ServerToClient, send))
        .map_err(crate::io_error)?;
    let result = forward_messages(client_write, server_write, &receive, &mut recorder);
    if result.is_ok() {
        client_reader
            .join()
            .map_err(|_| Error::Run("MCP relay client reader panicked".into()))?;
        server_reader
            .join()
            .map_err(|_| Error::Run("MCP relay server reader panicked".into()))?;
    }
    result
}

fn read_messages<R: Read>(
    reader: R,
    direction: mcp_transcript::Direction,
    send: SyncSender<Incoming>,
) {
    let mut reader = BufReader::new(reader);
    loop {
        match mcp_framing::read(&mut reader, SERVICE) {
            Ok(Some(message)) => {
                if send.send(Incoming::Message { direction, message }).is_err() {
                    return;
                }
            }
            Ok(None) => {
                let _ = send.send(Incoming::End(direction));
                return;
            }
            Err(error) => {
                let _ = send.send(Incoming::Error(error));
                return;
            }
        }
    }
}

fn forward_messages<ClientWrite: Write, ServerWrite: Write>(
    mut client_write: ClientWrite,
    server_write: ServerWrite,
    receive: &Receiver<Incoming>,
    recorder: &mut Recorder,
) -> Result<(), Error> {
    let mut observation = super::tool_tracing::Trace::from_environment();
    let mut client_closed = false;
    let mut server_closed = false;
    let mut server_write = Some(server_write);
    let mut pending = BTreeMap::<String, Option<u64>>::new();
    let started = Instant::now();
    while !client_closed || !server_closed {
        match receive
            .recv()
            .map_err(|_| Error::Run("MCP relay stream unexpectedly closed".into()))?
        {
            Incoming::Message { direction, message } => {
                if let Some(id) = message.value.get("id") {
                    let key = serde_json::to_string(id)
                        .map_err(|_| Error::Run("MCP relay ID is invalid".into()))?;
                    if direction == mcp_transcript::Direction::ClientToServer
                        && message
                            .value
                            .get("method")
                            .and_then(serde_json::Value::as_str)
                            == Some("tools/call")
                    {
                        pending.insert(key, observation.begin("mcp"));
                    } else if direction == mcp_transcript::Direction::ServerToClient {
                        let failed = message.value.get("error").is_some()
                            || message
                                .value
                                .get("result")
                                .and_then(|result| result.get("isError"))
                                .and_then(serde_json::Value::as_bool)
                                == Some(true);
                        observation.finish(pending.remove(&key).flatten(), failed);
                    }
                }
                let output: &mut dyn Write = match direction {
                    mcp_transcript::Direction::ClientToServer => server_write
                        .as_mut()
                        .ok_or_else(|| Error::Run("MCP relay upstream input is closed".into()))?,
                    mcp_transcript::Direction::ServerToClient => &mut client_write,
                };
                recorder.record(
                    direction,
                    message.framing,
                    started.elapsed().as_millis() as u64,
                    &message.bytes,
                )?;
                mcp_framing::write_bytes(output, &message.bytes, message.framing)?;
            }
            Incoming::End(mcp_transcript::Direction::ClientToServer) => {
                client_closed = true;
                // Dropping the upstream input is its EOF signal. `flush` alone
                // is insufficient for servers which wait for complete input.
                drop(server_write.take());
            }
            Incoming::End(mcp_transcript::Direction::ServerToClient) => {
                server_closed = true;
                client_write.flush().map_err(crate::io_error)?;
            }
            Incoming::Error(error) => return Err(error),
        }
    }
    recorder.finish()
}

struct Recorder {
    target: PathBuf,
    stage: PathBuf,
    stage_owner: fs::File,
    payload_owner: fs::File,
    owned_files: Vec<(PathBuf, fs::File)>,
    published: bool,
    retain_stage: bool,
    transcript: mcp_transcript::Transcript,
    payload_bytes: u64,
}

impl Recorder {
    fn new(target: &Path) -> Result<Self, Error> {
        if target.exists() {
            return Err(Error::Run(
                "MCP relay recording destination already exists".into(),
            ));
        }
        let parent = target
            .parent()
            .filter(|parent| parent.is_dir())
            .ok_or_else(|| Error::Run("MCP relay recording parent is unavailable".into()))?;
        let name = target
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty() && *name != "." && *name != "..")
            .ok_or_else(|| Error::Run("MCP relay recording name is invalid".into()))?;
        for attempt in 0..128_u16 {
            let stage = parent.join(format!(
                ".{name}.mcp-record-{}-{attempt}",
                std::process::id()
            ));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&stage) {
                Ok(()) => {
                    let stage_owner = open_stage(&stage).map_err(crate::io_error)?;
                    if !same_stage(&stage_owner, &stage) {
                        return Err(Error::Run(format!(
                            "MCP recording stage changed while opening {}; preserve existing evidence",
                            stage.display()
                        )));
                    }
                    let payload_path = owned_directory_path(&stage_owner, &stage).join("payloads");
                    fs::create_dir(&payload_path).map_err(crate::io_error)?;
                    let payload_owner = open_stage(&payload_path).map_err(crate::io_error)?;
                    return Ok(Self {
                        target: target.to_path_buf(),
                        stage,
                        stage_owner,
                        payload_owner,
                        owned_files: Vec::new(),
                        published: false,
                        retain_stage: false,
                        transcript: mcp_transcript::Transcript::default(),
                        payload_bytes: 0,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(crate::io_error(error)),
            }
        }
        Err(Error::Run(
            "cannot reserve MCP relay recording destination".into(),
        ))
    }

    fn record(
        &mut self,
        direction: mcp_transcript::Direction,
        framing: mcp_framing::Framing,
        relative_millis: u64,
        payload: &[u8],
    ) -> Result<(), Error> {
        if !same_stage(&self.stage_owner, &self.stage) {
            return Err(Error::Run(
                "MCP recording stage identity changed; preserve existing evidence".into(),
            ));
        }
        self.transcript
            .record(direction, framing, relative_millis, payload)?;
        let reference = mcp_transcript::payload_ref(payload);
        let filename = format!("{}.json", reference.sha256);
        let path =
            owned_directory_path(&self.payload_owner, &self.stage.join("payloads")).join(&filename);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                if fs::read(&path).map_err(crate::io_error)? != payload {
                    return Err(Error::Run("MCP relay payload digest collision".into()));
                }
            }
            Ok(_) => {
                return Err(Error::Run(
                    "MCP relay payload path is not a regular file".into(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.payload_bytes = self
                    .payload_bytes
                    .checked_add(reference.bytes)
                    .filter(|bytes| *bytes <= MAX_PAYLOAD_BYTES)
                    .ok_or_else(|| {
                        Error::Run("MCP relay payloads exceed aggregate bound".into())
                    })?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map_err(crate::io_error)?;
                file.write_all(payload).map_err(crate::io_error)?;
                file.sync_all().map_err(crate::io_error)?;
                self.owned_files
                    .push((PathBuf::from("payloads").join(filename), file));
            }
            Err(error) => return Err(crate::io_error(error)),
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), Error> {
        if !same_stage(&self.stage_owner, &self.stage) {
            return Err(Error::Run(
                "MCP recording stage identity changed; preserve existing evidence".into(),
            ));
        }
        let bytes = mcp_transcript::canonicalize(&self.transcript.encode()?)?;
        let index = owned_directory_path(&self.stage_owner, &self.stage).join("index.jsonl");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&index)
            .map_err(crate::io_error)?;
        file.write_all(&bytes).map_err(crate::io_error)?;
        file.sync_all().map_err(crate::io_error)?;
        self.owned_files.push((PathBuf::from("index.jsonl"), file));
        let (detached, quarantine) = detach_stage(&self.stage, &self.stage_owner)?;
        self.stage = detached;
        if let Err(error) = rename_directory_noreplace(&self.stage, &self.target) {
            self.retain_stage = true;
            return Err(publication_error(
                "MCP relay recording",
                &self.target,
                &self.stage,
                error,
            ));
        }
        self.published = true;
        fs::remove_dir(&quarantine).map_err(|error| {
            published_cleanup_error("MCP relay recording", &self.target, &quarantine, error)
        })?;
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if self.published || self.retain_stage {
            return;
        }
        let Ok((detached, quarantine)) = detach_stage(&self.stage, &self.stage_owner) else {
            return;
        };
        if !same_stage(&self.payload_owner, &detached.join("payloads")) {
            return;
        }
        let expected_root = 1 + usize::from(
            self.owned_files
                .iter()
                .any(|(path, _)| path == Path::new("index.jsonl")),
        );
        let expected_payloads = self
            .owned_files
            .iter()
            .filter(|(path, _)| path.starts_with("payloads"))
            .count();
        if fs::read_dir(&detached).ok().map(|entries| entries.count()) != Some(expected_root)
            || fs::read_dir(detached.join("payloads"))
                .ok()
                .map(|entries| entries.count())
                != Some(expected_payloads)
            || self
                .owned_files
                .iter()
                .any(|(path, owner)| !same_stage(owner, &detached.join(path)))
        {
            return;
        }
        for (path, _) in &self.owned_files {
            if fs::remove_file(detached.join(path)).is_err() {
                return;
            }
        }
        if fs::remove_dir(detached.join("payloads")).is_err() || fs::remove_dir(&detached).is_err()
        {
            return;
        }
        let _ = fs::remove_dir(quarantine);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

    fn retained_stage(error: &Error) -> PathBuf {
        let text = error.to_string();
        let path = text
            .split_once("inspect retained staging ")
            .expect("retained path in error")
            .1
            .split(" before retrying")
            .next()
            .unwrap();
        PathBuf::from(path)
    }

    #[test]
    fn post_publication_cleanup_failure_names_published_output_and_retained_stage() {
        let record = replay_recording("cleanup-after-publish");
        let parent = record.parent().unwrap();
        let output = parent.join("published.jsonl");
        let error = write_bundle_with_hooks(
            &record,
            &output,
            || {},
            || {
                let quarantine = fs::read_dir(parent)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".asb-mcp-detached-")
                    })
                    .unwrap();
                fs::write(quarantine.join("foreign"), "preserve").unwrap();
            },
        )
        .unwrap_err();
        let text = error.to_string();
        assert!(text.contains(&format!("was published at {}", output.display())));
        assert!(output.is_file());
        assert_eq!(
            fs::read_to_string(retained_stage(&error).join("foreign")).unwrap(),
            "preserve"
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn opening_stage_rejects_a_directory_symlink() {
        let root = record_dir("stage-link");
        let parent = root.parent().unwrap();
        let foreign = parent.join("foreign");
        fs::create_dir(&foreign).unwrap();
        std::os::unix::fs::symlink(&foreign, &root).unwrap();
        assert!(open_stage(&root).is_err());
        assert!(foreign.is_dir());
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn replaced_bundle_stage_is_not_published_or_deleted() {
        let record = replay_recording("bundle-stage-swap");
        let parent = record.parent().unwrap();
        let output = parent.join("racing.jsonl");
        let mut foreign = None;
        let result = write_bundle_with(&record, &output, || {
            let stage = fs::read_dir(parent)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains(".mcp-bundle-")
                })
                .unwrap();
            fs::rename(&stage, parent.join("held-original")).unwrap();
            fs::write(&stage, "foreign").unwrap();
            foreign = Some(stage);
        });
        assert!(result.is_err());
        assert!(!output.exists());
        assert_eq!(fs::read_to_string(foreign.unwrap()).unwrap(), "foreign");
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn replaced_replay_stage_is_not_published_or_deleted() {
        let record = replay_recording("replay-stage-swap");
        let bundle = bundle_recording(&record).unwrap();
        let parent = record.parent().unwrap();
        let output = parent.join("replay");
        let mut foreign = None;
        let result = materialize_bundle_with(&bundle, &output, || {
            let stage = fs::read_dir(parent)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains(".mcp-replay-")
                })
                .unwrap();
            fs::rename(&stage, parent.join("held-original")).unwrap();
            fs::create_dir(&stage).unwrap();
            fs::write(stage.join("foreign"), "foreign").unwrap();
            foreign = Some(stage);
        });
        assert!(result.is_err());
        assert!(!output.exists());
        assert_eq!(
            fs::read_to_string(foreign.unwrap().join("foreign")).unwrap(),
            "foreign"
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn replaced_recorder_stage_is_preserved_on_error() {
        let target = record_dir("record-stage-swap");
        let mut recorder = Recorder::new(&target).unwrap();
        let stage = recorder.stage.clone();
        fs::rename(&stage, stage.with_extension("held-original")).unwrap();
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("foreign"), "foreign").unwrap();
        assert!(recorder.finish().is_err());
        drop(recorder);
        assert!(!target.exists());
        assert_eq!(
            fs::read_to_string(stage.join("foreign")).unwrap(),
            "foreign"
        );
        fs::remove_dir_all(target.parent().unwrap()).unwrap();
    }

    #[test]
    fn recorder_cleanup_preserves_unexpected_stage_entries() {
        let target = record_dir("record-stage-unexpected");
        let recorder = Recorder::new(&target).unwrap();
        fs::write(recorder.stage.join("foreign"), "preserve").unwrap();
        drop(recorder);
        let retained = fs::read_dir(target.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".asb-mcp-detached-")
            })
            .unwrap();
        assert_eq!(
            fs::read_to_string(retained.join("stage/foreign")).unwrap(),
            "preserve"
        );
        fs::remove_dir_all(target.parent().unwrap()).unwrap();
    }

    #[test]
    fn concurrent_bundle_destination_is_never_overwritten() {
        let record = replay_recording("bundle-racing-destination");
        let output = record.parent().unwrap().join("racing.jsonl");
        let result = write_bundle_with(&record, &output, || {
            fs::write(&output, "other writer").unwrap()
        });
        let error = result.unwrap_err();
        assert!(error.to_string().contains("was not published"));
        assert!(retained_stage(&error).is_file());
        assert_eq!(fs::read_to_string(&output).unwrap(), "other writer");
        fs::remove_dir_all(record.parent().unwrap()).unwrap();
    }

    #[test]
    fn recording_does_not_replace_a_new_empty_destination_directory() {
        let target = record_dir("record-racing-destination");
        let mut recorder = Recorder::new(&target).unwrap();
        fs::create_dir(&target).unwrap();
        let error = recorder.finish().unwrap_err();
        assert!(error.to_string().contains("was not published"));
        let retained = retained_stage(&error);
        assert!(fs::read_dir(&target).unwrap().next().is_none());
        drop(recorder);
        assert!(retained.join("index.jsonl").is_file());
        fs::remove_dir_all(target.parent().unwrap()).unwrap();
    }

    #[test]
    fn replay_does_not_replace_a_new_empty_destination_directory() {
        let record = replay_recording("replay-racing-destination");
        let bundle = bundle_recording(&record).unwrap();
        let target = record.parent().unwrap().join("replay");
        let result = materialize_bundle_with(&bundle, &target, || fs::create_dir(&target).unwrap());
        let error = result.unwrap_err();
        assert!(error.to_string().contains("was not published"));
        assert!(retained_stage(&error).join("index.jsonl").is_file());
        assert!(fs::read_dir(&target).unwrap().next().is_none());
        fs::remove_dir_all(record.parent().unwrap()).unwrap();
    }

    fn record_dir(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("asb-mcp-relay-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root.join("recording")
    }

    fn replay_recording(name: &str) -> PathBuf {
        let record = record_dir(name);
        let mut recorder = Recorder::new(&record).unwrap();
        recorder
            .record(
                mcp_transcript::Direction::ClientToServer,
                mcp_framing::Framing::JsonLines,
                0,
                br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            )
            .unwrap();
        recorder
            .record(
                mcp_transcript::Direction::ServerToClient,
                mcp_framing::Framing::JsonLines,
                1,
                br#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}"#,
            )
            .unwrap();
        recorder
            .record(
                mcp_transcript::Direction::ServerToClient,
                mcp_framing::Framing::JsonLines,
                2,
                br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#,
            )
            .unwrap();
        recorder.finish().unwrap();
        record
    }

    #[test]
    fn forwards_both_directions_and_publishes_opaque_private_recording() {
        let record = record_dir("forward");
        let (client_relay, mut client) = UnixStream::pair().unwrap();
        let (server_relay, mut server) = UnixStream::pair().unwrap();
        let client_reader = client_relay.try_clone().unwrap();
        let server_reader = server_relay.try_clone().unwrap();
        let record_for_relay = record.clone();
        let relay = std::thread::spawn(move || {
            relay_record(
                client_reader,
                client_relay,
                server_relay,
                server_reader,
                Recorder::new(&record_for_relay).unwrap(),
            )
        });
        let server_task = std::thread::spawn(move || {
            let mut input = BufReader::new(server.try_clone().unwrap());
            let request = mcp_framing::read(&mut input, SERVICE).unwrap().unwrap();
            assert_eq!(request.value["id"], 1);
            mcp_framing::write_bytes(
                &mut server,
                br#"{ "jsonrpc":"2.0", "method":"notifications/progress" }"#,
                request.framing,
            )
            .unwrap();
            mcp_framing::write_bytes(
                &mut server,
                br#"{ "jsonrpc":"2.0", "id":1, "result":{} }"#,
                request.framing,
            )
            .unwrap();
            server.shutdown(Shutdown::Write).unwrap();
        });
        mcp_framing::write_bytes(
            &mut client,
            br#"{ "jsonrpc":"2.0", "id":1, "method":"tools/list" }"#,
            mcp_framing::Framing::JsonLines,
        )
        .unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        let mut output = BufReader::new(client.try_clone().unwrap());
        let notification = mcp_framing::read(&mut output, SERVICE).unwrap().unwrap();
        let response = mcp_framing::read(&mut output, SERVICE).unwrap().unwrap();
        assert_eq!(notification.value["method"], "notifications/progress");
        assert_eq!(response.value["id"], 1);
        server_task.join().unwrap();
        relay.join().unwrap().unwrap();
        let transcript =
            mcp_transcript::Transcript::decode(&fs::read(record.join("index.jsonl")).unwrap())
                .unwrap();
        assert_eq!(transcript.events().len(), 3);
        assert_eq!(fs::read_dir(record.join("payloads")).unwrap().count(), 3);
        let _ = fs::remove_dir_all(record.parent().unwrap());
    }

    #[test]
    fn rejects_invalid_recording_without_publication() {
        let record = record_dir("reject");
        let (client_relay, mut client) = UnixStream::pair().unwrap();
        let (server_relay, server) = UnixStream::pair().unwrap();
        let client_reader = client_relay.try_clone().unwrap();
        let server_reader = server_relay.try_clone().unwrap();
        let record_for_relay = record.clone();
        let relay = std::thread::spawn(move || {
            relay_record(
                client_reader,
                client_relay,
                server_relay,
                server_reader,
                Recorder::new(&record_for_relay).unwrap(),
            )
        });
        let server_task = std::thread::spawn(move || {
            // This fixture has no server-to-client message; close that
            // direction so the relay's independent server reader can settle.
            server.shutdown(Shutdown::Write).unwrap();
            let mut input = BufReader::new(server);
            assert!(mcp_framing::read(&mut input, SERVICE).unwrap().is_none());
        });
        mcp_framing::write_bytes(
            &mut client,
            br#"{"not":"json-rpc"}"#,
            mcp_framing::Framing::JsonLines,
        )
        .unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        assert!(relay.join().unwrap().is_err());
        server_task.join().unwrap();
        assert!(!record.exists());
        let _ = fs::remove_dir_all(record.parent().unwrap());
    }

    #[test]
    fn aggregate_payload_budget_removes_partial_recording() {
        let record = record_dir("payload-budget");
        let mut recorder = Recorder::new(&record).unwrap();
        for index in 0..4 {
            let payload = format!(
                "{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{\"index\":{index},\"detail\":\"{}\"}}}}",
                "x".repeat(1024 * 1024 - 128)
            );
            recorder
                .record(
                    mcp_transcript::Direction::ClientToServer,
                    mcp_framing::Framing::JsonLines,
                    index,
                    payload.as_bytes(),
                )
                .expect("payload below the aggregate budget is retained");
        }
        let payload = format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{\"detail\":\"{}\"}}}}",
            "x".repeat(1024 * 1024 - 128)
        );
        let error = recorder
            .record(
                mcp_transcript::Direction::ClientToServer,
                mcp_framing::Framing::JsonLines,
                4,
                payload.as_bytes(),
            )
            .expect_err("payload over the aggregate budget is rejected");
        assert!(error.to_string().contains("aggregate bound"));
        drop(recorder);
        assert!(!record.exists());
        assert_eq!(fs::read_dir(record.parent().unwrap()).unwrap().count(), 0);
        let _ = fs::remove_dir_all(record.parent().unwrap());
    }

    #[test]
    fn options_require_a_record_directory_and_direct_program() {
        assert!(options(&[]).is_err());
        assert!(options(&["--record-dir".into(), "out".into()]).is_err());
        let Options::Record(record_options) = options(&[
            "--record-dir".into(),
            "out".into(),
            "--".into(),
            "server".into(),
            "--flag".into(),
        ])
        .unwrap() else {
            panic!("record arguments must select record mode");
        };
        assert_eq!(record_options.record_dir, PathBuf::from("out"));
        assert_eq!(record_options.program, OsString::from("server"));
        assert_eq!(record_options.args, vec![OsString::from("--flag")]);

        let Options::Bundle { directory, output } = options(&[
            "--bundle-dir".into(),
            "recording".into(),
            "--output".into(),
            "bundle.jsonl".into(),
        ])
        .unwrap() else {
            panic!("bundle arguments must select bundle mode");
        };
        assert_eq!(directory, PathBuf::from("recording"));
        assert_eq!(output, PathBuf::from("bundle.jsonl"));

        let Options::Materialize {
            bundle,
            destination,
        } = options(&[
            "--materialize-bundle".into(),
            "bundle.jsonl".into(),
            "--output".into(),
            "recording".into(),
        ])
        .unwrap()
        else {
            panic!("materialize arguments must select materialize mode");
        };
        assert_eq!(bundle, PathBuf::from("bundle.jsonl"));
        assert_eq!(destination, PathBuf::from("recording"));
    }

    #[test]
    fn offline_replay_remaps_request_ids_without_an_upstream_server() {
        let record = replay_recording("replay");
        validate_recording(&record).unwrap();
        let replay = Replayer::load(&record).unwrap();
        let mut output = Vec::new();
        replay_messages(
            Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"tools/list\"}\n"),
            &mut output,
            &replay,
        )
        .unwrap();
        let mut output = BufReader::new(&output[..]);
        let notification = mcp_framing::read(&mut output, SERVICE).unwrap().unwrap();
        let response = mcp_framing::read(&mut output, SERVICE).unwrap().unwrap();
        assert_eq!(notification.value["method"], "notifications/progress");
        assert_eq!(response.value["id"], 42);
        let _ = fs::remove_dir_all(record.parent().unwrap());
    }

    #[test]
    fn approximate_replay_preserves_id_mapping_and_records_only_digests() {
        let record = replay_recording("approximate-mcp");
        let replay = Replayer::load(&record).unwrap();
        let path = record.parent().unwrap().join("journal.jsonl");
        fs::write(&path, b"").unwrap();
        let mut journal = McpJournal {
            path: path.clone(),
            server: "fixture".into(),
            session: "one".into(),
        };
        journal.write(McpIgnoredKind::Started).unwrap();
        let mut output = Vec::new();
        let request = b"{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"tools/list\",\"params\":{\"private\":\"secret-value\"}}\n";
        assert!(replay_messages(Cursor::new(request), Vec::new(), &replay).is_err());
        replay_messages_with(
            Cursor::new(request),
            &mut output,
            &replay,
            Some(&mut journal),
        )
        .unwrap();
        journal.write(McpIgnoredKind::Finished).unwrap();
        let result = String::from_utf8(output).unwrap();
        assert!(result.contains("\"id\":42"));
        let events = fs::read_to_string(path).unwrap();
        assert!(events.contains("\"kind\":\"request\""));
        assert!(!events.contains("secret-value"));
        assert!(replay_messages_with(Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"tools/call\",\"params\":{\"name\":\"other\"}}\n"), Vec::new(), &replay, Some(&mut journal)).is_err());
        assert!(replay_messages_with(Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"tools/list\"}\n{\"jsonrpc\":\"2.0\",\"id\":43,\"method\":\"tools/list\"}\n"), Vec::new(), &replay, Some(&mut journal)).is_err(), "an extra request has no recorded response");
        let payload = replay.transcript.events()[0].message.sha256.clone();
        fs::write(
            record.join("payloads").join(format!("{payload}.json")),
            b"{}\n",
        )
        .unwrap();
        assert!(
            replay_messages_with(
                Cursor::new(request),
                Vec::new(),
                &replay,
                Some(&mut journal)
            )
            .is_err(),
            "a corrupt payload must not be approximated"
        );
        fs::remove_dir_all(record.parent().unwrap()).unwrap();
    }

    #[test]
    fn approximate_replay_reports_unconsumed_messages_but_not_outstanding_responses() {
        let record = replay_recording("approximate-mcp-eof");
        let replay = Replayer::load(&record).unwrap();
        let path = record.parent().unwrap().join("journal.jsonl");
        fs::write(&path, b"").unwrap();
        let mut journal = McpJournal {
            path: path.clone(),
            server: "fixture".into(),
            session: "one".into(),
        };
        replay_messages_with(Cursor::new(b""), Vec::new(), &replay, Some(&mut journal)).unwrap();
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("\"kind\":\"unconsumed\"")
        );
        let record2 = record_dir("approximate-outstanding");
        let mut recorder = Recorder::new(&record2).unwrap();
        for payload in [
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.as_slice(),
        ] {
            recorder
                .record(
                    mcp_transcript::Direction::ClientToServer,
                    mcp_framing::Framing::JsonLines,
                    0,
                    payload,
                )
                .unwrap();
        }
        recorder.finish().unwrap();
        let replay2 = Replayer::load(&record2).unwrap();
        assert!(
            replay_messages_with(
                Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n"),
                Vec::new(),
                &replay2,
                Some(&mut journal)
            )
            .is_err()
        );
        fs::remove_dir_all(record.parent().unwrap()).unwrap();
        fs::remove_dir_all(record2.parent().unwrap()).unwrap();
    }

    #[test]
    fn duplicate_response_preserves_strict_behavior_but_approximation_rejects_it() {
        let record = record_dir("duplicate-mcp-response");
        let mut recorder = Recorder::new(&record).unwrap();
        recorder
            .record(
                mcp_transcript::Direction::ClientToServer,
                mcp_framing::Framing::JsonLines,
                0,
                br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            )
            .unwrap();
        for _ in 0..2 {
            recorder
                .record(
                    mcp_transcript::Direction::ServerToClient,
                    mcp_framing::Framing::JsonLines,
                    1,
                    br#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
                )
                .unwrap();
        }
        recorder.finish().unwrap();
        let replay = Replayer::load(&record).unwrap();
        let request = b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
        assert!(replay_messages(Cursor::new(request), Vec::new(), &replay).is_ok());
        let path = record.parent().unwrap().join("journal.jsonl");
        fs::write(&path, b"").unwrap();
        let mut journal = McpJournal {
            path,
            server: "fixture".into(),
            session: "one".into(),
        };
        assert!(
            replay_messages_with(
                Cursor::new(request),
                Vec::new(),
                &replay,
                Some(&mut journal)
            )
            .is_err()
        );
        fs::remove_dir_all(record.parent().unwrap()).unwrap();
    }

    #[test]
    fn approximate_journal_requires_settled_known_sessions() {
        let root = replay_recording("approximate-journal");
        let cassette = root.parent().unwrap().join("replay.jsonl");
        let bundle = replay_bundle_path(&cassette, "fixture").unwrap();
        fs::write(bundle, bundle_recording(&root).unwrap()).unwrap();
        let expected_sha256 = Replayer::load(&root).unwrap().transcript.events()[0]
            .message
            .sha256
            .clone();
        let path = root.parent().unwrap().join("journal.jsonl");
        fs::write(&path, b"").unwrap();
        let journal = McpJournal {
            path: path.clone(),
            server: "fixture".into(),
            session: "one".into(),
        };
        let servers = [crate::benchmark::McpServerConfig {
            id: "fixture".into(),
            program: "unused".into(),
            args: vec![],
        }];
        journal.write(McpIgnoredKind::Started).unwrap();
        assert!(settle_ignored_journal(&fs::read(&path).unwrap(), &cassette, &servers).is_err());
        journal
            .write(McpIgnoredKind::Request {
                sequence: 1,
                expected_sha256,
                actual_sha256: "b".repeat(64),
            })
            .unwrap();
        journal.write(McpIgnoredKind::Finished).unwrap();
        let ignored =
            settle_ignored_journal(&fs::read(&path).unwrap(), &cassette, &servers).unwrap();
        assert_eq!(ignored.len(), 1);
        assert!(settle_ignored_journal(b"invalid\n", &cassette, &servers).is_err());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn offline_replay_rejects_mismatched_extra_and_changed_payload_evidence() {
        let record = replay_recording("replay-reject");
        let replay = Replayer::load(&record).unwrap();
        let mut output = Vec::new();
        assert!(
            replay_messages(
                Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\"}\n"),
                &mut output,
                &replay,
            )
            .is_err()
        );
        assert!(replay_messages(
            Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"extra\"}\n"),
            Vec::new(),
            &replay,
        )
        .is_err());
        let payload = fs::read_dir(record.join("payloads"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::write(payload, b"{}\n").unwrap();
        assert!(
            replay_messages(
                Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n"),
                Vec::new(),
                &replay,
            )
            .is_err()
        );
        let _ = fs::remove_dir_all(record.parent().unwrap());
    }

    #[test]
    fn cassette_bundle_round_trips_private_recording_without_an_upstream() {
        let record = replay_recording("bundle-round-trip");
        let bundle_path = record.parent().unwrap().join("recording.mcp.jsonl");
        write_bundle(&record, &bundle_path).unwrap();
        let bundle = read_bundle(&bundle_path).unwrap();
        assert!(bundle.starts_with(br#"{"kind":"header","schema_version":1}"#));
        assert!(bundle.len() < MAX_BUNDLE_BYTES);

        let destination = record.parent().unwrap().join("staged");
        materialize_bundle(&bundle, &destination).unwrap();
        validate_recording(&destination).unwrap();
        let replay = Replayer::load(&destination).unwrap();
        let mut output = Vec::new();
        replay_messages(
            Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"tools/list\"}\n"),
            &mut output,
            &replay,
        )
        .unwrap();
        let mut output = BufReader::new(&output[..]);
        assert_eq!(
            mcp_framing::read(&mut output, SERVICE)
                .unwrap()
                .unwrap()
                .value["method"],
            "notifications/progress"
        );
        assert_eq!(
            mcp_framing::read(&mut output, SERVICE)
                .unwrap()
                .unwrap()
                .value["id"],
            42
        );
        let _ = fs::remove_dir_all(record.parent().unwrap());
    }

    #[test]
    fn cassette_bundle_rejects_tampering_and_existing_destinations() {
        let record = replay_recording("bundle-reject");
        let bundle = bundle_recording(&record).unwrap();
        let mut tampered = bundle.clone();
        let position = tampered
            .windows(b"tools/list".len())
            .position(|window| window == b"tools/list")
            .unwrap();
        tampered[position] = b'x';
        let destination = record.parent().unwrap().join("tampered");
        assert!(materialize_bundle(&tampered, &destination).is_err());
        assert!(!destination.exists());

        let destination = record.parent().unwrap().join("existing");
        fs::create_dir(&destination).unwrap();
        assert!(materialize_bundle(&bundle, &destination).is_err());
        let _ = fs::remove_dir_all(record.parent().unwrap());
    }
}
