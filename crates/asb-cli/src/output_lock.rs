//! Private persistent-inode output ownership shared by sweeps and cassettes.
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

pub(crate) struct OutputLock {
    file: File,
    unlock_on_drop: bool,
}

impl Drop for OutputLock {
    fn drop(&mut self) {
        // Ordinary output ownership releases even if a fork inherited a handle.
        // Installer custody instead ends only when the last inherited handle closes.
        // Never unlink the persistent inode.
        if self.unlock_on_drop {
            let _ = self.file.unlock();
        }
    }
}

impl OutputLock {
    #[cfg(target_os = "linux")]
    pub(crate) fn raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.file.as_raw_fd()
    }
    pub(crate) fn acquire(path: &Path, busy: &str) -> Result<Self, String> {
        Self::acquire_for(path, busy, std::time::Duration::ZERO)
    }

    /// Keep a shared open-file-description lock until its last inherited handle closes.
    pub(crate) fn acquire_retained(path: &Path, busy: &str) -> Result<Self, String> {
        let mut lock = Self::open(path, busy, std::time::Duration::ZERO, true)?;
        lock.unlock_on_drop = false;
        Ok(lock)
    }

    /// Contention is a typed OS result, not a diagnostic-string predicate.
    pub(crate) fn acquire_for(
        path: &Path,
        busy: &str,
        timeout: std::time::Duration,
    ) -> Result<Self, String> {
        Self::open(path, busy, timeout, true)
    }

    /// Continuation must not recreate a missing historical ownership inode.
    pub(crate) fn acquire_existing(path: &Path, busy: &str) -> Result<Self, String> {
        Self::open(path, busy, std::time::Duration::ZERO, false)
    }

    fn open(
        path: &Path,
        busy: &str,
        timeout: std::time::Duration,
        create: bool,
    ) -> Result<Self, String> {
        let failure = |error| format!("cannot lock output at {}: {error}", path.display());
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(format!(
                    "output lock {} must be a regular file, not a symlink or directory",
                    path.display()
                ));
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(failure(error)),
        }
        let mut options = OpenOptions::new();
        options
            .create(create)
            .read(true)
            .write(true)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(path).map_err(failure)?;
        if !file.metadata().map_err(failure)?.is_file() {
            return Err(format!(
                "output lock {} is not a regular file",
                path.display()
            ));
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(Self {
                        file,
                        unlock_on_drop: true,
                    });
                }
                Err(TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(format!("{busy} (lock: {})", path.display()));
                }
                Err(TryLockError::Error(error)) => return Err(failure(error)),
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn cloned_descriptor(&self) -> File {
        self.file
            .try_clone()
            .expect("clone locked fixture descriptor")
    }
}
