//! State for the setup wizard's runtime step, independent of terminal rendering.
use super::cpu_pool::CpuPool;
use super::settings::{CpuDescription, CpuPoolSettings, PerfRecordScope, RuntimeSettings};

/// The two supported CPU-affinity choices in setup.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AffinityChoice {
    /// Do not change affinity; configuration remains portable by default.
    #[default]
    None,
    /// Assign each instance a disjoint group from an ordered pool.
    CpuPool,
}

/// Editable runtime choice and optional CPU-wide perf scope.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RuntimeWizard {
    /// Selected affinity option.
    pub affinity: AffinityChoice,
    /// User-edited ordered CPU list.
    pub cpus: String,
    /// User-edited positive group-size text.
    pub num_cpus_per_instance: String,
    /// Opt-in recording scope, independent of perf stat.
    pub perf_record_scope: PerfRecordScope,
    /// Optional configuration-time CPU description for advisory comparison.
    pub cpu_description: Option<CpuDescription>,
}

/// Ordered groups and the maximum concurrency supported by a pool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AllocationPreview {
    /// Number of complete CPU groups in the configured pool.
    pub capacity: usize,
    /// Groups selected for the requested concurrency.
    pub allocations: Vec<Vec<u32>>,
}

impl RuntimeWizard {
    /// Restore editable state from one saved configuration.
    pub fn from_settings(settings: &RuntimeSettings) -> Self {
        let mut result = Self {
            perf_record_scope: settings.perf_record_scope,
            cpu_description: settings.cpu_description.clone(),
            ..Self::default()
        };
        if let Some(pool) = &settings.cpu_pool {
            result.affinity = AffinityChoice::CpuPool;
            result.cpus.clone_from(&pool.cpus);
            result.num_cpus_per_instance = pool.num_cpus_per_instance.to_string();
        }
        result
    }
    /// Validate the current selection and build declarative settings.
    pub fn settings(&self, concurrency: usize) -> Result<RuntimeSettings, String> {
        let cpu_pool = match self.affinity {
            AffinityChoice::None => None,
            AffinityChoice::CpuPool => {
                let count = self
                    .num_cpus_per_instance
                    .parse::<usize>()
                    .map_err(|_| "enter a positive number of CPUs per instance")?;
                Some(CpuPoolSettings {
                    cpus: self.cpus.clone(),
                    num_cpus_per_instance: count,
                })
            }
        };
        let result = RuntimeSettings {
            cpu_pool,
            perf_record_scope: self.perf_record_scope,
            cpu_description: self.cpu_description.clone(),
        };
        result.plan(concurrency)?;
        Ok(result)
    }
    /// Return maximum complete groups and actual allocations for display.
    pub fn preview(&self, concurrency: usize) -> Result<Option<AllocationPreview>, String> {
        let settings = self.settings(concurrency)?;
        let Some(pool) = settings.cpu_pool else {
            return Ok(None);
        };
        let pool = CpuPool::parse(&pool.cpus, pool.num_cpus_per_instance)?;
        Ok(Some(AllocationPreview {
            capacity: pool.capacity(),
            allocations: pool.allocate(concurrency)?,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_is_none_and_saved_pool_reopens_with_allocations() {
        let mut wizard = RuntimeWizard::default();
        assert_eq!(wizard.settings(1).unwrap(), RuntimeSettings::default());
        assert_eq!(wizard.preview(1).unwrap(), None);
        wizard.affinity = AffinityChoice::CpuPool;
        wizard.cpus = "1-3,5-7".into();
        wizard.num_cpus_per_instance = "3".into();
        wizard.perf_record_scope = PerfRecordScope::CpuWide;
        assert_eq!(
            wizard.preview(2).unwrap(),
            Some(AllocationPreview {
                capacity: 2,
                allocations: vec![vec![1, 2, 3], vec![5, 6, 7]]
            })
        );
        assert!(wizard.preview(3).is_err());
        assert_eq!(
            RuntimeWizard::from_settings(&wizard.settings(2).unwrap()),
            wizard
        );
    }
    #[test]
    fn invalid_input_or_cpu_wide_without_pool_cannot_finish() {
        let mut wizard = RuntimeWizard {
            perf_record_scope: PerfRecordScope::CpuWide,
            ..Default::default()
        };
        assert!(wizard.settings(1).is_err());
        wizard.affinity = AffinityChoice::CpuPool;
        wizard.cpus = "1,1".into();
        wizard.num_cpus_per_instance = "1".into();
        assert!(wizard.settings(1).is_err());
        wizard.cpus = "1-3".into();
        for size in ["", "0", "-1", "x"] {
            wizard.num_cpus_per_instance = size.into();
            assert!(wizard.settings(1).is_err());
        }
    }
}
