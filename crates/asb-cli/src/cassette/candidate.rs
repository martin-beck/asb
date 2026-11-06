//! Execute one explicitly requested sampled-cassette qualification.
use super::{Archive, Error, Limits, RecordingReceipt, ReplayAttempt};
use crate::{benchmark::BenchmarkConfig, runner, tape};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::path::Path;

/// Record one candidate and replay its single recorded track with the selected
/// replay concurrency. This operation makes live provider calls during record;
/// callers must invoke it only from an explicit preparation action.
///
/// Both configurations must identify the same trial and execution settings;
/// recording uses one replica, while replay may use the selected concurrency.
/// Publication and proxy-consumption observations are returned separately
/// from the workload grade. Output and cassette inspection are bounded.
pub fn record_and_replay_candidate(
    record_config: &Path,
    replay_config: &Path,
    cassette: &Path,
    invocation_dir: &Path,
    max_steps: Option<u64>,
) -> Result<ReplayAttempt, Error> {
    let load = |path: &Path| -> Result<BenchmarkConfig, Error> {
        let text = crate::read_config(path).map_err(|error| Error(error.to_string()))?;
        toml::from_str(&text)
            .map_err(|error| Error(format!("invalid sampled benchmark configuration: {error}")))
    };
    let record = load(record_config)?;
    let replay = load(replay_config)?;
    if record.concurrency != 1 || record.trial.is_none() || record.trial != replay.trial {
        return Err(Error("sample recording and replay must select the same explicit trial, with one recording replica".into()));
    }
    let mut comparable = record.clone();
    comparable.concurrency = replay.concurrency;
    comparable.workspace.clone_from(&replay.workspace);
    comparable.log_file.clone_from(&replay.log_file);
    if let (Some(record), Some(replay)) = (&mut comparable.measurements, &replay.measurements) {
        record.directory.clone_from(&replay.directory);
    }
    let record_value = serde_json::to_value(&comparable)
        .map_err(|_| Error("cannot encode sample recording configuration".into()))?;
    let replay_value = serde_json::to_value(&replay)
        .map_err(|_| Error("cannot encode sample replay configuration".into()))?;
    if record_value != replay_value {
        return Err(Error("sample recording and replay configurations differ beyond replica count and output paths".into()));
    }
    let cassette_string = cassette
        .to_str()
        .ok_or_else(|| Error("sample cassette path must be valid UTF-8".into()))?;
    let record_path = record_config
        .to_str()
        .ok_or_else(|| Error("sample recording configuration path must be valid UTF-8".into()))?;
    let replay_path = replay_config
        .to_str()
        .ok_or_else(|| Error("sample replay configuration path must be valid UTF-8".into()))?;
    let run = |path: &str, mode| -> (Result<(), crate::Error>, Capture) {
        let mut output = Capture(Vec::new());
        let result = runner::run_with_tape(
            path,
            runner::RunOptions {
                invocation_dir: Some(invocation_dir),
                repeat: Some(1),
                max_steps,
                json: true,
                quiet_output: true,
                mode: Some(mode),
                cassette: Some(cassette_string),
                ..Default::default()
            },
            &mut output,
        );
        (result, output)
    };
    let (record_result, record_output) = run(record_path, tape::Mode::Record);
    let receipt = RecordingReceipt::from_jsonl(&record_output.0, cassette).map_err(|error| {
        Error(format!(
            "sample recording was not safely published: {error}; runner: {}",
            record_result
                .err()
                .map_or_else(|| "no further diagnostic".into(), |error| error.to_string())
        ))
    })?;
    if receipt.tracks != 1 {
        return Err(Error(
            "sample recording did not publish exactly one track".into(),
        ));
    }
    let (length, digest) = hash_cassette(cassette)?;
    Archive::open_verified(cassette, length, &digest, Limits::default())?;
    let (replay_result, replay_output) = run(replay_path, tape::Mode::Replay);
    ReplayAttempt::from_jsonl(digest, &replay_output.0, replay.concurrency).map_err(|error| {
        Error(format!(
            "sample replay did not produce complete qualification evidence: {error}; runner: {}",
            replay_result
                .err()
                .map_or_else(|| "no further diagnostic".into(), |error| error.to_string())
        ))
    })
}

fn hash_cassette(path: &Path) -> Result<(u64, String), Error> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options
        .open(path)
        .map_err(|error| Error(format!("cannot open sample cassette: {error}")))?;
    let metadata = file
        .metadata()
        .map_err(|error| Error(format!("cannot inspect sample cassette: {error}")))?;
    let limit = Limits::default().max_archive_bytes;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(Error(
            "sample cassette is not a bounded regular file".into(),
        ));
    }
    let mut hash = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| Error(format!("cannot read sample cassette: {error}")))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .filter(|total| *total <= limit)
            .ok_or_else(|| Error("sample cassette grew beyond its size bound".into()))?;
        hash.update(&buffer[..read]);
    }
    if total != metadata.len() {
        return Err(Error("sample cassette changed while hashing".into()));
    }
    Ok((total, format!("{:x}", hash.finalize())))
}

struct Capture(Vec<u8>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > (16 * 1024 * 1024usize).saturating_sub(self.0.len()) {
            return Err(io::Error::other(
                "sample qualification output exceeds 16 MiB",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
