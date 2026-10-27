//! Compact, transport-neutral evidence for native agent interactions.
//!
//! The native recorder deliberately stores no command lines, paths, output, or
//! host values in its index.  Those values can contain prompts, credentials, or
//! private paths and belong in bounded, content-addressed private blobs owned
//! by a later capture backend.  This module instead defines the stable index
//! that binds a causal operation to those blobs.  Keeping the index small lets
//! ASB measure an agent without retaining an unbounded second copy of its
//! execution streams.

use crate::Error;
use serde::{Deserialize, Serialize};

pub(crate) const SCHEMA_VERSION: u32 = 1;
const MAX_EVENTS: usize = 65_536;
const MAX_BLOBS_PER_EVENT: usize = 8;
const SHA256_HEX_BYTES: usize = 64;

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
    ProcessExit,
    Filesystem,
    Network,
    Host,
}

/// Write the native root lifecycle captured by ASB's existing process gate.
/// This deliberately records only two events, so it adds neither a sampling
/// loop nor per-syscall work to the measured agent.  A later Linux backend can
/// append descendant fork/exec/exit events to the same trace contract.
pub(crate) fn write_root_lifecycle(run_dir: &std::path::Path, process: u64) -> Result<(), Error> {
    let mut transcript = Transcript::default();
    transcript.record(Event {
        schema_version: SCHEMA_VERSION,
        sequence: 1,
        process,
        operation: Operation::ProcessStart,
        request: None,
        response: None,
        effects: Vec::new(),
    })?;
    transcript.record(Event {
        schema_version: SCHEMA_VERSION,
        sequence: 2,
        process,
        operation: Operation::ProcessExit,
        request: None,
        response: None,
        effects: Vec::new(),
    })?;
    std::fs::write(
        run_dir.join("native-trace/native-trace.jsonl"),
        transcript.encode()?,
    )
    .map_err(|error| Error::Run(format!("cannot write native process trace: {error}")))
}

/// One causal native operation.  `request` and `response` are optional because
/// failed operations may have no response payload, while filesystem effects
/// can be represented as additional bounded blobs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub schema_version: u32,
    pub sequence: u64,
    pub process: u64,
    pub operation: Operation,
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
    pub(crate) fn record(&mut self, event: Event) -> Result<(), Error> {
        if self.events.len() >= MAX_EVENTS
            || event.schema_version != SCHEMA_VERSION
            || event.sequence != self.events.len() as u64 + 1
            || event.process == 0
            || event.effects.len() > MAX_BLOBS_PER_EVENT
            || !event
                .request
                .iter()
                .chain(event.response.iter())
                .chain(event.effects.iter())
                .all(valid_blob)
        {
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
            transcript.record(
                serde_json::from_str(line)
                    .map_err(|_| Error::Run("native trace index is malformed".into()))?,
            )?;
        }
        Ok(transcript)
    }
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
        write_root_lifecycle(&root, 42).unwrap();
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
        let _ = std::fs::remove_dir_all(root);
    }
}
