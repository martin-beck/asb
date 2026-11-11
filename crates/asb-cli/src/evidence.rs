//! Owned per-file post-grade publication, independent of evaluation/execution.
//! No cross-file transaction. Persistent destination locks are never unlinked.
use crate::output_lock::OutputLock;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

mod capture;
pub(crate) use capture::Capture;

const MAX_RECORD: usize = 16 * 1024 * 1024;
const LOCK_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug)]
enum PublicationError {
    BeforeCommit(String),
    CommitOutcomeUnconfirmed(String),
    PublishedUnconfirmed(String),
    PublishedCleanupRetained(String),
}
impl std::fmt::Display for PublicationError {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeforeCommit(message) => write!(output, "evidence not published: {message}"),
            Self::CommitOutcomeUnconfirmed(message) => write!(
                output,
                "evidence publication may have completed, but its commit outcome is unconfirmed: {message}; preserve destination and staging; do not repeat execution"
            ),
            Self::PublishedUnconfirmed(message) => write!(
                output,
                "evidence was published, but durability is unconfirmed: {message}; preserve the published bytes; do not repeat execution"
            ),
            Self::PublishedCleanupRetained(message) => write!(
                output,
                "evidence was published and synchronized, but private staging cleanup failed: {message}; preserve retained staging; do not repeat execution"
            ),
        }
    }
}
fn encode<T: Serialize>(value: &T, pretty: bool) -> Result<Vec<u8>, String> {
    struct Bounded {
        bytes: Vec<u8>,
        exceeded: bool,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_RECORD.saturating_sub(self.bytes.len()) {
                self.exceeded = true;
                return Err(io::Error::other("encoded JSON exceeds consumer bound"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut output = Bounded {
        bytes: Vec::new(),
        exceeded: false,
    };
    let result = if pretty {
        serde_json::to_writer_pretty(&mut output, value)
    } else {
        serde_json::to_writer(&mut output, value)
    };
    if result.is_err() {
        return Err(if output.exceeded {
            "encoded evidence JSON exceeds 16 MiB; published bytes are unchanged"
        } else {
            "cannot encode evidence JSON; published bytes are unchanged"
        }
        .into());
    }
    Ok(output.bytes)
}

/// Return the exact bounded, pretty JSON bytes used by `write_json`.
pub(crate) fn json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    encode(value, true)
}
pub(crate) fn append_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    // One prepared record, not the whole history, is retained in memory.
    let mut bytes = encode(value, false)?;
    // The consumer's tail window includes the terminal newline.
    if bytes.len() >= MAX_RECORD {
        return Err("log record exceeds 16 MiB; published bytes are unchanged".into());
    }
    bytes.push(b'\n');
    publish(path, |destination, stage| {
        if let Some(snapshot) = destination.snapshot.as_mut() {
            let length = snapshot.metadata()?.len();
            let mut reader = io::BufReader::new(snapshot.take(length));
            let mut line = Vec::new();
            loop {
                line.clear();
                let count = (&mut reader)
                    .take((MAX_RECORD + 1) as u64)
                    .read_until(b'\n', &mut line)?;
                if count == 0 {
                    break;
                }
                if count > MAX_RECORD || (count == MAX_RECORD && !line.ends_with(b"\n")) {
                    return Err(io::Error::other(
                        "historical log record exceeds 16 MiB; preserve it before repair",
                    ));
                }
                if !line.iter().all(u8::is_ascii_whitespace) {
                    serde_json::from_slice::<serde_json::Value>(&line).map_err(|_| io::Error::other("historical log has an incomplete or invalid JSON record; preserve it before repair"))?;
                }
                write_stage(stage, &line)?;
                if !line.ends_with(b"\n") {
                    write_stage(stage, b"\n")?;
                }
            }
        }
        write_stage(stage, &bytes)
    })
}
pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = json_bytes(value)?;
    publish(path, |_, stage| write_stage(stage, &bytes))
}
/// Keep domain-owned read/modify/write under the same lock and opened snapshot.
pub(crate) fn update_json<T: Serialize>(
    path: &Path,
    prepare: impl FnOnce(Option<&[u8]>) -> Result<T, String>,
) -> Result<(), String> {
    publish(path, |destination, stage| {
        let old = destination
            .snapshot
            .as_mut()
            .map(|file| {
                let mut bytes = Vec::new();
                file.take((MAX_RECORD + 1) as u64).read_to_end(&mut bytes)?;
                if bytes.len() > MAX_RECORD {
                    return Err(io::Error::other(
                        "historical JSON exceeds 16 MiB; preserve it before repair",
                    ));
                }
                Ok(bytes)
            })
            .transpose()?;
        let value = prepare(old.as_deref()).map_err(io::Error::other)?;
        let bytes = encode(&value, true).map_err(io::Error::other)?;
        write_stage(stage, &bytes)
    })
}
struct Destination {
    path: PathBuf,
    parent: PathBuf,
    snapshot: Option<File>,
    metadata: Option<Metadata>,
    _lock: OutputLock,
}
fn open_regular(path: &Path) -> io::Result<Option<File>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(io::Error::other(
                "destination must be a regular file, not a symlink, directory or pipe",
            ));
        }
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("opened destination is not a regular file"));
    }
    Ok(Some(file))
}
impl Destination {
    fn begin(path: &Path) -> Result<Self, String> {
        let name = path
            .file_name()
            .ok_or("evidence destination has no filename")?;
        let logical_parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(logical_parent).map_err(|error| error.to_string())?;
        let parent = logical_parent
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let path = parent.join(name);
        let key = format!("{:x}", Sha256::digest(name.as_encoded_bytes()));
        let lock = OutputLock::acquire_for(
            &parent.join(format!(".asb-evidence-{key}.lock")),
            "another evidence publisher owns this destination",
            LOCK_WAIT,
        )?;
        let snapshot = open_regular(&path).map_err(|error| error.to_string())?;
        let metadata = snapshot
            .as_ref()
            .map(File::metadata)
            .transpose()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            path,
            parent,
            snapshot,
            metadata,
            _lock: lock,
        })
    }
    fn unchanged(&self) -> io::Result<()> {
        let current = open_regular(&self.path)?;
        let matches = match (&self.metadata, current) {
            (None, None) => true,
            (Some(previous), Some(file)) => {
                let now = file.metadata()?;
                #[cfg(unix)]
                let identity = {
                    use std::os::unix::fs::MetadataExt;
                    previous.dev() == now.dev() && previous.ino() == now.ino()
                };
                #[cfg(not(unix))]
                let identity = portable_snapshot_identity(previous, &now);
                identity && previous.len() == now.len() && previous.modified()? == now.modified()?
            }
            _ => false,
        };
        if matches {
            Ok(())
        } else {
            Err(io::Error::other(
                "destination changed outside publication ownership; preserve both attempts",
            ))
        }
    }
}
// Conservative portable consistency, not a native file-ID/security promise.
// Filesystems without creation time still use regular-file, size and last-write
// checks rather than rejecting every replacement. Unix additionally binds the
// actual device/inode. Non-Unix runtime qualification is not implied.
#[cfg(any(not(unix), test))]
fn portable_snapshot_identity(previous: &Metadata, now: &Metadata) -> bool {
    previous.created().ok() == now.created().ok()
        && previous.permissions().readonly() == now.permissions().readonly()
}

struct Stage {
    directory: PathBuf,
    path: PathBuf,
    settled: std::cell::Cell<bool>,
}
impl Stage {
    fn create(parent: &Path) -> io::Result<(Self, File)> {
        let directory = parent.join(format!(
            ".asb-evidence-stage-{}",
            crate::tools::runtime::unique_id()
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory)?;
        let stage = Self {
            path: directory.join("content"),
            directory,
            settled: std::cell::Cell::new(false),
        };
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match checkpoint(FaultPoint::CreateFile).and_then(|()| options.open(&stage.path)) {
            Ok(file) => Ok((stage, file)),
            Err(error) => {
                let cleanup = stage.clean();
                Err(io::Error::other(format!(
                    "{error}{}",
                    cleanup.err().map_or(String::new(), |error| format!(
                        "; private staging cleanup failed: {error}; preserve {}",
                        stage.directory.display()
                    ))
                )))
            }
        }
    }
    fn clean(&self) -> io::Result<()> {
        // Explicit failure retains this exact stage for inspection. Drop must
        // not silently retry/remove bytes after reporting retained scratch.
        self.settled.set(true);
        checkpoint(FaultPoint::Cleanup)?;
        match fs::remove_file(&self.path) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        fs::remove_dir(&self.directory)
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        if !self.settled.get() {
            let _ = self.clean();
        }
    }
}
fn publish(
    path: &Path,
    prepare: impl FnOnce(&mut Destination, &mut File) -> io::Result<()>,
) -> Result<(), String> {
    let operation = || -> Result<(), PublicationError> {
        let before = |error: io::Error| PublicationError::BeforeCommit(error.to_string());
        let mut destination = Destination::begin(path).map_err(PublicationError::BeforeCommit)?;
        #[cfg(test)]
        let _active = tests::Active::new(&destination.path);
        let (stage, mut file) = Stage::create(&destination.parent).map_err(before)?;
        let preparation = (|| {
            prepare(&mut destination, &mut file)?;
            if let Some(metadata) = &destination.metadata {
                file.set_permissions(metadata.permissions())?;
            }
            checkpoint(FaultPoint::FileSync)?;
            file.sync_all()?;
            checkpoint(FaultPoint::Prepared)?;
            destination.unchanged()?;
            Ok(())
        })();
        let commit = preparation.map_err(CommitFailure::Before).and_then(|()| {
            checkpoint(FaultPoint::Rename).map_err(CommitFailure::Before)?;
            replace_stage(&stage.path, &destination.path)
                .and_then(|()| checkpoint(FaultPoint::RenameOutcome))
                .map_err(|error| classify_commit_failure(error, fs::symlink_metadata(&stage.path)))
        });
        if let Err(error) = commit {
            let error = match error {
                CommitFailure::Before(error) => error,
                CommitFailure::Unconfirmed(error) => {
                    // Do not remove evidence or retry a rename whose outcome
                    // cannot be established from our quiet private stage.
                    stage.settled.set(true);
                    return Err(PublicationError::CommitOutcomeUnconfirmed(format!(
                        "{error}; destination {}; staging {}",
                        destination.path.display(),
                        stage.directory.display()
                    )));
                }
            };
            let cleanup = stage.clean();
            return Err(PublicationError::BeforeCommit(format!(
                "{error}{}",
                cleanup.err().map_or(String::new(), |error| format!(
                    "; private staging cleanup failed: {error}; preserve {}",
                    stage.directory.display()
                ))
            )));
        }
        // Never repeat rename or roll back after this commit boundary.
        let confirmation = checkpoint(FaultPoint::DirectorySync).and_then(|()| {
            confirm_with(confirmation_policy(), || {
                File::open(&destination.parent)?.sync_all()
            })
        });
        let cleanup = stage.clean();
        match (confirmation, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(error)) => Err(PublicationError::PublishedCleanupRetained(format!(
                "{error}; retained at {}",
                stage.directory.display()
            ))),
            (confirmation, cleanup) => Err(PublicationError::PublishedUnconfirmed(format!(
                "{}{}",
                confirmation.err().map_or(String::new(), |error| format!(
                    "platform publication confirmation failed: {error}"
                )),
                cleanup.err().map_or(String::new(), |error| format!(
                    "; staging cleanup failed: {error}; retained at {}",
                    stage.directory.display()
                ))
            ))),
        }
    };
    operation().map_err(|error| format!("{}: {error}", path.display()))
}

enum CommitFailure {
    Before(io::Error),
    Unconfirmed(io::Error),
}
fn classify_commit_failure(error: io::Error, source: io::Result<Metadata>) -> CommitFailure {
    // Same-filesystem rename, no copy fallback, and a quiet private stage:
    // a remaining regular source means no move. Missing/uninspectable sources
    // are ambiguous, not proof of either success or failure. This is not a
    // hostile same-UID filesystem transaction/confinement guarantee.
    if source.is_ok_and(|metadata| metadata.is_file()) {
        CommitFailure::Before(error)
    } else {
        CommitFailure::Unconfirmed(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ConfirmationPolicy {
    UnixDirectory,
    WindowsWriteThrough,
    PortableFileAndRename,
}
fn confirmation_policy() -> ConfirmationPolicy {
    if cfg!(unix) {
        ConfirmationPolicy::UnixDirectory
    } else if cfg!(windows) {
        ConfirmationPolicy::WindowsWriteThrough
    } else {
        ConfirmationPolicy::PortableFileAndRename
    }
}
fn confirm_with(
    policy: ConfirmationPolicy,
    directory_sync: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    match policy {
        ConfirmationPolicy::UnixDirectory => directory_sync(),
        // File sync already completed. Windows additionally requests native
        // write-through rename; neither policy claims portable directory sync.
        ConfirmationPolicy::WindowsWriteThrough | ConfirmationPolicy::PortableFileAndRename => {
            Ok(())
        }
    }
}
#[cfg(not(windows))]
fn replace_stage(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}
#[cfg(any(windows, test))]
const WINDOWS_MOVE_FLAGS: u32 = 0x1 | 0x8; // REPLACE_EXISTING | WRITE_THROUGH only.
#[cfg(windows)]
#[allow(unsafe_code)]
fn replace_stage(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(source: *const u16, destination: *const u16, flags: u32) -> i32;
    }
    fn wide(path: &Path) -> io::Result<Vec<u16>> {
        let mut encoded: Vec<u16> = path.as_os_str().encode_wide().collect();
        if encoded.contains(&0) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        encoded.push(0);
        Ok(encoded)
    }
    let source = wide(source)?;
    let destination = wide(destination)?;
    // Paths are canonical-parent siblings on one volume. Never permit copy,
    // delayed moves or a weaker retry. File sync precedes this native request.
    // Microsoft documents WRITE_THROUGH as move completion, not a portable
    // directory-flush or multi-file/crash-transaction guarantee.
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), WINDOWS_MOVE_FLAGS) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum FaultPoint {
    Cleanup,
    CaptureMove,
    CreateFile,
    Prepared,
    FileSync,
    Rename,
    RenameOutcome,
    DirectorySync,
}
fn checkpoint(_point: FaultPoint) -> io::Result<()> {
    #[cfg(test)]
    return tests::checkpoint(_point);
    #[cfg(not(test))]
    Ok(())
}
fn write_stage(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    #[cfg(test)]
    return tests::write_stage(file, bytes);
    #[cfg(not(test))]
    file.write_all(bytes)
}
#[cfg(test)]
#[path = "../tests/unit/evidence_publication.rs"]
pub(crate) mod tests;
