//! Conservative preflight estimates for benchmark-owned output storage.
//!
//! This is intentionally a capacity warning rather than a resource-admission
//! contract: source images and upstream tools can vary.  It overestimates
//! retained workspaces, captures, recordings and reports so an operator gets a
//! useful warning before ASB creates a new result generation.
use crate::{Error, benchmark::BenchmarkConfig, planning::ResolvedPlan, tape};
#[cfg(test)]
use std::cell::Cell;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

const BASE_PER_ATTEMPT_MIB: u64 = 512;
const PROVIDER_SESSION_MIB: u64 = 256;
const REPORT_OVERHEAD_MIB: u64 = 512;
const SAFETY_NUMERATOR: u64 = 2;
// A sealed replay reuses an existing task image and cassette. It still needs a
// private writable workspace per active replica, but it does not reserve the
// full source-image build envelope used by live and recording runs.
const REPLAY_WORKSPACE_MIB: u64 = 1024;
// Project-local installed agents are shared across a replay cohort. Per-replica
// replay evidence is bounded, including for collectors whose live acquisition
// may reserve a larger host-side cache or symbol/image staging area.
const REPLAY_BASE_PER_ATTEMPT_MIB: u64 = 16;
const REPLAY_PROVIDER_SESSION_MIB: u64 = 16;
const REPLAY_AGENT_MIB: u64 = 32;
const REPLAY_COLLECTOR_MIB: u64 = 16;
const REPLAY_MODE_MIB: u64 = 16;

// Unit fixtures exercise later admission boundaries with deliberately tiny
// temporary filesystems. Keep their capacity premise explicit and scoped to
// the calling test thread; production always probes the target filesystem.
#[cfg(test)]
thread_local! {
    static TEST_AVAILABLE_MIB: Cell<Option<u64>> = const { Cell::new(None) };
}

#[cfg(any(test, debug_assertions))]
const TEST_AVAILABLE_MIB_ENV: &str = "ASB_TEST_AVAILABLE_MIB";

#[cfg(test)]
pub(crate) struct TestAvailableMib(Option<u64>);

#[cfg(test)]
pub(crate) fn test_available_mib(mib: u64) -> TestAvailableMib {
    TestAvailableMib(TEST_AVAILABLE_MIB.with(|available| available.replace(Some(mib))))
}

#[cfg(test)]
impl Drop for TestAvailableMib {
    fn drop(&mut self) {
        TEST_AVAILABLE_MIB.with(|available| available.set(self.0));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Estimate {
    required_mib: u64,
    attempts: u64,
    retained_mib: u64,
    transient_mib: u64,
}

/// Estimate retained output for all replicas and repeated batches of one
/// configuration. `policy` contributes the exact task-specific disk envelope
/// when a workload adapter has one.
pub(crate) fn benchmark(
    config: &BenchmarkConfig,
    policy: Option<&ResolvedPlan>,
    repeats: u64,
    mode: Option<tape::Mode>,
) -> Estimate {
    let repetitions = repeats.max(1);
    let attempts = u64::try_from(config.concurrency)
        .unwrap_or(u64::MAX)
        .saturating_mul(repetitions);
    let workload = policy
        .and_then(|plan| plan.task_requirements.as_ref())
        .and_then(|requirements| requirements.disk_mib)
        .unwrap_or_else(|| workload_mib(&config.workload));
    let workspace_mib = if mode == Some(tape::Mode::Replay) {
        workload.min(REPLAY_WORKSPACE_MIB)
    } else {
        workload
    };
    let collectors = policy.map_or_else(
        || configured_collectors(config),
        |plan| {
            plan.collectors
                .iter()
                .map(|collector| collector.id.as_str())
                .collect()
        },
    );
    let collector_mib = collectors.into_iter().fold(0u64, |total, id| {
        let estimate = collector_mib(id);
        let per_replica = if mode == Some(tape::Mode::Replay) {
            estimate.min(REPLAY_COLLECTOR_MIB)
        } else {
            estimate
        };
        total.saturating_add(per_replica)
    });
    let agent_mib = if mode == Some(tape::Mode::Replay) {
        agent_mib(&config.agent).min(REPLAY_AGENT_MIB)
    } else {
        agent_mib(&config.agent)
    };
    let (base_mib, provider_mib, mode_mib) = if mode == Some(tape::Mode::Replay) {
        (
            REPLAY_BASE_PER_ATTEMPT_MIB,
            REPLAY_PROVIDER_SESSION_MIB,
            REPLAY_MODE_MIB,
        )
    } else {
        (BASE_PER_ATTEMPT_MIB, PROVIDER_SESSION_MIB, mode_mib(mode))
    };
    let non_workspace_per_attempt = base_mib
        .saturating_add(provider_mib)
        .saturating_add(agent_mib)
        .saturating_add(collector_mib)
        .saturating_add(mode_mib);
    // A compact configuration retains the official grade, submission and
    // measurement evidence, but releases each successfully settled private
    // task workspace before the next repetition.  Its workspace reservation
    // is therefore a concurrent peak, not a cumulative retained population.
    // Failed or uncertain attempts deliberately remain preserved; the global
    // safety factor covers that bounded exception without claiming deletion.
    let workspace_attempts = if config.retention.compact_completed_attempts {
        u64::try_from(config.concurrency).unwrap_or(u64::MAX)
    } else {
        attempts
    };
    let retained_mib = non_workspace_per_attempt
        .saturating_mul(attempts)
        .saturating_mul(SAFETY_NUMERATOR);
    let transient_mib = workspace_mib
        .saturating_mul(workspace_attempts)
        .saturating_mul(SAFETY_NUMERATOR);
    Estimate {
        // A factor of two covers retained partial evidence, tool logs, output
        // growth and task sources that exceed their declared disk envelope.
        required_mib: retained_mib
            .saturating_add(transient_mib)
            .saturating_add(REPORT_OVERHEAD_MIB),
        attempts,
        retained_mib,
        transient_mib,
    }
}

pub(crate) fn sweep<'a>(
    entries: impl IntoIterator<Item = (&'a BenchmarkConfig, Option<&'a ResolvedPlan>)>,
    repeats: u64,
    mode: Option<tape::Mode>,
    parallel_configurations: usize,
) -> Estimate {
    let mut estimates = entries
        .into_iter()
        .map(|(config, policy)| benchmark(config, policy, repeats, mode))
        .collect::<Vec<_>>();
    estimates.sort_by_key(|estimate| std::cmp::Reverse(estimate.transient_mib));
    let parallel = parallel_configurations.max(1);
    let retained_mib = estimates.iter().fold(0u64, |total, estimate| {
        total.saturating_add(estimate.retained_mib)
    });
    let transient_mib = estimates
        .iter()
        .take(parallel)
        .fold(0u64, |total, estimate| {
            total.saturating_add(estimate.transient_mib)
        });
    let attempts = estimates.iter().fold(0u64, |total, estimate| {
        total.saturating_add(estimate.attempts)
    });
    Estimate {
        required_mib: retained_mib
            .saturating_add(transient_mib)
            .saturating_add(REPORT_OVERHEAD_MIB),
        attempts,
        retained_mib,
        transient_mib,
    }
}

/// Ask for an explicit confirmation only when the filesystem that will hold
/// the results has less free space than the conservative estimate. A pipeline
/// cannot answer a prompt, so it fails before starting an attempt instead.
pub(crate) fn confirm_capacity(
    output_directory: &Path,
    estimate: Estimate,
    interactive: bool,
    json: bool,
    output: &mut dyn Write,
) -> Result<(), Error> {
    let Some(available_mib) = available_mib(output_directory) else {
        if !json {
            writeln!(output, "Disk-space estimate: at least {} MiB for {} benchmark attempt(s); available space could not be determined for {}.", estimate.required_mib, estimate.attempts, output_directory.display()).map_err(crate::io_error)?;
        }
        return Ok(());
    };
    if !needs_confirmation(available_mib, estimate) {
        return Ok(());
    }
    if !json {
        writeln!(output, "Disk-space warning: ASB estimates at least {} MiB for {} benchmark attempt(s), including selected workload, agent, provider session, collectors, concurrency, repetitions, retained evidence and a 2x safety margin. Only {} MiB is available on {}.", estimate.required_mib, estimate.attempts, available_mib, existing_ancestor(output_directory).display()).map_err(crate::io_error)?;
    }
    if !interactive || !std::io::stdin().is_terminal() {
        return Err(Error::Input("insufficient estimated disk space; free space or rerun from an interactive terminal to explicitly confirm the risk".into()));
    }
    write!(output, "Continue despite the disk-space estimate? [y/N] ").map_err(crate::io_error)?;
    output.flush().map_err(crate::io_error)?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(crate::io_error)?;
    if answer.trim().eq_ignore_ascii_case("y") {
        writeln!(
            output,
            "Proceeding at user request despite the disk-space warning."
        )
        .map_err(crate::io_error)?;
        Ok(())
    } else {
        Err(Error::Input(
            "benchmark cancelled because the disk-space warning was not approved".into(),
        ))
    }
}

fn needs_confirmation(available_mib: u64, estimate: Estimate) -> bool {
    available_mib < estimate.required_mib
}

fn configured_collectors(config: &BenchmarkConfig) -> Vec<&str> {
    config
        .measurements
        .as_ref()
        .filter(|measurements| measurements.enabled)
        .map(|measurements| measurements.collectors.iter().map(String::as_str).collect())
        .unwrap_or_default()
}

fn workload_mib(workload: &str) -> u64 {
    crate::workloads::estimated_disk_mib(workload)
}

fn agent_mib(agent: &str) -> u64 {
    crate::agents::find(agent).map_or(512, |adapter| adapter.estimated_disk_mib())
}

fn collector_mib(id: &str) -> u64 {
    crate::collectors::adapter_for_selection(id).map_or(128, |adapter| adapter.estimated_disk_mib())
}

fn mode_mib(mode: Option<tape::Mode>) -> u64 {
    match mode {
        Some(tape::Mode::Record) => 1024,
        Some(tape::Mode::Replay) => 256,
        None => 128,
    }
}

fn existing_ancestor(path: &Path) -> PathBuf {
    let mut candidate = path.to_path_buf();
    while !candidate.exists() {
        if !candidate.pop() {
            return PathBuf::from(".");
        }
    }
    candidate
}

#[cfg(unix)]
fn available_mib(path: &Path) -> Option<u64> {
    #[cfg(test)]
    if let Some(available) = TEST_AVAILABLE_MIB.with(Cell::get) {
        return Some(available);
    }
    // Subprocess-backed fixtures execute the separately compiled debug CLI,
    // not this test crate.  Accept their explicit premise only in test/debug
    // builds; optimized release binaries always probe the target filesystem.
    #[cfg(any(test, debug_assertions))]
    if let Some(available) = std::env::var_os(TEST_AVAILABLE_MIB_ENV)
        .and_then(|value| value.to_string_lossy().parse::<u64>().ok())
    {
        return Some(available);
    }
    let path = existing_ancestor(path);
    // ASB forbids unsafe FFI. POSIX `df -Pk` gives a portable, fixed unit
    // capacity probe without accepting shell text or invoking a shell.
    let output = std::process::Command::new("df")
        .arg("-Pk")
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() || output.stdout.len() > 16 * 1024 {
        return None;
    }
    let fields = std::str::from_utf8(&output.stdout)
        .ok()?
        .lines()
        .skip(1)
        .last()?
        .split_whitespace()
        .collect::<Vec<_>>();
    fields
        .get(3)?
        .parse::<u64>()
        .ok()
        .map(|blocks| blocks / 1024)
}

#[cfg(not(unix))]
fn available_mib(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_owned_estimates_cover_known_selections_and_unknown_fallbacks() {
        assert_eq!(agent_mib("codex"), 1024);
        assert_eq!(agent_mib("pi"), 768);
        assert_eq!(agent_mib("unregistered-agent"), 512);
        assert_eq!(workload_mib("swe-bench"), 8192);
        assert_eq!(workload_mib("swe-bench-verified"), 8192);
        assert_eq!(workload_mib("terminal-bench-2.1"), 4096);
        assert_eq!(workload_mib("tau-bench"), 2048);
        assert_eq!(workload_mib("custom-workload"), 1024);
        assert_eq!(collector_mib("perfetto"), 1024);
        assert_eq!(collector_mib("bpftrace:sched-latency"), 768);
        assert_eq!(collector_mib("tool-calls"), 128);
        assert_eq!(collector_mib("unknown"), 128);
    }

    fn config() -> BenchmarkConfig {
        toml::from_str("agent='opencode'\nprovider='openrouter'\nmodel='fixture'\nworkload='swe-bench'\nconcurrency=2\n[measurements]\nenabled=true\ndirectory='measurements'\ncollectors=['perfetto','flamegraph','bpftrace:all']\n").unwrap()
    }

    #[test]
    fn estimate_includes_workload_agent_collectors_concurrency_repetition_and_recording() {
        let live = benchmark(&config(), None, 1, None);
        let recording = benchmark(&config(), None, 3, Some(tape::Mode::Record));
        assert_eq!(live.attempts, 2);
        assert_eq!(recording.attempts, 6);
        assert!(recording.required_mib > live.required_mib * 3);
    }

    #[test]
    fn sealed_replay_reserves_a_bounded_writable_workspace_not_a_fresh_image_build() {
        let live = benchmark(&config(), None, 1, None);
        let replay = benchmark(&config(), None, 1, Some(tape::Mode::Replay));
        assert_eq!(live.attempts, replay.attempts);
        assert!(replay.required_mib < live.required_mib / 2);
        // The replay workspace remains explicitly reserved for every active
        // replica instead of treating the existing cassette as cost-free.
        assert!(replay.required_mib > REPLAY_WORKSPACE_MIB);
    }

    #[test]
    fn compact_retention_reserves_workspace_only_for_concurrent_attempts() {
        let mut retained = config();
        retained.concurrency = 2;
        let compact = BenchmarkConfig {
            retention: crate::benchmark::RetentionConfig {
                compact_completed_attempts: true,
            },
            ..retained.clone()
        };
        let retained = benchmark(&retained, None, 3, None);
        let compact = benchmark(&compact, None, 3, None);
        assert_eq!(retained.attempts, compact.attempts);
        assert!(compact.required_mib < retained.required_mib);
    }

    #[test]
    fn sweep_accumulates_retained_outputs_for_every_configuration() {
        let config = config();
        let one = benchmark(&config, None, 1, None);
        let sweep = super::sweep([(&config, None), (&config, None)], 1, None, 1);
        assert_eq!(sweep.attempts, one.attempts * 2);
        assert!(sweep.required_mib > one.required_mib);
        assert!(sweep.required_mib < one.required_mib * 2);
    }

    #[test]
    fn replay_sweep_reserves_only_simultaneous_workspaces() {
        let mut small = config();
        small.concurrency = 1;
        let mut large = small.clone();
        large.concurrency = 32;
        let sequential = super::sweep(
            [(&small, None), (&large, None)],
            1,
            Some(tape::Mode::Replay),
            1,
        );
        let parallel = super::sweep(
            [(&small, None), (&large, None)],
            1,
            Some(tape::Mode::Replay),
            2,
        );
        assert_eq!(sequential.attempts, 33);
        assert!(parallel.required_mib > sequential.required_mib);
        assert_eq!(
            sequential.transient_mib,
            REPLAY_WORKSPACE_MIB * 32 * SAFETY_NUMERATOR
        );
    }

    #[test]
    fn existing_ancestor_handles_a_future_result_directory() {
        let ancestor = existing_ancestor(Path::new("/tmp/asb-does-not-exist/result/run-001"));
        assert!(ancestor.exists());
    }

    #[test]
    fn capacity_check_requires_confirmation_only_below_the_estimate() {
        let estimate = Estimate {
            required_mib: 2048,
            attempts: 1,
            retained_mib: 0,
            transient_mib: 0,
        };
        assert!(needs_confirmation(2047, estimate));
        assert!(!needs_confirmation(2048, estimate));
    }

    #[test]
    fn json_output_stays_machine_readable_when_capacity_is_unavailable() {
        let mut output = Vec::new();
        confirm_capacity(
            Path::new("\0"),
            Estimate {
                required_mib: 2048,
                attempts: 1,
                retained_mib: 0,
                transient_mib: 0,
            },
            false,
            false,
            &mut output,
        )
        .unwrap();
        assert!(output.is_empty());
    }
}
