//! Conservative, common-clock observations of the verified native principal.
//! A live lifetime is not CPU residency; these bounds exclude unsampled tails.
use super::ProcessIdentity;
use serde::Serialize;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct Window {
    root: Option<ProcessIdentity>,
    domain: Option<String>,
    release_before: Option<u64>,
    exit_after: Option<u64>,
    first_after: Option<u64>,
    last_before: Option<u64>,
    last_after: Option<u64>,
    samples: u64,
    completed: bool,
    invalid: bool,
}

#[derive(Serialize)]
struct Receipt<'a> {
    schema_version: u32,
    status: &'static str,
    scope: &'static str,
    clock: &'static str,
    clock_domain: &'a Option<String>,
    root: &'a Option<ProcessIdentity>,
    release_bound_ns: Option<u64>,
    exit_bound_ns: Option<u64>,
    start_bound_ns: Option<u64>,
    end_bound_ns: Option<u64>,
    live_observations: u64,
    sampling_interval_ms: u64,
    unavailable_reason: Option<&'static str>,
    limitations: &'static str,
}

impl Window {
    fn record(&mut self, before: u64, after: u64) {
        if after < before || self.last_after.is_some_and(|last| before < last) {
            self.invalid = true;
            return;
        }
        self.first_after.get_or_insert(after);
        self.last_before = Some(before);
        self.last_after = Some(after);
        self.samples = self.samples.saturating_add(1);
    }

    pub(super) fn end(&mut self) {
        self.completed = true;
    }

    pub(super) fn invalidate(&mut self) {
        self.invalid = true;
    }

    fn receipt(&self) -> Receipt<'_> {
        let observed = !self.invalid
            && self.completed
            && self.samples >= 2
            && self.root.is_some()
            && self.domain.is_some()
            && self
                .release_before
                .zip(self.first_after)
                .is_some_and(|(release, first)| release <= first)
            && self
                .last_before
                .zip(self.exit_after)
                .is_some_and(|(last, exit)| last <= exit)
            && self
                .first_after
                .zip(self.last_before)
                .is_some_and(|(start, end)| end > start);
        Receipt {
            schema_version: 1,
            status: if observed { "observed" } else { "unavailable" },
            scope: "verified-native-agent-principal-lifetime",
            clock: "CLOCK_MONOTONIC",
            clock_domain: &self.domain,
            root: &self.root,
            release_bound_ns: observed.then_some(self.release_before).flatten(),
            exit_bound_ns: observed.then_some(self.exit_after).flatten(),
            start_bound_ns: observed.then_some(self.first_after).flatten(),
            end_bound_ns: observed.then_some(self.last_before).flatten(),
            live_observations: self.samples,
            sampling_interval_ms: 100,
            unavailable_reason: (!observed).then_some("requires two ordered live observations of the verified actual native agent principal, a stable clock domain and completed execution"),
            limitations: "Conservative interior of a generation-verified live native principal lifetime, not CPU execution/residency or the full process-tree duration. Includes sleeping and stopped time between observations. Unsampled launch/exit tails and descendants after principal exit are excluded. Only identical boot/time-namespace clock domains are comparable. Missing, short, failed or unsupported observations remain unavailable.",
        }
    }

    pub(super) fn document(&self) -> serde_json::Value {
        serde_json::to_value(self.receipt())
            .expect("execution window contains only bounded integers")
    }
}

/// Independent of terminal progress and bounded to one root read per 100 ms.
pub(crate) struct Sampler {
    shared: Rc<RefCell<Window>>,
    root: ProcessIdentity,
    next: Instant,
    closed: bool,
}
impl Sampler {
    pub(super) fn new(shared: Rc<RefCell<Window>>, root: ProcessIdentity) -> Option<Self> {
        if root.pid == std::process::id() || !root.matches() {
            return None;
        }
        let domain = clock_domain()?;
        let mut window = shared.borrow_mut();
        if window.root.is_some() || window.completed || window.invalid {
            return None;
        }
        window.root = Some(root);
        window.domain = Some(domain);
        window.release_before = monotonic_ns();
        drop(window);
        Some(Self {
            shared,
            root,
            next: Instant::now(),
            closed: false,
        })
    }

    pub(crate) fn exit_observer(&self) -> ExitObserver {
        ExitObserver(self.shared.clone())
    }

    pub(crate) fn observe(&mut self) {
        if self.closed || Instant::now() < self.next {
            return;
        }
        self.next = Instant::now() + Duration::from_millis(100);
        let before = monotonic_ns();
        let live = self.root.matches();
        let after = monotonic_ns();
        if !live {
            self.closed = true;
            return;
        }
        let mut window = self.shared.borrow_mut();
        if let Some((before, after)) = before.zip(after) {
            window.record(before, after);
        } else {
            window.invalidate();
            self.closed = true;
        }
    }
}
/// Bound to the same window; called on owned exit before pipe draining.
pub(crate) struct ExitObserver(Rc<RefCell<Window>>);
impl ExitObserver {
    pub(crate) fn observe(&mut self) {
        let now = monotonic_ns();
        let mut window = self.0.borrow_mut();
        if window.exit_after.is_some() || now.is_none() {
            window.invalidate();
        }
        window.exit_after = now;
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        let mut window = self.shared.borrow_mut();
        if clock_domain() != window.domain {
            window.invalidate();
        }
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)] // Audited clock_gettime FFI, with a valid stack timespec.
fn monotonic_ns() -> Option<u64> {
    let mut value = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes only to this valid timespec.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut value) } != 0 {
        return None;
    }
    u64::try_from(value.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(
            u64::try_from(value.tv_nsec)
                .ok()
                .filter(|n| *n < 1_000_000_000)?,
        )
}
#[cfg(not(target_os = "linux"))]
fn monotonic_ns() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn clock_domain() -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut boot = String::new();
    std::fs::File::open("/proc/sys/kernel/random/boot_id")
        .ok()?
        .take(65)
        .read_to_string(&mut boot)
        .ok()?;
    let boot = boot.trim();
    if boot.len() != 36 || !boot.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return None;
    }
    let namespace = std::fs::read_link("/proc/self/ns/time").ok()?;
    let namespace = namespace.to_str()?;
    if namespace.len() > 64 || !namespace.starts_with("time:[") || !namespace.ends_with(']') {
        return None;
    }
    Some(format!(
        "{:x}",
        Sha256::digest(format!("asb-monotonic-v1\0{boot}\0{namespace}"))
    ))
}
#[cfg(not(target_os = "linux"))]
fn clock_domain() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn window() -> Window {
        Window {
            release_before: Some(1),
            exit_after: Some(200),
            root: Some(ProcessIdentity {
                pid: 42,
                start_time_ticks: 1,
            }),
            domain: Some("test-domain".into()),
            ..Window::default()
        }
    }
    #[test]
    fn interior_bounds_exclude_both_observation_uncertainties() {
        let mut w = window();
        w.record(10, 15);
        w.record(100, 110);
        w.end();
        let r = w.receipt();
        assert_eq!(r.status, "observed");
        assert_eq!(r.release_bound_ns, Some(1));
        assert_eq!(r.exit_bound_ns, Some(200));
        assert_eq!(r.start_bound_ns, Some(15));
        assert_eq!(r.end_bound_ns, Some(100));
    }
    #[test]
    fn missing_short_failed_or_backwards_observations_are_not_zero_windows() {
        for variant in 0..6 {
            let mut w = window();
            w.record(10, 15);
            if variant != 0 {
                w.record(100, 110);
            }
            if variant != 1 {
                w.end();
            }
            match variant {
                2 => w.invalidate(),
                3 => w.domain = None,
                4 => w.record(9, 12),
                5 => w.record(300, 200),
                _ => {}
            }
            let r = w.receipt();
            assert_eq!(r.status, "unavailable");
            assert!(r.start_bound_ns.is_none() && r.end_bound_ns.is_none());
        }
    }
    #[test]
    fn enclosing_phase_bounds_require_pre_release_and_observed_owned_exit() {
        for variant in 0..4 {
            let mut w = window();
            w.record(10, 15);
            w.record(100, 110);
            w.end();
            match variant {
                0 => w.release_before = None,
                1 => w.exit_after = None,
                2 => w.release_before = Some(20),
                _ => w.exit_after = Some(90),
            }
            assert_eq!(w.receipt().status, "unavailable");
            assert!(w.receipt().release_bound_ns.is_none() && w.receipt().exit_bound_ns.is_none());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn live_clock_domain_is_stable_and_sampler_rejects_unowned_rebinding() {
        assert_eq!(clock_domain(), clock_domain());
        assert!(clock_domain().is_some());
        let shared = Rc::new(RefCell::new(Window::default()));
        assert!(
            Sampler::new(
                shared.clone(),
                ProcessIdentity::read(std::process::id()).unwrap()
            )
            .is_none()
        );
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let root = ProcessIdentity::read(child.id()).unwrap();
        let mut sampler = Sampler::new(shared.clone(), root).unwrap();
        sampler.observe();
        assert_eq!(shared.borrow().samples, 1);
        assert!(Sampler::new(shared.clone(), root).is_none());
        // Immediate polls are throttled independently of terminal output.
        sampler.observe();
        assert_eq!(shared.borrow().samples, 1);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
