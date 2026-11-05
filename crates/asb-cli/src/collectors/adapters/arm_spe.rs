//! Optional inherited SPE capture, independent of the shared core-PMU producer.
mod decoder;
mod quality;
mod samples;
#[cfg(test)]
mod tests;

use crate::collectors::{CollectorAdapter, ProcessContext, ProcessData, TargetScope};
use crate::measurements::{self, Session};
use serde::Serialize;
use std::time::Duration;

pub(super) struct ArmSpe;
pub(super) static ADAPTER: ArmSpe = ArmSpe;
const MIB: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, PartialEq)]
struct Options {
    period: u64,
    budget_bytes: u64,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            period: 1_048_576,
            budget_bytes: 256 * MIB,
        }
    }
}
fn options(selection: &str) -> Option<Options> {
    let mut parts = selection.split(':');
    if parts.next()? != "arm-spe" {
        return None;
    }
    let mut result = Options::default();
    let mut seen = std::collections::BTreeSet::new();
    for part in parts {
        let (key, value) = part.split_once('=')?;
        if !seen.insert(key) {
            return None;
        }
        let value = value.parse::<u64>().ok()?;
        match key {
            "period" if (65_536..=1_073_741_824).contains(&value) => result.period = value,
            "budget-mib" if (16..=1024).contains(&value) => result.budget_bytes = value * MIB,
            _ => return None,
        }
    }
    Some(result)
}

#[derive(Default, Serialize)]
struct State {
    selected: bool,
    options: Options,
    pmu: Option<String>,
    reason: Option<String>,
    started: bool,
    #[serde(skip)]
    root: Option<measurements::ProcessIdentity>,
}

fn select_pmu(pmus: &[super::arm_platform::Pmu]) -> Result<String, String> {
    let candidates = pmus
        .iter()
        .filter(|p| p.domain == "spe")
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        return Err("SPE requires one unambiguous advertised SPE PMU; multiple PMU domains cannot be combined by this perf recorder".into());
    }
    let pmu = candidates[0];
    if !["ts_enable", "load_filter", "store_filter"]
        .iter()
        .all(|key| pmu.formats.contains_key(*key))
    {
        return Err("SPE PMU lacks required timestamp and load/store filter controls".into());
    }
    Ok(pmu.name.clone())
}

fn args(pmu: &str, pid: u32, options: &Options) -> Vec<String> {
    vec![
        "record".into(),
        "-e".into(),
        format!("{pmu}/ts_enable=1,load_filter=1,store_filter=1/"),
        "-c".into(),
        options.period.to_string(),
        "--inherit".into(),
        "-p".into(),
        pid.to_string(),
        "--max-size".into(),
        format!("{}M", options.budget_bytes / MIB),
        "--mmap-pages".into(),
        "64,64".into(),
    ]
}

impl CollectorAdapter for ArmSpe {
    fn id(&self) -> &'static str {
        "arm-spe"
    }
    fn description(&self) -> &'static str {
        "agent-tree SPE sampled memory latency and cache/TLB events"
    }
    fn default_enabled(&self) -> bool {
        false
    }
    fn graph_section(&self, name: &str) -> Option<crate::collectors::GraphSection> {
        ["metric-arm-spe.","series-arm-spe.","metric-analysis.arm_spe."].iter().any(|prefix|name.starts_with(prefix))
            .then_some(crate::collectors::GraphSection{
                title:"Arm sampled microarchitecture",
                description:"Process-tree SPE sampled classifications and known operation-latency cycles. These are sampled observations, not full event counts, cache miss rates or isolated DRAM latency.",
            })
    }
    fn dependencies(&self) -> &'static [&'static str] {
        &["arm-platform", "perf"]
    }
    fn target_scope(&self, _: &str) -> TargetScope {
        TargetScope::AgentProcess
    }
    fn requires_pid(&self) -> bool {
        true
    }
    fn estimated_disk_mib(&self) -> u64 {
        1536
    }
    fn accepts_selection(&self, selection: &str) -> bool {
        options(selection).is_some()
    }
    fn producer_policy(&self, name: &str) -> measurements::ProducerPolicy {
        measurements::ProducerPolicy {
            stream_bytes: 2 * MIB,
            graceful: (name == self.id()).then_some(Duration::from_secs(5)),
        }
    }
    fn accepts_termination(&self, _: &str, completion: &measurements::ProducerCompletion) -> bool {
        super::perf::accepted_exit(
            completion.exit,
            completion.interruption_requested
                && completion.interruption_acknowledged
                && completion.guardian_cleanup_confirmed == Some(true)
                && completion.capture_complete,
        )
    }
    fn allows_failed_termination(&self, _: &str) -> bool {
        true
    }
    fn retain_raw_on_processing_failure(&self) -> bool {
        true
    }
    fn configure(&self, session: &mut Session, selected: &[String]) -> Result<(), String> {
        let choices = selected
            .iter()
            .filter_map(|s| options(s))
            .collect::<Vec<_>>();
        if choices.len() > 1 {
            return Err(
                "Select one arm-spe configuration; use arm-spe[:period=N][:budget-mib=N]".into(),
            );
        }
        let Some(options) = choices.into_iter().next() else {
            return Ok(());
        };
        let inventory = super::arm_platform::resolved_inventory(session);
        let pmu = if cfg!(target_arch = "aarch64") {
            select_pmu(&inventory.pmus)
        } else {
            Err("SPE requires an AArch64 host with a readable task-scoped SPE PMU".into())
        };
        let state = session.state::<State>();
        let mut state = state.borrow_mut();
        state.selected = true;
        state.options = options;
        match pmu {
            Ok(pmu) => state.pmu = Some(pmu),
            Err(reason) => state.reason = Some(reason),
        }
        Ok(())
    }
    fn start(&self, session: &mut Session) -> Result<(), String> {
        let state = session.state::<State>();
        let state = state.borrow();
        if !state.selected {
            return Ok(());
        }
        measurements::write_json(
            &session.dir.join("arm-spe-plan.json"),
            &serde_json::json!({
                "schema_version":1,"scope":"agent-process-tree","plan":&*state,
                "limitations":"Explicitly opt-in. Counts are sampled observations, not retired instructions or whole-run event estimates. No host-wide fallback, guessed PMU encodings or per-agent uncore allocation."
            }),
        )
    }
    fn attach_pid(&self, session: &mut Session, pid: u32) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        start(session, pid)?;
        #[cfg(not(target_os = "linux"))]
        let _ = (session, pid);
        Ok(())
    }
    fn attach_isolated_pid(&self, session: &mut Session, pid: u32) -> Result<(), String> {
        let state = session.state::<State>();
        if !state.borrow().selected || state.borrow().pmu.is_none() {
            return Ok(());
        }
        let _ = session.enable_passwordless_sudo_collector("perf");
        self.attach_pid(session, pid)
    }
    fn process(
        &self,
        session: &mut Session,
        context: &ProcessContext<'_>,
    ) -> Result<ProcessData, String> {
        if !context.capture_complete
            || !super::perf::accepted_exit(context.status, context.controlled_interruption)
        {
            return Err("SPE capture did not finish with validated process/capture custody".into());
        }
        let state = session.state::<State>();
        let state = state.borrow();
        let root = state.root.ok_or("SPE target identity is missing")?;
        let raw = session.dir.join("arm-spe.data");
        let quality = quality::inspect(
            std::fs::File::open(&raw).map_err(|e| e.to_string())?,
            root.pid,
            state.options.budget_bytes + 16 * MIB,
        )?;
        let mut decoded = decoder::decode(session, &raw, &quality)?;
        let mut quality_document = serde_json::json!({
            "schema_version":1,"scope":"agent-process-tree","status":"observed",
            "root":{"pid":root.pid,"start_time_ticks":root.start_time_ticks},
            "period":state.options.period,"raw_record_quality":quality,
            "sample_counts":decoded.counts,"observed_tasks":decoded.observed_tasks,
        "memory_latency_weight_sum_cycles":decoded.memory_latency_weight_sum,
        "memory_latency_known_samples":decoded.memory_latency_known_samples,
            "first_sample_ns":decoded.first_sample_ns,"last_sample_ns":decoded.last_sample_ns,
        "clock":"PERF_TIME_CONV",
            "timestamp_precision_ns":1000,
            "series_origin":"first observed sample; one-second bins; final bin can be partial; absent bins are not invented zeroes",
            "limitations":"No reported loss/truncation flags, complete task metadata and a successful decoder are required. This cannot rule out sampling bias or unreported hardware loss. Counts are sampled observations, not full event counts or retired instructions. Memory weights are sampled operation latency in cycles, not isolated DRAM service time. Synthesized event populations overlap and must not be added. Ambiguous task reuse is rejected."
        });
        let alignment = measurements::phase_alignment::spe(
            &quality_document,
            &session.execution_window_document(),
        );
        if let Some(offset) = alignment.first_sample_offset_seconds {
            for times in decoded.timestamps.values_mut() {
                for time in times {
                    *time += offset;
                }
            }
            quality_document["series_origin"] = serde_json::json!(
                "verified native principal pre-release counter-clock bound; one-second bins anchored at first sample; absent bins are not invented zeroes"
            );
        }
        quality_document["phase_alignment"] =
            serde_json::to_value(alignment).map_err(|e| e.to_string())?;
        measurements::write_json(&session.dir.join("arm-spe-quality.json"), &quality_document)?;
        let mut result = ProcessData::from_output(context);
        result.status = "completed".into();
        result.metrics = decoded.metrics;
        result.series = decoded.series;
        result.timestamps = decoded.timestamps;
        result.artifact = Some("arm-spe-quality.json".into());
        Ok(result)
    }
    fn finish(&self, session: &mut Session) -> Result<(), String> {
        let state = session.state::<State>();
        let state = state.borrow();
        if state.selected && !state.started {
            measurements::write_json(
                &session.dir.join("arm-spe.json"),
                &serde_json::json!({
                    "schema_version":1,"collector":"arm-spe","scope":"agent-process-tree",
                    "status":"unavailable","reason":state.reason.as_deref().unwrap_or("No stopped agent measurement target was attached"),
                    "metrics":null
                }),
            )?;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn start(session: &mut Session, pid: u32) -> Result<(), String> {
    let state = session.state::<State>();
    let (pmu, options) = {
        let state = state.borrow();
        if !state.selected || state.started {
            return Ok(());
        }
        let Some(pmu) = state.pmu.clone() else {
            return Ok(());
        };
        (pmu, state.options.clone())
    };
    super::perf::validate_agent_target(pid, session.measurement_target())?;
    // Inheritance cannot retroactively include children created before attach.
    // The existing execution adapter must expose its stopped launch gate.
    use std::io::Read;
    let mut status = String::new();
    std::fs::File::open(format!("/proc/{pid}/status"))
        .map_err(|e| e.to_string())?
        .take(65537)
        .read_to_string(&mut status)
        .map_err(|e| e.to_string())?;
    if status.len() > 65536
        || !status
            .lines()
            .any(|s| s.starts_with("State:") && s.split_whitespace().nth(1) == Some("T"))
    {
        state.borrow_mut().reason=Some("SPE requires the verified stopped agent launch gate before it can fork; no running-target or host-wide fallback is allowed".into());
        return Ok(());
    }
    let directory = session.dir.canonicalize().map_err(|e| e.to_string())?;
    let mut ready =
        super::perf::readiness::Readiness::new(&directory, "arm-spe").map_err(|e| e.to_string())?;
    let raw = directory.join("arm-spe.data");
    super::perf::reserve_recording_artifact(&raw)?;
    let captures = [
        crate::evidence::Capture::create(&directory.join("arm-spe.stdout"))?,
        crate::evidence::Capture::create(&directory.join("arm-spe.stderr"))?,
    ];
    let (mut command, route) = session.collector_command("perf")?;
    command
        .args(args(&pmu, pid, &options))
        .arg("-o")
        .arg(&raw)
        .args(["-D", "-1", "--control", &ready.argument()]);
    let mut collector =
        measurements::Collector::spawn("arm-spe", &ADAPTER, command, route, captures)?;
    if let Err(reason) = ready.enable(Duration::from_secs(10), || collector.startup_exit()) {
        state.borrow_mut().reason =
            Some(collector.startup_failure(reason, session._directory_owner.clone()));
        return Ok(());
    }
    let mut state = state.borrow_mut();
    state.started = true;
    state.root = session.measurement_target();
    session.collectors.push(collector);
    Ok(())
}
