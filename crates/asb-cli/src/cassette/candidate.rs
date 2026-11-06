//! Execute one explicitly requested sampled-cassette qualification.
use super::{
    Archive, BoundTrialPreparation, Error, Limits, RecordingReceipt, ReplayAttempt, TrialOrder,
};
use crate::{benchmark::BenchmarkConfig, runner, tape};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
use std::path::Path;

/// Prepare and persist all replayable trials for one benchmark combination.
///
/// This operation records against the configured provider. The caller must
/// invoke it only from an explicit preparation action. Finished candidate
/// files remain under `artifact_root`; no existing candidate is overwritten.
pub fn prepare_sampled_group(
    ledger: &Path,
    artifact_root: &Path,
    template: &Path,
    invocation_dir: &Path,
    sweep_sha256: &str,
    order: TrialOrder,
    max_steps: Option<u64>,
) -> Result<BoundTrialPreparation, Error> {
    let text = crate::read_config(template).map_err(|error| Error(error.to_string()))?;
    let base: BenchmarkConfig = toml::from_str(&text)
        .map_err(|error| Error(format!("invalid sampled benchmark template: {error}")))?;
    let configuration_sha256 = configuration_digest(&text, max_steps);
    std::fs::create_dir_all(artifact_root)
        .map_err(|error| Error(format!("cannot create sampled artifact directory: {error}")))?;
    let root = artifact_root.canonicalize().map_err(|error| {
        Error(format!(
            "cannot resolve sampled artifact directory: {error}"
        ))
    })?;
    let state = BoundTrialPreparation::prepare_file(
        ledger,
        sweep_sha256,
        &configuration_sha256,
        order,
        |trial| {
            let directory = root.join(format!("candidate-{}", super::digest(trial.as_bytes())));
            std::fs::create_dir(&directory).map_err(|error| Error(format!(
            "cannot claim sampled candidate directory {}: {error}; preserve existing evidence", directory.display()
        )))?;
            let replay = replay_configuration(&base, trial, &directory)?;
            let as_string = |path: &Path| {
                path.to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| Error("sampled artifact path must be valid UTF-8".into()))
            };
            let mut record = replay.clone();
            record.concurrency = 1;
            record.workspace = as_string(&directory.join("record-workspace"))?;
            record.log_file = as_string(&directory.join("record.log"))?;
            if let Some(measurements) = record.measurements.as_mut() {
                measurements.directory = as_string(&directory.join("record-measurements"))?;
            }
            let record_path = directory.join("record.toml");
            let replay_path = directory.join("replay.toml");
            for (path, config) in [(&record_path, &record), (&replay_path, &replay)] {
                let bytes = toml::to_string_pretty(config)
                    .map_err(|error| Error(format!("cannot encode sampled benchmark: {error}")))?;
                if !crate::config::matrix::scalar_if_absent(path, bytes.as_bytes())
                    .map_err(|error| Error(format!("cannot publish sampled benchmark: {error}")))?
                {
                    return Err(Error("sampled benchmark configuration already exists; preserve it for inspection".into()));
                }
            }
            record_and_replay_candidate(
                &record_path,
                &replay_path,
                &directory.join("recording.asb.zip"),
                invocation_dir,
                max_steps,
            )
        },
    )?;
    verify_winners(&state, &base, &root, template, invocation_dir)?;
    Ok(state)
}

/// Reopen a completed sampled group and verify its winners without recording,
/// creating files, or connecting to a provider.
pub fn load_sampled_group(
    ledger: &Path,
    artifact_root: &Path,
    template: &Path,
    sweep_sha256: &str,
    order: &TrialOrder,
    max_steps: Option<u64>,
    invocation_dir: &Path,
) -> Result<BoundTrialPreparation, Error> {
    let text = crate::read_config(template).map_err(|error| Error(error.to_string()))?;
    let base: BenchmarkConfig = toml::from_str(&text)
        .map_err(|error| Error(format!("invalid sampled benchmark template: {error}")))?;
    let root = artifact_root.canonicalize().map_err(|error| {
        Error(format!(
            "cannot resolve sampled artifact directory: {error}"
        ))
    })?;
    let state = BoundTrialPreparation::load_completed_file(
        ledger,
        sweep_sha256,
        &configuration_digest(&text, max_steps),
        order,
    )?;
    verify_winners(&state, &base, &root, template, invocation_dir)?;
    Ok(state)
}

fn configuration_digest(text: &str, max_steps: Option<u64>) -> String {
    let mut hash = Sha256::new();
    hash.update(b"asb-sampled-configuration-v1\0");
    hash.update(text.as_bytes());
    hash.update(max_steps.unwrap_or(0).to_le_bytes());
    format!("{:x}", hash.finalize())
}

fn verify_winners(
    state: &BoundTrialPreparation,
    base: &BenchmarkConfig,
    root: &Path,
    template: &Path,
    invocation_dir: &Path,
) -> Result<(), Error> {
    let (project_root, settings) =
        crate::project::load_for_invocation(template, Some(invocation_dir))
            .map_err(|error| Error(error.to_string()))?;
    let workload_directory = crate::project::environment_path("ASB_WORKLOAD_DIR", invocation_dir)
        .unwrap_or_else(|| crate::project::resolve(&project_root, &settings.workloads_dir));
    let mut cached_catalog = None;
    // A completed ledger is not sufficient by itself: verify its winning
    // cassette and replay configuration still match before returning it.
    for (trial, expected_digest) in state.preparation().successful() {
        let directory = root.join(format!("candidate-{}", super::digest(trial.as_bytes())));
        let replay_text = crate::read_config(&directory.join("replay.toml")).map_err(|error| {
            Error(format!(
                "cannot read prepared replay configuration: {error}"
            ))
        })?;
        let actual: BenchmarkConfig = toml::from_str(&replay_text)
            .map_err(|error| Error(format!("invalid prepared replay configuration: {error}")))?;
        if actual != replay_configuration(base, trial, &directory)? {
            return Err(Error(
                "prepared replay configuration changed; preserve the existing evidence".into(),
            ));
        }
        let path = directory.join("recording.asb.zip");
        let (length, digest) = hash_cassette(&path)?;
        if digest != expected_digest {
            return Err(Error(
                "prepared cassette changed; preserve the existing evidence".into(),
            ));
        }
        let archive = Archive::open_verified(&path, length, expected_digest, Limits::default())?;
        let compatibility = &archive.manifest().compatibility;
        if !settings.replay.is_empty() || compatibility.normalization_sha256.is_some() {
            if cached_catalog.is_none() {
                cached_catalog = Some(
                    crate::workloads::planning_catalog(&actual.workload, &workload_directory)
                        .map_err(Error)?,
                );
            }
            let selection = crate::workloads::describe_selection(
                &actual,
                &workload_directory,
                cached_catalog.as_ref().and_then(Option::as_ref),
            )
            .map_err(Error)?;
            let policy = settings
                .replay
                .freeze_for_task(&project_root, &actual.agent, &selection)
                .map_err(Error)?;
            let current = (!policy.normalizers.is_empty()).then_some(policy.sha256.as_str());
            if current != compatibility.normalization_sha256.as_deref() {
                return Err(Error("prepared cassette normalization policy changed; preserve its evidence and run `asb setup` to create a new sweep, then run `asb sweep NEW_SWEEP.toml --prepare-replay`".into()));
            }
        }
        verify_current_protocol(compatibility)?;
    }
    Ok(())
}

// Readiness must not outlive a change in the replay protocol. Reconstruct the
// same static descriptor and default policy used by workload sessions. Dataset
// matching mirrors WorkloadSession::module_descriptor without launching a task.
fn verify_current_protocol(compatibility: &super::Compatibility) -> Result<(), Error> {
    let descriptor = crate::workloads::descriptor(&compatibility.workload);
    let module = match descriptor {
        Some(module)
            if module.support != crate::workloads::contracts::SupportLevel::ReferenceSmoke
                && crate::runner::batch::dataset_fingerprint(module.dataset.as_ref())
                    .map_err(|error| Error(error.to_string()))?
                    == compatibility.dataset_sha256 =>
        {
            Some(module)
        }
        _ => None,
    };
    let policy = crate::workloads::outcome::MetricPolicy::default();
    let base = crate::runner::batch::replay_protocol_fingerprint(module.as_ref(), &policy)
        .map_err(|error| Error(error.to_string()))?;
    let expected = crate::runner::batch::replay_protocol_with_normalizer_digest(
        &base,
        compatibility.normalization_sha256.as_deref(),
    )
    .map_err(|error| Error(error.to_string()))?;
    if compatibility.protocol_sha256.as_deref() != Some(expected.as_str()) {
        return Err(Error("prepared cassette uses an incompatible replay protocol; preserve its evidence and run `asb setup` to create a new sweep, then run `asb sweep NEW_SWEEP.toml --prepare-replay`".into()));
    }
    Ok(())
}

fn replay_configuration(
    base: &BenchmarkConfig,
    trial: &str,
    directory: &Path,
) -> Result<BenchmarkConfig, Error> {
    let as_string = |path: &Path| {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| Error("sampled artifact path must be valid UTF-8".into()))
    };
    let mut replay = base.clone();
    if replay.trial.is_some() {
        replay.trial = Some(trial.to_owned());
    }
    replay.workspace = as_string(&directory.join("replay-workspace"))?;
    replay.log_file = as_string(&directory.join("replay.log"))?;
    if let Some(measurements) = replay.measurements.as_mut() {
        measurements.directory = as_string(&directory.join("replay-measurements"))?;
    }
    Ok(replay)
}

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
    if record.concurrency != 1 || record.trial != replay.trial {
        return Err(Error(
            "sample recording and replay must select the same trial, with one recording replica"
                .into(),
        ));
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

#[cfg(test)]
mod protocol_tests {
    use super::*;
    #[test]
    fn current_protocol_is_required_for_prepared_cassette_reuse() {
        let module = crate::workloads::descriptor("swe-bench").unwrap();
        let dataset = crate::runner::batch::dataset_fingerprint(module.dataset.as_ref()).unwrap();
        let protocol = crate::runner::batch::replay_protocol_fingerprint(
            Some(&module),
            &crate::workloads::outcome::MetricPolicy::default(),
        )
        .unwrap();
        let mut identity = super::super::Compatibility {
            agent: "pi".into(),
            provider: "fixture".into(),
            model: "model".into(),
            workload: "swe-bench".into(),
            task: "task".into(),
            dataset_sha256: dataset,
            prompt_sha256: "a".repeat(64),
            material_sha256: "b".repeat(64),
            protocol_sha256: Some(protocol),
            normalization_sha256: None,
        };
        assert!(verify_current_protocol(&identity).is_ok());
        let original = identity.clone();
        identity.normalization_sha256 = Some("d".repeat(64));
        identity.protocol_sha256 = Some(
            crate::runner::batch::replay_protocol_with_normalizer_digest(
                original.protocol_sha256.as_deref().unwrap(),
                identity.normalization_sha256.as_deref(),
            )
            .unwrap(),
        );
        assert!(verify_current_protocol(&identity).is_ok());
        identity.normalization_sha256 = Some("e".repeat(64));
        assert!(verify_current_protocol(&identity).is_err());
        identity = original;
        identity.dataset_sha256 = Some("f".repeat(64));
        identity.protocol_sha256 = Some(
            crate::runner::batch::replay_protocol_fingerprint(
                None,
                &crate::workloads::outcome::MetricPolicy::default(),
            )
            .unwrap(),
        );
        assert!(verify_current_protocol(&identity).is_ok());
        identity.protocol_sha256 = Some("c".repeat(64));
        let error = verify_current_protocol(&identity).unwrap_err().to_string();
        assert!(error.contains("incompatible replay protocol"));
        assert!(error.contains("asb setup"));
        identity.protocol_sha256 = None;
        assert!(verify_current_protocol(&identity).is_err());
    }
}
