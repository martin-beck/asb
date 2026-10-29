//! Private OpenHands SDK terminal bridge for sealed native replay.
//!
//! The Python SDK process never reads a cassette or sidecar. It sends one
//! strict terminal action over stdio to this ASB-owned bridge, which alone
//! validates and consumes the already-authorized owner-only sidecar.

use crate::{Error, cassette::NativeToolLaunch};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

const SCHEMA_VERSION: u32 = 1;
const MAX_LINE_BYTES: usize = 512 * 1024;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_CONTENT_BYTES: usize = 384 * 1024;
const MAX_TIMEOUT_SECONDS: f64 = 86_400.0;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Options {
    sidecar: PathBuf,
    launch_json: String,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    TerminalAction {
        schema_version: u32,
        command: String,
        is_input: bool,
        timeout: Option<f64>,
        reset: bool,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Response {
    TerminalObservation {
        schema_version: u32,
        content: String,
        is_error: bool,
        command: Option<String>,
        exit_code: Option<i32>,
        timeout: bool,
        metadata: Metadata,
    },
    Error {
        schema_version: u32,
        message: &'static str,
    },
}

/// The only OpenHands terminal fields ASB carries across its private bridge.
/// Host-identifying metadata remains absent rather than being invented.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    exit_code: i32,
    #[serde(default = "negative_one")]
    pid: i32,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    py_interpreter_path: Option<String>,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    suffix: String,
}

fn negative_one() -> i32 {
    -1
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SidecarRequest<'a> {
    TerminalAction {
        schema_version: u32,
        command: &'a str,
        is_input: bool,
        timeout: Option<f64>,
        reset: bool,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SidecarResponse {
    TerminalObservation {
        schema_version: u32,
        content: String,
        is_error: bool,
        command: Option<String>,
        exit_code: Option<i32>,
        timeout: bool,
        metadata: Metadata,
    },
}

/// Run the adapter-private bridge. Stdout is protocol-only; diagnostics are
/// deliberately bounded and payload-free.
pub(crate) fn entry(args: &[OsString]) -> std::process::ExitCode {
    match run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ASB OpenHands native bridge: {error}");
            std::process::ExitCode::from(2)
        }
    }
}

fn run(args: &[OsString]) -> Result<(), Error> {
    let options = parse_options(args)?;
    if options.launch_json.len() > 32 * 1024 {
        return Err(Error::Run(
            "OpenHands native launch descriptor is excessive".into(),
        ));
    }
    let launch: NativeToolLaunch = serde_json::from_str(&options.launch_json)
        .map_err(|_| Error::Run("OpenHands native launch descriptor is invalid".into()))?;
    let mut transport = launch.open_staged(&options.sidecar).map_err(|_| {
        Error::Run("OpenHands native sidecar is unavailable or differs from its descriptor".into())
    })?;
    let [session] = launch.sessions() else {
        return Err(Error::Run(
            "OpenHands native replay requires exactly one sealed terminal session".into(),
        ));
    };
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = input
            .read_until(b'\n', &mut line)
            .map_err(|_| Error::Run("cannot read OpenHands native bridge request".into()))?;
        if read == 0 {
            break;
        }
        if line.len() > MAX_LINE_BYTES || !line.ends_with(b"\n") {
            return Err(Error::Run(
                "OpenHands native bridge request is oversized or unframed".into(),
            ));
        }
        let response = match serde_json::from_slice::<Request>(&line[..line.len() - 1]) {
            Ok(request) => replay_request(&mut transport, session.alias(), request),
            Err(_) => Err(Error::Run(
                "OpenHands native bridge request is invalid".into(),
            )),
        };
        let response = match response {
            Ok(response) => response,
            Err(_) => Response::Error {
                schema_version: SCHEMA_VERSION,
                message: "terminal action is unsupported or differs from recording",
            },
        };
        let encoded = serde_json::to_vec(&response)
            .map_err(|_| Error::Run("cannot encode OpenHands native bridge response".into()))?;
        output
            .write_all(&encoded)
            .and_then(|()| output.write_all(b"\n"))
            .and_then(|()| output.flush())
            .map_err(|_| Error::Run("cannot write OpenHands native bridge response".into()))?;
        if matches!(response, Response::Error { .. }) {
            return Err(Error::Run(
                "OpenHands native bridge rejected terminal action".into(),
            ));
        }
    }
    transport.require_complete_replay().map_err(|_| {
        Error::Run("OpenHands native replay left sealed terminal actions unused".into())
    })
}

fn parse_options(args: &[OsString]) -> Result<Options, Error> {
    let mut sidecar = None;
    let mut launch_json = None;
    let mut values = args.iter();
    while let Some(flag) = values.next() {
        let value = values
            .next()
            .ok_or_else(|| Error::Run("OpenHands native bridge arguments are incomplete".into()))?;
        match flag.to_str() {
            Some("--sidecar") if sidecar.is_none() => sidecar = Some(PathBuf::from(value)),
            Some("--launch-json") if launch_json.is_none() => {
                launch_json = value.to_str().map(str::to_owned)
            }
            _ => {
                return Err(Error::Run(
                    "OpenHands native bridge arguments are invalid".into(),
                ));
            }
        }
    }
    Ok(Options {
        sidecar: sidecar.ok_or_else(|| Error::Run("OpenHands native sidecar is missing".into()))?,
        launch_json: launch_json
            .ok_or_else(|| Error::Run("OpenHands native launch descriptor is missing".into()))?,
    })
}

fn replay_request(
    transport: &mut crate::cassette::NativeToolTransport,
    session: &str,
    request: Request,
) -> Result<Response, Error> {
    let Request::TerminalAction {
        schema_version,
        command,
        is_input,
        timeout,
        reset,
    } = request;
    if schema_version != SCHEMA_VERSION
        || command.len() > MAX_COMMAND_BYTES
        || timeout.is_some_and(|value| {
            !value.is_finite() || !(0.0..=MAX_TIMEOUT_SECONDS).contains(&value)
        })
        || (is_input && reset)
    {
        return Err(Error::Run(
            "OpenHands terminal action is unsupported".into(),
        ));
    }
    let encoded = serde_json::to_vec(&SidecarRequest::TerminalAction {
        schema_version,
        command: &command,
        is_input,
        timeout,
        reset,
    })
    .map_err(|_| Error::Run("cannot encode OpenHands terminal action".into()))?;
    let output = transport
        .replay_output(session, &encoded)
        .map_err(|_| Error::Run("OpenHands terminal action differs from recording".into()))?;
    let SidecarResponse::TerminalObservation {
        schema_version,
        content,
        is_error,
        command,
        exit_code,
        timeout,
        metadata,
    } = serde_json::from_slice(&output)
        .map_err(|_| Error::Run("OpenHands terminal observation is invalid".into()))?;
    if schema_version != SCHEMA_VERSION || content.len() > MAX_CONTENT_BYTES {
        return Err(Error::Run(
            "OpenHands terminal observation is unsupported".into(),
        ));
    }
    Ok(Response::TerminalObservation {
        schema_version,
        content,
        is_error,
        command,
        exit_code,
        timeout,
        metadata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_codec_rejects_extra_or_incompatible_action_fields() {
        let extra = r#"{"kind":"terminal_action","schema_version":1,"command":"ls","is_input":false,"timeout":null,"reset":false,"extra":true}"#;
        assert!(serde_json::from_str::<Request>(extra).is_err());
        let invalid = r#"{"kind":"terminal_action","schema_version":1,"command":"ls","is_input":true,"timeout":null,"reset":true}"#;
        let mut transport = impossible_transport();
        assert!(
            replay_request(
                &mut transport,
                "shell-1",
                serde_json::from_str(invalid).unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn terminal_codec_rejects_extra_observation_fields() {
        let invalid = r#"{"kind":"terminal_observation","schema_version":1,"content":"ok","is_error":false,"command":"ls","exit_code":0,"timeout":false,"metadata":{"exit_code":0},"extra":true}"#;
        assert!(serde_json::from_str::<SidecarResponse>(invalid).is_err());
    }

    fn impossible_transport() -> crate::cassette::NativeToolTransport {
        let digest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let request = b"{}";
        let output = b"{}";
        let lines = [
            format!(
                "{{\"event\":\"launch\",\"session\":\"launch-1\",\"program_sha256\":\"{digest}\"}}"
            ),
            "{\"event\":\"session_alias\",\"alias\":\"shell-1\",\"session\":\"launch-1\"}".into(),
            "{\"event\":\"poll\",\"alias\":\"shell-1\"}".into(),
            format!(
                "{{\"event\":\"write\",\"alias\":\"shell-1\",\"request_sha256\":\"{}\",\"request_b64\":\"e30=\"}}",
                crate::cassette::digest(request)
            ),
            format!(
                "{{\"event\":\"output\",\"alias\":\"shell-1\",\"response_sha256\":\"{}\",\"output_b64\":\"e30=\"}}",
                crate::cassette::digest(output)
            ),
            "{\"event\":\"exit\",\"alias\":\"shell-1\",\"success\":true}".into(),
            format!("{{\"event\":\"workspace_effect\",\"sha256\":\"{digest}\"}}"),
        ];
        let bytes = format!("{}\n", lines.join("\n")).into_bytes();
        let native = crate::cassette::NativeTrack {
            complete: true,
            events: lines.len() as u64,
            bytes: bytes.len() as u64,
            sha256: crate::cassette::digest(&bytes),
        };
        crate::cassette::NativeToolTransport::from_sidecar(&native, &bytes).unwrap()
    }
}
