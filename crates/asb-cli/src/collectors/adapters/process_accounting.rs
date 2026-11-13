//! Linux process-tree fallback accounting owned by the CPU collector.
//!
//! This is deliberately not part of `measurements`: it is one concrete
//! observation strategy used when delegated cgroup-v2 accounting is absent.

use crate::measurements::ProcessIdentity;
use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Default)]
pub(super) struct Sample {
    pub user_ticks: u64,
    pub system_ticks: u64,
    pub io_delay_ticks: u64,
    pub runqueue_ns: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cancelled_write_bytes: u64,
}

type Identity = (u32, u64);
const MAX_OBSERVED_PROCESSES: usize = 100_000;

#[derive(Default)]
struct Accounting {
    previous: BTreeMap<Identity, Sample>,
    total: Sample,
    invalid: bool,
}

impl Accounting {
    fn update(&mut self, processes: BTreeMap<Identity, Sample>) {
        if processes.len() > MAX_OBSERVED_PROCESSES {
            self.invalid = true;
            return;
        }
        self.total = add_samples(self.total, tree_delta(&self.previous, &processes));
        self.previous = processes;
    }

    fn delta(&self) -> Option<Sample> {
        (!self.invalid).then_some(self.total)
    }
}

fn add_samples(left: Sample, right: Sample) -> Sample {
    Sample {
        user_ticks: left.user_ticks.saturating_add(right.user_ticks),
        system_ticks: left.system_ticks.saturating_add(right.system_ticks),
        io_delay_ticks: left.io_delay_ticks.saturating_add(right.io_delay_ticks),
        runqueue_ns: left.runqueue_ns.saturating_add(right.runqueue_ns),
        read_bytes: left.read_bytes.saturating_add(right.read_bytes),
        write_bytes: left.write_bytes.saturating_add(right.write_bytes),
        cancelled_write_bytes: left
            .cancelled_write_bytes
            .saturating_add(right.cancelled_write_bytes),
    }
}

fn tree_delta(
    previous: &BTreeMap<Identity, Sample>,
    current: &BTreeMap<Identity, Sample>,
) -> Sample {
    current
        .iter()
        .fold(Sample::default(), |total, (identity, sample)| {
            let before = previous.get(identity).copied().unwrap_or_default();
            add_samples(
                total,
                Sample {
                    user_ticks: sample.user_ticks.saturating_sub(before.user_ticks),
                    system_ticks: sample.system_ticks.saturating_sub(before.system_ticks),
                    io_delay_ticks: sample.io_delay_ticks.saturating_sub(before.io_delay_ticks),
                    runqueue_ns: sample.runqueue_ns.saturating_sub(before.runqueue_ns),
                    read_bytes: sample.read_bytes.saturating_sub(before.read_bytes),
                    write_bytes: sample.write_bytes.saturating_sub(before.write_bytes),
                    cancelled_write_bytes: sample
                        .cancelled_write_bytes
                        .saturating_sub(before.cancelled_write_bytes),
                },
            )
        })
}

pub(super) struct Sampler {
    stop: std::sync::mpsc::Sender<()>,
    accounting: Arc<Mutex<Accounting>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Sampler {
    pub(super) fn start(root: ProcessIdentity, interval_ms: u64) -> Option<Self> {
        let initial = read_tree(root)?;
        if initial.len() > MAX_OBSERVED_PROCESSES {
            return None;
        }
        let (stop, receiver) = std::sync::mpsc::channel();
        let accounting = Arc::new(Mutex::new(Accounting {
            previous: initial,
            ..Accounting::default()
        }));
        let thread_accounting = Arc::clone(&accounting);
        let handle = std::thread::Builder::new()
            .name("asb-process-timing".into())
            .spawn(move || {
                loop {
                    let Some(sample) = read_tree(root) else {
                        break;
                    };
                    if let Ok(mut state) = thread_accounting.lock() {
                        state.update(sample);
                    }
                    if receiver.recv_timeout(Duration::from_millis(interval_ms))
                        != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    {
                        break;
                    }
                }
                if let Some(sample) = read_tree(root)
                    && let Ok(mut state) = thread_accounting.lock()
                {
                    state.update(sample);
                }
            })
            .ok()?;
        Some(Self {
            stop,
            accounting,
            handle: Some(handle),
        })
    }

    pub(super) fn stop(mut self) -> Option<Sample> {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.accounting.lock().ok().and_then(|state| state.delta())
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

#[derive(Clone, Copy)]
pub(super) struct TimedSample {
    pub elapsed: Duration,
    pub delta: Sample,
}

/// A direct `/proc` process-tree sampler for collector adapters that need a
/// time series. It shares the CPU fallback's PID-generation checks and avoids
/// one short-lived native process per sample.
pub(super) struct SeriesSampler {
    stop: std::sync::mpsc::Sender<()>,
    samples: Arc<Mutex<Vec<TimedSample>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SeriesSampler {
    pub(super) fn start(root: ProcessIdentity, interval_ms: u64) -> Option<Self> {
        let initial = read_tree(root)?;
        if initial.len() > MAX_OBSERVED_PROCESSES {
            return None;
        }
        let (stop, receiver) = std::sync::mpsc::channel();
        let samples = Arc::new(Mutex::new(Vec::new()));
        let thread_samples = Arc::clone(&samples);
        let interval = Duration::from_millis(interval_ms);
        let handle = std::thread::Builder::new()
            .name("asb-process-series".into())
            .spawn(move || {
                let started = std::time::Instant::now();
                let mut previous = initial;
                loop {
                    if receiver.recv_timeout(interval)
                        != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    {
                        break;
                    }
                    let Some(current) = read_tree(root) else {
                        break;
                    };
                    let delta = tree_delta(&previous, &current);
                    previous = current;
                    if let Ok(mut values) = thread_samples.lock() {
                        values.push(TimedSample {
                            elapsed: started.elapsed(),
                            delta,
                        });
                    }
                }
                if let Some(current) = read_tree(root) {
                    let delta = tree_delta(&previous, &current);
                    if let Ok(mut values) = thread_samples.lock() {
                        values.push(TimedSample {
                            elapsed: started.elapsed(),
                            delta,
                        });
                    }
                }
            })
            .ok()?;
        Some(Self {
            stop,
            samples,
            handle: Some(handle),
        })
    }

    pub(super) fn stop(mut self) -> Option<Vec<TimedSample>> {
        let _ = self.stop.send(());
        self.handle.take()?.join().ok()?;
        self.samples.lock().ok().map(|samples| samples.clone())
    }
}

impl Drop for SeriesSampler {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_tree(root: ProcessIdentity) -> Option<BTreeMap<Identity, Sample>> {
    if !root.matches() {
        return None;
    }
    let mut processes = BTreeMap::new();
    for pid in crate::measurements::process_tree_pids(root.pid) {
        let Some(identity) = ProcessIdentity::read(pid) else {
            continue;
        };
        if !identity.in_tree(root) {
            continue;
        }
        let Some(sample) = read_process(pid) else {
            continue;
        };
        if !identity.in_tree(root) {
            continue;
        }
        processes.insert((identity.pid, identity.start_time_ticks), sample);
    }
    (!processes.is_empty() && root.matches()).then_some(processes)
}

fn read_process(pid: u32) -> Option<Sample> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let schedstat = fs::read_to_string(format!("/proc/{pid}/schedstat")).ok();
    let io = fs::read_to_string(format!("/proc/{pid}/io")).ok();
    parse_sample(&stat, schedstat.as_deref(), io.as_deref())
}

fn parse_sample(stat: &str, schedstat: Option<&str>, io: Option<&str>) -> Option<Sample> {
    let fields = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let user_ticks = fields.get(11)?.parse().ok()?;
    let system_ticks = fields.get(12)?.parse().ok()?;
    let io_delay_ticks = fields
        .get(39)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let runqueue_ns = schedstat
        .and_then(|value| value.split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(0);
    let io_value = |key| {
        io.and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.trim().parse().ok())
        })
        .unwrap_or(0)
    };
    Some(Sample {
        user_ticks,
        system_ticks,
        io_delay_ticks,
        runqueue_ns,
        read_bytes: io_value("read_bytes:"),
        write_bytes: io_value("write_bytes:"),
        cancelled_write_bytes: io_value("cancelled_write_bytes:"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_stat_and_optional_schedstat() {
        let mut fields = vec!["0"; 40];
        fields[11] = "13";
        fields[12] = "5";
        fields[39] = "7";
        let sample = parse_sample(
            &format!("123 (agent) {}", fields.join(" ")),
            Some("1 11 2"),
            Some("read_bytes: 1024\nwrite_bytes: 2048\ncancelled_write_bytes: 512\n"),
        )
        .unwrap();
        assert_eq!(sample.user_ticks, 13);
        assert_eq!(sample.system_ticks, 5);
        assert_eq!(sample.io_delay_ticks, 7);
        assert_eq!(sample.runqueue_ns, 11);
        assert_eq!(sample.read_bytes, 1024);
        assert_eq!(sample.write_bytes, 2048);
        assert_eq!(sample.cancelled_write_bytes, 512);
        assert!(parse_sample("not a proc stat", None, None).is_none());
        assert!(parse_sample("123 (agent) S", None, None).is_none());
    }

    #[test]
    fn stale_process_generation_does_not_start_a_sampler() {
        let current = ProcessIdentity::read(std::process::id()).unwrap();
        let stale = ProcessIdentity {
            start_time_ticks: current.start_time_ticks + 1,
            ..current
        };
        assert!(Sampler::start(stale, 100).is_none());
    }

    #[test]
    fn accounting_saturates_when_a_kernel_counter_resets() {
        let initial = Sample {
            user_ticks: 10,
            system_ticks: 20,
            io_delay_ticks: 3,
            runqueue_ns: 100,
            ..Sample::default()
        };
        let mut accounting = Accounting {
            previous: [((1, 10), initial)].into(),
            ..Accounting::default()
        };
        accounting.update(
            [(
                (1, 10),
                Sample {
                    user_ticks: 14,
                    system_ticks: 28,
                    io_delay_ticks: 5,
                    runqueue_ns: 160,
                    ..Sample::default()
                },
            )]
            .into(),
        );
        let delta = accounting.delta().unwrap();
        assert_eq!(
            (
                delta.user_ticks,
                delta.system_ticks,
                delta.io_delay_ticks,
                delta.runqueue_ns
            ),
            (4, 8, 2, 60)
        );
        accounting.update([((1, 10), Sample::default())].into());
        let delta = accounting.delta().unwrap();
        assert_eq!(
            (
                delta.user_ticks,
                delta.system_ticks,
                delta.io_delay_ticks,
                delta.runqueue_ns
            ),
            (4, 8, 2, 60)
        );
    }

    #[test]
    fn observed_worker_cpu_survives_exit_and_pid_reuse() {
        let mut accounting = Accounting {
            previous: [(
                (1, 10),
                Sample {
                    user_ticks: 10,
                    ..Sample::default()
                },
            )]
            .into(),
            ..Accounting::default()
        };
        accounting.update(
            [
                (
                    (1, 10),
                    Sample {
                        user_ticks: 12,
                        ..Sample::default()
                    },
                ),
                (
                    (2, 20),
                    Sample {
                        user_ticks: 7,
                        ..Sample::default()
                    },
                ),
            ]
            .into(),
        );
        assert_eq!(accounting.delta().unwrap().user_ticks, 9);
        accounting.update(
            [(
                (1, 10),
                Sample {
                    user_ticks: 13,
                    ..Sample::default()
                },
            )]
            .into(),
        );
        assert_eq!(accounting.delta().unwrap().user_ticks, 10);
        accounting.update(
            [
                (
                    (1, 10),
                    Sample {
                        user_ticks: 14,
                        ..Sample::default()
                    },
                ),
                (
                    (2, 21),
                    Sample {
                        user_ticks: 3,
                        ..Sample::default()
                    },
                ),
            ]
            .into(),
        );
        assert_eq!(accounting.delta().unwrap().user_ticks, 14);
    }
}
