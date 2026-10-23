//! Official static uv distribution for project-local Python bootstrapping.
use super::{StaticTool, ToolKind};
use crate::{Error, install::Recipe};
use std::path::{Path, PathBuf};

pub(super) static ADAPTER: StaticTool = StaticTool::new(
    "uv",
    Recipe::Custom {
        executable: "uv",
        description: "install the pinned official static uv Python environment manager",
        summary: "uv 0.9.9 (static)",
        install,
        guest_assets: None,
    },
    ToolKind::Support,
)
.with_runtime_path()
.with_guest_assets(guest_assets);

fn distribution() -> Result<(&'static str, &'static str), Error> {
    Ok(match std::env::consts::ARCH {
        "x86_64" => (
            "https://github.com/astral-sh/uv/releases/download/0.9.9/uv-x86_64-unknown-linux-musl.tar.gz",
            "b0e4530c1fed09720d7eb703703047f097fe23d6b266945a98422f0a0e5c74b4",
        ),
        "aarch64" => (
            "https://github.com/astral-sh/uv/releases/download/0.9.9/uv-aarch64-unknown-linux-musl.tar.gz",
            "84675e9f81c7ec45b11761db4e1f047c64e772d96a7d99201507a10279f7f835",
        ),
        _ => {
            return Err(Error::Input(
                "uv runtime requires Linux x86_64 or aarch64".into(),
            ));
        }
    })
}

fn guest_assets(root: &Path) -> Result<Vec<super::GuestAsset>, Error> {
    let (url, checksum) = distribution()?;
    Ok(super::runtime::guest_archive(root, url, checksum))
}

fn install(
    context: &crate::install::OperationContext,
    root: &Path,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> crate::install::InstallResult<PathBuf> {
    let (url, hash) = distribution()?;
    super::runtime::archive(
        context,
        root,
        url,
        hash,
        &[("uv", "uv"), ("uvx", "uvx")],
        progress,
    )
}
