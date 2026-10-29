//! Private, fail-closed consumption of sealed native-tool sidecars.
//!
//! The sidecar is not evidence or report data.  This module retains only
//! bounded aliases, exit observations and digests after validation; it never
//! models command arguments, paths, prompts, request bodies or output bodies.
use super::{Archive, Error, NativeTrack, valid_digest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;

const MAX_EVENTS: u64 = 8_192;
const MAX_EVENT_BYTES: usize = 1024 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_PAYLOAD_BYTES: usize = 512 * 1024;
const MAX_TOTAL_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// One validated native session lifecycle, without captured tool payloads.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSession {
    alias: String,
    success: bool,
}

/// Serializable, payload-free launch authority for a staged private sidecar.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeToolLaunch {
    native: NativeTrack,
    launch_session: String,
    workspace_effect_sha256: String,
    sessions: Vec<NativeSession>,
}

impl NativeToolLaunch {
    /// Validate a staged owned sidecar without disclosing its pathname or bytes.
    pub fn validate_staged(&self, path: &Path) -> Result<(), Error> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options
            .open(path)
            .map_err(|_| Error("native tool sidecar is unavailable".into()))?;
        let metadata = file
            .metadata()
            .map_err(|_| Error("native tool sidecar is unavailable".into()))?;
        if !metadata.is_file() || metadata.len() != self.native.bytes {
            return Err(Error(
                "native tool sidecar differs from its launch descriptor".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(Error("native tool sidecar is not private".into()));
            }
        }
        let mut bytes = Vec::new();
        file.take(self.native.bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| Error("native tool sidecar cannot be read".into()))?;
        let transport = NativeToolTransport::from_sidecar(&self.native, &bytes)?;
        if transport.launch_session != self.launch_session
            || transport.workspace_effect_sha256 != self.workspace_effect_sha256
            || transport.sessions != self.sessions
        {
            return Err(Error(
                "native tool sidecar differs from its launch descriptor".into(),
            ));
        }
        Ok(())
    }
}

impl NativeSession {
    /// Stable opaque session alias.
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// Observed terminal status supplied by the sealed sidecar.
    pub fn success(&self) -> bool {
        self.success
    }
}

/// A fully validated native-tool replay sidecar.
///
/// Construction rejects any missing, extra, reordered, malformed or incomplete
/// lifecycle event before a future native transport can launch anything.
#[derive(Clone, Eq, PartialEq)]
pub struct NativeToolTransport {
    launch_session: String,
    workspace_effect_sha256: String,
    sessions: Vec<NativeSession>,
    exchanges: BTreeMap<String, Vec<NativeExchange>>,
    next_exchange: BTreeMap<String, usize>,
}

#[derive(Clone, Eq, PartialEq)]
struct NativeExchange {
    request: Vec<u8>,
    output: Vec<u8>,
}

impl NativeToolTransport {
    /// Validate and stage a selected sidecar as an owner-only private runtime file.
    pub fn stage_from_archive(
        archive: &mut Archive,
        track_id: u64,
        destination: &Path,
    ) -> Result<NativeToolLaunch, Error> {
        let native = archive
            .manifest()
            .track(track_id)?
            .native
            .clone()
            .ok_or_else(|| Error(format!("cassette track {track_id} has no native sidecar")))?;
        let bytes = archive.read_native(track_id)?;
        let transport = Self::from_sidecar(&native, &bytes)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(destination)
            .map_err(|_| Error("cannot create private native tool sidecar".into()))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| Error("cannot stage private native tool sidecar".into()))?;
        Ok(NativeToolLaunch {
            native,
            launch_session: transport.launch_session,
            workspace_effect_sha256: transport.workspace_effect_sha256,
            sessions: transport.sessions,
        })
    }
    /// Read and validate the sealed native sidecar selected from an archive.
    pub fn from_archive(archive: &mut Archive, track_id: u64) -> Result<Self, Error> {
        let track = archive.manifest().track(track_id)?;
        let native = track
            .native
            .as_ref()
            .ok_or_else(|| Error(format!("cassette track {track_id} has no native sidecar")))?;
        let native = native.clone();
        let bytes = archive.read_native(track_id)?;
        Self::from_sidecar(&native, &bytes)
    }

    /// Validate an already-selected private sidecar without retaining its bytes.
    pub fn from_sidecar(native: &NativeTrack, bytes: &[u8]) -> Result<Self, Error> {
        if !native.complete {
            return Err(Error("native tool sidecar is incomplete".into()));
        }
        if native.events == 0 || native.events > MAX_EVENTS {
            return Err(Error(
                "native tool sidecar event count is invalid or excessive".into(),
            ));
        }
        if bytes.len() as u64 != native.bytes {
            return Err(Error(
                "native tool sidecar length differs from sealed metadata".into(),
            ));
        }
        let actual_events = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
        if actual_events != native.events || !bytes.ends_with(b"\n") {
            return Err(Error(
                "native tool sidecar framing differs from sealed metadata".into(),
            ));
        }

        let mut launch = None;
        let mut sessions = BTreeMap::<String, SessionPhase>::new();
        let mut exchanges = BTreeMap::<String, Vec<NativeExchange>>::new();
        let mut workspace_effect = None;
        let mut retained_payload_bytes = 0usize;
        for (index, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
            let line = line
                .strip_suffix(b"\n")
                .ok_or_else(|| Error("native tool sidecar has an unterminated event".into()))?;
            if line.is_empty() || line.len() > MAX_EVENT_BYTES {
                return Err(Error(
                    "native tool sidecar event is empty or oversized".into(),
                ));
            }
            let event: Event = serde_json::from_slice(line)
                .map_err(|_| Error("native tool sidecar contains an invalid event".into()))?;
            match event {
                Event::Launch {
                    session,
                    program_sha256,
                } => {
                    if index != 0
                        || launch.is_some()
                        || !valid_identifier(&session)
                        || !valid_digest(&program_sha256)
                    {
                        return Err(Error(
                            "native tool launch event is invalid or reordered".into(),
                        ));
                    }
                    launch = Some(session);
                }
                Event::SessionAlias { alias, session } => {
                    if workspace_effect.is_some()
                        || launch.as_deref() != Some(session.as_str())
                        || !valid_identifier(&alias)
                        || sessions.contains_key(&alias)
                    {
                        return Err(Error(
                            "native tool session alias is invalid or reordered".into(),
                        ));
                    }
                    sessions.insert(alias, SessionPhase::Idle(false));
                }
                Event::Poll { alias } => {
                    require_idle(&sessions, &alias, "poll")?;
                    sessions.insert(alias, SessionPhase::Polled);
                }
                Event::Write {
                    alias,
                    request_sha256,
                    request_b64,
                } => {
                    let request = decode_payload(&request_b64)?;
                    if !valid_digest(&request_sha256) || super::digest(&request) != request_sha256 {
                        return Err(Error("native tool write digest is invalid".into()));
                    }
                    require_phase(&sessions, &alias, SessionPhase::Polled, "write")?;
                    retained_payload_bytes =
                        retain_payload_budget(retained_payload_bytes, request.len())?;
                    sessions.insert(alias, SessionPhase::Written { request });
                }
                Event::Output {
                    alias,
                    response_sha256,
                    output_b64,
                } => {
                    let output = decode_payload(&output_b64)?;
                    if !valid_digest(&response_sha256) || super::digest(&output) != response_sha256
                    {
                        return Err(Error("native tool output digest is invalid".into()));
                    }
                    let request = match sessions.remove(&alias) {
                        Some(SessionPhase::Written { request }) => request,
                        _ => {
                            return Err(Error(
                                "native tool output event is invalid or reordered".into(),
                            ));
                        }
                    };
                    retained_payload_bytes =
                        retain_payload_budget(retained_payload_bytes, output.len())?;
                    exchanges
                        .entry(alias.clone())
                        .or_default()
                        .push(NativeExchange { request, output });
                    sessions.insert(alias, SessionPhase::Idle(true));
                }
                Event::Exit { alias, success } => {
                    if !valid_identifier(&alias)
                        || sessions.get(&alias) != Some(&SessionPhase::Idle(true))
                    {
                        return Err(Error(
                            "native tool exit event is invalid or reordered".into(),
                        ));
                    }
                    sessions.insert(alias, SessionPhase::Exited(success));
                }
                Event::WorkspaceEffect { sha256 } => {
                    if workspace_effect.is_some()
                        || !valid_digest(&sha256)
                        || sessions.is_empty()
                        || sessions
                            .values()
                            .any(|state| !matches!(state, SessionPhase::Exited(_)))
                    {
                        return Err(Error(
                            "native tool workspace effect is invalid or reordered".into(),
                        ));
                    }
                    workspace_effect = Some(sha256);
                }
            }
        }
        let launch_session =
            launch.ok_or_else(|| Error("native tool sidecar has no launch event".into()))?;
        let workspace_effect_sha256 = workspace_effect
            .ok_or_else(|| Error("native tool sidecar has no terminal workspace effect".into()))?;
        let sessions = sessions
            .into_iter()
            .map(|(alias, phase)| match phase {
                SessionPhase::Exited(success) => Ok(NativeSession { alias, success }),
                _ => Err(Error(
                    "native tool sidecar has an unterminated session".into(),
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            launch_session,
            workspace_effect_sha256,
            sessions,
            exchanges,
            next_exchange: BTreeMap::new(),
        })
    }

    /// Opaque launch-session identity; not an executable path or command.
    pub fn launch_session(&self) -> &str {
        &self.launch_session
    }

    /// Digest of the terminal workspace-effect observation.
    pub fn workspace_effect_sha256(&self) -> &str {
        &self.workspace_effect_sha256
    }

    /// Validated completed sessions in stable alias order.
    pub fn sessions(&self) -> &[NativeSession] {
        &self.sessions
    }

    /// Return the exact private recorded output for the next matching request.
    ///
    /// Mismatch diagnostics intentionally contain neither recorded nor supplied
    /// bytes. A future launcher may relay this result directly to the agent.
    pub fn replay_output(&mut self, alias: &str, request: &[u8]) -> Result<Vec<u8>, Error> {
        if !valid_identifier(alias) {
            return Err(Error("native tool replay session is invalid".into()));
        }
        let next = *self.next_exchange.get(alias).unwrap_or(&0);
        let exchange = self
            .exchanges
            .get(alias)
            .and_then(|exchanges| exchanges.get(next))
            .ok_or_else(|| Error("native tool replay has no recorded request".into()))?;
        if exchange.request != request {
            return Err(Error(
                "native tool replay request differs from recording".into(),
            ));
        }
        self.next_exchange.insert(alias.to_owned(), next + 1);
        Ok(exchange.output.clone())
    }

    /// Reject a replay that did not consume every sealed tool interaction.
    pub fn require_complete_replay(&self) -> Result<(), Error> {
        if self.exchanges.iter().any(|(alias, exchanges)| {
            self.next_exchange.get(alias).copied().unwrap_or(0) != exchanges.len()
        }) {
            return Err(Error(
                "native tool replay left recorded interactions unused".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
enum Event {
    Launch {
        session: String,
        program_sha256: String,
    },
    SessionAlias {
        alias: String,
        session: String,
    },
    Poll {
        alias: String,
    },
    Write {
        alias: String,
        request_sha256: String,
        request_b64: String,
    },
    Output {
        alias: String,
        response_sha256: String,
        output_b64: String,
    },
    Exit {
        alias: String,
        success: bool,
    },
    WorkspaceEffect {
        sha256: String,
    },
}

#[derive(Debug, Eq, PartialEq)]
enum SessionPhase {
    Idle(bool),
    Polled,
    Written { request: Vec<u8> },
    Exited(bool),
}

fn require_phase(
    sessions: &BTreeMap<String, SessionPhase>,
    alias: &str,
    expected: SessionPhase,
    event: &str,
) -> Result<(), Error> {
    if !valid_identifier(alias) || sessions.get(alias) != Some(&expected) {
        return Err(Error(format!(
            "native tool {event} event is invalid or reordered"
        )));
    }
    Ok(())
}

fn retain_payload_budget(current: usize, next: usize) -> Result<usize, Error> {
    current
        .checked_add(next)
        .filter(|total| *total <= MAX_TOTAL_PAYLOAD_BYTES)
        .ok_or_else(|| Error("native tool sidecar payload budget is exceeded".into()))
}

fn decode_payload(value: &str) -> Result<Vec<u8>, Error> {
    if value.len() > MAX_PAYLOAD_BYTES.saturating_mul(4).div_ceil(3) + 4
        || !value.len().is_multiple_of(4)
    {
        return Err(Error(
            "native tool payload encoding is invalid or excessive".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(value.len() / 4 * 3);
    let chunks = value.as_bytes().chunks_exact(4);
    let chunk_count = chunks.len();
    for (index, chunk) in chunks.enumerate() {
        let (a, b, c, d) = (
            base64_value(chunk[0])?,
            base64_value(chunk[1])?,
            chunk[2],
            chunk[3],
        );
        let pad_c = c == b'=';
        let pad_d = d == b'=';
        if pad_c && !pad_d {
            return Err(Error("native tool payload encoding is invalid".into()));
        }
        let c = if pad_c { 0 } else { base64_value(c)? };
        let d = if pad_d { 0 } else { base64_value(d)? };
        bytes.push((a << 2) | (b >> 4));
        if !pad_c {
            bytes.push((b << 4) | (c >> 2));
        }
        if !pad_d {
            bytes.push((c << 6) | d);
        }
        if (pad_c || pad_d) && index + 1 != chunk_count {
            return Err(Error("native tool payload encoding is invalid".into()));
        }
    }
    if bytes.len() > MAX_PAYLOAD_BYTES {
        return Err(Error("native tool payload is excessive".into()));
    }
    Ok(bytes)
}

fn base64_value(byte: u8) -> Result<u8, Error> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(Error("native tool payload encoding is invalid".into())),
    }
}

fn require_idle(
    sessions: &BTreeMap<String, SessionPhase>,
    alias: &str,
    event: &str,
) -> Result<(), Error> {
    if !valid_identifier(alias) || !matches!(sessions.get(alias), Some(SessionPhase::Idle(_))) {
        return Err(Error(format!(
            "native tool {event} event is invalid or reordered"
        )));
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn sidecar(lines: &[String], complete: bool) -> (NativeTrack, Vec<u8>) {
        let bytes = format!("{}\n", lines.join("\n")).into_bytes();
        (
            NativeTrack {
                complete,
                events: lines.len() as u64,
                bytes: bytes.len() as u64,
                sha256: super::super::digest(&bytes),
            },
            bytes,
        )
    }

    fn exact_lines() -> Vec<String> {
        let request = b"read private workspace";
        let output = b"recorded private tool output";
        let request_sha256 = super::super::digest(request);
        let response_sha256 = super::super::digest(output);
        vec![
            format!(
                "{{\"event\":\"launch\",\"session\":\"launch-1\",\"program_sha256\":\"{DIGEST}\"}}"
            ),
            "{\"event\":\"session_alias\",\"alias\":\"shell-1\",\"session\":\"launch-1\"}".into(),
            "{\"event\":\"poll\",\"alias\":\"shell-1\"}".into(),
            format!(
                "{{\"event\":\"write\",\"alias\":\"shell-1\",\"request_sha256\":\"{request_sha256}\",\"request_b64\":\"{}\"}}",
                encode_base64(request)
            ),
            format!(
                "{{\"event\":\"output\",\"alias\":\"shell-1\",\"response_sha256\":\"{response_sha256}\",\"output_b64\":\"{}\"}}",
                encode_base64(output)
            ),
            "{\"event\":\"exit\",\"alias\":\"shell-1\",\"success\":true}".into(),
            format!("{{\"event\":\"workspace_effect\",\"sha256\":\"{DIGEST}\"}}"),
        ]
    }

    #[test]
    fn exact_native_lifecycle_is_private_and_deterministic() {
        let (native, bytes) = sidecar(&exact_lines(), true);
        let transport = NativeToolTransport::from_sidecar(&native, &bytes).unwrap();
        assert_eq!(transport.launch_session(), "launch-1");
        assert_eq!(transport.workspace_effect_sha256(), DIGEST);
        assert_eq!(
            transport.sessions(),
            &[NativeSession {
                alias: "shell-1".into(),
                success: true
            }]
        );
    }

    #[test]
    fn replay_returns_exact_private_output_without_exposing_payload_in_errors() {
        let (native, bytes) = sidecar(&exact_lines(), true);
        let mut transport = NativeToolTransport::from_sidecar(&native, &bytes).unwrap();
        let error = transport
            .replay_output("shell-1", b"different private request")
            .unwrap_err()
            .to_string();
        assert!(!error.contains("private"));
        assert!(!error.contains("recorded private tool output"));
        assert_eq!(
            transport
                .replay_output("shell-1", b"read private workspace")
                .unwrap(),
            b"recorded private tool output"
        );
        transport.require_complete_replay().unwrap();
    }

    #[test]
    fn missing_extra_reordered_mutated_and_incomplete_native_events_fail_closed() {
        let exact = exact_lines();
        let cases = [
            ("missing", exact[..exact.len() - 1].to_vec(), true),
            (
                "extra",
                [
                    exact.clone(),
                    vec![format!(
                        "{{\"event\":\"workspace_effect\",\"sha256\":\"{DIGEST}\"}}"
                    )],
                ]
                .concat(),
                true,
            ),
            (
                "reordered",
                vec![
                    exact[0].clone(),
                    exact[1].clone(),
                    exact[3].clone(),
                    exact[2].clone(),
                    exact[4].clone(),
                    exact[5].clone(),
                    exact[6].clone(),
                ],
                true,
            ),
            (
                "mutated",
                exact
                    .iter()
                    .enumerate()
                    .map(|(index, line)| {
                        if index == 4 {
                            line.replacen("output_b64", "unknown_payload", 1)
                        } else {
                            line.clone()
                        }
                    })
                    .collect(),
                true,
            ),
            ("incomplete", exact.clone(), false),
        ];
        for (name, lines, complete) in cases {
            let (native, bytes) = sidecar(&lines, complete);
            assert!(
                NativeToolTransport::from_sidecar(&native, &bytes).is_err(),
                "accepted {name} native sidecar"
            );
        }
    }

    #[test]
    fn sealed_metadata_must_match_the_private_sidecar() {
        let (mut native, bytes) = sidecar(&exact_lines(), true);
        native.events -= 1;
        assert!(NativeToolTransport::from_sidecar(&native, &bytes).is_err());
        native.events += 1;
        native.bytes -= 1;
        assert!(NativeToolTransport::from_sidecar(&native, &bytes).is_err());
    }

    fn encode_base64(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = String::new();
        for chunk in bytes.chunks(3) {
            let a = chunk[0];
            let b = *chunk.get(1).unwrap_or(&0);
            let c = *chunk.get(2).unwrap_or(&0);
            encoded.push(TABLE[(a >> 2) as usize] as char);
            encoded.push(TABLE[((a & 0x03) << 4 | b >> 4) as usize] as char);
            encoded.push(if chunk.len() > 1 {
                TABLE[((b & 0x0f) << 2 | c >> 6) as usize] as char
            } else {
                '='
            });
            encoded.push(if chunk.len() > 2 {
                TABLE[(c & 0x3f) as usize] as char
            } else {
                '='
            });
        }
        encoded
    }
}
