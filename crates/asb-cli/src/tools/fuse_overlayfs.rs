//! Pinned project-local static overlay helper for filesystem recording.
use super::{StaticTool, ToolKind};
use crate::{Error, install::Recipe};
use std::path::{Path, PathBuf};

pub(super) static ADAPTER: StaticTool = StaticTool::new(
    "fuse-overlayfs",
    Recipe::Custom {
        executable: "fuse-overlayfs",
        description: "install a pinned static FUSE overlay helper",
        summary: "fuse-overlayfs 1.18 (static)",
        install,
        guest_assets: None,
    },
    ToolKind::Support,
)
.with_guest_assets(guest_assets);

fn distribution() -> Result<(&'static str, &'static str), Error> {
    if !cfg!(target_os = "linux") {
        return Err(Error::Input("fuse-overlayfs requires Linux".into()));
    }
    match std::env::consts::ARCH {
        "x86_64" => Ok((
            "https://github.com/containers/fuse-overlayfs/releases/download/v1.18/fuse-overlayfs-x86_64",
            "56b0ae0aeb8abb308b068af2f137ed8d1bd239f4f27e21672ff0def861eea1e8",
        )),
        "aarch64" => Ok((
            "https://github.com/containers/fuse-overlayfs/releases/download/v1.18/fuse-overlayfs-aarch64",
            "82fed736197b2a881a822e5357b488796f654e8371ce8573a1592331510a0133",
        )),
        _ => Err(Error::Input(
            "fuse-overlayfs currently supports Linux x86_64 and aarch64".into(),
        )),
    }
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
    let (url, checksum) = distribution()?;
    let name = url.rsplit('/').next().expect("release asset name");
    let source = super::runtime::cached_archive_in(
        context,
        &root.join("cache").join(name),
        url,
        checksum,
        progress,
    )?;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).map_err(crate::io_error)?;
    let target = bin.join("fuse-overlayfs");
    std::fs::copy(source, &target).map_err(crate::io_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .map_err(crate::io_error)?;
    }
    progress(90, crate::install::ProgressStage::Finalizing);
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolAdapter;

    #[test]
    fn catalog_uses_pinned_upstream_binary_and_guest_cache() {
        let (url, hash) = distribution().unwrap();
        assert!(
            url.starts_with(
                "https://github.com/containers/fuse-overlayfs/releases/download/v1.18/"
            )
        );
        assert_eq!(hash.len(), 64);
        let assets = ADAPTER.guest_assets(Path::new("fixture")).unwrap();
        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].distribution, Some((url, hash)));
    }
}
