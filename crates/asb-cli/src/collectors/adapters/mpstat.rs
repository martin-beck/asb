use super::super::{CollectorAdapter, ProcessContext, ProcessData};
use super::pidstat::parse_columns;
use crate::measurements;
use crate::measurements::parse_float;
use std::collections::BTreeMap;
pub(super) struct MpstatCollector;
pub(super) static ADAPTER: MpstatCollector = MpstatCollector;

impl CollectorAdapter for MpstatCollector {
    fn description(&self) -> &'static str {
        "per-CPU utilization time series"
    }
    fn id(&self) -> &'static str {
        "mpstat"
    }
    fn environment_flag(&self) -> Option<&'static str> {
        Some("ASB_MEASURE_MPSTAT")
    }
    fn tool_id(&self) -> Option<&'static str> {
        Some("mpstat")
    }
    fn requires_pid(&self) -> bool {
        true
    }
    fn preflight(&self, context: &mut super::super::PreflightContext<'_>) -> Result<(), String> {
        if super::super::selected_or_environment("mpstat", context.selected) {
            super::pidstat::preflight(context.tool_dir, "-u")
        } else {
            Ok(())
        }
    }
    fn configure(
        &self,
        session: &mut measurements::Session,
        selected: &[String],
    ) -> Result<(), String> {
        session.state::<CpuState>().borrow_mut().enabled =
            super::super::selected_or_environment("mpstat", selected);
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
        let enabled = session.state::<CpuState>().borrow().enabled;
        super::pidstat::start_snapshot(session, "mpstat", enabled, pid, "-u")
    }
}

fn process(
    _: &mut measurements::Session,
    context: &ProcessContext<'_>,
) -> Result<ProcessData, String> {
    let mut result = ProcessData::from_output(context);
    let output = super::super::read_complete_output(context.stdout_reader()?)?;
    result.metrics = parse_pidstat_cpu_metrics(&output);
    if result.metrics.is_empty() {
        result.metrics = parse_mpstat_metrics(&output);
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
        &["%usr", "%system", "%guest", "%wait", "%CPU"],
        |name| {
            format!(
                "process.cpu.{}",
                match name {
                    "%usr" => "user_pct",
                    "%system" => "system_pct",
                    "%guest" => "guest_pct",
                    "%wait" => "wait_pct",
                    "%CPU" => "total_pct",
                    _ => "unknown",
                }
            )
        },
    )
}

#[derive(Default)]
struct CpuState {
    enabled: bool,
}

pub(crate) fn parse_mpstat_metrics(output: &str) -> BTreeMap<String, f64> {
    let mut header = None;
    let mut totals: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if let Some(cpu) = fields.iter().position(|field| *field == "CPU")
            && fields
                .get(cpu + 1)
                .is_some_and(|field| field.starts_with('%'))
        {
            header = Some((cpu, fields[cpu + 1..].to_vec()));
            continue;
        }
        let Some((cpu_index, names)) = &header else {
            continue;
        };
        if fields.get(*cpu_index).copied() != Some("all") {
            continue;
        }
        for (index, name) in names.iter().enumerate() {
            let Some(value) = fields
                .get(cpu_index + 1 + index)
                .and_then(|field| parse_float(field))
            else {
                continue;
            };
            let key = mpstat_name(name);
            let entry = totals.entry(key).or_default();
            entry.0 += value;
            entry.1 += 1;
        }
    }
    totals
        .into_iter()
        .filter_map(|(name, (sum, count))| {
            (count > 0).then_some((format!("cpu.{name}"), sum / count as f64))
        })
        .collect()
}

/// Parse process-scoped `pidstat -u` output. The collector is retained under
/// the stable `mpstat` result name for compatibility, but it no longer uses
/// host-wide mpstat CPU totals.
pub(crate) fn parse_pidstat_cpu_metrics(output: &str) -> BTreeMap<String, f64> {
    parse_columns(
        output,
        &["%usr", "%system", "%guest", "%wait", "%CPU"],
        |name| {
            format!(
                "process.cpu.{}",
                match name {
                    "%usr" => "user_pct",
                    "%system" => "system_pct",
                    "%guest" => "guest_pct",
                    "%wait" => "wait_pct",
                    "%CPU" => "total_pct",
                    _ => "unknown",
                }
            )
        },
    )
}

fn mpstat_name(name: &str) -> String {
    let mapped = match name.trim_start_matches('%') {
        "usr" => Some("user_pct"),
        "nice" => Some("nice_pct"),
        "sys" => Some("system_pct"),
        "iowait" => Some("iowait_pct"),
        "irq" => Some("irq_pct"),
        "soft" => Some("soft_pct"),
        "steal" => Some("steal_pct"),
        "guest" => Some("guest_pct"),
        "gnice" => Some("guest_nice_pct"),
        _ => None,
    };
    mapped
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{}_pct", name.trim_start_matches('%')))
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_mpstat_cpu_breakdown_into_averaged_metrics() {
        let metrics = super::parse_mpstat_metrics(
            "12:00:00 AM CPU %usr %nice %sys %iowait %irq %soft %steal %guest %gnice %idle\n12:00:00 AM all 1.0 2.0 3.0 4.0 5.0 6.0 7.0 8.0 9.0 55.0\n",
        );
        assert_eq!(metrics["cpu.user_pct"], 1.0);
        assert_eq!(metrics["cpu.system_pct"], 3.0);
        assert_eq!(metrics["cpu.iowait_pct"], 4.0);
        assert_eq!(metrics["cpu.guest_nice_pct"], 9.0);
        assert_eq!(metrics["cpu.idle_pct"], 55.0);
    }

    #[test]
    fn parses_process_scoped_pidstat_cpu_metrics() {
        let cpu = super::parse_pidstat_cpu_metrics(
            "Linux 6.1\n12:00:00 UID PID %usr %system %guest %wait %CPU CPU Command\n12:00:01 1000 42 10.00 2.00 0.00 1.00 12.00 3 agent\n12:00:02 1000 42 20.00 4.00 0.00 0.00 24.00 3 agent\n",
        );
        assert_eq!(cpu["process.cpu.user_pct"], 15.0);
        assert_eq!(cpu["process.cpu.system_pct"], 3.0);
        assert_eq!(cpu["process.cpu.total_pct"], 18.0);
        let series = super::parse_series(
            "Linux 6.1\n12:00:00 UID PID %usr %system %guest %wait %CPU CPU Command\n12:00:01 1000 42 10.00 2.00 0.00 1.00 12.00 3 agent\n12:00:02 1000 42 20.00 4.00 0.00 0.00 24.00 3 agent\n",
        );
        assert_eq!(series["process.cpu.user_pct"], vec![10.0, 20.0]);
        assert_eq!(series["process.cpu.total_pct"], vec![12.0, 24.0]);
    }

    #[test]
    fn cpu_processing_includes_observations_beyond_the_diagnostic_cap() {
        let mut input = "12:00:00 UID PID %CPU Command\n12:00:01 1000 42 20 agent\n".to_owned();
        input.push_str(&"# ignored fixture filler\n".repeat(15000));
        input.push_str("12:00:03 1000 42 60 agent\n");
        assert!(input.len() > 256 * 1024);
        let result =
            crate::collectors::process_output_fixture(&super::ADAPTER, "mpstat", &input).unwrap();
        assert_eq!(result.status, "completed");
        assert_eq!(result.metrics["process.cpu.total_pct"], 40.0);
        assert_eq!(result.series["process.cpu.total_pct"], [20.0, 60.0]);
        assert_eq!(result.timestamps["process.cpu.total_pct"], [1000.0, 3000.0]);
        assert!(
            crate::collectors::process_output_fixture(&super::ADAPTER, "mpstat", "unfinished")
                .is_err()
        );
    }
}
