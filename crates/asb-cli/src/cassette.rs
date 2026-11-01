//! Provider-independent multi-track cassette contracts and bounded ZIP reading.
//!
//! This backend does not launch agents, resolve credentials or change workspaces.
//! A selected payload is ordinary JSONL for the existing tape proxy. Checksums
//! detect corruption; they are not an authenticated provenance claim.

mod archive;
mod journal;
mod metadata;
mod native;
mod provenance;
mod selection;
mod serialization;
mod source;

pub use archive::{Archive, Limits};
pub use journal::{
    ArtifactInput, NativeInput, ProofLimits, Publication, RecordingJournal, RecordingReservation,
    TrackEvidence,
};
pub use native::{NativeSession, NativeToolLaunch, NativeToolTransport};
pub use provenance::{ImportedTrack, OriginPlan, RecordingSource, track_metadata_digest};
pub use selection::{Metric, SelectionPlan, Selector};
use serde::{Deserialize, Serialize};
pub use serialization::CompatibilityShape;
pub(crate) use source::SourceInspection;
#[cfg(test)]
pub(crate) use source::take_observation_count;
pub use source::{LegacyTrack, Source};
use std::fmt;

/// Current manifest format. Unknown versions are refused, not guessed.
pub const SCHEMA_VERSION: u32 = 2;
const MIN_SCHEMA_VERSION: u32 = 1;

/// An actionable cassette-format, compatibility or selection error.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Canonical content identity; excludes concurrency, repetition and measurements.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    /// Logical coding agent name.
    pub agent: String,
    /// Logical provider name.
    pub provider: String,
    /// Provider-specific model name.
    pub model: String,
    /// Logical workload name.
    pub workload: String,
    /// Selected trial or original dataset instance.
    pub task: String,
    /// Dataset source/revision/split fingerprint, when applicable.
    pub dataset_sha256: Option<String>,
    /// Digest of the effective prompt, not its text.
    pub prompt_sha256: String,
    /// Adapter-owned canonical initial files/environment definition digest.
    pub material_sha256: String,
    /// Replay-relevant module, harness and scoring protocol identity. Absent
    /// only in earlier backend manifests; new run recordings must supply it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_sha256: Option<String>,
}

impl Compatibility {
    /// Validate bounded logical identities and canonical lowercase SHA-256 values.
    pub fn validate(&self) -> Result<(), Error> {
        for value in [
            &self.agent,
            &self.provider,
            &self.model,
            &self.workload,
            &self.task,
        ] {
            if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
                return Err(Error(
                    "invalid or oversized cassette compatibility identity".into(),
                ));
            }
        }
        if !valid_digest(&self.prompt_sha256)
            || !valid_digest(&self.material_sha256)
            || self
                .protocol_sha256
                .as_deref()
                .is_some_and(|value| !valid_digest(value))
            || self
                .dataset_sha256
                .as_deref()
                .is_some_and(|value| !valid_digest(value))
        {
            return Err(Error("invalid cassette compatibility fingerprint".into()));
        }
        Ok(())
    }

    /// Reject different task/material/model identities before starting execution.
    pub fn require_match(&self, requested: &Self) -> Result<(), Error> {
        self.validate()?;
        requested.validate()?;
        if self != requested {
            return Err(Error("cassette compatibility mismatch: agent/provider/model/task, prompt, initial material or harness/protocol differs".into()));
        }
        Ok(())
    }
}

/// Optional observations; unknown is never represented as zero.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Statistics {
    /// Canonical input plus output tokens; cached input is not added twice.
    pub tokens: Option<u64>,
    /// Execution-only elapsed milliseconds, excluding preparation/evaluation.
    pub duration_ms: Option<u64>,
    /// Recorded inference requests, excluding health/readiness probes.
    pub requests: Option<u64>,
    /// Owning agent adapter's step observation.
    pub steps: Option<u64>,
}

/// One execution attempt, independent of trace completeness and task success.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Track {
    /// Recording proxy identity; absent on historical llmtape recordings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorder: Option<String>,
    /// Stable one-based ID in batch/replica order, not completion order.
    pub id: u64,
    /// One-based sequential batch.
    pub batch: u64,
    /// One-based concurrent replica within this batch.
    pub replica: u64,
    /// True only with positive recording finalization evidence.
    pub complete: bool,
    /// Client process outcome, or unknown after interruption/preparation failure.
    pub process_success: Option<bool>,
    /// Evaluator outcome, independent of client process status.
    pub workload_success: Option<bool>,
    /// Uncompressed JSONL length; empty attempts are retained for inspection.
    pub bytes: u64,
    /// Digest of exact uncompressed JSONL bytes.
    pub sha256: String,
    /// Optional normalized observations for selectors and presentation.
    pub statistics: Statistics,
    /// Optional sealed native-tool interaction stream.  Its bytes stay private
    /// to the cassette; this metadata is sufficient to reject absent, changed
    /// or incomplete sidecars before a future native replay transport starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeTrack>,
    /// Additional run-private evidence tied to this exact execution.  These
    /// payloads are deliberately catalogued with the provider transcript so a
    /// replay can never silently consume an unsealed sidecar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
}

/// Bounded, opaque native-tool stream for one execution track.
///
/// This foundation intentionally does not define command, path, prompt, or
/// output fields.  A future private ingress owns that schema and must supply a
/// sealed JSONL stream through the journal in the same commit as its track.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeTrack {
    /// True only when the native ingress observed a terminal stream boundary.
    pub complete: bool,
    /// Exact count of newline-delimited opaque events.
    pub events: u64,
    /// Uncompressed JSONL length.
    pub bytes: u64,
    /// Digest of the exact uncompressed JSONL bytes.
    pub sha256: String,
}

impl Track {
    /// Canonical archive entry, never an externally supplied extraction path.
    pub fn entry_name(&self) -> String {
        format!("tracks/{}.jsonl", self.id)
    }

    /// Canonical archive entry for a named, sealed auxiliary payload.
    pub fn artifact_entry_name(&self, kind: &str) -> String {
        format!("artifacts/{}/{}.jsonl", self.id, kind)
    }

    /// Canonical archive entry for this track's sealed native sidecar.
    pub fn native_entry_name(&self) -> String {
        format!("native/{}.jsonl", self.id)
    }

    /// Whether the native sidecar can be consumed by a future replay transport.
    pub fn native_replayable(&self) -> bool {
        self.native.as_ref().is_some_and(|native| native.complete)
    }

    /// Complete failed attempts remain eligible, but known probe-only captures do
    /// not supply inference traces. Missing legacy request statistics stay unknown.
    pub fn automatically_selectable(&self) -> bool {
        self.complete && self.bytes > 0 && self.statistics.requests != Some(0)
    }
}

/// A bounded auxiliary JSONL payload that belongs to one execution track.
///
/// Artifact kinds are a small, canonical namespace controlled by ASB.  The
/// payload is never exposed as a generic archive path, preventing a recording
/// from smuggling arbitrary files into a replay workspace.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// Stable lowercase ASCII kind (for example `local-tools`).
    pub kind: String,
    /// Uncompressed JSONL length.
    pub bytes: u64,
    /// Digest of the exact uncompressed bytes.
    pub sha256: String,
}

impl Artifact {
    fn validate(&self, limits: Limits) -> Result<(), Error> {
        if !valid_artifact_kind(&self.kind)
            || self.bytes > limits.max_track_bytes
            || !valid_digest(&self.sha256)
        {
            return Err(Error(
                "invalid cassette auxiliary artifact identity, digest or size".into(),
            ));
        }
        Ok(())
    }
}

fn valid_artifact_kind(kind: &str) -> bool {
    !kind.is_empty()
        && kind.len() <= 64
        && kind
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Versioned recording identity and immutable track catalogue.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Must equal [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Bounded recording ID, independent of its storage location.
    pub recording_id: String,
    /// Common compatibility identity for every track in this bundle.
    pub compatibility: Compatibility,
    /// Configured replicas per recording batch; does not constrain replay.
    pub concurrency: u64,
    /// Tracks in strictly increasing ID order; interrupted attempts may leave gaps.
    pub tracks: Vec<Track>,
    /// Optional immutable origin plan. Absence preserves exact legacy encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<OriginPlan>,
}

impl Manifest {
    /// Check schema, bounds, fingerprints and stable slot-to-track mapping.
    pub fn validate(&self, limits: Limits) -> Result<(), Error> {
        limits.validate()?;
        if !(MIN_SCHEMA_VERSION..=SCHEMA_VERSION).contains(&self.schema_version) {
            return Err(Error(format!(
                "unsupported cassette schema version {}",
                self.schema_version
            )));
        }
        self.compatibility.validate()?;
        if let Some(plan) = &self.provenance {
            plan.validate(self.concurrency, limits)?;
            if plan
                .sources
                .iter()
                .any(|source| source.recording_id == self.recording_id)
            {
                return Err(Error(
                    "union physical identity reuses a source recording".into(),
                ));
            }
        }
        if !valid_recording_id(&self.recording_id)
            || !(1..=64).contains(&self.concurrency)
            || self.tracks.len() > limits.max_tracks
        {
            return Err(Error(
                "invalid or oversized cassette recording identity/catalogue".into(),
            ));
        }
        let mut prior = 0;
        let mut bytes = 0u64;
        let mut payload_entries = self.tracks.len();
        for track in &self.tracks {
            let expected = track
                .batch
                .checked_sub(1)
                .and_then(|batch| batch.checked_mul(self.concurrency))
                .and_then(|base| base.checked_add(track.replica));
            if track.recorder.as_deref().is_some_and(|name| {
                name.is_empty()
                    || name.len() > 64
                    || !name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            }) || track.id <= prior
                || expected != Some(track.id)
                || !(1..=self.concurrency).contains(&track.replica)
                || !valid_digest(&track.sha256)
                || track.bytes > limits.max_track_bytes
            {
                return Err(Error(format!(
                    "invalid cassette track {} identity, digest or size",
                    track.id
                )));
            }
            if let Some(native) = &track.native {
                if self.schema_version < 2
                    || native.bytes > limits.max_track_bytes
                    || native.events > native.bytes
                    || !valid_digest(&native.sha256)
                {
                    return Err(Error(format!(
                        "invalid cassette native sidecar for track {}",
                        track.id
                    )));
                }
                payload_entries = payload_entries
                    .checked_add(1)
                    .ok_or_else(|| Error("cassette payload entry count overflow".into()))?;
                if payload_entries > limits.max_tracks {
                    return Err(Error("cassette exceeds payload entry limit".into()));
                }
                bytes = bytes
                    .checked_add(native.bytes)
                    .ok_or_else(|| Error("cassette total length overflow".into()))?;
                if bytes > limits.max_total_bytes {
                    return Err(Error(
                        "cassette exceeds total uncompressed size limit".into(),
                    ));
                }
            }
            bytes = bytes
                .checked_add(track.bytes)
                .ok_or_else(|| Error("cassette total length overflow".into()))?;
            if bytes > limits.max_total_bytes {
                return Err(Error(
                    "cassette exceeds total uncompressed size limit".into(),
                ));
            }
            let mut artifact_kinds = std::collections::BTreeSet::new();
            for artifact in &track.artifacts {
                artifact.validate(limits)?;
                if !artifact_kinds.insert(&artifact.kind) {
                    return Err(Error(
                        "cassette track contains duplicate auxiliary artifact kinds".into(),
                    ));
                }
                payload_entries = payload_entries
                    .checked_add(1)
                    .ok_or_else(|| Error("cassette payload entry count overflow".into()))?;
                if payload_entries > limits.max_tracks {
                    return Err(Error("cassette exceeds payload entry limit".into()));
                }
                bytes = bytes
                    .checked_add(artifact.bytes)
                    .ok_or_else(|| Error("cassette total length overflow".into()))?;
                if bytes > limits.max_total_bytes {
                    return Err(Error(
                        "cassette exceeds total uncompressed size limit".into(),
                    ));
                }
            }
            prior = track.id;
            if self
                .provenance
                .as_ref()
                .is_some_and(|plan| track.id > plan.population)
            {
                return Err(Error("track exceeds original provenance population".into()));
            }
        }
        Ok(())
    }

    /// Find a stable track ID without interpreting it as a vector offset.
    pub fn track(&self, id: u64) -> Result<&Track, Error> {
        self.tracks
            .binary_search_by_key(&id, |track| track.id)
            .map(|index| &self.tracks[index])
            .map_err(|_| Error(format!("cassette track {id} does not exist")))
    }
}

pub(crate) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// SHA-256 of exact bytes, encoded as lowercase hex for manifests and plans.
pub fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_recording_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

#[cfg(test)]
mod tests;
