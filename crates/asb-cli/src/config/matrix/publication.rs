//! Per-file scalar replacement and manifest-last, no-replace sweep publication.
//! Failed sweep generations are deliberately retained, never recursively removed.
use super::*;
use std::fs::{File, OpenOptions};
use std::io;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Boundary {
    Write,
    FileSync,
    DirectorySync,
    Detach,
    DirectoryDetach,
    Recovery,
    Commit,
    AfterCommit,
    Cleanup,
}
trait Backend {
    fn boundary(&self, _: Boundary, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn write(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes)
    }
    fn sync(&self, file: &File) -> io::Result<()> {
        file.sync_all()
    }
    fn commit(&self, source: &Path, destination: &Path, no_replace: bool) -> io::Result<()> {
        if no_replace {
            fs::hard_link(source, destination)
        } else {
            fs::rename(source, destination)
        }
    }
}
struct System;
impl Backend for System {}

struct PublicationFailure {
    state: CommitState,
    error: Error,
}
#[derive(Clone, Copy, PartialEq)]
enum CommitState {
    NotPublished,
    Published,
    Unconfirmed,
}
impl PublicationFailure {
    fn before(error: Error) -> Self {
        Self {
            state: CommitState::NotPublished,
            error,
        }
    }
    fn after(error: Error) -> Self {
        Self {
            state: CommitState::Published,
            error,
        }
    }
    fn unconfirmed(error: Error) -> Self {
        Self {
            state: CommitState::Unconfirmed,
            error,
        }
    }
}

pub(super) fn confirmation(path: &Path, error: impl std::fmt::Display) -> Error {
    Error::Io(format!(
        "Setup was published at {}, but confirmation output failed: {error}; do not rerun expecting rollback",
        path.display()
    ))
}
pub(crate) fn scalar(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    scalar_with(path, bytes, &System)
}
fn scalar_with(path: &Path, bytes: &[u8], backend: &impl Backend) -> Result<(), Error> {
    let mode = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Some(metadata.permissions()),
        Ok(_) => None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(crate::io_error(error)),
    };
    let stage = Staged::write(path, bytes, mode, backend)?;
    stage
        .publish(path, false, backend)
        .map_err(|failure| failure.error)
}
pub(super) fn sweep_with_progress(
    path: &Path,
    prepared: &prepared::Sweep,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<(), Error> {
    sweep_with_progress_with(path, prepared, &System, progress)
}

#[cfg(test)]
fn sweep_with(
    path: &Path,
    prepared: &prepared::Sweep,
    backend: &impl Backend,
) -> Result<(), Error> {
    sweep_with_progress_with(path, prepared, backend, &mut |_, _| {})
}

fn sweep_with_progress_with(
    path: &Path,
    prepared: &prepared::Sweep,
    backend: &impl Backend,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<(), Error> {
    let directory = &prepared.directory;
    fs::create_dir(directory).map_err(|error| {
        Error::Io(format!(
            "cannot create setup generation {}: {error}; sweep was not published",
            directory.display()
        ))
    })?;
    // Keep one descriptor at a time, not up to 65,536 open files. On ANY error
    // retain this exact generation, including partial files and unknown arrivals.
    let mut state = CommitState::NotPublished;
    let result = (|| {
        let generation_owner = open(directory).map_err(crate::io_error)?;
        let mut identities = Vec::with_capacity(prepared.configurations.len());
        let total = prepared.configurations.len() + 1;
        progress(0, total);
        for (index, (name, bytes)) in prepared.configurations.iter().enumerate() {
            let file_path = directory.join(name);
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file_path)
                .map_err(crate::io_error)?;
            backend
                .boundary(Boundary::Write, &file_path)
                .map_err(crate::io_error)?;
            backend
                .write(&mut file, bytes.as_bytes())
                .map_err(crate::io_error)?;
            backend
                .boundary(Boundary::FileSync, &file_path)
                .map_err(crate::io_error)?;
            backend.sync(&file).map_err(crate::io_error)?;
            if !matches(&file, &file_path).map_err(crate::io_error)? {
                return Err(Error::Io(format!(
                    "generated configuration {} changed during writing",
                    file_path.display()
                )));
            }
            identities.push((
                key(&file).map_err(crate::io_error)?,
                file.metadata().map_err(crate::io_error)?,
            ));
            progress(index + 1, total);
        }
        synchronize(directory, backend).map_err(crate::io_error)?;
        synchronize(parent(path), backend).map_err(crate::io_error)?;
        let stage = Staged::write(path, prepared.manifest.as_bytes(), None, backend)?;
        // Retain compact witnesses, not one open descriptor per configuration.
        // This is prepublication validation, not an immutable multi-file snapshot.
        let validated = (|| {
            if !matches(&generation_owner, directory).map_err(crate::io_error)? {
                return Err(Error::Io(
                    "generated configuration directory changed before manifest publication".into(),
                ));
            }
            for ((name, text), (identity, metadata)) in
                prepared.configurations.iter().zip(&identities)
            {
                let selected = open(&directory.join(name)).map_err(crate::io_error)?;
                if key(&selected).map_err(crate::io_error)? != *identity
                    || !unchanged(metadata, &selected.metadata().map_err(crate::io_error)?)
                {
                    return Err(Error::Io(format!(
                        "generated configuration {name} changed before manifest publication"
                    )));
                }
                use std::io::Read;
                let mut bytes = Vec::new();
                (&selected)
                    .take(crate::benchmark::MAX_CONFIG_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(crate::io_error)?;
                if bytes != text.as_bytes()
                    || !unchanged(metadata, &selected.metadata().map_err(crate::io_error)?)
                {
                    return Err(Error::Io(format!(
                        "generated configuration {name} content changed before manifest publication"
                    )));
                }
            }
            Ok(())
        })();
        if let Err(error) = validated {
            return Err(stage.failed(io::Error::other(error.to_string()), backend));
        }
        stage.publish(path, true, backend).map_err(|failure| {
            state = failure.state;
            failure.error
        })?;
        progress(total, total);
        Ok(())
    })();
    result.map_err(|error| Error::Io(format!("{}{error}; generated configurations (possibly partial) remain in {}; no setup generation is recursively cleaned", if state == CommitState::NotPublished { format!("Sweep manifest was not published at {}: ", path.display()) } else { String::new() }, directory.display())))
}
fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}
fn private_directory(parent: &Path) -> io::Result<PathBuf> {
    let path = parent.join(format!(".asb-setup-{}", crate::tools::runtime::unique_id()));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&path)?;
    Ok(path)
}
fn open(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0200_0000 | 0x0020_0000);
    }
    options.open(path)
}
fn matches(owner: &File, name: &Path) -> io::Result<bool> {
    same(owner, &open(name)?)
}
#[cfg(unix)]
type FileId = (u64, u64);
#[cfg(windows)]
type FileId = (u64, [u8; 16]);
#[cfg(not(any(unix, windows)))]
type FileId = SystemTime;
#[cfg(unix)]
fn key(file: &File) -> io::Result<FileId> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}
#[cfg(windows)]
fn key(file: &File) -> io::Result<FileId> {
    windows_id(file)
}
#[cfg(not(any(unix, windows)))]
fn key(file: &File) -> io::Result<FileId> {
    file.metadata()?.created()
}
fn same(first: &File, second: &File) -> io::Result<bool> {
    Ok(key(first)? == key(second)?)
}
fn unchanged(first: &fs::Metadata, second: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if first.ctime() != second.ctime() || first.ctime_nsec() != second.ctime_nsec() {
            return false;
        }
    }
    first.is_file()
        && second.is_file()
        && first.len() == second.len()
        && first.modified().ok() == second.modified().ok()
        && first.permissions() == second.permissions()
}
#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_id(file: &File) -> io::Result<(u64, [u8; 16])> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct Id {
        volume: u64,
        id: [u8; 16],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut std::ffi::c_void,
            class: i32,
            information: *mut std::ffi::c_void,
            size: u32,
        ) -> i32;
    }
    let mut id = Id {
        volume: 0,
        id: [0; 16],
    };
    // SAFETY: live owned handle and correctly sized FILE_ID_INFO output storage.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            18,
            (&raw mut id).cast(),
            std::mem::size_of::<Id>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((id.volume, id.id))
}
fn synchronize(path: &Path, backend: &impl Backend) -> io::Result<()> {
    backend.boundary(Boundary::DirectorySync, path)?;
    #[cfg(unix)]
    {
        open(path)?.sync_all()
    }
    // std Windows directory FlushFileBuffers has no portable usable contract.
    // Files are still synced; native directory durability is NOT claimed there.
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

struct Staged {
    directory: PathBuf,
    file: PathBuf,
    owner: File,
    directory_owner: File,
}
impl Staged {
    fn write(
        destination: &Path,
        bytes: &[u8],
        mode: Option<fs::Permissions>,
        backend: &impl Backend,
    ) -> Result<Self, Error> {
        let directory = private_directory(parent(destination)).map_err(crate::io_error)?;
        let file = directory.join("prepared");
        let created = (|| {
            let directory_owner = open(&directory)?;
            let owner = OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .open(&file)?;
            Ok::<_, io::Error>(Self {
                directory: directory.clone(),
                file,
                owner,
                directory_owner,
            })
        })();
        let mut stage = created.map_err(|error| {
            Error::Io(format!(
                "setup was not published: {error}; inspect retained staging {}",
                directory.display()
            ))
        })?;
        let result = (|| {
            backend.boundary(Boundary::Write, &stage.file)?;
            backend.write(&mut stage.owner, bytes)?;
            if let Some(mode) = mode {
                stage.owner.set_permissions(mode)?;
            }
            backend.boundary(Boundary::FileSync, &stage.file)?;
            backend.sync(&stage.owner)
        })();
        match result {
            Ok(()) => Ok(stage),
            Err(error) => Err(stage.failed(error, backend)),
        }
    }
    // Atomic detachment closes the last-check→pathname-use window. The new
    // namespace is never passed to subprocesses or fault/validation callbacks.
    fn detach(self, backend: &impl Backend) -> Result<Self, Error> {
        let result = (|| {
            if !matches(&self.directory_owner, &self.directory)?
                || !matches(&self.owner, &self.file)?
            {
                return Err(io::Error::other("setup staging identity changed"));
            }
            let quiet = private_directory(parent(&self.directory))?;
            let file = quiet.join("prepared");
            let directory_owner = open(&quiet)?;
            let detached = (|| {
                backend.boundary(Boundary::Detach, &self.file)?;
                fs::rename(&self.file, &file)?;
                if !matches(&self.directory_owner, &self.directory).unwrap_or(false)
                    || !matches(&self.owner, &file).unwrap_or(false)
                {
                    backend.boundary(Boundary::Recovery, &self.file)?;
                    let recovery =
                        if matches(&self.directory_owner, &self.directory).unwrap_or(false) {
                            fs::hard_link(&file, &self.file) // Never replaces a second creator.
                        } else {
                            Err(io::Error::other(
                                "original staging directory changed; recovery refused",
                            ))
                        };
                    return Err(io::Error::other(format!(
                        "foreign setup entry detached; no-clobber recovery: {}; inspect retained staging {}",
                        recovery.map_or_else(
                            |error| error.to_string(),
                            |()| "original name restored; foreign quarantine retained".into()
                        ),
                        quiet.display()
                    )));
                }
                // Move the exposed directory too before removing it. Unexpected
                // arrivals fail nonrecursive removal and remain inside the quarantine.
                let old = quiet.join("previous-stage");
                backend.boundary(Boundary::DirectoryDetach, &self.directory)?;
                fs::rename(&self.directory, &old)?;
                if !matches(&self.directory_owner, &old)? {
                    return Err(io::Error::other(format!(
                        "foreign setup directory retained at {}; automatic directory recovery refused",
                        old.display()
                    )));
                }
                fs::remove_dir(&old).map_err(|error| {
                    io::Error::other(format!(
                        "{error}; inspect retained staging {}",
                        quiet.display()
                    ))
                })?;
                Ok(Self {
                    directory: quiet.clone(),
                    file,
                    owner: self.owner.try_clone()?,
                    directory_owner,
                })
            })();
            detached.map_err(|error| {
                io::Error::other(format!(
                    "{error}; inspect retained staging {}",
                    quiet.display()
                ))
            })
        })();
        result.map_err(|error| {
            Error::Io(format!(
                "setup was not published: {error}; inspect retained staging {}",
                self.directory.display()
            ))
        })
    }
    fn failed(self, error: io::Error, backend: &impl Backend) -> Error {
        match self.detach(backend) {
            Ok(quiet) => match quiet.cleanup(backend) {
                Ok(()) => Error::Io(format!("setup was not published: {error}")),
                Err(cleanup) => Error::Io(format!("setup was not published: {error}; {cleanup}")),
            },
            Err(cleanup) => Error::Io(format!("setup was not published: {error}; {cleanup}")),
        }
    }
    fn cleanup(self, backend: &impl Backend) -> io::Result<()> {
        // No exposed name is used here, and no retry occurs in Drop.
        let result = (|| {
            backend.boundary(Boundary::Cleanup, Path::new("private setup staging"))?;
            if self.file.try_exists()? {
                if !matches(&self.owner, &self.file)? {
                    return Err(io::Error::other("quiet staging identity changed"));
                }
                fs::remove_file(&self.file)?;
            }
            fs::remove_dir(&self.directory)
        })();
        result.map_err(|error| {
            io::Error::other(format!(
                "scratch cleanup unconfirmed: {error}; inspect retained staging {}",
                self.directory.display()
            ))
        })
    }
    fn publish(
        self,
        destination: &Path,
        no_replace: bool,
        backend: &impl Backend,
    ) -> Result<(), PublicationFailure> {
        let stage = self.detach(backend).map_err(PublicationFailure::before)?;
        // A boundary fault happens BEFORE attempting the syscall and is known
        // not to have committed. Once attempted, an error need not prove that.
        if let Err(error) = backend.boundary(Boundary::Commit, destination) {
            let cleanup = stage.cleanup(backend);
            return Err(PublicationFailure::before(Error::Io(format!(
                "setup was not published at {}: {error}{}",
                destination.display(),
                cleanup
                    .err()
                    .map_or_else(String::new, |error| format!("; {error}"))
            ))));
        }
        let committed = backend.commit(&stage.file, destination, no_replace);
        if let Err(error) = committed {
            // NO-REPLACE AlreadyExists is an unchanged competing entry. For
            // other filesystem errors retain evidence conservatively: remote
            // filesystems can commit before returning EIO/Interrupted, and an
            // automatic retry or cleanup would erase the recovery evidence.
            if !(no_replace && error.kind() == io::ErrorKind::AlreadyExists) {
                return Err(PublicationFailure::unconfirmed(Error::Io(format!(
                    "Setup publication outcome is unconfirmed at {} after the commit operation returned {error}; inspect this destination and retained staging {} before retrying; no rollback, retry or cleanup was attempted",
                    destination.display(),
                    stage.directory.display()
                ))));
            }
            let cleanup = stage.cleanup(backend);
            return Err(PublicationFailure::before(Error::Io(format!(
                "setup was not published at {}: {error}{}",
                destination.display(),
                cleanup
                    .err()
                    .map_or_else(String::new, |error| format!("; {error}"))
            ))));
        }
        let durable = backend
            .boundary(Boundary::AfterCommit, destination)
            .and_then(|()| synchronize(parent(destination), backend));
        let cleanup = stage.cleanup(backend);
        match (durable, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (durability, cleanup) => Err(PublicationFailure::after(Error::Io(format!(
                "Setup was published at {}, but durability/settlement is unconfirmed: {}; {}",
                destination.display(),
                durability.err().map_or_else(
                    || "destination parent synchronized".into(),
                    |error| error.to_string()
                ),
                cleanup
                    .err()
                    .map_or_else(|| "staging settled".into(), |error| error.to_string())
            )))),
        }
    }
}

#[cfg(test)]
mod tests;
