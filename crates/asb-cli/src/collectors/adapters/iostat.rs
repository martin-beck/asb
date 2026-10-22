use super::super::{CollectorAdapter, ProcessContext, ProcessData};
use super::pidstat::parse_columns;
use crate::measurements;
use crate::measurements::parse_float;
use std::collections::BTreeMap;
pub(super) struct IostatCollector;
pub(super) static ADAPTER: IostatCollector = IostatCollector;

impl CollectorAdapter for IostatCollector {
    fn id(&self) -> &'static str {
        "iostat"
    }
    fn environment_flag(&self) -> Option<&'static str> {
        Some("ASB_MEASURE_IOSTAT")
    }
    fn tool_id(&self) -> Option<&'static str> {
        Some("iostat")
    }
    fn requires_pid(&self) -> bool {
        true
    }
    fn preflight(&self, context: &mut super::super::PreflightContext<'_>) -> Result<(), String> {
        if super::super::selected_or_environment("iostat", context.selected) {
            super::pidstat::preflight(context.tool_dir, "-d")
        } else {
            Ok(())
        }
    }
    fn configure(
        &self,
        session: &mut measurements::Session,
        selected: &[String],
    ) -> Result<(), String> {
        session.state::<IoState>().borrow_mut().enabled =
            super::super::selected_or_environment("iostat", selected);
        Ok(())
    }
    fn process(
        &self,
        session: &mut measurements::Session,
        context: &ProcessContext<'_>,
    ) -> Result<ProcessData, String> {
        process(session, context)
    }
    fn attach_pid(&self, session: &mut measurements::Session, pid: u32) -> Result<(), String> {
        let enabled = session.state::<IoState>().borrow().enabled;
        super::pidstat::start_snapshot(session, "iostat", enabled, pid, "-d")
    }
}

fn process(
    _: &mut measurements::Session,
    context: &ProcessContext<'_>,
) -> Result<ProcessData, String> {
    let mut result = ProcessData::from_output(context);
    let output = super::super::read_complete_output(context.stdout_reader()?)?;
    result.metrics = parse_pidstat_io_metrics(&output);
    if result.metrics.is_empty() {
        result.metrics = parse_iostat_metrics(&output);
    }
    let samples = samples(&output);
    result.series = samples.series;
    result.timestamps = samples.timestamps;
    if result.metrics.is_empty() && result.series.is_empty() {
        result.status = "unavailable".into();
    }
    super::super::validate_processed_output(&result)?;
    Ok(result)
}

#[cfg(test)]
pub(crate) fn parse_series(output: &str) -> std::collections::BTreeMap<String, Vec<f64>> {
    samples(output).series
}

fn samples(output: &str) -> super::pidstat::Samples {
    super::pidstat::parse_samples(
        output,
        &["kB_rd/s", "kB_wr/s", "kB_ccwr/s", "iodelay"],
        |name| {
            format!(
                "process.io.{}",
                match name {
                    "kB_rd/s" => "read_kib_per_s",
                    "kB_wr/s" => "write_kib_per_s",
                    "kB_ccwr/s" => "cancelled_write_kib_per_s",
                    "iodelay" => "delay_ticks",
                    _ => "unknown",
                }
            )
        },
    )
}

#[derive(Default)]
struct IoState {
    enabled: bool,
}

pub(crate) fn parse_pidstat_io_metrics(output: &str) -> BTreeMap<String, f64> {
    parse_columns(
        output,
        &["kB_rd/s", "kB_wr/s", "kB_ccwr/s", "iodelay"],
        |name| {
            format!(
                "process.io.{}",
                match name {
                    "kB_rd/s" => "read_kib_per_s",
                    "kB_wr/s" => "write_kib_per_s",
                    "kB_ccwr/s" => "cancelled_write_kib_per_s",
                    "iodelay" => "delay_ticks",
                    _ => "unknown",
                }
            )
        },
    )
}

pub(crate) fn parse_iostat_metrics(output: &str) -> BTreeMap<String, f64> {
    let mut metrics = BTreeMap::new();
    let mut lines = output.lines().peekable();
    let mut device_totals: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    while let Some(line) = lines.next() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if line.trim_start().starts_with("avg-cpu") {
            let headers: Vec<&str> = line
                .split_once(':')
                .map_or_else(Vec::new, |(_, rest)| rest.split_whitespace().collect());
            let Some(values) = lines
                .next()
                .map(|line| line.split_whitespace().collect::<Vec<_>>())
            else {
                continue;
            };
            for (header, value) in headers.iter().zip(values.iter()) {
                if let Some(value) = parse_float(value) {
                    metrics.insert(format!("cpu.{}", iostat_name(header)), value);
                }
            }
            continue;
        }
        if fields.first().copied() != Some("Device") || fields.len() < 2 {
            continue;
        }
        let headers = fields[1..].to_vec();
        while let Some(next) = lines.peek().copied() {
            let values: Vec<&str> = next.split_whitespace().collect();
            if values.is_empty() || values.first().is_some_and(|value| *value == "Device") {
                break;
            }
            if values.first().is_some_and(|value| *value == "avg-cpu:") {
                break;
            }
            let _ = lines.next();
            if values.len() < headers.len() + 1 {
                continue;
            }
            for (header, value) in headers.iter().zip(values[1..].iter()) {
                let Some(value) = parse_float(value) else {
                    continue;
                };
                let entry = device_totals.entry(iostat_name(header)).or_default();
                entry.0 += value;
                entry.1 += 1;
            }
        }
    }
    for (name, (sum, count)) in device_totals {
        if count > 0 {
            metrics.insert(format!("device.{name}"), sum / count as f64);
        }
    }
    metrics
}

fn iostat_name(name: &str) -> String {
    let mapped = match name {
        "%user" => Some("user_pct"),
        "%nice" => Some("nice_pct"),
        "%system" => Some("system_pct"),
        "%iowait" => Some("iowait_pct"),
        "%steal" => Some("steal_pct"),
        "%idle" => Some("idle_pct"),
        "r/s" => Some("read_ops_per_s"),
        "rkB/s" => Some("read_kib_per_s"),
        "rrqm/s" => Some("read_merged_ops_per_s"),
        "%rrqm" => Some("read_merged_pct"),
        "r_await" => Some("read_await_ms"),
        "rareq-sz" => Some("read_request_kib"),
        "w/s" => Some("write_ops_per_s"),
        "wkB/s" => Some("write_kib_per_s"),
        "wrqm/s" => Some("write_merged_ops_per_s"),
        "%wrqm" => Some("write_merged_pct"),
        "w_await" => Some("write_await_ms"),
        "wareq-sz" => Some("write_request_kib"),
        "d/s" => Some("discard_ops_per_s"),
        "dkB/s" => Some("discard_kib_per_s"),
        "drqm/s" => Some("discard_merged_ops_per_s"),
        "%drqm" => Some("discard_merged_pct"),
        "d_await" => Some("discard_await_ms"),
        "dareq-sz" => Some("discard_request_kib"),
        "f/s" => Some("flush_ops_per_s"),
        "f_await" => Some("flush_await_ms"),
        "aqu-sz" => Some("queue_depth"),
        "%util" => Some("util_pct"),
        _ => None,
    };
    mapped.map(str::to_owned).unwrap_or_else(|| {
        name.trim_matches('%')
            .replace(['/', '-'], "_")
            .to_lowercase()
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_pidstat_io_metrics_and_series() {
        let io = super::parse_pidstat_io_metrics(
            "Linux 6.1\n12:00:00 UID PID kB_rd/s kB_wr/s kB_ccwr/s iodelay Command\n12:00:01 1000 42 1.0 2.0 0.5 3 agent\n",
        );
        assert_eq!(io["process.io.read_kib_per_s"], 1.0);
        assert_eq!(io["process.io.write_kib_per_s"], 2.0);
        assert_eq!(io["process.io.delay_ticks"], 3.0);
        let series = super::parse_series(
            "Linux 6.1\n12:00:01 UID PID kB_rd/s kB_wr/s kB_ccwr/s iodelay Command\n12:00:01 1000 42 1.0 2.0 0.5 3 agent\n12:00:02 1000 42 2.0 4.0 1.0 5 agent\n",
        );
        assert_eq!(series["process.io.read_kib_per_s"], vec![1.0, 2.0]);
    }

    #[test]
    fn parses_iostat_cpu_and_device_throughput_and_latency() {
        let metrics = super::parse_iostat_metrics(
            "avg-cpu:  %user   %nice %system %iowait  %steal   %idle\n           1.00    2.00    3.00    4.00    5.00   85.00\n\nDevice            r/s     rkB/s   r_await w/s     wkB/s   w_await aqu-sz %util\nvda              10.00   20.00   1.50    30.00   40.00   2.50    0.10   7.00\n",
        );
        assert_eq!(metrics["cpu.user_pct"], 1.0);
        assert_eq!(metrics["cpu.iowait_pct"], 4.0);
        assert_eq!(metrics["device.read_ops_per_s"], 10.0);
        assert_eq!(metrics["device.read_await_ms"], 1.5);
        assert_eq!(metrics["device.write_kib_per_s"], 40.0);
        assert_eq!(metrics["device.util_pct"], 7.0);
    }

    #[test]
    fn io_processing_includes_observations_beyond_the_diagnostic_cap() {
        let mut input = "12:00:00 UID PID kB_rd/s kB_wr/s kB_ccwr/s iodelay Command\n12:00:01 1000 42 10 20 0 1 agent\n".to_owned();
        input.push_str(&"# ignored fixture filler\n".repeat(15000));
        input.push_str("12:00:03 1000 42 30 40 0 3 agent\n");
        assert!(input.len() > 256 * 1024);
        let result =
            crate::collectors::process_output_fixture(&super::ADAPTER, "iostat", &input).unwrap();
        assert_eq!(result.status, "completed");
        assert_eq!(result.metrics["process.io.read_kib_per_s"], 20.0);
        assert_eq!(result.series["process.io.read_kib_per_s"], [10.0, 30.0]);
        assert_eq!(
            result.timestamps["process.io.read_kib_per_s"],
            [1000.0, 3000.0]
        );
        assert!(
            crate::collectors::process_output_fixture(&super::ADAPTER, "iostat", "unfinished")
                .is_err()
        );
    }
}
