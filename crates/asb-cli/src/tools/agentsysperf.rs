//! Project-local AgentSysPerf runtime used by optional ASB collectors.

use super::{StaticTool, ToolKind};
use crate::install::Recipe;

/// AgentSysPerf does not currently publish the collector integration on PyPI.
/// Pin the reviewed upstream source revision so a project records the exact
/// protocol/analyzer surface its ASB bridge invokes.  The tool is deliberately
/// a support runtime: it never becomes an agent executable or a workload
/// harness in ASB.
pub(super) static ADAPTER: StaticTool = StaticTool::new(
    "agentsysperf",
    Recipe::Python {
        package: "agentsysperf @ git+https://github.com/intel/agentsysperf.git@a760738988552fafc62068dbb2da36cc562a6d78",
        dependencies: &[],
        executable: "agentsysperf",
    },
    ToolKind::Support,
);
