//! Record/replay entry point, backed by the configured proxy chain.

use crate::{Error, proxy};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Request normalization, ordered matching and full-consumption replay semantics.
/// Bump for incompatible tape matching changes, not scheduling/measurement changes.
pub(crate) const REPLAY_MATCHING_CONTRACT: &str = "asb-tape-request-matching-v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Mode {
    Record,
    Replay,
}

/// A running record/replay chain.
pub struct Session(proxy::Chain);

/// Attempt-owned independent proxy chains for adapter-declared HTTP channels.
#[derive(Default)]
pub(crate) struct AuxiliarySessions {
    sessions: std::collections::BTreeMap<String, (Mode, Session)>,
}

impl AuxiliarySessions {
    /// Start a separately owned tape using the adapter's selected upstream.
    #[allow(
        dead_code,
        reason = "adapter session planning is integrated separately"
    )]
    pub(crate) fn start(
        &mut self,
        name: &str,
        mode: Mode,
        cassette: &Path,
        upstream: Option<&str>,
        respect_record_timings: bool,
        context: AgentContext<'_>,
    ) -> Result<(), Error> {
        if name.is_empty()
            || name.len() > 55
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            || self.sessions.contains_key(name)
        {
            return Err(Error::Input(
                "invalid or duplicate auxiliary tape identity".into(),
            ));
        }
        let session =
            Session::start_for_agent_at(mode, cassette, upstream, respect_record_timings, context)?;
        self.sessions.insert(name.to_owned(), (mode, session));
        Ok(())
    }

    pub(crate) fn endpoints(&self) -> std::collections::BTreeMap<String, String> {
        self.sessions
            .iter()
            .map(|(name, (_, session))| (name.clone(), session.url().to_owned()))
            .collect()
    }

    /// Drain all sessions, even when one fails, and merge their private capture
    /// receipts into the attempt's existing proxy evidence.
    pub(crate) fn finish(self, evidence: &mut proxy::Evidence) -> Result<(), Error> {
        let mut failure = None;
        for (name, (mode, session)) in self.sessions {
            let completed = session.finish();
            evidence.cleanup_confirmed &= completed.evidence.cleanup_confirmed;
            evidence
                .output_files
                .extend(completed.evidence.output_files);
            if let Err(error) = completed.result {
                failure.get_or_insert(error);
            }
            if mode == Mode::Record {
                let captures = completed.evidence.captures;
                if captures.len() != 1 {
                    failure.get_or_insert_with(|| {
                        Error::Run("auxiliary tape did not produce exactly one capture".into())
                    });
                } else {
                    evidence.auxiliary_captures.push(proxy::AuxiliaryCapture {
                        kind: format!("aux-http-{name}"),
                        capture: captures.into_iter().next().expect("one capture"),
                    });
                }
            }
        }
        if failure.is_some() {
            evidence.auxiliary_complete = false;
        }
        failure.map_or(Ok(()), Err)
    }
}

pub(crate) struct AgentContext<'a> {
    pub workspace: &'a Path,
    pub agent: Option<&'a str>,
    pub recorder: Option<&'a str>,
    pub invocation_dir: &'a Path,
}

impl Session {
    #[cfg(test)]
    pub fn start(
        mode: Mode,
        cassette: &Path,
        upstream: Option<&str>,
        respect_record_timings: bool,
        workspace: &Path,
    ) -> Result<Self, Error> {
        Self::start_for_agent(
            mode,
            cassette,
            upstream,
            respect_record_timings,
            workspace,
            None,
            None,
        )
    }

    /// Start a tape session with an optional adapter-owned opaque wire
    /// extension. The tape itself never interprets that extension's protocol.
    #[cfg(test)]
    pub fn start_for_agent(
        mode: Mode,
        cassette: &Path,
        upstream: Option<&str>,
        respect_record_timings: bool,
        workspace: &Path,
        agent: Option<&str>,
        recorder: Option<&str>,
    ) -> Result<Self, Error> {
        Self::start_for_agent_at(
            mode,
            cassette,
            upstream,
            respect_record_timings,
            AgentContext {
                workspace,
                agent,
                recorder,
                invocation_dir: Path::new("."),
            },
        )
    }

    pub fn start_for_agent_at(
        mode: Mode,
        cassette: &Path,
        upstream: Option<&str>,
        respect_record_timings: bool,
        context: AgentContext<'_>,
    ) -> Result<Self, Error> {
        if mode == Mode::Replay {
            require_replay_cassette(cassette)?;
        }
        let settings = proxy::RecordingRequest {
            recorder: context.recorder,
            mode,
            cassette,
            respect_record_timings,
            workspace: context.workspace,
            invocation_dir: context.invocation_dir,
            wire_extension: context.agent.and_then(crate::agents::tape_wire_extension),
        };
        proxy::start_recording_chain(&settings, upstream).map(Self)
    }

    pub fn url(&self) -> &str {
        self.0.url()
    }

    #[cfg(test)]
    pub fn stop(self) -> Result<(), Error> {
        self.0.stop()
    }

    pub(crate) fn finish(self) -> proxy::Completion {
        self.0.finish()
    }
}

/// Validate a replay input before allocating run resources or starting a proxy.
pub(crate) fn require_replay_cassette(cassette: &Path) -> Result<(), Error> {
    let mut present = crate::cassette::Source::is_present(cassette)
        .map_err(|error| Error::Input(error.to_string()))?;
    for replica in 1..=crate::benchmark::MAX_CONCURRENCY {
        present |= crate::cassette::Source::is_present(&crate::benchmark_paths::replica_path(
            cassette, replica,
        ))
        .map_err(|error| Error::Input(error.to_string()))?;
    }
    if present {
        Ok(())
    } else {
        Err(Error::Input(format!(
            "replay cassette {} does not exist; run `asb run CONFIG --record --cassette {}` first",
            cassette.display(),
            cassette.display()
        )))
    }
}

#[cfg(test)]
mod auxiliary_tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn recorded_auxiliary_session_returns_named_finalized_capture() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/auxiliary-tape-record-tests")
            .join(crate::tools::runtime::unique_id());
        std::fs::create_dir_all(&root).unwrap();
        let cassette = root.canonicalize().unwrap().join("capture.jsonl");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let headers = String::from_utf8(request).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}").unwrap();
        });
        let mut sessions = AuxiliarySessions::default();
        sessions
            .start(
                "embedding",
                Mode::Record,
                &cassette,
                Some(&upstream),
                false,
                AgentContext {
                    workspace: &root,
                    agent: None,
                    recorder: None,
                    invocation_dir: &root,
                },
            )
            .unwrap();
        let endpoints = sessions.endpoints();
        let address = endpoints["embedding"]
            .trim_start_matches("http://")
            .trim_end_matches("/v1");
        let mut socket = std::net::TcpStream::connect(address).unwrap();
        let body = r#"{"model":"embedding","input":["fixture"]}"#;
        write!(socket, "POST /v1/embeddings HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        assert!(response.contains("200 OK"));
        server.join().unwrap();
        let mut evidence = proxy::Evidence {
            captures: Vec::new(),
            auxiliary_captures: Vec::new(),
            auxiliary_complete: true,
            output_files: Vec::new(),
            cleanup_confirmed: true,
        };
        sessions.finish(&mut evidence).unwrap();
        assert!(evidence.cleanup_confirmed);
        assert_eq!(evidence.auxiliary_captures.len(), 1);
        assert_eq!(evidence.auxiliary_captures[0].kind, "aux-http-embedding");
        assert!(evidence.auxiliary_captures[0].capture.complete);
        assert_eq!(evidence.auxiliary_captures[0].capture.inference_requests, 0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn independent_auxiliary_sessions_drain_and_validate_their_own_replay() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/auxiliary-tape-tests")
            .join(crate::tools::runtime::unique_id());
        std::fs::create_dir_all(&root).unwrap();
        let cassette = root.join("fixture.jsonl");
        let body = r#"{"model":"embedding","input":["fixture"]}"#;
        let entry = serde_json::json!({"request":{"method":"POST","url":"/v1/embeddings","body":body},
            "response":{"status":200,"headers":{"content-type":"application/json"},"body":"{}"}});
        std::fs::write(&cassette, format!("{entry}\n")).unwrap();
        let mut sessions = AuxiliarySessions::default();
        for name in ["first", "second"] {
            sessions
                .start(
                    name,
                    Mode::Replay,
                    &cassette,
                    None,
                    false,
                    AgentContext {
                        workspace: &root,
                        agent: None,
                        recorder: None,
                        invocation_dir: &root,
                    },
                )
                .unwrap();
        }
        assert!(
            sessions
                .start(
                    "first",
                    Mode::Replay,
                    &cassette,
                    None,
                    false,
                    AgentContext {
                        workspace: &root,
                        agent: None,
                        recorder: None,
                        invocation_dir: &root,
                    }
                )
                .is_err()
        );
        let endpoints = sessions.endpoints();
        assert_ne!(endpoints["first"], endpoints["second"]);
        for endpoint in endpoints.values() {
            let address = endpoint
                .trim_start_matches("http://")
                .trim_end_matches("/v1");
            let mut socket = std::net::TcpStream::connect(address).unwrap();
            write!(socket, "POST /v1/embeddings HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).unwrap();
            assert!(response.contains("200 OK"));
        }
        let mut evidence = proxy::Evidence {
            captures: Vec::new(),
            auxiliary_captures: Vec::new(),
            auxiliary_complete: true,
            output_files: Vec::new(),
            cleanup_confirmed: true,
        };
        sessions.finish(&mut evidence).unwrap();
        assert!(evidence.cleanup_confirmed);
        assert!(evidence.auxiliary_captures.is_empty());
        let mut incomplete = AuxiliarySessions::default();
        incomplete
            .start(
                "unused",
                Mode::Replay,
                &cassette,
                None,
                false,
                AgentContext {
                    workspace: &root,
                    agent: None,
                    recorder: None,
                    invocation_dir: &root,
                },
            )
            .unwrap();
        assert!(incomplete.finish(&mut evidence).is_err());
        assert!(!evidence.auxiliary_complete);
        std::fs::remove_dir_all(root).unwrap();
    }
}
