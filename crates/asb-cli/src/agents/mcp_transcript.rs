//! Opaque, bounded protocol evidence for a transparent MCP relay.
//!
//! This contract intentionally records neither MCP payloads nor JSON-RPC IDs.
//! It binds each observed message to a private payload by digest and byte
//! count, so later relay stages can stage those payloads without making prompts,
//! tool arguments, paths, or server output part of the durable index.

use super::mcp_framing;
use crate::Error;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) const SCHEMA_VERSION: u32 = 1;
const MAX_EVENTS: usize = 65_536;
const MAX_INDEX_BYTES: usize = MAX_EVENTS * 512;
const MAX_RELATIVE_MILLIS: u64 = 24 * 60 * 60 * 1_000;
const SHA256_HEX_BYTES: usize = 64;

/// A bounded private JSON payload reference. Its bytes are owned separately by
/// the recorder; the transcript index carries only this commitment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Blob {
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
    ClientToServer,
    ServerToClient,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MessageKind {
    Request,
    Notification,
    Response,
    Error,
}

/// One ordered MCP JSON-RPC message. `id` is an opaque digest of the original
/// JSON-RPC ID, not that ID itself. It permits a relay to correlate concurrent,
/// out-of-order responses without retaining user-controlled identifier text.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub schema_version: u32,
    pub sequence: u64,
    pub direction: Direction,
    pub relative_millis: u64,
    pub kind: MessageKind,
    pub message: Blob,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Blob>,
}

/// Canonical JSONL index for one MCP relay session.
#[derive(Default)]
pub(crate) struct Transcript {
    events: Vec<Event>,
}

impl Transcript {
    #[cfg(test)]
    pub(crate) fn events(&self) -> &[Event] {
        &self.events
    }

    /// Record one validated JSON-RPC message. The caller owns the corresponding
    /// private bytes; this method only emits their opaque commitment.
    pub(crate) fn record(
        &mut self,
        direction: Direction,
        relative_millis: u64,
        payload: &[u8],
    ) -> Result<(), Error> {
        if payload.len() > mcp_framing::MAX_MESSAGE_BYTES {
            return Err(Error::Run("MCP transcript message exceeds bound".into()));
        }
        let value: Value = serde_json::from_slice(payload)
            .map_err(|_| Error::Run("MCP transcript message is not JSON".into()))?;
        let (kind, id) = classify(&value)?;
        self.append(Event {
            schema_version: SCHEMA_VERSION,
            sequence: self.events.len() as u64 + 1,
            direction,
            relative_millis,
            kind,
            message: payload_ref(payload),
            id,
        })
    }

    pub(crate) fn append(&mut self, event: Event) -> Result<(), Error> {
        if !valid_event(&event, self.events.len()) {
            return Err(Error::Run("invalid MCP transcript event".into()));
        }
        self.events.push(event);
        Ok(())
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut output = Vec::new();
        for event in &self.events {
            let line = serde_json::to_vec(event)
                .map_err(|_| Error::Io("cannot encode MCP transcript index".into()))?;
            if line.len() > 512 {
                return Err(Error::Run("MCP transcript event exceeds bound".into()));
            }
            output.extend(line);
            output.push(b'\n');
        }
        Ok(output)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_INDEX_BYTES {
            return Err(Error::Run("MCP transcript exceeds bound".into()));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error::Run("MCP transcript is not UTF-8 JSONL".into()))?;
        let mut transcript = Self::default();
        for line in text.lines() {
            transcript.append(
                serde_json::from_str(line)
                    .map_err(|_| Error::Run("MCP transcript event is malformed".into()))?,
            )?;
        }
        Ok(transcript)
    }
}

/// Validate and re-encode private evidence before it enters a cassette.
pub(crate) fn canonicalize(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    Transcript::decode(bytes)?.encode()
}

fn classify(value: &Value) -> Result<(MessageKind, Option<Blob>), Error> {
    let object = value
        .as_object()
        .filter(|object| object.get("jsonrpc") == Some(&Value::String("2.0".into())))
        .ok_or_else(|| Error::Run("MCP transcript message is not JSON-RPC 2.0".into()))?;
    let id = match object.get("id") {
        None | Some(Value::Null) => None,
        Some(value) if value.is_string() || value.is_number() => Some(json_blob(value)),
        Some(_) => {
            return Err(Error::Run(
                "MCP transcript JSON-RPC ID is not a string or number".into(),
            ));
        }
    };
    match (
        object.get("method").and_then(Value::as_str),
        object.get("result"),
        object.get("error"),
    ) {
        (Some(method), None, None) if !method.is_empty() => Ok((
            if id.is_some() {
                MessageKind::Request
            } else {
                MessageKind::Notification
            },
            id,
        )),
        (None, Some(_), None) if id.is_some() => Ok((MessageKind::Response, id)),
        (None, None, Some(_)) if id.is_some() => Ok((MessageKind::Error, id)),
        _ => Err(Error::Run(
            "MCP transcript JSON-RPC message is unsupported".into(),
        )),
    }
}

pub(crate) fn payload_ref(bytes: &[u8]) -> Blob {
    Blob {
        sha256: format!("{:x}", Sha256::digest(bytes)),
        bytes: bytes.len() as u64,
    }
}

fn json_blob(value: &Value) -> Blob {
    // JSON-RPC permits string and number IDs. Canonical JSON also prevents a
    // formatting difference in a numeric ID from appearing as a different
    // response correlation in the durable index.
    payload_ref(&serde_json::to_vec(value).expect("JSON value encodes"))
}

fn valid_blob(blob: &Blob) -> bool {
    blob.sha256.len() == SHA256_HEX_BYTES
        && blob.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_event(event: &Event, prior_events: usize) -> bool {
    prior_events < MAX_EVENTS
        && event.schema_version == SCHEMA_VERSION
        && event.sequence == prior_events as u64 + 1
        && event.relative_millis <= MAX_RELATIVE_MILLIS
        && valid_blob(&event.message)
        && event.id.as_ref().is_none_or(valid_blob)
        && match event.kind {
            MessageKind::Notification => event.id.is_none(),
            MessageKind::Request | MessageKind::Response | MessageKind::Error => event.id.is_some(),
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(text: &str) -> Vec<u8> {
        text.as_bytes().to_vec()
    }

    #[test]
    fn canonical_index_retains_only_opaque_payload_references() {
        let mut transcript = Transcript::default();
        transcript
            .record(
                Direction::ClientToServer,
                1,
                &message(r#"{"jsonrpc":"2.0","id":"request-a","method":"tools/call","params":{"secret":"hidden"}}"#),
            )
            .unwrap();
        let encoded = canonicalize(&transcript.encode().unwrap()).unwrap();
        let rendered = String::from_utf8(encoded.clone()).unwrap();
        assert!(!rendered.contains("request-a"));
        assert!(!rendered.contains("hidden"));
        assert_eq!(
            Transcript::decode(&encoded).unwrap().encode().unwrap(),
            encoded
        );
    }

    #[test]
    fn records_notifications_and_out_of_order_response_identities() {
        let mut transcript = Transcript::default();
        transcript
            .record(
                Direction::ClientToServer,
                0,
                &message(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#),
            )
            .unwrap();
        transcript
            .record(
                Direction::ClientToServer,
                1,
                &message(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call"}"#),
            )
            .unwrap();
        transcript
            .record(
                Direction::ServerToClient,
                2,
                &message(r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}"#),
            )
            .unwrap();
        transcript
            .record(
                Direction::ServerToClient,
                3,
                &message(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#),
            )
            .unwrap();
        transcript
            .record(
                Direction::ServerToClient,
                4,
                &message(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1}}"#),
            )
            .unwrap();
        let events = transcript.events();
        assert_eq!(events[2].kind, MessageKind::Notification);
        assert_ne!(events[3].id, events[4].id);
        assert_eq!(events.len(), 5);
    }

    #[test]
    fn rejects_non_jsonrpc_oversized_and_noncausal_indexes() {
        let mut transcript = Transcript::default();
        assert!(
            transcript
                .record(Direction::ClientToServer, 0, br#"{"method":"tools/call"}"#)
                .is_err()
        );
        assert!(
            transcript
                .record(
                    Direction::ClientToServer,
                    0,
                    &vec![b'x'; mcp_framing::MAX_MESSAGE_BYTES + 1],
                )
                .is_err()
        );
        assert!(
            transcript
                .record(
                    Direction::ClientToServer,
                    0,
                    br#"{"jsonrpc":"2.0","id":{},"method":"tools/call"}"#,
                )
                .is_err()
        );
        let malformed = b"{\"schema_version\":1,\"sequence\":2,\"direction\":\"client_to_server\",\"relative_millis\":0,\"kind\":\"notification\",\"message\":{\"sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"bytes\":2}}\n";
        assert!(Transcript::decode(malformed).is_err());
    }
}
