//! Target-specific CPU allocation preparation. The caller must keep admission
//! alive until all placed processes and managed containers have settled.

use std::path::Path;

use super::{
    affinity::AffinityMask, cpu_lease::CpuLease, cpu_pool::CpuPool, settings::RuntimeSettings,
};

/// Validated replica allocations and their cooperative campaign lease.
pub struct Admission {
    allocations: Vec<Vec<u32>>,
    masks: Vec<AffinityMask>,
    _lease: CpuLease,
}

impl Admission {
    /// Validate against the execution thread's current target and reserve each
    /// selected group amongst campaigns using the same host-local lock directory.
    /// No process or container is pinned by this operation.
    pub fn acquire(
        settings: &RuntimeSettings,
        concurrency: usize,
        lock_directory: &Path,
    ) -> Result<Option<Self>, String> {
        let Some(allocations) = settings.plan(concurrency)? else {
            return Ok(None);
        };
        let pool = settings
            .cpu_pool
            .as_ref()
            .ok_or("CPU allocations require a CPU pool")?;
        CpuPool::parse(&pool.cpus, pool.num_cpus_per_instance)?.validate_current_target()?;
        let masks = allocations
            .iter()
            .map(|group| AffinityMask::new(group))
            .collect::<Result<Vec<_>, _>>()?;
        let cpus = allocations.iter().flatten().copied().collect::<Vec<_>>();
        let lease = CpuLease::try_acquire(lock_directory, &cpus)?;
        Ok(Some(Self {
            allocations,
            masks,
            _lease: lease,
        }))
    }

    /// Return the stable allocation and precomputed mask for a one-based replica.
    pub fn replica(&self, index: usize) -> Option<(&[u32], &AffinityMask)> {
        let slot = index.checked_sub(1)?;
        Some((
            self.allocations.get(slot)?.as_slice(),
            self.masks.get(slot)?,
        ))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::super::settings::CpuPoolSettings;
    use super::*;

    #[test]
    fn portable_admission_has_no_lease_or_directory() {
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        assert!(
            Admission::acquire(&RuntimeSettings::default(), 1, &root)
                .unwrap()
                .is_none()
        );
        assert!(!root.exists());
    }

    #[test]
    fn absent_target_cpu_fails_before_lock_creation() {
        let settings = RuntimeSettings {
            cpu_pool: Some(CpuPoolSettings {
                cpus: "1048575".into(),
                num_cpus_per_instance: 1,
            }),
            ..Default::default()
        };
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        assert!(Admission::acquire(&settings, 1, &root).is_err());
        assert!(!root.exists());
    }

    #[test]
    fn admission_validates_target_and_holds_campaign_lease() {
        let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        let allowed = status
            .lines()
            .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
            .unwrap()
            .trim();
        let first = allowed
            .split(',')
            .next()
            .unwrap()
            .split('-')
            .next()
            .unwrap();
        let settings = RuntimeSettings {
            cpu_pool: Some(CpuPoolSettings {
                cpus: first.into(),
                num_cpus_per_instance: 1,
            }),
            ..Default::default()
        };
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        let admission = Admission::acquire(&settings, 1, &root).unwrap().unwrap();
        assert_eq!(
            admission.replica(1).unwrap().0,
            &[first.parse::<u32>().unwrap()]
        );
        assert!(admission.replica(0).is_none() && admission.replica(2).is_none());
        assert!(Admission::acquire(&settings, 1, &root).is_err());
        drop(admission);
        assert!(Admission::acquire(&settings, 1, &root).unwrap().is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}
