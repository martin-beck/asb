//! One frozen installation operation and the SAME actual source-stage owner.
use super::*;
use crate::install::process::{
    InstallFailure, InstallResult, OperationContext, ProcessScope, Purpose,
};
use std::sync::Arc;

pub(crate) fn check_version_in(
    scope: &mut ProcessScope,
    path: &Path,
    label: &str,
) -> InstallResult<()> {
    let mut step = scope.prepare(path, Purpose::Version, label)?;
    step.command_mut().arg("--version").stdin(Stdio::null());
    let result = scope.run(step)?;
    if !result.status.success() {
        let diagnostics = result.log.path.as_ref().map_or_else(
            || "the installation diagnostics".into(),
            |path| path.display().to_string(),
        );
        // This is an advisory about the post-completion filesystem check, NOT
        // evidence that status 127 means no process started. The producer and
        // owner observer have already completed through the normal boundary.
        if fs::symlink_metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound) {
            return Err(Error::Io(format!(
                "cannot start staged {label} again from {}: a post-failure check found the executable missing; the completed step reported {}; reinstall the missing artifact and retry; previous installation was preserved; inspect {diagnostics}",
                path.display(), result.status
            )).into());
        }
        return Err(Error::Io(format!("staged {label} at {} did not report a usable version ({}); previous installation was preserved; inspect {diagnostics}", path.display(), result.status)).into());
    }
    Ok(())
}

fn download_in(
    scope: &mut ProcessScope,
    source: &str,
    staging: &Path,
    options: &Options<'_>,
) -> InstallResult<()> {
    // The owning download transport knows its prerequisite. Resolve it before
    // FIRST admission; this is current availability, not pinned identity or an
    // inference from any later process exit code. Keep legacy PATHEXT behavior
    // on non-Linux hosts, which this lifecycle repair does not qualify.
    #[cfg(target_os = "linux")]
    let curl = tools::host_executable(Path::new("curl")).ok_or_else(|| Error::Io(format!(
        "{} needs `curl`, but no executable is currently available on PATH. Install curl with your system package manager (on Debian/Ubuntu: `sudo apt-get update && sudo apt-get install -y curl`), then retry.", options.kind
    )))?;
    #[cfg(not(target_os = "linux"))]
    let curl = PathBuf::from("curl");
    let headers = staging.with_extension(format!("headers-{}", tools::runtime::unique_id()));
    let download = super::super::transfer::Download::start(staging, &headers, options.kind);
    let mut step = scope.prepare(curl, Purpose::Download, options.kind)?;
    step.command_mut()
        .args([
            "--disable",
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--connect-timeout",
            "10",
            "--max-filesize",
        ])
        .arg(options.limit.to_string())
        .arg("--dump-header")
        .arg(&headers)
        .arg("--output")
        .arg(staging)
        .arg(source)
        .stdin(Stdio::null());
    let result = scope.run(step);
    let progress = download.finish();
    let _ = fs::remove_file(&headers);
    let result = result?;
    if !result.status.success() {
        return Err(Error::Io(format!("could not download {}: curl exited with {}; previous installation was preserved; inspect {}", options.kind, result.status,
            result.log.path.as_ref().map_or_else(|| "the installation diagnostics".into(), |path| path.display().to_string()))).into());
    }
    if let Some(total) = progress
        .total_bytes
        .filter(|total| *total != progress.completed_bytes)
    {
        return Err(Error::Io(format!(
            "could not download {}: received {} bytes but the server declared {} bytes; previous installation was preserved",
            options.kind, progress.completed_bytes, total
        )).into());
    }
    Ok(())
}

pub(in crate::install) fn install_input_in(
    context: &OperationContext,
    source: &Input,
    destination: &Path,
    options: Options<'_>,
    validate: impl FnOnce(&Path, &mut ProcessScope) -> InstallResult<()>,
    publication: Publication,
) -> InstallResult<()> {
    context.check_before_admission(options.kind, destination)?;
    let staging = Arc::new(Staging::new(destination)?);
    staging.monitor_producer(options.kind, options.limit)?;
    let mut scope = staging.producer_scope(context);
    let prepared = (|| -> InstallResult<prepared::Snapshot> {
        match source {
            Input::Remote(url) => download_in(&mut scope, url, &staging.file, &options)?,
            Input::Local(path) | Input::Pinned { path, .. } => {
                let mut input_options = OpenOptions::new();
                input_options.read(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    input_options.custom_flags(libc::O_NONBLOCK);
                }
                let mut input = if let Input::Pinned { handle, .. } = source {
                    use std::io::{Seek, SeekFrom};
                    let mut file = handle.try_clone().map_err(crate::io_error)?;
                    file.seek(SeekFrom::Start(0)).map_err(crate::io_error)?;
                    file
                } else {
                    input_options.open(path).map_err(|error| {
                        Error::Input(format!(
                            "cannot read {} source {}: {error}",
                            options.kind,
                            path.display()
                        ))
                    })?
                };
                let metadata = input.metadata().map_err(crate::io_error)?;
                if !metadata.is_file() {
                    return Err(Error::Input(format!(
                        "{} source is not a file: {}",
                        options.kind,
                        path.display()
                    ))
                    .into());
                }
                if metadata.len() > options.limit {
                    return Err(size_error(options.kind, options.limit).into());
                }
                let mut output = staging.writer().map_err(crate::io_error)?;
                let copied = io::copy(
                    &mut (&mut input).take(options.limit.saturating_add(1)),
                    &mut output,
                )
                .map_err(|error| {
                    Error::Io(format!("cannot copy {} source: {error}", options.kind))
                })?;
                checkpoint(FaultPoint::Copy, &staging.file)?;
                if copied > options.limit {
                    return Err(size_error(options.kind, options.limit).into());
                }
                if copied != metadata.len()
                    || !prepared::unchanged_metadata(
                        &metadata,
                        &input.metadata().map_err(crate::io_error)?,
                    )
                {
                    return Err(Error::Input(
                        "local installation source changed during bounded copying".into(),
                    )
                    .into());
                }
                output
                    .set_permissions(metadata.permissions())
                    .map_err(crate::io_error)?;
            }
        }
        staging.file_owned().map_err(crate::io_error)?;
        validate_file(&staging.file, &options)?;
        if options.executable {
            checkpoint(FaultPoint::Mode, &staging.file)?;
            staging.make_executable()?;
        }
        let pinned = staging.snapshot(&options)?;
        validate(&staging.file, &mut scope)?;
        let validated = staging.snapshot(&options)?;
        if !pinned.matches(&validated) {
            return Err(Error::Input("staged installation source changed after its version probe; previous installation was preserved".into()).into());
        }
        Ok(pinned)
    })();
    // Never attempt quiet cleanup or publication after typed uncertainty. The
    // retained worker owns this SAME real Staging; its eventual Drop keeps raw.
    if let Err(InstallFailure::Abort(abort)) = prepared {
        staging.retain_producer_scratch();
        return Err(InstallFailure::Abort(abort));
    }
    let prior = match &prepared {
        Err(InstallFailure::Ordinary(error)) => Some(error),
        _ => None,
    };
    let permission = scope
        .finish()
        .inspect_err(|failure| {
            if matches!(failure, InstallFailure::Abort(_)) {
                staging.retain_producer_scratch();
            }
        })
        .map_err(|failure| failure.with_prior_ordinary(prior))?;
    // Acquire every actual Stage resource before enabling its quiet Drop.
    // Uniqueness is NOT producer proof; the intrinsic permission is still
    // mandatory, and a pending observer must never gain destructive cleanup.
    let mut staging = Arc::try_unwrap(staging)
        .map_err(|stage| {
            stage.retain_producer_scratch();
            context.custody_failure(
                "settled installation producer still retains its staging owner",
                &stage.directory,
            )
        })
        .map_err(|failure| failure.with_prior_ordinary(prior))?;
    let permission = staging
        .accept_producer_permission(permission)
        .inspect_err(|failure| {
            if matches!(failure, InstallFailure::Abort(_)) {
                staging.retain_producer_scratch();
            }
        })
        .map_err(|failure| failure.with_prior_ordinary(prior))?;
    let result = match prepared {
        Ok(pinned) => (|| -> Result<(), super::prepared::PublicationFailure<InstallFailure>> {
            staging.synchronize()?;
            checkpoint(FaultPoint::Commit, &staging.file)?;
            if !pinned.matches(&staging.snapshot(&options)?) {
                return Err(Error::Input("staged installation source changed before commit; previous installation was preserved".into()).into());
            }
            staging.file_owned().map_err(crate::io_error)?;
            staging.publish_copy_checked(destination, &options, &pinned, publication, || {
                permission.check()
            })
        })(),
        Err(InstallFailure::Ordinary(error)) => {
            Err(super::prepared::PublicationFailure::CorePublication(error))
        }
        Err(InstallFailure::Abort(_)) => unreachable!("typed abort returned before settlement"),
    };
    match result {
        Ok(()) => staging.finish(Ok(())).map_err(Into::into),
        Err(super::prepared::PublicationFailure::CorePublication(error)) => {
            staging.finish(Err(error)).map_err(Into::into)
        }
        Err(super::prepared::PublicationFailure::Check(error)) => Err(error),
    }
}
