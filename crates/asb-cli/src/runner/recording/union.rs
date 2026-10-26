//! Frozen recording continuation inputs; artifact import never runs an agent.
use super::*;
use crate::cassette::{Archive, ImportedTrack, OriginPlan, RecordingReservation, RecordingSource};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Descriptor {
    pub physical_id: String,
    pub destination: PathBuf,
    pub origins: OriginPlan,
    pub sources: Vec<PathBuf>,
    pub reservation: RecordingReservation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    pub reference: crate::runner::campaign::generation::StartReference,
    pub origins_sha256: String,
}

pub(crate) enum Staging {
    Fresh,
    RecoverUnused,
}

/// Pure admission of all configuration declarations, without payload I/O.
/// Retained-node recovery calls this before source archive or journal reads;
/// fresh planning also uses it after deriving descriptors from source indexes.
pub(crate) fn admit_population<'a>(
    descriptors: impl IntoIterator<Item = &'a Descriptor>,
) -> Result<(), Error> {
    let mut capacity = RecordingReservation::default();
    let mut imported_bytes = 0u64;
    for descriptor in descriptors {
        capacity = capacity
            .checked_add(&descriptor.reservation)
            .map_err(recording_error)?;
        for imported in &descriptor.origins.imports {
            imported_bytes = imported_bytes
                .checked_add(imported.bytes)
                .filter(|bytes| *bytes <= Limits::default().max_total_bytes)
                .ok_or_else(|| Error::Input("union imports exceed global payload budget".into()))?;
        }
    }
    capacity
        .validate(crate::cassette::ProofLimits::default())
        .map_err(recording_error)
}

impl Descriptor {
    pub(crate) fn recover_unused(
        &self,
        compatibility: Option<&Compatibility>,
        concurrency: usize,
    ) -> Result<Option<RecordingJournal>, Error> {
        let Some(compatibility) = compatibility else {
            if !self.origins.imports.is_empty() {
                return Err(Error::Input(
                    "unused recording imports have no verified material identity".into(),
                ));
            }
            // No baseline meant the original admission staged no writer. Do
            // not adopt a foreign partial directory on this uninitialized path.
            if inspect_destination(&self.destination)? != self.destination {
                return Err(Error::Input("unused recording destination changed".into()));
            }
            return Ok(None);
        };
        let journal = RecordingJournal::recover(&self.destination, Limits::default())
            .map_err(recording_error)?;
        let snapshot = journal.imported_only_snapshot().map_err(recording_error)?;
        if snapshot.recording_id != self.physical_id
            || snapshot.provenance.as_ref() != Some(&self.origins)
            || snapshot.concurrency != concurrency as u64
        {
            return Err(Error::Input(
                "unused recording writer differs from its admitted union".into(),
            ));
        }
        snapshot
            .compatibility
            .require_match(compatibility)
            .map_err(recording_error)?;
        Ok(Some(journal))
    }

    pub(crate) fn plan(
        plan: &super::super::campaign::Plan,
        generation: &str,
        configuration: usize,
        completed: &BTreeMap<u64, serde_json::Value>,
    ) -> Result<Self, Error> {
        let mut groups: BTreeMap<String, (PathBuf, Vec<Track>)> = BTreeMap::new();
        for (slot, summary) in completed {
            let evidence: Evidence =
                serde_json::from_value(summary["recording"].clone()).map_err(|_| {
                    Error::Input("completed recording has no immutable track evidence".into())
                })?;
            if evidence.track.track.id != *slot {
                return Err(Error::Input(
                    "imported recording differs from its original slot".into(),
                ));
            }
            let entry = groups
                .entry(evidence.track.recording_id.clone())
                .or_insert_with(|| (evidence.track.destination.clone(), Vec::new()));
            if entry.0 != evidence.track.destination {
                return Err(Error::Input(
                    "one recording identity names multiple import sources".into(),
                ));
            }
            entry.1.push(evidence.track.track);
        }
        let mut sources = Vec::new();
        let mut origins = OriginPlan {
            schema_version: 1,
            population: plan.slots.len() as u64,
            sources: Vec::new(),
            imports: Vec::new(),
        };
        for (recording_id, (destination, tracks)) in groups {
            let archive =
                Archive::open(&destination, Limits::default()).map_err(recording_error)?;
            if archive.manifest().recording_id != recording_id
                || archive.manifest().concurrency != plan.concurrency as u64
            {
                return Err(Error::Input(
                    "recording import source identity or width changed".into(),
                ));
            }
            let source = origins.sources.len();
            origins.sources.push(RecordingSource {
                recording_id,
                manifest_sha256: crate::cassette::digest(
                    &serde_json::to_vec(archive.manifest())
                        .map_err(|error| Error::Io(error.to_string()))?,
                ),
            });
            sources.push(destination);
            for track in tracks {
                if archive
                    .manifest()
                    .track(track.id)
                    .map_err(recording_error)?
                    != &track
                {
                    return Err(Error::Input(
                        "recording import metadata differs from its verified receipt".into(),
                    ));
                }
                origins.imports.push(ImportedTrack {
                    slot: track.id,
                    source,
                    metadata_sha256: crate::cassette::track_metadata_digest(&track)
                        .map_err(recording_error)?,
                    bytes: track.bytes,
                });
            }
        }
        origins.imports.sort_unstable_by_key(|track| track.slot);
        let physical_id = format!("{generation}-{configuration:x}");
        let destination = Self::destination(plan, &physical_id)?;
        let reservation = Self::capacity(plan, &physical_id, &destination, &origins)?;
        Ok(Self {
            physical_id,
            destination,
            origins,
            sources,
            reservation,
        })
    }

    fn destination(
        plan: &super::super::campaign::Plan,
        physical_id: &str,
    ) -> Result<PathBuf, Error> {
        let original = plan
            .recording
            .as_ref()
            .ok_or_else(|| Error::Input("recording campaign has no original destination".into()))?;
        Ok(original
            .parent()
            .ok_or_else(|| Error::Input("recording destination has no parent".into()))?
            .join(format!("continuation-{physical_id}.asb.zip")))
    }

    fn capacity(
        plan: &super::super::campaign::Plan,
        physical_id: &str,
        destination: &Path,
        origins: &OriginPlan,
    ) -> Result<RecordingReservation, Error> {
        let selection = plan.recording_selection.as_ref().ok_or_else(|| {
            Error::Input("recording continuation lost its original selection".into())
        })?;
        RecordingReservation::for_origin_plan(
            crate::cassette::CompatibilityShape {
                agent: &plan.configuration.agent,
                provider: &plan.configuration.provider,
                model: &plan.configuration.model,
                workload: &selection.workload,
                task: &selection.task_id,
            },
            physical_id,
            destination,
            plan.concurrency as u64,
            origins,
            Limits::default(),
        )
        .map_err(recording_error)
    }

    pub(crate) fn validate(
        &self,
        plan: &super::super::campaign::Plan,
        generation: &str,
        configuration: usize,
        completed: impl Iterator<Item = u64>,
    ) -> Result<(), Error> {
        let expected_destination = Self::destination(plan, &self.physical_id)?;
        if self.physical_id != format!("{generation}-{configuration:x}")
            || self.destination != expected_destination
            || self.origins.population != plan.slots.len() as u64
            || self.sources.len() != self.origins.sources.len()
            || self.sources.iter().any(|path| !path.is_absolute())
            || self.sources.iter().collect::<BTreeSet<_>>().len() != self.sources.len()
            || self
                .origins
                .imports
                .iter()
                .map(|track| track.slot)
                .collect::<BTreeSet<_>>()
                != completed.collect()
            || self.reservation
                != Self::capacity(plan, &self.physical_id, &self.destination, &self.origins)?
        {
            return Err(Error::Input(
                "recording union differs from its admitted campaign/import population".into(),
            ));
        }
        Ok(())
    }

    /// Stage all historical bytes before any executable capability is returned.
    /// An empty source set with no baseline stays uninitialized until preparation.
    pub(super) fn stage(
        &self,
        compatibility: Option<&Compatibility>,
        concurrency: usize,
        repeat: u64,
    ) -> Result<Option<RecordingJournal>, Error> {
        let Some(compatibility) = compatibility else {
            if !self.origins.imports.is_empty() {
                return Err(Error::Input(
                    "recording imports have no verified material identity".into(),
                ));
            }
            return Ok(None);
        };
        if preflight(&self.destination)? != self.destination {
            return Err(Error::Input(
                "union destination changed before import staging".into(),
            ));
        }
        let mut journal = RecordingJournal::create(
            &self.destination,
            Manifest {
                schema_version: crate::cassette::SCHEMA_VERSION,
                recording_id: self.physical_id.clone(),
                compatibility: compatibility.clone(),
                concurrency: concurrency as u64,
                tracks: Vec::new(),
                provenance: Some(self.origins.clone()),
            },
            repeat,
            Limits::default(),
        )
        .map_err(recording_error)?;
        let mut by_source = vec![Vec::new(); self.sources.len()];
        for imported in &self.origins.imports {
            by_source[imported.source].push(imported);
        }
        for (index, source) in self.sources.iter().enumerate() {
            let mut archive = Archive::open(source, Limits::default()).map_err(recording_error)?;
            let expected = &self.origins.sources[index];
            if archive.manifest().recording_id != expected.recording_id
                || archive.manifest().concurrency != concurrency as u64
                || crate::cassette::digest(
                    &serde_json::to_vec(archive.manifest())
                        .map_err(|error| Error::Io(error.to_string()))?,
                ) != expected.manifest_sha256
            {
                return Err(Error::Input(
                    "recording origin changed before import admission".into(),
                ));
            }
            archive
                .manifest()
                .compatibility
                .require_match(compatibility)
                .map_err(recording_error)?;
            for imported in &by_source[index] {
                let track = archive
                    .manifest()
                    .track(imported.slot)
                    .map_err(recording_error)?
                    .clone();
                let payload = archive.read_track(imported.slot).map_err(recording_error)?;
                let mut artifact_payloads = Vec::new();
                for artifact in &track.artifacts {
                    artifact_payloads.push((
                        artifact.clone(),
                        std::io::Cursor::new(
                            archive
                                .read_artifact(imported.slot, &artifact.kind)
                                .map_err(recording_error)?,
                        ),
                    ));
                }
                let artifact_inputs = artifact_payloads
                    .iter_mut()
                    .map(|(artifact, bytes)| crate::cassette::ArtifactInput {
                        artifact: artifact.clone(),
                        source: bytes,
                    })
                    .collect();
                journal
                    .import_track_with_artifacts(
                        track,
                        index,
                        &mut payload.as_slice(),
                        artifact_inputs,
                    )
                    .map_err(recording_error)?;
            }
        }
        Ok(Some(journal))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Published {
    schema_version: u32,
    scope: String,
    binding: Binding,
    physical_id: String,
    destination: PathBuf,
    manifest_sha256: String,
    archive_bytes: u64,
    archive_sha256: String,
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Current {
    schema_version: u32,
    scope: String,
    publication: PathBuf,
    content: crate::runner::campaign::generation::Blob,
}

fn publish_exact(root: &Path, relative: &Path, value: &impl Serialize) -> Result<(), Error> {
    use crate::runner::campaign::reading;
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| Error::Io(error.to_string()))?;
    if bytes.len() > 64 * 1024 {
        return Err(Error::Input(
            "union publication metadata exceeds 64 KiB".into(),
        ));
    }
    if reading::present_relative(root, relative)? {
        let (file, observed) = reading::pinned_bytes(root, relative, 64 * 1024)?;
        if observed != bytes {
            return Err(Error::Input(
                "union publication conflicts with retained metadata; no overwrite".into(),
            ));
        }
        publication_point(relative, "existing-before-file-sync")?;
        file.sync_all().map_err(io_error)?;
        publication_point(relative, "existing-before-directory-sync")?;
        File::open(root.join(relative.parent().unwrap_or_else(|| Path::new(""))))
            .and_then(|directory| directory.sync_all())
            .map_err(io_error)?;
        return Ok(());
    }
    crate::runner::repeated::publish_bytes_after_link(&root.join(relative), &bytes, || {
        publication_point(relative, "after-link")
    })
}

#[cfg(test)]
std::thread_local! {
    static PUBLICATION_FAULT: std::cell::Cell<Option<(&'static str, &'static str)>> = const { std::cell::Cell::new(None) };
    static PUBLICATION_POINTS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn publication_point(_relative: &Path, _point: &str) -> Result<(), Error> {
    #[cfg(test)]
    {
        let name = _relative
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        PUBLICATION_POINTS.with_borrow_mut(|points| points.push(format!("{name}:{_point}")));
        if PUBLICATION_FAULT.get() == Some((name, _point)) {
            PUBLICATION_FAULT.set(None);
            return Err(Error::Io(format!(
                "injected union publication failure: {_point}"
            )));
        }
    }
    Ok(())
}

pub(crate) fn publish(
    root: &Path,
    descriptor: &Descriptor,
    binding: &Binding,
    publication: &Publication,
    current_relative: &Path,
) -> Result<(), Error> {
    let prepared = prepare_publication(root, descriptor, binding, publication, current_relative)?;
    prepared.preflight()?;
    prepared.commit()
}

pub(crate) struct PublicationPlan {
    root: PathBuf,
    receipt_path: PathBuf,
    receipt: Published,
    current: Current,
    current_relative: PathBuf,
}

impl PublicationPlan {
    /// Read-only checks across every configuration precede any ledger writes.
    pub(crate) fn preflight(&self) -> Result<(), Error> {
        use crate::runner::campaign::reading;
        for (relative, expected) in [
            (&self.receipt_path, serde_json::to_vec_pretty(&self.receipt)),
            (
                &self.current_relative,
                serde_json::to_vec_pretty(&self.current),
            ),
        ] {
            let expected = expected.map_err(|error| Error::Io(error.to_string()))?;
            if expected.len() > 64 * 1024 {
                return Err(Error::Input(
                    "union publication metadata exceeds 64 KiB".into(),
                ));
            }
            if reading::present_relative(&self.root, relative)?
                && reading::bounded_relative(&self.root, relative, 64 * 1024)? != expected
            {
                return Err(Error::Input(
                    "union publication conflicts with retained metadata; no overwrite".into(),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn commit(self) -> Result<(), Error> {
        publish_exact(&self.root, &self.receipt_path, &self.receipt)?;
        publish_exact(&self.root, &self.current_relative, &self.current)
    }
}

pub(crate) fn prepare_publication(
    root: &Path,
    descriptor: &Descriptor,
    binding: &Binding,
    publication: &Publication,
    current_relative: &Path,
) -> Result<PublicationPlan, Error> {
    let generation_current = PathBuf::from(".asb-generations")
        .join(&binding.reference.generation)
        .join("recording-current.json");
    if current_relative != Path::new("recording-current.json")
        && current_relative != generation_current
    {
        return Err(Error::Input(
            "union publication ledger is not a derived generation location".into(),
        ));
    }
    if publication.destination != descriptor.destination
        || publication.manifest.recording_id != descriptor.physical_id
        || publication.manifest.provenance.as_ref() != Some(&descriptor.origins)
        || binding.origins_sha256 != descriptor.origins.commitment().map_err(recording_error)?
    {
        return Err(Error::Input(
            "confirmed recording differs from frozen union publication".into(),
        ));
    }
    let receipt = Published {
        schema_version: 1,
        scope: "confirmed-recording-union-publication".into(),
        binding: binding.clone(),
        physical_id: descriptor.physical_id.clone(),
        destination: descriptor.destination.clone(),
        manifest_sha256: crate::cassette::digest(
            &serde_json::to_vec(&publication.manifest)
                .map_err(|error| Error::Io(error.to_string()))?,
        ),
        archive_bytes: publication.archive_bytes,
        archive_sha256: publication.archive_sha256.clone(),
    };
    let relative = PathBuf::from(".asb-generations")
        .join(&binding.reference.generation)
        .join("recording-published.json");
    let bytes =
        serde_json::to_vec_pretty(&receipt).map_err(|error| Error::Io(error.to_string()))?;
    // Immutable current reference is LAST. A failed publication cannot redirect
    // default replay to an unconfirmed ZIP or silently abandon an old pointer.
    Ok(PublicationPlan {
        root: root.to_path_buf(),
        receipt_path: relative.clone(),
        receipt,
        current_relative: current_relative.to_path_buf(),
        current: Current {
            schema_version: 1,
            scope: "current-recording-union-reference".into(),
            publication: relative,
            content: crate::runner::campaign::generation::Blob::of(&bytes),
        },
    })
}

/// Resolve an explicit immutable publication through the owner's loaded catalog.
/// Payload verification remains the normal bounded selected-track replay path.
pub(crate) fn current(
    root: &Path,
    catalog: &crate::runner::campaign::generation::Catalog,
    configuration: usize,
) -> Result<Option<PathBuf>, Error> {
    use crate::runner::campaign::reading;
    let Some(reference) = catalog.recording_reference(configuration)? else {
        if reading::present_relative(root, Path::new("recording-current.json"))? {
            return Err(Error::Input(
                "recording current reference has no admitted union".into(),
            ));
        }
        return Ok(None);
    };
    confirmed(root, catalog, &reference).map(Some)
}

pub(crate) fn confirmed(
    root: &Path,
    catalog: &crate::runner::campaign::generation::Catalog,
    reference: &crate::runner::campaign::generation::StartReference,
) -> Result<PathBuf, Error> {
    use crate::runner::campaign::reading;
    let configuration = reference.configuration;
    let current_relative = catalog.recording_current_path(reference)?;
    if !reading::present_relative(root, &current_relative)? {
        return Err(Error::Input("admitted recording union has no confirmed current reference; use sweep --resume --record --recover-recording before default replay".into()));
    }
    let current: Current = serde_json::from_slice(&reading::bounded_relative(
        root,
        &current_relative,
        64 * 1024,
    )?)
    .map_err(|_| Error::Input("malformed current recording reference".into()))?;
    let relative = PathBuf::from(".asb-generations")
        .join(&reference.generation)
        .join("recording-published.json");
    if current.schema_version != 1
        || current.scope != "current-recording-union-reference"
        || current.publication != relative
    {
        return Err(Error::Input(
            "current recording reference is outside its admitted generation".into(),
        ));
    }
    let bytes = reading::bounded_relative(root, &relative, 64 * 1024)?;
    if !current.content.matches(&bytes) {
        return Err(Error::Input(
            "current recording publication bytes changed".into(),
        ));
    }
    let receipt: Published = serde_json::from_slice(&bytes)
        .map_err(|_| Error::Input("malformed union publication receipt".into()))?;
    let descriptor = catalog.recording_descriptor(configuration, &receipt.binding.reference)?;
    let archive = Archive::open_verified(
        &descriptor.destination,
        receipt.archive_bytes,
        &receipt.archive_sha256,
        Limits::default(),
    )
    .map_err(recording_error)?;
    if receipt.schema_version != 1
        || receipt.binding.reference != *reference
        || receipt.scope != "confirmed-recording-union-publication"
        || receipt.physical_id != descriptor.physical_id
        || receipt.destination != descriptor.destination
        || receipt.binding.origins_sha256
            != descriptor.origins.commitment().map_err(recording_error)?
        || archive.manifest().recording_id != descriptor.physical_id
        || archive.manifest().provenance.as_ref() != Some(&descriptor.origins)
        || crate::cassette::digest(
            &serde_json::to_vec(archive.manifest())
                .map_err(|error| Error::Io(error.to_string()))?,
        ) != receipt.manifest_sha256
    {
        return Err(Error::Input(
            "current union differs from its confirmed publication".into(),
        ));
    }
    Ok(descriptor.destination.clone())
}

#[cfg(test)]
mod tests;
