//! Compact, transport-neutral evidence for native agent interactions.
//!
//! The native recorder deliberately stores no command lines, paths, output, or
//! host values in its index.  Those values can contain prompts, credentials, or
//! private paths and belong in bounded, content-addressed private blobs owned
//! by a later capture backend.  This module instead defines the stable index
//! that binds a causal operation to those blobs.  Keeping the index small lets
//! ASB measure an agent without retaining an unbounded second copy of its
//! execution streams.
#![allow(unsafe_code)] // sealed descriptor inheritance requires Unix `fcntl`/`pre_exec`.

use crate::Error;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};

pub(crate) const SCHEMA_VERSION: u32 = 1;
const MAX_EVENTS: usize = 65_536;
const MAX_BLOBS_PER_EVENT: usize = 8;
const SHA256_HEX_BYTES: usize = 64;
/// Trace-local identity for the directly launched agent process.
///
/// Kernel PIDs are host metadata and are deliberately never serialized into a
/// recording.  A capture backend retains any PID-to-trace-ID mapping only in
/// its short-lived private state; published native-trace events use these
/// opaque, per-trace identifiers instead.
pub(crate) const ROOT_PROCESS: u64 = 1;

/// Inherited only by the private ASB process gate.  The gate immediately marks
/// it close-on-exec, so the benchmark client cannot retain the trace writer.
#[cfg(unix)]
const ENV_FD: &str = "ASB_NATIVE_TRACE_FD";
#[cfg(unix)]
const ENV_EVENTS: &str = "ASB_NATIVE_TRACE_EVENTS";

/// A content-addressed private payload.  The trace index only carries the
/// digest and exact byte count; the payload is staged separately by a capture
/// backend and is never rendered in ASB results.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Blob {
    pub sha256: String,
    pub bytes: u64,
}

/// An externally observable operation class.  The event payload remains in
/// private blobs so this type is safe to carry through cassette metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Operation {
    Process,
    ProcessStart,
    ProcessFork,
    ProcessExec,
    ProcessExit,
    Filesystem,
    Network,
    Host,
}

/// Write the native root lifecycle captured by ASB's existing process gate.
/// This deliberately records only two events, so it adds neither a sampling
/// loop nor per-syscall work to the measured agent.  A later Linux backend can
/// append descendant fork/exec/exit events to the same trace contract.
pub(crate) fn write_root_lifecycle(run_dir: &std::path::Path) -> Result<(), Error> {
    let path = run_dir.join("native-trace/native-trace.jsonl");
    // Linux native execution writes directly through the gate's sealed file
    // descriptor.  Keep this fallback for non-gated routes while retaining a
    // pre-existing host/determinism event from an adapter extension.
    let mut transcript = match std::fs::read(&path) {
        Ok(bytes) => Transcript::decode(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Transcript::default(),
        Err(error) => {
            return Err(Error::Run(format!(
                "cannot read native process trace fallback: {error}"
            )));
        }
    };
    transcript.record(Event {
        schema_version: SCHEMA_VERSION,
        sequence: transcript.len() as u64 + 1,
        process: ROOT_PROCESS,
        parent_process: None,
        operation: Operation::ProcessStart,
        request: None,
        response: None,
        effects: Vec::new(),
    })?;
    transcript.record(Event {
        schema_version: SCHEMA_VERSION,
        sequence: transcript.len() as u64 + 1,
        process: ROOT_PROCESS,
        parent_process: None,
        operation: Operation::ProcessExit,
        request: None,
        response: None,
        effects: Vec::new(),
    })?;
    std::fs::write(path, transcript.encode()?)
        .map_err(|error| Error::Run(format!("cannot write native process trace: {error}")))
}

/// Create the sealed output channel before the gate is spawned.  The parent
/// retains the file until spawn has completed; only the gate receives a usable
/// inherited descriptor, and it restores close-on-exec before starting the
/// benchmark client.
#[cfg(unix)]
pub(crate) fn prepare_gate_channel(
    command: &mut std::process::Command,
    run_dir: &std::path::Path,
) -> Result<File, Error> {
    use std::os::unix::process::CommandExt;

    let path = run_dir.join("native-trace/native-trace.jsonl");
    let (file, prior_events) = match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            let bytes = std::fs::read(&path).map_err(|error| {
                Error::Run(format!("cannot read native process trace: {error}"))
            })?;
            let transcript = Transcript::decode(&bytes)?;
            let file = OpenOptions::new()
                .append(true)
                .open(&path)
                .map_err(|error| {
                    Error::Run(format!("cannot append native process trace: {error}"))
                })?;
            (file, transcript.len())
        }
        Ok(_) => {
            return Err(Error::Run(
                "native process trace is not a regular fresh artifact".into(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|error| {
                    Error::Run(format!("cannot create native process trace: {error}"))
                })?;
            (file, 0)
        }
        Err(error) => {
            return Err(Error::Run(format!(
                "cannot inspect native process trace: {error}"
            )));
        }
    };
    let descriptor = file.as_raw_fd();
    command.env(ENV_FD, descriptor.to_string());
    command.env(ENV_EVENTS, prior_events.to_string());
    // `File` starts close-on-exec.  Clear that flag only in the gate child;
    // the gate restores it as its first trace operation, before target spawn.
    unsafe {
        command.pre_exec(move || {
            let flags = libc::fcntl(descriptor, libc::F_GETFD);
            if flags < 0 || libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(file)
}

/// A bounded, append-only writer owned by the exclusive process gate.
///
/// Its only state is the accepted-event count.  It neither observes syscalls
/// nor buffers benchmark output; a later event backend may call `record` for
/// fork/exec/exit notifications using the same compact JSONL contract.
#[cfg(unix)]
pub(crate) struct GateWriter {
    file: File,
    events: usize,
}

#[cfg(unix)]
impl GateWriter {
    /// Take the descriptor passed by `prepare_gate_channel` and make it
    /// close-on-exec before the client command is constructed.
    pub(crate) fn from_environment() -> Result<Option<Self>, Error> {
        let Some(value) = std::env::var_os(ENV_FD) else {
            return Ok(None);
        };
        // The exclusive gate is still single-threaded; remove the inherited
        // control value before spawning the benchmark client.
        unsafe {
            std::env::remove_var(ENV_FD);
        };
        let descriptor = value
            .to_str()
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|descriptor| *descriptor >= 3)
            .ok_or_else(|| Error::Run("invalid native process trace descriptor".into()))?;
        let events = std::env::var_os(ENV_EVENTS)
            .and_then(|value| value.to_str().and_then(|value| value.parse::<usize>().ok()))
            .filter(|events| *events <= MAX_EVENTS)
            .ok_or_else(|| Error::Run("invalid native process trace event count".into()))?;
        // See the single-threaded environment note above for ENV_FD.
        unsafe { std::env::remove_var(ENV_EVENTS) };
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        if flags < 0
            || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
        {
            return Err(Error::Run(format!(
                "cannot seal native process trace descriptor: {}",
                std::io::Error::last_os_error()
            )));
        }
        // SAFETY: this descriptor was created by `prepare_gate_channel`, is
        // transferred exclusively to the gate, and is consumed exactly once.
        let file = unsafe { File::from_raw_fd(descriptor) };
        Ok(Some(Self { file, events }))
    }

    /// Append one opaque process-tree event.  Host PID mapping stays entirely
    /// inside the gate; only trace-local IDs are allowed into the sidecar.
    pub(crate) fn record_event(
        &mut self,
        process: u64,
        operation: Operation,
        parent_process: Option<u64>,
    ) -> Result<(), Error> {
        self.record_event_with_request(process, operation, parent_process, None)
    }

    /// Append an opaque operation whose request has a bounded, private content
    /// reference.  The reference identifies content without putting an
    /// executable path, command line, prompt, credential, or payload in the
    /// durable trace index.
    pub(crate) fn record_event_with_request(
        &mut self,
        process: u64,
        operation: Operation,
        parent_process: Option<u64>,
        request: Option<Blob>,
    ) -> Result<(), Error> {
        let sequence = self.events as u64 + 1;
        let event = Event {
            schema_version: SCHEMA_VERSION,
            sequence,
            process,
            operation,
            parent_process,
            request,
            response: None,
            effects: Vec::new(),
        };
        if !valid_event(&event, self.events) {
            return Err(Error::Run("invalid native trace event".into()));
        }
        let bytes = serde_json::to_vec(&event)
            .map_err(|_| Error::Io("cannot encode native trace index".into()))?;
        self.file
            .write_all(&bytes)
            .and_then(|()| self.file.write_all(b"\n"))
            .and_then(|()| self.file.flush())
            .map_err(|error| Error::Run(format!("cannot write native process trace: {error}")))?;
        self.events += 1;
        Ok(())
    }
}

/// One causal native operation.  `request` and `response` are optional because
/// failed operations may have no response payload, while filesystem effects
/// can be represented as additional bounded blobs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub schema_version: u32,
    pub sequence: u64,
    /// Opaque, trace-local process identity.  This is never a host PID.
    pub process: u64,
    pub operation: Operation,
    /// Parent trace-local identity for a fork event, never a host PID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_process: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<Blob>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Blob>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Blob>,
}

/// Canonical JSONL trace index.
#[derive(Default)]
pub(crate) struct Transcript {
    events: Vec<Event>,
}

impl Transcript {
    #[cfg(test)]
    pub(crate) fn events(&self) -> &[Event] {
        &self.events
    }

    pub(crate) fn len(&self) -> usize {
        self.events.len()
    }

    pub(crate) fn record(&mut self, event: Event) -> Result<(), Error> {
        if !valid_event(&event, self.events.len()) {
            return Err(Error::Run("invalid native trace event".into()));
        }
        self.events.push(event);
        Ok(())
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut output = Vec::new();
        for event in &self.events {
            let line = serde_json::to_vec(event)
                .map_err(|_| Error::Io("cannot encode native trace index".into()))?;
            output.extend(line);
            output.push(b'\n');
        }
        Ok(output)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        // The index contains only fixed-size metadata and a bounded number of
        // references.  This cap prevents a malformed cassette from consuming
        // disk-sized memory before event validation can reject it.
        const MAX_INDEX_BYTES: usize = MAX_EVENTS * 2_048;
        if bytes.len() > MAX_INDEX_BYTES {
            return Err(Error::Run("native trace index exceeds bound".into()));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error::Run("native trace index is not UTF-8 JSONL".into()))?;
        let mut transcript = Self::default();
        for line in text.lines() {
            let position = transcript.len() + 1;
            transcript.record(serde_json::from_str(line).map_err(|error| {
                Error::Run(format!(
                    "native trace index event {position} is malformed ({error})"
                ))
            })?)?;
        }
        Ok(transcript)
    }
}

fn valid_event(event: &Event, prior_events: usize) -> bool {
    prior_events < MAX_EVENTS
        && event.schema_version == SCHEMA_VERSION
        && event.sequence == prior_events as u64 + 1
        && event.process > 0
        && match event.operation {
            Operation::ProcessFork => event
                .parent_process
                .is_some_and(|parent| parent > 0 && parent != event.process),
            _ => event.parent_process.is_none(),
        }
        && event.effects.len() <= MAX_BLOBS_PER_EVENT
        && event
            .request
            .iter()
            .chain(event.response.iter())
            .chain(event.effects.iter())
            .all(valid_blob)
}

/// Reject malformed trace data and return canonical JSONL before publication.
pub(crate) fn canonicalize(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    Transcript::decode(bytes)?.encode()
}

fn valid_blob(blob: &Blob) -> bool {
    blob.sha256.len() == SHA256_HEX_BYTES
        && blob.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(byte: char) -> Blob {
        Blob {
            sha256: std::iter::repeat_n(byte, SHA256_HEX_BYTES).collect(),
            bytes: 7,
        }
    }

    fn event(sequence: u64, operation: Operation) -> Event {
        Event {
            schema_version: SCHEMA_VERSION,
            sequence,
            process: 1,
            operation,
            parent_process: None,
            request: Some(blob('a')),
            response: Some(blob('b')),
            effects: vec![blob('c')],
        }
    }

    #[test]
    fn canonical_trace_preserves_private_payload_references() {
        let mut transcript = Transcript::default();
        transcript.record(event(1, Operation::Process)).unwrap();
        transcript.record(event(2, Operation::Filesystem)).unwrap();
        let bytes = canonicalize(&transcript.encode().unwrap()).unwrap();
        let encoded = Transcript::decode(&bytes).unwrap().encode().unwrap();
        assert_eq!(encoded, bytes);
    }

    #[test]
    fn rejects_private_payloads_and_non_causal_events() {
        let mut transcript = Transcript::default();
        let mut malformed = event(1, Operation::Network);
        malformed.request.as_mut().unwrap().sha256 = "/private/path".into();
        assert!(transcript.record(malformed).is_err());
        assert!(transcript.record(event(2, Operation::Network)).is_err());
        transcript.record(event(1, Operation::Network)).unwrap();
    }

    #[test]
    fn fork_events_bind_distinct_opaque_parent_and_child_ids() {
        let mut transcript = Transcript::default();
        let mut fork = event(1, Operation::ProcessFork);
        fork.process = 2;
        fork.parent_process = Some(ROOT_PROCESS);
        transcript.record(fork).unwrap();

        let mut missing_parent = event(2, Operation::ProcessFork);
        missing_parent.process = 3;
        assert!(transcript.record(missing_parent).is_err());

        let mut self_parent = event(2, Operation::ProcessFork);
        self_parent.process = 3;
        self_parent.parent_process = Some(3);
        assert!(transcript.record(self_parent).is_err());

        let mut non_fork_parent = event(2, Operation::ProcessExec);
        non_fork_parent.process = 2;
        non_fork_parent.parent_process = Some(ROOT_PROCESS);
        assert!(transcript.record(non_fork_parent).is_err());
    }

    #[test]
    fn rejects_unknown_fields_and_unbounded_index() {
        assert!(canonicalize(br#"{"schema_version":1,"sequence":1,"process":1,"operation":"process","unknown":true}
"#).is_err());
        assert!(Transcript::decode(&vec![b'x'; MAX_EVENTS * 2_048 + 1]).is_err());
    }

    #[test]
    fn root_lifecycle_is_two_bounded_process_events() {
        let root = std::env::temp_dir().join(format!("asb-native-trace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("native-trace")).unwrap();
        write_root_lifecycle(&root).unwrap();
        let trace = Transcript::decode(
            &std::fs::read(root.join("native-trace/native-trace.jsonl")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            std::str::from_utf8(&trace.encode().unwrap())
                .unwrap()
                .lines()
                .count(),
            2
        );
        assert!(
            trace
                .events
                .iter()
                .all(|event| event.process == ROOT_PROCESS)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
