//! Pure metadata capacity planning using the actual evidence wire schemas.
use super::*;
use crate::cassette::serialization::{ManifestWire, TrackEvidenceWire, encoded_size};
use crate::cassette::{CompatibilityShape, ProofLimits};

struct Layout<'a> {
    recording_id: &'a str,
    destination: &'a Path,
    repeat: u64,
    concurrency: u64,
}

/// Conservative capacity for metadata whose size is known before execution.
/// This is requested capacity, never task, material, grading or cleanup evidence.
/// Payload/compressed lengths depend on responses and are not guessed here.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingReservation {
    /// Capacity schema, distinct from recording/manifest schemas.
    pub schema_version: u32,
    /// Planned execution/proof occurrences.
    pub references: u64,
    /// Encoded immutable track references, including repeated compatibility.
    pub reference_bytes: u64,
    /// Recording intent plus immutable starts and commits.
    pub journal_bytes: u64,
    /// Final manifest, including all planned tracks with maximum-width statistics.
    pub manifest_bytes: u64,
    /// Final publication marker with bounded maximum-width observations.
    pub publication_bytes: u64,
    /// ZIP32 central-directory headers/names and end record.
    pub directory_bytes: u64,
}

impl Default for RecordingReservation {
    fn default() -> Self {
        Self {
            schema_version: 1,
            references: 0,
            reference_bytes: 0,
            journal_bytes: 0,
            manifest_bytes: 0,
            publication_bytes: 0,
            directory_bytes: 0,
        }
    }
}

impl RecordingReservation {
    /// Reserve metadata without opening paths, publishing files, preparing a
    /// provider or constructing observed fingerprint values. Optional digests
    /// reserve their present representation; statistics reserve u64::MAX.
    pub fn for_plan(
        shape: CompatibilityShape<'_>,
        recording_id: &str,
        destination: &Path,
        repeat: u64,
        concurrency: u64,
        limits: Limits,
    ) -> Result<Self, Error> {
        Self::reserve(
            shape,
            Layout {
                recording_id,
                destination,
                repeat,
                concurrency,
            },
            limits,
            None,
        )
    }

    /// Reserve a full union before importing or admitting any new execution.
    /// Uses the same serialization projection as actual intent and manifests.
    pub fn for_origin_plan(
        shape: CompatibilityShape<'_>,
        recording_id: &str,
        destination: &Path,
        concurrency: u64,
        provenance: &crate::cassette::OriginPlan,
        limits: Limits,
    ) -> Result<Self, Error> {
        provenance.validate(concurrency, limits)?;
        if provenance
            .sources
            .iter()
            .any(|source| source.recording_id == recording_id)
        {
            return Err(Error(
                "union physical identity reuses a source recording".into(),
            ));
        }
        Self::reserve(
            shape,
            Layout {
                recording_id,
                destination,
                repeat: provenance.population / concurrency,
                concurrency,
            },
            limits,
            Some(provenance),
        )
    }

    fn reserve(
        shape: CompatibilityShape<'_>,
        layout: Layout<'_>,
        limits: Limits,
        provenance: Option<&crate::cassette::OriginPlan>,
    ) -> Result<Self, Error> {
        let Layout {
            recording_id,
            destination,
            repeat,
            concurrency,
        } = layout;
        limits.validate()?;
        let count = validate_count(repeat, concurrency, limits)?;
        if !crate::cassette::valid_recording_id(recording_id) || !destination.is_absolute() {
            return Err(Error(
                "invalid recording capacity identity or destination".into(),
            ));
        }
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error("recording capacity needs a UTF-8 filename".into()))?;
        let manifest = ManifestWire {
            schema_version: crate::cassette::SCHEMA_VERSION,
            recording_id,
            compatibility: shape.wire()?,
            concurrency,
            tracks: &[],
            provenance,
        };
        let intent = Intent {
            manifest,
            repeat,
            destination_name: name.to_owned(),
        };
        let capacity = metadata_budget(
            &intent,
            recording_id,
            concurrency,
            count,
            limits,
            provenance.and_then(|plan| plan.sources.len().checked_sub(1)),
        )?;
        let track = maximum_track(count, repeat, concurrency, limits);
        let reference = TrackEvidenceWire {
            schema_version: 1,
            destination,
            recording_id,
            compatibility: shape.wire()?,
            concurrency,
            track: &track,
            origin_sha256: provenance
                .map(|_| "0000000000000000000000000000000000000000000000000000000000000000"),
        };
        let reference_bytes = encoded_size(&reference, u64::MAX)?
            .checked_mul(count)
            .ok_or_else(|| Error("recording reference capacity overflow".into()))?;
        let publication_bytes = super::publication::publication_capacity(recording_id, limits)?;
        let directory_bytes = crate::cassette::metadata::recording_directory_size(count)?;
        if directory_bytes > limits.max_directory_bytes {
            return Err(Error(
                "planned recording directory exceeds metadata budget".into(),
            ));
        }
        Ok(Self {
            schema_version: 1,
            references: count,
            reference_bytes,
            journal_bytes: capacity.journal,
            manifest_bytes: capacity.manifest,
            publication_bytes,
            directory_bytes,
        })
    }

    /// Checked sum for one sweep's recording population, independent of jobs.
    pub fn checked_add(&self, other: &Self) -> Result<Self, Error> {
        if self.schema_version != 1 || other.schema_version != 1 {
            return Err(Error("unsupported recording capacity schema".into()));
        }
        let add = |a: u64, b| {
            a.checked_add(b)
                .ok_or_else(|| Error("recording capacity overflow".into()))
        };
        Ok(Self {
            schema_version: 1,
            references: add(self.references, other.references)?,
            reference_bytes: add(self.reference_bytes, other.reference_bytes)?,
            journal_bytes: add(self.journal_bytes, other.journal_bytes)?,
            manifest_bytes: add(self.manifest_bytes, other.manifest_bytes)?,
            publication_bytes: add(self.publication_bytes, other.publication_bytes)?,
            directory_bytes: add(self.directory_bytes, other.directory_bytes)?,
        })
    }

    /// Admit deterministic metadata under whole-operation budgets. Unknown
    /// response sizes remain subject to runtime archive/producer bounds.
    pub fn validate(&self, limits: ProofLimits) -> Result<(), Error> {
        limits.validate()?;
        let metadata = self
            .journal_bytes
            .checked_add(self.manifest_bytes)
            .and_then(|bytes| bytes.checked_add(self.publication_bytes));
        if self.schema_version != 1
            || self.references > limits.max_references as u64
            || self.reference_bytes > limits.max_reference_bytes
            || metadata.is_none_or(|bytes| bytes > limits.max_metadata_bytes)
            || self.directory_bytes > limits.max_directory_bytes
        {
            return Err(Error("planned recording proofs exceed aggregate metadata budget; reduce the selected recording population".into()));
        }
        Ok(())
    }
}

pub(super) struct MetadataCapacity {
    pub journal: u64,
    pub manifest: u64,
}

pub(super) fn metadata_budget<M: Serialize>(
    intent: &Intent<M>,
    recording_id: &str,
    concurrency: u64,
    count: u64,
    limits: Limits,
    import_source: Option<usize>,
) -> Result<MetadataCapacity, Error> {
    let track = maximum_track(count, intent.repeat, concurrency, limits);
    let start = Started {
        recording_id: recording_id.into(),
        id: count,
        batch: intent.repeat,
        replica: concurrency,
        import_source,
    };
    let track_bytes = encoded_size(&track, limits.max_manifest_bytes)?;
    let start_bytes = encoded_size(&start, MAX_RECORD_BYTES)?;
    let commit_bytes = encoded_size(
        &Committed {
            recording_id: recording_id.into(),
            track,
        },
        MAX_RECORD_BYTES,
    )?;
    let intent_bytes = encoded_size(intent, limits.max_manifest_bytes)?;
    let manifest_bytes = encoded_size(&intent.manifest, limits.max_manifest_bytes)?;
    let journal = start_bytes
        .checked_add(commit_bytes)
        .and_then(|slot| slot.checked_mul(count))
        .and_then(|slots| slots.checked_add(intent_bytes));
    let catalogue = track_bytes
        .checked_add(1)
        .and_then(|slot| slot.checked_mul(count))
        .and_then(|slots| slots.checked_sub(1))
        .and_then(|slots| slots.checked_add(manifest_bytes));
    match (journal, catalogue) {
        (Some(journal), Some(manifest)) if journal <= limits.max_manifest_bytes && manifest <= limits.max_manifest_bytes => Ok(MetadataCapacity { journal, manifest }),
        _ => Err(Error("planned recording metadata exceeds the manifest budget; reduce repeat/concurrency or choose larger explicit limits".into())),
    }
}

fn maximum_track(count: u64, repeat: u64, concurrency: u64, limits: Limits) -> Track {
    Track {
        recorder: Some("a".repeat(64)),
        id: count,
        batch: repeat,
        replica: concurrency,
        complete: false,
        process_success: Some(false),
        workload_success: Some(false),
        bytes: limits.max_track_bytes,
        sha256: "f".repeat(64),
        statistics: Statistics {
            tokens: Some(u64::MAX),
            duration_ms: Some(u64::MAX),
            requests: Some(u64::MAX),
            steps: Some(u64::MAX),
        },
        native: None,
        artifacts: Vec::new(),
    }
}
