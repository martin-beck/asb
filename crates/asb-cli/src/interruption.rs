//! Cooperative command interruption for bounded, owned workload cleanup.

#[cfg(unix)]
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

#[cfg(unix)]
static REQUESTED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Install temporary process signal handlers for one foreground ASB command.
///
/// The handlers perform only an atomic store. Workload controllers observe the
/// request in their bounded polling loop, close their protocol stream, and
/// confirm their own resource cleanup before the command returns.
pub(crate) struct Guard {
    #[cfg(unix)]
    registrations: Vec<signal_hook::SigId>,
}

impl Guard {
    pub(crate) fn install() -> Self {
        #[cfg(unix)]
        {
            let requested = Arc::clone(REQUESTED.get_or_init(|| Arc::new(AtomicBool::new(false))));
            requested.store(false, Ordering::Release);
            let mut registrations = Vec::new();
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                if let Ok(registration) =
                    signal_hook::flag::register(signal, Arc::clone(&requested))
                {
                    registrations.push(registration);
                }
            }
            Self { registrations }
        }
        #[cfg(not(unix))]
        Self {}
    }
}

pub(crate) fn requested() -> bool {
    #[cfg(unix)]
    return REQUESTED
        .get()
        .is_some_and(|requested| requested.load(Ordering::Acquire));
    #[cfg(not(unix))]
    false
}

#[cfg(all(test, unix))]
pub(crate) fn request_for_test() {
    REQUESTED
        .get_or_init(|| Arc::new(AtomicBool::new(false)))
        .store(true, Ordering::Release);
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            for registration in self.registrations.drain(..) {
                signal_hook::low_level::unregister(registration);
            }
            if let Some(requested) = REQUESTED.get() {
                requested.store(false, Ordering::Release);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_starts_without_an_interruption_request() {
        let _guard = Guard::install();
        assert!(!requested());
    }
}
