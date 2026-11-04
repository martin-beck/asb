//! Acknowledged perf event activation before the measured process resumes.
#![allow(unsafe_code)]
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(super) struct Readiness {
    dir: PathBuf,
    control: File,
    ack: File,
}

impl Readiness {
    pub(super) fn new(parent: &Path, name: &str) -> io::Result<Self> {
        let dir = parent.join(format!(".perf-{name}-control"));
        // perf separates FIFO names on commas, so refuse ambiguous paths.
        if dir.as_os_str().as_bytes().contains(&b',') {
            return Err(io::Error::other("perf control path contains a comma"));
        }
        if dir.to_str().is_none() {
            return Err(io::Error::other("perf control path is not valid UTF-8"));
        }
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let create = || {
            for name in ["control", "ack"] {
                let path = dir.join(name);
                let path = CString::new(path.as_os_str().as_bytes())
                    .map_err(|_| io::Error::other("invalid perf control path"))?;
                // SAFETY: the path is NUL terminated and refers to a new private directory.
                if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            let open = |name| {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                    .open(dir.join(name))
            };
            Ok((open("control")?, open("ack")?))
        };
        match create() {
            Ok((control, ack)) => Ok(Self { dir, control, ack }),
            Err(error) => {
                let _ = fs::remove_file(dir.join("control"));
                let _ = fs::remove_file(dir.join("ack"));
                let _ = fs::remove_dir(&dir);
                Err(error)
            }
        }
    }

    pub(super) fn argument(&self) -> String {
        format!(
            "fifo:{},{}",
            self.dir.join("control").display(),
            self.dir.join("ack").display()
        )
    }

    pub(super) fn enable(
        &mut self,
        timeout: Duration,
        mut exited: impl FnMut() -> Result<Option<String>, String>,
    ) -> Result<(), String> {
        self.control
            .write_all(b"enable\n")
            .map_err(|e| format!("cannot request perf activation: {e}"))?;
        let deadline = Instant::now() + timeout;
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0; 32];
            match self.ack.read(&mut buffer) {
                Ok(0) => (),
                Ok(n) => {
                    bytes.extend_from_slice(&buffer[..n]);
                    if bytes.starts_with(b"ack\n") {
                        return Ok(());
                    }
                    if bytes.len() >= 32 || !b"ack\n".starts_with(&bytes) {
                        return Err("perf returned an invalid activation acknowledgement".into());
                    }
                }
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    return Err(format!(
                        "cannot read perf activation acknowledgement: {error}"
                    ));
                }
            }
            if let Some(reason) = exited()? {
                return Err(reason);
            }
            if Instant::now() >= deadline {
                return Err("perf did not acknowledge event activation before the deadline".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Readiness {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.dir.join("control"));
        let _ = fs::remove_file(self.dir.join("ack"));
        let _ = fs::remove_dir(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    fn root() -> PathBuf {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/perf-readiness-fixtures")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }
    #[test]
    fn accepts_ack_only_after_the_enable_request() {
        let root = root();
        let mut ready = Readiness::new(&root, "record").unwrap();
        let dir = ready.dir.clone();
        let thread = std::thread::spawn(move || {
            let mut control = File::open(dir.join("control")).unwrap();
            let mut message = [0; 7];
            control.read_exact(&mut message).unwrap();
            assert_eq!(&message, b"enable\n");
            OpenOptions::new()
                .write(true)
                .open(dir.join("ack"))
                .unwrap()
                .write_all(b"ack\n\0")
                .unwrap();
        });
        ready.enable(Duration::from_secs(2), || Ok(None)).unwrap();
        thread.join().unwrap();
        drop(ready);
        assert!(!root.join(".perf-record-control").exists());
        fs::remove_dir(root).unwrap();
    }
    #[test]
    fn rejects_missing_or_invalid_ack_and_preserves_existing_directory() {
        let root = root();
        let mut ready = Readiness::new(&root, "record").unwrap();
        assert!(Readiness::new(&root, "record").is_err());
        assert!(
            ready
                .enable(Duration::from_millis(15), || Ok(None))
                .unwrap_err()
                .contains("deadline")
        );
        drop(ready);
        let mut ready = Readiness::new(&root, "record").unwrap();
        OpenOptions::new()
            .write(true)
            .open(ready.dir.join("ack"))
            .unwrap()
            .write_all(b"bad\n")
            .unwrap();
        assert!(
            ready
                .enable(Duration::from_secs(1), || Ok(None))
                .unwrap_err()
                .contains("invalid")
        );
        drop(ready);
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn reports_early_exit_and_rejects_ambiguous_fifo_paths() {
        let root = root();
        let mut ready = Readiness::new(&root, "stat").unwrap();
        assert!(
            ready
                .enable(Duration::from_secs(1), || Ok(Some(
                    "collector exited".into()
                )))
                .unwrap_err()
                .contains("collector exited")
        );
        drop(ready);
        let ambiguous = root.join("parent,with-comma");
        fs::create_dir(&ambiguous).unwrap();
        assert!(Readiness::new(&ambiguous, "record").is_err());
        fs::remove_dir(ambiguous).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
