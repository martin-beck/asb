//! Pinned archive preparation and explicit, nonrecursive extraction settlement.
use super::*;

/// Adapter-owned policy for a pinned archive containing one executable.
pub(crate) struct ArchiveSpec {
    pub name: &'static str,
    pub executable: &'static str,
    pub max_bytes: u64,
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use sha2::Digest;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn adapter_policy_selects_archive_entry_destination_and_name() {
        let root = std::env::temp_dir().join(format!(
            "asb-archive-policy-{}",
            tools::runtime::unique_id()
        ));
        let payload = root.join("payload");
        let install = root.join("install");
        fs::create_dir_all(&payload).unwrap();
        fs::create_dir_all(&install).unwrap();
        let executable = payload.join("example-agent");
        fs::write(&executable, b"#!/bin/sh\necho example-agent version 1\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let archive = root.join("example.tar.gz");
        assert!(
            std::process::Command::new("tar")
                .args(["-czf"])
                .arg(&archive)
                .arg("-C")
                .arg(&payload)
                .arg(".")
                .status()
                .unwrap()
                .success()
        );
        let checksum = format!("{:x}", sha2::Sha256::digest(fs::read(&archive).unwrap()));
        let spec = ArchiveSpec {
            name: "Example",
            executable: "example-agent",
            max_bytes: 1024 * 1024,
        };
        let result = install_archive(&install, &archive, &checksum, &mut |_, _| {}, &spec).unwrap();
        assert_eq!(result, install.join("bin/example-agent"));
        assert!(result.is_file());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
pub(super) fn install_archive(
    root: &Path,
    archive: &Path,
    checksum: &str,
    progress: &mut dyn FnMut(u8, ProgressStage),
    spec: &ArchiveSpec,
) -> Result<PathBuf, Error> {
    let context = OperationContext::new(root, &project::ProjectConfig::default())
        .map_err(InstallFailure::into_public)?;
    install_archive_in(&context, root, archive, checksum, progress, spec)
        .map_err(InstallFailure::into_public)
}

pub(crate) fn install_archive_in(
    context: &OperationContext,
    root: &Path,
    archive: &Path,
    checksum: &str,
    progress: &mut dyn FnMut(u8, ProgressStage),
    spec: &ArchiveSpec,
) -> InstallResult<PathBuf> {
    let metadata = fs::symlink_metadata(archive).map_err(crate::io_error)?;
    if !metadata.is_file() || metadata.len() > spec.max_bytes {
        return Err(Error::Input(format!(
            "pinned {} archive is not a bounded regular file",
            spec.name
        ))
        .into());
    }
    let archive_handle = source::open_named(archive).map_err(crate::io_error)?;
    if !source::same_object(
        &metadata,
        &archive_handle.metadata().map_err(crate::io_error)?,
    ) || !source::same_named(&archive_handle, archive).map_err(crate::io_error)?
    {
        return Err(Error::Input(format!(
            "pinned {} archive identity changed before extraction",
            spec.name
        ))
        .into());
    }
    source::verify_pinned_checksum(
        &archive_handle,
        archive,
        checksum,
        &format!("{} archive", spec.name),
        spec.max_bytes,
    )?;
    let extract = root.join(format!("extract-{}", tools::runtime::unique_id()));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&extract).map_err(|error| {
        Error::Io(format!(
            "cannot create {} extraction directory: {error}",
            spec.name
        ))
    })?;
    let directory_identity = fs::symlink_metadata(&extract).map_err(crate::io_error)?;
    let directory_handle = source::open_named(&extract).map_err(crate::io_error)?;
    let owner = ProducerDirectory::open(&extract)?;
    let mut scope = owner.scope(context);
    // Before a complete extraction is positively inspected, no child-created
    // entry is ours to unlink. Errors retain the entire scratch, without Drop.
    let prepared = (|| -> InstallResult<_> {
        progress(60, ProgressStage::Extracting);
        let mut step = scope.prepare(
            "tar",
            Purpose::Extract,
            &format!("{} extraction", spec.name),
        )?;
        step.command_mut()
            .args(["--extract", "--gzip", "--file"])
            .arg(archive)
            .arg("--directory")
            .arg(&extract)
            .stdin(Stdio::null());
        let mut extraction = crate::install::transfer::Activity::new(
            format!("Extracting {} archive", spec.name),
            Some(metadata.len()),
        );
        extraction.update(0);
        let status = scope.run(step)?.status;
        extraction.update(metadata.len());
        extraction.finish();
        owner.consume(scope.finish()?)?;
        if !status.success() {
            return Err(
                Error::Io(format!("could not extract the pinned {} binary", spec.name)).into(),
            );
        }
        let current = fs::symlink_metadata(archive).map_err(crate::io_error)?;
        if !source::same_named(&archive_handle, archive).map_err(crate::io_error)?
            || !current.is_file()
            || !source::same_object(&metadata, &current)
            || !source::same_object(
                &metadata,
                &archive_handle.metadata().map_err(crate::io_error)?,
            )
        {
            return Err(Error::Input(format!(
                "pinned {} archive identity changed during extraction",
                spec.name
            ))
            .into());
        }
        source::verify_pinned_checksum(
            &archive_handle,
            archive,
            checksum,
            &format!("{} archive", spec.name),
            spec.max_bytes,
        )?;
        let source = extract.join(spec.executable);
        let mut entries = fs::read_dir(&extract).map_err(crate::io_error)?;
        if entries
            .next()
            .transpose()
            .map_err(crate::io_error)?
            .is_none_or(|entry| entry.path() != source)
            || entries.next().is_some()
        {
            return Err(Error::Input(format!(
                "pinned {} archive has missing or unexpected extraction entries",
                spec.name
            ))
            .into());
        }
        let source_identity = fs::symlink_metadata(&source).map_err(crate::io_error)?;
        if !source_identity.is_file() {
            return Err(Error::Input(format!(
                "pinned {} source is not a regular executable",
                spec.name
            ))
            .into());
        }
        let source_handle = source::open_named(&source).map_err(crate::io_error)?;
        if !source::same_object(
            &source_identity,
            &source_handle.metadata().map_err(crate::io_error)?,
        ) {
            return Err(Error::Input(format!(
                "pinned {} extraction source identity changed",
                spec.name
            ))
            .into());
        }
        Ok((source, source_handle))
    })();
    let (extracted, handle) = prepared.map_err(|failure| match failure {
        InstallFailure::Abort(abort) => InstallFailure::Abort(abort),
        InstallFailure::Ordinary(error) => InstallFailure::Ordinary(Error::Io(format!(
            "{error}; inspect retained {} extraction {}",
            spec.name,
            extract.display()
        ))),
    })?;
    let destination = root.join("bin").join(spec.executable);
    let result = (|| -> InstallResult<_> {
        fs::create_dir_all(destination.parent().expect("archive executable parent"))
            .map_err(crate::io_error)?;
        progress(80, ProgressStage::Validating);
        source::install_input_in(
            context,
            &source::Input::pinned(
                extracted.clone(),
                handle.try_clone().map_err(crate::io_error)?,
            ),
            &destination,
            source::Options {
                kind: &format!("{} executable", spec.name),
                limit: source::MAX_SOURCE_BYTES,
                checksum: None,
                executable: true,
            },
            |path, scope| check_version_in(scope, path, spec.name),
            source::Publication::Replace,
        )?;
        Ok(destination)
    })();
    if matches!(&result, Err(InstallFailure::Abort(_))) {
        return result;
    }
    let settled = (|| -> io::Result<()> {
        let directory = fs::symlink_metadata(&extract)?;
        let named = fs::symlink_metadata(&extracted)?;
        if !source::same_named(&directory_handle, &extract)?
            || !source::same_named(&handle, &extracted)?
            || !directory.is_dir()
            || !source::same_object(&directory_identity, &directory)
            || !named.is_file()
            || !source::same_object(&handle.metadata()?, &named)
        {
            return Err(io::Error::other(format!(
                "{} extraction namespace identity changed",
                spec.name
            )));
        }
        source::Quarantined::take(
            &extracted,
            &handle,
            &directory_handle,
            source::FaultPoint::ArchiveCleanupTake,
        )?
        .finish(Ok(()))
        .map_err(|error| io::Error::other(error.to_string()))?;
        source::settle_directory(
            &extract,
            &directory_handle,
            source::FaultPoint::ArchiveDirectoryTake,
        )
    })();
    match (result, settled) {
        (Ok(path), Ok(())) => {
            progress(100, ProgressStage::Ready);
            Ok(path)
        }
        (Err(error), Ok(())) => Err(error),
        (result, Err(cleanup)) => Err(Error::Io(format!(
            "{}; {} scratch settlement failed: {cleanup}; inspect retained extraction {}",
            result.err().map_or_else(
                || format!("{} executable was published", spec.name),
                |error| error.to_string()
            ),
            spec.name,
            extract.display()
        ))
        .into()),
    }
}
