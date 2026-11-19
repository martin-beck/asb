use super::{StaticTool, ToolKind};
use crate::Error;
use crate::install::{InstallResult, OperationContext, Recipe, install_executable_source_in};
use std::path::{Path, PathBuf};

pub(super) static ADAPTER: StaticTool = StaticTool::new(
    "flamegraph",
    Recipe::Custom {
        executable: "flamegraph",
        description: "download pinned FlameGraph scripts",
        summary: "pinned FlameGraph scripts -> flamegraph",
        install,
        guest_assets: None,
    },
    ToolKind::Collector,
)
.with_helper_executables(&["stackcollapse-perf.pl", "difffolded.pl"])
.with_init_runtime();

fn install(
    context: &OperationContext,
    root: &Path,
    progress: &mut dyn FnMut(u8, crate::install::ProgressStage),
) -> InstallResult<PathBuf> {
    progress(25, crate::install::ProgressStage::Downloading);
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", bin.display())))?;
    for (name, url) in [
        (
            "stackcollapse-perf.pl",
            "https://raw.githubusercontent.com/brendangregg/FlameGraph/41fee1f99f9276008b7cd112fca19dc3ea84ac32/stackcollapse-perf.pl",
        ),
        (
            "flamegraph",
            "https://raw.githubusercontent.com/brendangregg/FlameGraph/41fee1f99f9276008b7cd112fca19dc3ea84ac32/flamegraph.pl",
        ),
        (
            "difffolded.pl",
            "https://raw.githubusercontent.com/brendangregg/FlameGraph/41fee1f99f9276008b7cd112fca19dc3ea84ac32/difffolded.pl",
        ),
    ] {
        let destination = bin.join(name);
        progress(
            match name {
                "stackcollapse-perf.pl" => 25,
                "difffolded.pl" => 50,
                "flamegraph" => 70,
                _ => unreachable!("registered FlameGraph asset"),
            },
            crate::install::ProgressStage::DownloadingAndValidating,
        );
        install_executable_source_in(context, url, &destination, "flamegraph tool")?;
        progress(
            match name {
                "stackcollapse-perf.pl" => 50,
                "difffolded.pl" => 70,
                "flamegraph" => 90,
                _ => unreachable!("registered FlameGraph asset"),
            },
            crate::install::ProgressStage::Copying,
        );
    }
    Ok(bin.join("flamegraph"))
}
