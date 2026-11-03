//! Read-only immutable track evidence; never acquires writer/recovery custody.
use super::*;

/// Whole-operation budgets, distinct from each archive's format limits.
/// They bound the distinct selected input set; a published catalogue may be
/// read twice while only one descriptor and one decoded track are retained.
#[derive(Clone, Copy, Debug)]
pub struct ProofLimits {
    /// Per-recording format and decompression limits.
    pub archive: Limits,
    /// Supplied reference occurrences, including identical duplicates.
    pub max_references: usize,
    /// Encoded reference bytes, counted without allocating encoded copies.
    pub max_reference_bytes: u64,
    /// Combined journal records and published manifest bytes.
    pub max_metadata_bytes: u64,
    /// Combined central-directory/end-record snapshots.
    pub max_directory_bytes: u64,
    /// Combined compressed lengths of selected published recordings.
    pub max_archive_bytes: u64,
    /// Combined uncompressed lengths of distinct selected tracks.
    pub max_selected_bytes: u64,
}

impl Default for ProofLimits {
    fn default() -> Self {
        Self {
            archive: Limits::default(),
            max_references: 65_533,
            max_reference_bytes: 256 * 1024 * 1024,
            max_metadata_bytes: 256 * 1024 * 1024,
            max_directory_bytes: 64 * 1024 * 1024,
            max_archive_bytes: 128 * 1024 * 1024 * 1024,
            max_selected_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

impl ProofLimits {
    /// Validate per-archive and whole-operation resource budgets.
    pub fn validate(self) -> Result<(), Error> {
        self.archive.validate()?;
        if self.max_references == 0
            || self.max_reference_bytes == 0
            || self.max_metadata_bytes == 0
            || self.max_directory_bytes == 0
            || self.max_archive_bytes == 0
            || self.max_selected_bytes == 0
        {
            return Err(Error("track evidence budgets must be positive".into()));
        }
        Ok(())
    }
}

/// Exact committed track identity, independent of growing archive/staging bytes.
/// The journal retains immutable metadata after payload cleanup.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TrackEvidence {
    /// Evidence format, distinct from the manifest schema.
    pub schema_version: u32,
    /// Canonical final destination; no implicit recovery or recreation.
    pub destination: PathBuf,
    /// Stable journal/manifest identity.
    pub recording_id: String,
    /// Full replay compatibility, not scheduling settings.
    pub compatibility: super::super::Compatibility,
    /// Recording batch width, for stable slot identity validation.
    pub concurrency: u64,
    /// Immutable committed metadata and exact uncompressed payload identity.
    pub track: Track,
    /// Optional immutable origin commitment, checked against the actual journal.
    #[serde(default)]
    pub origin_sha256: Option<String>,
}

impl TrackEvidence {
    /// Validate bounded metadata without opening artifacts.
    pub fn validate(&self, limits: Limits) -> Result<(), Error> {
        if self.schema_version != 1
            || !self.destination.is_absolute()
            || self.recording_id.len() > 128
            || self
                .origin_sha256
                .as_ref()
                .is_some_and(|digest| !super::super::valid_digest(digest))
        {
            return Err(Error("invalid immutable recording track reference".into()));
        }
        self.compatibility.validate()?;
        Manifest {
            schema_version: super::super::SCHEMA_VERSION,
            recording_id: self.recording_id.clone(),
            compatibility: self.compatibility.clone(),
            concurrency: self.concurrency,
            tracks: vec![self.track.clone()],
            provenance: None,
        }
        .validate(limits)
    }

    /// Verify immutable commit metadata plus staged bytes or a selected published
    /// ZIP entry. Never acquires a writer lock, recovers state or starts a client.
    /// A disappeared published target cannot fall back to leftover staging.
    pub fn verify(&self, limits: Limits) -> Result<(), Error> {
        // Historical single proofs have separate journal, publication-marker
        // and manifest allowances. Do not collapse those into one manifest cap.
        Self::verify_batch(
            std::slice::from_ref(self),
            ProofLimits {
                archive: limits,
                max_references: 1,
                max_metadata_bytes: limits
                    .max_manifest_bytes
                    .saturating_mul(2)
                    .saturating_add(MAX_RECORD_BYTES),
                max_directory_bytes: limits.max_directory_bytes,
                max_archive_bytes: limits.max_archive_bytes,
                max_selected_bytes: limits.max_total_bytes,
                ..ProofLimits::default()
            },
        )
    }

    /// Verify a bounded collection across recordings without acquiring writer
    /// custody. All references and immutable metadata are checked before any
    /// JSONL bytes. Identical references share one selected-payload read and
    /// each published recording uses two bounded metadata passes, not one
    /// catalogue parse per selected track. Only one archive is open at a time.
    ///
    /// Separate batch budgets apply to reference occurrences, encoded references,
    /// journal/manifest metadata, ZIP directories, compressed archives and
    /// unique selected bytes. Distinct input sizes are charged once; the second
    /// metadata pass is constrained to the exact first-pass sizes and receipt.
    /// A recording ID cannot denote two different destinations in one batch.
    /// This checks selected entries, not whole-archive SHA-256 or authentication.
    pub fn verify_batch(expected: &[Self], limits: ProofLimits) -> Result<(), Error> {
        limits.validate()?;
        if expected.len() > limits.max_references {
            return Err(Error("track evidence batch exceeds catalogue limit".into()));
        }
        let mut groups: BTreeMap<&Path, Group<'_>> = BTreeMap::new();
        let mut identities = BTreeMap::new();
        let mut reference_bytes = 0;
        let mut selected_bytes = 0u64;
        for evidence in expected {
            evidence.validate(limits.archive)?;
            reference_bytes += crate::cassette::serialization::encoded_size(
                evidence,
                limits.max_reference_bytes - reference_bytes,
            )
            .map_err(|error| Error(format!("track evidence reference metadata: {error}")))?;
            if let Some(previous) = identities.insert(&evidence.recording_id, &evidence.destination)
                && previous != &evidence.destination
            {
                return Err(Error("recording ID denotes multiple destinations".into()));
            }
            let group = groups
                .entry(&evidence.destination)
                .or_insert_with(|| Group {
                    expected: evidence,
                    tracks: BTreeMap::new(),
                });
            if group.expected.recording_id != evidence.recording_id
                || group.expected.compatibility != evidence.compatibility
                || group.expected.concurrency != evidence.concurrency
                || group.expected.origin_sha256 != evidence.origin_sha256
            {
                return Err(Error(
                    "conflicting recording identities in evidence batch".into(),
                ));
            }
            if let Some(previous) = group.tracks.insert(evidence.track.id, &evidence.track) {
                if previous != &evidence.track {
                    return Err(Error("conflicting duplicate track evidence".into()));
                }
            } else {
                charge(
                    &mut selected_bytes,
                    evidence.track.bytes,
                    limits.max_selected_bytes,
                    "selected payloads",
                )?;
            }
        }
        for group in groups.values() {
            let mut bytes = 0;
            for track in group.tracks.values() {
                charge(
                    &mut bytes,
                    track.bytes,
                    limits.archive.max_total_bytes,
                    "per-recording selected payloads",
                )?;
            }
        }
        let mut budget = Budget {
            metadata: limits.max_metadata_bytes,
            directories: limits.max_directory_bytes,
            archives: 0,
            archive_limit: limits.max_archive_bytes,
        };
        // Retain bounded metadata, not archive descriptors, across this barrier.
        // A sweep may contain more configurations than the process's FD limit.
        let mut prepared = groups
            .into_values()
            .map(|group| Prepared::read(group, limits.archive, &mut budget))
            .collect::<Result<Vec<_>, Error>>()?;
        observed("metadata-ready");
        for group in &mut prepared {
            group.verify(limits.archive, &mut budget)?;
        }
        Ok(())
    }
}

struct Group<'a> {
    expected: &'a TrackEvidence,
    tracks: BTreeMap<u64, &'a Track>,
}

struct Budget {
    metadata: u64,
    directories: u64,
    archives: u64,
    archive_limit: u64,
}

struct Prepared<'a> {
    origin_plan: Option<String>,
    group: Group<'a>,
    root: PathBuf,
    original: fs::Metadata,
    publication: Option<PublicationSnapshot>,
}

struct PublicationSnapshot {
    receipt: super::publication::Published,
    directory: u64,
    manifest: u64,
}

impl<'a> Prepared<'a> {
    fn read(group: Group<'a>, limits: Limits, budget: &mut Budget) -> Result<Self, Error> {
        let expected = group.expected;
        let (destination, root, name) = recording_paths(&expected.destination)?;
        if destination != expected.destination {
            return Err(Error("recording destination identity changed".into()));
        }
        let original = fs::symlink_metadata(&root).map_err(io_error)?;
        check_root_identity(&root, &original)?;
        let journal_allowance = limits.max_manifest_bytes.min(budget.metadata);
        let mut journal_remaining = journal_allowance;
        observed("intent");
        let intent: Intent = read_json(
            &root.join("intent.json"),
            limits.max_manifest_bytes,
            &mut journal_remaining,
        )?;
        intent.manifest.validate(limits)?;
        let count = validate_count(intent.repeat, intent.manifest.concurrency, limits)?;
        let origin_plan = intent
            .manifest
            .provenance
            .as_ref()
            .map(crate::cassette::OriginPlan::commitment)
            .transpose()?;
        if !intent.manifest.tracks.is_empty()
            || origin_plan != expected.origin_sha256
            || intent
                .manifest
                .provenance
                .as_ref()
                .is_some_and(|plan| plan.population != count)
            || intent.destination_name != name
            || intent.manifest.recording_id != expected.recording_id
            || intent.manifest.compatibility != expected.compatibility
            || intent.manifest.concurrency != expected.concurrency
            || group.tracks.keys().any(|id| *id > count)
        {
            return Err(Error("track evidence differs from recording intent".into()));
        }
        for track in group.tracks.values() {
            if let Some(plan) = &intent.manifest.provenance
                && let Some(imported) = plan.imported(track.id)
            {
                plan.require_track(track, imported.source)?;
            }
            let started: Started = read_json(
                &root.join(format!("started-{}.json", track.id)),
                MAX_RECORD_BYTES,
                &mut journal_remaining,
            )?;
            let committed: Committed = read_json(
                &root.join(format!("committed-{}.json", track.id)),
                MAX_RECORD_BYTES,
                &mut journal_remaining,
            )?;
            if started.recording_id != expected.recording_id
                || started.id != track.id
                || started.batch != track.batch
                || started.replica != track.replica
                || started.import_source
                    != intent
                        .manifest
                        .provenance
                        .as_ref()
                        .and_then(|plan| plan.imported(track.id))
                        .map(|imported| imported.source)
                || committed.recording_id != expected.recording_id
                || committed.track != **track
            {
                return Err(Error(
                    "track evidence differs from immutable start/commit metadata".into(),
                ));
            }
        }
        budget.metadata -= journal_allowance - journal_remaining;
        let mut prepared = Self {
            origin_plan,
            group,
            root,
            original,
            publication: None,
        };
        if exists(&prepared.root.join("published.json"))? {
            let (archive, receipt) = prepared.open_archive(limits, budget)?;
            let (directory, manifest) = archive.metadata_bytes();
            prepared.publication = Some(PublicationSnapshot {
                receipt,
                directory,
                manifest,
            });
        }
        check_root_identity(&prepared.root, &prepared.original)?;
        Ok(prepared)
    }

    fn open_archive(
        &self,
        limits: Limits,
        budget: &mut Budget,
    ) -> Result<(crate::cassette::Archive, super::publication::Published), Error> {
        observed("archive");
        let (archive, receipt) = super::publication::open_selected_publication(
            &self.root,
            self.group.expected,
            &self.group.tracks,
            Limits {
                max_manifest_bytes: limits.max_manifest_bytes.min(budget.metadata),
                max_directory_bytes: limits.max_directory_bytes.min(budget.directories),
                max_archive_bytes: limits
                    .max_archive_bytes
                    .min(budget.archive_limit - budget.archives),
                ..limits
            },
            &mut budget.metadata,
        )?;
        if archive
            .manifest()
            .provenance
            .as_ref()
            .map(crate::cassette::OriginPlan::commitment)
            .transpose()?
            != self.origin_plan
        {
            return Err(Error(
                "published origin plan differs from immutable recording intent".into(),
            ));
        }
        charge(
            &mut budget.archives,
            receipt.archive_bytes,
            budget.archive_limit,
            "compressed archives",
        )?;
        let (directory, manifest) = archive.metadata_bytes();
        budget.directories = budget
            .directories
            .checked_sub(directory)
            .ok_or_else(|| Error("track evidence ZIP directories exceed aggregate limit".into()))?;
        budget.metadata = budget.metadata.checked_sub(manifest).ok_or_else(|| {
            Error("track evidence manifest metadata exceeds aggregate limit".into())
        })?;
        Ok((archive, receipt))
    }

    fn verify(&mut self, limits: Limits, budget: &mut Budget) -> Result<(), Error> {
        check_root_identity(&self.root, &self.original)?;
        let mut archive = if let Some(snapshot) = &self.publication {
            observed("archive");
            let mut remaining = snapshot.manifest.saturating_add(MAX_RECORD_BYTES);
            let (archive, receipt) = super::publication::open_selected_publication(
                &self.root,
                self.group.expected,
                &self.group.tracks,
                Limits {
                    max_archive_bytes: snapshot.receipt.archive_bytes,
                    max_directory_bytes: snapshot.directory,
                    max_manifest_bytes: snapshot.manifest,
                    ..limits
                },
                &mut remaining,
            )?;
            if receipt != snapshot.receipt
                || archive.metadata_bytes() != (snapshot.directory, snapshot.manifest)
            {
                return Err(Error(
                    "published recording metadata changed between verification passes".into(),
                ));
            }
            Some(archive)
        } else {
            None
        };
        if archive.is_none() {
            for track in self.group.tracks.values() {
                observed("staged");
                let staged = hash_file(
                    &self.root.join(format!("track-{}.jsonl", track.id)),
                    track.bytes,
                );
                // A durable publication may remove staged bytes during a read.
                // Switch the whole group once; never fall back after publication.
                if exists(&self.root.join("published.json"))? {
                    archive = Some(self.open_archive(limits, budget)?.0);
                    break;
                }
                if staged? != (track.bytes, track.sha256.clone()) {
                    return Err(Error("immutable staged track bytes differ".into()));
                }
            }
        }
        if let Some(archive) = &mut archive {
            for id in self.group.tracks.keys() {
                observed("payload");
                archive.read_track(*id)?;
            }
        }
        check_root_identity(&self.root, &self.original)
    }
}

fn charge(used: &mut u64, bytes: u64, limit: u64, kind: &str) -> Result<(), Error> {
    *used = used
        .checked_add(bytes)
        .filter(|value| *value <= limit)
        .ok_or_else(|| Error(format!("track evidence {kind} exceed aggregate limit")))?;
    Ok(())
}

pub(in crate::cassette) fn observed(_event: &'static str) {
    #[cfg(test)]
    {
        OBSERVATIONS.with_borrow_mut(|events| *events.entry(_event).or_default() += 1);
        HOOK.with_borrow_mut(|hook| {
            if let Some(hook) = hook {
                hook(_event);
            }
        });
    }
}

#[cfg(test)]
std::thread_local! {
    static OBSERVATIONS: std::cell::RefCell<BTreeMap<&'static str, u64>> = const { std::cell::RefCell::new(BTreeMap::new()) };
    static HOOK: std::cell::RefCell<Option<ObservationHook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
type ObservationHook = Box<dyn FnMut(&'static str)>;

#[cfg(test)]
pub(in crate::cassette) fn take_observations() -> BTreeMap<&'static str, u64> {
    OBSERVATIONS.with_borrow_mut(std::mem::take)
}

#[cfg(test)]
pub(in crate::cassette) fn set_hook(hook: Option<ObservationHook>) {
    HOOK.set(hook);
}

impl RecordingJournal {
    /// Issue a reference only after independently verifying committed bytes.
    pub fn track_evidence(&self, track: &Track) -> Result<TrackEvidence, Error> {
        self.verify_track_evidence(track)?;
        Ok(TrackEvidence {
            schema_version: 1,
            destination: self.destination.clone(),
            recording_id: self.intent.manifest.recording_id.clone(),
            compatibility: self.intent.manifest.compatibility.clone(),
            concurrency: self.intent.manifest.concurrency,
            track: track.clone(),
            origin_sha256: self.origin_commitment.clone(),
        })
    }
}
