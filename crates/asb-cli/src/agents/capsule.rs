//! Private, bounded manifest for explicit native replay capsules.
//!
//! A capsule records workspace-relative objects at controlled boundaries. It
//! is not a filesystem interceptor or security policy: a later staging backend
//! reconstructs these declared inputs/effects before launching the real agent.

use super::native_trace::Blob;
use crate::Error;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path};

pub(crate) const SCHEMA_VERSION: u32 = 1;
const MAX_ENTRIES: usize = 4096;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_OBJECT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_OBJECT_STORE_BYTES: u64 = 64 * 1024 * 1024;

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
        || manifest.effects.len() > MAX_ENTRIES
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
    for entry in &manifest.effects {
        valid_entry(entry, &mut effects)?;
    }
    Ok(())
}

fn valid_directory(directory: &Directory, paths: &mut BTreeSet<String>) -> Result<(), Error> {
    let path = Path::new(&directory.path);
    if directory.path.len() > 1024
        || path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || directory.mode > 0o777
        || !paths.insert(directory.path.clone())
    {
        return Err(Error::Run("native capsule manifest is invalid".into()));
    }
    Ok(())
}

fn valid_entry(entry: &Entry, paths: &mut BTreeSet<String>) -> Result<(), Error> {
    let path = Path::new(&entry.path);
    if entry.path.len() > 1024
        || path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        || entry.mode > 0o777
        || !valid_blob(&entry.object)
        || !paths.insert(entry.path.clone())
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
