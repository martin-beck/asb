//! Bounded stdio framing shared by ASB-owned MCP endpoints and relays.
//!
//! MCP clients in the supported agent families use either JSON Lines or the
//! `Content-Length` framing familiar from language servers. Keeping this
//! transport detail here makes a future transparent relay obey exactly the
//! private local-tool server's limits without coupling either endpoint to MCP
//! method semantics.

use crate::Error;
use serde_json::Value;
use std::io::{BufRead, Write};

pub(crate) const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Framing {
    JsonLines,
    ContentLength,
}

/// A validated JSON message retaining its original bounded JSON bytes. A relay
/// forwards `bytes` directly so whitespace/key ordering are not changed merely
/// because ASB observed the message.
#[derive(Clone, Debug)]
pub(crate) struct Message {
    pub value: Value,
    pub bytes: Vec<u8>,
    pub framing: Framing,
}

/// Read one complete bounded JSON message, retaining the client's framing for
/// its corresponding response. `service` only scopes diagnostics; it is never
/// retained in a recording.
pub(crate) fn read(input: &mut impl BufRead, service: &str) -> Result<Option<Message>, Error> {
    let mut first = String::new();
    let read = input.read_line(&mut first).map_err(crate::io_error)?;
    if read == 0 {
        return Ok(None);
    }
    if read > MAX_MESSAGE_BYTES {
        return Err(Error::Run(format!("{service} request exceeds bound")));
    }
    let first = first.trim_end_matches(['\r', '\n']);
    if !first.eq_ignore_ascii_case("content-length:")
        && !first.to_ascii_lowercase().starts_with("content-length:")
    {
        let bytes = first.as_bytes().to_vec();
        return serde_json::from_slice(&bytes)
            .map(|value| {
                Some(Message {
                    value,
                    bytes,
                    framing: Framing::JsonLines,
                })
            })
            .map_err(|_| Error::Run(format!("{service} request is malformed")));
    }
    let length = first
        .split_once(':')
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .filter(|length| *length <= MAX_MESSAGE_BYTES)
        .ok_or_else(|| Error::Run(format!("{service} Content-Length is invalid")))?;
    loop {
        let mut header = String::new();
        let read = input.read_line(&mut header).map_err(crate::io_error)?;
        if read == 0 {
            return Err(Error::Run(format!("{service} header is incomplete")));
        }
        if header == "\n" || header == "\r\n" {
            break;
        }
        if header.len() > MAX_MESSAGE_BYTES {
            return Err(Error::Run(format!("{service} header exceeds bound")));
        }
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body).map_err(crate::io_error)?;
    serde_json::from_slice(&body)
        .map(|value| {
            Some(Message {
                value,
                bytes: body,
                framing: Framing::ContentLength,
            })
        })
        .map_err(|_| Error::Run(format!("{service} request is malformed")))
}

/// Encode one JSON response with the framing selected by its request.
pub(crate) fn write(
    output: &mut (impl Write + ?Sized),
    response: &Value,
    framing: Framing,
    service: &str,
) -> Result<(), Error> {
    let bytes = serde_json::to_vec(response)
        .map_err(|_| Error::Run(format!("cannot encode {service} response")))?;
    write_bytes(output, &bytes, framing)
}

/// Forward already validated JSON bytes without changing their representation.
pub(crate) fn write_bytes(
    output: &mut (impl Write + ?Sized),
    bytes: &[u8],
    framing: Framing,
) -> Result<(), Error> {
    match framing {
        Framing::JsonLines => output.write_all(bytes).map_err(crate::io_error)?,
        Framing::ContentLength => {
            write!(output, "Content-Length: {}\r\n\r\n", bytes.len()).map_err(crate::io_error)?;
            output.write_all(bytes).map_err(crate::io_error)?;
        }
    }
    if framing == Framing::JsonLines {
        output.write_all(b"\n").map_err(crate::io_error)?;
    }
    output.flush().map_err(crate::io_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    const SERVICE: &str = "fixture MCP";

    #[test]
    fn content_length_round_trips_and_preserves_framing() {
        let body = br#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#;
        let request = [
            format!("Content-Length: {}\r\nX-Trace: fixture\r\n\r\n", body.len()).into_bytes(),
            body.to_vec(),
        ]
        .concat();
        let request = read(&mut BufReader::new(&request[..]), SERVICE)
            .unwrap()
            .unwrap();
        assert_eq!(request.framing, Framing::ContentLength);
        assert_eq!(request.value["method"], "initialize");

        let mut output = Vec::new();
        write(
            &mut output,
            &serde_json::json!({"jsonrpc":"2.0","id":7,"result":{}}),
            request.framing,
            SERVICE,
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
    fn json_lines_round_trip() {
        let request = read(
            &mut BufReader::new(&b"{\"jsonrpc\":\"2.0\",\"id\":1}\n"[..]),
            SERVICE,
        )
        .unwrap()
        .unwrap();
        assert_eq!(request.framing, Framing::JsonLines);
        assert_eq!(request.value["id"], 1);
        let mut output = Vec::new();
        write(
            &mut output,
            &serde_json::json!({"id":1}),
            request.framing,
            SERVICE,
        )
        .unwrap();
        assert_eq!(&output, b"{\"id\":1}\n");
    }

    #[test]
    fn forwards_original_validated_json_bytes() {
        let raw = b"{ \"jsonrpc\" : \"2.0\", \"id\" : 1 }\n";
        let message = read(&mut BufReader::new(&raw[..]), SERVICE)
            .unwrap()
            .unwrap();
        let mut output = Vec::new();
        write_bytes(&mut output, &message.bytes, message.framing).unwrap();
        assert_eq!(&output, raw);
    }

    #[test]
    fn rejects_malformed_and_oversized_messages() {
        assert!(read(&mut BufReader::new(&b"not json\n"[..]), SERVICE).is_err());
        let oversized = format!("{}\n", "x".repeat(MAX_MESSAGE_BYTES));
        assert!(read(&mut BufReader::new(oversized.as_bytes()), SERVICE).is_err());
    }
}
