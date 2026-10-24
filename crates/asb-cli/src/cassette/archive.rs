//! Bounded metadata preflight and selective extraction; no filesystem extraction.
use super::metadata::MetadataReader;
use super::{Error, Manifest, digest};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;
use zip::{
    CompressionMethod, ZipArchive,
    read::{ArchiveOffset, Config},
};

/// Reader resource limits, checked before allocating or decompressing payloads.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum compressed archive file length.
    pub max_archive_bytes: u64,
    /// Maximum central directory plus end-record bytes, checked before allocation.
    pub max_directory_bytes: u64,
    /// Maximum manifest JSON bytes.
    pub max_manifest_bytes: u64,
    /// Maximum tracks, excluding the manifest entry.
    pub max_tracks: usize,
    /// Maximum uncompressed length of one JSONL track.
    pub max_track_bytes: u64,
    /// Maximum aggregate uncompressed track length.
    pub max_total_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_archive_bytes: 2 * 1024 * 1024 * 1024,
            max_directory_bytes: 16 * 1024 * 1024,
            max_manifest_bytes: 16 * 1024 * 1024,
            max_tracks: 65_533,
            max_track_bytes: 256 * 1024 * 1024,
            max_total_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

impl Limits {
    pub(super) fn validate(self) -> Result<(), Error> {
        if self.max_archive_bytes == 0
            || self.max_directory_bytes == 0
            || self.max_manifest_bytes == 0
            || self.max_track_bytes == 0
            || self.max_total_bytes == 0
            || self.max_tracks == 0
            || self.max_tracks > 65_533
        {
            return Err(Error(
                "cassette reader limits must be positive; this ZIP32 schema supports at most 65533 tracks"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// A validated catalogue backed by an open archive, without inflated track data.
pub struct Archive {
    archive: ZipArchive<MetadataReader>,
    manifest: Manifest,
    entries: BTreeMap<u64, usize>,
    artifact_entries: BTreeMap<(u64, String), usize>,
    limits: Limits,
    metadata_bytes: (u64, u64),
}

impl Archive {
    /// Open and validate metadata/manifest only. Payload CRC/digests are checked
    /// when selected. Schema one uses bounded single-disk ZIP32, not ZIP64.
    pub fn open(path: &Path, limits: Limits) -> Result<Self, Error> {
        limits.validate()?;
        Self::from_file(open_file(path)?, limits)
    }

    /// Pin and verify exact compressed publication bytes before inspecting its
    /// manifest. Hashing does not decompress unselected track entries.
    pub fn open_verified(
        path: &Path,
        bytes: u64,
        sha256: &str,
        limits: Limits,
    ) -> Result<Self, Error> {
        use sha2::{Digest, Sha256};
        use std::io::{Seek, SeekFrom};
        limits.validate()?;
        if bytes > limits.max_archive_bytes || !super::valid_digest(sha256) {
            return Err(Error("invalid confirmed archive identity".into()));
        }
        let mut file = open_file(path)?;
        if file.metadata().map_err(io_error)?.len() != bytes {
            return Err(Error("confirmed archive length changed".into()));
        }
        let mut hash = Sha256::new();
        let mut count = 0u64;
        let mut buffer = [0; 64 * 1024];
        loop {
            let read = file.read(&mut buffer).map_err(io_error)?;
            if read == 0 {
                break;
            }
            count = count
                .checked_add(read as u64)
                .filter(|count| *count <= bytes)
                .ok_or_else(|| Error("confirmed archive grew while reading".into()))?;
            hash.update(&buffer[..read]);
        }
        if count != bytes || format!("{:x}", hash.finalize()) != sha256 {
            return Err(Error("confirmed archive checksum changed".into()));
        }
        file.seek(SeekFrom::Start(0)).map_err(io_error)?;
        Self::from_file(file, limits)
    }

    /// Consume the exact descriptor already checked by a recording proof.
    pub(super) fn from_file(file: std::fs::File, limits: Limits) -> Result<Self, Error> {
        limits.validate()?;
        if !file.metadata().map_err(io_error)?.is_file() {
            return Err(Error("cassette archive must be a regular file".into()));
        }
        let (reader, metadata_phase, expected_entries, directory_start) =
            MetadataReader::new(file, limits)?;
        let directory_bytes = reader.metadata_bytes();
        let mut archive = ZipArchive::with_config(
            Config {
                archive_offset: ArchiveOffset::Known(0),
            },
            reader,
        )
        .map_err(zip_error)?;
        if archive.len() != expected_entries
            || archive.central_directory_start() != directory_start
            || archive.offset() != 0
        {
            return Err(Error("cassette ZIP entry count differs from directory; duplicate entries are unsupported".into()));
        }
        metadata_phase.store(false, std::sync::atomic::Ordering::Release);
        let mut names = BTreeMap::new();
        let mut total = 0u64;
        for index in 0..archive.len() {
            let entry = archive.by_index_data(index).map_err(zip_error)?;
            let name = entry.name().map_err(zip_error)?.into_owned();
            let is_manifest = name == "manifest.json";
            let canonical_track = name
                .strip_prefix("tracks/")
                .and_then(|value| value.strip_suffix(".jsonl"))
                .and_then(|value| value.parse::<u64>().ok())
                .is_some_and(|id| id > 0 && name == format!("tracks/{id}.jsonl"));
            let canonical_artifact = name
                .strip_prefix("artifacts/")
                .and_then(|value| value.split_once('/'))
                .and_then(|(id, kind)| {
                    kind.strip_suffix(".jsonl")
                        .filter(|base| !base.is_empty())
                        .and_then(|base| id.parse::<u64>().ok().map(|id| (id, base)))
                })
                .is_some_and(|(id, kind)| id > 0 && name == format!("artifacts/{id}/{kind}.jsonl"));
            let size_limit = if is_manifest {
                limits.max_manifest_bytes
            } else {
                limits.max_track_bytes
            };
            if (!is_manifest && !canonical_track && !canonical_artifact)
                || names.contains_key(&name)
                || entry.encrypted()
                || entry.compression() != CompressionMethod::Deflated
                || entry
                    .unix_mode()
                    .is_some_and(|mode| mode & 0o170000 != 0 && mode & 0o170000 != 0o100000)
                || entry.size() > size_limit
                || entry.compressed_size() > limits.max_archive_bytes
            {
                return Err(Error(
                    "unsafe, unsupported, duplicate or oversized cassette ZIP entry".into(),
                ));
            }
            total = total
                .checked_add(entry.size())
                .ok_or_else(|| Error("cassette entry length overflow".into()))?;
            if total
                > limits
                    .max_total_bytes
                    .saturating_add(limits.max_manifest_bytes)
            {
                return Err(Error(
                    "cassette exceeds aggregate decompression limit".into(),
                ));
            }
            names.insert(name, index);
        }
        let manifest_index = *names
            .get("manifest.json")
            .ok_or_else(|| Error("cassette manifest.json is missing".into()))?;
        let bytes = read_bounded(&mut archive, manifest_index, limits.max_manifest_bytes)?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|error| Error(format!("invalid cassette manifest: {error}")))?;
        manifest.validate(limits)?;
        manifest.validate_imports()?;
        let artifact_count = manifest
            .tracks
            .iter()
            .map(|track| track.artifacts.len())
            .sum::<usize>();
        if names.len() != manifest.tracks.len() + artifact_count + 1 {
            return Err(Error(
                "cassette manifest and ZIP track catalogue differ".into(),
            ));
        }
        let mut entries = BTreeMap::new();
        let mut artifact_entries = BTreeMap::new();
        for track in &manifest.tracks {
            let index = *names
                .get(&track.entry_name())
                .ok_or_else(|| Error(format!("cassette track {} payload is missing", track.id)))?;
            if archive.by_index_data(index).map_err(zip_error)?.size() != track.bytes {
                return Err(Error(format!(
                    "cassette track {} length differs from manifest",
                    track.id
                )));
            }
            entries.insert(track.id, index);
            for artifact in &track.artifacts {
                let name = track.artifact_entry_name(&artifact.kind);
                let index = *names.get(&name).ok_or_else(|| {
                    Error(format!(
                        "cassette track {} auxiliary artifact {} payload is missing",
                        track.id, artifact.kind
                    ))
                })?;
                if archive.by_index_data(index).map_err(zip_error)?.size() != artifact.bytes {
                    return Err(Error(format!(
                        "cassette track {} auxiliary artifact {} length differs from manifest",
                        track.id, artifact.kind
                    )));
                }
                artifact_entries.insert((track.id, artifact.kind.clone()), index);
            }
        }
        Ok(Self {
            archive,
            manifest,
            entries,
            artifact_entries,
            limits,
            metadata_bytes: (directory_bytes, bytes.len() as u64),
        })
    }

    pub(super) fn metadata_bytes(&self) -> (u64, u64) {
        self.metadata_bytes
    }

    /// Inspect metadata without decoding any track payload.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Decode only this entry, verify its CRC/length/SHA-256 and return JSONL bytes.
    /// The caller owns any private scratch file and existing proxy validation.
    pub fn read_track(&mut self, id: u64) -> Result<Vec<u8>, Error> {
        let track = self.manifest.track(id)?;
        let index = *self
            .entries
            .get(&id)
            .ok_or_else(|| Error("cassette track payload is absent".into()))?;
        let bytes = read_bounded(&mut self.archive, index, self.limits.max_track_bytes)?;
        if bytes.len() as u64 != track.bytes || digest(&bytes) != track.sha256 {
            return Err(Error(format!(
                "cassette track {id} checksum or length mismatch"
            )));
        }
        Ok(bytes)
    }

    /// Decode one sealed auxiliary payload for the selected execution track.
    pub fn read_artifact(&mut self, id: u64, kind: &str) -> Result<Vec<u8>, Error> {
        let track = self.manifest.track(id)?;
        let artifact = track
            .artifacts
            .iter()
            .find(|artifact| artifact.kind == kind)
            .ok_or_else(|| {
                Error(format!(
                    "cassette track {id} has no auxiliary artifact {kind}"
                ))
            })?;
        let index = *self
            .artifact_entries
            .get(&(id, kind.to_owned()))
            .ok_or_else(|| Error("cassette auxiliary artifact payload is absent".into()))?;
        let bytes = read_bounded(&mut self.archive, index, self.limits.max_track_bytes)?;
        if bytes.len() as u64 != artifact.bytes || digest(&bytes) != artifact.sha256 {
            return Err(Error(format!(
                "cassette track {id} auxiliary artifact {kind} checksum or length mismatch"
            )));
        }
        Ok(bytes)
    }
}

fn read_bounded(
    archive: &mut ZipArchive<MetadataReader>,
    index: usize,
    limit: u64,
) -> Result<Vec<u8>, Error> {
    let mut entry = archive.by_index(index).map_err(zip_error)?;
    let expected = entry.size();
    let mut bytes = Vec::new();
    entry
        .by_ref()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > limit || bytes.len() as u64 != expected {
        return Err(Error(
            "cassette decompressed entry exceeds its declared length or limit".into(),
        ));
    }
    Ok(bytes)
}

fn open_file(path: &Path) -> Result<std::fs::File, Error> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() {
        return Err(Error("cassette archive must be a regular file".into()));
    }
    Ok(file)
}

fn io_error(error: std::io::Error) -> Error {
    Error(format!("cassette I/O error: {error}"))
}

fn zip_error(error: zip::result::ZipError) -> Error {
    Error(format!("invalid or unsupported cassette ZIP: {error}"))
}
