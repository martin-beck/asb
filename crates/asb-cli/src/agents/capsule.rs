//! Private, bounded manifest for explicit native replay capsules.
//!
//! A capsule records workspace-relative objects at controlled boundaries. It
//! is not a filesystem interceptor or security policy: a later staging backend
//! reconstructs these declared inputs/effects before launching the real agent.

use super::native_trace::Blob;
use crate::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

pub(crate) const SCHEMA_VERSION: u32 = 1;
const MAX_ENTRIES: usize = 4096;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_OBJECT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_OBJECT_STORE_BYTES: u64 = 64 * 1024 * 1024;
const OBJECTS_DIRECTORY: &str = "objects";
const MANIFEST_FILENAME: &str = "manifest.json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    /// A workspace-relative path. The manifest is private recording material.
    pub path: String,
    pub object: Blob,
    pub mode: u32,
}

/// An empty or permission-significant workspace directory in a capsule base.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Directory {
    /// A workspace-relative directory path.
    pub path: String,
    pub mode: u32,
}

/// A bounded workspace snapshot. Objects remain separately content addressed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Snapshot {
    pub directories: Vec<Directory>,
    pub files: Vec<Entry>,
}

/// Changes to apply only after a native replay's ordinary agent execution.
/// Removals are paths, never host handles or command traces.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Effects {
    pub directories: Vec<Directory>,
    pub files: Vec<Entry>,
    pub removed: Vec<String>,
}

/// A fresh private on-disk capsule. The manifest is created only after every
/// referenced object has been stored, so its existence marks a complete,
/// immutable capsule boundary.
pub(crate) struct Store {
    root: std::path::PathBuf,
    objects: std::path::PathBuf,
}

impl Store {
    /// Reserve a new empty capsule directory without replacing prior evidence.
    pub(crate) fn create(root: &Path) -> Result<Self, Error> {
        fs::create_dir(root).map_err(|error| match error.kind() {
            std::io::ErrorKind::AlreadyExists => {
                Error::Run("native capsule destination already exists".into())
            }
            _ => crate::io_error(error),
        })?;
        ensure_directory(root, "native capsule destination")?;
        let objects = root.join(OBJECTS_DIRECTORY);
        fs::create_dir(&objects).map_err(crate::io_error)?;
        ensure_directory(&objects, "native capsule object store")?;
        Ok(Self {
            root: root.to_path_buf(),
            objects,
        })
    }

    /// Open only a complete, canonical capsule. A directory without a valid
    /// final manifest is an interrupted capture, never replay input.
    pub(crate) fn open(root: &Path) -> Result<(Self, Manifest), Error> {
        ensure_directory(root, "native capsule destination")?;
        let objects = root.join(OBJECTS_DIRECTORY);
        ensure_directory(&objects, "native capsule object store")?;
        let path = root.join(MANIFEST_FILENAME);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| Error::Run("native capsule manifest is unavailable".into()))?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_BYTES as u64
        {
            return Err(Error::Run("native capsule manifest is invalid".into()));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        open_read_no_follow(&path)
            .map_err(crate::io_error)?
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(crate::io_error)?;
        if bytes.len() as u64 != metadata.len() {
            return Err(Error::Run(
                "native capsule manifest changed during read".into(),
            ));
        }
        let canonical = canonicalize(&bytes)?;
        if canonical != bytes {
            return Err(Error::Run(
                "native capsule manifest is not canonical".into(),
            ));
        }
        let manifest = serde_json::from_slice(&canonical)
            .map_err(|_| Error::Run("native capsule manifest is malformed".into()))?;
        Ok((
            Self {
                root: root.to_path_buf(),
                objects,
            },
            manifest,
        ))
    }

    pub(crate) fn objects(&self) -> &Path {
        &self.objects
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Publish the canonical manifest once. It intentionally has no replace
    /// operation; callers must make a new capsule if capture changes.
    pub(crate) fn publish(&self, manifest: &Manifest) -> Result<(), Error> {
        let bytes = canonicalize(
            &serde_json::to_vec(manifest)
                .map_err(|_| Error::Io("cannot encode native capsule manifest".into()))?,
        )?;
        let path = self.root.join(MANIFEST_FILENAME);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(crate::io_error)?;
        file.write_all(&bytes).map_err(crate::io_error)?;
        file.sync_all().map_err(crate::io_error)
    }
}

/// In-progress native capsule capture. A failed or abandoned capture has no
/// manifest, so it cannot be selected as replay evidence.
pub(crate) struct Capture {
    store: Store,
    manifest: Manifest,
}

impl Capture {
    /// Snapshot the prepared isolated workspace before agent execution.
    pub(crate) fn start(destination: &Path, workspace: &Path) -> Result<Self, Error> {
        let store = Store::create(destination)?;
        let base = capture_tree(store.objects(), workspace)?;
        Ok(Self {
            store,
            manifest: Manifest::from_snapshot(None, base),
        })
    }

    /// Snapshot final state, calculate declared effects, and publish the
    /// manifest last. No partial capture is replayable.
    pub(crate) fn finish(mut self, workspace: &Path) -> Result<PathBuf, Error> {
        self.manifest
            .set_effects(&capture_tree(self.store.objects(), workspace)?)?;
        self.store.publish(&self.manifest)?;
        Ok(self.store.root().to_path_buf())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_executable: Option<Blob>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base_directories: Vec<Directory>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Entry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effect_directories: Vec<Directory>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
}

impl Manifest {
    /// Bind one captured base workspace to an optional root executable identity.
    pub(crate) fn from_snapshot(root_executable: Option<Blob>, snapshot: Snapshot) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            root_executable,
            base: snapshot.files,
            base_directories: snapshot.directories,
            effects: Vec::new(),
            effect_directories: Vec::new(),
            removed: Vec::new(),
        }
    }

    /// Reconstruct only the captured base. Declared final effects remain a
    /// separate replay boundary and are never applied implicitly.
    pub(crate) fn stage_base(&self, objects: &Path, workspace: &Path) -> Result<(), Error> {
        validate(self)?;
        stage_snapshot(
            objects,
            &Snapshot {
                directories: self.base_directories.clone(),
                files: self.base.clone(),
            },
            workspace,
        )
    }

    /// Replace only an existing isolated workspace's contents with the
    /// verified base snapshot. The workspace directory itself remains owned by
    /// the workload session, preserving its lifecycle and mount identity.
    pub(crate) fn restore_base(&self, objects: &Path, workspace: &Path) -> Result<(), Error> {
        validate(self)?;
        clear_workspace(workspace)?;
        self.stage_base(objects, workspace)
    }

    /// Bind final workspace changes without applying them during base staging.
    pub(crate) fn set_effects(&mut self, final_snapshot: &Snapshot) -> Result<(), Error> {
        let effects = diff_snapshots(
            &Snapshot {
                directories: self.base_directories.clone(),
                files: self.base.clone(),
            },
            final_snapshot,
        )?;
        self.effects = effects.files;
        self.effect_directories = effects.directories;
        self.removed = effects.removed;
        validate(self)
    }

    /// Apply the declared final state after normal replay execution. This is
    /// intentionally separate from `stage_base`: no recorded effect is made
    /// visible to the agent before it runs.
    pub(crate) fn apply_effects(&self, objects: &Path, workspace: &Path) -> Result<(), Error> {
        validate(self)?;
        ensure_workspace(workspace)?;
        let mut removed = self.removed.clone();
        removed.sort_by_key(|path| std::cmp::Reverse(Path::new(path).components().count()));
        for path in removed {
            remove_path(workspace, &path)?;
        }
        let mut directories = self.effect_directories.clone();
        directories.sort_by_key(|directory| Path::new(&directory.path).components().count());
        for directory in directories {
            apply_directory(workspace, &directory)?;
        }
        for entry in &self.effects {
            replace_object(objects, entry, workspace)?;
        }
        Ok(())
    }
}

/// Reject non-canonical/private-unsafe manifest data before a capsule enters a
/// recording. Object bytes are staged separately and verified by digest.
pub(crate) fn canonicalize(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    if bytes.len() > MAX_BYTES {
        return Err(Error::Run("native capsule manifest exceeds bound".into()));
    }
    let manifest: Manifest = serde_json::from_slice(bytes)
        .map_err(|_| Error::Run("native capsule manifest is malformed".into()))?;
    validate(&manifest)?;
    serde_json::to_vec(&manifest)
        .map_err(|_| Error::Io("cannot encode native capsule manifest".into()))
}

fn validate(manifest: &Manifest) -> Result<(), Error> {
    if manifest.schema_version != SCHEMA_VERSION
        || manifest.base.len() + manifest.base_directories.len() > MAX_ENTRIES
        || manifest.effects.len() + manifest.effect_directories.len() + manifest.removed.len()
            > MAX_ENTRIES
        || !manifest.root_executable.iter().all(valid_blob)
    {
        return Err(Error::Run("native capsule manifest is invalid".into()));
    }
    let mut base = BTreeSet::new();
    for directory in &manifest.base_directories {
        valid_directory(directory, &mut base)?;
    }
    for entry in &manifest.base {
        valid_entry(entry, &mut base)?;
    }
    let mut effects = BTreeSet::new();
    for directory in &manifest.effect_directories {
        valid_directory(directory, &mut effects)?;
    }
    for entry in &manifest.effects {
        valid_entry(entry, &mut effects)?;
    }
    for path in &manifest.removed {
        valid_path(path, &mut effects)?;
    }
    Ok(())
}

fn valid_directory(directory: &Directory, paths: &mut BTreeSet<String>) -> Result<(), Error> {
    if directory.mode > 0o777 || valid_path(&directory.path, paths).is_err() {
        return Err(Error::Run("native capsule manifest is invalid".into()));
    }
    Ok(())
}

fn valid_entry(entry: &Entry, paths: &mut BTreeSet<String>) -> Result<(), Error> {
    if entry.mode > 0o777 || !valid_blob(&entry.object) || valid_path(&entry.path, paths).is_err() {
        return Err(Error::Run("native capsule manifest is invalid".into()));
    }
    Ok(())
}

fn valid_path(value: &str, paths: &mut BTreeSet<String>) -> Result<(), Error> {
    let path = Path::new(value);
    if value.len() > 1024
        || path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || !paths.insert(value.into())
    {
        return Err(Error::Run("native capsule manifest is invalid".into()));
    }
    Ok(())
}

fn valid_blob(blob: &Blob) -> bool {
    blob.sha256.len() == 64 && blob.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Store one regular bounded file by content digest. Existing exact objects are
/// reused; conflicting objects and symlinks are rejected without overwrite.
pub(crate) fn store_object(objects: &Path, source: &Path) -> Result<Blob, Error> {
    let metadata = fs::symlink_metadata(source)
        .map_err(|_| Error::Run("native capsule source is unavailable".into()))?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_OBJECT_BYTES
    {
        return Err(Error::Run(
            "native capsule source is invalid or exceeds bound".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    open_read_no_follow(source)
        .map_err(crate::io_error)?
        .take(MAX_OBJECT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(crate::io_error)?;
    if bytes.len() as u64 != metadata.len() {
        return Err(Error::Run(
            "native capsule source changed during capture".into(),
        ));
    }
    let object = payload(&bytes);
    fs::create_dir_all(objects).map_err(crate::io_error)?;
    ensure_directory(objects, "native capsule object store")?;
    let path = objects.join(&object.sha256);
    if path.exists() {
        if read_object(&path)? != bytes {
            return Err(Error::Run("native capsule object digest collision".into()));
        }
        return Ok(object);
    }
    if stored_bytes(objects)? + object.bytes > MAX_OBJECT_STORE_BYTES {
        return Err(Error::Run(
            "native capsule object store exceeds bound".into(),
        ));
    }
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(&bytes).map_err(crate::io_error)?;
            file.sync_all().map_err(crate::io_error)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if read_object(&path)? != bytes {
                return Err(Error::Run("native capsule object digest collision".into()));
            }
        }
        Err(error) => return Err(crate::io_error(error)),
    }
    Ok(object)
}

/// Stage one object below a fresh capsule workspace without replacing an
/// existing file. Callers must provide a fresh private workspace.
pub(crate) fn stage_object(objects: &Path, entry: &Entry, workspace: &Path) -> Result<(), Error> {
    let mut paths = BTreeSet::new();
    valid_entry(entry, &mut paths)?;
    let bytes = read_object(&objects.join(&entry.object.sha256))
        .map_err(|_| Error::Run("native capsule object is unavailable".into()))?;
    if payload(&bytes) != entry.object {
        return Err(Error::Run(
            "native capsule object differs from manifest".into(),
        ));
    }
    ensure_workspace(workspace)?;
    let target = workspace.join(&entry.path);
    create_parent_directories(workspace, Path::new(&entry.path))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(crate::io_error)?;
    file.write_all(&bytes).map_err(crate::io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(entry.mode))
            .map_err(crate::io_error)?;
    }
    Ok(())
}

fn replace_object(objects: &Path, entry: &Entry, workspace: &Path) -> Result<(), Error> {
    let mut paths = BTreeSet::new();
    valid_entry(entry, &mut paths)?;
    let bytes = read_object(&objects.join(&entry.object.sha256))
        .map_err(|_| Error::Run("native capsule object is unavailable".into()))?;
    if payload(&bytes) != entry.object {
        return Err(Error::Run(
            "native capsule object differs from manifest".into(),
        ));
    }
    create_parent_directories(workspace, Path::new(&entry.path))?;
    let target = workspace.join(&entry.path);
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(Error::Run("native capsule effect target is unsafe".into())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return stage_object(objects, entry, workspace);
        }
        Err(error) => return Err(crate::io_error(error)),
    }
    let mut file = open_write_truncate_no_follow(&target).map_err(crate::io_error)?;
    file.write_all(&bytes).map_err(crate::io_error)?;
    file.sync_all().map_err(crate::io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(entry.mode))
            .map_err(crate::io_error)?;
    }
    Ok(())
}

fn remove_path(workspace: &Path, relative: &str) -> Result<(), Error> {
    let mut paths = BTreeSet::new();
    valid_path(relative, &mut paths)?;
    let target = workspace.join(relative);
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(&target).map_err(crate::io_error)
        }
        Ok(_) => fs::remove_file(&target).map_err(crate::io_error),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(crate::io_error(error)),
    }
}

fn apply_directory(workspace: &Path, directory: &Directory) -> Result<(), Error> {
    let mut paths = BTreeSet::new();
    valid_directory(directory, &mut paths)?;
    create_parent_directories(workspace, Path::new(&directory.path))?;
    let target = workspace.join(&directory.path);
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(Error::Run("native capsule effect target is unsafe".into())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&target).map_err(crate::io_error)?;
        }
        Err(error) => return Err(crate::io_error(error)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(directory.mode))
            .map_err(crate::io_error)?;
    }
    Ok(())
}

/// Capture a small, regular-file-only workspace tree at an explicit boundary.
/// The caller owns the object store and must keep it outside `source`.
pub(crate) fn capture_tree(objects: &Path, source: &Path) -> Result<Snapshot, Error> {
    ensure_directory(source, "native capsule source workspace")?;
    let source = source.canonicalize().map_err(crate::io_error)?;
    let objects = std::path::absolute(objects).map_err(crate::io_error)?;
    if objects.starts_with(&source) {
        return Err(Error::Run(
            "native capsule object store must be outside source workspace".into(),
        ));
    }
    let mut snapshot = Snapshot::default();
    let mut pending = vec![(source, Path::new("").to_path_buf())];
    while let Some((directory, relative)) = pending.pop() {
        let mut children = fs::read_dir(&directory)
            .map_err(crate::io_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(crate::io_error)?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let name = child.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| Error::Run("native capsule source has a non-UTF-8 path".into()))?;
            let child_relative = relative.join(name);
            let child_path = child.path();
            let metadata = fs::symlink_metadata(&child_path).map_err(crate::io_error)?;
            if metadata.file_type().is_symlink() {
                return Err(Error::Run(
                    "native capsule source contains a symlink".into(),
                ));
            }
            if metadata.file_type().is_dir() {
                if snapshot.files.len() + snapshot.directories.len() >= MAX_ENTRIES {
                    return Err(Error::Run(
                        "native capsule snapshot exceeds entry bound".into(),
                    ));
                }
                snapshot.directories.push(Directory {
                    path: relative_string(&child_relative)?,
                    mode: file_mode(&metadata),
                });
                pending.push((child_path, child_relative));
            } else if metadata.file_type().is_file() {
                if snapshot.files.len() + snapshot.directories.len() >= MAX_ENTRIES {
                    return Err(Error::Run(
                        "native capsule snapshot exceeds entry bound".into(),
                    ));
                }
                snapshot.files.push(Entry {
                    path: relative_string(&child_relative)?,
                    object: store_object(&objects, &child_path)?,
                    mode: file_mode(&metadata),
                });
            } else {
                return Err(Error::Run(
                    "native capsule source contains an unsupported file".into(),
                ));
            }
        }
    }
    snapshot
        .directories
        .sort_by(|left, right| left.path.cmp(&right.path));
    snapshot
        .files
        .sort_by(|left, right| left.path.cmp(&right.path));
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

/// Reconstruct a snapshot into a fresh workspace without overwrite.
pub(crate) fn stage_snapshot(
    objects: &Path,
    snapshot: &Snapshot,
    workspace: &Path,
) -> Result<(), Error> {
    validate_snapshot(snapshot)?;
    ensure_workspace(workspace)?;
    let mut directories = snapshot.directories.clone();
    directories.sort_by_key(|directory| Path::new(&directory.path).components().count());
    for directory in directories {
        create_directory(workspace, &directory)?;
    }
    for file in &snapshot.files {
        stage_object(objects, file, workspace)?;
    }
    Ok(())
}

fn clear_workspace(workspace: &Path) -> Result<(), Error> {
    ensure_workspace(workspace)?;
    for entry in fs::read_dir(workspace).map_err(crate::io_error)? {
        let entry = entry.map_err(crate::io_error)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(crate::io_error)?;
        if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(path).map_err(crate::io_error)?;
        } else {
            fs::remove_file(path).map_err(crate::io_error)?;
        }
    }
    Ok(())
}

/// Calculate final workspace effects from two bounded snapshots. The result
/// holds only changed/new content and minimal root removals, not a duplicate
/// final tree.
pub(crate) fn diff_snapshots(base: &Snapshot, final_snapshot: &Snapshot) -> Result<Effects, Error> {
    validate_snapshot(base)?;
    validate_snapshot(final_snapshot)?;
    let base_directories: BTreeMap<_, _> = base
        .directories
        .iter()
        .map(|directory| (directory.path.as_str(), directory))
        .collect();
    let base_files: BTreeMap<_, _> = base
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let final_directories: BTreeMap<_, _> = final_snapshot
        .directories
        .iter()
        .map(|directory| (directory.path.as_str(), directory))
        .collect();
    let final_files: BTreeMap<_, _> = final_snapshot
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    if final_directories
        .keys()
        .any(|path| base_files.contains_key(path))
        || final_files
            .keys()
            .any(|path| base_directories.contains_key(path))
    {
        return Err(Error::Run(
            "native capsule cannot capture a file-kind replacement".into(),
        ));
    }
    let mut effects = Effects::default();
    for (path, directory) in &final_directories {
        if base_directories.get(path) != Some(directory) {
            effects.directories.push((*directory).clone());
        }
    }
    for (path, file) in &final_files {
        if base_files.get(path) != Some(file) {
            effects.files.push((*file).clone());
        }
    }
    let mut removed = BTreeSet::new();
    for path in base_directories.keys() {
        if !final_directories.contains_key(path) {
            removed.insert((*path).to_owned());
        }
    }
    for path in base_files.keys() {
        if !final_files.contains_key(path) {
            removed.insert((*path).to_owned());
        }
    }
    // Removing a directory removes its descendants. Persist only roots to
    // minimize the index and avoid repeated filesystem operations in replay.
    effects.removed = removed
        .iter()
        .filter(|path| {
            Path::new(path)
                .parent()
                .and_then(Path::to_str)
                .is_none_or(|parent| !removed.contains(parent))
        })
        .cloned()
        .collect();
    effects
        .directories
        .sort_by(|left, right| left.path.cmp(&right.path));
    effects
        .files
        .sort_by(|left, right| left.path.cmp(&right.path));
    validate_effects(&effects)?;
    Ok(effects)
}

fn validate_effects(effects: &Effects) -> Result<(), Error> {
    if effects.files.len() + effects.directories.len() + effects.removed.len() > MAX_ENTRIES {
        return Err(Error::Run(
            "native capsule effects exceed entry bound".into(),
        ));
    }
    let mut paths = BTreeSet::new();
    for directory in &effects.directories {
        valid_directory(directory, &mut paths)?;
    }
    for file in &effects.files {
        valid_entry(file, &mut paths)?;
    }
    for path in &effects.removed {
        valid_path(path, &mut paths)?;
    }
    Ok(())
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<(), Error> {
    if snapshot.files.len() + snapshot.directories.len() > MAX_ENTRIES {
        return Err(Error::Run(
            "native capsule snapshot exceeds entry bound".into(),
        ));
    }
    let mut paths = BTreeSet::new();
    for directory in &snapshot.directories {
        valid_directory(directory, &mut paths)?;
    }
    for file in &snapshot.files {
        valid_entry(file, &mut paths)?;
    }
    Ok(())
}

fn create_directory(workspace: &Path, directory: &Directory) -> Result<(), Error> {
    let mut paths = BTreeSet::new();
    valid_directory(directory, &mut paths)?;
    create_parent_directories(workspace, Path::new(&directory.path))?;
    let target = workspace.join(&directory.path);
    fs::create_dir(&target).map_err(crate::io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(directory.mode))
            .map_err(crate::io_error)?;
    }
    Ok(())
}

fn relative_string(path: &Path) -> Result<String, Error> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| Error::Run("native capsule source has a non-UTF-8 path".into()))
}

fn file_mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

fn open_read_no_follow(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn open_write_truncate_no_follow(path: &Path) -> std::io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn ensure_directory(path: &Path, name: &str) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path).map_err(crate::io_error)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Run(format!("{name} is not a directory")));
    }
    Ok(())
}

fn ensure_workspace(workspace: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(workspace) {
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => Err(Error::Run(
            "native capsule workspace is not a directory".into(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(workspace).map_err(crate::io_error)?;
            ensure_directory(workspace, "native capsule workspace")
        }
        Err(error) => Err(crate::io_error(error)),
    }
}

fn create_parent_directories(workspace: &Path, relative: &Path) -> Result<(), Error> {
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut current = workspace.to_path_buf();
    for component in parent.components() {
        let Component::Normal(component) = component else {
            return Err(Error::Run("native capsule target is invalid".into()));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => return Err(Error::Run("native capsule workspace path is unsafe".into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(crate::io_error)?;
                ensure_directory(&current, "native capsule workspace path")?;
            }
            Err(error) => return Err(crate::io_error(error)),
        }
    }
    Ok(())
}

fn read_object(path: &Path) -> Result<Vec<u8>, Error> {
    let metadata = fs::symlink_metadata(path).map_err(crate::io_error)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_OBJECT_BYTES
    {
        return Err(Error::Run("native capsule object is invalid".into()));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    open_read_no_follow(path)
        .map_err(crate::io_error)?
        .take(MAX_OBJECT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(crate::io_error)?;
    if bytes.len() as u64 != metadata.len() {
        return Err(Error::Run(
            "native capsule object changed during read".into(),
        ));
    }
    Ok(bytes)
}

fn stored_bytes(objects: &Path) -> Result<u64, Error> {
    let mut total = 0u64;
    let mut count = 0usize;
    for entry in fs::read_dir(objects).map_err(crate::io_error)? {
        let entry = entry.map_err(crate::io_error)?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(crate::io_error)?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_OBJECT_BYTES
        {
            return Err(Error::Run("native capsule object store is invalid".into()));
        }
        count += 1;
        if count > MAX_ENTRIES {
            return Err(Error::Run(
                "native capsule object store exceeds bound".into(),
            ));
        }
        total = total
            .checked_add(metadata.len())
            .ok_or_else(|| Error::Run("native capsule object store exceeds bound".into()))?;
        if total > MAX_OBJECT_STORE_BYTES {
            return Err(Error::Run(
                "native capsule object store exceeds bound".into(),
            ));
        }
    }
    Ok(total)
}

fn payload(bytes: &[u8]) -> Blob {
    Blob {
        sha256: format!("{:x}", Sha256::digest(bytes)),
        bytes: bytes.len() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

    struct TestRoot(std::path::PathBuf);

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_root(label: &str) -> TestRoot {
        let root = std::env::temp_dir().join(format!(
            "asb-capsule-{label}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        TestRoot(root)
    }

    fn blob(byte: char) -> Blob {
        Blob {
            sha256: std::iter::repeat_n(byte, 64).collect(),
            bytes: 3,
        }
    }

    #[test]
    fn canonical_manifest_retains_only_valid_bounded_workspace_entries() {
        let manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            root_executable: Some(blob('a')),
            base_directories: vec![Directory {
                path: "src".into(),
                mode: 0o755,
            }],
            base: vec![Entry {
                path: "src/main.rs".into(),
                object: blob('b'),
                mode: 0o644,
            }],
            effects: vec![Entry {
                path: "answer.txt".into(),
                object: blob('c'),
                mode: 0o644,
            }],
            effect_directories: Vec::new(),
            removed: Vec::new(),
        };
        let bytes = canonicalize(&serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert_eq!(
            serde_json::from_slice::<Manifest>(&bytes).unwrap(),
            manifest
        );
    }

    #[test]
    fn rejects_escape_duplicate_unknown_and_unbounded_entries() {
        let mut manifest = Manifest {
            schema_version: SCHEMA_VERSION,
            root_executable: None,
            base_directories: Vec::new(),
            base: vec![Entry {
                path: "../escape".into(),
                object: blob('a'),
                mode: 0o644,
            }],
            effects: Vec::new(),
            effect_directories: Vec::new(),
            removed: Vec::new(),
        };
        assert!(canonicalize(&serde_json::to_vec(&manifest).unwrap()).is_err());
        manifest.base[0].path = "safe".into();
        manifest.base.push(manifest.base[0].clone());
        assert!(canonicalize(&serde_json::to_vec(&manifest).unwrap()).is_err());
        assert!(canonicalize(br#"{"schema_version":1,"unknown":true}"#).is_err());
    }

    #[test]
    fn object_store_stages_verified_content_once() {
        let root = test_root("stage");
        let source = root.0.join("source.txt");
        fs::write(&source, b"capsule input").unwrap();
        let objects = root.0.join("objects");
        let object = store_object(&objects, &source).unwrap();
        let entry = Entry {
            path: "nested/input.txt".into(),
            object,
            mode: 0o640,
        };
        let workspace = root.0.join("workspace");
        stage_object(&objects, &entry, &workspace).unwrap();
        assert_eq!(
            fs::read(workspace.join("nested/input.txt")).unwrap(),
            b"capsule input"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(workspace.join("nested/input.txt"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o640
            );
        }
        assert!(stage_object(&objects, &entry, &workspace).is_err());
    }

    #[test]
    fn object_store_rejects_mutated_content() {
        let root = test_root("mutated");
        let source = root.0.join("source.txt");
        fs::write(&source, b"capsule input").unwrap();
        let objects = root.0.join("objects");
        let object = store_object(&objects, &source).unwrap();
        fs::write(objects.join(&object.sha256), b"changed").unwrap();
        let entry = Entry {
            path: "input.txt".into(),
            object,
            mode: 0o644,
        };
        assert!(stage_object(&objects, &entry, &root.0.join("workspace")).is_err());
    }

    #[test]
    fn snapshot_round_trips_regular_files_and_empty_directories() {
        let root = test_root("snapshot");
        let source = root.0.join("source");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("nested")).unwrap();
        fs::create_dir(source.join("empty")).unwrap();
        fs::write(source.join("nested/input.txt"), b"capsule input").unwrap();
        let objects = root.0.join("objects");
        let snapshot = capture_tree(&objects, &source).unwrap();
        assert_eq!(
            snapshot
                .directories
                .iter()
                .map(|directory| directory.path.as_str())
                .collect::<Vec<_>>(),
            ["empty", "nested"]
        );
        assert_eq!(snapshot.files.len(), 1);
        let workspace = root.0.join("workspace");
        stage_snapshot(&objects, &snapshot, &workspace).unwrap();
        assert!(workspace.join("empty").is_dir());
        assert_eq!(
            fs::read(workspace.join("nested/input.txt")).unwrap(),
            b"capsule input"
        );
    }

    #[test]
    fn snapshot_refuses_an_object_store_inside_the_source_workspace() {
        let root = test_root("source-object-store");
        let source = root.0.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("input.txt"), b"capsule input").unwrap();
        assert!(capture_tree(&source.join("objects"), &source).is_err());
    }

    #[test]
    fn manifest_stages_only_the_captured_base_snapshot() {
        let root = test_root("manifest-base");
        let source = root.0.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("input.txt"), b"capsule input").unwrap();
        let objects = root.0.join("objects");
        let mut manifest = Manifest::from_snapshot(None, capture_tree(&objects, &source).unwrap());
        let effect = store_object(&objects, &source.join("input.txt")).unwrap();
        manifest.effects.push(Entry {
            path: "output.txt".into(),
            object: effect,
            mode: 0o644,
        });
        let workspace = root.0.join("workspace");
        manifest.stage_base(&objects, &workspace).unwrap();
        assert_eq!(
            fs::read(workspace.join("input.txt")).unwrap(),
            b"capsule input"
        );
        assert!(!workspace.join("output.txt").exists());
    }

    #[test]
    fn restore_base_replaces_only_workspace_contents() {
        let root = test_root("restore-base");
        let source = root.0.join("source");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("nested")).unwrap();
        fs::write(source.join("nested/input.txt"), b"capsule input").unwrap();
        let objects = root.0.join("objects");
        let manifest = Manifest::from_snapshot(None, capture_tree(&objects, &source).unwrap());
        let workspace = root.0.join("workspace");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("stale.txt"), b"stale").unwrap();
        fs::create_dir(workspace.join("stale")).unwrap();
        fs::write(workspace.join("stale/data.txt"), b"stale").unwrap();
        manifest.restore_base(&objects, &workspace).unwrap();
        assert!(!workspace.join("stale.txt").exists());
        assert!(!workspace.join("stale").exists());
        assert_eq!(
            fs::read(workspace.join("nested/input.txt")).unwrap(),
            b"capsule input"
        );
        assert!(workspace.is_dir());
    }

    #[test]
    fn effects_retain_only_changed_content_and_root_removals() {
        let base = Snapshot {
            directories: vec![
                Directory {
                    path: "gone".into(),
                    mode: 0o755,
                },
                Directory {
                    path: "stable".into(),
                    mode: 0o755,
                },
            ],
            files: vec![
                Entry {
                    path: "changed.txt".into(),
                    object: blob('a'),
                    mode: 0o644,
                },
                Entry {
                    path: "removed.txt".into(),
                    object: blob('b'),
                    mode: 0o644,
                },
            ],
        };
        let final_snapshot = Snapshot {
            directories: vec![
                Directory {
                    path: "created".into(),
                    mode: 0o700,
                },
                Directory {
                    path: "stable".into(),
                    mode: 0o755,
                },
            ],
            files: vec![
                Entry {
                    path: "changed.txt".into(),
                    object: blob('c'),
                    mode: 0o644,
                },
                Entry {
                    path: "created/new.txt".into(),
                    object: blob('d'),
                    mode: 0o600,
                },
            ],
        };
        let effects = diff_snapshots(&base, &final_snapshot).unwrap();
        assert_eq!(
            effects.directories,
            vec![final_snapshot.directories[0].clone()]
        );
        assert_eq!(effects.files, final_snapshot.files);
        assert_eq!(effects.removed, vec!["gone", "removed.txt"]);

        let mut manifest = Manifest::from_snapshot(None, base);
        manifest.set_effects(&final_snapshot).unwrap();
        assert_eq!(manifest.effect_directories, effects.directories);
        assert_eq!(manifest.effects, effects.files);
        assert_eq!(manifest.removed, effects.removed);
    }

    #[test]
    fn effects_reject_file_directory_replacements() {
        let base = Snapshot {
            directories: Vec::new(),
            files: vec![Entry {
                path: "entry".into(),
                object: blob('a'),
                mode: 0o644,
            }],
        };
        let final_snapshot = Snapshot {
            directories: vec![Directory {
                path: "entry".into(),
                mode: 0o755,
            }],
            files: Vec::new(),
        };
        assert!(diff_snapshots(&base, &final_snapshot).is_err());
    }

    #[test]
    fn manifest_applies_declared_effects_only_after_base_staging() {
        let root = test_root("apply-effects");
        let source = root.0.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("changed.txt"), b"before").unwrap();
        fs::write(source.join("removed.txt"), b"remove").unwrap();
        let objects = root.0.join("objects");
        let mut manifest = Manifest::from_snapshot(None, capture_tree(&objects, &source).unwrap());
        fs::write(source.join("changed.txt"), b"after").unwrap();
        fs::remove_file(source.join("removed.txt")).unwrap();
        fs::create_dir(source.join("created")).unwrap();
        fs::write(source.join("created/output.txt"), b"output").unwrap();
        manifest
            .set_effects(&capture_tree(&objects, &source).unwrap())
            .unwrap();

        let workspace = root.0.join("workspace");
        manifest.stage_base(&objects, &workspace).unwrap();
        assert_eq!(fs::read(workspace.join("changed.txt")).unwrap(), b"before");
        assert!(workspace.join("removed.txt").exists());
        assert!(!workspace.join("created/output.txt").exists());

        manifest.apply_effects(&objects, &workspace).unwrap();
        assert_eq!(fs::read(workspace.join("changed.txt")).unwrap(), b"after");
        assert!(!workspace.join("removed.txt").exists());
        assert_eq!(
            fs::read(workspace.join("created/output.txt")).unwrap(),
            b"output"
        );
    }

    #[cfg(unix)]
    #[test]
    fn effect_application_refuses_a_replaced_symlink_target() {
        use std::os::unix::fs::symlink;

        let root = test_root("effect-symlink");
        let source = root.0.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("input.txt"), b"before").unwrap();
        let objects = root.0.join("objects");
        let mut manifest = Manifest::from_snapshot(None, capture_tree(&objects, &source).unwrap());
        fs::write(source.join("input.txt"), b"after").unwrap();
        manifest
            .set_effects(&capture_tree(&objects, &source).unwrap())
            .unwrap();
        let workspace = root.0.join("workspace");
        manifest.stage_base(&objects, &workspace).unwrap();
        let outside = root.0.join("outside.txt");
        fs::write(&outside, b"outside").unwrap();
        fs::remove_file(workspace.join("input.txt")).unwrap();
        symlink(&outside, workspace.join("input.txt")).unwrap();
        assert!(manifest.apply_effects(&objects, &workspace).is_err());
        assert_eq!(fs::read(outside).unwrap(), b"outside");
    }

    #[test]
    fn store_publishes_one_complete_immutable_manifest_last() {
        let root = test_root("store");
        let source = root.0.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("input.txt"), b"capsule input").unwrap();
        let destination = root.0.join("capsule");
        let store = Store::create(&destination).unwrap();
        let manifest =
            Manifest::from_snapshot(None, capture_tree(store.objects(), &source).unwrap());
        assert!(!destination.join(MANIFEST_FILENAME).exists());
        store.publish(&manifest).unwrap();
        let published = fs::read(destination.join(MANIFEST_FILENAME)).unwrap();
        assert_eq!(
            serde_json::from_slice::<Manifest>(&published).unwrap(),
            manifest
        );
        let (opened, reopened) = Store::open(&destination).unwrap();
        assert_eq!(opened.root(), destination);
        assert_eq!(reopened, manifest);
        assert!(store.publish(&manifest).is_err());
        assert!(Store::create(&destination).is_err());
    }

    #[test]
    fn store_refuses_an_interrupted_capture_without_manifest() {
        let root = test_root("incomplete-store");
        let destination = root.0.join("capsule");
        let _store = Store::create(&destination).unwrap();
        assert!(Store::open(&destination).is_err());
    }

    #[test]
    fn capture_publishes_only_after_a_final_effect_snapshot() {
        let root = test_root("capture");
        let workspace = root.0.join("workspace");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("input.txt"), b"before").unwrap();
        let destination = root.0.join("capsule");
        let capture = Capture::start(&destination, &workspace).unwrap();
        assert!(!destination.join(MANIFEST_FILENAME).exists());
        fs::write(workspace.join("input.txt"), b"after").unwrap();
        let published = capture.finish(&workspace).unwrap();
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(published.join(MANIFEST_FILENAME)).unwrap()).unwrap();
        assert_eq!(manifest.effects.len(), 1);
        assert_eq!(manifest.effects[0].path, "input.txt");
    }

    #[cfg(unix)]
    #[test]
    fn object_store_rejects_symlink_sources_and_workspace_parents() {
        use std::os::unix::fs::symlink;

        let root = test_root("symlink");
        let source = root.0.join("source.txt");
        fs::write(&source, b"capsule input").unwrap();
        let source_link = root.0.join("source-link.txt");
        symlink(&source, &source_link).unwrap();
        let objects = root.0.join("objects");
        assert!(store_object(&objects, &source_link).is_err());

        let object = store_object(&objects, &source).unwrap();
        let entry = Entry {
            path: "nested/input.txt".into(),
            object,
            mode: 0o644,
        };
        let workspace = root.0.join("workspace");
        fs::create_dir(&workspace).unwrap();
        let outside = root.0.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, workspace.join("nested")).unwrap();
        assert!(stage_object(&objects, &entry, &workspace).is_err());
        assert!(!outside.join("input.txt").exists());

        let symlink_source = root.0.join("tree");
        fs::create_dir(&symlink_source).unwrap();
        symlink(&source, symlink_source.join("input.txt")).unwrap();
        assert!(capture_tree(&root.0.join("tree-objects"), &symlink_source).is_err());
    }
}
