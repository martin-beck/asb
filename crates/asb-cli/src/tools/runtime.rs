//! Shared publication of pinned, project-local language runtime archives.
use crate::Error;
use crate::install::{
    InstallResult, OperationContext, ProducerDirectory, Purpose, install_source_bounded_in,
    verify_sha256_preserving as verify_sha256,
};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn unique_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{:x}-{:x}-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

pub(super) fn guest_archive(
    root: &Path,
    url: &'static str,
    checksum: &'static str,
) -> Vec<super::GuestAsset> {
    let name = url.rsplit('/').next().expect("runtime archive name");
    vec![super::GuestAsset {
        source: root.join("cache").join(name),
        target: Path::new("cache").join(name),
        distribution: Some((url, checksum)),
    }]
}

pub(crate) fn cached_archive_in(
    context: &OperationContext,
    destination: &Path,
    url: &str,
    checksum: &str,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> InstallResult<PathBuf> {
    let cache = destination.parent().expect("archive parent");
    fs::create_dir_all(cache).map_err(|error| Error::Io(error.to_string()))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(destination.with_extension("download-lock"))
        .map_err(|error| Error::Io(error.to_string()))?;
    loop {
        context.check_before_admission("runtime download cache lock", cache)?;
        match lock.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Err(fs::TryLockError::Error(error)) => {
                return Err(Error::Io(format!("cannot lock runtime download: {error}")).into());
            }
        }
    }
    if !destination.is_symlink()
        && destination.is_file()
        && verify_sha256(destination, checksum, "runtime archive").is_ok()
    {
        return Ok(destination.to_path_buf());
    }
    if fs::symlink_metadata(destination).is_ok() {
        fs::rename(
            destination,
            destination.with_extension(format!("backup-{}", unique_id())),
        )
        .map_err(|error| Error::Io(format!("cannot preserve damaged runtime archive: {error}")))?;
    }
    progress(15, crate::install::ProgressStage::Downloading);
    let temporary = destination.with_extension(format!("part-{}", unique_id()));
    install_source_bounded_in(
        context,
        url,
        &temporary,
        "runtime archive",
        256 * 1024 * 1024,
    )?;
    verify_sha256(&temporary, checksum, "runtime archive")?;
    fs::rename(&temporary, destination).map_err(|error| Error::Io(error.to_string()))?;
    Ok(destination.to_path_buf())
}

pub(super) fn archive(
    context: &OperationContext,
    root: &Path,
    url: &str,
    checksum: &str,
    programs: &[(&str, &str)],
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> InstallResult<PathBuf> {
    if !cfg!(target_os = "linux") {
        return Err(Error::Input(
            "project-local bootstrap runtimes currently support Linux".into(),
        )
        .into());
    }
    let filename = url.rsplit('/').next().expect("runtime archive name");
    let download = cached_archive_in(
        context,
        &root.join("cache").join(filename),
        url,
        checksum,
        progress,
    )?;
    let runtime = root.join("runtime");
    let receipt = root.join("ASB-RUNTIME");
    if fs::read_to_string(&receipt).ok().as_deref() != Some(checksum)
        || programs
            .iter()
            .any(|(_, path)| !runtime.join(path).is_file())
    {
        let stage = root.join(format!(".runtime-{}", unique_id()));
        fs::create_dir(&stage).map_err(|error| Error::Io(error.to_string()))?;
        let owner = ProducerDirectory::open(&stage)?;
        let mut scope = owner.scope(context);
        progress(50, crate::install::ProgressStage::Extracting);
        let mut step = scope.prepare("tar", Purpose::Extract, "language runtime extraction")?;
        step.command_mut()
            .args(["--extract", "--file"])
            .arg(&download)
            .arg("--directory")
            .arg(&stage)
            .args([
                "--strip-components=1",
                "--no-same-owner",
                "--no-same-permissions",
            ])
            .stdin(std::process::Stdio::null());
        let archive_bytes = fs::metadata(&download).ok().map(|metadata| metadata.len());
        let mut extraction =
            crate::install::transfer::Activity::new("Extracting language runtime", archive_bytes);
        extraction.update(0);
        let status = scope.run(step)?.status;
        if let Some(bytes) = archive_bytes {
            extraction.update(bytes);
        }
        extraction.finish();
        owner.consume(scope.finish()?)?;
        if !status.success() || programs.iter().any(|(_, path)| !stage.join(path).is_file()) {
            return Err(Error::Io(format!(
                "runtime archive extraction failed; retained evidence at {}",
                stage.display()
            ))
            .into());
        }
        if runtime.exists() {
            fs::rename(
                &runtime,
                root.join(format!("runtime.asb-backup-{}", unique_id())),
            )
            .map_err(|error| Error::Io(format!("cannot preserve previous runtime: {error}")))?;
        }
        fs::rename(&stage, &runtime).map_err(|error| Error::Io(error.to_string()))?;
        fs::write(&receipt, checksum).map_err(|error| Error::Io(error.to_string()))?;
    }
    let bin = root.join("bin");
    fs::create_dir_all(&bin).map_err(|error| Error::Io(error.to_string()))?;
    for (name, relative) in programs {
        let target = bin.join(name);
        let temporary = bin.join(format!(".{name}-{}", unique_id()));
        #[cfg(unix)]
        std::os::unix::fs::symlink(Path::new("../runtime").join(relative), &temporary)
            .map_err(|error| Error::Io(error.to_string()))?;
        #[cfg(not(unix))]
        fs::copy(runtime.join(relative), &temporary)
            .map_err(|error| Error::Io(error.to_string()))?;
        fs::rename(&temporary, &target).map_err(|error| {
            Error::Io(format!(
                "cannot publish runtime executable {}: {error}",
                target.display()
            ))
        })?;
    }
    progress(100, crate::install::ProgressStage::Ready);
    Ok(bin.join(programs[0].0))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Command;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("asb-runtime-{}", unique_id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn package(fixture: &Fixture, body: &str, filename: &str) -> (PathBuf, String) {
        use std::os::unix::fs::PermissionsExt;
        let source = fixture.0.join(format!("{filename}-source"));
        let program = source.join("distribution/bin/runtime");
        fs::create_dir_all(program.parent().unwrap()).unwrap();
        fs::write(&program, body).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let archive = fixture.0.join(filename);
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(&source)
                .arg("distribution")
                .status()
                .unwrap()
                .success()
        );
        let output = Command::new("sha256sum").arg(&archive).output().unwrap();
        assert!(output.status.success());
        let digest = String::from_utf8(output.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();
        (archive, digest)
    }

    #[test]
    fn cached_runtime_publication_is_offline_repeatable_and_preserves_previous_install() {
        let fixture = Fixture::new();
        let root = fixture.0.join("project-tool");
        let (first, hash) = package(&fixture, "#!/bin/sh\nprintf first\\n", "first.tar.gz");
        let programs = &[("runtime", "bin/runtime")];
        let context =
            OperationContext::new(&fixture.0, &crate::project::ProjectConfig::default()).unwrap();
        let mut stages = Vec::new();
        let executable = archive(
            &context,
            &root,
            first.to_str().unwrap(),
            &hash,
            programs,
            &mut |_, stage| stages.push(stage),
        )
        .unwrap();
        assert_eq!(
            stages,
            [
                crate::install::ProgressStage::Downloading,
                crate::install::ProgressStage::Extracting,
                crate::install::ProgressStage::Ready
            ]
        );
        assert_eq!(
            fs::read_to_string(&executable).unwrap(),
            "#!/bin/sh\nprintf first\\n"
        );
        fs::remove_file(&first).unwrap();
        // The primary source is now absent: a second install must use cache.
        assert_eq!(
            archive(
                &context,
                &root,
                first.to_str().unwrap(),
                &hash,
                programs,
                &mut |_, _| {}
            )
            .unwrap(),
            executable
        );
        assert!(!fs::read_dir(&root).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("runtime.asb-backup-")
        }));
        let (second, next_hash) = package(&fixture, "#!/bin/sh\nprintf second\\n", "second.tar.gz");
        archive(
            &context,
            &root,
            second.to_str().unwrap(),
            &next_hash,
            programs,
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(executable).unwrap(),
            "#!/bin/sh\nprintf second\\n"
        );
        let previous = fs::read_dir(&root)
            .unwrap()
            .find_map(|entry| {
                let entry = entry.unwrap();
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("runtime.asb-backup-")
                    .then(|| entry.path())
            })
            .unwrap();
        assert_eq!(
            fs::read_to_string(previous.join("bin/runtime")).unwrap(),
            "#!/bin/sh\nprintf first\\n"
        );
    }

    #[test]
    fn damaged_cached_download_is_preserved_and_replaced_before_extraction() {
        let fixture = Fixture::new();
        let (source, hash) = package(&fixture, "#!/bin/sh\nexit 0\n", "runtime.tar.gz");
        let root = fixture.0.join("project-tool");
        let cache = root.join("cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("runtime.tar.gz"), b"damaged evidence").unwrap();
        let context =
            OperationContext::new(&fixture.0, &crate::project::ProjectConfig::default()).unwrap();
        let executable = archive(
            &context,
            &root,
            source.to_str().unwrap(),
            &hash,
            &[("runtime", "bin/runtime")],
            &mut |_, _| {},
        )
        .unwrap();
        assert!(Command::new(executable).status().unwrap().success());
        let previous = fs::read_dir(&cache)
            .unwrap()
            .find_map(|entry| {
                let entry = entry.unwrap();
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("backup-")
                    .then(|| entry.path())
            })
            .unwrap();
        assert_eq!(fs::read(previous).unwrap(), b"damaged evidence");
    }
}
