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
    reason: Option<String>,
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
    state.sampler =
        root.and_then(|root| SeriesSampler::start(root, measurements::measurement_interval_ms()));
    if state.sampler.is_none() {
        state.reason = Some("cannot start direct /proc process-tree sampler".into());
    }
}

pub(super) fn lifecycle(session: &mut measurements::Session, event: LifecycleEvent) {
    if !matches!(
        event,
        LifecycleEvent::AgentExecuteEnd | LifecycleEvent::RunFailed
    ) {
        return;
    }
    settle(session);
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
        state.reason.as_deref(),
        metrics,
        series,
        timestamps,
    )
}

pub(super) fn write_io(session: &mut measurements::Session) -> Result<(), String> {
    settle(session);
    let state = session.state::<State>();
    let state = state.borrow();
    if !state.io_enabled {
        return Ok(());
    }
    let (metrics, series, timestamps) = state.samples.as_deref().map(io_values).unwrap_or_default();
    write_result(
        session,
        "iostat",
        state.reason.as_deref(),
        metrics,
        series,
        timestamps,
    )
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
}

type Values = (
    BTreeMap<String, f64>,
    BTreeMap<String, Vec<f64>>,
    BTreeMap<String, Vec<f64>>,
);

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

fn io_values(samples: &[TimedSample]) -> Values {
    values(samples, |sample, seconds| {
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
            ("process.io.delay_ticks", sample.io_delay_ticks as f64),
        ]
    })
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
    reason: Option<&str>,
    metrics: BTreeMap<String, f64>,
    series: BTreeMap<String, Vec<f64>>,
    timestamps: BTreeMap<String, Vec<f64>>,
) -> Result<(), String> {
    measurements::write_json(
        &session.dir.join(format!("{collector}.json")),
        &serde_json::json!({
            "schema_version": 1,
            "collector": collector,
            "status": if metrics.is_empty() { "unavailable" } else { "completed" },
            "source": "direct-procfs-process-tree",
            "reason": reason,
            "metrics": metrics,
            "series": series,
            "series_timestamps": timestamps,
            "series_timestamp_unit": "milliseconds",
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
        let (metrics, series, timestamps) = io_values(&[sample(
            2,
            Sample {
                read_bytes: 2_048,
                write_bytes: 1_024,
                cancelled_write_bytes: 512,
                io_delay_ticks: 7,
                ..Sample::default()
            },
        )]);
        assert_eq!(metrics["process.io.read_kib_per_s"], 1.0);
        assert_eq!(metrics["process.io.write_kib_per_s"], 0.5);
        assert_eq!(metrics["process.io.cancelled_write_kib_per_s"], 0.25);
        assert_eq!(metrics["process.io.delay_ticks"], 7.0);
        assert_eq!(series["process.io.delay_ticks"], vec![7.0]);
        assert_eq!(timestamps["process.io.delay_ticks"], vec![2_000.0]);
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
