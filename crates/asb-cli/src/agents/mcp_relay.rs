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
        Options::Replay { directory } => run_replay(&directory),
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

fn run_replay(directory: &Path) -> Result<(), Error> {
    validate_recording(directory)?;
    let replay = Replayer::load(directory)?;
    replay_messages(std::io::stdin(), std::io::stdout().lock(), &replay)
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
    let stage = reserve_file(parent, name, "mcp-bundle")?;
    let result = (|| {
        write_reserved(&stage, &bytes)?;
        fs::rename(&stage, output)
            .map_err(|_| Error::Run("cannot publish MCP relay bundle without overwrite".into()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&stage);
    }
    result
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
    let result = (|| {
        fs::create_dir(stage.join("payloads")).map_err(crate::io_error)?;
        let index = mcp_transcript::canonicalize(&bundle.transcript.encode()?)?;
        write_new(&stage.join("index.jsonl"), &index)?;
        for (digest, payload) in &bundle.payloads {
            write_new(
                &stage.join("payloads").join(format!("{digest}.json")),
                payload,
            )?;
        }
        fs::rename(&stage, destination).map_err(|_| {
            Error::Run("cannot publish complete MCP replay recording without overwrite".into())
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
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

fn reserve_stage(parent: &Path, name: &str) -> Result<PathBuf, Error> {
    for attempt in 0..128_u16 {
        let stage = parent.join(format!(
            ".{name}.mcp-replay-{0}-{attempt}",
            std::process::id()
        ));
        match fs::create_dir(&stage) {
            Ok(()) => return Ok(stage),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(crate::io_error(error)),
        }
    }
    Err(Error::Run(
        "cannot reserve MCP replay recording destination".into(),
    ))
}

fn reserve_file(parent: &Path, name: &str, purpose: &str) -> Result<PathBuf, Error> {
    for attempt in 0..128_u16 {
        let stage = parent.join(format!(
            ".{name}.{purpose}-{0}-{attempt}",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&stage) {
            Ok(file) => {
                drop(file);
                return Ok(stage);
            }
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

fn write_reserved(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(crate::io_error)?;
    file.write_all(bytes).map_err(crate::io_error)?;
    file.sync_all().map_err(crate::io_error)
}

fn replay_messages<R: Read, W: Write>(
    reader: R,
    mut writer: W,
    replay: &Replayer,
) -> Result<(), Error> {
    let mut reader = BufReader::new(reader);
    let mut ids = BTreeMap::<String, serde_json::Value>::new();
    for event in replay.transcript.events() {
        let recorded = replay.payload(&event.message)?;
        match event.direction {
            mcp_transcript::Direction::ClientToServer => {
                let actual = mcp_framing::read(&mut reader, SERVICE)?.ok_or_else(|| {
                    Error::Run("MCP replay ended before its recorded client message".into())
                })?;
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
                        if without_id(&expected) != without_id(&actual.value) {
                            return Err(Error::Run(
                                "MCP replay request differs from recording".into(),
                            ));
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
                        ids.insert(recorded_id.sha256.clone(), actual_id);
                    }
                    mcp_transcript::MessageKind::Notification => {
                        if expected != actual.value {
                            return Err(Error::Run(
                                "MCP replay notification differs from recording".into(),
                            ));
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
                        let mut value: serde_json::Value = serde_json::from_slice(&recorded)
                            .map_err(|_| Error::Run("MCP replay payload is not JSON".into()))?;
                        value["id"] = actual_id.clone();
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
    let mut client_closed = false;
    let mut server_closed = false;
    let mut server_write = Some(server_write);
    let started = Instant::now();
    while !client_closed || !server_closed {
        match receive
            .recv()
            .map_err(|_| Error::Run("MCP relay stream unexpectedly closed".into()))?
        {
            Incoming::Message { direction, message } => {
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
    transcript: mcp_transcript::Transcript,
    payload_bytes: u64,
    published: bool,
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
            match fs::create_dir(&stage) {
                Ok(()) => {
                    fs::create_dir(stage.join("payloads")).map_err(crate::io_error)?;
                    return Ok(Self {
                        target: target.to_path_buf(),
                        stage,
                        transcript: mcp_transcript::Transcript::default(),
                        payload_bytes: 0,
                        published: false,
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
        self.transcript
            .record(direction, framing, relative_millis, payload)?;
        let reference = mcp_transcript::payload_ref(payload);
        let path = self
            .stage
            .join("payloads")
            .join(format!("{}.json", reference.sha256));
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
                    .open(path)
                    .map_err(crate::io_error)?;
                file.write_all(payload).map_err(crate::io_error)?;
                file.sync_all().map_err(crate::io_error)?;
            }
            Err(error) => return Err(crate::io_error(error)),
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), Error> {
        let bytes = mcp_transcript::canonicalize(&self.transcript.encode()?)?;
        let index = self.stage.join("index.jsonl");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(index)
            .map_err(crate::io_error)?;
        file.write_all(&bytes).map_err(crate::io_error)?;
        file.sync_all().map_err(crate::io_error)?;
        fs::rename(&self.stage, &self.target).map_err(|_| {
            Error::Run("cannot publish complete MCP relay recording without overwrite".into())
        })?;
        self.published = true;
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.stage);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;

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
