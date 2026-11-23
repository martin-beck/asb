//! Shared direct process-tree sampling for the CPU and I/O adapters.

use super::process_accounting::{Sample, SeriesSampler, TimedSample};
use crate::collectors::LifecycleEvent;
use crate::measurements::{self, ProcessIdentity};
use std::collections::BTreeMap;
use std::time::Duration;

mod parsing;
pub(super) use parsing::{Samples, parse_columns, parse_samples};

#[derive(Default)]
pub(super) struct State {
    pub(super) cpu_enabled: bool,
    pub(super) io_enabled: bool,
    sampler: Option<SeriesSampler>,
    samples: Option<Vec<TimedSample>>,
    cgroup_io_sampler: Option<super::kernel_pressure::capture::Sampler>,
    cgroup_io_capture: Option<super::kernel_pressure::capture::Capture>,
    cgroup_io_scope: Option<&'static str>,
    cgroup_io_reason: Option<String>,
    reason: Option<String>,
    task_delayacct_enabled: bool,
}

pub(super) fn activate(session: &mut measurements::Session, pid: u32) {
    let state = session.state::<State>();
    let mut state = state.borrow_mut();
    if (!state.cpu_enabled && !state.io_enabled)
        || state.sampler.is_some()
        || state.samples.is_some()
    {
        return;
    }
    let root = session
        .measurement_target()
        .or_else(|| ProcessIdentity::read(pid));
    state.task_delayacct_enabled = super::process_accounting::task_delayacct_enabled();
    state.sampler = root
        .and_then(|root| SeriesSampler::start_cpu(root, measurements::measurement_interval_ms()));
    if state.sampler.is_none() {
        state.reason = Some("cannot start direct /proc process-tree sampler".into());
    }
}

pub(super) fn lifecycle(session: &mut measurements::Session, event: LifecycleEvent) {
    if event == LifecycleEvent::AgentExecuteBegin {
        activate_cgroup_io(session);
    }
    if matches!(
        event,
        LifecycleEvent::AgentExecuteEnd | LifecycleEvent::RunFailed
    ) {
        settle(session);
    }
}

pub(super) fn write_cpu(session: &mut measurements::Session) -> Result<(), String> {
    settle(session);
    let state = session.state::<State>();
    let state = state.borrow();
    if !state.cpu_enabled {
        return Ok(());
    }
    let (metrics, series, timestamps) = state
        .samples
        .as_deref()
        .and_then(|samples| {
            session
                .clock_ticks_per_second()
                .map(|hz| cpu_values(samples, hz))
        })
        .unwrap_or_default();
    write_result(
        session,
        "mpstat",
        ResultData {
            source: "direct-procfs-process-tree",
            scope: "verified-agent-process-tree",
            reason: state.reason.clone(),
            metrics,
            series,
            timestamps,
            unavailable: BTreeMap::new(),
        },
    )
}

pub(super) fn write_io(session: &mut measurements::Session) -> Result<(), String> {
    settle(session);
    let state = session.state::<State>();
    let state = state.borrow();
    if !state.io_enabled {
        return Ok(());
    }
    let mut result = if let Some(capture) = state.cgroup_io_capture.as_ref() {
        let cgroup = super::kernel_pressure::capture::io_throughput(capture);
        let mut unavailable = cgroup.unavailable;
        unavailable.insert(
            "process.io.cancelled_write_kib_per_s".into(),
            "verified cgroup-v2 io.stat does not expose cancelled-write bytes".into(),
        );
        unavailable.insert(
            "process.io.delay_ticks".into(),
            "verified cgroup-v2 io.stat does not expose per-task I/O delay ticks".into(),
        );
        ResultData {
            source: "verified-cgroup-v2-io",
            scope: state
                .cgroup_io_scope
                .unwrap_or("verified-cgroup-v2-execution"),
            reason: state.cgroup_io_reason.clone(),
            metrics: cgroup.metrics,
            series: cgroup.series,
            timestamps: cgroup.timestamps,
            unavailable,
        }
    } else {
        let (metrics, series, timestamps) = state
            .samples
            .as_deref()
            .map(|samples| io_values(samples, state.task_delayacct_enabled))
            .unwrap_or_default();
        ResultData {
            source: "direct-procfs-process-tree",
            scope: "verified-agent-process-tree",
            reason: state.reason.clone(),
            metrics,
            series,
            timestamps,
            unavailable: BTreeMap::new(),
        }
    };
    if result.source == "direct-procfs-process-tree" && !state.task_delayacct_enabled {
        let delayacct_reason = "kernel.task_delayacct is disabled or unreadable; process.io.delay_ticks is unavailable. Run `asb init --configure-host` and approve kernel.task_delayacct=1 before the next benchmark.";
        match &mut result.reason {
            Some(reason) => {
                reason.push(' ');
                reason.push_str(delayacct_reason);
            }
            None => result.reason = Some(delayacct_reason.into()),
        }
    }
    write_result(session, "iostat", result)
}

fn activate_cgroup_io(session: &mut measurements::Session) {
    let enabled = {
        let state = session.state::<State>();
        let state = state.borrow();
        state.io_enabled && state.cgroup_io_sampler.is_none() && state.cgroup_io_capture.is_none()
    };
    if !enabled {
        return;
    }
    let Some(scope) = super::cpu_accounting::pressure_scope(session) else {
        return;
    };
    let (path, scope) = match scope {
        super::cpu_accounting::PressureScope::ExclusiveAgent(path) => {
            (path, "exclusive-agent-process-tree")
        }
        super::cpu_accounting::PressureScope::OwnedContainer(path) => {
            (path, "owned-container-execution")
        }
    };
    let state = session.state::<State>();
    let mut state = state.borrow_mut();
    match super::kernel_pressure::capture::Sampler::start(
        path,
        measurements::measurement_interval_ms(),
    ) {
        Ok(sampler) => {
            state.cgroup_io_sampler = Some(sampler);
            state.cgroup_io_scope = Some(scope);
        }
        Err(reason) => state.cgroup_io_reason = Some(reason),
    }
}

/// Finalization can be reached without an execution-end lifecycle event when
/// an adapter fails early. Settle here too so the other shared collector sees
/// the same bounded sample set and the sampling thread is never left behind.
fn settle(session: &mut measurements::Session) {
    let state = session.state::<State>();
    let mut state = state.borrow_mut();
    if let Some(sampler) = state.sampler.take() {
        state.samples = sampler.stop();
        if state.samples.is_none() {
            state.reason = Some("direct /proc sampler did not settle".into());
        }
    }
    if let Some(sampler) = state.cgroup_io_sampler.take() {
        match sampler.stop() {
            Ok(capture) => state.cgroup_io_capture = Some(capture),
            Err(reason) => state.cgroup_io_reason = Some(reason),
        }
    }
}

type Values = (
    BTreeMap<String, f64>,
    BTreeMap<String, Vec<f64>>,
    BTreeMap<String, Vec<f64>>,
);

struct ResultData {
    source: &'static str,
    scope: &'static str,
    reason: Option<String>,
    metrics: BTreeMap<String, f64>,
    series: BTreeMap<String, Vec<f64>>,
    timestamps: BTreeMap<String, Vec<f64>>,
    unavailable: BTreeMap<String, String>,
}

fn cpu_values(samples: &[TimedSample], ticks_per_second: u64) -> Values {
    values(samples, |sample, seconds| {
        let scale = 100.0 / ticks_per_second as f64 / seconds;
        [
            ("process.cpu.user_pct", sample.user_ticks as f64 * scale),
            ("process.cpu.system_pct", sample.system_ticks as f64 * scale),
            (
                "process.cpu.total_pct",
                (sample.user_ticks + sample.system_ticks) as f64 * scale,
            ),
        ]
    })
}

fn io_values(samples: &[TimedSample], task_delayacct_enabled: bool) -> Values {
    let mut result = values(samples, |sample, seconds| {
        [
            (
                "process.io.read_kib_per_s",
                sample.read_bytes as f64 / 1024.0 / seconds,
            ),
            (
                "process.io.write_kib_per_s",
                sample.write_bytes as f64 / 1024.0 / seconds,
            ),
            (
                "process.io.cancelled_write_kib_per_s",
                sample.cancelled_write_bytes as f64 / 1024.0 / seconds,
            ),
        ]
    });
    if task_delayacct_enabled {
        let (metrics, series, timestamps) = values(samples, |sample, _| {
            [("process.io.delay_ticks", sample.io_delay_ticks as f64)]
        });
        result.0.extend(metrics);
        result.1.extend(series);
        result.2.extend(timestamps);
    }
    result
}

fn values<const N: usize>(
    samples: &[TimedSample],
    fields: impl Fn(Sample, f64) -> [(&'static str, f64); N],
) -> Values {
    let mut series = BTreeMap::new();
    let mut timestamps = BTreeMap::new();
    let mut totals = BTreeMap::<String, (f64, usize)>::new();
    let mut previous = Duration::ZERO;
    for sample in samples {
        let seconds = sample.elapsed.saturating_sub(previous).as_secs_f64();
        previous = sample.elapsed;
        if seconds <= f64::EPSILON {
            continue;
        }
        for (name, value) in fields(sample.delta, seconds) {
            series
                .entry(name.into())
                .or_insert_with(Vec::new)
                .push(value);
            timestamps
                .entry(name.into())
                .or_insert_with(Vec::new)
                .push(sample.elapsed.as_secs_f64() * 1_000.0);
            let entry = totals.entry(name.into()).or_default();
            entry.0 += value;
            entry.1 += 1;
        }
    }
    let metrics = totals
        .into_iter()
        .filter_map(|(name, (sum, count))| (count > 0).then_some((name, sum / count as f64)))
        .collect();
    (metrics, series, timestamps)
}

fn write_result(
    session: &measurements::Session,
    collector: &str,
    result: ResultData,
) -> Result<(), String> {
    measurements::write_json(
        &session.dir.join(format!("{collector}.json")),
        &serde_json::json!({
            "schema_version": 1,
            "collector": collector,
            "status": if result.metrics.is_empty() { "unavailable" } else if result.unavailable.is_empty() { "completed" } else { "partial" },
            "source": result.source,
            "scope": result.scope,
            "reason": result.reason,
            "metrics": result.metrics,
            "series": result.series,
            "series_timestamps": result.timestamps,
            "series_timestamp_unit": "milliseconds",
            "unavailable": result.unavailable,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collectors::CollectorAdapter;
    use crate::measurements::Session;
    use std::fs;

    fn sample(seconds: u64, delta: Sample) -> TimedSample {
        TimedSample {
            elapsed: Duration::from_secs(seconds),
            delta,
        }
    }

    #[test]
    fn direct_proc_cpu_samples_publish_rates_and_timestamps() {
        let (metrics, series, timestamps) = cpu_values(
            &[sample(
                2,
                Sample {
                    user_ticks: 100,
                    system_ticks: 50,
                    ..Sample::default()
                },
            )],
            100,
        );
        assert_eq!(metrics["process.cpu.user_pct"], 50.0);
        assert_eq!(metrics["process.cpu.system_pct"], 25.0);
        assert_eq!(metrics["process.cpu.total_pct"], 75.0);
        assert_eq!(series["process.cpu.total_pct"], vec![75.0]);
        assert_eq!(timestamps["process.cpu.total_pct"], vec![2_000.0]);
    }

    #[test]
    fn direct_proc_io_samples_publish_throughput_and_delay() {
        let (metrics, series, timestamps) = io_values(
            &[sample(
                2,
                Sample {
                    read_bytes: 2_048,
                    write_bytes: 1_024,
                    cancelled_write_bytes: 512,
                    io_delay_ticks: 7,
                    ..Sample::default()
                },
            )],
            true,
        );
        assert_eq!(metrics["process.io.read_kib_per_s"], 1.0);
        assert_eq!(metrics["process.io.write_kib_per_s"], 0.5);
        assert_eq!(metrics["process.io.cancelled_write_kib_per_s"], 0.25);
        assert_eq!(metrics["process.io.delay_ticks"], 7.0);
        assert_eq!(series["process.io.delay_ticks"], vec![7.0]);
        assert_eq!(timestamps["process.io.delay_ticks"], vec![2_000.0]);
    }

    #[test]
    fn disabled_task_delayacct_omits_iostat_delay_ticks() {
        let (metrics, series, timestamps) = io_values(
            &[sample(
                2,
                Sample {
                    read_bytes: 2_048,
                    io_delay_ticks: 7,
                    ..Sample::default()
                },
            )],
            false,
        );
        assert_eq!(metrics["process.io.read_kib_per_s"], 1.0);
        assert!(!metrics.contains_key("process.io.delay_ticks"));
        assert!(!series.contains_key("process.io.delay_ticks"));
        assert!(!timestamps.contains_key("process.io.delay_ticks"));
    }

    #[test]
    fn iostat_publishes_verified_cgroup_throughput_without_process_zeroes() {
        let root =
            crate::fixture_namespace::allocate(&std::env::temp_dir(), "asb-iostat-").unwrap();
        let cgroup = root.join("cgroup");
        fs::create_dir(&cgroup).unwrap();
        fs::write(
            cgroup.join("io.stat"),
            "8:0 rbytes=1024 wbytes=2048 rios=1 wios=2\n",
        )
        .unwrap();
        let sampler =
            super::super::kernel_pressure::capture::Sampler::start(cgroup, 60_000).unwrap();
        fs::write(
            root.join("cgroup/io.stat"),
            "8:0 rbytes=3072 wbytes=10240 rios=4 wios=6\n",
        )
        .unwrap();
        let capture = sampler.stop().unwrap();
        let selected = ["iostat".into()];
        let mut session = Session::start_with_collectors(&root, &selected, None).unwrap();
        {
            let state = session.state::<State>();
            let mut state = state.borrow_mut();
            state.io_enabled = true;
            state.cgroup_io_capture = Some(capture);
            state.cgroup_io_scope = Some("owned-container-execution");
        }
        write_io(&mut session).unwrap();
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("iostat.json")).unwrap()).unwrap();
        assert_eq!(report["status"], "partial");
        assert_eq!(report["source"], "verified-cgroup-v2-io");
        assert_eq!(report["scope"], "owned-container-execution");
        assert!(
            report["metrics"]["container.io.read_kib_per_s"]
                .as_f64()
                .is_some_and(|value| value > 0.0)
        );
        assert!(
            report["metrics"]["container.io.write_kib_per_s"]
                .as_f64()
                .is_some_and(|value| value > 0.0)
        );
        assert!(report["metrics"].get("process.io.read_kib_per_s").is_none());
        assert!(
            report["unavailable"]["process.io.cancelled_write_kib_per_s"]
                .as_str()
                .unwrap()
                .contains("does not expose")
        );
        drop(session);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_direct_sampler_publishes_cpu_and_io_results() {
        let dir = std::env::temp_dir().join(format!(
            "asb-direct-procfs-{}",
            crate::tools::runtime::unique_id()
        ));
        let selected = ["mpstat".into(), "iostat".into()];
        let mut session = Session::start_with_collectors(&dir, &selected, None).unwrap();
        session.attach_pid(std::process::id()).unwrap();
        session.event(LifecycleEvent::AgentExecuteBegin).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        session.event(LifecycleEvent::AgentExecuteEnd).unwrap();
        super::super::mpstat::ADAPTER.finish(&mut session).unwrap();
        super::super::iostat::ADAPTER.finish(&mut session).unwrap();
        for collector in ["mpstat", "iostat"] {
            let report: serde_json::Value =
                serde_json::from_slice(&fs::read(dir.join(format!("{collector}.json"))).unwrap())
                    .unwrap();
            assert_eq!(report["status"], "completed");
            assert_eq!(report["source"], "direct-procfs-process-tree");
            assert!(
                report["metrics"]
                    .as_object()
                    .is_some_and(|metrics| !metrics.is_empty())
            );
            assert!(report["series_timestamps"].is_object());
        }
        drop(session);
        fs::remove_dir_all(dir).unwrap();
    }
}
