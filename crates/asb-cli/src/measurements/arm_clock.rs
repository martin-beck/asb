//! Read the architectural timer only after a disposable child proved access.
//! This is timestamp metadata, never host activity or another process's counters.

#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
pub(super) fn ticks() -> Option<u64> {
    None
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
pub(super) fn ticks() -> Option<u64> {
    static ACCESS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    ACCESS.get_or_init(probe).then(read_counter)
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[allow(unsafe_code)] // Register-only read; probe isolates denied access first.
fn read_counter() -> u64 {
    let value: u64;
    // SAFETY: ISB orders the read; MRS has no memory operands. Only called in
    // the probe child or after that child successfully read the same register.
    unsafe {
        core::arch::asm!("isb", "mrs {value}, cntvct_el0", value=out(reg) value, options(nostack, preserves_flags));
    }
    value
}

#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
#[allow(unsafe_code)] // Exact owned child; child uses only a register read/_exit.
fn probe() -> bool {
    use std::time::{Duration, Instant};
    // SAFETY: the child never returns to Rust, allocates, locks or unwinds. It
    // only reads the timer and calls async-signal-safe _exit. A denied register
    // read terminates this disposable child rather than the benchmark runner.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return false;
    }
    if child == 0 {
        // A denied counter must not create a core dump of inherited memory.
        unsafe {
            libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        }
        let _ = read_counter();
        unsafe {
            libc::_exit(0);
        }
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        let mut status = 0;
        // SAFETY: valid status storage and this probe's exact positive PID.
        let observed = unsafe { libc::waitpid(child, &mut status, libc::WNOHANG) };
        if observed == child {
            return libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
        }
        if observed < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return false;
        }
        if Instant::now() >= deadline {
            // The unreaped child PID cannot be reused. It executes no blocking
            // operation; kill and exact-child reap settle the failed probe.
            unsafe {
                libc::kill(child, libc::SIGKILL);
            }
            loop {
                let result = unsafe { libc::waitpid(child, &mut status, 0) };
                if result >= 0
                    || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
                {
                    break;
                }
            }
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
