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

// Unit fixtures exercise later admission boundaries with deliberately tiny
// temporary filesystems. Keep their capacity premise explicit and scoped to
// the calling test thread; production always probes the target filesystem.
#[cfg(test)]
thread_local! {
    static TEST_AVAILABLE_MIB: Cell<Option<u64>> = const { Cell::new(None) };
}

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
}

impl Estimate {
    fn combine(self, other: Self) -> Self {
        Self {
            required_mib: self.required_mib.saturating_add(other.required_mib),
            attempts: self.attempts.saturating_add(other.attempts),
        }
    }
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
    let collectors = policy.map_or_else(
        || configured_collectors(config),
        |plan| {
            plan.collectors
                .iter()
                .map(|collector| collector.id.as_str())
                .collect()
        },
    );
    let collector_mib = collectors
        .into_iter()
        .fold(0u64, |total, id| total.saturating_add(collector_mib(id)));
    let per_attempt = BASE_PER_ATTEMPT_MIB
        .saturating_add(PROVIDER_SESSION_MIB)
        .saturating_add(agent_mib(&config.agent))
        .saturating_add(workload)
        .saturating_add(collector_mib)
        .saturating_add(mode_mib(mode));
    Estimate {
        // A factor of two covers retained partial evidence, tool logs, output
        // growth and task sources that exceed their declared disk envelope.
        required_mib: per_attempt
            .saturating_mul(attempts)
            .saturating_mul(SAFETY_NUMERATOR)
            .saturating_add(REPORT_OVERHEAD_MIB),
        attempts,
    }
}

pub(crate) fn sweep<'a>(
    entries: impl IntoIterator<Item = (&'a BenchmarkConfig, Option<&'a ResolvedPlan>)>,
    repeats: u64,
    mode: Option<tape::Mode>,
) -> Estimate {
    entries.into_iter().fold(
        Estimate {
            required_mib: REPORT_OVERHEAD_MIB,
            attempts: 0,
        },
        |total, (config, policy)| total.combine(benchmark(config, policy, repeats, mode)),
    )
}

/// Ask for an explicit confirmation only when the filesystem that will hold
/// the results has less free space than the conservative estimate. A pipeline
/// cannot answer a prompt, so it fails before starting an attempt instead.
pub(crate) fn confirm_capacity(
    output_directory: &Path,
    estimate: Estimate,
    interactive: bool,
    output: &mut dyn Write,
) -> Result<(), Error> {
    let Some(available_mib) = available_mib(output_directory) else {
        writeln!(output, "Disk-space estimate: at least {} MiB for {} benchmark attempt(s); available space could not be determined for {}.", estimate.required_mib, estimate.attempts, output_directory.display()).map_err(crate::io_error)?;
        return Ok(());
    };
    if !needs_confirmation(available_mib, estimate) {
        return Ok(());
    }
    writeln!(output, "Disk-space warning: ASB estimates at least {} MiB for {} benchmark attempt(s), including selected workload, agent, provider session, collectors, concurrency, repetitions, retained evidence and a 2x safety margin. Only {} MiB is available on {}.", estimate.required_mib, estimate.attempts, available_mib, existing_ancestor(output_directory).display()).map_err(crate::io_error)?;
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
    if workload.starts_with("swe-bench") {
        8192
    } else if workload.starts_with("terminal-bench") {
        4096
    } else if workload == "tau-bench" {
        2048
    } else {
        1024
    }
}

fn agent_mib(agent: &str) -> u64 {
    match agent {
        "codex" | "claude-code" | "opencode" | "goose" | "cline" | "continue" => 1024,
        "pi" | "hermes" | "openjiuwen" | "workbuddy" => 768,
        _ => 512,
    }
}

fn collector_mib(id: &str) -> u64 {
    match id.split(':').next().unwrap_or(id) {
        "perfetto" => 1024,
        "flamegraph" => 1024,
        "bpftrace" => 768,
        "perf" => 512,
        "mpstat" | "iostat" | "agent-memory" | "agent-workers" => 128,
        "system" | "runtime" | "token_usage" | "steps" | "success_rate" | "tool_calls" => 64,
        _ => 128,
    }
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
    fn sweep_accumulates_retained_outputs_for_every_configuration() {
        let config = config();
        let one = benchmark(&config, None, 1, None);
        let sweep = super::sweep([(&config, None), (&config, None)], 1, None);
        assert_eq!(sweep.attempts, one.attempts * 2);
        assert!(sweep.required_mib > one.required_mib * 2);
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
        };
        assert!(needs_confirmation(2047, estimate));
        assert!(!needs_confirmation(2048, estimate));
    }
}
