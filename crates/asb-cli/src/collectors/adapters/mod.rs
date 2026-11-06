//! Individual collector adapters.

use super::CollectorAdapter;

mod agent_memory;
pub(crate) mod agent_processes;
mod agent_workers;
mod agentsysperf;
pub(crate) mod arm_platform;
pub(crate) mod arm_pmu;
mod arm_spe;
pub(crate) mod bpftrace;
mod cpu_accounting;
pub(crate) mod flamegraph;
pub(crate) mod iostat;
mod kernel_pressure;
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
    &kernel_pressure::ADAPTER,
    &agentsysperf::ADAPTER,
    &arm_platform::ADAPTER,
    &arm_pmu::ADAPTER,
    &arm_spe::ADAPTER,
    &system::ADAPTER,
    &mpstat::ADAPTER,
    &iostat::ADAPTER,
    &perf::ADAPTER,
    &perfetto::ADAPTER,
    &flamegraph::ADAPTER,
    &bpftrace::ADAPTER,
    &tool_calls::ADAPTER,
];
