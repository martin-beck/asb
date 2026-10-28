//! Pinned archive preparation and explicit, nonrecursive extraction settlement.
use super::*;

#[cfg(test)]
pub(super) fn install_archive(
    root: &Path,
    archive: &Path,
    checksum: &str,
    progress: &mut dyn FnMut(u8, ProgressStage),
) -> Result<PathBuf, Error> {
    let context = OperationContext::new(root, &project::ProjectConfig::default())
        .map_err(InstallFailure::into_public)?;
    install_archive_in(&context, root, archive, checksum, progress)
        .map_err(InstallFailure::into_public)
}

pub(crate) fn install_archive_in(
    context: &OperationContext,
    root: &Path,
    archive: &Path,
    checksum: &str,
    progress: &mut dyn FnMut(u8, ProgressStage),
) -> InstallResult<PathBuf> {
    let metadata = fs::symlink_metadata(archive).map_err(crate::io_error)?;
    if !metadata.is_file() || metadata.len() > 256 * 1024 * 1024 {
        return Err(
            Error::Input("pinned Goose archive is not a bounded regular file".into()).into(),
        );
    }
    let archive_handle = source::open_named(archive).map_err(crate::io_error)?;
    if !source::same_object(
        &metadata,
        &archive_handle.metadata().map_err(crate::io_error)?,
    ) || !source::same_named(&archive_handle, archive).map_err(crate::io_error)?
    {
        return Err(
            Error::Input("pinned Goose archive identity changed before extraction".into()).into(),
        );
    }
    source::verify_pinned_checksum(
        &archive_handle,
        archive,
        checksum,
        "Goose archive",
        256 * 1024 * 1024,
    )?;
    let extract = root.join(format!("extract-{}", tools::runtime::unique_id()));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&extract)
        .map_err(|error| Error::Io(format!("cannot create Goose extraction directory: {error}")))?;
    let directory_identity = fs::symlink_metadata(&extract).map_err(crate::io_error)?;
    let directory_handle = source::open_named(&extract).map_err(crate::io_error)?;
    let owner = ProducerDirectory::open(&extract)?;
    let mut scope = owner.scope(context);
    // Before a complete extraction is positively inspected, no child-created
    // entry is ours to unlink. Errors retain the entire scratch, without Drop.
    let prepared = (|| -> InstallResult<_> {
        progress(60, ProgressStage::Extracting);
        let mut step = scope.prepare("tar", Purpose::Extract, "Goose extraction")?;
        step.command_mut()
            .args(["--extract", "--gzip", "--file"])
            .arg(archive)
            .arg("--directory")
            .arg(&extract)
            .stdin(Stdio::null());
        let mut extraction = crate::install::transfer::Activity::new(
            "Extracting Goose archive",
            Some(metadata.len()),
        );
        extraction.update(0);
        let status = scope.run(step)?.status;
        extraction.update(metadata.len());
        extraction.finish();
        owner.consume(scope.finish()?)?;
        if !status.success() {
            return Err(Error::Io("could not extract the pinned Goose binary".into()).into());
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
            return Err(Error::Input(
                "pinned Goose archive identity changed during extraction".into(),
            )
            .into());
        }
        source::verify_pinned_checksum(
            &archive_handle,
            archive,
            checksum,
            "Goose archive",
            256 * 1024 * 1024,
        )?;
        let source = extract.join("goose");
        let mut entries = fs::read_dir(&extract).map_err(crate::io_error)?;
        if entries
            .next()
            .transpose()
            .map_err(crate::io_error)?
            .is_none_or(|entry| entry.path() != source)
            || entries.next().is_some()
        {
            return Err(Error::Input(
                "pinned Goose archive has missing or unexpected extraction entries".into(),
            )
            .into());
        }
        let source_identity = fs::symlink_metadata(&source).map_err(crate::io_error)?;
        if !source_identity.is_file() {
            return Err(
                Error::Input("pinned Goose source is not a regular executable".into()).into(),
            );
        }
        let source_handle = source::open_named(&source).map_err(crate::io_error)?;
        if !source::same_object(
            &source_identity,
            &source_handle.metadata().map_err(crate::io_error)?,
        ) {
            return Err(
                Error::Input("pinned Goose extraction source identity changed".into()).into(),
            );
        }
        Ok((source, source_handle))
    })();
    let (extracted, handle) = prepared.map_err(|failure| match failure {
        InstallFailure::Abort(abort) => InstallFailure::Abort(abort),
        InstallFailure::Ordinary(error) => InstallFailure::Ordinary(Error::Io(format!(
            "{error}; inspect retained Goose extraction {}",
            extract.display()
        ))),
    })?;
    let destination = root.join("bin/goose");
    let result = (|| -> InstallResult<_> {
        fs::create_dir_all(destination.parent().expect("Goose bin parent"))
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
                kind: "Goose executable",
                limit: source::MAX_SOURCE_BYTES,
                checksum: None,
                executable: true,
            },
            |path, scope| check_version_in(scope, path, "Goose"),
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
            return Err(io::Error::other(
                "Goose extraction namespace identity changed",
            ));
        }
        source::Quarantined::take(
            &extracted,
            &handle,
            &directory_handle,
            source::FaultPoint::GooseCleanupTake,
        )?
        .finish(Ok(()))
        .map_err(|error| io::Error::other(error.to_string()))?;
        source::settle_directory(
            &extract,
            &directory_handle,
            source::FaultPoint::GooseDirectoryTake,
        )
    })();
    match (result, settled) {
        (Ok(path), Ok(())) => {
            progress(100, ProgressStage::Ready);
            Ok(path)
        }
        (Err(error), Ok(())) => Err(error),
        (result, Err(cleanup)) => Err(Error::Io(format!(
            "{}; Goose scratch settlement failed: {cleanup}; inspect retained extraction {}",
            result.err().map_or_else(
                || "Goose executable was published".into(),
                |error| error.to_string()
            ),
            extract.display()
        ))
        .into()),
    }
}
