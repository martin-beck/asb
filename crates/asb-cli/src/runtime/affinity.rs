//! Linux thread-affinity operations for a prepared logical-CPU allocation.
//!
//! These primitives do not place containers or other existing processes. Call
//! them only inside the intended child before work starts, and separately
//! verify every process or workload boundary that must be covered.

use std::io;

const MAX_CPU_ID: u32 = 1_048_575;

/// An immutable, bounded Linux logical-CPU affinity mask.
#[derive(Clone, Debug)]
pub struct AffinityMask {
    words: Vec<usize>,
}

impl AffinityMask {
    /// Build a mask for a nonempty set of distinct logical CPU identifiers.
    pub fn new(cpus: &[u32]) -> Result<Self, String> {
        if cpus.is_empty() {
            return Err("CPU affinity allocation is empty".into());
        }
        let bits = usize::BITS as usize;
        let max = *cpus.iter().max().expect("nonempty");
        if max > MAX_CPU_ID {
            return Err("CPU affinity identifier exceeds the supported bound".into());
        }
        // Large enough for Linux's current kernel mask even when the selected
        // CPU number is small. Zeroed tail bits never request additional CPUs.
        let mut words = vec![0usize; (MAX_CPU_ID as usize + 1).div_ceil(bits)];
        for &cpu in cpus {
            let word = cpu as usize / bits;
            let bit = 1usize << (cpu as usize % bits);
            if words[word] & bit != 0 {
                return Err(format!("CPU affinity repeats logical CPU {cpu}"));
            }
            words[word] |= bit;
        }
        Ok(Self { words })
    }

    /// Set and verify affinity of the calling Linux thread. Descendants inherit
    /// the setting subject to later changes. A failed verification can leave
    /// the caller changed; callers must abort the intended child on failure.
    #[cfg(target_os = "linux")]
    pub fn apply_current(&self) -> io::Result<()> {
        let mut observed = vec![0usize; self.words.len()];
        set_and_read(&self.words, &mut observed)?;
        if observed != self.words {
            return Err(io::Error::other(
                "kernel did not apply the complete requested CPU affinity",
            ));
        }
        Ok(())
    }

    /// Affinity is not available through this implementation on non-Linux hosts.
    #[cfg(not(target_os = "linux"))]
    pub fn apply_current(&self) -> io::Result<()> {
        let _ = &self.words;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "CPU affinity requires Linux",
        ))
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)] // Both pointers refer to owned, aligned, live mask buffers.
fn set_and_read(requested: &[usize], observed: &mut [usize]) -> io::Result<()> {
    let len = std::mem::size_of_val(requested);
    let result = unsafe { libc::sched_setaffinity(0, len, requested.as_ptr().cast()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let result = unsafe {
        libc::sched_getaffinity(
            0,
            std::mem::size_of_val(observed),
            observed.as_mut_ptr().cast(),
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_empty_duplicate_and_out_of_range_masks() {
        assert!(AffinityMask::new(&[]).is_err());
        assert!(AffinityMask::new(&[1, 1]).is_err());
        assert!(AffinityMask::new(&[MAX_CPU_ID + 1]).is_err());
        assert!(AffinityMask::new(&[1, 3]).is_ok());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn isolated_child_applies_and_verifies_an_allowed_cpu() {
        const CHILD: &str = "ASB_TEST_CPU_AFFINITY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
            let allowed = status
                .lines()
                .find_map(|s| s.strip_prefix("Cpus_allowed_list:"))
                .unwrap();
            let pool = super::super::cpu_pool::CpuPool::parse(allowed.trim(), 1).unwrap();
            let cpu = pool.allocate(1).unwrap()[0][0];
            AffinityMask::new(&[cpu]).unwrap().apply_current().unwrap();
            let actual = std::fs::read_to_string("/proc/thread-self/status").unwrap();
            let actual = actual
                .lines()
                .find_map(|s| s.strip_prefix("Cpus_allowed_list:"))
                .unwrap()
                .trim();
            assert_eq!(actual, cpu.to_string());
            assert!(
                AffinityMask::new(&[MAX_CPU_ID])
                    .unwrap()
                    .apply_current()
                    .is_err()
            );
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("runtime::affinity::tests::isolated_child_applies_and_verifies_an_allowed_cpu")
            .arg("--nocapture")
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
    }
}
