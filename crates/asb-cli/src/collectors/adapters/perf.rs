use super::super::CollectorAdapter;
use super::super::PreflightContext;
use super::super::{ProcessContext, ProcessData, SharedArtifact};
use crate::measurements;
use std::process::{Command, Stdio};

/// Build the process-scoped `perf record` arguments.  Attaching to the runner
/// root with inheritance is deliberate: the root may fork a bridge, agent
/// helper, or workload process after attachment, and each descendant must be
/// included without sampling unrelated host processes.
fn record_args(pid: u32) -> Vec<String> {
    vec![
        "record".into(),
        "-g".into(),
        "-F".into(),
        "19".into(),
        "--inherit".into(),
        "-p".into(),
        pid.to_string(),
    ]
}

/// Build the process-scoped interval counter arguments with the same
/// inheritance contract as call-stack recording.
fn stat_args(pid: u32, interval_ms: u64) -> Vec<String> {
    vec![
        "stat".into(),
        "-I".into(),
        interval_ms.to_string(),
        "-x,".into(),
        "-e".into(),
        "task-clock,context-switches,cpu-migrations,page-faults,cycles,instructions".into(),
        "--inherit".into(),
        "-p".into(),
        pid.to_string(),
    ]
}
pub(super) struct PerfCollector;
pub(super) static ADAPTER: PerfCollector = PerfCollector;

impl CollectorAdapter for PerfCollector {
    fn id(&self) -> &'static str {
        "perf"
    }
    fn environment_flag(&self) -> Option<&'static str> {
        Some("ASB_MEASURE_PERF")
    }
    fn tool_id(&self) -> Option<&'static str> {
        Some("perf")
    }
    fn requires_pid(&self) -> bool {
        true
    }
    fn preflight(&self, context: &mut PreflightContext<'_>) -> Result<(), String> {
        preflight(context)
    }
    fn configure(
        &self,
        session: &mut measurements::Session,
        selected: &[String],
    ) -> Result<(), String> {
        session.state::<PerfState>().borrow_mut().enabled =
            super::super::selected_or_environment("perf", selected);
        Ok(())
    }
    fn graceful_shutdown(&self, name: &str) -> bool {
        name == "perf"
    }
    fn process(
        &self,
        session: &mut measurements::Session,
        context: &ProcessContext<'_>,
    ) -> Result<ProcessData, String> {
        process(session, context)
    }
    fn attach_pid(&self, session: &mut measurements::Session, pid: u32) -> Result<(), String> {
        session.start_perf(pid)
    }
}

fn process(
    session: &mut measurements::Session,
    context: &ProcessContext<'_>,
) -> Result<ProcessData, String> {
    let mut result = ProcessData::from_output(context);
    if context.name == "perf-stat" {
        result.metrics = parse_perf_stat_metrics(context.stderr);
        result.series = parse_perf_stat_series(context.stderr);
        if context.status.code().is_none() && !context.stderr.is_empty() {
            result.status = "completed".into();
        }
    } else {
        result.artifact = session
            .artifact(SharedArtifact::StackSamples)
            .filter(|path| path.is_file())
            .map(measurements::file_name);
        let exists = session
            .artifact(SharedArtifact::StackSamples)
            .is_some_and(|path| path.metadata().is_ok_and(|metadata| metadata.len() > 0));
        result.status =
            if context.status.success() || (context.status.code().is_none() && exists) {
                "completed"
            } else {
                "failed"
            }
            .into();
    }
    Ok(result)
}

fn preflight(context: &mut PreflightContext<'_>) -> Result<(), String> {
    if !super::super::selected_or_environment("perf", context.selected) {
        return Ok(());
    }
    let executable = measurements::resolve_executable(context.tool_dir, "perf");
    for (name, args) in [
        (
            "stat",
            &[
                "stat",
                "-e",
                "task-clock,context-switches,cpu-migrations,page-faults,cycles,instructions",
                "--inherit",
                "--",
                "sleep",
                "0.01",
            ][..],
        ),
        (
            "record",
            &["record", "-g", "-F", "19", "-o", "-", "--", "sleep", "0.01"][..],
        ),
    ] {
        let output = Command::new(&executable)
            .args(args)
            .stdout(Stdio::null())
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
                if stderr.contains("permission") || stderr.contains("perf_event_paranoid") || stderr.contains("access to performance monitoring") || stderr.contains("operation not permitted") {
                    return Err(perf_permission_error());
                }
                return Err(format!("perf {name} preflight failed ({}). Inspect the host configuration with `perf {name} -- sleep 0.01` and retry.", output.status));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err("perf is required for the selected collector. Run `asb tool install perf` and retry.".into()),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return Err(format!("cannot execute perf at {}: {error}. Check parent-directory access and reinstall with `asb tool install perf`.", executable.display())),
            Err(error) => return Err(format!("cannot probe perf collector: {error}; check with `perf --version`")),
        }
    }
    Ok(())
}

use crate::collectors;
use crate::measurements::parse_float;
use crate::measurements::{Collector, env_true, measurement_interval_ms, write_unavailable};
use std::collections::BTreeMap;
use std::fs::File;
use std::time::Instant;

#[derive(Default)]
struct PerfState {
    enabled: bool,
}

impl measurements::Session {
    pub(crate) fn start_perf(&mut self, pid: u32) -> Result<(), String> {
        if !self.state::<PerfState>().borrow().enabled {
            return Ok(());
        }
        let data = self.dir.join("perf.data");
        let stderr_path = self.dir.join("perf.stderr");
        let output =
            File::create(&stderr_path).map_err(|e| format!("cannot create perf output: {e}"))?;
        let stdout_path = self.dir.join("perf.stdout");
        let stdout = File::create(&stdout_path)
            .map_err(|e| format!("cannot create perf standard output: {e}"))?;
        // Attach to the runner root and inherit counters into every child.
        // Unlike a one-time comma-separated PID snapshot, perf's inheritance
        // follows helpers forked after attachment as well as descendants of
        // those helpers, while remaining scoped to this benchmark tree.
        let record_args = record_args(pid);
        let child = match self
            .command("perf")
            // Keep perf data bounded enough for the offline flamegraph
            // renderer while retaining process-scoped call samples.
            .args(&record_args)
            .args(["-o"])
            .arg(&data)
            .stderr(Stdio::from(output))
            .stdout(Stdio::from(stdout))
            .spawn()
        {
            Ok(child) => child,
            Err(error) if !env_true("ASB_MEASURE_PERF") => {
                write_unavailable(&self.dir.join("perf.json"), "perf", error.to_string())?;
                return Ok(());
            }
            Err(error) => {
                return Err(format!(
                    "cannot start perf (disable with ASB_MEASURE_PERF=0): {error}"
                ));
            }
        };
        self.set_artifact(SharedArtifact::StackSamples, data);
        self.collectors.push(Collector {
            name: "perf".into(),
            adapter: collectors::find("perf").expect("registered collector"),
            child,
            started: Instant::now(),
            stdout_path,
            stderr_path,
        });
        let stat_stdout_path = self.dir.join("perf-stat.stdout");
        let stat_stderr_path = self.dir.join("perf-stat.stderr");
        let stat_stdout = File::create(&stat_stdout_path)
            .map_err(|e| format!("cannot create perf stat output: {e}"))?;
        let stat_stderr = File::create(&stat_stderr_path)
            .map_err(|e| format!("cannot create perf stat error output: {e}"))?;
        let stat_args = stat_args(pid, measurement_interval_ms());
        let stat = self
            .command("perf")
            .args(&stat_args)
            .stdout(Stdio::from(stat_stdout))
            .stderr(Stdio::from(stat_stderr))
            .spawn();
        match stat {
            Ok(child) => self.collectors.push(Collector {
                name: "perf-stat".into(),
                adapter: collectors::find("perf").expect("registered collector"),
                child,
                started: Instant::now(),
                stdout_path: stat_stdout_path,
                stderr_path: stat_stderr_path,
            }),
            Err(error) => write_unavailable(
                &self.dir.join("perf-stat.json"),
                "perf-stat",
                error.to_string(),
            )?,
        }
        Ok(())
    }
}

pub(crate) fn parse_perf_stat_series(output: &str) -> BTreeMap<String, Vec<f64>> {
    let mut series: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for line in output.lines() {
        let fields = line.split(',').map(str::trim).collect::<Vec<_>>();
        let Some(event_index) = fields.iter().position(|field| {
            matches!(
                *field,
                "task-clock"
                    | "context-switches"
                    | "cpu-migrations"
                    | "page-faults"
                    | "cycles"
                    | "instructions"
            )
        }) else {
            continue;
        };
        let Some(value) = event_index
            .checked_sub(2)
            .and_then(|index| fields.get(index))
            .and_then(|field| parse_float(field))
        else {
            continue;
        };
        let Some(name) = perf_metric_name(fields[event_index]) else {
            continue;
        };
        series
            .entry(format!("perf.{name}"))
            .or_default()
            .push(value);
    }
    series
}

pub(crate) fn parse_perf_stat_metrics(output: &str) -> BTreeMap<String, f64> {
    let mut metrics = BTreeMap::new();
    for line in output.lines() {
        let fields = line.split(',').map(str::trim).collect::<Vec<_>>();
        let Some(event_index) = fields.iter().position(|field| {
            matches!(
                *field,
                "task-clock"
                    | "context-switches"
                    | "cpu-migrations"
                    | "page-faults"
                    | "cycles"
                    | "instructions"
            )
        }) else {
            continue;
        };
        let Some(value) = event_index
            .checked_sub(2)
            .and_then(|index| fields.get(index))
            .and_then(|field| parse_float(field))
        else {
            continue;
        };
        let Some(name) = perf_metric_name(fields[event_index]) else {
            continue;
        };
        metrics.insert(format!("perf.{name}"), value);
    }
    metrics
}

fn perf_metric_name(event: &str) -> Option<&'static str> {
    match event {
        "task-clock" => Some("task_clock_ms"),
        "context-switches" => Some("context_switches"),
        "cpu-migrations" => Some("cpu_migrations"),
        "page-faults" => Some("page_faults"),
        "cycles" => Some("cycles"),
        "instructions" => Some("instructions"),
        _ => None,
    }
}

pub(crate) fn perf_permission_error() -> String {
    "perf was denied access to performance events. A restrictive kernel.perf_event_paranoid setting is one possible cause.\nWhat to do: if your administrator approves system-wide perf access, run:\n  sudo sysctl -w kernel.perf_event_paranoid=-1\nTo persist the setting across reboots:\n  printf 'kernel.perf_event_paranoid=-1\\n' | sudo tee /etc/sysctl.d/99-asb-perf.conf\nThis changes access for the whole host; containers, capabilities, security policy or other restrictions may still prevent tracing. ASB does not create sudoers rules for perf.".into()
}

#[cfg(test)]
mod tests {
    use super::{record_args, stat_args};

    #[test]
    fn perf_record_is_root_attached_and_inherits_all_descendants() {
        assert_eq!(
            record_args(42),
            vec!["record", "-g", "-F", "19", "--inherit", "-p", "42"]
        );
    }

    #[test]
    fn perf_stat_uses_the_same_process_tree_scope() {
        assert_eq!(
            stat_args(42, 1000),
            vec![
                "stat",
                "-I",
                "1000",
                "-x,",
                "-e",
                "task-clock,context-switches,cpu-migrations,page-faults,cycles,instructions",
                "--inherit",
                "-p",
                "42"
            ]
        );
    }
}
