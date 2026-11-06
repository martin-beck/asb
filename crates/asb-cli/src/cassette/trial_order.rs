//! Reproducible sampling without replacement for workload-trial preparation.
use super::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::path::Path;

/// A reproducible candidate order, including spare trials for failed replays.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrialOrder {
    candidates: Vec<String>,
    target: usize,
}

/// A bounded, resumable per-combination qualification ledger.
///
/// The owner must verify and durably publish the referenced cassette before
/// marking a replay successful; this ledger does not itself verify archives.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialPreparation {
    schema_version: u8,
    target: usize,
    candidates: Vec<String>,
    outcomes: Vec<TrialOutcome>,
    in_flight: Option<String>,
}

/// A preparation ledger bound to immutable sweep and configuration inputs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BoundTrialPreparation {
    sweep_sha256: String,
    configuration_sha256: String,
    preparation: TrialPreparation,
}

/// One settled recording and its offline replay observations.
pub struct ReplayAttempt {
    /// SHA-256 of the immutable, independently verified recording.
    pub cassette_sha256: String,
    /// Observations from all replicas of the candidate replay.
    pub replicas: Vec<ReplayObservation>,
    /// Number of replay replicas required by the benchmark configuration.
    pub expected_replicas: usize,
}

/// A clean publication observation for one recorded candidate.
pub struct RecordingReceipt {
    /// Number of tracks published into the cassette.
    pub tracks: u64,
}

impl RecordingReceipt {
    /// Find one clean cassette-publication event for the requested output path.
    /// A workload grade is not used to decide whether a cassette was published.
    pub fn from_jsonl(output: &[u8], expected_path: &Path) -> Result<Self, Error> {
        if output.len() > 16 * 1024 * 1024 {
            return Err(Error("recording result exceeds 16 MiB".into()));
        }
        let mut receipt = None;
        for line in output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let value: serde_json::Value = serde_json::from_slice(line)
                .map_err(|_| Error("recording produced malformed JSON evidence".into()))?;
            if value.get("type").and_then(serde_json::Value::as_str) != Some("cassette-publication")
            {
                continue;
            }
            if receipt.is_some() {
                return Err(Error(
                    "recording produced multiple publication events".into(),
                ));
            }
            let publication = value
                .get("publication")
                .ok_or_else(|| Error("recording publication evidence is missing".into()))?;
            let path = publication
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| Error("recording publication path is missing".into()))?;
            let tracks = publication
                .get("tracks")
                .and_then(serde_json::Value::as_u64)
                .filter(|tracks| *tracks > 0)
                .ok_or_else(|| Error("recording publication has no tracks".into()))?;
            if Path::new(path) != expected_path
                || publication
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    != Some("published")
                || publication
                    .get("format")
                    .and_then(serde_json::Value::as_str)
                    != Some("asb-zip-v1")
                || publication
                    .get("cleanup_complete")
                    .and_then(serde_json::Value::as_bool)
                    != Some(true)
            {
                return Err(Error(
                    "recording publication or cleanup was not confirmed for the selected cassette"
                        .into(),
                ));
            }
            receipt = Some(Self { tracks });
        }
        receipt
            .ok_or_else(|| Error("recording did not produce a cassette publication receipt".into()))
    }
}

impl ReplayAttempt {
    /// Collect bounded replica summaries from an ASB JSONL replay result.
    /// The caller must independently hash and verify the published cassette.
    pub fn from_jsonl(
        cassette_sha256: String,
        output: &[u8],
        expected_replicas: usize,
    ) -> Result<Self, Error> {
        if output.len() > 16 * 1024 * 1024 || !super::valid_digest(&cassette_sha256) {
            return Err(Error(
                "replay result or cassette identity exceeds its bound or is invalid".into(),
            ));
        }
        if expected_replicas == 0 || expected_replicas > 64 {
            return Err(Error("invalid replay qualification replica count".into()));
        }
        let mut replicas = Vec::new();
        for line in output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let value: serde_json::Value = serde_json::from_slice(line)
                .map_err(|_| Error("replay produced malformed JSON evidence".into()))?;
            if value.get("task").is_some_and(serde_json::Value::is_object) {
                if replicas.len() >= expected_replicas {
                    return Err(Error("replay produced too many replica summaries".into()));
                }
                replicas.push(ReplayObservation::from_summary(&value)?);
            }
        }
        if replicas.len() != expected_replicas {
            return Err(Error(
                "replay did not produce every expected replica summary".into(),
            ));
        }
        Ok(Self {
            cassette_sha256,
            replicas,
            expected_replicas,
        })
    }
}

impl BoundTrialPreparation {
    /// Maximum size of a serialized per-combination ledger.
    pub const MAX_BYTES: usize = 32 * 1024 * 1024;

    /// Bind a new ledger to canonical digests of the sweep and one combination.
    pub fn new(
        sweep_sha256: &str,
        configuration_sha256: &str,
        order: TrialOrder,
    ) -> Result<Self, Error> {
        if !super::valid_digest(sweep_sha256) || !super::valid_digest(configuration_sha256) {
            return Err(Error(
                "sample preparation requires valid sweep and configuration digests".into(),
            ));
        }
        Ok(Self {
            sweep_sha256: sweep_sha256.into(),
            configuration_sha256: configuration_sha256.into(),
            preparation: TrialPreparation::new(order),
        })
    }

    /// Validate persisted state against the current immutable inputs and order.
    pub fn validate(
        &self,
        sweep_sha256: &str,
        configuration_sha256: &str,
        order: &TrialOrder,
    ) -> Result<(), Error> {
        if !super::valid_digest(sweep_sha256)
            || !super::valid_digest(configuration_sha256)
            || self.sweep_sha256 != sweep_sha256
            || self.configuration_sha256 != configuration_sha256
            || self.preparation.target != order.target
            || self.preparation.candidates != order.candidates
        {
            return Err(Error("sample preparation inputs or candidate order changed; preserve existing evidence and create a new sweep".into()));
        }
        self.preparation.validate()
    }

    /// Encode validated state before durable publication by its locked owner.
    pub fn encode(
        &self,
        sweep_sha256: &str,
        configuration_sha256: &str,
        order: &TrialOrder,
    ) -> Result<Vec<u8>, Error> {
        self.validate(sweep_sha256, configuration_sha256, order)?;
        let mut output = BoundedLedger(Vec::new());
        serde_json::to_writer(&mut output, self)
            .map_err(|_| Error("cannot encode sample preparation ledger within 32 MiB".into()))?;
        Ok(output.0)
    }

    /// Decode and validate a bounded saved ledger against its current inputs.
    pub fn decode(
        bytes: &[u8],
        sweep_sha256: &str,
        configuration_sha256: &str,
        order: &TrialOrder,
    ) -> Result<Self, Error> {
        if bytes.len() > Self::MAX_BYTES {
            return Err(Error("sample preparation ledger exceeds 32 MiB".into()));
        }
        let value: Self = serde_json::from_slice(bytes)
            .map_err(|_| Error("invalid sample preparation ledger".into()))?;
        value.validate(sweep_sha256, configuration_sha256, order)?;
        Ok(value)
    }

    /// Access the underlying ledger after validating its bindings.
    pub fn preparation(&self) -> &TrialPreparation {
        &self.preparation
    }

    /// Modify the ledger after validating its bindings while holding its owner lock.
    pub fn preparation_mut(&mut self) -> &mut TrialPreparation {
        &mut self.preparation
    }

    /// Prepare candidates in order, persisting ownership and every settlement.
    ///
    /// The caller owns the ledger lock. `persist` must durably publish its bytes;
    /// `execute` must record the trial, independently verify the cassette, then
    /// replay it offline and return all observed replica facts. A callback error
    /// leaves the candidate in flight and does not launch a replacement.
    pub fn prepare<P, E>(
        &mut self,
        sweep_sha256: &str,
        configuration_sha256: &str,
        order: &TrialOrder,
        mut persist: P,
        mut execute: E,
    ) -> Result<(), Error>
    where
        P: FnMut(&[u8]) -> Result<(), Error>,
        E: FnMut(&str) -> Result<ReplayAttempt, Error>,
    {
        self.validate(sweep_sha256, configuration_sha256, order)?;
        loop {
            if self.preparation.complete()? {
                return Ok(());
            }
            let trial = self.preparation.start()?.ok_or_else(|| {
                Error(
                    "sampled trial pool exhausted before enough recordings replayed successfully"
                        .into(),
                )
            })?;
            persist(&self.encode(sweep_sha256, configuration_sha256, order)?)?;
            let attempt = execute(&trial)?;
            self.preparation.settle_replay(
                &trial,
                attempt.cassette_sha256,
                &attempt.replicas,
                attempt.expected_replicas,
            )?;
            persist(&self.encode(sweep_sha256, configuration_sha256, order)?)?;
        }
    }

    /// Lock and durably update a per-combination preparation ledger.
    ///
    /// The parent directory must already exist. Each candidate is published as
    /// in-flight before `execute` runs; an interrupted candidate is retained and
    /// never silently retried. A completed matching ledger starts no processes.
    pub fn prepare_file<E>(
        path: &Path,
        sweep_sha256: &str,
        configuration_sha256: &str,
        order: TrialOrder,
        execute: E,
    ) -> Result<Self, Error>
    where
        E: FnMut(&str) -> Result<ReplayAttempt, Error>,
    {
        let lock_path = ledger_lock_path(path)?;
        let _lock = crate::output_lock::OutputLock::acquire(
            &lock_path,
            "another process is preparing this sampled benchmark combination",
        )
        .map_err(Error)?;
        let mut state = match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => Self::decode(
                &read_ledger(path)?,
                sweep_sha256,
                configuration_sha256,
                &order,
            )?,
            Ok(_) => {
                return Err(Error(
                    "sample ledger must be a regular file, not a symlink or directory".into(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let state = Self::new(sweep_sha256, configuration_sha256, order.clone())?;
                let bytes = state.encode(sweep_sha256, configuration_sha256, &order)?;
                let published = crate::config::matrix::scalar_if_absent(path, &bytes)
                    .map_err(|error| Error(format!("cannot publish sample ledger: {error}")))?;
                if !published {
                    return Err(Error("sample ledger appeared while it was being initialized; inspect it before retrying".into()));
                }
                state
            }
            Err(error) => return Err(Error(format!("cannot inspect sample ledger: {error}"))),
        };
        state.prepare(
            sweep_sha256,
            configuration_sha256,
            &order,
            |bytes| {
                crate::config::matrix::publish_atomic(path, bytes)
                    .map_err(|error| Error(format!("cannot persist sample ledger: {error}")))
            },
            execute,
        )?;
        Ok(state)
    }

    /// Read a completed ledger under its existing lock without recording or
    /// creating files. Incomplete or changed state cannot authorize replay.
    pub fn load_completed_file(
        path: &Path,
        sweep_sha256: &str,
        configuration_sha256: &str,
        order: &TrialOrder,
    ) -> Result<Self, Error> {
        let _lock = crate::output_lock::OutputLock::acquire_existing(
            &ledger_lock_path(path)?,
            "another process is preparing this sampled benchmark combination",
        )
        .map_err(Error)?;
        let state = Self::decode(
            &read_ledger(path)?,
            sweep_sha256,
            configuration_sha256,
            order,
        )?;
        if !state.preparation.complete()? {
            return Err(Error(
                "sample preparation is incomplete; run asb sweep --prepare-replay first".into(),
            ));
        }
        Ok(state)
    }
}

fn ledger_lock_path(path: &Path) -> Result<std::path::PathBuf, Error> {
    let mut name = path
        .file_name()
        .ok_or_else(|| Error("sample ledger needs a file name".into()))?
        .to_os_string();
    name.push(".lock");
    Ok(path.with_file_name(name))
}

fn read_ledger(path: &Path) -> Result<Vec<u8>, Error> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| Error(format!("cannot inspect sample ledger: {error}")))?;
    if !metadata.file_type().is_file() {
        return Err(Error(
            "sample ledger must be a regular file, not a symlink or directory".into(),
        ));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|error| Error(format!("cannot open sample ledger: {error}")))?;
    if !file
        .metadata()
        .map_err(|error| Error(format!("cannot inspect sample ledger: {error}")))?
        .is_file()
    {
        return Err(Error("sample ledger must be a regular file".into()));
    }
    let mut bytes = Vec::new();
    file.take((BoundTrialPreparation::MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| Error(format!("cannot read sample ledger: {error}")))?;
    Ok(bytes)
}

struct BoundedLedger(Vec<u8>);
impl Write for BoundedLedger {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > BoundTrialPreparation::MAX_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("sample preparation ledger exceeds 32 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct TrialOutcome {
    trial: String,
    /// A content digest of an independently verified cassette, when qualified.
    cassette_sha256: Option<String>,
}

/// Observed replay and cleanup facts for one execution replica.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayObservation {
    /// One-based replica index in this replay.
    pub replica: usize,
    /// The workload trial actually executed.
    pub trial: String,
    /// Whether the agent process completed successfully.
    pub process_success: bool,
    /// Whether the provider proxy confirmed complete replay consumption.
    pub provider_consumed: Option<bool>,
    /// Native replay consumption when that layer reports it.
    pub native_consumed: Option<bool>,
    /// Whether this execution selected native replay requiring a receipt.
    pub native_required: bool,
    /// Whether owned workload, proxy and provider resources settled.
    pub cleanup_confirmed: Option<bool>,
}

impl ReplayObservation {
    /// Extract qualification facts from one ASB JSON run summary.
    /// Unknown or malformed evidence is never converted into success.
    pub fn from_summary(summary: &serde_json::Value) -> Result<Self, Error> {
        let invalid = || {
            Error("replay result lacks valid trial, replica or provider-selection evidence".into())
        };
        let trial = summary
            .pointer("/task/task_id")
            .and_then(serde_json::Value::as_str)
            .filter(|trial| !trial.is_empty())
            .ok_or_else(invalid)?
            .to_owned();
        let replica = summary
            .get("replica_index")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(invalid)?;
        let selection = summary.get("replay_selection").ok_or_else(invalid)?;
        if selection
            .get("provider_replay")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Err(invalid());
        }
        let tools = selection
            .get("tools")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(invalid)?;
        if !matches!(
            tools,
            "not-selected" | "pi-adapter-local" | "native-snapshot" | "other-selected"
        ) {
            return Err(invalid());
        }
        let process_success = summary
            .get("process_success")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(invalid)?;
        let failure = summary.get("failure_kind").ok_or_else(invalid)?;
        let operational_success = if failure.is_null() {
            true
        } else {
            match failure.as_str().ok_or_else(invalid)? {
                "workload_check_failed" | "grader_error" | "collector_error" => true,
                "run_error"
                | "agent_failure"
                | "recording_error"
                | "output_error"
                | "provider_cleanup_error" => false,
                _ => return Err(invalid()),
            }
        };
        Ok(Self {
            trial,
            replica,
            process_success: process_success && operational_success,
            provider_consumed: summary
                .get("provider_replay_consumed")
                .and_then(serde_json::Value::as_bool),
            native_consumed: summary
                .get("native_replay_consumed")
                .and_then(serde_json::Value::as_bool),
            native_required: tools == "native-snapshot",
            cleanup_confirmed: summary
                .pointer("/benchmark_outcome/cleanup_confirmed")
                .and_then(serde_json::Value::as_bool),
        })
    }
}

impl TrialPreparation {
    /// Begin qualification with a precomputed complete candidate order.
    pub fn new(order: TrialOrder) -> Self {
        Self {
            schema_version: 1,
            target: order.target,
            candidates: order.candidates,
            outcomes: Vec::new(),
            in_flight: None,
        }
    }

    /// Validate loaded state before it can select or reuse any recording.
    pub fn validate(&self) -> Result<(), Error> {
        if self.schema_version != 1
            || self.target == 0
            || self.target > self.candidates.len()
            || self.candidates.len() > 65_536
            || self.outcomes.len() > self.candidates.len()
        {
            return Err(Error(
                "invalid trial preparation population or progress".into(),
            ));
        }
        let mut seen = BTreeSet::new();
        let mut bytes = 0usize;
        for id in &self.candidates {
            bytes = bytes
                .checked_add(id.len())
                .filter(|n| *n <= 16 * 1024 * 1024)
                .ok_or_else(|| Error("trial preparation identifiers exceed 16 MiB".into()))?;
            if id.is_empty()
                || id.len() > 4096
                || id.chars().any(char::is_control)
                || !seen.insert(id)
            {
                return Err(Error(
                    "invalid or duplicate trial preparation candidate".into(),
                ));
            }
        }
        for (index, outcome) in self.outcomes.iter().enumerate() {
            if outcome.trial != self.candidates[index]
                || outcome.cassette_sha256.as_ref().is_some_and(|digest| {
                    digest.len() != 64
                        || !digest
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
            {
                return Err(Error(
                    "invalid trial preparation outcome or cassette digest".into(),
                ));
            }
        }
        let successes = self
            .outcomes
            .iter()
            .filter(|outcome| outcome.cassette_sha256.is_some())
            .count();
        if successes > self.target
            || (successes == self.target
                && self
                    .outcomes
                    .last()
                    .is_some_and(|outcome| outcome.cassette_sha256.is_none()))
        {
            return Err(Error(
                "trial preparation contains outcomes after completion".into(),
            ));
        }
        if let Some(trial) = &self.in_flight
            && (successes == self.target || self.candidates.get(self.outcomes.len()) != Some(trial))
        {
            return Err(Error(
                "trial preparation has invalid in-flight ownership".into(),
            ));
        }
        Ok(())
    }

    /// Next candidate, or none once the target is met or the pool is exhausted.
    pub fn next(&self) -> Result<Option<&str>, Error> {
        self.validate()?;
        if self.in_flight.is_some() {
            return Err(Error("trial preparation has an unsettled attempt; inspect its retained evidence before continuing".into()));
        }
        if self.successful().len() == self.target {
            return Ok(None);
        }
        Ok(self.candidates.get(self.outcomes.len()).map(String::as_str))
    }

    /// Return a previously started candidate that needs explicit settlement.
    pub fn in_flight(&self) -> Result<Option<&str>, Error> {
        self.validate()?;
        Ok(self.in_flight.as_deref())
    }

    /// Mark the next candidate as in-flight before starting its recording.
    /// The owner must persist this transition before starting any process.
    pub fn start(&mut self) -> Result<Option<String>, Error> {
        let next = self.next()?.map(str::to_owned);
        self.in_flight = next.clone();
        Ok(next)
    }

    /// Record one outcome after the in-flight attempt has settled.
    /// A digest means recording and offline replay both succeeded; `None`
    /// marks a failed candidate and advances to a different trial.
    fn settle(&mut self, trial: &str, cassette_sha256: Option<String>) -> Result<(), Error> {
        self.validate()?;
        if self.in_flight.as_deref() != Some(trial) {
            return Err(Error(
                "trial outcome is not the next preparation candidate".into(),
            ));
        }
        let outcome = TrialOutcome {
            trial: trial.into(),
            cassette_sha256,
        };
        self.in_flight = None;
        self.outcomes.push(outcome);
        if let Err(error) = self.validate() {
            self.outcomes.pop();
            self.in_flight = Some(trial.into());
            return Err(error);
        }
        Ok(())
    }

    /// Settle a replay attempt after verifying the immutable cassette bytes.
    ///
    /// A false workload grade is deliberately irrelevant. An explicit failed
    /// replay advances to a different candidate only after cleanup is confirmed.
    /// Missing evidence leaves the attempt in flight for explicit recovery.
    pub fn settle_replay(
        &mut self,
        trial: &str,
        cassette_sha256: String,
        replicas: &[ReplayObservation],
        expected_replicas: usize,
    ) -> Result<bool, Error> {
        if expected_replicas == 0 || expected_replicas > 64 || replicas.len() != expected_replicas {
            return Err(Error(
                "replay qualification lacks the expected replica observations".into(),
            ));
        }
        let mut qualified = true;
        let mut seen = BTreeSet::new();
        for replica in replicas {
            if !(1..=expected_replicas).contains(&replica.replica)
                || !seen.insert(replica.replica)
                || replica.trial != trial
                || replica.cleanup_confirmed != Some(true)
            {
                return Err(Error(
                    "replay trial identity or resource cleanup is unconfirmed".into(),
                ));
            }
            let consumed = replica.provider_consumed.ok_or_else(|| {
                Error("provider replay consumption was not observed; retain the attempt for inspection".into())
            })?;
            if replica.native_required && replica.native_consumed.is_none() {
                return Err(Error(
                    "native replay consumption was not observed; retain the attempt for inspection"
                        .into(),
                ));
            }
            qualified &=
                replica.process_success && consumed && replica.native_consumed != Some(false);
        }
        self.settle(trial, qualified.then_some(cassette_sha256))?;
        Ok(qualified)
    }

    /// Verified-cassette identities in their original sampled order.
    pub fn successful(&self) -> Vec<(&str, &str)> {
        self.outcomes
            .iter()
            .filter_map(|outcome| {
                outcome
                    .cassette_sha256
                    .as_deref()
                    .map(|digest| (outcome.trial.as_str(), digest))
            })
            .collect()
    }

    /// Whether the requested number of successful trials was reached.
    pub fn complete(&self) -> Result<bool, Error> {
        self.validate()?;
        Ok(self.successful().len() == self.target)
    }
}

impl TrialOrder {
    /// Shuffle a bounded catalog without replacement using the supplied seed.
    ///
    /// Catalog ordering does not affect the result. Duplicate or malformed IDs
    /// and impossible targets are rejected rather than silently narrowed.
    pub fn new(ids: &[String], target: usize, seed: u64) -> Result<Self, Error> {
        if ids.is_empty() || ids.len() > 65_536 || target == 0 || target > ids.len() {
            return Err(Error(
                "trial sample must be between one and the bounded catalog size".into(),
            ));
        }
        let mut unique = BTreeSet::new();
        let mut bytes = 0usize;
        for id in ids {
            bytes = bytes
                .checked_add(id.len())
                .filter(|total| *total <= 16 * 1024 * 1024)
                .ok_or_else(|| Error("trial catalog identifiers exceed 16 MiB".into()))?;
            if id.is_empty()
                || id.len() > 4096
                || id.chars().any(char::is_control)
                || !unique.insert(id.clone())
            {
                return Err(Error(
                    "trial catalog contains a malformed or duplicate identifier".into(),
                ));
            }
        }
        let mut candidates: Vec<_> = unique.into_iter().collect();
        let mut counter = 0u64;
        for end in (1..candidates.len()).rev() {
            let width = (end + 1) as u64;
            let threshold = width.wrapping_neg() % width;
            let index = loop {
                let mut hash = Sha256::new();
                hash.update(b"asb-trial-shuffle-sha256-v1\0");
                hash.update(seed.to_le_bytes());
                hash.update(counter.to_le_bytes());
                counter = counter
                    .checked_add(1)
                    .ok_or_else(|| Error("trial selection counter exhausted".into()))?;
                let digest = hash.finalize();
                let draw =
                    u64::from_le_bytes(digest[..8].try_into().expect("SHA-256 has eight bytes"));
                if draw >= threshold {
                    break (draw % width) as usize;
                }
            };
            candidates.swap(end, index);
        }
        Ok(Self { candidates, target })
    }

    /// First candidates to prepare initially.
    pub fn initial(&self) -> &[String] {
        &self.candidates[..self.target]
    }

    /// Complete ordered candidate pool, including fallback trials.
    pub fn candidates(&self) -> &[String] {
        &self.candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_order_has_unique_initial_and_replacement_candidates() {
        let ids: Vec<_> = (0..100).map(|n| format!("trial-{n:03}")).collect();
        let order = TrialOrder::new(&ids, 7, 42).unwrap();
        assert_eq!(
            order.initial(),
            &[
                "trial-057",
                "trial-095",
                "trial-094",
                "trial-065",
                "trial-031",
                "trial-093",
                "trial-070"
            ]
        );
        let mut reversed = ids.clone();
        reversed.reverse();
        assert_eq!(order, TrialOrder::new(&reversed, 7, 42).unwrap());
        assert_eq!(order.initial().len(), 7);
        assert_eq!(
            order.candidates().iter().collect::<BTreeSet<_>>().len(),
            100
        );
        assert_ne!(order, TrialOrder::new(&ids, 7, 43).unwrap());
    }

    #[test]
    fn rejects_impossible_and_invalid_catalogs() {
        let ids = vec!["one".into(), "two".into()];
        assert!(TrialOrder::new(&ids, 0, 1).is_err());
        assert!(TrialOrder::new(&ids, 3, 1).is_err());
        assert!(TrialOrder::new(&["one".into(), "one".into()], 1, 1).is_err());
        assert!(TrialOrder::new(&["bad\nid".into()], 1, 1).is_err());
    }

    #[test]
    fn failed_replay_uses_a_distinct_replacement_and_recovers_successes() {
        let ids = (0..4).map(|n| format!("trial-{n}")).collect::<Vec<_>>();
        let mut state = TrialPreparation::new(TrialOrder::new(&ids, 2, 4).unwrap());
        let first = state.start().unwrap().unwrap();
        state.settle(&first, None).unwrap();
        let second = state.start().unwrap().unwrap();
        assert_ne!(first, second);
        state.settle(&second, Some("a".repeat(64))).unwrap();
        let serialized = serde_json::to_string(&state).unwrap();
        let mut restored: TrialPreparation = serde_json::from_str(&serialized).unwrap();
        let third = restored.start().unwrap().unwrap();
        assert_ne!(first, third);
        assert_ne!(second, third);
        restored.settle(&third, Some("b".repeat(64))).unwrap();
        assert!(restored.complete().unwrap());
        assert_eq!(restored.next().unwrap(), None);
        assert_eq!(
            restored.successful(),
            vec![
                (second.as_str(), "a".repeat(64).as_str()),
                (third.as_str(), "b".repeat(64).as_str())
            ]
        );
        assert!(restored.settle(&third, None).is_err());
    }

    #[test]
    fn exhausted_pool_and_tampered_receipts_do_not_qualify() {
        let ids = vec!["a".into(), "b".into()];
        let mut state = TrialPreparation::new(TrialOrder::new(&ids, 2, 9).unwrap());
        let first = state.start().unwrap().unwrap();
        assert!(state.settle(&first, Some("bad-digest".into())).is_err());
        assert!(state.next().is_err());
        state.settle(&first, None).unwrap();
        let second = state.start().unwrap().unwrap();
        state.settle(&second, None).unwrap();
        assert_eq!(state.next().unwrap(), None);
        assert!(!state.complete().unwrap());
        let mut bad = state.clone();
        bad.outcomes[0].trial = "unrelated".into();
        assert!(bad.validate().is_err());
        bad = state.clone();
        bad.schema_version = 2;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn interrupted_attempt_cannot_silently_start_another_trial() {
        let ids = vec!["a".into(), "b".into()];
        let mut state = TrialPreparation::new(TrialOrder::new(&ids, 1, 3).unwrap());
        let started = state.start().unwrap().unwrap();
        let encoded = serde_json::to_vec(&state).unwrap();
        let mut restored: TrialPreparation = serde_json::from_slice(&encoded).unwrap();
        assert!(restored.start().is_err());
        assert_eq!(restored.in_flight().unwrap(), Some(started.as_str()));
        assert!(!restored.complete().unwrap());
        restored.settle(&started, None).unwrap();
        assert_ne!(restored.start().unwrap().unwrap(), started);
    }

    #[test]
    fn qualification_requires_distinct_consumed_replicas_and_confirmed_cleanup() {
        let ids = vec!["a".into(), "b".into(), "c".into()];
        let mut state = TrialPreparation::new(TrialOrder::new(&ids, 1, 7).unwrap());
        let first = state.start().unwrap().unwrap();
        let observed = |replica, consumed, cleanup| ReplayObservation {
            replica,
            trial: first.clone(),
            process_success: true,
            provider_consumed: consumed,
            native_consumed: None,
            native_required: false,
            cleanup_confirmed: cleanup,
        };
        let digest = "a".repeat(64);
        let duplicate = [
            observed(1, Some(true), Some(true)),
            observed(1, Some(true), Some(true)),
        ];
        assert!(
            state
                .settle_replay(&first, digest.clone(), &duplicate, 2)
                .is_err()
        );
        let missing = [observed(1, None, Some(true))];
        assert!(
            state
                .settle_replay(&first, digest.clone(), &missing, 1)
                .is_err()
        );
        let unclean = [observed(1, Some(false), None)];
        assert!(
            state
                .settle_replay(&first, digest.clone(), &unclean, 1)
                .is_err()
        );
        let mut missing_native = observed(1, Some(true), Some(true));
        missing_native.native_required = true;
        assert!(
            state
                .settle_replay(&first, digest.clone(), &[missing_native], 1)
                .is_err()
        );
        assert_eq!(state.in_flight().unwrap(), Some(first.as_str()));
        let failed = [observed(1, Some(false), Some(true))];
        assert!(
            !state
                .settle_replay(&first, digest.clone(), &failed, 1)
                .unwrap()
        );
        let next = state.start().unwrap().unwrap();
        assert_ne!(first, next);
        let success = [ReplayObservation {
            replica: 1,
            trial: next.clone(),
            process_success: true,
            provider_consumed: Some(true),
            native_consumed: None,
            native_required: false,
            cleanup_confirmed: Some(true),
        }];
        assert!(state.settle_replay(&next, digest, &success, 1).unwrap());
        assert!(state.complete().unwrap());
    }

    #[test]
    fn summary_keeps_replay_evidence_separate_from_the_workload_grade() {
        let mut summary = serde_json::json!({
            "task": {"task_id": "case"}, "replica_index": 1,
            "process_success": true, "workload_success": false,
            "failure_kind": "workload_check_failed",
            "replay_selection": {"provider_replay": true, "tools": "not-selected"},
            "provider_replay_consumed": true,
            "benchmark_outcome": {"cleanup_confirmed": true}
        });
        let observation = ReplayObservation::from_summary(&summary).unwrap();
        assert!(observation.process_success);
        assert_eq!(observation.provider_consumed, Some(true));
        summary["failure_kind"] = serde_json::json!("run_error");
        assert!(
            !ReplayObservation::from_summary(&summary)
                .unwrap()
                .process_success
        );
        summary["replay_selection"]["provider_replay"] = serde_json::json!(false);
        assert!(ReplayObservation::from_summary(&summary).is_err());
        summary["replay_selection"]["provider_replay"] = serde_json::json!(true);
        summary.as_object_mut().unwrap().remove("failure_kind");
        assert!(ReplayObservation::from_summary(&summary).is_err());
    }

    #[test]
    fn replay_attempt_rejects_missing_or_malformed_replica_output() {
        let digest = "a".repeat(64);
        assert!(ReplayAttempt::from_jsonl(digest.clone(), b"{}\n", 1).is_err());
        assert!(ReplayAttempt::from_jsonl(digest.clone(), b"not-json\n", 1).is_err());
        assert!(ReplayAttempt::from_jsonl("bad".into(), b"", 1).is_err());
        assert!(ReplayAttempt::from_jsonl(digest, b"", 0).is_err());
    }

    #[test]
    fn recording_receipt_requires_matching_published_cassette_and_cleanup() {
        let path = Path::new("sample.asb.zip");
        let mut value = serde_json::json!({"type":"cassette-publication", "publication": {
            "path":"sample.asb.zip", "status":"published", "format":"asb-zip-v1",
            "tracks":1, "cleanup_complete":true
        }});
        let bytes = |value: &serde_json::Value| serde_json::to_vec(value).unwrap();
        assert_eq!(
            RecordingReceipt::from_jsonl(&bytes(&value), path)
                .unwrap()
                .tracks,
            1
        );
        assert!(RecordingReceipt::from_jsonl(&bytes(&value), Path::new("other.zip")).is_err());
        value["publication"]["cleanup_complete"] = serde_json::json!(false);
        assert!(RecordingReceipt::from_jsonl(&bytes(&value), path).is_err());
        value["publication"]["cleanup_complete"] = serde_json::json!(true);
        value["publication"]["tracks"] = serde_json::json!(0);
        assert!(RecordingReceipt::from_jsonl(&bytes(&value), path).is_err());
        assert!(RecordingReceipt::from_jsonl(b"{}\n", path).is_err());
    }

    #[test]
    fn persisted_ledger_refuses_changed_sweep_configuration_or_catalog() {
        let ids = vec!["one".into(), "two".into(), "three".into()];
        let order = TrialOrder::new(&ids, 1, 42).unwrap();
        let sweep = "a".repeat(64);
        let config = "b".repeat(64);
        let mut bound = BoundTrialPreparation::new(&sweep, &config, order.clone()).unwrap();
        let started = bound.preparation_mut().start().unwrap().unwrap();
        let bytes = bound.encode(&sweep, &config, &order).unwrap();
        let restored = BoundTrialPreparation::decode(&bytes, &sweep, &config, &order).unwrap();
        restored.validate(&sweep, &config, &order).unwrap();
        assert_eq!(
            restored.preparation().in_flight().unwrap(),
            Some(started.as_str())
        );
        assert!(restored.validate(&"c".repeat(64), &config, &order).is_err());
        assert!(restored.validate(&sweep, &"d".repeat(64), &order).is_err());
        assert!(
            restored
                .validate(&sweep, &config, &TrialOrder::new(&ids, 2, 42).unwrap())
                .is_err()
        );
        assert!(BoundTrialPreparation::new("bad", &config, order).is_err());
    }

    #[test]
    fn preparation_runs_replacements_and_reuses_persisted_successes() {
        use std::cell::RefCell;
        let ids = (0..5)
            .map(|index| format!("case-{index}"))
            .collect::<Vec<_>>();
        let order = TrialOrder::new(&ids, 2, 42).unwrap();
        let sweep = "a".repeat(64);
        let config = "b".repeat(64);
        let mut state = BoundTrialPreparation::new(&sweep, &config, order.clone()).unwrap();
        let saved = RefCell::new(Vec::new());
        let seen = RefCell::new(Vec::new());
        state
            .prepare(
                &sweep,
                &config,
                &order,
                |bytes| {
                    *saved.borrow_mut() = bytes.to_vec();
                    Ok(())
                },
                |trial| {
                    let mut seen = seen.borrow_mut();
                    seen.push(trial.to_owned());
                    Ok(ReplayAttempt {
                        cassette_sha256: "c".repeat(64),
                        expected_replicas: 1,
                        replicas: vec![ReplayObservation {
                            trial: trial.into(),
                            replica: 1,
                            process_success: true,
                            provider_consumed: Some(seen.len() != 1),
                            native_consumed: None,
                            native_required: false,
                            cleanup_confirmed: Some(true),
                        }],
                    })
                },
            )
            .unwrap();
        assert_eq!(seen.borrow().len(), 3);
        assert_eq!(state.preparation().successful().len(), 2);
        let mut restored =
            BoundTrialPreparation::decode(&saved.borrow(), &sweep, &config, &order).unwrap();
        restored
            .prepare(
                &sweep,
                &config,
                &order,
                |_| panic!("completed state must not be rewritten"),
                |_| panic!("completed state must not launch another recording"),
            )
            .unwrap();
    }

    #[test]
    fn preparation_persists_ownership_before_execution_and_stops_on_uncertainty() {
        use std::cell::RefCell;
        let order = TrialOrder::new(&["one".into(), "two".into()], 1, 1).unwrap();
        let sweep = "a".repeat(64);
        let config = "b".repeat(64);
        let mut state = BoundTrialPreparation::new(&sweep, &config, order.clone()).unwrap();
        let saved = RefCell::new(Vec::new());
        let result = state.prepare(
            &sweep,
            &config,
            &order,
            |bytes| {
                *saved.borrow_mut() = bytes.to_vec();
                Ok(())
            },
            |_| Err(Error("unconfirmed process cleanup".into())),
        );
        assert!(result.is_err());
        let mut restored =
            BoundTrialPreparation::decode(&saved.borrow(), &sweep, &config, &order).unwrap();
        assert!(restored.preparation().in_flight().unwrap().is_some());
        assert!(
            restored
                .prepare(
                    &sweep,
                    &config,
                    &order,
                    |_| Ok(()),
                    |_| panic!("must not retry an unsettled attempt")
                )
                .is_err()
        );
    }

    #[test]
    fn preparation_stops_on_exhaustion_and_never_executes_after_failed_persist() {
        let order = TrialOrder::new(&["one".into(), "two".into()], 1, 1).unwrap();
        let sweep = "a".repeat(64);
        let config = "b".repeat(64);
        let mut state = BoundTrialPreparation::new(&sweep, &config, order.clone()).unwrap();
        let error = state
            .prepare(
                &sweep,
                &config,
                &order,
                |_| Ok(()),
                |trial| {
                    Ok(ReplayAttempt {
                        cassette_sha256: "c".repeat(64),
                        expected_replicas: 1,
                        replicas: vec![ReplayObservation {
                            trial: trial.into(),
                            replica: 1,
                            process_success: true,
                            provider_consumed: Some(false),
                            native_consumed: None,
                            native_required: false,
                            cleanup_confirmed: Some(true),
                        }],
                    })
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("pool exhausted"));
        assert!(!state.preparation().complete().unwrap());
        let mut fresh = BoundTrialPreparation::new(&sweep, &config, order.clone()).unwrap();
        assert!(
            fresh
                .prepare(
                    &sweep,
                    &config,
                    &order,
                    |_| Err(Error("disk full".into())),
                    |_| panic!("must persist ownership before starting a process")
                )
                .is_err()
        );
    }

    #[test]
    fn file_backed_preparation_is_locked_and_reuses_completed_trials() {
        let root = std::env::temp_dir().join(format!(
            "asb-sampled-ledger-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("ledger.json");
        let order = TrialOrder::new(&["one".into(), "two".into(), "three".into()], 1, 1).unwrap();
        let sweep = "a".repeat(64);
        let config = "b".repeat(64);
        let mut attempts = 0;
        let state =
            BoundTrialPreparation::prepare_file(&path, &sweep, &config, order.clone(), |trial| {
                attempts += 1;
                let contention = BoundTrialPreparation::prepare_file(
                    &path,
                    &sweep,
                    &config,
                    order.clone(),
                    |_| panic!("a second preparation must not execute"),
                );
                assert!(contention.is_err());
                Ok(ReplayAttempt {
                    cassette_sha256: "c".repeat(64),
                    expected_replicas: 1,
                    replicas: vec![ReplayObservation {
                        trial: trial.into(),
                        replica: 1,
                        process_success: true,
                        provider_consumed: Some(attempts > 1),
                        native_consumed: None,
                        native_required: false,
                        cleanup_confirmed: Some(true),
                    }],
                })
            })
            .unwrap();
        assert_eq!(attempts, 2);
        assert!(state.preparation().complete().unwrap());
        let restored =
            BoundTrialPreparation::prepare_file(&path, &sweep, &config, order.clone(), |_| {
                panic!("completed preparation must not execute again")
            })
            .unwrap();
        assert_eq!(state, restored);
        let loaded =
            BoundTrialPreparation::load_completed_file(&path, &sweep, &config, &order).unwrap();
        assert_eq!(state, loaded);
        let missing = root.join("missing.json");
        assert!(
            BoundTrialPreparation::load_completed_file(&missing, &sweep, &config, &order).is_err()
        );
        assert!(!missing.exists());
        assert!(!missing.with_file_name("missing.json.lock").exists());
        assert!(
            BoundTrialPreparation::prepare_file(&path, &"d".repeat(64), &config, order, |_| {
                panic!("changed input must not execute")
            })
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_backed_interrupted_attempt_is_not_automatically_restarted() {
        let root = std::env::temp_dir().join(format!(
            "asb-sampled-interrupted-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("ledger.json");
        let order = TrialOrder::new(&["one".into(), "two".into()], 1, 2).unwrap();
        let sweep = "a".repeat(64);
        let config = "b".repeat(64);
        assert!(
            BoundTrialPreparation::prepare_file(&path, &sweep, &config, order.clone(), |_| {
                Err(Error("uncertain cleanup".into()))
            })
            .is_err()
        );
        assert!(
            BoundTrialPreparation::load_completed_file(&path, &sweep, &config, &order).is_err()
        );
        let err = BoundTrialPreparation::prepare_file(&path, &sweep, &config, order, |_| {
            panic!("an unsettled attempt must not be relaunched")
        })
        .err()
        .unwrap();
        assert!(err.to_string().contains("unsettled attempt"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
