//! Immutable artifact-copy origins, independent of runner attempt protocols.
use super::{Error, Limits, Manifest, Track, digest, valid_digest, valid_recording_id};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{self, Write};

/// A physical source archive commitment, not a pathname or authentication claim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingSource {
    /// Original immutable physical recording identity.
    pub recording_id: String,
    /// SHA-256 of its canonical serialized manifest.
    pub manifest_sha256: String,
}

/// One exact imported original slot. No new execution or grade is asserted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedTrack {
    /// Original one-based stable track/slot ID, never renumbered.
    pub slot: u64,
    /// Index into the canonical, deduplicated source list.
    pub source: usize,
    /// Canonical serialized full Track metadata, including unknown observations.
    pub metadata_sha256: String,
    /// Exact imported payload length, admitted before any payload is read.
    pub bytes: u64,
}

/// Frozen origin plan for a self-contained continuation archive.
///
/// Slots outside `imports` are planned local slots, NOT observations that a
/// client launched or a capture completed. Missing/incomplete tracks stay unknown.
/// Sources are commitments for provenance; replay reads only this archive.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OriginPlan {
    /// Origin-plan schema; independent of the legacy ZIP/track schemas.
    pub schema_version: u32,
    /// Complete original repeat-times-concurrency population.
    pub population: u64,
    /// Strictly recording-ID-ordered physical source commitments.
    pub sources: Vec<RecordingSource>,
    /// Strictly slot-ordered imported original tracks.
    pub imports: Vec<ImportedTrack>,
}

impl OriginPlan {
    /// Validate the entire metadata declaration before allocation or imports.
    pub fn validate(&self, concurrency: u64, limits: Limits) -> Result<(), Error> {
        limits.validate()?;
        if self.schema_version != 1
            || !(1..=crate::benchmark::MAX_CONCURRENCY as u64).contains(&concurrency)
            || self.population == 0
            || self.population > limits.max_tracks as u64
            || !self.population.is_multiple_of(concurrency)
            || self.imports.len() as u64 > self.population
            || self.sources.len() > self.imports.len()
            || self
                .sources
                .windows(2)
                .any(|pair| pair[0].recording_id >= pair[1].recording_id)
            || self.sources.iter().any(|source| {
                !valid_recording_id(&source.recording_id) || !valid_digest(&source.manifest_sha256)
            })
        {
            return Err(Error("invalid or excessive recording origin plan".into()));
        }
        let mut prior = 0;
        let mut bytes = 0u64;
        let mut used = BTreeSet::new();
        for imported in &self.imports {
            if imported.slot <= prior
                || imported.slot > self.population
                || imported.source >= self.sources.len()
                || !valid_digest(&imported.metadata_sha256)
                || imported.bytes > limits.max_track_bytes
            {
                return Err(Error("invalid recording import identity or bounds".into()));
            }
            bytes = bytes
                .checked_add(imported.bytes)
                .filter(|total| *total <= limits.max_total_bytes)
                .ok_or_else(|| Error("recording imports exceed aggregate payload budget".into()))?;
            prior = imported.slot;
            used.insert(imported.source);
        }
        if used.len() != self.sources.len() {
            return Err(Error(
                "recording origin plan contains unused sources".into(),
            ));
        }
        super::serialization::encoded_size(self, limits.max_manifest_bytes)?;
        Ok(())
    }

    /// Exact stable commitment; caller validates bounds before computing this.
    pub fn commitment(&self) -> Result<String, Error> {
        #[cfg(test)]
        super::journal::observed("origin-commitment");
        // The owner has already validated its chosen limits. Hash the canonical
        // wire directly, without allocating a second metadata-sized buffer or
        // silently imposing the default policy on custom-limit journals.
        struct HashWriter(Sha256);
        impl Write for HashWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut writer = HashWriter(Sha256::new());
        serde_json::to_writer(&mut writer, self).map_err(|error| Error(error.to_string()))?;
        Ok(format!("{:x}", writer.0.finalize()))
    }

    /// Find an imported slot without a quadratic population scan.
    pub fn imported(&self, slot: u64) -> Option<&ImportedTrack> {
        self.imports
            .binary_search_by_key(&slot, |entry| entry.slot)
            .ok()
            .map(|index| &self.imports[index])
    }

    pub(super) fn require_track(&self, track: &Track, source: usize) -> Result<(), Error> {
        let imported = self
            .imported(track.id)
            .ok_or_else(|| Error("track is not reserved for import".into()))?;
        if imported.source != source
            || imported.bytes != track.bytes
            || imported.metadata_sha256 != track_metadata_digest(track)?
        {
            return Err(Error(
                "imported track differs from its frozen origin plan".into(),
            ));
        }
        Ok(())
    }
}

/// Canonical complete metadata commitment, not just a JSONL payload checksum.
pub fn track_metadata_digest(track: &Track) -> Result<String, Error> {
    if !valid_digest(&track.sha256) {
        return Err(Error("invalid imported track digest".into()));
    }
    serde_json::to_vec(track)
        .map(|bytes| digest(&bytes))
        .map_err(|error| Error(error.to_string()))
}

impl Manifest {
    /// An archive must contain every reserved import with exactly its old facts.
    /// Intent validation is separate: imports are not committed at creation.
    pub(super) fn validate_imports(&self) -> Result<(), Error> {
        if let Some(plan) = &self.provenance {
            for imported in &plan.imports {
                plan.require_track(self.track(imported.slot)?, imported.source)?;
            }
        }
        Ok(())
    }
}
