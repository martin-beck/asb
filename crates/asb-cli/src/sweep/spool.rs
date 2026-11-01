//! Private bounded command output, drained one configuration at a time.
use crate::{Error, benchmark};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

pub(super) struct Output {
    file: File,
    remaining: u64,
}

fn limit(replicas: usize) -> u64 {
    replicas as u64 * 4 * 1024 * 1024 + 65536
}

impl Output {
    pub(super) fn create(path: &Path, replicas: usize) -> Result<Self, Error> {
        Self::create_bounded(path, replicas, benchmark::MAX_CONCURRENCY)
    }

    pub(super) fn create_campaign(path: &Path, executions: usize) -> Result<Self, Error> {
        Self::create_bounded(path, executions, 65_533)
    }

    fn create_bounded(path: &Path, replicas: usize, maximum: usize) -> Result<Self, Error> {
        if !(1..=maximum).contains(&replicas) {
            return Err(Error::Input("invalid output spool replica count".into()));
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(Self {
            file: options.open(path).map_err(|error| {
                Error::Io(format!(
                    "cannot create private sweep output {}: {error}",
                    path.display()
                ))
            })?,
            remaining: limit(replicas),
        })
    }
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(io::Error::other(
                "sweep command output exceeded its per-attempt limit; inspect the agent log instead of --show-output",
            ));
        }
        let written = self.file.write(bytes)?;
        self.remaining -= written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

pub(super) fn drain(path: &Path, replicas: usize, output: &mut dyn Write) -> Result<(), Error> {
    drain_bounded(path, replicas, benchmark::MAX_CONCURRENCY, output)
}

pub(super) fn drain_campaign(
    path: &Path,
    executions: usize,
    output: &mut dyn Write,
) -> Result<(), Error> {
    drain_bounded(path, executions, 65_533, output)
}

fn drain_bounded(
    path: &Path,
    replicas: usize,
    maximum: usize,
    output: &mut dyn Write,
) -> Result<(), Error> {
    if !(1..=maximum).contains(&replicas) {
        return Err(Error::Input("invalid output spool replica count".into()));
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(crate::io_error(error)),
    };
    if !metadata.is_file() || metadata.len() > limit(replicas) {
        return Err(Error::Io(
            "private sweep output is not a bounded regular file".into(),
        ));
    }
    let mut file = OpenOptions::new();
    file.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        file.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let result = file.open(path).and_then(|file| {
        let copied = io::copy(&mut file.take(limit(replicas) + 1), output)?;
        if copied > limit(replicas) {
            Err(io::Error::other(
                "private sweep output grew beyond its bound",
            ))
        } else {
            Ok(())
        }
    });
    let cleanup = std::fs::remove_file(path);
    result.and(cleanup).map_err(crate::io_error)
}

pub(super) fn discard(path: &Path) -> Result<(), Error> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(crate::io_error(error)),
    }
}

/// Preserve interrupted presentation bytes after continuation validation.
/// These files are not grading evidence and must never overwrite a new spool.
pub(super) fn preserve(path: &Path, replicas: usize, history: &Path) -> Result<(), Error> {
    preserve_bounded(path, replicas, benchmark::MAX_CONCURRENCY, history)
}

/// A repeated campaign retains one presentation stream across all executions.
pub(super) fn preserve_campaign(
    path: &Path,
    executions: usize,
    history: &Path,
) -> Result<(), Error> {
    preserve_bounded(path, executions, 65_533, history)
}

fn preserve_bounded(
    path: &Path,
    replicas: usize,
    maximum: usize,
    history: &Path,
) -> Result<(), Error> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(crate::io_error(error)),
    };
    if !(1..=maximum).contains(&replicas) || !metadata.is_file() || metadata.len() > limit(replicas)
    {
        return Err(Error::Input("interrupted terminal output is not a bounded regular file; preserve it before resuming this sweep".into()));
    }
    if std::fs::symlink_metadata(history).is_ok_and(|metadata| !metadata.is_dir()) {
        return Err(Error::Input(
            "interrupted-output history must be a real directory".into(),
        ));
    }
    let mut directory = std::fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(history).map_err(crate::io_error)?;
    let target = history.join(format!(
        "{}.output",
        crate::workloads::TaskIdentity::local("interrupted-terminal", None).attempt_id
    ));
    std::fs::hard_link(path, &target).map_err(crate::io_error)?;
    std::fs::File::open(history)
        .and_then(|file| file.sync_all())
        .map_err(crate::io_error)?;
    std::fs::remove_file(path).map_err(crate::io_error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(crate::workloads::TaskIdentity::local("spool", None).attempt_id);
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn campaign_presentation_counts_every_execution_without_changing_legacy_replica_bounds() {
        let root = Directory::new();
        let path = root.0.join("campaign");
        assert!(Output::create(&path, 65).is_err());
        assert!(!path.exists());
        assert!(Output::create_campaign(&path, 65_534).is_err());
        let mut output = Output::create_campaign(&path, 65).unwrap();
        assert_eq!(output.remaining, limit(65));
        output.write_all(b"one campaign block\n").unwrap();
        output.flush().unwrap();
        drop(output);
        let mut bytes = Vec::new();
        assert!(drain(&path, 65, &mut bytes).is_err());
        assert!(path.exists());
        drain_campaign(&path, 65, &mut bytes).unwrap();
        assert_eq!(bytes, b"one campaign block\n");
        assert!(!path.exists());
    }

    #[test]
    fn interrupted_output_is_preserved_without_overwriting_new_output_or_following_symlinks() {
        let root = Directory::new();
        let source = root.0.join("output");
        let history = root.0.join("history");
        let mut spool = Output::create(&source, 1).unwrap();
        spool.write_all(b"previous presentation bytes").unwrap();
        spool.flush().unwrap();
        drop(spool);
        preserve(&source, 1, &history).unwrap();
        assert!(!source.exists());
        let files: Vec<_> = std::fs::read_dir(&history)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(
            std::fs::read(&files[0]).unwrap(),
            b"previous presentation bytes"
        );
        Output::create(&source, 1).unwrap();
        assert_eq!(
            std::fs::read(&files[0]).unwrap(),
            b"previous presentation bytes"
        );
        #[cfg(unix)]
        {
            std::fs::remove_file(&source).unwrap();
            std::os::unix::fs::symlink(&files[0], &source).unwrap();
            assert!(preserve(&source, 1, &history).is_err());
            assert_eq!(
                std::fs::read(&files[0]).unwrap(),
                b"previous presentation bytes"
            );
        }
    }

    #[test]
    fn interrupted_repeated_campaign_preserves_its_full_output() {
        let root = Directory::new();
        let source = root.0.join("output");
        let history = root.0.join("history");
        let bytes = vec![b'x'; limit(1) as usize + 1];
        let mut spool = Output::create_campaign(&source, 2).unwrap();
        spool.write_all(&bytes).unwrap();
        drop(spool);
        // The original continuation path passed only concurrency here.
        assert!(preserve(&source, 1, &history).is_err());
        preserve_campaign(&source, 2, &history).unwrap();
        let saved = std::fs::read_dir(history)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(std::fs::read(saved).unwrap(), bytes);
        assert!(!source.exists());
    }

    #[test]
    fn campaign_preservation_keeps_legacy_and_population_bounds() {
        let root = Directory::new();
        let source = root.0.join("output");
        let history = root.0.join("history");
        std::fs::write(&source, b"campaign output").unwrap();
        assert!(preserve(&source, 65, &history).is_err());
        assert!(preserve_campaign(&source, 65_534, &history).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"campaign output");
        preserve_campaign(&source, 65, &history).unwrap();
        assert!(!source.exists());
    }

    #[test]
    fn output_is_private_bounded_non_overwriting_and_drained_once() {
        let root = Directory::new();
        let path = root.0.join("output");
        let mut spool = Output::create(&path, 1).unwrap();
        spool.remaining = 3;
        spool.write_all(b"abc").unwrap();
        assert!(spool.write_all(b"more").is_err());
        spool.flush().unwrap();
        drop(spool);
        assert!(Output::create(&path, 1).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let mut output = Vec::new();
        drain(&path, 1, &mut output).unwrap();
        assert_eq!(output, b"abc");
        assert!(!path.exists());
        drain(&path, 1, &mut output).unwrap();
        assert_eq!(output, b"abc");
    }

    #[test]
    fn broken_terminal_and_discard_remove_only_the_temporary_duplicate() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("broken output fixture"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let root = Directory::new();
        let path = root.0.join("output");
        std::fs::write(root.0.join("agent.log"), "retained").unwrap();
        Output::create(&path, 1)
            .unwrap()
            .write_all(b"result")
            .unwrap();
        assert!(drain(&path, 1, &mut Broken).is_err());
        assert!(!path.exists());
        Output::create(&path, 1).unwrap();
        discard(&path).unwrap();
        discard(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.0.join("agent.log")).unwrap(),
            "retained"
        );
        assert!(Output::create(&path, 0).is_err());
        assert!(Output::create(&path, 65).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn draining_does_not_follow_links_or_read_oversized_files() {
        let root = Directory::new();
        let target = root.0.join("unrelated");
        std::fs::write(&target, "private").unwrap();
        let link = root.0.join("linked");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(drain(&link, 1, &mut Vec::new()).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "private");
        let path = root.0.join("oversized");
        let file = File::create(&path).unwrap();
        file.set_len(limit(1) + 1).unwrap();
        assert!(drain(&path, 1, &mut Vec::new()).is_err());
        assert!(
            path.exists(),
            "retain unexpected evidence instead of following/deleting it"
        );
    }
}
