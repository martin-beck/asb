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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Framing {
    JsonLines,
    ContentLength,
}

/// Read one complete bounded JSON message, retaining the client's framing for
/// its corresponding response. `service` only scopes diagnostics; it is never
/// retained in a recording.
pub(crate) fn read(
    input: &mut impl BufRead,
    service: &str,
) -> Result<Option<(Value, Framing)>, Error> {
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
        return serde_json::from_str(first)
            .map(|request| Some((request, Framing::JsonLines)))
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
        .map(|request| Some((request, Framing::ContentLength)))
        .map_err(|_| Error::Run(format!("{service} request is malformed")))
}

/// Encode one JSON response with the framing selected by its request.
pub(crate) fn write(
    output: &mut impl Write,
    response: &Value,
    framing: Framing,
    service: &str,
) -> Result<(), Error> {
    let bytes = serde_json::to_vec(response)
        .map_err(|_| Error::Run(format!("cannot encode {service} response")))?;
    match framing {
        Framing::JsonLines => output.write_all(&bytes).map_err(crate::io_error)?,
        Framing::ContentLength => {
            write!(output, "Content-Length: {}\r\n\r\n", bytes.len()).map_err(crate::io_error)?;
            output.write_all(&bytes).map_err(crate::io_error)?;
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
        let (request, framing) = read(&mut BufReader::new(&request[..]), SERVICE)
            .unwrap()
            .unwrap();
        assert_eq!(framing, Framing::ContentLength);
        assert_eq!(request["method"], "initialize");

        let mut output = Vec::new();
        write(
            &mut output,
            &serde_json::json!({"jsonrpc":"2.0","id":7,"result":{}}),
            framing,
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
        let (request, framing) = read(
            &mut BufReader::new(&b"{\"jsonrpc\":\"2.0\",\"id\":1}\n"[..]),
            SERVICE,
        )
        .unwrap()
        .unwrap();
        assert_eq!(framing, Framing::JsonLines);
        assert_eq!(request["id"], 1);
        let mut output = Vec::new();
        write(&mut output, &serde_json::json!({"id":1}), framing, SERVICE).unwrap();
        assert_eq!(&output, b"{\"id\":1}\n");
    }

    #[test]
    fn rejects_malformed_and_oversized_messages() {
        assert!(read(&mut BufReader::new(&b"not json\n"[..]), SERVICE).is_err());
        let oversized = format!("{}\n", "x".repeat(MAX_MESSAGE_BYTES));
        assert!(read(&mut BufReader::new(oversized.as_bytes()), SERVICE).is_err());
    }
}
