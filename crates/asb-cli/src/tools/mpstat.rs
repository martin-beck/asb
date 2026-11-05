use super::{StaticTool, ToolKind};
use crate::install::{InstallResult, OperationContext, ProgressStage, Recipe};
use std::path::{Path, PathBuf};
pub(super) static ADAPTER: StaticTool = StaticTool::new(
    "mpstat",
    Recipe::Custom {
        executable: "mpstat",
        description: "build project-local sysstat collectors",
        summary: "project-local mpstat and pidstat",
        install,
        guest_assets: None,
    },
    ToolKind::Collector,
)
.with_init_runtime()
.with_helper_executables(&["pidstat"])
.with_host_probe(host_usable);
fn install(
    context: &OperationContext,
    root: &Path,
    progress: &mut dyn FnMut(u8, ProgressStage),
) -> InstallResult<PathBuf> {
    super::sysstat::install(context, root, progress, "mpstat")
}
fn host_usable(_: &OperationContext) -> InstallResult<bool> {
    Ok(super::sysstat::host_usable("mpstat"))
}
