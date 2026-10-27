use super::{StaticTool, ToolKind};
use crate::install::Recipe;

/// Pinned dataset client used only while installing LiveCodeBench task
/// catalogs. It is deliberately separate from an agent/provider runtime:
/// installed task data is read locally during benchmark execution.
pub(super) static ADAPTER: StaticTool = StaticTool::new(
    "livecodebench",
    Recipe::Python {
        package: "datasets==3.5.0",
        // The pinned upstream evaluators import these at evaluation time. They
        // are installed once into the project-local support-tool environment,
        // never downloaded while a benchmark is running.
        dependencies: &["numpy==2.2.4", "tqdm==4.67.1"],
        executable: "datasets-cli",
    },
    ToolKind::Support,
);
