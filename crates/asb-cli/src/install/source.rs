//! Complete, validate and synchronize private source bytes before atomic publication.
use super::*;
use std::io::Read;

pub(super) const MAX_SOURCE_BYTES: u64 = 1024 * 1024 * 1024;

mod controlled;
mod prepared;
pub(crate) use controlled::check_version_in;
pub(in crate::install) use controlled::install_input_in;
pub(super) use prepared::Input;
pub(super) use prepared::Quarantined;
pub(in crate::install) use prepared::Staging;
pub(super) use prepared::same_object;
pub(super) use prepared::settle_directory;
pub(super) use prepared::verify_pinned_checksum;
pub(super) use prepared::{open_named, same_named};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum FaultPoint {
    Copy,
    Mode,
    Sync,
    Commit,
    CommitTake,
    CommitCopy,
    CommitSync,
    CommitRename,
    Cleanup,
    CleanupTake,
    ArchiveCleanupTake,
    DirectoryTake,
    ArchiveDirectoryTake,
    Recovery,
}
fn checkpoint(point: FaultPoint, path: &Path) -> Result<(), Error> {
    #[cfg(test)]
    faults::run(point, path)?;
    #[cfg(not(test))]
    let _ = (point, path);
    Ok(())
}
#[cfg(test)]
pub(super) mod faults {
    use super::*;
    type Action = Box<dyn FnOnce(&Path) -> Result<(), Error>>;
    std::thread_local! {static HOOK:std::cell::RefCell<Option<(FaultPoint,Action)>>=const {std::cell::RefCell::new(None)};}
    pub(in crate::install) struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            HOOK.with_borrow_mut(|hook| *hook = None);
        }
    }
    pub(in crate::install) fn set(
        point: FaultPoint,
        action: impl FnOnce(&Path) -> Result<(), Error> + 'static,
    ) -> Reset {
        arm(point, action);
        Reset
    }
    pub(in crate::install) fn arm(
        point: FaultPoint,
        action: impl FnOnce(&Path) -> Result<(), Error> + 'static,
    ) {
        HOOK.with_borrow_mut(|hook| {
            assert!(hook.is_none());
            *hook = Some((point, Box::new(action)));
        });
    }
    pub(super) fn run(point: FaultPoint, path: &Path) -> Result<(), Error> {
        let action = HOOK.with_borrow_mut(|hook| {
            if hook.as_ref().is_some_and(|value| value.0 == point) {
                hook.take().map(|value| value.1)
            } else {
                None
            }
        });
        action.map_or(Ok(()), |action| action(path))
    }
}

pub(super) struct Options<'a> {
    pub kind: &'a str,
    pub limit: u64,
    pub checksum: Option<&'a str>,
    pub executable: bool,
}

fn size_error(kind: &str, limit: u64) -> Error {
    Error::Input(format!(
        "{kind} exceeds the installation size limit ({limit} bytes)"
    ))
}

fn validate_file(path: &Path, options: &Options<'_>) -> Result<fs::Metadata, Error> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| Error::Io(format!("cannot inspect staged {}: {error}", options.kind)))?;
    if !metadata.is_file() {
        return Err(Error::Input(format!(
            "staged {} is not a regular file",
            options.kind
        )));
    }
    if metadata.len() > options.limit {
        return Err(size_error(options.kind, options.limit));
    }
    if options.executable && metadata.len() == 0 {
        return Err(Error::Input(format!(
            "downloaded {} is empty; previous installation was preserved",
            options.kind
        )));
    }
    Ok(metadata)
}

#[derive(Clone, Copy)]
pub(super) enum Publication {
    Replace,
    NoClobber,
}

#[cfg(all(test, unix))]
mod publication_tests;
#[cfg(all(test, unix))]
mod repair_tests;

#[cfg(all(test, target_os = "linux"))]
mod lifecycle_tests;
