//! Pure, bounded layout checks. Never reads input payloads or prepares workloads.
use super::{Plan, Slot};
use crate::{Error, benchmark::BenchmarkConfig, planning::ExecutionMode};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(crate) const MAX_EXECUTIONS: u64 = 65_533;

fn invalid(reason: &str) -> Error {
    Error::Input(format!("invalid execution campaign: {reason}"))
}

fn population(repeat: u64, concurrency: usize) -> Result<usize, Error> {
    repeat
        .checked_mul(concurrency as u64)
        .filter(|total| {
            repeat > 0
                && (1..=crate::benchmark::MAX_CONCURRENCY).contains(&concurrency)
                && *total <= MAX_EXECUTIONS
        })
        .map(|total| total as usize)
        .ok_or_else(|| invalid("repeat/concurrency population is invalid or excessive"))
}

fn slot(batch: u64, replica: usize, concurrency: usize, measured: bool) -> Slot {
    let path = |name: &str| {
        let base = PathBuf::from(format!("batch-{batch}/{name}"));
        if concurrency == 1 {
            base
        } else {
            crate::benchmark_paths::replica_path(&base, replica)
        }
    };
    Slot {
        replay_track: None,
        id: (batch - 1) * concurrency as u64 + replica as u64,
        batch,
        replica,
        workspace: path("workspace"),
        log: path("run.jsonl"),
        measurements: measured.then(|| path("measurements")),
        replay_input: None,
    }
}

pub(crate) fn planned_slots(
    config: &BenchmarkConfig,
    repeat: u64,
    measured: bool,
) -> Result<Vec<Slot>, Error> {
    let total = population(repeat, config.concurrency)?;
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(total)
        .map_err(|_| invalid("cannot allocate bounded execution slots"))?;
    for batch in 1..=repeat {
        for replica in 1..=config.concurrency {
            slots.push(slot(batch, replica, config.concurrency, measured));
        }
    }
    Ok(slots)
}

fn exact_path(actual: &Path, expected: &Path) -> bool {
    // Path equality normalizes some lexical aliases. Persist only emitted names.
    actual.as_os_str() == expected.as_os_str()
}

impl Plan {
    /// Validate identities and layout without opening a single artifact/source.
    /// This is not input verification, attempt settlement, or execution consent.
    pub(crate) fn validate_layout(&self) -> Result<usize, Error> {
        let total = population(self.repeat, self.concurrency)?;
        if self.schema_version != 1
            || self.scope != "intended-executions-not-attempt-evidence"
            || self.campaign.is_empty()
            || self.campaign.len() > 256
            || !self
                .campaign
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || self.slots.len() != total
            || self.replay_inputs.len() > total
            || self.configuration.concurrency != self.concurrency
            || self.admission != crate::planning::BatchAdmissionPolicy::current()
            || self.mode == ExecutionMode::Unknown
            || self
                .max_steps
                .is_some_and(|limit| !(1..=65533).contains(&limit))
            || self.max_steps.is_some()
                && !crate::agents::find(&self.configuration.agent)
                    .is_some_and(|adapter| adapter.supports_max_steps())
            || (self.respect_record_timings && self.mode != ExecutionMode::Replay)
            || self.source.as_os_str().is_empty()
        {
            return Err(invalid(
                "schema, identity, mode, admission or population differs",
            ));
        }
        if let Some(expected) = &self.replay_compatibility {
            if self.mode != ExecutionMode::Replay || self.replay_selection.is_none() {
                return Err(invalid(
                    "source replay compatibility requires bundled replay",
                ));
            }
            expected
                .validate()
                .map_err(|error| invalid(&error.to_string()))?;
        }
        let configuration =
            serde_json::to_vec(&self.configuration).map_err(|error| invalid(&error.to_string()))?;
        if crate::cassette::digest(&configuration) != self.configuration_sha256 {
            return Err(invalid("configuration fingerprint differs"));
        }
        let capacity = self.recording_capacity()?;
        if capacity != self.recording_reservation {
            return Err(invalid(
                "recording metadata reservation differs from selection",
            ));
        }
        if let Some(capacity) = capacity {
            capacity
                .validate(crate::cassette::ProofLimits::default())
                .map_err(|error| invalid(&error.to_string()))?;
        }
        if (self.mode == ExecutionMode::Record) != self.recording.is_some()
            || (self.measurement_storage.is_some()
                && self.measurement_root.as_ref().is_none_or(|root| {
                    root.file_name().and_then(|name| name.to_str()) != Some(self.campaign.as_str())
                }))
            || self
                .recording
                .as_ref()
                .is_some_and(|path| !path.is_absolute())
            || self
                .measurement_root
                .as_ref()
                .is_some_and(|path| !path.is_absolute())
        {
            return Err(invalid(
                "recording or measurement root differs from execution mode",
            ));
        }
        for (index, actual) in self.slots.iter().enumerate() {
            let expected = slot(
                (index / self.concurrency) as u64 + 1,
                index % self.concurrency + 1,
                self.concurrency,
                self.measurement_root.is_some(),
            );
            if actual.id != expected.id
                || actual.batch != expected.batch
                || actual.replica != expected.replica
                || !exact_path(&actual.workspace, &expected.workspace)
                || !exact_path(&actual.log, &expected.log)
                || actual.measurements.as_ref().map(|path| path.as_os_str())
                    != expected.measurements.as_ref().map(|path| path.as_os_str())
            {
                return Err(invalid("slot identity or generated path differs"));
            }
        }
        self.validate_replay()?;
        Ok(total)
    }

    fn validate_replay(&self) -> Result<(), Error> {
        if self.mode != ExecutionMode::Replay {
            if self.replay_source.is_some()
                || !self.replay_inputs.is_empty()
                || self.replay_selection.is_some()
                || self
                    .slots
                    .iter()
                    .any(|slot| slot.replay_input.is_some() || slot.replay_track.is_some())
            {
                return Err(invalid("non-replay campaign contains replay state"));
            }
            return Ok(());
        }
        if self
            .replay_source
            .as_ref()
            .is_some_and(|source| !source.is_absolute())
        {
            return Err(invalid("nominal replay source must be absolute"));
        }
        if self.replay_inputs.is_empty() {
            return Err(invalid("replay campaign has no private inputs"));
        }
        let selected = self.replay_inputs[0].track.is_some();
        let bundled = self.replay_selection.is_some();
        let mut identities = BTreeSet::new();
        let mut total_bytes = 0u64;
        let mut bounded_bytes = 0u64;
        for (index, input) in self.replay_inputs.iter().enumerate() {
            let expected = if selected {
                let id = input
                    .track
                    .filter(|id| *id > 0)
                    .ok_or_else(|| invalid("selected input has no positive track ID"))?;
                if !identities.insert(id) || (!bundled && id > 64) {
                    return Err(invalid("duplicate or invalid selected track ID"));
                }
                PathBuf::from(format!("inputs/track-{id}.jsonl"))
            } else {
                if input.track.is_some() {
                    return Err(invalid("legacy and selected inputs are mixed"));
                }
                PathBuf::from(format!("inputs/recording-{}.jsonl", index + 1))
            };
            let max_bytes = if selected {
                crate::cassette::Limits::default().max_track_bytes
            } else {
                64 * 1024 * 1024
            };
            if !exact_path(&input.private, &expected)
                || input.source.as_os_str().is_empty()
                || !crate::cassette::valid_digest(&input.sha256)
                || input.bytes > max_bytes
                || input.complete.is_some() != bundled
                || input.recorded_max_steps == Some(0)
                || (!bundled && input.recorded_max_steps.is_some())
            {
                return Err(invalid("input path, size, digest or completeness differs"));
            }
            bounded_bytes = bounded_bytes
                .checked_add(input.bytes)
                .filter(|bytes| {
                    *bytes <= crate::cassette::ProofLimits::default().max_selected_bytes
                })
                .ok_or_else(|| invalid("selected inputs exceed aggregate byte budget"))?;
            total_bytes = total_bytes
                .checked_add(input.bytes)
                .filter(|bytes| *bytes <= crate::cassette::Limits::default().max_total_bytes)
                .ok_or_else(|| invalid("selected inputs exceed aggregate byte budget"))?;
            let mut auxiliary_kinds = BTreeSet::new();
            for auxiliary in &input.auxiliary_recordings {
                let id = input
                    .track
                    .filter(|_| bundled)
                    .ok_or_else(|| invalid("auxiliary input requires a selected bundle track"))?;
                let filesystem =
                    matches!(auxiliary.kind.as_str(), "aux-fs-events" | "aux-fs-payload");
                let max_auxiliary = if filesystem {
                    crate::cassette::Limits::default().max_artifact_bytes
                } else {
                    crate::cassette::Limits::default().max_track_bytes
                };
                if !super::auxiliary_recording_kind(&auxiliary.kind)
                    || auxiliary.kind == "aux-http-"
                    || auxiliary.kind == "aux-meta-"
                    || auxiliary.kind.len() > 64
                    || !auxiliary.kind.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
                    || !auxiliary_kinds.insert(&auxiliary.kind)
                    || !exact_path(
                        &auxiliary.private,
                        &PathBuf::from(format!("inputs/{}-{id}.jsonl", auxiliary.kind)),
                    )
                    || !crate::cassette::valid_digest(&auxiliary.sha256)
                    || auxiliary.bytes > max_auxiliary
                {
                    return Err(invalid(
                        "auxiliary input identity, path, size or digest differs",
                    ));
                }
                total_bytes = total_bytes
                    .checked_add(auxiliary.bytes)
                    .filter(|bytes| *bytes <= crate::cassette::Limits::default().max_total_bytes)
                    .ok_or_else(|| invalid("selected inputs exceed aggregate byte budget"))?;
                if !filesystem {
                    bounded_bytes = bounded_bytes
                        .checked_add(auxiliary.bytes)
                        .filter(|bytes| {
                            *bytes <= crate::cassette::ProofLimits::default().max_selected_bytes
                        })
                        .ok_or_else(|| invalid("selected inputs exceed aggregate byte budget"))?;
                }
            }
        }
        if bundled
            && (!selected
                || self
                    .replay_inputs
                    .iter()
                    .any(|input| !exact_path(&input.source, &self.replay_inputs[0].source)))
        {
            return Err(invalid("bundle inputs do not share one source identity"));
        }
        if selected && !bundled && self.replay_inputs.len() != 1 {
            return Err(invalid("numeric legacy replay must select one fixed track"));
        }
        let mut seen = BTreeSet::new();
        for slot in &self.slots {
            let index = slot
                .replay_input
                .ok_or_else(|| invalid("replay slot has no input"))?;
            let input = self
                .replay_inputs
                .get(index)
                .ok_or_else(|| invalid("replay slot input index is outside the plan"))?;
            if slot.replay_track != input.track {
                return Err(invalid("slot and input track IDs differ"));
            }
            if !seen.contains(&index) && index != seen.len() {
                return Err(invalid("input table is not ordered by first slot use"));
            }
            seen.insert(index);
            if !selected && self.slots[slot.replica - 1].replay_input != slot.replay_input {
                return Err(invalid("legacy replica input changes between batches"));
            }
        }
        if seen.len() != self.replay_inputs.len() {
            return Err(invalid("unused replay input in plan"));
        }
        if let Some(selection) = &self.replay_selection {
            if selection.algorithm != "sha256-counter-rejection-v1"
                || !crate::cassette::valid_digest(&selection.manifest_sha256)
                || selection.tracks.len() != self.slots.len()
                || self
                    .slots
                    .iter()
                    .zip(&selection.tracks)
                    .any(|(slot, id)| slot.replay_track != Some(*id))
            {
                return Err(invalid(
                    "saved replay selection differs from execution slots",
                ));
            }
            if selection.selector.is_some()
                && selection.tracks.iter().any(|id| *id != selection.tracks[0])
            {
                return Err(invalid("fixed selection changes between execution slots"));
            }
            if let Some(crate::cassette::Selector::Id(id)) = selection.selector {
                if selection.tracks[0] != id {
                    return Err(invalid("numeric selector differs from selected track"));
                }
            } else if self
                .replay_inputs
                .iter()
                .any(|input| input.complete != Some(true) || input.bytes == 0)
            {
                return Err(invalid(
                    "automatic/statistic selection includes an incomplete or empty trace",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
