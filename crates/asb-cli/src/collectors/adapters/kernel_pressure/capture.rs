use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_POINTS: usize = 4096;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const LIMITATIONS: &str = "PSI is cgroup stall occupancy, not accumulated thread time or additive wall-time loss. Some/full and different resource stalls overlap. Throttled time can overlap across CPUs. Charged memory excludes pages charged to other cgroups. I/O counters are cgroup block accounting, not filesystem latency or network traffic. Capture starts once both agent attachment and execution-begin have been observed and stops at the execution-end hook; boundary hook overhead remains possible. Host pressure is never a fallback. A verified owned-container scope can include workload runtime helpers and is labelled separately from an exclusive agent-process-tree scope.";

#[derive(Clone, Default, Serialize)]
pub(super) struct Snapshot {
    values: BTreeMap<String, u64>,
    unavailable: BTreeMap<String, String>,
}

pub(super) struct Capture {
    points: Vec<(u64, Snapshot)>,
    unavailable: BTreeMap<String, String>,
    peaks: BTreeMap<String, u64>,
    downsampled: bool,
    samples: u64,
    sample_read_us_total: u64,
    sample_read_us_max: u64,
    interval_jitter_us_max: u64,
    schedule_lateness_us_max: u64,
    missed_scheduled_intervals: u64,
    previous_scheduled_sample_us: Option<u64>,
    interval_us: u64,
}

impl Capture {
    fn new(initial: Snapshot, initial_read_us: u64, interval_us: u64) -> Self {
        let mut capture = Self {
            points: Vec::new(),
            unavailable: BTreeMap::new(),
            peaks: BTreeMap::new(),
            downsampled: false,
            samples: 0,
            sample_read_us_total: 0,
            sample_read_us_max: 0,
            interval_jitter_us_max: 0,
            schedule_lateness_us_max: 0,
            missed_scheduled_intervals: 0,
            previous_scheduled_sample_us: None,
            interval_us,
        };
        capture.push(0, 0, initial, initial_read_us, None);
        capture
    }
    fn push(
        &mut self,
        sample_started_us: u64,
        elapsed_us: u64,
        point: Snapshot,
        read_us: u64,
        scheduled_due_us: Option<u64>,
    ) {
        self.samples = self.samples.saturating_add(1);
        self.sample_read_us_total = self.sample_read_us_total.saturating_add(read_us);
        self.sample_read_us_max = self.sample_read_us_max.max(read_us);
        if let Some(due_us) = scheduled_due_us {
            self.schedule_lateness_us_max = self
                .schedule_lateness_us_max
                .max(sample_started_us.saturating_sub(due_us));
            if let Some(previous) = self.previous_scheduled_sample_us {
                self.interval_jitter_us_max = self.interval_jitter_us_max.max(
                    sample_started_us
                        .saturating_sub(previous)
                        .abs_diff(self.interval_us),
                );
            }
            self.previous_scheduled_sample_us = Some(sample_started_us);
        }
        self.unavailable.extend(point.unavailable.clone());
        if let Some((_, previous)) = self.points.last() {
            for (key, before) in &previous.values {
                if key != "memory.current_bytes"
                    && point.values.get(key).is_some_and(|after| after < before)
                {
                    self.unavailable
                        .insert(key.clone(), "counter decreased during execution".into());
                }
            }
        }
        if let Some(value) = point.values.get("memory.current_bytes") {
            self.peaks
                .entry("memory.sampled_peak_bytes".into())
                .and_modify(|peak| *peak = (*peak).max(*value))
                .or_insert(*value);
        }
        if self.points.len() == MAX_POINTS {
            self.points = self
                .points
                .drain(..)
                .enumerate()
                .filter_map(|(i, p)| i.is_multiple_of(2).then_some(p))
                .collect();
            self.downsampled = true;
        }
        self.points.push((elapsed_us, point));
    }

    fn record_missed_scheduled_intervals(&mut self, missed: u64) {
        self.missed_scheduled_intervals = self.missed_scheduled_intervals.saturating_add(missed);
    }

    fn overhead(&self) -> serde_json::Value {
        serde_json::json!({
            "sample_count": self.samples,
            "sample_read_us_total": self.sample_read_us_total,
            "sample_read_us_max": self.sample_read_us_max,
            "interval_jitter_us_max": self.interval_jitter_us_max,
            "schedule_lateness_us_max": self.schedule_lateness_us_max,
            "missed_scheduled_intervals": self.missed_scheduled_intervals,
            "configured_interval_us": self.interval_us,
        })
    }
}

pub(super) struct Sampler {
    stop: Sender<()>,
    handle: Option<JoinHandle<Capture>>,
}
impl Sampler {
    pub(super) fn start(path: PathBuf, interval_ms: u64) -> Result<Self, String> {
        let initial_started = Instant::now();
        let initial = snapshot(&path);
        let initial_read_us = initial_started.elapsed().as_micros().min(u64::MAX as u128) as u64;
        if initial.values.is_empty() {
            return Err(
                "Exclusive agent cgroup exposes no readable pressure/accounting fields".into(),
            );
        }
        let started = Instant::now();
        let (stop, receiver) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("asb-agent-pressure".into())
            .spawn(move || {
                let mut capture =
                    Capture::new(initial, initial_read_us, interval_ms.saturating_mul(1_000));
                let interval = Duration::from_millis(interval_ms);
                let mut next_deadline = started + interval;
                loop {
                    let stopped = !matches!(
                        receiver
                            .recv_timeout(next_deadline.saturating_duration_since(Instant::now())),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    );
                    let read_started = Instant::now();
                    let point = snapshot(&path);
                    let elapsed_us = started.elapsed().as_micros().min(u64::MAX as u128) as u64;
                    let sample_started_us = read_started
                        .saturating_duration_since(started)
                        .as_micros()
                        .min(u64::MAX as u128) as u64;
                    let read_us = read_started.elapsed().as_micros().min(u64::MAX as u128) as u64;
                    let due_us = next_deadline
                        .saturating_duration_since(started)
                        .as_micros()
                        .min(u64::MAX as u128) as u64;
                    capture.push(
                        sample_started_us,
                        elapsed_us,
                        point,
                        read_us,
                        (!stopped).then_some(due_us),
                    );
                    if stopped {
                        break;
                    }
                    let (advanced, missed) =
                        advance_deadline(next_deadline, Instant::now(), interval);
                    next_deadline = advanced;
                    capture.record_missed_scheduled_intervals(missed);
                }
                capture
            })
            .map_err(|error| format!("Cannot start agent pressure sampler: {error}"))?;
        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
    pub(super) fn stop(mut self) -> Result<Capture, String> {
        let _ = self.stop.send(());
        self.handle
            .take()
            .expect("owned sampler")
            .join()
            .map_err(|_| "Agent pressure sampler failed".into())
    }
}
impl Drop for Sampler {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn advance_deadline(
    mut deadline: Instant,
    observed: Instant,
    interval: Duration,
) -> (Instant, u64) {
    deadline += interval;
    let mut missed = 0_u64;
    while deadline <= observed {
        deadline += interval;
        missed = missed.saturating_add(1);
    }
    (deadline, missed)
}

fn read(path: &Path) -> Result<String, String> {
    let mut text = String::new();
    File::open(path)
        .map_err(|_| "field is not readable")?
        .take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| "field is not valid text")?;
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err("field exceeds capture bound".into());
    }
    Ok(text)
}

fn pairs(text: &str) -> Result<BTreeMap<String, u64>, String> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2
            || values
                .insert(
                    fields[0].into(),
                    fields[1].parse().map_err(|_| "invalid integer counter")?,
                )
                .is_some()
        {
            return Err("invalid or duplicate counter".into());
        }
    }
    Ok(values)
}

fn psi(text: &str) -> Result<BTreeMap<String, u64>, String> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let kind = parts.next().ok_or("empty PSI row")?;
        if !matches!(kind, "some" | "full") {
            return Err("unknown PSI row".into());
        }
        let mut fields = BTreeMap::new();
        for item in parts {
            let (key, value) = item.split_once('=').ok_or("invalid PSI field")?;
            if fields.insert(key, value).is_some() {
                return Err("duplicate PSI field".into());
            }
        }
        if fields.len() != 4 {
            return Err("incomplete PSI row".into());
        }
        for key in ["avg10", "avg60", "avg300"] {
            let value = fields
                .get(key)
                .ok_or("missing PSI average")?
                .parse::<f64>()
                .map_err(|_| "invalid PSI average")?;
            if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                return Err("invalid PSI average".into());
            }
        }
        let total = fields
            .get("total")
            .ok_or("missing PSI total")?
            .parse()
            .map_err(|_| "invalid PSI total")?;
        if values.insert(format!("{kind}_total_us"), total).is_some() {
            return Err("duplicate PSI row".into());
        }
    }
    if !values.contains_key("some_total_us") {
        return Err("missing PSI some row".into());
    }
    Ok(values)
}

fn io(text: &str) -> Result<BTreeMap<String, u64>, String> {
    let mut totals = BTreeMap::<String, u64>::new();
    let mut devices = std::collections::BTreeSet::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let device = fields.next().ok_or("missing block device")?;
        let (major, minor) = device.split_once(':').ok_or("invalid block device")?;
        major.parse::<u32>().map_err(|_| "invalid block device")?;
        minor.parse::<u32>().map_err(|_| "invalid block device")?;
        if !devices.insert(device) {
            return Err("duplicate block device".into());
        }
        let mut values = BTreeMap::new();
        for field in fields {
            let (key, value) = field.split_once('=').ok_or("invalid I/O field")?;
            let value = value.parse::<u64>().map_err(|_| "invalid I/O counter")?;
            if values.insert(key, value).is_some() {
                return Err("duplicate I/O field".into());
            }
        }
        for key in ["rbytes", "wbytes", "rios", "wios"] {
            let value = values.get(key).ok_or("missing I/O counter")?;
            let total = totals.entry(key.into()).or_default();
            *total = total.checked_add(*value).ok_or("I/O counter overflow")?;
        }
    }
    // Empty io.stat is the kernel's closed-set representation of no devices
    // charged yet. Only a successfully read empty file authorizes zero.
    for key in ["rbytes", "wbytes", "rios", "wios"] {
        totals.entry(key.into()).or_default();
    }
    Ok(totals)
}

fn snapshot(path: &Path) -> Snapshot {
    let mut out = Snapshot::default();
    let mut load = |file: &str,
                    fields: &[(&str, &str)],
                    parser: fn(&str) -> Result<BTreeMap<String, u64>, String>| {
        match read(&path.join(file)).and_then(|text| parser(&text)) {
            Ok(values) => {
                for (source, key) in fields {
                    match values.get(*source) {
                        Some(value) => {
                            out.values.insert((*key).into(), *value);
                        }
                        None => {
                            out.unavailable
                                .insert((*key).into(), "counter is not exposed".into());
                        }
                    }
                }
            }
            Err(reason) => {
                for (_, key) in fields {
                    let reason = if file.ends_with(".pressure") {
                        format!(
                            "{file}: {reason}; enable kernel PSI (psi=1 on PSI-default-disabled kernels) in the measurement environment"
                        )
                    } else {
                        format!(
                            "{file}: {reason}; verify that the delegated cgroup exposes the required controller"
                        )
                    };
                    out.unavailable.insert((*key).into(), reason);
                }
            }
        }
    };
    for (file, resource) in [
        ("cpu.pressure", "cpu"),
        ("memory.pressure", "memory"),
        ("io.pressure", "io"),
    ] {
        let some = format!("{resource}.some_total_us");
        let full = format!("{resource}.full_total_us");
        load(
            file,
            &[("some_total_us", &some), ("full_total_us", &full)],
            psi,
        );
    }
    load(
        "cpu.stat",
        &[
            ("nr_periods", "cpu.period_count"),
            ("nr_throttled", "cpu.throttled_period_count"),
            ("throttled_usec", "cpu.throttled_total_us"),
        ],
        pairs,
    );
    load(
        "memory.events",
        &[
            ("high", "memory.high_count"),
            ("max", "memory.max_count"),
            ("oom", "memory.oom_count"),
            ("oom_kill", "memory.oom_kill_count"),
        ],
        pairs,
    );
    load(
        "memory.current",
        &[("current", "memory.current_bytes")],
        |text| {
            Ok(BTreeMap::from([(
                "current".into(),
                text.trim().parse().map_err(|_| "invalid memory gauge")?,
            )]))
        },
    );
    load(
        "io.stat",
        &[
            ("rbytes", "io.read_bytes"),
            ("wbytes", "io.write_bytes"),
            ("rios", "io.read_count"),
            ("wios", "io.write_count"),
        ],
        io,
    );
    out
}

fn converted(key: &str, delta: u64, wall_us: u64) -> (String, f64) {
    if let Some(prefix) = key.strip_suffix("_total_us") {
        (
            format!("{prefix}_stall_pct"),
            delta as f64 / wall_us as f64 * 100.0,
        )
    } else {
        (
            format!("{key}_per_s"),
            delta as f64 / wall_us as f64 * 1_000_000.0,
        )
    }
}

pub(super) fn report(
    capture: Option<&Capture>,
    reason: Option<&str>,
    scope: super::PressureScope,
) -> (serde_json::Value, serde_json::Value) {
    let mut metrics = BTreeMap::<String, f64>::new();
    let mut series = BTreeMap::<String, Vec<f64>>::new();
    let mut timestamps = BTreeMap::<String, Vec<f64>>::new();
    let mut unavailable = BTreeMap::new();
    let mut duration_us = None;
    if let Some(capture) = capture {
        unavailable.clone_from(&capture.unavailable);
        if let (Some((_, initial)), Some((end, final_value))) =
            (capture.points.first(), capture.points.last())
        {
            duration_us = Some(*end);
            if *end > 0 {
                for (key, before) in &initial.values {
                    if unavailable.contains_key(key) {
                        continue;
                    }
                    if let Some(after) = final_value.values.get(key) {
                        if key == "memory.current_bytes" {
                            metrics.insert(key.clone(), *after as f64);
                        } else if let Some(delta) = after.checked_sub(*before) {
                            let (id, value) = converted(key, delta, *end);
                            // Throttled duration is accumulated across CPUs;
                            // it must not be mislabeled PSI occupancy.
                            if key == "cpu.throttled_total_us" {
                                metrics.insert("cpu.throttled_ms".into(), delta as f64 / 1000.0);
                            } else {
                                metrics.insert(id, value);
                            }
                            if key == "cpu.throttled_total_us" { /* Already retained as accumulated milliseconds. */
                            } else if let Some(prefix) = key.strip_suffix("_total_us") {
                                metrics.insert(format!("{prefix}_stall_ms"), delta as f64 / 1000.0);
                            } else {
                                metrics.insert(key.clone(), delta as f64);
                            }
                        }
                    }
                }
                metrics.extend(
                    capture
                        .peaks
                        .iter()
                        .map(|(key, value)| (key.clone(), *value as f64)),
                );
            }
        }
        for pair in capture.points.windows(2) {
            let (start, before) = &pair[0];
            let (end, after) = &pair[1];
            let Some(wall) = end.checked_sub(*start).filter(|v| *v > 0) else {
                continue;
            };
            for (key, value) in &after.values {
                if unavailable.contains_key(key) {
                    continue;
                }
                let sample = if key == "memory.current_bytes" {
                    Some((key.clone(), *value as f64))
                } else {
                    before
                        .values
                        .get(key)
                        .and_then(|previous| value.checked_sub(*previous))
                        .map(|delta| {
                            if key == "cpu.throttled_total_us" {
                                (
                                    "cpu.throttled_ms_per_s".into(),
                                    delta as f64 / wall as f64 * 1000.0,
                                )
                            } else {
                                converted(key, delta, wall)
                            }
                        })
                };
                if let Some((id, value)) = sample {
                    let id = format!("kernel-pressure.{id}");
                    series.entry(id.clone()).or_default().push(value);
                    timestamps
                        .entry(id)
                        .or_default()
                        .push(*end as f64 / 1_000_000.0);
                }
            }
        }
    }
    let summary = serde_json::json!({
        "schema_version":1, "collector":"kernel-pressure", "scope":scope.scope(),
        "scope_source":scope.source(), "status":if metrics.is_empty() {"unavailable"} else if unavailable.is_empty() {"completed"} else {"partial"},
        "reason":reason.or(if metrics.is_empty() {Some("No complete agent-only execution interval was observed")} else {None}),
        "duration_us":duration_us, "observations":metrics, "unavailable":unavailable,
        "limitations":LIMITATIONS,
        "collector_overhead":capture.map(Capture::overhead),
    });
    let mut report = summary.clone();
    report["metrics"] = report
        .as_object_mut()
        .unwrap()
        .remove("observations")
        .unwrap();
    report["series"] = serde_json::json!(series);
    report["series_timestamps"] = serde_json::json!(timestamps);
    report["series_timestamp_unit"] = serde_json::json!("seconds");
    report["clock"] =
        serde_json::json!("monotonic-elapsed-seconds-since-agent-pressure-attachment");
    report["series_downsampled"] = serde_json::json!(capture.is_some_and(|c| c.downsampled));
    report["raw_initial"] = serde_json::json!(capture.and_then(|c| c.points.first()).map(|p| &p.1));
    report["raw_final"] = serde_json::json!(capture.and_then(|c| c.points.last()).map(|p| &p.1));
    (report, summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn point(values: &[(&str, u64)]) -> Snapshot {
        Snapshot {
            values: values.iter().map(|(k, v)| ((*k).into(), *v)).collect(),
            unavailable: BTreeMap::new(),
        }
    }
    #[test]
    fn psi_rejects_bad_rows_and_keeps_exact_integer_counters() {
        let row = "some avg10=0.00 avg60=1.01 avg300=2.02 total=9007199254740993\n";
        assert_eq!(psi(row).unwrap()["some_total_us"], 9007199254740993);
        for bad in [
            "",
            "some total=10",
            "some avg10=NaN avg60=0 avg300=0 total=1",
            "some avg10=101 avg60=0 avg300=0 total=1",
            "other avg10=0 avg60=0 avg300=0 total=1",
            "some avg10=0 avg10=0 avg300=0 total=1",
        ] {
            assert!(psi(bad).is_err(), "{bad}");
        }
        assert!(psi(&format!("{row}{row}")).is_err());
    }
    #[test]
    fn block_counters_require_complete_devices_and_checked_sums() {
        assert_eq!(io("").unwrap()["rbytes"], 0);
        assert_eq!(
            io("8:0 rbytes=5 wbytes=6 rios=1 wios=2\n8:1 rbytes=7 wbytes=8 rios=3 wios=4").unwrap()
                ["rbytes"],
            12
        );
        for bad in [
            "8:0 rbytes=1",
            "bad rbytes=1 wbytes=1 rios=1 wios=1",
            "8:0 rbytes=1 rbytes=2 wbytes=1 rios=1 wios=1",
            "8:0 rbytes=1 wbytes=1 rios=1 wios=1\n8:0 rbytes=1 wbytes=1 rios=1 wios=1",
            "8:0 rbytes=18446744073709551615 wbytes=1 rios=1 wios=1\n8:1 rbytes=1 wbytes=1 rios=1 wios=1",
        ] {
            assert!(io(bad).is_err(), "{bad}");
        }
        assert!(pairs("high 1\nhigh 2").is_err());
        assert!(pairs("high 1 extra").is_err());
    }
    #[test]
    fn missing_files_are_unavailable_not_host_fallback_or_zero() {
        let path =
            crate::fixture_namespace::allocate(&std::env::temp_dir(), "asb-pressure-missing-")
                .unwrap();
        let snapshot = snapshot(&path);
        assert!(snapshot.values.is_empty());
        assert!(snapshot.unavailable.contains_key("cpu.some_total_us"));
        assert!(Sampler::start(path.clone(), 100).is_err());
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn counters_use_real_intervals_and_thread_throttling_is_not_wall_occupancy() {
        let mut capture = Capture::new(
            point(&[
                ("cpu.some_total_us", 100),
                ("cpu.throttled_total_us", 0),
                ("memory.current_bytes", 20),
                ("io.read_bytes", 1),
            ]),
            1,
            1_000_000,
        );
        capture.push(
            1_000_000,
            1_000_000,
            point(&[
                ("cpu.some_total_us", 100_100),
                ("cpu.throttled_total_us", 3_000_000),
                ("memory.current_bytes", 10),
                ("io.read_bytes", 101),
            ]),
            2,
            Some(1_000_000),
        );
        let (report, summary) = report(
            Some(&capture),
            None,
            super::super::PressureScope::AgentProcessTree,
        );
        assert_eq!(summary["status"], "completed");
        assert_eq!(summary["observations"]["cpu.some_stall_pct"], 10.0);
        assert_eq!(summary["observations"]["cpu.throttled_ms"], 3000.0);
        assert!(
            summary["observations"]
                .get("cpu.throttled_stall_pct")
                .is_none()
        );
        assert!(
            summary["observations"]
                .get("cpu.throttled_stall_ms")
                .is_none()
        );
        assert_eq!(summary["observations"]["memory.sampled_peak_bytes"], 20.0);
        assert_eq!(
            report["series"]["kernel-pressure.io.read_bytes_per_s"][0],
            100.0
        );
        assert_eq!(
            report["series_timestamps"]["kernel-pressure.io.read_bytes_per_s"][0],
            1.0
        );
        assert_eq!(report["raw_initial"]["values"]["cpu.some_total_us"], 100);
        assert_eq!(summary["collector_overhead"]["sample_count"], 2);
        assert_eq!(summary["collector_overhead"]["sample_read_us_total"], 3);
        assert_eq!(summary["collector_overhead"]["sample_read_us_max"], 2);
        assert_eq!(summary["collector_overhead"]["interval_jitter_us_max"], 0);
        assert_eq!(summary["collector_overhead"]["schedule_lateness_us_max"], 0);
        assert_eq!(
            summary["collector_overhead"]["missed_scheduled_intervals"],
            0
        );
        assert!(summary.get("series").is_none());
    }

    #[test]
    fn overhead_keeps_read_cost_separate_from_schedule_lateness() {
        let mut capture = Capture::new(point(&[("memory.current_bytes", 10)]), 1, 1_000_000);
        // The first scheduled sample took 650 ms, but it began exactly on its
        // absolute deadline. Read duration is overhead, not scheduling lateness.
        capture.push(
            1_000_000,
            1_650_000,
            point(&[("memory.current_bytes", 11)]),
            650_000,
            Some(1_000_000),
        );
        // The following sample started 50 ms late. Its lower read cost must
        // not be added to that lateness or the observed cadence jitter.
        capture.push(
            2_050_000,
            2_100_000,
            point(&[("memory.current_bytes", 12)]),
            50_000,
            Some(2_000_000),
        );
        let (_, summary) = report(
            Some(&capture),
            None,
            super::super::PressureScope::AgentProcessTree,
        );
        assert_eq!(summary["collector_overhead"]["sample_read_us_max"], 650_000);
        assert_eq!(
            summary["collector_overhead"]["schedule_lateness_us_max"],
            50_000
        );
        assert_eq!(
            summary["collector_overhead"]["interval_jitter_us_max"],
            50_000
        );
    }

    #[test]
    fn cadence_advances_from_absolute_deadlines_and_counts_missed_intervals() {
        let started = Instant::now();
        let interval = Duration::from_secs(1);
        let first_deadline = started + interval;
        let (next, missed) = advance_deadline(
            first_deadline,
            started + Duration::from_millis(3_250),
            interval,
        );
        assert_eq!(missed, 2);
        assert_eq!(next.duration_since(started), Duration::from_secs(4));
    }

    #[test]
    fn owned_container_scope_is_explicit_not_agent_process_tree() {
        let mut capture = Capture::new(point(&[("memory.current_bytes", 10)]), 1, 1_000);
        capture.push(
            1_000,
            1_000,
            point(&[("memory.current_bytes", 11)]),
            2,
            Some(1_000),
        );
        let (report, summary) = report(
            Some(&capture),
            None,
            super::super::PressureScope::OwnedContainerExecution,
        );
        assert_eq!(report["scope"], "owned-container-execution");
        assert_eq!(summary["scope_source"], "verified-owned-container-cgroup");
        assert!(
            summary["limitations"]
                .as_str()
                .unwrap()
                .contains("runtime helpers")
        );
    }
    #[test]
    fn reset_and_mid_capture_loss_invalidate_entire_metric_not_other_fields() {
        let mut capture = Capture::new(
            point(&[("cpu.some_total_us", 20), ("io.read_bytes", 10)]),
            1,
            100,
        );
        capture.push(
            100,
            100,
            point(&[("cpu.some_total_us", 10), ("io.read_bytes", 11)]),
            2,
            Some(100),
        );
        let mut missing = point(&[("cpu.some_total_us", 30)]);
        missing
            .unavailable
            .insert("io.read_bytes".into(), "fixture inaccessible".into());
        capture.push(200, 200, missing, 2, Some(200));
        capture.push(
            300,
            300,
            point(&[("cpu.some_total_us", 50), ("io.read_bytes", 15)]),
            2,
            Some(300),
        );
        let (report, summary) = report(
            Some(&capture),
            None,
            super::super::PressureScope::AgentProcessTree,
        );
        assert_eq!(summary["status"], "unavailable");
        assert!(report["metrics"].as_object().unwrap().is_empty());
        assert!(report["series"].as_object().unwrap().is_empty());
        assert!(
            summary["unavailable"]["cpu.some_total_us"]
                .as_str()
                .unwrap()
                .contains("decreased")
        );
    }
    #[test]
    fn retained_series_is_bounded_and_preserves_actual_elapsed_timestamps() {
        let mut capture = Capture::new(point(&[("cpu.some_total_us", 0)]), 1, 1_000);
        for index in 1..MAX_POINTS * 3 {
            capture.push(
                index as u64 * 1000,
                index as u64 * 1000,
                point(&[("cpu.some_total_us", index as u64)]),
                1,
                Some(index as u64 * 1000),
            );
        }
        assert!(capture.downsampled);
        assert!(capture.points.len() <= MAX_POINTS);
        let (report, _) = report(
            Some(&capture),
            None,
            super::super::PressureScope::AgentProcessTree,
        );
        let times = report["series_timestamps"]["kernel-pressure.cpu.some_stall_pct"]
            .as_array()
            .unwrap();
        assert!(times.windows(2).all(|p| p[0].as_f64() < p[1].as_f64()));
        assert_eq!(
            times.last().unwrap().as_f64().unwrap(),
            (MAX_POINTS * 3 - 1) as f64 / 1000.0
        );
    }
    #[test]
    fn sampler_stops_without_waiting_for_long_poll_interval() {
        let path = crate::fixture_namespace::allocate(&std::env::temp_dir(), "asb-pressure-stop-")
            .unwrap();
        std::fs::write(path.join("memory.current"), "10").unwrap();
        let sampler = Sampler::start(path.clone(), 60_000).unwrap();
        let started = Instant::now();
        let capture = sampler.stop().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(capture.points.len(), 2);
        std::fs::remove_dir_all(path).unwrap();
    }
}
