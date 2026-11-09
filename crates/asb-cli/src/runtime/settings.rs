//! Declarative runtime settings and host-independent admission checks.
use super::cpu_pool::CpuPool;
use serde::{Deserialize, Serialize};

/// Optional CPU pool description in a benchmark runtime configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CpuPoolSettings {
    /// Ordered comma-separated logical CPU IDs and inclusive ranges.
    pub cpus: String,
    /// Logical CPUs assigned to each concurrent instance.
    pub num_cpus_per_instance: usize,
}

/// How perf records samples. Perf stat remains process-tree scoped.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PerfRecordScope {
    /// Record only the agent's process tree.
    #[default]
    ProcessTree,
    /// Record every task executing on the instance's allocated CPUs.
    CpuWide,
}

/// Advisory characteristics captured during configuration, never host identity.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CpuDescription {
    /// Machine architecture, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    /// CPU model, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Observed logical CPU count, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_cpus: Option<usize>,
}

impl CpuDescription {
    /// Capture advisory local characteristics without binding a configuration
    /// to a machine. Missing or unreadable observations remain unknown.
    pub fn current() -> Self {
        #[cfg(target_os = "linux")]
        let (model, logical_cpus) = {
            let model = super::cpu_pool::read_small("/proc/cpuinfo")
                .ok()
                .and_then(|text| {
                    text.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        (key.trim() == "model name")
                            .then(|| value.trim().to_owned())
                            .filter(|value| !value.is_empty())
                    })
                });
            let logical_cpus = super::cpu_pool::read_small("/sys/devices/system/cpu/present")
                .ok()
                .and_then(|value| CpuPool::parse(value.trim(), 1).ok())
                .map(|pool| pool.capacity());
            (model, logical_cpus)
        };
        #[cfg(not(target_os = "linux"))]
        let (model, logical_cpus) = (None, None);
        Self {
            architecture: Some(std::env::consts::ARCH.into()),
            model,
            logical_cpus,
        }
    }
    /// Human-readable differences; absent observations never become mismatches.
    pub fn differences(&self, actual: &Self) -> Vec<&'static str> {
        let mut differences = Vec::new();
        if self
            .architecture
            .as_ref()
            .zip(actual.architecture.as_ref())
            .is_some_and(|(a, b)| a != b)
        {
            differences.push("architecture");
        }
        if self
            .model
            .as_ref()
            .zip(actual.model.as_ref())
            .is_some_and(|(a, b)| a != b)
        {
            differences.push("CPU model");
        }
        if self
            .logical_cpus
            .zip(actual.logical_cpus)
            .is_some_and(|(a, b)| a != b)
        {
            differences.push("logical CPU count");
        }
        differences
    }
}

/// Portable runtime intent; no process affinity is applied by this type.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSettings {
    /// Absent means no affinity changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pool: Option<CpuPoolSettings>,
    /// The requested perf recording scope.
    #[serde(default)]
    pub perf_record_scope: PerfRecordScope,
    /// Optional configuration-time description for advisory comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_description: Option<CpuDescription>,
}

impl RuntimeSettings {
    /// Validate static settings and return the selected ordered allocation.
    pub fn plan(&self, concurrency: usize) -> Result<Option<Vec<Vec<u32>>>, String> {
        if concurrency == 0 {
            return Err("concurrency must be greater than zero".into());
        }
        if concurrency > crate::benchmark::MAX_CONCURRENCY {
            return Err(format!(
                "concurrency exceeds the ASB limit of {}",
                crate::benchmark::MAX_CONCURRENCY
            ));
        }
        match &self.cpu_pool {
            Some(pool) => Ok(Some(
                CpuPool::parse(&pool.cpus, pool.num_cpus_per_instance)?.allocate(concurrency)?,
            )),
            None if self.perf_record_scope == PerfRecordScope::CpuWide => {
                Err("CPU-wide perf recording requires a CPU pool".into())
            }
            None => Ok(None),
        }
    }
}

/// Ask once for an advisory configuration-time CPU mismatch across an entire
/// invocation. Missing observations do not count as mismatches.
pub(crate) fn confirm_topology<'a>(
    settings: impl Iterator<Item = &'a RuntimeSettings>,
    interactive: bool,
    output: &mut dyn std::io::Write,
) -> Result<(), crate::Error> {
    use std::io::IsTerminal;
    let actual = CpuDescription::current();
    let differences: std::collections::BTreeSet<_> = settings
        .filter(|settings| settings.cpu_pool.is_some())
        .filter_map(|settings| settings.cpu_description.as_ref())
        .flat_map(|saved| saved.differences(&actual))
        .collect();
    let terminal = interactive && std::io::stdin().is_terminal();
    confirm_differences(&differences, terminal, &mut std::io::stdin().lock(), output)
}

fn confirm_differences(
    differences: &std::collections::BTreeSet<&str>,
    interactive: bool,
    input: &mut dyn std::io::BufRead,
    output: &mut dyn std::io::Write,
) -> Result<(), crate::Error> {
    if differences.is_empty() {
        return Ok(());
    }
    if !interactive {
        return Err(crate::Error::Input(format!(
            "CPU topology differs from setup ({}); rerun interactively to review and confirm",
            differences.iter().copied().collect::<Vec<_>>().join(", ")
        )));
    }
    write!(output, "CPU topology differs from setup ({}). The selected logical CPUs were validated on this host. Continue? [y/N] ", differences.iter().copied().collect::<Vec<_>>().join(", "))
        .map_err(crate::io_error)?;
    output.flush().map_err(crate::io_error)?;
    let mut answer = String::new();
    input.read_line(&mut answer).map_err(crate::io_error)?;
    if answer.trim().eq_ignore_ascii_case("y") {
        writeln!(output, "Continuing with the current host CPU topology.")
            .map_err(crate::io_error)?;
        Ok(())
    } else {
        Err(crate::Error::Input(
            "benchmark cancelled at the CPU topology warning".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topology_mismatch_requires_explicit_confirmation_and_no_mismatch_is_silent() {
        let mismatches = std::collections::BTreeSet::from(["CPU model", "logical CPU count"]);
        for (answer, accepted) in [("y\n", true), ("Y\n", true), ("n\n", false), ("\n", false)] {
            let mut output = Vec::new();
            let result = confirm_differences(
                &mismatches,
                true,
                &mut std::io::Cursor::new(answer),
                &mut output,
            );
            assert_eq!(result.is_ok(), accepted);
            assert!(String::from_utf8(output).unwrap().contains("[y/N]"));
        }
        assert!(
            confirm_differences(
                &mismatches,
                false,
                &mut std::io::Cursor::new("y\n"),
                &mut Vec::new()
            )
            .is_err()
        );
        let mut output = Vec::new();
        assert!(
            confirm_differences(
                &Default::default(),
                false,
                &mut std::io::Cursor::new(""),
                &mut output
            )
            .is_ok()
        );
        assert!(output.is_empty());
    }
    #[test]
    fn defaults_are_portable_and_cpu_wide_requires_a_pool() {
        let default: RuntimeSettings = toml::from_str("").unwrap();
        assert_eq!(default.plan(1).unwrap(), None);
        assert_eq!(default.perf_record_scope, PerfRecordScope::ProcessTree);
        let cpu_wide: RuntimeSettings = toml::from_str("perf_record_scope='cpu-wide'").unwrap();
        assert!(
            cpu_wide
                .plan(1)
                .unwrap_err()
                .contains("requires a CPU pool")
        );
        assert!(default.plan(0).is_err());
    }
    #[test]
    fn settings_roundtrip_and_reject_unknown_fields_and_over_capacity() {
        let settings: RuntimeSettings = toml::from_str(
            "perf_record_scope='cpu-wide'\n[cpu_pool]\ncpus='1-3,5-7'\nnum_cpus_per_instance=3\n",
        )
        .unwrap();
        assert_eq!(
            settings.plan(2).unwrap().unwrap(),
            vec![vec![1, 2, 3], vec![5, 6, 7]]
        );
        assert!(settings.plan(3).is_err());
        assert_eq!(
            toml::from_str::<RuntimeSettings>(&toml::to_string(&settings).unwrap()).unwrap(),
            settings
        );
        assert!(toml::from_str::<RuntimeSettings>("unknown=1").is_err());
        assert!(
            toml::from_str::<RuntimeSettings>(
                "[cpu_pool]\ncpus='1'\nnum_cpus_per_instance=1\nextra=1"
            )
            .is_err()
        );
    }
    #[test]
    fn topology_difference_is_advisory_and_unknown_does_not_mismatch() {
        let saved = CpuDescription {
            architecture: Some("aarch64".into()),
            model: Some("A".into()),
            logical_cpus: Some(64),
        };
        let actual = CpuDescription {
            architecture: Some("x86_64".into()),
            model: None,
            logical_cpus: Some(32),
        };
        assert_eq!(
            saved.differences(&actual),
            vec!["architecture", "logical CPU count"]
        );
        assert!(saved.differences(&CpuDescription::default()).is_empty());
        assert_eq!(
            CpuDescription::current().architecture.as_deref(),
            Some(std::env::consts::ARCH)
        );
    }
}
