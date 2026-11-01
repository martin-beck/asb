//! Pinned legacy inputs: historical routing, bounded reads, no path reopen.
use super::{Plan, ReplayInput, RunOptions, Slot, path_text, private_directory};
use crate::{Error, benchmark::BenchmarkConfig, io_error};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};

const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

pub(super) struct Prepared {
    files: Vec<File>,
}

/// A routing/metadata declaration, not a serializable payload identity. No
/// empty or capacity-only digest can escape as a publishable ReplayInput.
pub(super) struct Inspection {
    files: Vec<File>,
    inputs: Vec<InputMetadata>,
}

struct InputMetadata {
    source: PathBuf,
    bytes: u64,
}

impl Inspection {
    pub fn validate(&self) -> Result<(), Error> {
        for (input, file) in self.inputs.iter().zip(&self.files) {
            if check_metadata(file)? != input.bytes {
                return Err(Error::Input(
                    "legacy replay input length changed before reading".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn input_cost(&self) -> super::InputCost {
        super::InputCost {
            inputs: self.inputs.len(),
            bytes: self.inputs.iter().map(|input| input.bytes).sum(),
            observed_bytes: self.inputs.iter().map(|input| input.bytes).sum(),
            files: self.files.len(),
            directory_bytes: 0,
            manifest_bytes: 0,
        }
    }

    pub fn open(
        config_path: &Path,
        config: &BenchmarkConfig,
        options: RunOptions<'_>,
        slots: &mut [Slot],
    ) -> Result<Self, Error> {
        let base = options
            .cassette
            .map(PathBuf::from)
            .unwrap_or_else(|| super::super::default_cassette(config_path, config));
        let mut seen = BTreeMap::new();
        let mut inputs = Vec::new();
        let mut files = Vec::new();
        let mut total = 0u64;
        let mut replicas = Vec::new();
        // Inspect the whole population before hashing any payload. Discovery
        // deliberately retains replica_cassette's existing precedence rules.
        for replica in 1..=config.concurrency {
            let source = if config.concurrency == 1 {
                base.clone()
            } else {
                super::super::replica_cassette(crate::tape::Mode::Replay, &base, replica)
            };
            let source = source.canonicalize().map_err(|error| {
                Error::Input(format!(
                    "cannot resolve replay input {}: {error}",
                    source.display()
                ))
            })?;
            let id = if let Some(id) = seen.get(&source) {
                *id
            } else {
                let file = open_regular(&source)?;
                let bytes = file.metadata().map_err(io_error)?.len();
                total = total
                    .checked_add(bytes)
                    .filter(|total| *total <= crate::cassette::Limits::default().max_total_bytes)
                    .ok_or_else(|| {
                        Error::Input("replay inputs exceed 2 GiB aggregate bound".into())
                    })?;
                let id = inputs.len();
                inputs.push(InputMetadata {
                    source: source.clone(),
                    bytes,
                });
                files.push(file);
                seen.insert(source, id);
                id
            };
            replicas.push(id);
        }
        for slot in slots {
            slot.replay_input = Some(replicas[slot.replica - 1]);
        }
        Ok(Self { files, inputs })
    }

    pub fn observe(mut self) -> Result<(Prepared, Vec<ReplayInput>), Error> {
        #[cfg(test)]
        tests::after_metadata_pass();
        // Validate ALL pinned lengths again before even the first payload read.
        self.validate()?;
        let mut inputs = Vec::new();
        for (index, (input, file)) in self.inputs.into_iter().zip(&mut self.files).enumerate() {
            let (bytes, sha256) = transfer(file, input.bytes, &mut std::io::sink())?;
            if bytes != input.bytes {
                return Err(Error::Input(
                    "legacy replay input changed during planning".into(),
                ));
            }
            inputs.push(ReplayInput {
                recorder: None,
                complete: None,
                track: None,
                source: input.source,
                private: format!("inputs/recording-{}.jsonl", index + 1).into(),
                local_tools: None,
                tool_protocol: None,
                deterministic_manifest: None,
                native_trace: None,
                native_tool: None,
                #[cfg(not(test))]
                native_tool_launch: None,
                mcp_relays: Vec::new(),
                bytes,
                sha256,
            });
        }
        Ok((Prepared { files: self.files }, inputs))
    }
}

impl Prepared {
    #[cfg(test)]
    pub fn open(
        config_path: &Path,
        config: &BenchmarkConfig,
        options: RunOptions<'_>,
        slots: &mut [Slot],
    ) -> Result<(Self, Vec<ReplayInput>), Error> {
        Inspection::open(config_path, config, options, slots)?.observe()
    }

    pub fn stage(mut self, root: &Path, plan: &Plan) -> Result<Option<String>, Error> {
        if self.files.len() != plan.replay_inputs.len() || self.files.is_empty() {
            return Err(Error::Input(
                "pinned legacy input population differs from plan".into(),
            ));
        }
        private_directory(&root.join("inputs"), false)?;
        for (input, source) in plan.replay_inputs.iter().zip(&mut self.files) {
            let destination = root.join(&input.private);
            let pending = destination.with_extension("partial");
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&pending).map_err(io_error)?;
            let observed = transfer(source, input.bytes, &mut file).map_err(|error| match error {
                Error::Input(reason) => Error::Input(format!("replay input changed after execution planning; no agent was started: {reason}")),
                other => other,
            })?;
            if observed != (input.bytes, input.sha256.clone()) {
                return Err(Error::Input(
                    "replay input changed after execution planning; no agent was started".into(),
                ));
            }
            file.sync_all()
                .and_then(|()| fs::hard_link(&pending, &destination))
                .map_err(io_error)?;
            sync_inputs(root)?;
            fs::remove_file(pending).map_err(io_error)?;
        }
        let base = root.join("inputs/cassette.jsonl");
        for slot in plan.slots.iter().take(plan.concurrency) {
            let source = &plan.replay_inputs[slot.replay_input.expect("validated replay slot")];
            let destination = if plan.concurrency == 1 {
                base.clone()
            } else {
                super::super::replica_cassette(crate::tape::Mode::Record, &base, slot.replica)
            };
            fs::hard_link(root.join(&source.private), destination).map_err(io_error)?;
        }
        sync_inputs(root)?;
        path_text(&base).map(Some)
    }
}

fn sync_inputs(root: &Path) -> Result<(), Error> {
    File::open(root.join("inputs"))
        .and_then(|file| file.sync_all())
        .map_err(io_error)
}

fn open_regular(path: &Path) -> Result<File, Error> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(io_error)?;
    check_metadata(&file)?;
    Ok(file)
}

fn check_metadata(file: &File) -> Result<u64, Error> {
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() || metadata.len() > MAX_INPUT_BYTES {
        return Err(Error::Input(
            "legacy replay input must be a regular file at most 64 MiB".into(),
        ));
    }
    Ok(metadata.len())
}

fn transfer(
    file: &mut File,
    expected: u64,
    output: &mut dyn Write,
) -> Result<(u64, String), Error> {
    let length = check_metadata(file)?;
    if length != expected || expected > MAX_INPUT_BYTES {
        return Err(Error::Input(
            "legacy replay input length changed before reading".into(),
        ));
    }
    file.rewind().map_err(io_error)?;
    #[cfg(test)]
    TRANSFERS.with(|count| count.set(count.get() + 1));
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        // A single sentinel byte detects growth without consuming its suffix.
        let allowance = (expected - bytes + 1).min(buffer.len() as u64) as usize;
        let count = file.read(&mut buffer[..allowance]).map_err(io_error)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        if bytes > expected {
            return Err(Error::Input(
                "legacy replay input changed while reading".into(),
            ));
        }
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count]).map_err(io_error)?;
    }
    if bytes != length {
        return Err(Error::Input(
            "legacy replay input changed while reading".into(),
        ));
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}

#[cfg(test)]
pub(super) fn transfer_input(path: &Path, output: &mut dyn Write) -> Result<(u64, String), Error> {
    let mut file = open_regular(path)?;
    let expected = check_metadata(&file)?;
    transfer(&mut file, expected, output)
}

#[cfg(test)]
thread_local! {
    static TRANSFERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(super) fn take_transfer_count() -> usize {
    TRANSFERS.with(|count| count.replace(0))
}
