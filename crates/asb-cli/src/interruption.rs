//! Cooperative command interruption for bounded, owned workload cleanup.

#[cfg(unix)]
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

#[cfg(all(test, unix))]
use std::cell::Cell;

#[cfg(unix)]
static REQUESTED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

// Unit tests need to exercise an interruption without delivering a process
// signal. Keep that synthetic request local to its test thread so unrelated
// concurrently executing tests cannot be cancelled by the fixture.
#[cfg(all(test, unix))]
thread_local! {
    static TEST_REQUESTED: Cell<bool> = const { Cell::new(false) };
}

/// Internal ASB helpers have their own protocol and lifecycle signals.  Only
/// the user-facing command may install the foreground interruption observer.
pub(crate) fn foreground(args: &[std::ffi::OsString]) -> bool {
    !args
        .first()
        .and_then(|argument| argument.to_str())
        .is_some_and(|argument| argument.starts_with("__asb-"))
}

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
    #[cfg(all(test, unix))]
    if TEST_REQUESTED.with(Cell::get) {
        return true;
    }
    #[cfg(unix)]
    return REQUESTED
        .get()
        .is_some_and(|requested| requested.load(Ordering::Acquire));
    #[cfg(not(unix))]
    false
}

/// Stop starting new work after a foreground interruption.  Callers that own
/// already-admitted work must still settle it before returning.
pub(crate) fn check() -> Result<(), crate::Error> {
    if requested() {
        Err(crate::Error::Run(
            "ASB interruption requested graceful setup shutdown; waiting for owned work to settle"
                .into(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(all(test, unix))]
pub(crate) fn request_for_test() {
    TEST_REQUESTED.with(|requested| requested.set(true));
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
            #[cfg(test)]
            TEST_REQUESTED.with(|requested| requested.set(false));
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

    #[test]
    fn only_user_facing_commands_install_the_foreground_guard() {
        assert!(foreground(&["sweep".into()]));
        assert!(foreground(&[]));
        assert!(!foreground(&["__asb-gate".into()]));
        assert!(!foreground(&["__asb-agent".into()]));
    }

    #[cfg(unix)]
    #[test]
    fn check_reports_a_graceful_setup_shutdown_request() {
        let _guard = Guard::install();
        request_for_test();
        assert!(
            check()
                .unwrap_err()
                .to_string()
                .contains("graceful setup shutdown")
        );
    }

    #[cfg(unix)]
    #[test]
    fn synthetic_request_is_confined_to_its_test_thread() {
        let child = std::thread::spawn(|| {
            request_for_test();
            assert!(requested());
        });
        child.join().unwrap();
        assert!(
            !requested(),
            "a synthetic interruption must not affect a concurrent test"
        );
    }

    #[cfg(unix)]
    #[test]
    fn guard_observes_a_real_term_signal() {
        let _guard = Guard::install();
        let status = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        for _ in 0..100 {
            if requested() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("SIGTERM handler did not request controlled interruption");
    }
}
