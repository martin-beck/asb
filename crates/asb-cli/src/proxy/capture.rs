//! Bounded local recorder receipts, independent of archives and client outcomes.
use crate::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_RECEIPT: u64 = 16 * 1024;
const MAX_PAYLOAD: u64 = 256 * 1024 * 1024;
const MAX_REQUESTS: u64 = 1_000_000;
const MAX_DELIVERY_FAILURES: usize = 16;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CaptureEvidence {
    pub adapter: String,
    pub output: PathBuf,
    pub complete: bool,
    pub accepted: u64,
    pub completed: u64,
    pub failed: u64,
    pub recorded: u64,
    pub inference_requests: u64,
    pub bytes: u64,
    pub sha256: String,
}

impl CaptureEvidence {
    pub fn validate(&self) -> Result<(), Error> {
        if self.adapter.is_empty()
            || self.adapter.len() > 64
            || self.adapter.chars().any(char::is_control)
            || !self.output.is_absolute()
            || self.accepted > MAX_REQUESTS
            || self.completed.checked_add(self.failed) != Some(self.accepted)
            || self.completed > self.recorded
            || self.recorded > self.accepted
            || self.inference_requests > self.recorded
            || self.bytes > MAX_PAYLOAD
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || (self.bytes == 0 && self.recorded != 0)
            || (self.complete
                && (self.failed != 0
                    || self.completed != self.recorded
                    || (self.bytes != 0 && self.recorded == 0)))
        {
            return Err(invalid("capture metadata or handler accounting differs"));
        }
        Ok(())
    }
}

pub(crate) struct Request {
    pub receipt: PathBuf,
    pub output: PathBuf,
    pub nonce: String,
    pub pid: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    mode: String,
    pid: u32,
    nonce: String,
    output: PathBuf,
    accepted: u64,
    completed: u64,
    failed: u64,
    inflight: u64,
    recorded: u64,
    inference_requests: u64,
    bytes: u64,
    sha256: String,
    complete: bool,
    #[serde(default)]
    delivery_failures: Vec<DeliveryFailure>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveryFailure {
    sequence: u64,
    method: String,
    path: String,
    cause: String,
}

impl Request {
    pub fn read(&self, adapter: &str) -> Result<CaptureEvidence, Error> {
        let mut bytes = Vec::new();
        regular(&self.receipt)?
            .take(MAX_RECEIPT + 1)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() as u64 > MAX_RECEIPT {
            return Err(invalid("receipt exceeds its size limit"));
        }
        let receipt: Receipt = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("malformed or unsupported receipt"))?;
        if receipt.schema_version != 1
            || receipt.mode != "record"
            || receipt.pid != self.pid
            || receipt.nonce != self.nonce
            || receipt.output.as_os_str() != self.output.as_os_str()
            || receipt.inflight != 0
        {
            return Err(invalid("receipt identity or handler accounting differs"));
        }
        validate_delivery_failures(&receipt.delivery_failures, receipt.failed, receipt.accepted)?;
        let evidence = CaptureEvidence {
            adapter: adapter.into(),
            output: self.output.clone(),
            complete: receipt.complete,
            accepted: receipt.accepted,
            completed: receipt.completed,
            failed: receipt.failed,
            recorded: receipt.recorded,
            inference_requests: receipt.inference_requests,
            bytes: receipt.bytes,
            sha256: receipt.sha256.clone(),
        };
        evidence.validate()?;
        let mut payload = regular(&self.output)?;
        if payload.metadata().map_err(io_error)?.len() != receipt.bytes {
            return Err(invalid("payload length differs from receipt"));
        }
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut length = 0u64;
        let mut lines = 0u64;
        let mut last = None;
        loop {
            let count = payload.read(&mut buffer).map_err(io_error)?;
            if count == 0 {
                break;
            }
            length = length
                .checked_add(count as u64)
                .ok_or_else(|| invalid("payload length overflow"))?;
            if length > receipt.bytes {
                return Err(invalid("payload grew after finalization"));
            }
            lines += buffer[..count]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count() as u64;
            last = Some(buffer[count - 1]);
            hash.update(&buffer[..count]);
        }
        if length != receipt.bytes
            || payload.metadata().map_err(io_error)?.len() != length
            || format!("{:x}", hash.finalize()) != receipt.sha256
            || (receipt.complete
                && (lines != receipt.recorded || (length != 0 && last != Some(b'\n'))))
        {
            return Err(invalid("payload checksum or complete line count differs"));
        }
        Ok(evidence)
    }
}

fn validate_delivery_failures(
    failures: &[DeliveryFailure],
    failed: u64,
    accepted: u64,
) -> Result<(), Error> {
    if failures.len() > MAX_DELIVERY_FAILURES || failures.len() as u64 > failed {
        return Err(invalid(
            "delivery failure metadata exceeds accounting bounds",
        ));
    }
    let mut sequences = std::collections::BTreeSet::new();
    for failure in failures {
        if failure.sequence == 0
            || failure.sequence > accepted
            || !sequences.insert(failure.sequence)
            || failure.method.len() > 16
            || failure.method.len() < 3
            || !failure.method.bytes().all(|byte| byte.is_ascii_uppercase())
            || failure.path.len() > 256
            || !failure.path.starts_with('/')
            || failure
                .path
                .bytes()
                .any(|byte| byte.is_ascii_control() || matches!(byte, b'?' | b'#'))
            || !matches!(
                failure.cause.as_str(),
                "close" | "error" | "handler_error" | "undelivered"
            )
        {
            return Err(invalid("delivery failure metadata is malformed"));
        }
    }
    Ok(())
}

fn regular(path: &Path) -> Result<File, Error> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(invalid("receipt or payload is not a regular file"));
    }
    Ok(file)
}

fn invalid(reason: &str) -> Error {
    Error::Run(format!("recording finalization invalid: {reason}"))
}
fn io_error(error: std::io::Error) -> Error {
    Error::Run(format!("cannot verify recording finalization: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    struct Fixture {
        root: PathBuf,
        request: Request,
    }
    impl Fixture {
        fn new(payload: &[u8]) -> Self {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/capture-receipt-fixtures")
                .join(crate::tools::runtime::unique_id());
            std::fs::create_dir_all(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let request = Request {
                receipt: root.join("receipt.json"),
                output: root.join("trace.jsonl"),
                nonce: "private-invocation".into(),
                pid: 123,
            };
            std::fs::write(&request.output, payload).unwrap();
            Self { root, request }
        }
        fn receipt(&self, complete: bool) -> Value {
            let payload = std::fs::read(&self.request.output).unwrap();
            let count = u64::from(!payload.is_empty());
            json!({"schema_version":1,"mode":"record","pid":123,"nonce":"private-invocation","output":self.request.output,
                "accepted":count,"completed":if complete {count} else {0},"failed":if complete {0} else {count},"inflight":0,
                "recorded":if complete {count} else {0},"inference_requests":if complete {count} else {0},
                "bytes":payload.len(),"sha256":format!("{:x}",Sha256::digest(&payload)),"complete":complete})
        }
        fn publish(&self, value: &Value) {
            std::fs::write(&self.request.receipt, serde_json::to_vec(value).unwrap()).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn binds_complete_incomplete_and_empty_payloads_independently_of_process_outcome() {
        for (payload, complete) in [
            (b"{\"response\":500}\n".as_slice(), true),
            (b"partial".as_slice(), false),
            (b"".as_slice(), true),
        ] {
            let fixture = Fixture::new(payload);
            fixture.publish(&fixture.receipt(complete));
            let evidence = fixture.request.read("fixture-adapter").unwrap();
            assert_eq!(evidence.complete, complete);
            assert_eq!(evidence.bytes, payload.len() as u64);
            assert_eq!(evidence.adapter, "fixture-adapter");
        }
    }

    #[test]
    fn rejects_identity_counter_and_version_drift_without_echoing_receipt_contents() {
        let fixture = Fixture::new(b"{}\n");
        for (key, value) in [
            ("schema_version", json!(2)),
            ("mode", json!("replay")),
            ("pid", json!(124)),
            ("nonce", json!("do-not-echo-private-nonce")),
            ("output", json!("unrelated-output")),
            ("accepted", json!(MAX_REQUESTS + 1)),
            ("accepted", json!(2)),
            ("completed", json!(2)),
            ("failed", json!(1)),
            ("inflight", json!(1)),
            ("recorded", json!(0)),
            ("inference_requests", json!(2)),
            ("bytes", json!(MAX_PAYLOAD + 1)),
            ("bytes", json!(4)),
            ("sha256", json!("a".repeat(64))),
            ("sha256", json!("A".repeat(64))),
            ("unknown_field", json!(1)),
        ] {
            let mut receipt = fixture.receipt(true);
            receipt[key] = value;
            fixture.publish(&receipt);
            let error = fixture.request.read("fixture").unwrap_err().to_string();
            assert!(!error.contains("do-not-echo-private-nonce"));
        }
    }

    #[test]
    fn accepts_only_bounded_non_secret_delivery_failure_metadata() {
        let fixture = Fixture::new(b"{}\n");
        let mut receipt = fixture.receipt(false);
        receipt["accepted"] = json!(1);
        receipt["completed"] = json!(0);
        receipt["failed"] = json!(1);
        receipt["recorded"] = json!(1);
        receipt["inference_requests"] = json!(1);
        receipt["delivery_failures"] = json!([{
            "sequence": 1, "method": "POST", "path": "/v1/messages", "cause": "close"
        }]);
        fixture.publish(&receipt);
        assert!(fixture.request.read("llmtape").is_ok());

        for path in ["/v1/messages?credential=private", "not-a-path"] {
            let mut invalid = receipt.clone();
            invalid["delivery_failures"][0]["path"] = json!(path);
            fixture.publish(&invalid);
            assert!(fixture.request.read("llmtape").is_err());
        }
    }

    #[test]
    fn rejects_missing_oversized_malformed_and_changed_payload_evidence() {
        let fixture = Fixture::new(b"{}\n");
        assert!(fixture.request.read("fixture").is_err());
        for contents in [
            vec![b'x'; MAX_RECEIPT as usize + 1],
            b"{".to_vec(),
            b"{\"pid\":123,\"pid\":123}".to_vec(),
        ] {
            std::fs::write(&fixture.request.receipt, contents).unwrap();
            assert!(fixture.request.read("fixture").is_err());
        }
        fixture.publish(&fixture.receipt(true));
        std::fs::write(&fixture.request.output, b"[]\n").unwrap();
        assert!(fixture.request.read("fixture").is_err());
        fixture.publish(&fixture.receipt(true));
        std::fs::write(&fixture.request.output, b"[]\n{}\n").unwrap();
        assert!(fixture.request.read("fixture").is_err());
        fixture.publish(&fixture.receipt(true));
        assert!(
            fixture.request.read("fixture").is_err(),
            "two lines cannot claim one recorded interaction"
        );
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_receipt_or_payload_symlinks_or_blocks_on_fifo() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new(b"{}\n");
        let original = fixture.root.join("original.json");
        std::fs::write(
            &original,
            serde_json::to_vec(&fixture.receipt(true)).unwrap(),
        )
        .unwrap();
        symlink(&original, &fixture.request.receipt).unwrap();
        assert!(fixture.request.read("fixture").is_err());
        std::fs::remove_file(&fixture.request.receipt).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fixture.request.receipt)
                .status()
                .unwrap()
                .success()
        );
        assert!(fixture.request.read("fixture").is_err());
        std::fs::remove_file(&fixture.request.receipt).unwrap();
        fixture.publish(&fixture.receipt(true));
        std::fs::rename(
            &fixture.request.output,
            fixture.root.join("payload-original"),
        )
        .unwrap();
        symlink(
            fixture.root.join("payload-original"),
            &fixture.request.output,
        )
        .unwrap();
        assert!(fixture.request.read("fixture").is_err());
    }
}
