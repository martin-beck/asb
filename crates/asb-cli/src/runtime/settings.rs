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

#[cfg(test)]
mod tests {
    use super::*;
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
    }
}
