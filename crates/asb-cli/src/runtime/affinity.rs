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

    /// Configure an owned child to apply and verify this mask after fork and
    /// before exec. The parent thread is unchanged. The caller still owns
    /// process launch, readiness, cancellation, and child cleanup.
    #[cfg(target_os = "linux")]
    #[allow(unsafe_code)] // The closure uses only affinity syscalls and preallocated memory.
    pub fn prepare_command(&self, command: &mut std::process::Command) -> io::Result<()> {
        use std::os::unix::process::CommandExt;
        let requested = self.words.clone();
        let mut observed = vec![0usize; requested.len()];
        unsafe {
            command.pre_exec(move || {
                observed.fill(0);
                set_and_read(&requested, &mut observed)?;
                if observed != requested {
                    return Err(io::Error::from_raw_os_error(libc::EINVAL));
                }
                Ok(())
            });
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

    /// Preparing a child affinity is currently supported only on Linux.
    #[cfg(not(target_os = "linux"))]
    pub fn prepare_command(&self, _: &mut std::process::Command) -> io::Result<()> {
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
        const GRANDCHILD: &str = "ASB_TEST_CPU_AFFINITY_GRANDCHILD";
        if let Some(expected) = std::env::var_os(GRANDCHILD) {
            let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
            let actual = status
                .lines()
                .find_map(|s| s.strip_prefix("Cpus_allowed_list:"))
                .unwrap()
                .trim();
            assert_eq!(std::ffi::OsStr::new(actual), expected);
            return;
        }
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
            let inherited = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("runtime::affinity::tests::isolated_child_applies_and_verifies_an_allowed_cpu")
                .env(GRANDCHILD, cpu.to_string())
                .status()
                .unwrap();
            assert!(inherited.success());
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

    #[cfg(target_os = "linux")]
    #[test]
    fn prepared_command_pins_before_child_starts_without_pinning_parent() {
        const EXPECTED: &str = "ASB_TEST_CPU_AFFINITY_EXPECTED";
        let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        let allowed = status
            .lines()
            .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
            .unwrap()
            .trim();
        if let Some(expected) = std::env::var_os(EXPECTED) {
            assert_eq!(std::ffi::OsStr::new(allowed), expected);
            return;
        }
        let pool = super::super::cpu_pool::CpuPool::parse(allowed, 1).unwrap();
        let cpu = pool.allocate(1).unwrap()[0][0];
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.arg("--exact").arg("runtime::affinity::tests::prepared_command_pins_before_child_starts_without_pinning_parent").env(EXPECTED, cpu.to_string());
        AffinityMask::new(&[cpu])
            .unwrap()
            .prepare_command(&mut command)
            .unwrap();
        assert!(command.status().unwrap().success());
        let after = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        let after = after
            .lines()
            .find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
            .unwrap()
            .trim();
        assert_eq!(after, allowed);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_child_affinity_never_starts_the_program() {
        const MARKER: &str = "ASB_TEST_CPU_AFFINITY_MARKER";
        if let Some(path) = std::env::var_os(MARKER) {
            std::fs::write(path, b"unexpected execution").unwrap();
            return;
        }
        let present = std::fs::read_to_string("/sys/devices/system/cpu/present").unwrap();
        let pool = super::super::cpu_pool::CpuPool::parse(present.trim(), 1).unwrap();
        let used = pool
            .allocate(pool.capacity())
            .unwrap()
            .into_iter()
            .flatten()
            .collect::<std::collections::BTreeSet<_>>();
        let unavailable = (0..=MAX_CPU_ID)
            .rev()
            .find(|cpu| !used.contains(cpu))
            .expect("bounded pool leaves an unavailable CPU");
        let marker = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("runtime::affinity::tests::failed_child_affinity_never_starts_the_program")
            .env(MARKER, &marker);
        AffinityMask::new(&[unavailable])
            .unwrap()
            .prepare_command(&mut command)
            .unwrap();
        assert!(command.spawn().is_err());
        assert!(!marker.exists());
    }
}
