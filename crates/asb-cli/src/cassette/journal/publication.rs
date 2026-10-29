//! Freeze, verify, publish without replacement, and conservatively clean staging.
use super::*;
use crate::cassette::Archive;
use std::io::{Seek, SeekFrom};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

/// Confirmed usable archive, with separate nonfatal staging-cleanup diagnostics.
#[derive(Clone, Debug)]
pub struct Publication {
    /// Canonical final archive path.
    pub destination: PathBuf,
    /// Frozen manifest: only actually started slots, including incomplete ones.
    pub manifest: Manifest,
    /// Whether known staging payloads and the prepared duplicate were removed.
    pub cleanup_complete: bool,
    /// Cleanup warnings; publication itself is already confirmed and usable.
    pub warnings: Vec<String>,
    /// Exact confirmed compressed archive length from the prepared receipt.
    pub archive_bytes: u64,
    /// Exact confirmed compressed archive digest from the prepared receipt.
    pub archive_sha256: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Prepared {
    recording_id: String,
    manifest_sha256: String,
    candidate: String,
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Published {
    recording_id: String,
    manifest_sha256: String,
    archive_sha256: String,
    pub(super) archive_bytes: u64,
}

pub(super) fn publication_capacity(recording_id: &str, limits: Limits) -> Result<u64, Error> {
    let marker = Published {
        recording_id: recording_id.into(),
        manifest_sha256: "f".repeat(64),
        archive_sha256: "f".repeat(64),
        archive_bytes: limits.max_archive_bytes,
    };
    crate::cassette::serialization::encoded_size(&marker, MAX_RECORD_BYTES)
}

impl RecordingJournal {
    /// Whether recovery verified an existing immutable finalization manifest.
    /// This is publication state, not agent cleanup or retry permission.
    pub fn publication_is_frozen(&self) -> bool {
        self.finalization.is_some()
    }

    /// Freeze and publish a verified ZIP, without replacing any destination.
    ///
    /// The caller must first stop/join recording producers. Freezing prevents
    /// journal mutation but does not itself drain a provider or proxy. Ordinary
    /// failed/incomplete attempts remain in the archive. Errors retain staging
    /// and require explicit recovery; after confirmed publication, cleanup
    /// failures instead return a usable destination with warnings.
    pub fn finalize(&mut self) -> Result<Publication, Error> {
        self.check_root()?;
        if self.finalization.is_none() {
            if exists(&self.destination)? {
                return Err(Error(
                    "recording destination already exists; refusing to replace it".into(),
                ));
            }
            let manifest = self.snapshot()?;
            manifest.validate_imports()?;
            self.recovery_required = true;
            self.publication_fault.check("before-freeze")?;
            self.publish_phase("finalizing.json", &manifest, self.limits.max_manifest_bytes)?;
            self.publication_fault.check("after-freeze")?;
            self.finalization = Some(manifest);
        }
        self.recovery_required = true;
        let result = self.finalize_frozen();
        if result.is_ok() {
            self.recovery_required = false;
        }
        result
    }

    /// Verify one immutable track from staged bytes or its confirmed archive.
    /// Evidence consumers bind the retained commit/manifest identity and digest,
    /// never the temporary payload pathname which publication may remove.
    pub fn verify_track_evidence(&self, expected: &Track) -> Result<(), Error> {
        self.verify_track_evidence_batch(std::slice::from_ref(expected))
    }

    /// Verify multiple immutable tracks with one global metadata validation and
    /// one archive index. Duplicate IDs are decoded once but every supplied
    /// metadata record must match. Sweep receipts should use this batch form to
    /// avoid repeatedly parsing the whole catalogue for each execution.
    ///
    /// This verifies selected entries, not the whole compressed archive digest;
    /// final group evidence must independently bind the published archive.
    pub fn verify_track_evidence_batch(&self, expected: &[Track]) -> Result<(), Error> {
        self.check_root()?;
        self.require_published_destination()?;
        if expected.len() > self.limits.max_tracks {
            return Err(Error("track evidence batch exceeds catalogue limit".into()));
        }
        if let Some(manifest) = &self.finalization {
            self.validate_frozen(manifest)?;
        }
        let mut selected = std::collections::BTreeMap::new();
        for track in expected {
            let actual = if let Some(manifest) = &self.finalization {
                manifest.track(track.id)?
            } else {
                self.commits
                    .get(&track.id)
                    .ok_or_else(|| Error("track has no immutable committed evidence".into()))?
            };
            if actual != track {
                return Err(Error(
                    "track evidence differs from its immutable metadata".into(),
                ));
            }
            selected.insert(track.id, track);
        }
        if exists(&self.destination)? {
            let manifest = self
                .finalization
                .as_ref()
                .ok_or_else(|| Error("archive appeared before recording freeze".into()))?;
            let prepared = self.read_prepared(manifest)?;
            if exists(&self.root.join("published.json"))? {
                self.validate_published(manifest, &prepared)?;
            }
            drop(open_regular(
                &self.destination,
                self.limits.max_archive_bytes,
            )?);
            let mut archive = Archive::open(&self.destination, self.limits)?;
            if archive.manifest() != manifest {
                return Err(Error(
                    "archive manifest differs from frozen track evidence".into(),
                ));
            }
            for id in selected.keys() {
                archive.read_track(*id)?;
            }
        } else {
            for track in selected.values() {
                self.verify_staged_track(track)?;
            }
        }
        Ok(())
    }
    pub(super) fn recover_finalization(&mut self) -> Result<(), Error> {
        if !exists(&self.root.join("finalizing.json"))? {
            if exists(&self.root.join("prepared.json"))?
                || exists(&self.root.join("published.json"))?
            {
                return Err(Error(
                    "publication receipt has no frozen recording manifest".into(),
                ));
            }
            if exists(&self.destination)? {
                return Err(Error("destination exists without a prepared recording publication; refusing unrelated data".into()));
            }
            return Ok(());
        }
        let manifest: Manifest =
            self.read_phase("finalizing.json", self.limits.max_manifest_bytes)?;
        self.validate_frozen(&manifest)?;
        self.finalization = Some(manifest);
        let manifest = self.finalization.as_ref().expect("loaded frozen manifest");
        if exists(&self.destination)? {
            self.read_prepared(manifest)?;
            self.allow_existing_destination = true;
        } else if exists(&self.root.join("published.json"))? {
            return Err(Error("published recording archive is missing; retained payloads are not permission to replace it".into()));
        }
        // recover() performs one full snapshot verification before returning
        // the guard; no payload is read before all metadata has been checked.
        Ok(())
    }

    pub(super) fn verify_finalization(&self, manifest: &Manifest) -> Result<(), Error> {
        self.require_published_destination()?;
        self.validate_frozen(manifest)?;
        if exists(&self.destination)? {
            let prepared = self.read_prepared(manifest)?;
            self.verify_archive(&self.destination, manifest, Some(&prepared))?;
            if exists(&self.root.join("published.json"))? {
                self.validate_published(manifest, &prepared)?;
            }
        } else {
            if exists(&self.root.join("prepared.json"))? {
                let prepared = self.read_prepared(manifest)?;
                self.verify_archive(
                    &self.root.join(&prepared.candidate),
                    manifest,
                    Some(&prepared),
                )?;
                return Ok(());
            }
            for track in &manifest.tracks {
                self.verify_staged_track(track)?;
            }
        }
        Ok(())
    }

    fn finalize_frozen(&mut self) -> Result<Publication, Error> {
        self.require_published_destination()?;
        let manifest = self
            .finalization
            .clone()
            .expect("freeze precedes publication");
        self.validate_frozen(&manifest)?;
        let prepared = if exists(&self.root.join("prepared.json"))? {
            self.read_prepared(&manifest)?
        } else {
            let prepared = self.build_candidate(&manifest)?;
            self.publication_fault.check("before-prepared")?;
            self.publish_phase("prepared.json", &prepared, MAX_RECORD_BYTES)?;
            self.publication_fault.check("after-prepared")?;
            prepared
        };
        if exists(&self.destination)? {
            if !self.allow_existing_destination {
                return Err(Error("recording destination appeared during publication; refusing to replace or adopt it".into()));
            }
            self.verify_archive(&self.destination, &manifest, Some(&prepared))?;
            // A prior owner may have died after link but before syncing this
            // directory. Verification is not durability confirmation.
            self.confirm_destination_directory()?;
        } else {
            let candidate = self.root.join(&prepared.candidate);
            self.verify_archive(&candidate, &manifest, Some(&prepared))?;
            self.publication_fault.check("before-final-link")?;
            fs::hard_link(&candidate, &self.destination).map_err(io_error)?;
            let confirm = self
                .publication_fault
                .check("after-final-link")
                .and_then(|()| self.confirm_destination_directory());
            confirm.map_err(|error| Error(format!("destination was linked but publication durability is unconfirmed: {error}; explicitly recover, never overwrite")))?;
        }
        if exists(&self.root.join("published.json"))? {
            self.validate_published(&manifest, &prepared)?;
        } else {
            self.publication_fault.check("before-published")?;
            let published = Published {
                recording_id: manifest.recording_id.clone(),
                manifest_sha256: manifest_digest(&manifest)?,
                archive_sha256: prepared.sha256.clone(),
                archive_bytes: prepared.bytes,
            };
            self.publish_phase("published.json", &published, MAX_RECORD_BYTES)
                .map_err(|error| Error(format!("destination exists but publication confirmation is incomplete: {error}; explicitly recover")))?;
        }
        self.allow_existing_destination = true;
        if let Err(error) = self.publication_fault.check("after-published") {
            return Ok(Publication {
                archive_bytes: prepared.bytes,
                archive_sha256: prepared.sha256.clone(),
                destination: self.destination.clone(),
                manifest,
                cleanup_complete: false,
                warnings: vec![format!(
                    "archive publication confirmed; staging cleanup not completed: {error}"
                )],
            });
        }
        let warnings = self.cleanup_payloads(&manifest, &prepared);
        Ok(Publication {
            archive_bytes: prepared.bytes,
            archive_sha256: prepared.sha256.clone(),
            destination: self.destination.clone(),
            manifest,
            cleanup_complete: warnings.is_empty(),
            warnings,
        })
    }

    fn confirm_destination_directory(&self) -> Result<(), Error> {
        self.publication_fault.check("before-parent-sync")?;
        sync_directory(self.destination.parent().expect("canonical parent"))?;
        self.publication_fault.check("after-parent-sync")
    }

    fn require_published_destination(&self) -> Result<(), Error> {
        if exists(&self.root.join("published.json"))? && !exists(&self.destination)? {
            return Err(Error("published recording archive is missing; retained payloads are not permission to replace it".into()));
        }
        Ok(())
    }

    fn build_candidate(&self, manifest: &Manifest) -> Result<Prepared, Error> {
        for track in &manifest.tracks {
            self.verify_staged_track(track)?;
        }
        self.publication_fault.check("before-zip-create")?;
        let (name, file) = self.new_candidate()?;
        let mut writer = ZipWriter::new(BoundedFile {
            file,
            limit: self.limits.max_archive_bytes,
        });
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o600);
        writer
            .start_file("manifest.json", options)
            .map_err(zip_error)?;
        let bytes = serde_json::to_vec(manifest).map_err(json_error)?;
        if bytes.len() as u64 > self.limits.max_manifest_bytes {
            return Err(Error("frozen manifest exceeds byte limit".into()));
        }
        writer.write_all(&bytes).map_err(io_error)?;
        for track in &manifest.tracks {
            self.publication_fault.check("before-zip-track")?;
            writer
                .start_file(track.entry_name(), options)
                .map_err(zip_error)?;
            let path = self.payload_path(track.id);
            if exists(&path)? {
                let copied = std::io::copy(
                    &mut open_regular(&path, track.bytes)?.take(track.bytes.saturating_add(1)),
                    &mut writer,
                )
                .map_err(io_error)?;
                if copied != track.bytes {
                    return Err(Error("staged payload changed while building ZIP".into()));
                }
            } else if track.bytes != 0 {
                return Err(Error(
                    "staged payload is missing during ZIP publication".into(),
                ));
            }
            if let Some(native) = &track.native {
                self.publication_fault.check("before-zip-native")?;
                writer
                    .start_file(track.native_entry_name(), options)
                    .map_err(zip_error)?;
                let copied = std::io::copy(
                    &mut open_regular(&self.native_path(track.id), native.bytes)?
                        .take(native.bytes.saturating_add(1)),
                    &mut writer,
                )
                .map_err(io_error)?;
                if copied != native.bytes {
                    return Err(Error(
                        "staged native sidecar changed while building ZIP".into(),
                    ));
                }
            }
            for artifact in &track.artifacts {
                self.publication_fault.check("before-zip-artifact")?;
                writer
                    .start_file(track.artifact_entry_name(&artifact.kind), options)
                    .map_err(zip_error)?;
                let path = self.artifact_path(track.id, &artifact.kind);
                let copied = std::io::copy(
                    &mut open_regular(&path, artifact.bytes)?
                        .take(artifact.bytes.saturating_add(1)),
                    &mut writer,
                )
                .map_err(io_error)?;
                if copied != artifact.bytes {
                    return Err(Error(
                        "staged auxiliary artifact changed while building ZIP".into(),
                    ));
                }
            }
        }
        self.publication_fault.check("before-zip-finish")?;
        let output = writer.finish().map_err(zip_error)?;
        self.publication_fault.check("before-zip-sync")?;
        output.file.sync_all().map_err(io_error)?;
        self.publication_fault.check("after-zip-sync")?;
        sync_directory(&self.root)?;
        self.publication_fault
            .check("after-candidate-directory-sync")?;
        let path = self.root.join(&name);
        self.verify_archive(&path, manifest, None)?;
        let (bytes, sha256) = hash_file(&path, self.limits.max_archive_bytes)?;
        Ok(Prepared {
            recording_id: manifest.recording_id.clone(),
            manifest_sha256: manifest_digest(manifest)?,
            candidate: name,
            bytes,
            sha256,
        })
    }

    fn new_candidate(&self) -> Result<(String, File), Error> {
        for _ in 0..256 {
            let name = format!(
                ".archive-{}-{}.zip",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            );
            let path = self.root.join(&name);
            match create_private(&path) {
                Ok(file) => return Ok((name, file)),
                Err(_) if exists(&path)? => continue,
                Err(error) => return Err(error),
            }
        }
        Err(Error("too many private archive filename collisions".into()))
    }

    fn validate_frozen(&self, manifest: &Manifest) -> Result<(), Error> {
        manifest.validate(self.limits)?;
        manifest.validate_imports()?;
        let intent = &self.intent.manifest;
        if manifest.recording_id != intent.recording_id
            || manifest.compatibility != intent.compatibility
            || manifest.concurrency != intent.concurrency
            || manifest.provenance != intent.provenance
            || manifest.tracks.len() != self.starts.len()
        {
            return Err(Error(
                "frozen manifest differs from recording intent/started slots".into(),
            ));
        }
        for track in &manifest.tracks {
            let start = self
                .starts
                .get(&track.id)
                .ok_or_else(|| Error("frozen track has no matching start".into()))?;
            if track.batch != start.batch || track.replica != start.replica {
                return Err(Error("frozen track changed its execution slot".into()));
            }
            if let Some(committed) = self.commits.get(&track.id) {
                if committed != track {
                    return Err(Error(
                        "frozen track differs from immutable committed metadata".into(),
                    ));
                }
            } else if track.complete
                || track.process_success.is_some()
                || track.workload_success.is_some()
                || track.statistics != Statistics::default()
            {
                return Err(Error(
                    "uncommitted frozen track cannot claim completeness, outcomes or statistics"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    fn verify_staged_track(&self, track: &Track) -> Result<(), Error> {
        if !exists(&self.payload_path(track.id))? && track.bytes == 0 && track.sha256 == digest(b"")
        {
            return Ok(());
        }
        if hash_file(&self.payload_path(track.id), track.bytes)?
            != (track.bytes, track.sha256.clone())
        {
            return Err(Error(
                "staged recording payload changed after freeze".into(),
            ));
        }
        for artifact in &track.artifacts {
            if hash_file(
                &self.artifact_path(track.id, &artifact.kind),
                artifact.bytes,
            )? != (artifact.bytes, artifact.sha256.clone())
            {
                return Err(Error(
                    "staged recording auxiliary artifact changed after freeze".into(),
                ));
            }
        }
        if let Some(native) = &track.native
            && hash_file(&self.native_path(track.id), native.bytes)?
                != (native.bytes, native.sha256.clone())
        {
            return Err(Error("staged native sidecar changed after freeze".into()));
        }
        Ok(())
    }

    fn verify_archive(
        &self,
        path: &Path,
        manifest: &Manifest,
        prepared: Option<&Prepared>,
    ) -> Result<(), Error> {
        // No-follow checks precede the reusable reader. Hash exact compressed
        // bytes when a prepared receipt exists; all entry digests are checked too.
        let file = open_regular(path, self.limits.max_archive_bytes)?;
        drop(file);
        if let Some(prepared) = prepared
            && hash_file(path, self.limits.max_archive_bytes)?
                != (prepared.bytes, prepared.sha256.clone())
        {
            return Err(Error(
                "archive differs from prepared publication bytes".into(),
            ));
        }
        let mut archive = Archive::open(path, self.limits)?;
        if archive.manifest() != manifest {
            return Err(Error(
                "archive manifest differs from frozen recording".into(),
            ));
        }
        for track in &manifest.tracks {
            archive.read_track(track.id)?;
            if track.native.is_some() {
                archive.read_native(track.id)?;
            }
            for artifact in &track.artifacts {
                archive.read_artifact(track.id, &artifact.kind)?;
            }
        }
        Ok(())
    }

    fn read_prepared(&self, manifest: &Manifest) -> Result<Prepared, Error> {
        let prepared: Prepared = self.read_phase("prepared.json", MAX_RECORD_BYTES)?;
        let name = prepared
            .candidate
            .strip_prefix(".archive-")
            .and_then(|name| name.strip_suffix(".zip"));
        if prepared.recording_id != manifest.recording_id
            || prepared.manifest_sha256 != manifest_digest(manifest)?
            || !super::super::valid_digest(&prepared.sha256)
            || prepared.bytes > self.limits.max_archive_bytes
            || prepared.bytes == 0
            || name.is_none_or(|name| {
                name.is_empty()
                    || !name
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte == b'-')
            })
        {
            return Err(Error(
                "invalid prepared recording publication receipt".into(),
            ));
        }
        Ok(prepared)
    }

    fn validate_published(&self, manifest: &Manifest, prepared: &Prepared) -> Result<(), Error> {
        let published: Published = self.read_phase("published.json", MAX_RECORD_BYTES)?;
        if published.recording_id != manifest.recording_id
            || published.manifest_sha256 != manifest_digest(manifest)?
            || published.archive_sha256 != prepared.sha256
            || published.archive_bytes != prepared.bytes
        {
            return Err(Error(
                "published receipt differs from prepared archive identity".into(),
            ));
        }
        Ok(())
    }

    fn cleanup_payloads(&self, manifest: &Manifest, prepared: &Prepared) -> Vec<String> {
        let mut warnings = Vec::new();
        for track in &manifest.tracks {
            let path = self.payload_path(track.id);
            let result = (|| {
                self.check_root_identity()?;
                self.publication_fault.check("before-payload-cleanup")?;
                if exists(&path)? {
                    self.verify_staged_track(track)?;
                    fs::remove_file(&path).map_err(io_error)?;
                    sync_directory(&self.root)?;
                }
                self.publication_fault.check("after-payload-cleanup")
            })();
            if let Err(error) = result {
                warnings.push(format!(
                    "published archive is usable; track {} staging cleanup incomplete: {error}",
                    track.id
                ));
                return warnings;
            }
            for artifact in &track.artifacts {
                let path = self.artifact_path(track.id, &artifact.kind);
                let result = (|| {
                    self.check_root_identity()?;
                    self.publication_fault.check("before-payload-cleanup")?;
                    if exists(&path)? {
                        if hash_file(&path, artifact.bytes)?
                            != (artifact.bytes, artifact.sha256.clone())
                        {
                            return Err(Error(
                                "staged recording auxiliary artifact changed after freeze".into(),
                            ));
                        }
                        fs::remove_file(&path).map_err(io_error)?;
                        sync_directory(&self.root)?;
                    }
                    self.publication_fault.check("after-payload-cleanup")
                })();
                if let Err(error) = result {
                    warnings.push(format!(
                        "could not remove sealed auxiliary recording payload {}: {error}",
                        path.display()
                    ));
                }
            }
            if let Some(native) = &track.native {
                let path = self.native_path(track.id);
                let result = (|| {
                    self.check_root_identity()?;
                    self.publication_fault.check("before-payload-cleanup")?;
                    if exists(&path)? {
                        if hash_file(&path, native.bytes)? != (native.bytes, native.sha256.clone())
                        {
                            return Err(Error("staged native sidecar changed after freeze".into()));
                        }
                        fs::remove_file(&path).map_err(io_error)?;
                        sync_directory(&self.root)?;
                    }
                    self.publication_fault.check("after-payload-cleanup")
                })();
                if let Err(error) = result {
                    warnings.push(format!(
                        "could not remove sealed native recording payload {}: {error}",
                        path.display()
                    ));
                    return warnings;
                }
            }
        }
        let candidate = self.root.join(&prepared.candidate);
        let cleanup = (|| {
            self.check_root_identity()?;
            self.publication_fault.check("before-candidate-cleanup")?;
            if exists(&candidate)? {
                if candidate == self.destination {
                    return Err(Error(
                        "candidate aliases destination pathname; retaining it".into(),
                    ));
                }
                if hash_file(&candidate, self.limits.max_archive_bytes)?
                    != (prepared.bytes, prepared.sha256.clone())
                {
                    return Err(Error(
                        "candidate changed; retaining diagnostic bytes".into(),
                    ));
                }
                fs::remove_file(candidate).map_err(io_error)?;
                sync_directory(&self.root)?;
            }
            self.publication_fault.check("after-candidate-cleanup")
        })();
        if let Err(error) = cleanup {
            warnings.push(format!(
                "published archive is usable; candidate cleanup incomplete: {error}"
            ));
        }
        warnings
    }

    fn publish_phase<T: Serialize>(&self, name: &str, record: &T, limit: u64) -> Result<(), Error> {
        let bytes = serde_json::to_vec(record).map_err(json_error)?;
        if bytes.len() as u64 > limit {
            return Err(Error(
                "publication metadata exceeds bounded record limit".into(),
            ));
        }
        publish_new(&self.root, name, &bytes, &self.publication_fault)
    }

    fn read_phase<T: DeserializeOwned>(&self, name: &str, limit: u64) -> Result<T, Error> {
        let mut remaining = limit;
        read_json(&self.root.join(name), limit, &mut remaining)
    }
}

/// Prepare one read-only selected-entry proof. Validate every selected metadata
/// record before decoding any track; this is not a whole-archive hash assertion.
pub(super) fn open_selected_publication(
    root: &Path,
    expected: &super::TrackEvidence,
    tracks: &BTreeMap<u64, &Track>,
    limits: Limits,
    remaining: &mut u64,
) -> Result<(Archive, Published), Error> {
    let published: Published =
        read_json(&root.join("published.json"), MAX_RECORD_BYTES, remaining)?;
    let file = open_regular(&expected.destination, limits.max_archive_bytes)?;
    super::evidence::observed("archive-opened");
    if published.recording_id != expected.recording_id
        || !super::super::valid_digest(&published.archive_sha256)
        || published.archive_bytes != file.metadata().map_err(io_error)?.len()
    {
        return Err(Error(
            "published recording target differs from receipt".into(),
        ));
    }
    let archive = Archive::from_file(
        file,
        Limits {
            max_manifest_bytes: limits.max_manifest_bytes.min(*remaining),
            ..limits
        },
    )?;
    let manifest = archive.manifest();
    if manifest.recording_id != expected.recording_id
        || manifest.compatibility != expected.compatibility
        || manifest.concurrency != expected.concurrency
        || manifest_digest(manifest)? != published.manifest_sha256
    {
        return Err(Error(
            "published manifest differs from immutable track evidence".into(),
        ));
    }
    for (id, track) in tracks {
        if manifest.track(*id)? != *track {
            return Err(Error(
                "published manifest differs from immutable track evidence".into(),
            ));
        }
    }
    Ok((archive, published))
}

fn manifest_digest(manifest: &Manifest) -> Result<String, Error> {
    Ok(digest(&serde_json::to_vec(manifest).map_err(json_error)?))
}
fn zip_error(error: zip::result::ZipError) -> Error {
    Error(format!(
        "recording ZIP publication failed; partial retained: {error}"
    ))
}

struct BoundedFile {
    file: File,
    limit: u64,
}
impl Write for BoundedFile {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .file
            .stream_position()?
            .checked_add(bytes.len() as u64)
            .is_none_or(|end| end > self.limit)
        {
            return Err(std::io::Error::other(
                "recording ZIP output exceeds archive byte limit",
            ));
        }
        self.file.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
impl Seek for BoundedFile {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let current = self.file.stream_position()?;
        let position = self.file.seek(from)?;
        if position > self.limit {
            self.file.seek(SeekFrom::Start(current))?;
            return Err(std::io::Error::other(
                "recording ZIP seek exceeds archive byte limit",
            ));
        }
        Ok(position)
    }
}
