//! Cooperative, persistent-inode CPU leases for ASB campaigns.
//!
//! All cooperating campaigns must use the same host-local lock directory.
//! These locks do not reserve CPUs from the operating system, noncooperating
//! programs or users who cannot access the shared directory. The owner must
//! hold the lease until every placed process and container has settled.
use std::collections::BTreeSet;
use std::path::Path;

/// Holds advisory locks for a collection of logical CPU IDs.
pub struct CpuLease {
    _locks: Vec<crate::output_lock::OutputLock>,
}

impl CpuLease {
    /// Try to acquire all CPUs without waiting. On conflict or error, every
    /// already-acquired lock is released and no lock file is removed.
    pub fn try_acquire(directory: &Path, cpus: &[u32]) -> Result<Self, String> {
        let sorted: BTreeSet<u32> = cpus.iter().copied().collect();
        if sorted.is_empty()
            || sorted.len() != cpus.len()
            || sorted.len() > 65_536
            || sorted.last().is_some_and(|cpu| *cpu > 1_048_575)
        {
            return Err(
                "CPU lease requires a nonempty bounded set of distinct logical CPU IDs".into(),
            );
        }
        std::fs::create_dir_all(directory).map_err(|e| {
            format!(
                "cannot create CPU lease directory {}: {e}",
                directory.display()
            )
        })?;
        let mut locks = Vec::with_capacity(sorted.len());
        for cpu in sorted {
            let path = directory.join(format!("cpu-{cpu}.lock"));
            let busy =
                format!("logical CPU {cpu} is already leased by another cooperating ASB campaign");
            locks.push(crate::output_lock::OutputLock::acquire(&path, &busy)?);
        }
        Ok(Self { _locks: locks })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlapping_campaigns_conflict_and_partial_acquisitions_release() {
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        let held = CpuLease::try_acquire(&root, &[3, 1]).unwrap();
        assert!(CpuLease::try_acquire(&root, &[2, 3]).is_err());
        let separate = CpuLease::try_acquire(&root, &[2, 4]).unwrap();
        assert!(CpuLease::try_acquire(&root, &[1, 1]).is_err());
        drop(held);
        let released = CpuLease::try_acquire(&root, &[1, 3]).unwrap();
        drop((separate, released));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn lease_conflicts_across_processes() {
        const CHILD: &str = "ASB_TEST_CPU_LEASE_DIRECTORY";
        if let Some(path) = std::env::var_os(CHILD) {
            assert!(CpuLease::try_acquire(Path::new(&path), &[7]).is_err());
            return;
        }
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        let held = CpuLease::try_acquire(&root, &[7]).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("runtime::cpu_lease::tests::lease_conflicts_across_processes")
            .env(CHILD, &root)
            .status()
            .unwrap();
        assert!(status.success());
        drop(held);
        std::fs::remove_dir_all(root).unwrap();
    }
}
