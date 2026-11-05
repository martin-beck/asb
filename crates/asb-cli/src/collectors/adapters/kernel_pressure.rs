//! Pressure observations from the CPU collector's exclusive agent-tree leaf.
//! A whole-container or host PSI file is never an isolation fallback.
mod capture;
#[cfg(test)]
mod tests;

use crate::collectors::{CollectorAdapter, LifecycleEvent, TargetScope};
use crate::measurements::{self, Session};
use capture::{Capture, Sampler};

pub(super) struct KernelPressure;
pub(super) static ADAPTER: KernelPressure = KernelPressure;

#[derive(Default)]
struct State {
    enabled: bool,
    sampler: Option<Sampler>,
    capture: Option<Capture>,
    reason: Option<String>,
    execution_ended: bool,
    execution_begun: bool,
    target: Option<measurements::ProcessIdentity>,
}

fn start_capture(session: &mut Session) {
    let state = session.state::<State>();
    let mut state = state.borrow_mut();
    if !state.enabled || !state.execution_begun || state.execution_ended || state.sampler.is_some()
    {
        return;
    }
    state.reason = Some("No exclusive agent cgroup is available; host and whole-container pressure are excluded. Use a delegated native cgroup-v2 scope for agent pressure measurements.".into());
    state.target = session.measurement_target().filter(|root| root.matches());
    if state.target.is_some()
        && let Some(path) = super::cpu_accounting::exclusive_leaf(session)
    {
        match Sampler::start(
            path,
            measurements::measurement_interval_ms().clamp(100, 1000),
        ) {
            Ok(sampler) => {
                state.sampler = Some(sampler);
                state.reason = None;
            }
            Err(reason) => state.reason = Some(reason),
        }
    }
}

impl CollectorAdapter for KernelPressure {
    fn id(&self) -> &'static str {
        "kernel-pressure"
    }
    fn description(&self) -> &'static str {
        "agent-only CPU, memory and I/O pressure time series"
    }
    fn target_scope(&self, _: &str) -> TargetScope {
        TargetScope::AgentProcess
    }
    fn dependencies(&self) -> &'static [&'static str] {
        &["cpu-accounting"]
    }
    fn requires_pid(&self) -> bool {
        true
    }
    fn estimated_disk_mib(&self) -> u64 {
        16
    }
    fn configure(&self, session: &mut Session, selected: &[String]) -> Result<(), String> {
        session.state::<State>().borrow_mut().enabled =
            crate::collectors::selected_or_environment(self.id(), selected);
        Ok(())
    }
    fn attach_pid(&self, session: &mut Session, _: u32) -> Result<(), String> {
        // Ordinary native execution emits begin before spawn; cohort execution
        // emits it after attachment. Capture needs both boundaries, in either order.
        start_capture(session);
        Ok(())
    }
    fn lifecycle_event(&self, session: &mut Session, event: LifecycleEvent) -> Result<(), String> {
        let state = session.state::<State>();
        let mut state = state.borrow_mut();
        if !state.enabled {
            return Ok(());
        }
        match event {
            LifecycleEvent::AgentExecuteBegin => {
                if state.execution_begun || state.execution_ended {
                    return Err("kernel-pressure received a repeated execution boundary".into());
                }
                state.execution_begun = true;
            }
            LifecycleEvent::AgentExecuteEnd | LifecycleEvent::RunFailed => {
                state.execution_ended = true;
                if let Some(sampler) = state.sampler.take() {
                    match sampler.stop() {
                        Ok(capture) => state.capture = Some(capture),
                        Err(reason) => state.reason = Some(reason),
                    }
                }
            }
            _ => {}
        }
        drop(state);
        if event == LifecycleEvent::AgentExecuteBegin {
            start_capture(session);
        }
        Ok(())
    }
    fn finish(&self, session: &mut Session) -> Result<(), String> {
        let state = session.state::<State>();
        let mut state = state.borrow_mut();
        if !state.enabled {
            return Ok(());
        }
        // Finalization is not an execution boundary: never include grading if
        // the caller failed to dispatch AgentExecuteEnd/RunFailed.
        if let Some(sampler) = state.sampler.take() {
            drop(sampler);
            state.capture = None;
            state.reason =
                Some("Agent execution end was not observed; pressure capture is incomplete".into());
        }
        let (mut report, mut summary) =
            capture::report(state.capture.as_ref(), state.reason.as_deref());
        let target = state.target.map(
            |root| serde_json::json!({"pid":root.pid,"start_time_ticks":root.start_time_ticks}),
        );
        report["target"] = serde_json::json!(target);
        summary["target"] = serde_json::json!(target);
        measurements::write_json(&session.dir.join("kernel-pressure.json"), &report)?;
        measurements::write_json(&session.dir.join("kernel-pressure-summary.json"), &summary)
    }
}
