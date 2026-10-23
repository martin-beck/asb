//! Pre-established fallback custody for directly owned child processes.
//!
//! Create this worker fallibly before spawning children. Callers perform their
//! own bounded shutdown first, then transfer any unconfirmed children here.
//! This does not supervise descendants or provide crash-persistent recovery.
use std::process::Child;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

fn poll_owned(child: &mut Child) -> std::io::Result<Option<std::process::ExitStatus>> {
    #[cfg(all(test, target_os = "linux"))]
    test_support::observation_fault(child)?;
    child.try_wait()
}

#[derive(Default)]
struct State {
    children: Vec<(Child, ReapPolicy)>,
    closed: bool,
}

enum ReapPolicy {
    Terminate,
    WaitOnly,
}

/// A private handoff cannot disconnect like a channel. The worker retains its
/// Arc until all transferred children are reaped; Drop joins it only when no
/// child was handed off.
pub(crate) struct OwnedReaper {
    state: Arc<(Mutex<State>, Condvar)>,
    // Retain the handle so the overwhelmingly common no-handoff path can
    // synchronously retire its one custody worker.  A handoff deliberately
    // remains detached: it may need to wait for a live child, and Drop must
    // never turn that uncertainty into an unbounded caller wait.
    worker: Option<JoinHandle<()>>,
}

impl OwnedReaper {
    pub(crate) fn new(label: &str) -> Result<Self, String> {
        #[cfg(all(test, target_os = "linux"))]
        test_support::check_creation(label)?;
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker = Arc::clone(&shared);
        #[cfg(all(test, target_os = "linux"))]
        let receipt = test_support::worker_receipt();
        #[cfg(all(test, target_os = "linux"))]
        let fault = test_support::worker_fault();
        let worker = std::thread::Builder::new()
            .name(format!("asb-{label}-reaper"))
            .spawn(move || {
                #[cfg(all(test, target_os = "linux"))]
                test_support::bind_worker_fault(fault);
                let (state, ready) = &*worker;
                let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
                while !state.closed {
                    state = ready.wait(state).unwrap_or_else(|error| error.into_inner());
                }
                let mut children = std::mem::take(&mut state.children);
                drop(state); // Never hold the handoff lock across a kernel wait.
                while !children.is_empty() {
                    // One nonblocking observation per owner in each round.
                    // A held/uncertain first child must not prevent independent
                    // siblings from receiving termination or actual reap work.
                    children.retain_mut(|(child, policy)| match poll_owned(child) {
                        Ok(Some(_)) => false, // Actual exit/reap confirmed.
                        Ok(None) => {
                            if matches!(policy, ReapPolicy::Terminate) {
                                let _ = child.kill();
                            }
                            true
                        }
                        // Observation uncertainty never authorizes a signal or
                        // discarding this exact Child. WaitOnly NEVER signals.
                        Err(_) => true,
                    });
                    if !children.is_empty() {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
                #[cfg(all(test, target_os = "linux"))]
                test_support::confirm_worker_exit(receipt);
            })
            .map_err(|error| format!("cannot establish {label} cleanup worker: {error}"))?;
        Ok(Self {
            state: shared,
            worker: Some(worker),
        })
    }

    /// Transfer custody without creating another worker or waiting for exit.
    /// The worker starts reaping when this handoff owner is dropped.
    pub(crate) fn adopt(&self, child: Child) {
        self.adopt_with_policy(child, ReapPolicy::Terminate);
    }

    /// Keep a supervising gate alive until it confirms and finishes its own
    /// descendant teardown. Killing this child would abandon its custody.
    pub(crate) fn adopt_wait_only(&self, child: Child) {
        self.adopt_with_policy(child, ReapPolicy::WaitOnly);
    }

    fn adopt_with_policy(&self, child: Child, policy: ReapPolicy) {
        self.state
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .children
            .push((child, policy));
    }
}

impl Drop for OwnedReaper {
    fn drop(&mut self) {
        let (state, ready) = &*self.state;
        // If no child was handed off, the worker has no cleanup work.  Join it
        // here so one successful gate admission cannot leave a transient
        // `asb-*-reaper` thread behind for every benchmark attempt.
        let idle = {
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            state.closed = true;
            state.children.is_empty()
        };
        ready.notify_one();
        if idle && let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) mod test_support {
    use std::cell::{Cell, RefCell};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    };
    use std::time::{Duration, Instant};

    thread_local! {
        static FAIL_CREATION: Cell<bool> = const { Cell::new(false) };
        static WORKERS: RefCell<Option<Vec<Arc<AtomicBool>>>> = const { RefCell::new(None) };
        static OBSERVATION_FAULT: RefCell<Option<Arc<ObservationState>>> = const { RefCell::new(None) };
    }

    pub(super) struct ObservationState {
        enabled: AtomicBool,
        observed: AtomicBool,
        target: Option<AtomicU32>,
    }

    /// Parent control is local to the worker created while this thread is armed.
    /// Drop releases the controlled fault even if a test assertion unwinds.
    pub(crate) struct ObservationFault(Arc<ObservationState>);
    impl ObservationFault {
        pub(crate) fn arm() -> Self {
            Self::arm_inner(false)
        }
        pub(crate) fn arm_targeted() -> Self {
            Self::arm_inner(true)
        }
        fn arm_inner(targeted: bool) -> Self {
            let state = Arc::new(ObservationState {
                enabled: AtomicBool::new(true),
                observed: AtomicBool::new(false),
                target: targeted.then(|| AtomicU32::new(0)),
            });
            OBSERVATION_FAULT.with_borrow_mut(|fault| *fault = Some(Arc::clone(&state)));
            Self(state)
        }
        /// Select only a Child already owned by this fixture, before handoff.
        /// This keeper-local ID filters an injected observation error; it never
        /// authorizes signaling, discovery or acquisition by numeric PID.
        pub(crate) fn select_owned(&self, child: &std::process::Child) {
            self.0
                .target
                .as_ref()
                .expect("targeted observation fixture")
                .store(child.id(), Ordering::Release);
        }
        pub(crate) fn observed(&self) -> bool {
            self.0.observed.load(Ordering::Acquire)
        }
        pub(crate) fn release(&self) {
            self.0.enabled.store(false, Ordering::Release);
        }
    }
    impl Drop for ObservationFault {
        fn drop(&mut self) {
            self.release();
        }
    }
    pub(super) fn worker_fault() -> Option<Arc<ObservationState>> {
        OBSERVATION_FAULT.with_borrow(Clone::clone)
    }
    pub(super) fn bind_worker_fault(fault: Option<Arc<ObservationState>>) {
        OBSERVATION_FAULT.with_borrow_mut(|current| *current = fault);
    }
    pub(super) fn observation_fault(child: &std::process::Child) -> std::io::Result<()> {
        OBSERVATION_FAULT.with_borrow(|fault| {
            if let Some(fault) = fault
                && fault.enabled.load(Ordering::Acquire)
                && fault
                    .target
                    .as_ref()
                    .is_none_or(|target| target.load(Ordering::Acquire) == child.id())
            {
                fault.observed.store(true, Ordering::Release);
                return Err(std::io::Error::other(
                    "controlled fixture observation error, not a kernel failure",
                ));
            }
            Ok(())
        })
    }

    pub(crate) fn set_creation_failure(enabled: bool) {
        FAIL_CREATION.set(enabled);
    }

    pub(super) fn check_creation(label: &str) -> Result<(), String> {
        if FAIL_CREATION.get() {
            return Err(format!(
                "cannot establish {label} cleanup worker: fixture resource exhaustion"
            ));
        }
        Ok(())
    }

    pub(super) fn worker_receipt() -> Option<Arc<AtomicBool>> {
        WORKERS.with_borrow_mut(|workers| {
            workers.as_mut().map(|workers| {
                let receipt = Arc::new(AtomicBool::new(false));
                workers.push(Arc::clone(&receipt));
                receipt
            })
        })
    }

    pub(super) fn confirm_worker_exit(receipt: Option<Arc<AtomicBool>>) {
        if let Some(receipt) = receipt {
            receipt.store(true, Ordering::Release);
        }
    }

    pub(crate) struct Scope;
    impl Scope {
        pub(crate) fn new() -> Self {
            WORKERS.with_borrow_mut(|workers| {
                assert!(workers.is_none());
                *workers = Some(Vec::new());
            });
            Self
        }

        pub(crate) fn wait_finished(&self) {
            let deadline = Instant::now() + Duration::from_secs(3);
            WORKERS.with_borrow(|workers| {
                for receipt in workers.as_ref().unwrap() {
                    while !receipt.load(Ordering::Acquire) {
                        assert!(Instant::now() < deadline, "cleanup worker did not finish");
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            });
        }

        pub(crate) fn count(&self) -> usize {
            WORKERS.with_borrow(|workers| workers.as_ref().unwrap().len())
        }
        pub(crate) fn finished(&self) -> bool {
            WORKERS.with_borrow(|workers| {
                workers
                    .as_ref()
                    .unwrap()
                    .iter()
                    .all(|receipt| receipt.load(Ordering::Acquire))
            })
        }
        pub(crate) fn worker_finished(&self, index: usize) -> bool {
            WORKERS.with_borrow(|workers| {
                workers.as_ref().expect("fixture scope")[index].load(Ordering::Acquire)
            })
        }
    }
    impl Drop for Scope {
        fn drop(&mut self) {
            WORKERS.with_borrow_mut(|workers| *workers = None);
            FAIL_CREATION.set(false);
            OBSERVATION_FAULT.with_borrow_mut(|fault| *fault = None);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "../tests/unit/process_cleanup.rs"]
mod tests;
