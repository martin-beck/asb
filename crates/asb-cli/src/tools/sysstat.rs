//! Build the process-scoped sysstat collectors in the project when absent.
use crate::{
    Error,
    install::{InstallResult, OperationContext, ProducerDirectory, ProgressStage, Purpose},
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

const SOURCE: &str = "https://github.com/sysstat/sysstat/archive/refs/tags/v12.8.0.tar.gz";
const SHA256: &str = "8aa2054c56c941ab30e1b14ad2e0076a7e6d6bf01f50e22d954885b8a7f9a679";

pub(super) fn install(
    context: &OperationContext,
    root: &Path,
    progress: &mut dyn FnMut(u8, ProgressStage),
    program: &str,
) -> InstallResult<PathBuf> {
    if !cfg!(target_os = "linux") {
        return Err(Error::Input("sysstat requires Linux".into()).into());
    }
    let root = std::path::absolute(root).map_err(crate::io_error)?;
    let archive = super::runtime::cached_archive_in(
        context,
        &root.join("cache/sysstat-12.8.0.tar.gz"),
        SOURCE,
        SHA256,
        progress,
    )?;
    let stage = root.join(format!(".build-{}", super::runtime::unique_id()));
    std::fs::create_dir_all(&stage).map_err(crate::io_error)?;
    let owner = ProducerDirectory::open(&stage)?;
    let mut scope = owner.scope(context);
    let result = (|| -> InstallResult<()> {
        let mut extract = scope.prepare("tar", Purpose::Extract, "sysstat source extraction")?;
        extract
            .command_mut()
            .arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&stage)
            .arg("--strip-components=1")
            .stdin(Stdio::null());
        if !scope.run(extract)?.status.success() {
            return Err(Error::Io("could not extract sysstat sources".into()).into());
        }
        progress(45, ProgressStage::Configuring);
        let mut configure = scope.prepare(
            stage.join("configure"),
            Purpose::Build,
            "sysstat configuration",
        )?;
        configure
            .command_mut()
            .current_dir(&stage)
            .arg("--disable-nls")
            .stdin(Stdio::null());
        if !scope.run(configure)?.status.success() {
            return Err(Error::Io(
                "could not configure sysstat; install a C compiler and make".into(),
            )
            .into());
        }
        progress(60, ProgressStage::Building);
        let mut make = scope.prepare("make", Purpose::Build, "sysstat compilation")?;
        make.command_mut()
            .current_dir(&stage)
            .args(["-j2", program, "pidstat"])
            .stdin(Stdio::null());
        if !scope.run(make)?.status.success() {
            return Err(Error::Io(
                "could not build sysstat; inspect installation diagnostics".into(),
            )
            .into());
        }
        Ok(())
    })();
    owner.consume(scope.finish()?)?;
    result?;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).map_err(crate::io_error)?;
    for name in [program, "pidstat"] {
        let source = stage.join(name);
        let source = source
            .to_str()
            .ok_or_else(|| Error::Input("sysstat build path is not valid UTF-8".into()))?;
        crate::install::install_executable_source_in(
            context,
            source,
            &bin.join(name),
            "sysstat tool",
        )?;
    }
    std::fs::remove_dir_all(&stage).map_err(crate::io_error)?;
    progress(100, ProgressStage::Ready);
    Ok(bin.join(program))
}

pub(super) fn host_usable(program: &str) -> bool {
    super::host_executable(Path::new(program)).is_some()
        && super::host_executable(Path::new("pidstat")).is_some()
}
