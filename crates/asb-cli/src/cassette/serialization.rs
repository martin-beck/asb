//! Shared wire projections: actual evidence and size-only shapes use one schema.
use super::{Compatibility, Error, Manifest, Track, TrackEvidence};
use serde::{Serialize, Serializer};
use std::io::Write;
use std::path::Path;

/// Known logical selection for capacity planning, not observed compatibility.
/// Fingerprint contents are deliberately absent. This type cannot initialize a
/// recording journal or satisfy a replay compatibility check.
#[derive(Clone, Copy, Debug)]
pub struct CompatibilityShape<'a> {
    /// Selected logical agent name.
    pub agent: &'a str,
    /// Selected logical provider name.
    pub provider: &'a str,
    /// Selected provider-scoped model name.
    pub model: &'a str,
    /// Selected workload name.
    pub workload: &'a str,
    /// Requested task identity, not an attempt or a successful baseline.
    pub task: &'a str,
}

#[derive(Serialize)]
pub(super) struct CompatibilityWire<'a, D> {
    agent: &'a str,
    provider: &'a str,
    model: &'a str,
    workload: &'a str,
    task: &'a str,
    dataset_sha256: Option<D>,
    prompt_sha256: D,
    material_sha256: D,
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol_sha256: Option<D>,
    #[serde(skip_serializing_if = "Option::is_none")]
    normalization_sha256: Option<D>,
}

impl CompatibilityShape<'_> {
    pub(super) fn wire(&self) -> Result<CompatibilityWire<'_, DigestWidth>, Error> {
        for value in [
            self.agent,
            self.provider,
            self.model,
            self.workload,
            self.task,
        ] {
            if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
                return Err(Error(
                    "invalid or oversized planned compatibility identity".into(),
                ));
            }
        }
        Ok(CompatibilityWire {
            agent: self.agent,
            provider: self.provider,
            model: self.model,
            workload: self.workload,
            task: self.task,
            // Reserve present optional fingerprints. Unknown does not mean absent.
            dataset_sha256: Some(DigestWidth),
            prompt_sha256: DigestWidth,
            material_sha256: DigestWidth,
            protocol_sha256: Some(DigestWidth),
            normalization_sha256: Some(DigestWidth),
        })
    }
}

pub(super) struct DigestWidth;
impl Serialize for DigestWidth {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Sent only to the size counter, never persisted as an observed digest.
        serializer.serialize_str("0000000000000000000000000000000000000000000000000000000000000000")
    }
}

impl Serialize for Compatibility {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        CompatibilityWire {
            agent: &self.agent,
            provider: &self.provider,
            model: &self.model,
            workload: &self.workload,
            task: &self.task,
            dataset_sha256: self.dataset_sha256.as_deref(),
            prompt_sha256: self.prompt_sha256.as_str(),
            material_sha256: self.material_sha256.as_str(),
            protocol_sha256: self.protocol_sha256.as_deref(),
            normalization_sha256: self.normalization_sha256.as_deref(),
        }
        .serialize(serializer)
    }
}

#[derive(Serialize)]
pub(super) struct ManifestWire<'a, C> {
    pub schema_version: u32,
    pub recording_id: &'a str,
    pub compatibility: C,
    pub concurrency: u64,
    pub tracks: &'a [Track],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<&'a super::OriginPlan>,
}

impl Serialize for Manifest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ManifestWire {
            schema_version: self.schema_version,
            recording_id: &self.recording_id,
            compatibility: &self.compatibility,
            concurrency: self.concurrency,
            tracks: &self.tracks,
            provenance: self.provenance.as_ref(),
        }
        .serialize(serializer)
    }
}

#[derive(Serialize)]
pub(super) struct TrackEvidenceWire<'a, C> {
    pub schema_version: u32,
    pub destination: &'a Path,
    pub recording_id: &'a str,
    pub compatibility: C,
    pub concurrency: u64,
    pub track: &'a Track,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_sha256: Option<&'a str>,
}

impl Serialize for TrackEvidence {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        TrackEvidenceWire {
            schema_version: self.schema_version,
            destination: &self.destination,
            recording_id: &self.recording_id,
            compatibility: &self.compatibility,
            concurrency: self.concurrency,
            track: &self.track,
            origin_sha256: self.origin_sha256.as_deref(),
        }
        .serialize(serializer)
    }
}

/// Count exact encoded bytes without allocating a serialized copy.
pub(super) fn encoded_size(value: &impl Serialize, limit: u64) -> Result<u64, Error> {
    struct Counter {
        bytes: u64,
        limit: u64,
    }
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self
                .bytes
                .checked_add(bytes.len() as u64)
                .filter(|total| *total <= self.limit)
                .ok_or_else(|| std::io::Error::other("encoded metadata exceeds byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { bytes: 0, limit };
    serde_json::to_writer(&mut counter, value).map_err(|error| Error(error.to_string()))?;
    Ok(counter.bytes)
}
