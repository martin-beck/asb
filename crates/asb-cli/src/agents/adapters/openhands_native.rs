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

struct Options {
    mode: Mode,
    sidecar: PathBuf,
    launch_json: Option<String>,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    Record,
    Replay,
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
    RecordTerminal {
        schema_version: u32,
        action: serde_json::Value,
        observation: serde_json::Value,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Response {
    Ack {
        schema_version: u32,
    },
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

#[derive(Deserialize, Serialize)]
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
    if options.mode == Mode::Record {
        return record(options);
    }
    let launch_json = options
        .launch_json
        .as_deref()
        .ok_or_else(|| Error::Run("OpenHands native launch descriptor is missing".into()))?;
    if launch_json.len() > 32 * 1024 {
        return Err(Error::Run(
            "OpenHands native launch descriptor is excessive".into(),
        ));
    }
    let launch: NativeToolLaunch = serde_json::from_str(launch_json)
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

fn record(options: Options) -> Result<(), Error> {
    let parent = options
        .sidecar
        .parent()
        .ok_or_else(|| Error::Run("OpenHands native sidecar has no parent".into()))?;
    std::fs::create_dir_all(parent)
        .map_err(|_| Error::Run("cannot create OpenHands native sidecar directory".into()))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&options.sidecar)
        .map_err(|_| Error::Run("cannot create OpenHands native sidecar".into()))?;
    let session = "openhands-terminal-1";
    let digest = crate::cassette::digest(b"asb-openhands-sdk-terminal-v1");
    let mut write = |value: serde_json::Value| -> Result<(), Error> {
        let line = serde_json::to_vec(&value)
            .map_err(|_| Error::Run("cannot encode OpenHands native sidecar".into()))?;
        file.write_all(&line)
            .and_then(|_| file.write_all(b"\n"))
            .map_err(|_| Error::Run("cannot write OpenHands native sidecar".into()))
    };
    write(
        serde_json::json!({"event":"launch","session":"openhands-launch-1","program_sha256":digest}),
    )?;
    write(
        serde_json::json!({"event":"session_alias","alias":session,"session":"openhands-launch-1"}),
    )?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let mut line = Vec::new();
    let mut effect = Vec::new();
    loop {
        line.clear();
        if input
            .read_until(b'\n', &mut line)
            .map_err(|_| Error::Run("cannot read OpenHands native record frame".into()))?
            == 0
        {
            break;
        }
        if line.len() > MAX_LINE_BYTES || !line.ends_with(b"\n") {
            return Err(Error::Run(
                "OpenHands native record frame is oversized or unframed".into(),
            ));
        }
        let Request::RecordTerminal {
            schema_version,
            action,
            observation,
        } = serde_json::from_slice::<Request>(&line[..line.len() - 1])
            .map_err(|_| Error::Run("OpenHands native record frame is invalid".into()))?
        else {
            return Err(Error::Run(
                "OpenHands native record frame is invalid".into(),
            ));
        };
        if schema_version != SCHEMA_VERSION {
            return Err(Error::Run(
                "OpenHands native record schema is invalid".into(),
            ));
        }
        let request: Request = serde_json::from_value(action)
            .map_err(|_| Error::Run("OpenHands terminal action is invalid".into()))?;
        let encoded = match request {
            Request::TerminalAction {
                schema_version,
                command,
                is_input,
                timeout,
                reset,
            } => {
                if schema_version != 1 || command.len() > MAX_COMMAND_BYTES || (is_input && reset) {
                    return Err(Error::Run(
                        "OpenHands terminal action is unsupported".into(),
                    ));
                }
                serde_json::to_vec(&SidecarRequest::TerminalAction {
                    schema_version,
                    command: &command,
                    is_input,
                    timeout,
                    reset,
                })
                .unwrap()
            }
            _ => unreachable!(),
        };
        let observation: SidecarResponse = serde_json::from_value(observation)
            .map_err(|_| Error::Run("OpenHands terminal observation is invalid".into()))?;
        let SidecarResponse::TerminalObservation {
            schema_version,
            content,
            ..
        } = &observation;
        if *schema_version != SCHEMA_VERSION || content.len() > MAX_CONTENT_BYTES {
            return Err(Error::Run(
                "OpenHands terminal observation is unsupported".into(),
            ));
        }
        let encoded_output = serde_json::to_vec(&observation)
            .map_err(|_| Error::Run("cannot encode OpenHands terminal observation".into()))?;
        write(serde_json::json!({"event":"poll","alias":session}))?;
        write(
            serde_json::json!({"event":"write","alias":session,"request_sha256":crate::cassette::digest(&encoded),"request_b64":base64(&encoded)}),
        )?;
        write(
            serde_json::json!({"event":"output","alias":session,"response_sha256":crate::cassette::digest(&encoded_output),"output_b64":base64(&encoded_output)}),
        )?;
        effect.extend_from_slice(&encoded);
        effect.extend_from_slice(&encoded_output);
        let ack = serde_json::to_vec(&Response::Ack { schema_version: 1 }).unwrap();
        output
            .write_all(&ack)
            .and_then(|_| output.write_all(b"\n"))
            .and_then(|_| output.flush())
            .map_err(|_| Error::Run("cannot acknowledge OpenHands terminal record".into()))?;
    }
    write(serde_json::json!({"event":"exit","alias":session,"success":true}))?;
    write(
        serde_json::json!({"event":"workspace_effect","sha256":crate::cassette::digest(&effect)}),
    )?;
    Ok(())
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn parse_options(args: &[OsString]) -> Result<Options, Error> {
    let mut sidecar = None;
    let mut launch_json = None;
    let mut mode = None;
    let mut values = args.iter();
    while let Some(flag) = values.next() {
        let value = values
            .next()
            .ok_or_else(|| Error::Run("OpenHands native bridge arguments are incomplete".into()))?;
        match flag.to_str() {
            Some("--mode") if mode.is_none() => {
                mode = match value.to_str() {
                    Some("record") => Some(Mode::Record),
                    Some("replay") => Some(Mode::Replay),
                    _ => None,
                }
            }
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
        mode: mode.ok_or_else(|| {
            Error::Run("OpenHands native bridge mode is missing or invalid".into())
        })?,
        sidecar: sidecar.ok_or_else(|| Error::Run("OpenHands native sidecar is missing".into()))?,
        launch_json,
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
    } = request
    else {
        return Err(Error::Run("OpenHands terminal action is invalid".into()));
    };
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
