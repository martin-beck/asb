//! Individual collector adapters.

use super::CollectorAdapter;

mod agent_memory;
mod agent_processes;
mod agent_workers;
pub(crate) mod bpftrace;
mod cpu_accounting;
pub(crate) mod flamegraph;
pub(crate) mod iostat;
pub(crate) mod mpstat;
pub(crate) mod perf;
pub(crate) mod perfetto;
mod pidstat;
mod process_accounting;
mod system;
mod tool_calls;

pub(super) static REGISTRY: &[&dyn CollectorAdapter] = &[
    &agent_memory::ADAPTER,
    &agent_workers::ADAPTER,
    &cpu_accounting::ADAPTER,
    &system::ADAPTER,
    &mpstat::ADAPTER,
    &iostat::ADAPTER,
    &perf::ADAPTER,
    &perfetto::ADAPTER,
    &flamegraph::ADAPTER,
    &bpftrace::ADAPTER,
    &tool_calls::ADAPTER,
];
