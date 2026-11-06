//! Report ownership, native atomic publication and explicitly recoverable fallback.
use super::*;
use crate::output_lock::OutputLock;

pub(super) struct Publisher {
    destination: PathBuf,
    stage: PathBuf,
    files: Vec<String>,
    committed: bool,
    _lock: OutputLock,
}
pub(super) struct Published {
    pub report: PathBuf,
    pub backup: Option<PathBuf>,
    pub recoverable_fallback: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RenameOperation {
    Exchange,
    NoReplace,
}
pub(super) trait RenameBackend {
    fn rename(&self, from: &Path, to: &Path, operation: RenameOperation) -> std::io::Result<()>;
    fn synchronize(&self, parent: &Path) -> std::io::Result<()> {
        fs::File::open(parent).and_then(|file| file.sync_all())
    }
}
struct NativeRename;
impl RenameBackend for NativeRename {
    fn rename(&self, from: &Path, to: &Path, operation: RenameOperation) -> std::io::Result<()> {
        native_rename(from, to, operation)
    }
}
impl Published {
    pub(super) fn confirmation_error(&self, error: impl std::fmt::Display) -> Error {
        Error::Io(format!(
            "Report was published at {}, but confirmation output failed: {error}. Publication method: {}. Previous report backup: {}",
            self.report.display(),
            if self.recoverable_fallback {
                "recoverable non-atomic replacement (destination absence/crash window possible)"
            } else {
                "native atomic rename/exchange"
            },
            self.backup.as_ref().map_or_else(
                || "none (first report)".into(),
                |path| path.display().to_string()
            )
        ))
    }
}
fn check_destination(path: &Path) -> Result<bool, Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(Error::Input(format!(
            "report destination {} must be a real directory, not a symlink or file; preserve it and choose another report location",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::Io(format!(
            "cannot inspect report destination {}: {error}",
            path.display()
        ))),
    }
}
impl Publisher {
    /// Called only after all source validation, aggregation and preview reads.
    pub(super) fn begin(destination: &Path, files: Vec<String>) -> Result<Self, Error> {
        let mut names = BTreeSet::new();
        for name in &files {
            let mut components = Path::new(name).components();
            if !matches!(components.next(), Some(std::path::Component::Normal(_)))
                || components.next().is_some()
                || !names.insert(name)
            {
                return Err(Error::Input(
                    "generated report artifact names must be unique single filenames".into(),
                ));
            }
        }
        check_destination(destination)?;
        let parent = destination
            .parent()
            .ok_or_else(|| Error::Input("report destination has no parent".into()))?;
        fs::create_dir_all(parent).map_err(crate::io_error)?;
        let name = destination
            .file_name()
            .ok_or_else(|| Error::Input("report destination has no filename".into()))?
            .to_string_lossy();
        let lock = OutputLock::acquire(
            &parent.join(format!(".{name}.asb-report.lock")),
            "another comparison report writer is active",
        )
        .map_err(Error::Io)?;
        check_destination(destination)?;
        let stage = parent.join(format!(
            ".{name}.asb-report-backup-{}",
            crate::tools::runtime::unique_id()
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&stage).map_err(crate::io_error)?;
        Ok(Self {
            destination: destination.into(),
            stage,
            files,
            committed: false,
            _lock: lock,
        })
    }
    pub(super) fn stage(&self) -> &Path {
        &self.stage
    }
    pub(super) fn commit(self) -> Result<Published, Error> {
        self.commit_with(&NativeRename)
    }
    pub(super) fn commit_with(mut self, backend: &impl RenameBackend) -> Result<Published, Error> {
        // Synchronize every known generated file before making the generation
        // visible. Unknown entries are never removed by cleanup.
        for name in &self.files {
            fs::File::open(self.stage.join(name))
                .and_then(|file| file.sync_all())
                .map_err(|error| {
                    Error::Io(format!(
                        "report preparation was not published: cannot synchronize {name}: {error}"
                    ))
                })?;
        }
        fs::File::open(&self.stage)
            .and_then(|file| file.sync_all())
            .map_err(crate::io_error)?;
        let previous = check_destination(&self.destination)?;
        let (backup, recoverable_fallback) = if previous {
            match backend.rename(&self.stage, &self.destination, RenameOperation::Exchange) {
                Ok(()) => (Some(self.stage.clone()), false),
                Err(error) if unsupported(&error) => {
                    (Some(self.replace_recoverably(backend)?), true)
                }
                Err(error) => return Err(self.unpublished(error)),
            }
        } else {
            backend
                .rename(&self.stage, &self.destination, RenameOperation::NoReplace)
                .map_err(|error| self.unpublished(error))?;
            (None, false)
        };
        self.committed = true;
        let published = Published {
            report: self.destination.join("report.html"),
            backup,
            recoverable_fallback,
        };
        let parent = self.destination.parent().expect("validated report parent");
        if let Err(error) = synchronize_published_parent(parent, backend) {
            return Err(Error::Io(format!(
                "Report was published at {}, but directory durability is unconfirmed: {error}. Publication method: {}. Previous report backup: {}",
                published.report.display(),
                if published.recoverable_fallback {
                    "recoverable non-atomic replacement (destination absence/crash window possible)"
                } else {
                    "native atomic rename/exchange"
                },
                published.backup.as_ref().map_or_else(
                    || "no previous generation".into(),
                    |path| path.display().to_string()
                )
            )));
        }
        Ok(published)
    }
    fn unpublished(&self, error: impl std::fmt::Display) -> Error {
        Error::Io(format!(
            "report was not published; existing report at {} is preserved: {error}. Use a filesystem supporting native no-replace rename or preserve/move the old report before retrying",
            self.destination.display()
        ))
    }
    fn replace_recoverably(&self, backend: &impl RenameBackend) -> Result<PathBuf, Error> {
        // Prove no-replace support on private, disposable directories BEFORE
        // moving any user report. Never probe an operation on user data.
        probe_no_replace(&self.stage, backend).map_err(|error| self.unpublished(error))?;
        let parent = self.destination.parent().expect("validated report parent");
        let backup = parent.join(format!(
            ".asb-report-previous-{}",
            crate::tools::runtime::unique_id()
        ));
        backend
            .rename(&self.destination, &backup, RenameOperation::NoReplace)
            .map_err(|error| self.unpublished(error))?;
        let forward = backend.synchronize(parent).and_then(|()| {
            backend.rename(&self.stage, &self.destination, RenameOperation::NoReplace)
        });
        if let Err(error) = forward {
            return Err(
                match backend.rename(&backup, &self.destination, RenameOperation::NoReplace) {
                    Ok(()) => {
                        let durability =
                            backend
                                .synchronize(parent)
                                .err()
                                .map_or_else(String::new, |sync| {
                                    format!(
                                        "; restored directory durability is unconfirmed: {sync}"
                                    )
                                });
                        Error::Io(format!(
                            "report was not published; previous report restored at {} after recoverable replacement failed: {error}{durability}",
                            self.destination.display()
                        ))
                    }
                    Err(restore) => Error::Io(format!(
                        "report was not published; previous report remains at recoverable backup {}, but automatic restore to {} failed: {restore}; replacement failed: {error}. Preserve both locations, move any conflicting destination aside, then {}",
                        backup.display(),
                        self.destination.display(),
                        restore_instruction(&backup, &self.destination)
                    )),
                },
            );
        }
        Ok(backup)
    }
}
impl Drop for Publisher {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for name in &self.files {
            let _ = fs::remove_file(self.stage.join(name));
        }
        let _ = fs::remove_dir(&self.stage);
    }
}
fn synchronize_published_parent(
    parent: &Path,
    backend: &impl RenameBackend,
) -> std::io::Result<()> {
    #[cfg(test)]
    if super::pipeline_tests::fail_post_commit_sync() {
        return Err(std::io::Error::other(
            "fixture parent synchronization failed",
        ));
    }
    backend.synchronize(parent)
}
fn restore_instruction(backup: &Path, destination: &Path) -> String {
    match (backup.to_str(), destination.to_str()) {
        (Some(backup), Some(destination)) => format!(
            "restore the backup with: mv -- '{}' '{}'",
            backup.replace('\'', "'\\''"),
            destination.replace('\'', "'\\''")
        ),
        _ => {
            "restore the backup with filesystem-safe tooling (these paths contain non-UTF-8 bytes)"
                .into()
        }
    }
}
fn unsupported(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    if matches!(
        error.raw_os_error(),
        Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)
    ) {
        return true;
    }
    false
}
fn probe_no_replace(stage: &Path, backend: &impl RenameBackend) -> std::io::Result<()> {
    let root = stage.join(format!(
        ".rename-probe-{}",
        crate::tools::runtime::unique_id()
    ));
    fs::create_dir(&root)?;
    let from = root.join("from");
    let to = root.join("to");
    let result = (|| {
        fs::create_dir(&from)?;
        fs::create_dir(&to)?;
        match backend.rename(&from, &to, RenameOperation::NoReplace) {
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
            Ok(()) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "native rename did not enforce no-replace semantics",
                ));
            }
        }
        fs::remove_dir(&to)?;
        backend.rename(&from, &to, RenameOperation::NoReplace)
    })();
    let cleanup = (|| {
        for path in [&from, &to] {
            match fs::remove_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        fs::remove_dir(&root)
    })();
    result.and(cleanup)?;
    fs::File::open(stage).and_then(|file| file.sync_all())
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(unsafe_code)]
pub(super) fn native_rename(
    stage: &Path,
    destination: &Path,
    operation: RenameOperation,
) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let stage = std::ffi::CString::new(stage.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "report staging path contains a NUL byte",
        )
    })?;
    let destination_c =
        std::ffi::CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "report destination contains a NUL byte",
            )
        })?;
    #[cfg(target_os = "linux")]
    let flags = if operation == RenameOperation::Exchange {
        libc::RENAME_EXCHANGE
    } else {
        libc::RENAME_NOREPLACE
    };
    // SAFETY: both C strings are live, NUL-terminated owned filesystem paths;
    // the report lock serializes ASB writers. Neither path is a broad root.
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            stage.as_ptr(),
            libc::AT_FDCWD,
            destination_c.as_ptr(),
            flags,
        )
    };
    #[cfg(target_os = "macos")]
    // SAFETY: the same owned live C strings above; platform flags preserve
    // exchange/no-replace semantics, rather than ordinary overwriting rename.
    let result = unsafe {
        libc::renamex_np(
            stage.as_ptr(),
            destination_c.as_ptr(),
            if operation == RenameOperation::Exchange {
                libc::RENAME_SWAP
            } else {
                libc::RENAME_EXCL
            },
        )
    };
    if result == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn native_rename(_: &Path, _: &Path, _: RenameOperation) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform has no configured native no-replace/exchange report backend",
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod native_rename_tests {
    use super::{RenameOperation, native_rename};

    #[test]
    fn linux_rename_preserves_no_replace_and_exchange_semantics() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("asb-native-rename-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let left = root.join("left");
        let right = root.join("right");
        std::fs::write(&left, b"left").unwrap();
        std::fs::write(&right, b"right").unwrap();
        assert_eq!(
            native_rename(&left, &right, RenameOperation::NoReplace)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&left).unwrap(), b"left");
        assert_eq!(std::fs::read(&right).unwrap(), b"right");
        native_rename(&left, &right, RenameOperation::Exchange).unwrap();
        assert_eq!(std::fs::read(&left).unwrap(), b"right");
        assert_eq!(std::fs::read(&right).unwrap(), b"left");
        std::fs::remove_dir_all(root).unwrap();
    }
}
