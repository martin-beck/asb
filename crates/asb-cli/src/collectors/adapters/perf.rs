use super::super::PreflightContext;
use super::super::StaticCollector;
use super::super::{ProcessContext, ProcessData, SharedArtifact};
use crate::measurements;
use std::process::{Command, Stdio};
pub(super) static ADAPTER: StaticCollector =
    StaticCollector::new("perf", Some("ASB_MEASURE_PERF"), Some("perf"), true)
        .with_preflight(preflight)
        .with_configure(|session, selected| {
            session.state::<PerfState>().borrow_mut().enabled =
                super::super::selected_or_environment("perf", selected);
            Ok(())
        })
        .with_processing(|name| name == "perf", process)
        .with_attach_pid(|session, pid| session.start_perf(pid));

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
        let target_pid = pid.to_string();
        let child = match self
            .command("perf")
            // Keep perf data bounded enough for the offline flamegraph
            // renderer while retaining process-scoped call samples.
            .args(["record", "-g", "-F", "19", "--inherit", "-p"])
            .arg(&target_pid)
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
        let interval_ms = measurement_interval_ms().to_string();
        let stat = self
            .command("perf")
            .args([
                "stat",
                "-I",
                &interval_ms,
                "-x,",
                "-e",
                "task-clock,context-switches,cpu-migrations,page-faults,cycles,instructions",
                "--inherit",
                "-p",
            ])
            .arg(&target_pid)
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
