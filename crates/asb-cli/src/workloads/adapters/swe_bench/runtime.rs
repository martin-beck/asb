//! SWE-bench-owned SDK selection; shared execution has no benchmark branches.
use super::{ENGINE, SweBenchAdapter, catalog};
use crate::workloads::{PreparationContext, WorkloadSession};

pub(super) fn prepare(
    adapter: &SweBenchAdapter,
    context: &PreparationContext<'_>,
) -> Result<Box<dyn WorkloadSession>, String> {
    let installed = catalog::read(adapter, context.workload_directory)?;
    let task = context
        .trial
        .ok_or("select an official instance ID in setup before running this benchmark")?;
    if !installed.tasks.iter().any(|item| item.id == task) {
        return Err("selected instance is absent from the installed dataset".into());
    }
    let tool = context.tool_root(ENGINE.tool());
    ENGINE.publish(&tool).map_err(|error| error.to_string())?;
    crate::workloads::remote::launch_with_cpu_allocation(
        ENGINE.tool(),
        &ENGINE.script(),
        Some("session"),
        adapter.id,
        installed.dataset,
        context,
    )
}
