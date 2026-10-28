//! Private quiet-custody staging; not confinement against a hostile same UID.
use super::*;
use sha2::{Digest, Sha256};
use std::io::{Seek, SeekFrom};

/// Keep the caller's admission failure typed and out of ordinary cleanup or
/// candidate-fallback handling. Only the core's own failures are core errors.
pub(super) enum PublicationFailure<E> {
    CorePublication(Error),
    Check(E),
}
impl<E> From<Error> for PublicationFailure<E> {
    fn from(error: Error) -> Self {
        Self::CorePublication(error)
    }
}

#[derive(Debug)]
pub(in crate::install) enum Input {
    Remote(String),
    Local(PathBuf),
    Pinned { path: PathBuf, handle: fs::File },
}
impl Input {
    pub(in crate::install) fn pin_local(path: PathBuf) -> Result<Self, Error> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let handle = options.open(&path).map_err(|error| {
            Error::Io(format!(
                "cannot prepare local source {}: {error}",
                path.display()
            ))
        })?;
        if !handle.metadata().map_err(crate::io_error)?.is_file() {
            return Err(Error::Input("prepared local source is not regular".into()));
        }
        Ok(Self::pinned(path, handle))
    }
    pub(in crate::install) fn pinned(path: PathBuf, handle: fs::File) -> Self {
        Self::Pinned { path, handle }
    }
    pub(in crate::install) fn from_os(value: &std::ffi::OsStr) -> Self {
        match value.to_str() {
            Some(text) if text.starts_with("https://") || text.starts_with("http://") => {
                Self::Remote(text.to_owned())
            }
            _ => Self::Local(PathBuf::from(value)),
        }
    }
}

pub(in crate::install) fn same_object(first: &fs::Metadata, second: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        first.dev() == second.dev() && first.ino() == second.ino()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Supplemental metadata consistency only. Publication/settlement also
        // compare live handles with the complete native FileIdInfo below.
        first.creation_time() == second.creation_time()
    }
    #[cfg(not(any(unix, windows)))]
    {
        first.created().ok().is_some() && first.created().ok() == second.created().ok()
    }
}

pub(in crate::install) struct Staging {
    pub(super) directory: PathBuf,
    pub(super) file: PathBuf,
    handle: fs::File,
    directory_identity: fs::Metadata,
    directory_handle: fs::File,
    settled: bool,
    published: bool,
    producer: crate::install::process::ProducerState,
    producer_quiet: std::sync::atomic::AtomicBool,
    monitor: std::sync::OnceLock<ProducerMonitor>,
    // Last field: test receipt follows the Drop body AND all resource fields.
    #[cfg(test)]
    drop_completion: DropCompletion,
}
#[cfg(test)]
struct DropCompletion(std::sync::Arc<std::sync::atomic::AtomicBool>);
#[cfg(test)]
impl Drop for DropCompletion {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}
struct ProducerMonitor {
    kind: String,
    bytes: u64,
}
impl Staging {
    pub(in crate::install) fn new(destination: &Path) -> Result<Self, Error> {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let directory = parent.join(format!(".asb-install-{}", tools::runtime::unique_id()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory).map_err(|error| {
            Error::Io(format!(
                "cannot stage installation beside {}: {error}",
                destination.display()
            ))
        })?;
        let created = (|| {
            let directory_identity = fs::symlink_metadata(&directory)?;
            let directory_handle = open_named(&directory)?;
            // Preserve argv[0] basename for multicall version probes.
            let file = directory.join(
                destination
                    .file_name()
                    .unwrap_or(std::ffi::OsStr::new("source")),
            );
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let created = options.open(&file)?;
            let initial = created.metadata()?;
            // A writable open description would make an actual ELF version
            // probe fail with ETXTBSY. Retain a read-only identity owner instead.
            let handle = fs::File::open(&file)?;
            if !same_object(&initial, &handle.metadata()?) || !same_files(&created, &handle)? {
                return Err(io::Error::other(
                    "private installation source changed while opening its identity owner",
                ));
            }
            drop(created);
            Ok(Self {
                directory: directory.clone(),
                file,
                handle,
                directory_identity,
                directory_handle,
                settled: false,
                published: false,
                producer: crate::install::process::ProducerState::default(),
                producer_quiet: std::sync::atomic::AtomicBool::new(true),
                monitor: std::sync::OnceLock::new(),
                #[cfg(test)]
                drop_completion: DropCompletion(std::sync::Arc::new(
                    std::sync::atomic::AtomicBool::new(false),
                )),
            })
        })();
        created.map_err(|error: io::Error| {
            Error::Io(format!(
                "cannot create private installation source: {error}; inspect retained staging {}",
                directory.display()
            ))
        })
    }

    fn directory_owned(&self) -> io::Result<()> {
        let now = fs::symlink_metadata(&self.directory)?;
        if !now.is_dir()
            || !same_object(&self.directory_identity, &now)
            || !same_named(&self.directory_handle, &self.directory)?
        {
            return Err(io::Error::other(
                "private installation directory identity changed",
            ));
        }
        Ok(())
    }
    pub(super) fn file_owned(&self) -> io::Result<()> {
        self.directory_owned()?;
        let named = fs::symlink_metadata(&self.file)?;
        if !named.is_file()
            || !same_object(&self.handle.metadata()?, &named)
            || !same_named(&self.handle, &self.file)?
        {
            return Err(io::Error::other(
                "private installation source identity changed",
            ));
        }
        Ok(())
    }
    pub(super) fn writer(&self) -> io::Result<fs::File> {
        self.file_owned()?;
        let mut options = OpenOptions::new();
        options.write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let writer = options.open(&self.file)?;
        if !same_object(&self.handle.metadata()?, &writer.metadata()?)
            || !same_files(&self.handle, &writer)?
        {
            return Err(io::Error::other(
                "staged installation writer identity changed",
            ));
        }
        self.file_owned()?;
        Ok(writer)
    }
    pub(super) fn make_executable(&self) -> Result<(), Error> {
        self.file_owned().map_err(crate::io_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = self
                .handle
                .metadata()
                .map_err(crate::io_error)?
                .permissions();
            permissions.set_mode(0o755);
            self.handle
                .set_permissions(permissions)
                .map_err(crate::io_error)?;
        }
        Ok(())
    }
    pub(super) fn snapshot(&self, options: &Options<'_>) -> Result<Snapshot, Error> {
        self.file_owned().map_err(crate::io_error)?;
        let snapshot = Snapshot::read(&self.handle, options)?;
        self.file_owned().map_err(crate::io_error)?;
        Ok(snapshot)
    }
    pub(super) fn synchronize(&self) -> Result<(), Error> {
        self.file_owned().map_err(crate::io_error)?;
        checkpoint(FaultPoint::Sync, &self.file)?;
        // Unix fsync accepts the admitted read-only descriptor: reopening for
        // write would reject valid readonly workload files. No writable open
        // description survives into version exec (ETXTBSY on Unix).
        #[cfg(unix)]
        let result = self.handle.sync_all();
        // Windows FlushFileBuffers requires write access.
        #[cfg(not(unix))]
        let result = self.writer().map_err(crate::io_error)?.sync_all();
        result.map_err(|error| Error::Io(format!("cannot synchronize staged source: {error}")))
    }
    pub(super) fn published(&mut self) {
        self.published = true;
    }
    #[cfg(test)]
    pub(super) fn publish_copy(
        &mut self,
        destination: &Path,
        options: &Options<'_>,
        validated: &Snapshot,
        publication: Publication,
    ) -> Result<(), Error> {
        match self.publish_copy_checked(destination, options, validated, publication, || {
            Ok::<(), std::convert::Infallible>(())
        }) {
            Ok(()) => Ok(()),
            Err(PublicationFailure::CorePublication(error)) => Err(error),
            Err(PublicationFailure::Check(never)) => match never {},
        }
    }
    pub(super) fn publish_copy_checked<E>(
        &mut self,
        destination: &Path,
        options: &Options<'_>,
        validated: &Snapshot,
        publication: Publication,
        check: impl FnOnce() -> Result<(), E>,
    ) -> Result<(), PublicationFailure<E>> {
        self.require_quiet_producer().map_err(crate::io_error)?;
        // The probe knows the original pathname (and may retain its directory
        // handle). Move that entry out before trusting a commit pathname.
        let quiet = Quarantined::take(
            &self.file,
            &self.handle,
            &self.directory_handle,
            FaultPoint::CommitTake,
        )
        .map_err(crate::io_error)?;
        self.published = true; // The original entry was detached, not a receipt.
        let result = (|| {
            let detached = Snapshot::read(&self.handle, options)?;
            if !validated.same_payload(&detached) {
                return Err(Error::Input(
                    "validated installation bytes changed during detachment".into(),
                )
                .into());
            }
            // Never publish from any pathname supplied to a downloader/probe.
            // Copy the admitted handle into a NEW quiet namespace; a retained
            // descriptor for the exposed source directory cannot alter it.
            let mut commit = Self::new(destination)?;
            let copied = (|| {
                let mut input = self.handle.try_clone().map_err(crate::io_error)?;
                input.seek(SeekFrom::Start(0)).map_err(crate::io_error)?;
                let mut output = commit.writer().map_err(crate::io_error)?;
                let count = io::copy(
                    &mut input.take(options.limit.saturating_add(1)),
                    &mut output,
                )
                .map_err(crate::io_error)?;
                checkpoint(FaultPoint::CommitCopy, &self.file)?;
                if count != detached.metadata.len() {
                    return Err(Error::Input(
                        "validated installation changed during commit copying".into(),
                    )
                    .into());
                }
                output
                    .set_permissions(detached.metadata.permissions())
                    .map_err(crate::io_error)?;
                checkpoint(FaultPoint::CommitSync, &self.file)?;
                output.sync_all().map_err(crate::io_error)?;
                drop(output);
                if !detached.matches(&Snapshot::read(&self.handle, options)?)
                    || !validated.same_payload(&commit.snapshot(options)?)
                {
                    return Err(Error::Input(
                        "validated installation changed before publication".into(),
                    )
                    .into());
                }
                // Fault hooks deliberately receive only the old exposed name,
                // never the private commit namespace.
                checkpoint(FaultPoint::CommitRename, &self.file)?;
                // Admission is checked after the last fault hook and directly
                // before BOTH commit variants. This does not make the clock
                // check and the following filesystem syscall atomic.
                if let Err(error) = check() {
                    self.retain_producer_scratch();
                    commit.retain_producer_scratch();
                    return Err(PublicationFailure::Check(error));
                }
                match publication {
                    Publication::Replace => fs::rename(&commit.file, destination),
                    Publication::NoClobber => match fs::hard_link(&commit.file, destination) {
                        Err(error)
                            if error.kind() == io::ErrorKind::AlreadyExists
                                && (if options.executable {
                                    crate::executables::is_executable(destination)
                                } else {
                                    destination.is_file()
                                }) =>
                        {
                            Ok(())
                        }
                        result => result,
                    },
                }
                .map_err(|error| {
                    Error::Io(format!(
                        "cannot install {} at {}: {error}",
                        options.kind,
                        destination.display()
                    ))
                })?;
                commit.published();
                Ok(())
            })();
            match copied {
                Ok(()) => commit.finish(Ok(())).map_err(Into::into),
                Err(PublicationFailure::CorePublication(error)) => {
                    commit.finish(Err(error)).map_err(Into::into)
                }
                Err(PublicationFailure::Check(error)) => Err(PublicationFailure::Check(error)),
            }
        })();
        match result {
            Ok(()) => quiet.finish(Ok(())).map_err(Into::into),
            Err(PublicationFailure::CorePublication(error)) => {
                quiet.finish(Err(error)).map_err(Into::into)
            }
            // Quarantined has no destructive Drop. Both actual staging owners
            // above were disarmed before this typed rejection escapes.
            Err(PublicationFailure::Check(error)) => Err(PublicationFailure::Check(error)),
        }
    }
    fn settle(&self) -> io::Result<()> {
        self.require_quiet_producer()?;
        checkpoint(FaultPoint::Cleanup, &self.file)
            .map_err(|error| io::Error::other(error.to_string()))?;
        self.directory_owned()?;
        match fs::symlink_metadata(&self.file) {
            Ok(_) => {
                self.file_owned()?;
                Quarantined::take(
                    &self.file,
                    &self.handle,
                    &self.directory_handle,
                    FaultPoint::CleanupTake,
                )?
                .finish(Ok(()))
                .map_err(|error| io::Error::other(error.to_string()))?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && self.published => {}
            Err(error) => return Err(error),
        }
        // The directory pathname was exposed too. Detach and verify it before
        // even a nonrecursive removal, so an empty foreign directory survives.
        settle_directory(
            &self.directory,
            &self.directory_handle,
            FaultPoint::DirectoryTake,
        )
    }
    pub(super) fn finish<T>(mut self, result: Result<T, Error>) -> Result<T, Error> {
        self.settled = true; // A failed explicit settlement is never retried in Drop.
        match (result, self.settle()) {
            (result, Ok(())) => result,
            (result, Err(error)) => Err(Error::Io(format!(
                "{}; installation scratch cleanup unconfirmed: {error}; inspect retained staging {}",
                result.err().map_or_else(
                    || "installation publication completed".to_owned(),
                    |error| error.to_string()
                ),
                self.directory.display()
            ))),
        }
    }
}

/// The exposed entry is atomically detached into a fresh namespace which has
/// never been supplied to a child/callback. Only positively owned entries are
/// deleted there. This is ordinary-concurrency custody, not a same-UID sandbox.
pub(in crate::install) struct Quarantined {
    directory: PathBuf,
    file: PathBuf,
}

pub(in crate::install) fn settle_directory(
    exposed: &Path,
    identity: &fs::File,
    point: FaultPoint,
) -> io::Result<()> {
    if !same_named(identity, exposed)? {
        return Err(io::Error::other("scratch directory identity changed"));
    }
    let parent = exposed
        .parent()
        .ok_or_else(|| io::Error::other("scratch has no parent"))?;
    let directory = parent.join(format!(
        ".asb-install-settlement-{}",
        tools::runtime::unique_id()
    ));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&directory)?;
    let quiet = directory.join("entry");
    let result = (|| {
        checkpoint(point, exposed).map_err(|error| io::Error::other(error.to_string()))?;
        fs::rename(exposed, &quiet)?;
        if !same_named(identity, &quiet)? {
            // Directories cannot be restored with a no-clobber hard link.
            // Retain the whole detached arrival instead of guessing that the
            // original name is still vacant or overwriting another creator.
            return Err(io::Error::other(
                "scratch directory changed during detachment; automatic directory recovery refused",
            ));
        }
        fs::remove_dir(&quiet) // Never recurse over unknown arrivals.
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir(&directory); // Only succeeds if nothing moved.
        return Err(io::Error::other(format!(
            "{error}; inspect retained staging {} (original {})",
            directory.display(),
            exposed.display()
        )));
    }
    fs::remove_dir(&directory)
}
impl Quarantined {
    pub(in crate::install) fn take(
        exposed: &Path,
        identity: &fs::File,
        exposed_directory: &fs::File,
        point: FaultPoint,
    ) -> io::Result<Self> {
        let parent = exposed
            .parent()
            .ok_or_else(|| io::Error::other("source has no parent"))?;
        let directory = parent.parent().unwrap_or(parent).join(format!(
            ".asb-install-settlement-{}",
            tools::runtime::unique_id()
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory)?;
        let file = directory.join("entry");
        let moved = (|| {
            checkpoint(point, exposed).map_err(|error| io::Error::other(error.to_string()))?;
            // The destination slot is absent in a newly created private directory.
            fs::rename(exposed, &file)?;
            if !same_named(exposed_directory, parent).unwrap_or(false)
                || !same_named(identity, &file).unwrap_or(false)
            {
                // Recovery NEVER replaces another arrival. Keep the quarantine
                // link even after successful recovery: it is foreign evidence.
                checkpoint(FaultPoint::Recovery, exposed)
                    .map_err(|error| io::Error::other(error.to_string()))?;
                let recovery = if same_named(exposed_directory, parent).unwrap_or(false) {
                    fs::hard_link(&file, exposed)
                } else {
                    Err(io::Error::other("source directory identity changed"))
                };
                return Err(io::Error::other(format!(
                    "source identity changed during detachment; no-clobber recovery: {}; inspect retained staging {} and original {}",
                    recovery.map_or_else(
                        |error| error.to_string(),
                        |()| "restored original name (foreign quarantine retained)".into()
                    ),
                    directory.display(),
                    exposed.display(),
                )));
            }
            Ok(())
        })();
        if let Err(error) = moved {
            // Nonrecursive removal succeeds only when no entry was moved.
            let _ = fs::remove_dir(&directory);
            return Err(io::Error::other(format!(
                "{error}; inspect retained staging {}",
                directory.display()
            )));
        }
        Ok(Self { directory, file })
    }
    pub(in crate::install) fn finish<T>(self, result: Result<T, Error>) -> Result<T, Error> {
        let cleanup = fs::remove_file(&self.file).and_then(|()| fs::remove_dir(&self.directory));
        match cleanup {
            Ok(()) => result,
            Err(error) => Err(Error::Io(format!(
                "{}; quiet scratch settlement failed: {error}; inspect retained staging {}",
                result.err().map_or_else(
                    || "installation publication completed".into(),
                    |error| error.to_string()
                ),
                self.directory.display()
            ))),
        }
    }
}

pub(in crate::install) fn open_named(path: &Path) -> io::Result<fs::File> {
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
        // FILE_FLAG_BACKUP_SEMANTICS permits directory identity ownership;
        // FILE_FLAG_OPEN_REPARSE_POINT does not traverse a replaced link.
        options.custom_flags(0x0200_0000 | 0x0020_0000);
    }
    options.open(path)
}
pub(in crate::install) fn same_named(original: &fs::File, path: &Path) -> io::Result<bool> {
    same_files(original, &open_named(path)?)
}
fn same_files(first: &fs::File, second: &fs::File) -> io::Result<bool> {
    #[cfg(not(windows))]
    {
        Ok(same_object(&first.metadata()?, &second.metadata()?))
    }
    #[cfg(windows)]
    {
        Ok(windows_identity(first)? == windows_identity(second)?)
    }
}
#[cfg(windows)]
#[allow(unsafe_code)]
fn windows_identity(file: &fs::File) -> io::Result<(u64, [u8; 16])> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    struct Information {
        volume: u64,
        id: [u8; 16],
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut std::ffi::c_void,
            class: i32,
            information: *mut std::ffi::c_void,
            length: u32,
        ) -> i32;
    }
    let mut information = std::mem::MaybeUninit::<Information>::uninit();
    // SAFETY: borrowed live File, FileIdInfo=18 and exact ABI layout. Keep all
    // 128 ID bits; failure cannot manufacture a positive identity proof.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            18,
            information.as_mut_ptr().cast(),
            std::mem::size_of::<Information>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let information = unsafe { information.assume_init() };
    Ok((information.volume, information.id))
}
impl Drop for Staging {
    fn drop(&mut self) {
        if !self.settled {
            self.settled = true;
            let _ = self.settle();
        }
    }
}

impl crate::install::process::ProducerOwner for Staging {
    #[cfg(test)]
    fn drop_completion(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        Some(std::sync::Arc::clone(&self.drop_completion.0))
    }
    fn producer_state(&self) -> &crate::install::process::ProducerState {
        &self.producer
    }
    fn producer_started(&self) {
        self.producer_quiet
            .store(false, std::sync::atomic::Ordering::Release);
    }
    fn observation(
        self: std::sync::Arc<Self>,
    ) -> Option<crate::install::process::ObservationCheck> {
        self.monitor.get()?;
        Some(Box::new(move || {
            let policy = self.monitor.get().expect("fixed source observation policy");
            self.file_owned().map_err(|error| error.to_string())?;
            let metadata = self.handle.metadata().map_err(|error| {
                format!(
                    "cannot inspect staged {} at {}: {error}",
                    policy.kind,
                    self.file.display()
                )
            })?;
            if !metadata.is_file() {
                return Err(format!(
                    "staged {} at {} is not a regular file",
                    policy.kind,
                    self.file.display()
                ));
            }
            if metadata.len() > policy.bytes {
                return Err(format!(
                    "staged {} at {} exceeds the installation size limit ({} bytes); raw evidence was retained",
                    policy.kind,
                    self.file.display(),
                    policy.bytes
                ));
            }
            Ok(())
        }))
    }
}

impl Staging {
    pub(in crate::install) fn monitor_producer(&self, kind: &str, bytes: u64) -> Result<(), Error> {
        self.monitor
            .set(ProducerMonitor {
                kind: kind.to_owned(),
                bytes,
            })
            .map_err(|_| {
                Error::Input(
                    "source observation policy was already fixed for this staging owner".into(),
                )
            })
    }
    fn require_quiet_producer(&self) -> io::Result<()> {
        if self
            .producer_quiet
            .load(std::sync::atomic::Ordering::Acquire)
        {
            Ok(())
        } else {
            Err(io::Error::other(
                "installation producer settlement is unavailable; private staging is retained",
            ))
        }
    }
    pub(in crate::install) fn producer_scope(
        self: &std::sync::Arc<Self>,
        context: &crate::install::process::OperationContext,
    ) -> crate::install::process::ProcessScope {
        let lease = crate::install::process::ProducerLease::new(
            std::sync::Arc::clone(self),
            &self.directory,
        );
        crate::install::process::ProcessScope::new(context, lease)
    }
    pub(in crate::install) fn accept_producer_permission(
        &self,
        permission: crate::install::process::ProducerPermission,
    ) -> crate::install::process::InstallResult<crate::install::process::ConsumedPermission> {
        self.producer_quiet
            .store(false, std::sync::atomic::Ordering::Release);
        let guard = permission.consume_guard_for(&self.producer)?;
        self.producer_quiet
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(guard)
    }
    pub(in crate::install) fn retain_producer_scratch(&self) {
        self.producer.invalidate_settlement();
        self.producer_quiet
            .store(false, std::sync::atomic::Ordering::Release);
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(in crate::install) fn producer_path(&self) -> &Path {
        &self.file
    }
    #[cfg(all(test, target_os = "linux"))]
    pub(in crate::install) fn publish_with_permission_for_test(
        mut self,
        permission: crate::install::process::ProducerPermission,
        destination: &Path,
    ) -> crate::install::process::InstallResult<()> {
        self.accept_producer_permission(permission)?;
        let options = Options {
            kind: "producer custody fixture",
            limit: 1024 * 1024,
            checksum: None,
            executable: false,
        };
        let snapshot = self.snapshot(&options)?;
        let result = self.publish_copy(destination, &options, &snapshot, Publication::Replace);
        self.finish(result).map_err(Into::into)
    }
}

pub(super) struct Snapshot {
    metadata: fs::Metadata,
    digest: [u8; 32],
}
impl Snapshot {
    fn same_payload(&self, other: &Self) -> bool {
        self.metadata.len() == other.metadata.len()
            && self.metadata.permissions() == other.metadata.permissions()
            && self.digest == other.digest
    }
    fn read(handle: &fs::File, options: &Options<'_>) -> Result<Self, Error> {
        let metadata = handle.metadata().map_err(crate::io_error)?;
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
            return Err(Error::Input(format!("staged {} is empty", options.kind)));
        }
        #[cfg(unix)]
        if options.executable {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                return Err(Error::Input(
                    "staged executable lost executable permissions".into(),
                ));
            }
        }
        let mut reader = handle.try_clone().map_err(crate::io_error)?;
        reader.seek(SeekFrom::Start(0)).map_err(crate::io_error)?;
        let mut digest = Sha256::new();
        let mut total = 0u64;
        let mut progress = super::super::transfer::Activity::new(
            format!("Validating {}", options.kind),
            Some(metadata.len()),
        );
        let mut buffer = [0; 64 * 1024];
        loop {
            let count = reader.read(&mut buffer).map_err(crate::io_error)?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(count as u64)
                .ok_or_else(|| size_error(options.kind, options.limit))?;
            if total > options.limit {
                return Err(size_error(options.kind, options.limit));
            }
            digest.update(&buffer[..count]);
            progress.update(total);
        }
        progress.finish();
        if total != metadata.len()
            || !unchanged_metadata(&metadata, &handle.metadata().map_err(crate::io_error)?)
        {
            return Err(Error::Input(
                "installation source changed during bounded validation".into(),
            ));
        }
        let snapshot = Self {
            metadata,
            digest: digest.finalize().into(),
        };
        if let Some(expected) = options.checksum {
            let actual = snapshot
                .digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            if actual != expected {
                return Err(Error::Io(format!(
                    "{} checksum mismatch: expected {expected}, got {actual}",
                    options.kind
                )));
            }
        }
        Ok(snapshot)
    }
    pub(super) fn matches(&self, other: &Self) -> bool {
        same_object(&self.metadata, &other.metadata)
            && unchanged_metadata(&self.metadata, &other.metadata)
            && self.digest == other.digest
    }
}
pub(in crate::install) fn verify_pinned_checksum(
    handle: &fs::File,
    path: &Path,
    expected: &str,
    kind: &str,
    limit: u64,
) -> Result<(), Error> {
    if !same_named(handle, path).map_err(crate::io_error)? {
        return Err(Error::Input(format!("{kind} identity changed")));
    }
    Snapshot::read(
        handle,
        &Options {
            kind,
            limit,
            checksum: Some(expected),
            executable: false,
        },
    )?;
    if !same_named(handle, path).map_err(crate::io_error)? {
        return Err(Error::Input(format!("{kind} identity changed")));
    }
    Ok(())
}
pub(super) fn unchanged_metadata(first: &fs::Metadata, second: &fs::Metadata) -> bool {
    if first.len() != second.len()
        || first.modified().ok() != second.modified().ok()
        || first.permissions() != second.permissions()
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if first.ctime() != second.ctime() || first.ctime_nsec() != second.ctime_nsec() {
            return false;
        }
    }
    true
}
