//! Project-local upstream static Btrfs tools for snapshot workloads.
use super::{StaticTool, ToolKind};
use crate::{
    Error,
    install::{InstallResult, OperationContext, ProgressStage, Recipe},
};
use std::path::{Path, PathBuf};

pub(super) static ADAPTER: StaticTool = StaticTool::new(
    "btrfs",
    Recipe::Custom {
        executable: "btrfs",
        description: "install project-local Btrfs snapshot tools",
        summary: "btrfs-progs 7.1 (static)",
        install,
        guest_assets: None,
    },
    ToolKind::Support,
)
.with_init_runtime()
.with_runtime_path()
.with_host_probe(host_usable)
.with_helper_executables(&["mkfs.btrfs"]);

fn host_usable(_: &OperationContext) -> InstallResult<bool> {
    Ok(super::host_executable(Path::new("btrfs")).is_some()
        && super::host_executable(Path::new("mkfs.btrfs")).is_some())
}

fn distribution() -> Result<(&'static str, &'static str), Error> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok((
            "https://github.com/kdave/btrfs-progs/releases/download/v7.1/btrfs.box.static",
            "ed9a8815d12e40d2bf413d1259133a6727106715a2e5d8095179439b5d3f1467",
        )),
        ("linux", "aarch64") => Ok((
            "https://github.com/kdave/btrfs-progs/releases/download/v7.1/btrfs.box.static-arm",
            "42c59468de77ad5fac2b9eb378ee9201ad501d8fcbc588807ce6ff123dfbdac6",
        )),
        _ => Err(Error::Input(
            "project-local Btrfs tools require Linux x86_64 or aarch64".into(),
        )),
    }
}

fn install(
    context: &OperationContext,
    root: &Path,
    progress: &mut dyn FnMut(u8, ProgressStage),
) -> InstallResult<PathBuf> {
    let (url, checksum) = distribution()?;
    let archive = root.join("cache").join(url.rsplit('/').next().unwrap());
    let source = super::runtime::cached_archive_in(context, &archive, url, checksum, progress)?;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).map_err(crate::io_error)?;
    let source = source
        .to_str()
        .ok_or_else(|| Error::Input("Btrfs source path is not valid UTF-8".into()))?;
    for name in ["btrfs", "mkfs.btrfs"] {
        crate::install::install_executable_source_in(
            context,
            source,
            &bin.join(name),
            "Btrfs tool",
        )?;
    }
    progress(100, ProgressStage::Ready);
    Ok(bin.join("btrfs"))
}
