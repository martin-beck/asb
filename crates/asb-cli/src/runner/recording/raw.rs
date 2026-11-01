//! Consume only a committed raw object moved into a fresh private namespace.
//!
//! All destructive Unix operations are relative to pinned directory handles.
//! Quiet private custody excludes ordinary source-name races, not hostile
//! same-UID intrusion into the newly created owner-only custody directory.
use super::{Error, Track};
use std::fs;
use std::path::Path;

#[cfg(unix)]
mod unix {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

    pub(super) fn cleanup(
        output: &Path,
        track: &Track,
        original: Option<&fs::Metadata>,
    ) -> Result<(), Error> {
        let parent_path = output
            .parent()
            .ok_or_else(|| Error::Io("raw capture has no producer namespace; retained".into()))?;
        let parent = open_directory(parent_path).map_err(super::super::io_error)?;
        let name = CString::new(
            output
                .file_name()
                .ok_or_else(|| Error::Io("raw capture has no producer filename; retained".into()))?
                .as_bytes(),
        )
        .map_err(|_| Error::Io("raw capture filename contains a NUL; retained".into()))?;
        let mut file = match open_child(&parent, &name, false) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(_) => {
                return Err(Error::Io(
                    "raw capture cannot be opened without following links; retained".into(),
                ));
            }
        };
        let original = original.ok_or_else(|| {
            Error::Io("raw capture has no original file identity; retained".into())
        })?;
        let current = file.metadata().map_err(super::super::io_error)?;
        if !unchanged(original, &current, true) {
            return Err(Error::Io("raw capture identity changed; retained".into()));
        }
        if !matches_track(&mut file, track)?
            || !unchanged(
                original,
                &file.metadata().map_err(super::super::io_error)?,
                true,
            )
        {
            return Err(Error::Io(
                "raw capture changed after commit; retained".into(),
            ));
        }
        let custody_name = CString::new(format!(
            ".asb-raw-custody-{}",
            crate::workloads::TaskIdentity::local("raw", None).attempt_id
        ))
        .expect("fixed identity alphabet");
        create_custody(&parent, &custody_name).map_err(super::super::io_error)?;
        let custody = open_child(&parent, &custody_name, true).map_err(super::super::io_error)?;
        let source = c"source";
        #[cfg(test)]
        super::super::BEFORE_RAW_UNLINK.with_borrow_mut(|hook| {
            if let Some(replace) = hook.take() {
                replace();
            }
        });
        #[cfg(test)]
        test_support::check(test_support::Point::Move).map_err(super::super::io_error)?;
        move_entry(&parent, &name, &custody, source).map_err(|error| {
            Error::Io(format!(
                "raw capture could not enter private custody; source was not consumed: {error}"
            ))
        })?;
        #[cfg(test)]
        test_support::run_after_move();
        // A rename can change ctime without changing the object or its bytes.
        // Require full stable identity/size/mtime plus committed digest; then
        // require *all* metadata stable across the post-move bounded read.
        let verified = (|| {
            let mut moved = open_child(&custody, source, false).ok()?;
            let before = moved.metadata().ok()?;
            if !unchanged(original, &before, false)
                || !matches_track(&mut moved, track).ok()?
                || !unchanged(&before, &moved.metadata().ok()?, true)
            {
                return None;
            }
            Some(moved)
        })();
        let location = || location(parent_path, &parent, &custody_name);
        if verified.is_none() {
            return Err(Error::Io(match restore(&custody, source, &parent, &name) {
                Restoration::Restored => "raw capture candidate changed; restored without replacing any arrival".into(),
                Restoration::RestoredSyncUnknown => "raw capture candidate changed; restored without replacement, but restoration durability is unconfirmed".into(),
                Restoration::BothNames => format!("raw capture candidate changed; restored without replacement and also retained in {} (custody unlink failed)", location()),
                Restoration::Retained => format!("raw capture candidate changed; retained in {}; source restoration was not confirmed (existing arrivals preserved)", location()),
            }));
        }
        // The original source pathname is never unlinked. A later source-name
        // arrival is outside our custody and is left untouched.
        #[cfg(test)]
        test_support::check(test_support::Point::Unlink).map_err(|_| {
            Error::Io(format!(
                "raw capture unlink failed; retained in {}",
                location()
            ))
        })?;
        unlink(&custody, source).map_err(|_| {
            Error::Io(format!(
                "raw capture unlink failed; retained in {}",
                location()
            ))
        })?;
        #[cfg(test)]
        test_support::check(test_support::Point::Sync).map_err(|_| {
            Error::Io(
                "raw capture was removed; namespace durability is unconfirmed (not retained)"
                    .into(),
            )
        })?;
        custody
            .sync_all()
            .and_then(|()| parent.sync_all())
            .map_err(|_| {
                Error::Io(
                    "raw capture was removed; namespace durability is unconfirmed (not retained)"
                        .into(),
                )
            })?;
        // Intentionally retain the now-empty private directory: removing its
        // original name after checking would reintroduce a namespace race.
        Ok(())
    }

    fn matches_track(file: &mut File, track: &Track) -> Result<bool, Error> {
        Ok(super::super::payload_identity(file)? == (track.bytes, track.sha256.clone()))
    }

    fn unchanged(a: &fs::Metadata, b: &fs::Metadata, ctime: bool) -> bool {
        a.is_file()
            && b.is_file()
            && a.dev() == b.dev()
            && a.ino() == b.ino()
            && a.len() == b.len()
            && a.mtime() == b.mtime()
            && a.mtime_nsec() == b.mtime_nsec()
            && (!ctime || (a.ctime() == b.ctime() && a.ctime_nsec() == b.ctime_nsec()))
    }

    fn location(path: &Path, parent: &File, name: &CStr) -> String {
        let identity = parent
            .metadata()
            .map(|m| format!("device {} inode {}", m.dev(), m.ino()))
            .unwrap_or_else(|_| "identity unavailable".into());
        format!(
            "pinned producer namespace {identity}, custody {}/source (original parent {})",
            name.to_string_lossy(),
            path.display()
        )
    }

    #[allow(unsafe_code)]
    fn open_directory(path: &Path) -> io::Result<File> {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
    }

    #[allow(unsafe_code)]
    fn open_child(parent: &File, name: &CStr, directory: bool) -> io::Result<File> {
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC
            | if directory { libc::O_DIRECTORY } else { 0 };
        // SAFETY: live owned directory handle and NUL-terminated single component.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    #[allow(unsafe_code)]
    fn create_custody(parent: &File, name: &CStr) -> io::Result<()> {
        // SAFETY: relative, create-new directory; no pre-existing entry adopted.
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[allow(unsafe_code)]
    fn move_entry(from: &File, name: &CStr, to: &File, target: &CStr) -> io::Result<()> {
        // SAFETY: target is absent in a fresh, owner-only private directory.
        if unsafe {
            libc::renameat(
                from.as_raw_fd(),
                name.as_ptr(),
                to.as_raw_fd(),
                target.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    enum Restoration {
        Restored,
        RestoredSyncUnknown,
        BothNames,
        Retained,
    }

    #[allow(unsafe_code)]
    fn restore(from: &File, source: &CStr, to: &File, name: &CStr) -> Restoration {
        #[cfg(test)]
        if test_support::check(test_support::Point::Restore).is_err() {
            return Restoration::Retained;
        }
        // linkat without AT_SYMLINK_FOLLOW atomically refuses an existing name,
        // including symlinks. Directories/unsupported filesystems are retained.
        // This is not a multi-entry transaction: interrupted cleanup may leave
        // both hard links, never replacement of a new source-name arrival.
        if unsafe {
            libc::linkat(
                from.as_raw_fd(),
                source.as_ptr(),
                to.as_raw_fd(),
                name.as_ptr(),
                0,
            )
        } != 0
        {
            return Restoration::Retained;
        }
        #[cfg(test)]
        if test_support::check(test_support::Point::RestoreUnlink).is_err() {
            return Restoration::BothNames;
        }
        if unlink(from, source).is_err() {
            return Restoration::BothNames;
        }
        #[cfg(test)]
        if test_support::check(test_support::Point::RestoreSync).is_err() {
            return Restoration::RestoredSyncUnknown;
        }
        if from.sync_all().and_then(|()| to.sync_all()).is_err() {
            return Restoration::RestoredSyncUnknown;
        }
        Restoration::Restored
    }

    #[allow(unsafe_code)]
    fn unlink(parent: &File, name: &CStr) -> io::Result<()> {
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) mod test_support {
        use std::cell::RefCell;
        #[derive(Clone, Copy, Eq, PartialEq)]
        pub enum Point {
            Move,
            Restore,
            RestoreUnlink,
            RestoreSync,
            Unlink,
            Sync,
        }
        std::thread_local! {
            pub static FAULT: RefCell<Option<Point>> = const { RefCell::new(None) };
            pub static AFTER_MOVE: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
        }
        pub fn check(point: Point) -> std::io::Result<()> {
            if FAULT.with_borrow(|fault| *fault == Some(point)) {
                Err(std::io::Error::other("private fixture fault"))
            } else {
                Ok(())
            }
        }
        pub fn run_after_move() {
            AFTER_MOVE.with_borrow_mut(|hook| {
                if let Some(hook) = hook.take() {
                    hook();
                }
            });
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::unix::test_support::{AFTER_MOVE, FAULT, Point};
    use super::*;
    use std::path::PathBuf;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/raw-custody-fixtures")
                .join(crate::workloads::TaskIdentity::local("raw-custody", None).attempt_id);
            fs::create_dir_all(root.join("producer")).unwrap();
            fs::write(root.join("producer/capture.jsonl"), b"{}\n").unwrap();
            Self(root)
        }
        fn output(&self) -> PathBuf {
            self.0.join("producer/capture.jsonl")
        }
        fn track(&self) -> Track {
            Track {
                recorder: None,
                id: 1,
                batch: 1,
                replica: 1,
                complete: true,
                process_success: Some(true),
                workload_success: Some(true),
                bytes: 3,
                sha256: crate::cassette::digest(b"{}\n"),
                statistics: Default::default(),
                native: None,
                artifacts: Vec::new(),
            }
        }
        fn custody(&self, parent: &str) -> PathBuf {
            fs::read_dir(self.0.join(parent))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".asb-raw-custody-")
                })
                .unwrap()
        }
        fn replace_before_move(&self) {
            let root = self.0.clone();
            super::super::BEFORE_RAW_UNLINK.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move || {
                    fs::rename(
                        root.join("producer/capture.jsonl"),
                        root.join("saved-original"),
                    )
                    .unwrap();
                    fs::write(root.join("producer/capture.jsonl"), b"foreign bytes").unwrap();
                }))
            });
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            FAULT.with_borrow_mut(|fault| *fault = None);
            AFTER_MOVE.with_borrow_mut(|hook| {
                hook.take();
            });
            super::super::BEFORE_RAW_UNLINK.with_borrow_mut(|hook| {
                hook.take();
            });
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn missing_source_move_preserves_the_os_cause_and_never_consumes_original() {
        let fixture = Fixture::new();
        let original = fs::metadata(fixture.output()).unwrap();
        let root = fixture.0.clone();
        super::super::BEFORE_RAW_UNLINK.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move || {
                fs::rename(
                    root.join("producer/capture.jsonl"),
                    root.join("saved-original"),
                )
                .unwrap();
            }))
        });
        let error = cleanup(&fixture.output(), &fixture.track(), Some(&original))
            .unwrap_err()
            .to_string();
        assert!(error.contains("source was not consumed"), "{error}");
        assert!(
            error.contains(&std::io::Error::from_raw_os_error(libc::ENOENT).to_string()),
            "{error}"
        );
        assert_eq!(fs::read(fixture.0.join("saved-original")).unwrap(), b"{}\n");
        assert!(!fixture.custody("producer").join("source").exists());
    }

    #[test]
    fn custody_faults_distinguish_retention_from_post_unlink_sync_failure() {
        for fault in [Point::Move, Point::Unlink, Point::Sync] {
            let fixture = Fixture::new();
            let original = fs::metadata(fixture.output()).unwrap();
            FAULT.with_borrow_mut(|point| *point = Some(fault));
            let error = cleanup(&fixture.output(), &fixture.track(), Some(&original))
                .unwrap_err()
                .to_string();
            let custody = fixture.custody("producer");
            match fault {
                Point::Move => {
                    assert_eq!(fs::read(fixture.output()).unwrap(), b"{}\n");
                    assert!(!custody.join("source").exists());
                }
                Point::Unlink => {
                    assert!(!fixture.output().exists());
                    assert_eq!(fs::read(custody.join("source")).unwrap(), b"{}\n");
                    assert!(
                        error.contains("retained in pinned producer namespace"),
                        "{error}"
                    );
                    assert!(
                        error.contains(custody.file_name().unwrap().to_str().unwrap()),
                        "{error}"
                    );
                }
                Point::Sync => {
                    assert!(!fixture.output().exists());
                    assert!(!custody.join("source").exists());
                    assert!(error.contains("was removed"), "{error}");
                    assert!(error.contains("not retained"), "{error}");
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn restoration_never_replaces_an_arrival_and_fault_diagnostics_match_actual_names() {
        for fault in [
            None,
            Some(Point::Restore),
            Some(Point::RestoreUnlink),
            Some(Point::RestoreSync),
        ] {
            let fixture = Fixture::new();
            let original = fs::metadata(fixture.output()).unwrap();
            fixture.replace_before_move();
            FAULT.with_borrow_mut(|point| *point = fault);
            let error = cleanup(&fixture.output(), &fixture.track(), Some(&original))
                .unwrap_err()
                .to_string();
            let custody = fixture.custody("producer");
            assert_eq!(fs::read(fixture.0.join("saved-original")).unwrap(), b"{}\n");
            if fault == Some(Point::Restore) {
                assert!(!fixture.output().exists());
                assert_eq!(fs::read(custody.join("source")).unwrap(), b"foreign bytes");
                assert!(error.contains("retained in"), "{error}");
            } else {
                assert_eq!(fs::read(fixture.output()).unwrap(), b"foreign bytes");
                if fault == Some(Point::RestoreUnlink) {
                    assert_eq!(fs::read(custody.join("source")).unwrap(), b"foreign bytes");
                    assert!(error.contains("also retained"), "{error}");
                } else {
                    assert!(!custody.join("source").exists());
                    assert!(!error.contains("retained in"), "{error}");
                    if fault == Some(Point::RestoreSync) {
                        assert!(error.contains("durability is unconfirmed"), "{error}");
                    }
                }
            }
        }
        let fixture = Fixture::new();
        let original = fs::metadata(fixture.output()).unwrap();
        fixture.replace_before_move();
        let output = fixture.output();
        AFTER_MOVE.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move || fs::write(output, b"new arrival").unwrap()))
        });
        let error = cleanup(&fixture.output(), &fixture.track(), Some(&original))
            .unwrap_err()
            .to_string();
        assert_eq!(fs::read(fixture.output()).unwrap(), b"new arrival");
        assert_eq!(
            fs::read(fixture.custody("producer").join("source")).unwrap(),
            b"foreign bytes"
        );
        assert!(error.contains("existing arrivals preserved"), "{error}");
    }

    #[test]
    fn namespace_and_source_arrivals_after_move_are_outside_cleanup_authority() {
        for replace_parent in [false, true] {
            let fixture = Fixture::new();
            let original = fs::metadata(fixture.output()).unwrap();
            let root = fixture.0.clone();
            AFTER_MOVE.with_borrow_mut(|hook| {
                *hook = Some(Box::new(move || {
                    if replace_parent {
                        fs::rename(root.join("producer"), root.join("saved-producer")).unwrap();
                        fs::create_dir(root.join("producer")).unwrap();
                    }
                    fs::write(root.join("producer/capture.jsonl"), b"new arrival").unwrap();
                }))
            });
            cleanup(&fixture.output(), &fixture.track(), Some(&original)).unwrap();
            assert_eq!(fs::read(fixture.output()).unwrap(), b"new arrival");
            assert!(
                !fixture
                    .custody(if replace_parent {
                        "saved-producer"
                    } else {
                        "producer"
                    })
                    .join("source")
                    .exists()
            );
        }
    }

    #[test]
    fn restoration_uses_the_pinned_namespace_even_after_parent_replacement() {
        let fixture = Fixture::new();
        let original = fs::metadata(fixture.output()).unwrap();
        fixture.replace_before_move();
        let root = fixture.0.clone();
        AFTER_MOVE.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move || {
                fs::rename(root.join("producer"), root.join("saved-producer")).unwrap();
                fs::create_dir(root.join("producer")).unwrap();
                fs::write(
                    root.join("producer/capture.jsonl"),
                    b"replacement namespace",
                )
                .unwrap();
            }))
        });
        let error = cleanup(&fixture.output(), &fixture.track(), Some(&original))
            .unwrap_err()
            .to_string();
        assert_eq!(
            fs::read(fixture.output()).unwrap(),
            b"replacement namespace"
        );
        assert_eq!(
            fs::read(fixture.0.join("saved-producer/capture.jsonl")).unwrap(),
            b"foreign bytes"
        );
        assert!(!fixture.custody("saved-producer").join("source").exists());
        assert!(error.contains("restored without replacing"), "{error}");
    }

    #[test]
    fn unwind_after_move_retains_bytes_without_drop_cleanup_or_retry() {
        let fixture = Fixture::new();
        let original = fs::metadata(fixture.output()).unwrap();
        AFTER_MOVE
            .with_borrow_mut(|hook| *hook = Some(Box::new(|| panic!("private post-move fixture"))));
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cleanup(&fixture.output(), &fixture.track(), Some(&original))
        }));
        assert!(unwind.is_err());
        assert!(!fixture.output().exists());
        assert_eq!(
            fs::read(fixture.custody("producer").join("source")).unwrap(),
            b"{}\n"
        );
        // Opening/dropping handles is the only unwind work: it neither unlinks
        // the retained source nor starts a retry worker.
        assert!(AFTER_MOVE.with_borrow(|hook| hook.is_none()));
    }

    #[test]
    fn moved_foreign_directory_is_retained_without_traversal_or_recursive_cleanup() {
        let fixture = Fixture::new();
        let original = fs::metadata(fixture.output()).unwrap();
        let root = fixture.0.clone();
        super::super::BEFORE_RAW_UNLINK.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move || {
                fs::rename(
                    root.join("producer/capture.jsonl"),
                    root.join("saved-original"),
                )
                .unwrap();
                fs::create_dir(root.join("producer/capture.jsonl")).unwrap();
                fs::write(
                    root.join("producer/capture.jsonl/sentinel"),
                    b"foreign tree",
                )
                .unwrap();
            }))
        });
        let error = cleanup(&fixture.output(), &fixture.track(), Some(&original))
            .unwrap_err()
            .to_string();
        assert!(error.contains("retained in"), "{error}");
        assert_eq!(
            fs::read(fixture.custody("producer").join("source/sentinel")).unwrap(),
            b"foreign tree"
        );
        assert_eq!(fs::read(fixture.0.join("saved-original")).unwrap(), b"{}\n");
    }

    #[test]
    fn changed_committed_bytes_after_move_are_restored_not_consumed() {
        let fixture = Fixture::new();
        let original = fs::metadata(fixture.output()).unwrap();
        let root = fixture.0.clone();
        AFTER_MOVE.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move || {
                let custody = fs::read_dir(root.join("producer"))
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| path.is_dir())
                    .unwrap();
                fs::write(custody.join("source"), b"[]\n").unwrap();
            }))
        });
        let error = cleanup(&fixture.output(), &fixture.track(), Some(&original))
            .unwrap_err()
            .to_string();
        assert_eq!(fs::read(fixture.output()).unwrap(), b"[]\n");
        assert!(error.contains("restored without replacing"), "{error}");
    }

    #[test]
    fn ordinary_rename_ctime_change_is_not_a_false_identity_failure_and_drop_never_retries() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let original = fs::metadata(fixture.output()).unwrap();
        cleanup(&fixture.output(), &fixture.track(), Some(&original)).unwrap();
        let custody = fixture.custody("producer");
        assert_eq!(
            fs::metadata(&custody).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(fs::read_dir(custody).unwrap().next().is_none());
        assert!(!fixture.output().exists());
        let fixture = Fixture::new();
        let original = fs::metadata(fixture.output()).unwrap();
        FAULT.with_borrow_mut(|point| *point = Some(Point::Unlink));
        cleanup(&fixture.output(), &fixture.track(), Some(&original)).unwrap_err();
        FAULT.with_borrow_mut(|point| *point = None);
        // No owner Drop or failed-cleanup retry exists. Ordinary local files
        // can be inspected unchanged after explicit cleanup has returned.
        assert_eq!(
            fs::read(fixture.custody("producer").join("source")).unwrap(),
            b"{}\n"
        );
    }
}

pub(super) fn cleanup(
    output: &Path,
    track: &Track,
    original: Option<&fs::Metadata>,
) -> Result<(), Error> {
    #[cfg(unix)]
    {
        unix::cleanup(output, track, original)
    }
    #[cfg(not(unix))]
    {
        // Existing non-Unix behavior never confirmed raw destructive identity.
        // Preserve execution/archive publication and retain raw bytes honestly.
        if super::open_payload(output)?.is_none() {
            return Ok(());
        }
        let _ = (track, original);
        Err(Error::Io(
            "raw capture identity cannot be confirmed on this platform; retained".into(),
        ))
    }
}
