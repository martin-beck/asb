//! Immutable recording intent/start/commit records under one output owner.
mod evidence;
mod publication;
mod reservation;
use super::{Artifact, Error, Limits, Manifest, NativeTrack, Statistics, Track, digest};
use crate::output_lock::OutputLock;
#[cfg(test)]
pub(super) use evidence::observed;
pub use evidence::{ProofLimits, TrackEvidence};
#[cfg(test)]
pub(super) use evidence::{set_hook, take_observations};
pub use publication::Publication;
pub use reservation::RecordingReservation;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
const MAX_RECORD_BYTES: u64 = 64 * 1024;

/// Bytes for one named auxiliary payload, staged atomically with its provider
/// transcript.  It is intentionally journal-owned: callers cannot attach a
/// loose sidecar after a track is committed.
pub struct ArtifactInput<'a> {
    /// Immutable catalogue metadata for this payload.
    pub artifact: Artifact,
    /// Exact bytes to stage and verify against [`Self::artifact`].
    pub source: &'a mut dyn Read,
}

/// Bytes for a sealed native sidecar. The journal treats this as opaque JSONL;
/// a future native ingress owns event semantics and must not attach it after
/// the provider track has committed.
pub struct NativeInput<'a> {
    /// Immutable metadata for the native sidecar.
    pub native: NativeTrack,
    /// Exact bytes to stage and verify against [`Self::native`].
    pub source: &'a mut dyn Read,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Intent<M = Manifest> {
    manifest: M,
    repeat: u64,
    destination_name: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Started {
    recording_id: String,
    id: u64,
    batch: u64,
    replica: u64,
    /// Artifact-copy intent, not a new execution start. Historical starts omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    import_source: Option<usize>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Committed {
    recording_id: String,
    track: Track,
}

/// A single writer's recoverable partial recording; dropping retains all data.
///
/// Started slots and completed records are immutable, atomically published and
/// synced. Recovering never launches an execution, assigns a grade or makes an
/// uncommitted trace automatically complete. Final ZIP publication is separate.
pub struct RecordingJournal {
    _owner: OutputLock,
    root: PathBuf,
    destination: PathBuf,
    intent: Intent,
    // Derived once from validated immutable intent, never loaded from the wire.
    origin_commitment: Option<String>,
    starts: BTreeMap<u64, Started>,
    commits: BTreeMap<u64, Track>,
    limits: Limits,
    staged_bytes: u64,
    metadata_bytes: u64,
    recovery_required: bool,
    publication_fault: PublicationFault,
    root_identity: fs::Metadata,
    finalization: Option<Manifest>,
    allow_existing_destination: bool,
}

impl RecordingJournal {
    /// Reserve a new partial directory and publish immutable recording intent.
    /// Refuses existing final/partial destinations and validates limits first.
    pub fn create(
        destination: &Path,
        manifest: Manifest,
        repeat: u64,
        limits: Limits,
    ) -> Result<Self, Error> {
        manifest.validate(limits)?;
        if !manifest.tracks.is_empty() {
            return Err(Error(
                "a new recording journal must start with an empty track catalogue".into(),
            ));
        }
        let count = validate_count(repeat, manifest.concurrency, limits)?;
        if manifest
            .provenance
            .as_ref()
            .is_some_and(|plan| plan.population != count)
        {
            return Err(Error(
                "origin population differs from recording repeat/concurrency".into(),
            ));
        }
        let (_, _, name) = recording_paths(destination)?;
        let intent = Intent {
            manifest,
            repeat,
            destination_name: name,
        };
        let origin_commitment = intent
            .manifest
            .provenance
            .as_ref()
            .map(super::OriginPlan::commitment)
            .transpose()?;
        let encoded = serde_json::to_vec(&intent).map_err(json_error)?;
        reserve_metadata(&intent, count, limits)?;
        let (destination, root, _, owner) = paths_and_lock(destination)?;
        if exists(&destination)? {
            return Err(Error(
                "recording destination already exists; choose a new cassette name".into(),
            ));
        }
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&root).map_err(io_error)?;
        let root_identity = fs::symlink_metadata(&root).map_err(io_error)?;
        publish_new(&root, "intent.json", &encoded, &PublicationFault::default())?;
        sync_directory(root.parent().expect("canonical parent"))?;
        Ok(Self {
            _owner: owner,
            root,
            destination,
            intent,
            origin_commitment,
            starts: BTreeMap::new(),
            commits: BTreeMap::new(),
            limits,
            staged_bytes: 0,
            metadata_bytes: encoded.len() as u64,
            recovery_required: false,
            publication_fault: PublicationFault::default(),
            root_identity,
            finalization: None,
            allow_existing_destination: false,
        })
    }

    /// Acquire the original owner lock and reconstruct committed/started state.
    /// Missing or changed committed payloads are errors, not retry permission.
    pub fn recover(destination: &Path, limits: Limits) -> Result<Self, Error> {
        limits.validate()?;
        let (destination, root, name, owner) = paths_and_lock(destination)?;
        let root_identity = fs::symlink_metadata(&root).map_err(io_error)?;
        if !root_identity.is_dir() {
            return Err(Error(
                "recording partial path must be a directory, not a symlink".into(),
            ));
        }
        let mut metadata_remaining = limits.max_manifest_bytes;
        let intent: Intent = read_json(
            &root.join("intent.json"),
            limits.max_manifest_bytes,
            &mut metadata_remaining,
        )?;
        intent.manifest.validate(limits)?;
        let count = validate_count(intent.repeat, intent.manifest.concurrency, limits)?;
        if intent
            .manifest
            .provenance
            .as_ref()
            .is_some_and(|plan| plan.population != count)
        {
            return Err(Error(
                "origin population differs from recording intent".into(),
            ));
        }
        if intent.destination_name != name || !intent.manifest.tracks.is_empty() {
            return Err(Error(
                "recording intent does not match this destination".into(),
            ));
        }
        let origin_commitment = intent
            .manifest
            .provenance
            .as_ref()
            .map(super::OriginPlan::commitment)
            .transpose()?;
        let mut journal = Self {
            _owner: owner,
            root,
            destination,
            intent,
            origin_commitment,
            starts: BTreeMap::new(),
            commits: BTreeMap::new(),
            limits,
            staged_bytes: 0,
            metadata_bytes: 0,
            recovery_required: false,
            publication_fault: PublicationFault::default(),
            root_identity,
            finalization: None,
            allow_existing_destination: false,
        };
        // Known slots bound enumeration; unknown files remain diagnostic only.
        for id in 1..=count {
            let start_path = journal.root.join(format!("started-{id}.json"));
            let commit_path = journal.root.join(format!("committed-{id}.json"));
            let start_present = exists(&start_path)?;
            let commit_present = exists(&commit_path)?;
            if !start_present {
                if commit_present {
                    return Err(Error("committed track has no durable start intent".into()));
                }
                continue;
            }
            let started: Started =
                read_json(&start_path, MAX_RECORD_BYTES, &mut metadata_remaining)?;
            let expected = journal.slot(started.batch, started.replica)?;
            if expected != id
                || started.id != id
                || started.recording_id != journal.intent.manifest.recording_id
                || started.import_source
                    != journal
                        .intent
                        .manifest
                        .provenance
                        .as_ref()
                        .and_then(|plan| plan.imported(id))
                        .map(|imported| imported.source)
            {
                return Err(Error(
                    "recording start identity differs from planned slot".into(),
                ));
            }
            journal.starts.insert(id, started);
            if commit_present {
                let committed: Committed =
                    read_json(&commit_path, MAX_RECORD_BYTES, &mut metadata_remaining)?;
                if committed.recording_id != journal.intent.manifest.recording_id
                    || committed.track.id != id
                {
                    return Err(Error(
                        "recording commit belongs to a different recording/slot".into(),
                    ));
                }
                journal.validate_track(&committed.track)?;
                journal.commits.insert(id, committed.track);
            }
        }
        journal.metadata_bytes = limits.max_manifest_bytes - metadata_remaining;
        journal.recover_finalization()?;
        journal.staged_bytes = journal
            .snapshot()?
            .tracks
            .iter()
            .try_fold(0u64, |total, track| {
                let total = total.checked_add(track.bytes)?;
                let total = track
                    .artifacts
                    .iter()
                    .try_fold(total, |total, artifact| total.checked_add(artifact.bytes))?;
                track
                    .native
                    .as_ref()
                    .map_or(Some(total), |native| total.checked_add(native.bytes))
            })
            .ok_or_else(|| Error("recording staged byte count overflow".into()))?;
        Ok(journal)
    }

    /// Record a local slot intent before launching processes, not launch evidence.
    /// A duplicate start is never interpreted as permission to retry.
    pub fn begin(&mut self, batch: u64, replica: u64) -> Result<u64, Error> {
        self.check_root()?;
        self.ensure_recording()?;
        let id = self.slot(batch, replica)?;
        if self
            .intent
            .manifest
            .provenance
            .as_ref()
            .and_then(|plan| plan.imported(id))
            .is_some()
        {
            return Err(Error(
                "imported slots cannot begin another local execution".into(),
            ));
        }
        if self.starts.contains_key(&id) {
            return Err(Error(format!(
                "recording track {id} has already started; refusing implicit retry"
            )));
        }
        let started = Started {
            recording_id: self.intent.manifest.recording_id.clone(),
            id,
            batch,
            replica,
            import_source: None,
        };
        self.publish_record(
            &format!("started-{id}.json"),
            &serde_json::to_vec(&started).map_err(json_error)?,
        )?;
        self.starts.insert(id, started);
        Ok(id)
    }

    /// Stage exact bounded bytes and publish immutable completion metadata.
    ///
    /// The caller must derive completeness from recording finalization evidence,
    /// never process exit alone. Failed staging retains its uncommitted payload
    /// for inspection; another commit cannot overwrite it.
    pub fn commit(&mut self, track: Track, source: &mut dyn Read) -> Result<(), Error> {
        self.commit_with_native(track, source, Vec::new(), None)
    }

    /// Stage a provider transcript and every catalogue-declared auxiliary
    /// payload before publishing one immutable track commit.
    pub fn commit_with_artifacts(
        &mut self,
        track: Track,
        source: &mut dyn Read,
        artifacts: Vec<ArtifactInput<'_>>,
    ) -> Result<(), Error> {
        self.commit_with_native(track, source, artifacts, None)
    }

    /// Stage a provider transcript, every declared artifact, and an optional
    /// sealed native sidecar before publishing one immutable track commit.
    pub fn commit_with_native(
        &mut self,
        mut track: Track,
        source: &mut dyn Read,
        artifacts: Vec<ArtifactInput<'_>>,
        native: Option<NativeInput<'_>>,
    ) -> Result<(), Error> {
        if self
            .intent
            .manifest
            .provenance
            .as_ref()
            .and_then(|plan| plan.imported(track.id))
            .is_some()
        {
            return Err(Error("imported slots require the artifact-copy API".into()));
        }
        if !track.artifacts.is_empty() || track.native.is_some() {
            return Err(Error(
                "caller must supply auxiliary payloads through the journal bundle API".into(),
            ));
        }
        track.artifacts = artifacts
            .iter()
            .map(|input| input.artifact.clone())
            .collect();
        track.native = native.as_ref().map(|input| input.native.clone());
        self.commit_bundle(track, source, artifacts, native)
    }

    /// Import a reserved original track without manufacturing an execution,
    /// capture receipt or grade. The owner verifies its source archive/receipt;
    /// this writer independently checks complete metadata and exact bytes.
    pub fn import_track(
        &mut self,
        track: Track,
        source_index: usize,
        source: &mut dyn Read,
    ) -> Result<(), Error> {
        self.import_track_with_artifacts(track, source_index, source, Vec::new())
    }

    /// Import a reserved original track together with every sealed auxiliary
    /// payload it declares.  A continuation must preserve local-tool and
    /// deterministic sidecars exactly; publishing only the provider body
    /// would produce a cassette that cannot safely replay.
    pub fn import_track_with_artifacts(
        &mut self,
        track: Track,
        source_index: usize,
        source: &mut dyn Read,
        artifacts: Vec<ArtifactInput<'_>>,
    ) -> Result<(), Error> {
        self.check_root()?;
        self.ensure_recording()?;
        self.intent
            .manifest
            .provenance
            .as_ref()
            .ok_or_else(|| Error("recording has no frozen import plan".into()))?
            .require_track(&track, source_index)?;
        if self.slot(track.batch, track.replica)? != track.id || self.starts.contains_key(&track.id)
        {
            return Err(Error(
                "imported slot is foreign or already has an intent".into(),
            ));
        }
        let started = Started {
            recording_id: self.intent.manifest.recording_id.clone(),
            id: track.id,
            batch: track.batch,
            replica: track.replica,
            import_source: Some(source_index),
        };
        self.publish_record(
            &format!("started-{}.json", track.id),
            &serde_json::to_vec(&started).map_err(json_error)?,
        )?;
        self.starts.insert(track.id, started);
        self.commit_bundle(track, source, artifacts, None)
    }

    /// Explicitly finish an artifact import after interruption. This never
    /// retries an execution or overwrites retained payloads. An existing exact
    /// payload can receive its missing immutable commit; partial bytes remain
    /// diagnostic evidence and cannot be replaced through this operation.
    pub fn resume_import(
        &mut self,
        track: Track,
        source_index: usize,
        source: &mut dyn Read,
    ) -> Result<(), Error> {
        self.check_root()?;
        self.ensure_recording()?;
        self.intent
            .manifest
            .provenance
            .as_ref()
            .ok_or_else(|| Error("recording has no frozen import plan".into()))?
            .require_track(&track, source_index)?;
        let started = self
            .starts
            .get(&track.id)
            .ok_or_else(|| Error("import has no durable artifact-copy intent".into()))?;
        if started.recording_id != self.intent.manifest.recording_id
            || started.id != track.id
            || started.batch != track.batch
            || started.replica != track.replica
            || started.import_source != Some(source_index)
        {
            return Err(Error(
                "recovered import intent differs from its frozen origin".into(),
            ));
        }
        self.validate_track(&track)?;
        if self.commits.contains_key(&track.id) {
            return Err(Error(
                "import is already committed; refusing duplicate recovery".into(),
            ));
        }
        if !track.artifacts.is_empty() {
            return Err(Error(
                "artifact-bearing imports require an auxiliary artifact-copy API".into(),
            ));
        }
        // commit_payload's create-new open is authoritative even if a foreign
        // pathname appears after this check. No retained bytes are truncated.
        if exists(&self.payload_path(track.id))? {
            let mut file = open_regular(&self.payload_path(track.id), track.bytes)?;
            let (bytes, sha256) = hash_open_file(&mut file, track.bytes)?;
            if bytes != track.bytes || sha256 != track.sha256 {
                return Err(Error(
                    "partial import payload retained without overwrite".into(),
                ));
            }
            // Readable cached bytes are not durable bytes: the interrupted
            // copier may never have reached its sync. Hash and sync the same
            // descriptor before publishing the immutable completion metadata.
            self.publication_fault
                .check("before-import-recovery-sync")?;
            file.sync_all().map_err(io_error)?;
            return self.publish_commit(track);
        }
        self.commit_bundle(track, source, Vec::new(), None)
    }

    fn commit_bundle(
        &mut self,
        track: Track,
        source: &mut dyn Read,
        artifacts: Vec<ArtifactInput<'_>>,
        native: Option<NativeInput<'_>>,
    ) -> Result<(), Error> {
        self.check_root()?;
        self.ensure_recording()?;
        self.validate_track(&track)?;
        if track.artifacts.len() != artifacts.len()
            || track
                .artifacts
                .iter()
                .zip(&artifacts)
                .any(|(declared, supplied)| declared != &supplied.artifact)
        {
            return Err(Error(
                "track auxiliary artifact catalogue differs from supplied payloads".into(),
            ));
        }
        if track.native.as_ref() != native.as_ref().map(|input| &input.native) {
            return Err(Error(
                "track native sidecar metadata differs from supplied payload".into(),
            ));
        }
        if self.commits.contains_key(&track.id) {
            return Err(Error(format!(
                "recording track {} is already committed",
                track.id
            )));
        }
        self.ensure_commit_metadata_capacity(&track)?;
        let bundle_bytes = track
            .artifacts
            .iter()
            .try_fold(track.bytes, |total, artifact| {
                total.checked_add(artifact.bytes)
            })
            .ok_or_else(|| Error("recording payload length overflow".into()))?;
        let bundle_bytes = if let Some(native) = &track.native {
            bundle_bytes
                .checked_add(native.bytes)
                .ok_or_else(|| Error("recording payload length overflow".into()))?
        } else {
            bundle_bytes
        };
        if self
            .staged_bytes
            .checked_add(bundle_bytes)
            .is_none_or(|total| total > self.limits.max_total_bytes)
        {
            return Err(Error("recording exceeds total staged byte limit".into()));
        }
        self.stage_payload(
            &self.payload_path(track.id),
            track.bytes,
            &track.sha256,
            source,
        )?;
        for input in artifacts {
            self.stage_payload(
                &self.artifact_path(track.id, &input.artifact.kind),
                input.artifact.bytes,
                &input.artifact.sha256,
                input.source,
            )?;
        }
        if let Some(input) = native {
            self.stage_native_payload(track.id, &input.native, input.source)?;
        }
        #[cfg(test)]
        self.publication_fault.check("before-payload-commit")?;
        self.publish_commit(track)
    }

    fn stage_native_payload(
        &mut self,
        id: u64,
        native: &NativeTrack,
        source: &mut dyn Read,
    ) -> Result<(), Error> {
        self.stage_payload(&self.native_path(id), native.bytes, &native.sha256, source)?;
        let file = open_regular(&self.native_path(id), native.bytes)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(usize::try_from(native.bytes).map_err(|_| {
                Error("native sidecar length cannot fit local address space".into())
            })?)
            .map_err(|_| Error("cannot allocate bounded native sidecar inspection".into()))?;
        file.take(native.bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() as u64 != native.bytes {
            return Err(Error(
                "native sidecar changed during staged inspection".into(),
            ));
        }
        let events = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
        if events != native.events || (!bytes.is_empty() && !bytes.ends_with(b"\n")) {
            return Err(Error(
                "native sidecar event framing mismatch; incomplete payload retained".into(),
            ));
        }
        Ok(())
    }

    fn stage_payload(
        &mut self,
        path: &Path,
        expected_bytes: u64,
        expected_sha256: &str,
        source: &mut dyn Read,
    ) -> Result<(), Error> {
        let mut file = create_private(path)?;
        let mut hash = Sha256::new();
        let mut length = 0u64;
        let mut buffer = [0; 16 * 1024];
        loop {
            let read = source.read(&mut buffer).map_err(io_error)?;
            if read == 0 {
                break;
            }
            length = length
                .checked_add(read as u64)
                .ok_or_else(|| Error("recording payload length overflow".into()))?;
            if length > expected_bytes {
                return Err(Error(
                    "recording payload exceeds declared length; incomplete payload retained".into(),
                ));
            }
            hash.update(&buffer[..read]);
            // Count before writing: a short/failed write can leave some bytes.
            // Conservative accounting prevents later staging from exceeding the
            // budget; explicit recovery measures the retained actual length.
            self.staged_bytes += read as u64;
            file.write_all(&buffer[..read]).map_err(io_error)?;
        }
        file.sync_all().map_err(io_error)?;
        if length != expected_bytes || format!("{:x}", hash.finalize()) != expected_sha256 {
            return Err(Error(
                "recording payload length/checksum mismatch; incomplete payload retained".into(),
            ));
        }
        Ok(())
    }

    /// Reject an auxiliary catalogue that would make either the immutable
    /// commit record or eventual manifest unparsable before any payload file
    /// is created.  A failed record must retain no ambiguous sidecar bytes.
    fn ensure_commit_metadata_capacity(&self, track: &Track) -> Result<(), Error> {
        let committed = Committed {
            recording_id: self.intent.manifest.recording_id.clone(),
            track: track.clone(),
        };
        if serde_json::to_vec(&committed).map_err(json_error)?.len() as u64 > MAX_RECORD_BYTES {
            return Err(Error(
                "recording auxiliary metadata exceeds commit record bound".into(),
            ));
        }
        let mut manifest = self.intent.manifest.clone();
        manifest.tracks = self.commits.values().cloned().collect();
        manifest.tracks.push(track.clone());
        manifest.tracks.sort_by_key(|candidate| candidate.id);
        manifest.validate(self.limits)?;
        if serde_json::to_vec(&manifest).map_err(json_error)?.len() as u64
            > self.limits.max_manifest_bytes
        {
            return Err(Error(
                "recording auxiliary metadata exceeds manifest bound".into(),
            ));
        }
        Ok(())
    }

    fn publish_commit(&mut self, track: Track) -> Result<(), Error> {
        let committed = Committed {
            recording_id: self.intent.manifest.recording_id.clone(),
            track: track.clone(),
        };
        self.publish_record(
            &format!("committed-{}.json", track.id),
            &serde_json::to_vec(&committed).map_err(json_error)?,
        )?;
        self.commits.insert(track.id, track);
        Ok(())
    }

    /// Verify an unused continuation writer before reactivation. A synthesized
    /// incomplete snapshot is not proof of a durable import commit: require the
    /// exact import start/commit population before checking payload bytes.
    pub(crate) fn imported_only_snapshot(&self) -> Result<Manifest, Error> {
        self.check_root()?;
        self.ensure_recording()?;
        let origins = self
            .intent
            .manifest
            .provenance
            .as_ref()
            .ok_or_else(|| Error("recording has no frozen import plan".into()))?;
        if self.starts.len() != origins.imports.len() || self.commits.len() != origins.imports.len()
        {
            return Err(Error(
                "unused recording contains missing imports or locally started tracks".into(),
            ));
        }
        for imported in &origins.imports {
            let started = self
                .starts
                .get(&imported.slot)
                .ok_or_else(|| Error("unused recording lost an import start".into()))?;
            let track = self
                .commits
                .get(&imported.slot)
                .ok_or_else(|| Error("unused recording lost a durable import commit".into()))?;
            if started.import_source != Some(imported.source) {
                return Err(Error(
                    "unused recording contains a local producer intent".into(),
                ));
            }
            origins.require_track(track, imported.source)?;
        }
        // No orphan payload/temp/association for a pending local producer may
        // hide outside the reconstructed start map and then fail after payment.
        let mut names = std::collections::BTreeSet::from([std::ffi::OsString::from("intent.json")]);
        for imported in &origins.imports {
            for name in [
                format!("started-{}.json", imported.slot),
                format!("committed-{}.json", imported.slot),
                format!("track-{}.jsonl", imported.slot),
            ] {
                names.insert(name.into());
            }
        }
        for entry in fs::read_dir(&self.root).map_err(io_error)? {
            if !names.remove(&entry.map_err(io_error)?.file_name()) {
                return Err(Error(
                    "unused recording contains orphan or foreign staging files".into(),
                ));
            }
        }
        if !names.is_empty() {
            return Err(Error("unused recording lost a required import file".into()));
        }
        self.snapshot()
    }

    /// Reconstruct metadata in stable slot order. Never-started slots are absent;
    /// started but uncommitted slots are incomplete with unknown outcomes.
    pub fn snapshot(&self) -> Result<Manifest, Error> {
        self.check_root()?;
        self.verify_auxiliary_staging_names()?;
        if let Some(manifest) = &self.finalization {
            self.verify_finalization(manifest)?;
            return Ok(manifest.clone());
        }
        let declared = self
            .commits
            .values()
            .try_fold(0u64, |total, track| {
                track
                    .artifacts
                    .iter()
                    .try_fold(total.checked_add(track.bytes)?, |total, artifact| {
                        total.checked_add(artifact.bytes)
                    })
            })
            .ok_or_else(|| Error("recording committed length overflow".into()))?;
        if declared > self.limits.max_total_bytes {
            return Err(Error(
                "recording committed payloads exceed aggregate byte limit".into(),
            ));
        }
        // Inspect all lengths before hashing any payload. Actual reads below
        // also consume a remaining-byte budget, covering files that grow after
        // this preflight without allowing max_tracks * max_track_bytes reads.
        let mut total = 0u64;
        for started in self.starts.values() {
            let path = self.payload_path(started.id);
            let length = if exists(&path)? {
                open_regular(&path, self.limits.max_track_bytes)?
                    .metadata()
                    .map_err(io_error)?
                    .len()
            } else if self.commits.contains_key(&started.id) {
                return Err(Error("committed recording payload is missing".into()));
            } else {
                0
            };
            total = total
                .checked_add(length)
                .filter(|total| *total <= self.limits.max_total_bytes)
                .ok_or_else(|| Error("recording payloads exceed aggregate byte limit".into()))?;
            if let Some(track) = self.commits.get(&started.id) {
                if let Some(native) = &track.native {
                    let path = self.native_path(track.id);
                    let length = open_regular(&path, self.limits.max_track_bytes)?
                        .metadata()
                        .map_err(io_error)?
                        .len();
                    total = total
                        .checked_add(length)
                        .filter(|total| *total <= self.limits.max_total_bytes)
                        .ok_or_else(|| {
                            Error("recording payloads exceed aggregate byte limit".into())
                        })?;
                    if length != native.bytes {
                        return Err(Error(
                            "committed native sidecar is missing or changed".into(),
                        ));
                    }
                }
                for artifact in &track.artifacts {
                    let path = self.artifact_path(track.id, &artifact.kind);
                    let length = open_regular(&path, self.limits.max_track_bytes)?
                        .metadata()
                        .map_err(io_error)?
                        .len();
                    total = total
                        .checked_add(length)
                        .filter(|total| *total <= self.limits.max_total_bytes)
                        .ok_or_else(|| {
                            Error("recording payloads exceed aggregate byte limit".into())
                        })?;
                }
            }
        }
        let mut remaining = self.limits.max_total_bytes;
        let mut manifest = self.intent.manifest.clone();
        for started in self.starts.values() {
            let track = if let Some(track) = self.commits.get(&started.id) {
                let actual = hash_file(&self.payload_path(track.id), track.bytes.min(remaining))?;
                if actual != (track.bytes, track.sha256.clone()) {
                    return Err(Error(format!(
                        "recording track {} payload differs from its commit",
                        track.id
                    )));
                }
                let mut artifact_remaining = remaining
                    .checked_sub(track.bytes)
                    .ok_or_else(|| Error("recording aggregate byte limit exceeded".into()))?;
                if let Some(native) = &track.native {
                    if hash_file(
                        &self.native_path(track.id),
                        native.bytes.min(artifact_remaining),
                    )? != (native.bytes, native.sha256.clone())
                    {
                        return Err(Error(format!(
                            "recording track {} native sidecar differs from its commit",
                            track.id
                        )));
                    }
                    artifact_remaining = artifact_remaining
                        .checked_sub(native.bytes)
                        .ok_or_else(|| Error("recording aggregate byte limit exceeded".into()))?;
                }
                for artifact in &track.artifacts {
                    if hash_file(
                        &self.artifact_path(track.id, &artifact.kind),
                        artifact.bytes.min(artifact_remaining),
                    )? != (artifact.bytes, artifact.sha256.clone())
                    {
                        return Err(Error(format!(
                            "recording track {} auxiliary artifact {} differs from its commit",
                            track.id, artifact.kind
                        )));
                    }
                    artifact_remaining = artifact_remaining
                        .checked_sub(artifact.bytes)
                        .ok_or_else(|| Error("recording aggregate byte limit exceeded".into()))?;
                }
                track.clone()
            } else {
                let (bytes, sha256) = if exists(&self.payload_path(started.id))? {
                    hash_file(
                        &self.payload_path(started.id),
                        self.limits.max_track_bytes.min(remaining),
                    )?
                } else {
                    (0, digest(b""))
                };
                Track {
                    recorder: None,
                    id: started.id,
                    batch: started.batch,
                    replica: started.replica,
                    complete: false,
                    process_success: None,
                    workload_success: None,
                    bytes,
                    sha256,
                    statistics: Statistics::default(),
                    native: None,
                    artifacts: Vec::new(),
                }
            };
            let payload_bytes = track
                .artifacts
                .iter()
                .try_fold(track.bytes, |total, artifact| {
                    total.checked_add(artifact.bytes)
                })
                .ok_or_else(|| Error("recording aggregate byte limit exceeded".into()))?;
            let payload_bytes = if let Some(native) = &track.native {
                payload_bytes
                    .checked_add(native.bytes)
                    .ok_or_else(|| Error("recording aggregate byte limit exceeded".into()))?
            } else {
                payload_bytes
            };
            remaining = remaining
                .checked_sub(payload_bytes)
                .ok_or_else(|| Error("recording aggregate byte limit exceeded".into()))?;
            manifest.tracks.push(track);
        }
        manifest.validate(self.limits)?;
        Ok(manifest)
    }

    /// Retained diagnostic directory. No cleanup is performed by dropping it.
    pub fn partial_directory(&self) -> &Path {
        &self.root
    }

    /// Canonical intended final path; journal operations never overwrite it.
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    fn payload_path(&self, id: u64) -> PathBuf {
        self.root.join(format!("track-{id}.jsonl"))
    }

    fn artifact_path(&self, id: u64, kind: &str) -> PathBuf {
        self.root.join(format!("artifact-{id}-{kind}.jsonl"))
    }

    fn native_path(&self, id: u64) -> PathBuf {
        self.root.join(format!("native-{id}.jsonl"))
    }

    /// Auxiliary payloads are never diagnostic sidecars: every such filename
    /// must be named by a committed track or finalization refuses publication.
    fn verify_auxiliary_staging_names(&self) -> Result<(), Error> {
        let mut expected =
            self.commits
                .values()
                .flat_map(|track| {
                    track.artifacts.iter().map(move |artifact| {
                        format!("artifact-{}-{}.jsonl", track.id, artifact.kind)
                    })
                })
                .collect::<std::collections::BTreeSet<_>>();
        expected.extend(self.commits.values().filter_map(|track| {
            track
                .native
                .as_ref()
                .map(|_| format!("native-{}.jsonl", track.id))
        }));
        for entry in fs::read_dir(&self.root).map_err(io_error)? {
            let name = entry
                .map_err(io_error)?
                .file_name()
                .to_string_lossy()
                .into_owned();
            if (name.starts_with("artifact-") || name.starts_with("native-"))
                && !expected.contains(&name)
            {
                return Err(Error(
                    "recording contains an uncommitted or foreign auxiliary artifact".into(),
                ));
            }
        }
        Ok(())
    }

    fn slot(&self, batch: u64, replica: u64) -> Result<u64, Error> {
        if batch == 0
            || batch > self.intent.repeat
            || !(1..=self.intent.manifest.concurrency).contains(&replica)
        {
            return Err(Error(
                "recording batch/replica is outside planned slots".into(),
            ));
        }
        (batch - 1)
            .checked_mul(self.intent.manifest.concurrency)
            .and_then(|base| base.checked_add(replica))
            .ok_or_else(|| Error("recording slot overflow".into()))
    }

    fn validate_track(&self, track: &Track) -> Result<(), Error> {
        if self.slot(track.batch, track.replica)? != track.id
            || !self.starts.contains_key(&track.id)
        {
            return Err(Error(
                "recording track has no matching durable start".into(),
            ));
        }
        if let Some(imported) = self
            .intent
            .manifest
            .provenance
            .as_ref()
            .and_then(|plan| plan.imported(track.id))
        {
            self.intent
                .manifest
                .provenance
                .as_ref()
                .expect("import plan")
                .require_track(track, imported.source)?;
        }
        let mut manifest = self.intent.manifest.clone();
        manifest.tracks.push(track.clone());
        manifest.validate(self.limits)
    }

    fn check_root(&self) -> Result<(), Error> {
        if self.recovery_required {
            return Err(Error("recording publication may have succeeded; drop this writer and explicitly recover before continuing".into()));
        }
        self.check_root_identity()
    }

    fn check_root_identity(&self) -> Result<(), Error> {
        check_root_identity(&self.root, &self.root_identity)
    }

    fn ensure_recording(&self) -> Result<(), Error> {
        if self.finalization.is_some() {
            return Err(Error(
                "recording is frozen for final publication; no new starts or commits are allowed"
                    .into(),
            ));
        }
        Ok(())
    }

    fn publish_record(&mut self, name: &str, bytes: &[u8]) -> Result<(), Error> {
        let total = self
            .metadata_bytes
            .checked_add(bytes.len() as u64)
            .filter(|total| *total <= self.limits.max_manifest_bytes)
            .ok_or_else(|| {
                Error("recording metadata exceeds aggregate manifest byte limit".into())
            })?;
        self.recovery_required = true;
        publish_new(&self.root, name, bytes, &self.publication_fault).map_err(|error| {
            Error(format!(
                "{error}; publication state requires explicit recovery"
            ))
        })?;
        self.metadata_bytes = total;
        self.recovery_required = false;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn inject_publication_failure(&mut self, point: &'static str) {
        self.publication_fault.point = Some(point);
    }

    #[cfg(test)]
    pub(super) fn staged_bytes(&self) -> u64 {
        self.staged_bytes
    }

    #[cfg(test)]
    pub(crate) fn observe_publication(&mut self, observer: fn(&str)) {
        self.publication_fault.observer = Some(observer);
    }

    #[cfg(test)]
    pub(super) fn inject_publication_failure_after(&mut self, point: &'static str, skip: usize) {
        self.publication_fault.point = Some(point);
        self.publication_fault.skip.set(skip);
    }
}

fn check_root_identity(root: &Path, original: &fs::Metadata) -> Result<(), Error> {
    let current = fs::symlink_metadata(root).map_err(io_error)?;
    if !current.is_dir() {
        return Err(Error("recording partial directory was replaced".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if current.dev() != original.dev() || current.ino() != original.ino() {
            return Err(Error("recording partial directory identity changed".into()));
        }
    }
    Ok(())
}

fn validate_count(repeat: u64, concurrency: u64, limits: Limits) -> Result<u64, Error> {
    repeat
        .checked_mul(concurrency)
        .filter(|count| {
            repeat > 0 && (1..=64).contains(&concurrency) && *count <= limits.max_tracks as u64
        })
        .ok_or_else(|| {
            Error("recording repeat/concurrency product is invalid or exceeds track limit".into())
        })
}

/// Reserve the complete planned population before creating any output or
/// admitting clients. Numeric observations may grow to u64::MAX and nullable
/// outcomes may become false; reserve their longest valid JSON representation.
/// One maximum-width slot bounds every slot without allocating the population.
fn reserve_metadata(intent: &Intent, count: u64, limits: Limits) -> Result<(), Error> {
    reservation::metadata_budget(
        intent,
        &intent.manifest.recording_id,
        intent.manifest.concurrency,
        count,
        limits,
        intent
            .manifest
            .provenance
            .as_ref()
            .and_then(|plan| plan.sources.len().checked_sub(1)),
    )
    .map(|_| ())
}

fn paths_and_lock(destination: &Path) -> Result<(PathBuf, PathBuf, String, OutputLock), Error> {
    let (destination, root, name) = recording_paths(destination)?;
    let owner = OutputLock::acquire(
        &destination
            .parent()
            .expect("canonical parent")
            .join(format!(".{name}.recording.lock")),
        "another writer owns this cassette recording",
    )
    .map_err(Error)?;
    Ok((destination, root, name, owner))
}

fn recording_paths(destination: &Path) -> Result<(PathBuf, PathBuf, String), Error> {
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| Error("cassette destination needs a UTF-8 filename".into()))?
        .to_owned();
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()
        .map_err(io_error)?;
    Ok((
        parent.join(&name),
        parent.join(format!(".{name}.partial")),
        name,
    ))
}

fn exists(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error(error)),
    }
}

fn create_private(path: &Path) -> Result<File, Error> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(io_error)
}

fn open_regular(path: &Path, limit: u64) -> Result<File, Error> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(Error(
            "recording evidence is not a bounded regular file".into(),
        ));
    }
    Ok(file)
}

fn read_json<T: DeserializeOwned>(
    path: &Path,
    limit: u64,
    remaining: &mut u64,
) -> Result<T, Error> {
    let mut bytes = Vec::new();
    let limit = limit.min(*remaining);
    open_regular(path, limit)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > limit {
        return Err(Error("recording metadata exceeds limit".into()));
    }
    *remaining -= bytes.len() as u64;
    serde_json::from_slice(&bytes).map_err(json_error)
}

fn hash_file(path: &Path, limit: u64) -> Result<(u64, String), Error> {
    hash_open_file(&mut open_regular(path, limit)?, limit)
}

fn hash_open_file(file: &mut File, limit: u64) -> Result<(u64, String), Error> {
    let mut source = file.take(limit.saturating_add(1));
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0; 16 * 1024];
    loop {
        let read = source.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        bytes += read as u64;
        if bytes > limit {
            return Err(Error("recording payload exceeds limit".into()));
        }
        hash.update(&buffer[..read]);
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}

#[derive(Default)]
struct PublicationFault {
    #[cfg(test)]
    observer: Option<fn(&str)>,
    #[cfg(test)]
    point: Option<&'static str>,
    #[cfg(test)]
    skip: std::cell::Cell<usize>,
}

impl PublicationFault {
    fn check(&self, _point: &str) -> Result<(), Error> {
        #[cfg(test)]
        if let Some(observer) = self.observer {
            observer(_point);
        }
        #[cfg(test)]
        if self.point == Some(_point) {
            if self.skip.get() > 0 {
                self.skip.set(self.skip.get() - 1);
                return Ok(());
            }
            return Err(Error(format!("injected publication failure at {_point}")));
        }
        Ok(())
    }
}

fn publish_new(
    root: &Path,
    name: &str,
    bytes: &[u8],
    fault: &PublicationFault,
) -> Result<(), Error> {
    let check = |point: &str| -> Result<(), Error> {
        fault.check(point)?;
        fault.check(&format!("{name}:{point}"))
    };
    let mut temporary = None;
    for _ in 0..256 {
        let path = root.join(format!(
            ".pending-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        match create_private(&path) {
            Ok(mut file) => {
                check("before-write")?;
                file.write_all(bytes).map_err(io_error)?;
                check("before-file-sync")?;
                file.sync_all().map_err(io_error)?;
                check("after-file-sync")?;
                temporary = Some(path);
                break;
            }
            Err(error) if exists(&path)? => {
                let _ = error;
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    let temporary = temporary
        .ok_or_else(|| Error("too many private recording publication collisions".into()))?;
    check("before-link")?;
    fs::hard_link(&temporary, root.join(name)).map_err(io_error)?;
    check("after-link")?;
    check("before-directory-sync")?;
    sync_directory(root)?;
    check("after-directory-sync")?;
    check("before-temporary-removal")?;
    fs::remove_file(&temporary).map_err(io_error)?;
    check("after-temporary-removal")?;
    sync_directory(root)
}

fn sync_directory(path: &Path) -> Result<(), Error> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(io_error)
}
fn io_error(error: std::io::Error) -> Error {
    Error(format!(
        "recording I/O error; partial evidence retained: {error}"
    ))
}
fn json_error(error: serde_json::Error) -> Error {
    Error(format!("invalid recording metadata: {error}"))
}
