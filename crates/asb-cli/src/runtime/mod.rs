//! Planning primitives for portable benchmark runtime settings.
//!
//! CPU-pool planning does not start a process or apply affinity. Launch-time
//! enforcement and CPU-wide profiling are separate, required integration work.

pub mod admission;
pub mod affinity;
pub mod cpu_lease;
pub mod cpu_pool;
pub mod evidence;
pub mod perf_scope;
pub mod settings;
pub mod wizard;
