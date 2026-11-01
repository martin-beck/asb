//! Bounded teardown in an exclusive, short-lived agent supervisor process.
//!
//! Never construct this in the shared runner: it owns *all* children of the
//! calling process. Kernel child ownership plus pidfds prevents stale proc PIDs
//! from signalling unrelated processes, including under PID reuse.
#![allow(unsafe_code)]

use std::collections::BTreeSet;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

pub(crate) struct DescendantScope {
    owner: u32,
    attempted: bool,
}

impl DescendantScope {
    pub(crate) fn new() -> io::Result<Self> {
        super::become_child_subreaper()?;
        // Probe the required kernel API before starting a client. Our own
        // process is not a child, so the ownership check cannot send a signal.
        signal_and_reap_owned_child(std::process::id(), None)?;
        Ok(Self {
            owner: std::process::id(),
            attempted: false,
        })
    }

    /// Reap exited adopted services without signalling any live process.
    /// Only an exclusive supervisor may use this after reaping its principal.
    pub(crate) fn reap_exited(&self) -> io::Result<()> {
        if self.owner != std::process::id() {
            return Err(io::Error::other(
                "workload service supervisor changed process",
            ));
        }
        let children = children(self.owner)?;
        if children.len() > 65536 {
            return Err(io::Error::other(
                "workload service child list exceeded its bound",
            ));
        }
        let mut first_error = None;
        for pid in children {
            if let Err(error) = signal_and_reap_owned_child(pid, None) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Called after the principal client is reaped, before accepting its
    /// submission. A short TERM grace is followed by KILL; repeated sweeps
    /// retain and reap double-forked helpers as their parents exit.
    pub(crate) fn quiesce(&mut self) -> io::Result<()> {
        self.attempted = true;
        if self.owner != std::process::id() {
            return Err(io::Error::other(
                "agent descendant scope changed supervisor process",
            ));
        }
        let started = Instant::now();
        let mut terminated = BTreeSet::new();
        let mut first_error = None;
        loop {
            // The exclusive gate can still own lifecycle-traced threads after
            // its principal exits. A traced exit is a waitable stop, not a
            // completed exit; leaving it stopped prevents its thread group
            // from becoming reapable. Consume only wait statuses owned by this
            // gate and resume ptrace stops so teardown can finish.
            if let Err(error) = resume_owned_trace_stops() {
                first_error.get_or_insert(error);
            }
            let children = match children(self.owner) {
                Ok(children) => children,
                Err(error) => return Err(first_error.unwrap_or(error)),
            };
            if children.is_empty() {
                return Ok(());
            }
            if started.elapsed() >= Duration::from_secs(5) {
                return Err(first_error.unwrap_or_else(|| {
                    io::Error::other("agent descendants did not stop within the cleanup deadline")
                }));
            }
            for pid in children {
                // One inaccessible child must not starve separately owned
                // siblings, nor restart the shared five-second budget.
                if started.elapsed() >= Duration::from_secs(5) {
                    return Err(first_error.unwrap_or_else(|| {
                        io::Error::other(
                            "agent descendants did not stop within the cleanup deadline",
                        )
                    }));
                }
                if terminated.len() > 65536 {
                    return Err(first_error.unwrap_or_else(|| {
                        io::Error::other("agent descendant churn exceeded its cleanup bound")
                    }));
                }
                let signal = if started.elapsed() >= Duration::from_millis(500) {
                    Some(libc::SIGKILL)
                } else if terminated.insert(pid) {
                    Some(libc::SIGTERM)
                } else {
                    None
                };
                if let Err(error) = signal_and_reap_owned_child(pid, signal) {
                    first_error.get_or_insert(error);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for DescendantScope {
    fn drop(&mut self) {
        if !self.attempted {
            let _ = self.quiesce();
        }
    }
}

fn resume_owned_trace_stops() -> io::Result<()> {
    for _ in 0..65536 {
        let mut status = 0;
        // This supervisor is an exclusive reaper and calls this only after
        // the principal wait and watcher have settled.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::__WALL | libc::WNOHANG) };
        if pid == 0 {
            return Ok(());
        }
        if pid < 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(libc::ECHILD) {
                Ok(())
            } else if error.kind() == io::ErrorKind::Interrupted {
                continue;
            } else {
                Err(error)
            };
        }
        if libc::WIFSTOPPED(status) {
            let result = unsafe {
                libc::ptrace(
                    libc::PTRACE_CONT,
                    pid,
                    std::ptr::null_mut::<libc::c_void>(),
                    std::ptr::null_mut::<libc::c_void>(),
                )
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
    }
    Err(io::Error::other(
        "agent trace cleanup exceeded its event bound",
    ))
}

fn children(owner: u32) -> io::Result<Vec<u32>> {
    #[cfg(test)]
    let task = if owner == std::process::id() {
        CURRENT_TASK_VIEW.get().unwrap_or(owner)
    } else {
        owner
    };
    #[cfg(not(test))]
    let task = owner;
    let mut bytes = Vec::new();
    std::fs::File::open(format!("/proc/{owner}/task/{task}/children"))?
        .take(512 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 512 * 1024 {
        return Err(io::Error::other(
            "agent child list exceeded its cleanup bound",
        ));
    }
    parse_children(&bytes)
}

fn parse_children(bytes: &[u8]) -> io::Result<Vec<u32>> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| io::Error::other("invalid agent child list"))?;
    text.split_whitespace()
        .map(|value| {
            value
                .parse::<u32>()
                .ok()
                .filter(|pid| *pid > 1 && *pid <= i32::MAX as u32)
                .ok_or_else(|| io::Error::other("invalid agent child identity"))
        })
        .collect()
}

fn signal_and_reap_owned_child(pid: u32, signal: Option<i32>) -> io::Result<()> {
    // SAFETY: the syscall takes integer arguments, and a successful result is
    // a newly owned descriptor. It pins identity, not just the numeric PID.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor as i32) };
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // WNOWAIT proves that the pinned process is our child without releasing
    // its exit information. The descriptor remains safe if it exits or its
    // numeric PID is reused before the following signal operation.
    let owned = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            descriptor.as_raw_fd() as libc::id_t,
            information.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if owned < 0 {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ECHILD) {
            Ok(())
        } else {
            Err(error)
        };
    }
    #[cfg(test)]
    if signal.is_none() && FAILED_REAP_CHILD.get() == pid {
        return Err(io::Error::from_raw_os_error(libc::EIO));
    }
    // WNOWAIT may already report a zombie. Reap it without requiring signal
    // permission; a signal failure must not prevent collecting an exact exited
    // child. pidfd + kernel wait ownership still pin this identity throughout.
    let exited = unsafe { information.assume_init_ref().si_pid() } != 0;
    if !exited && let Some(signal) = signal {
        #[cfg(test)]
        if DENIED_CHILD.get() == pid {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                descriptor.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Err(io::Error::last_os_error());
        }
    }
    let result = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            descriptor.as_raw_fd() as libc::id_t,
            information.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG,
        )
    };
    if result < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ECHILD) {
        return Err(io::Error::last_os_error());
    }
    // A lifecycle ptrace handoff can leave an exited direct child waitable by
    // its original parent even after the pidfd wait path has observed it.
    // The preceding pidfd + WNOWAIT ownership proof pins this exact child, so
    // this numeric wait cannot target a reused PID.  Reap that residual zombie
    // before the supervisor's child-list loop decides it must stay alive.
    let mut status = 0;
    // A lifecycle-traced child reports its exit through the ptrace wait
    // domain.  `waitpid` without `__WALL` cannot consume that exact zombie,
    // leaving the gate in its bounded quiescence loop forever even though the
    // pidfd ownership proof above already established it is ours.
    let reaped = unsafe {
        libc::waitpid(
            pid as libc::pid_t,
            &mut status,
            libc::WNOHANG | libc::__WALL,
        )
    };
    if reaped < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ECHILD) {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    // Scoped to an exclusive reexec fixture and exact owned pid, after the real
    // kernel ownership check. No environment-controlled production fault.
    static DENIED_CHILD: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static FAILED_REAP_CHILD: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    // Exclusive libtest fixtures run on a worker rather than the process
    // leader. This task-view override never changes production child discovery.
    static CURRENT_TASK_VIEW: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CurrentTaskView(Option<u32>);
    impl CurrentTaskView {
        fn enter() -> Self {
            // Exact calling task only; never an arbitrary PID or another owner.
            let task = u32::try_from(unsafe { libc::syscall(libc::SYS_gettid) })
                .expect("current fixture task identity");
            assert!(task > 0 && task <= i32::MAX as u32);
            Self(CURRENT_TASK_VIEW.replace(Some(task)))
        }
    }
    impl Drop for CurrentTaskView {
        fn drop(&mut self) {
            CURRENT_TASK_VIEW.set(self.0);
        }
    }

    #[test]
    fn current_task_fixture_view_restores_after_unwind() {
        let original = CURRENT_TASK_VIEW.get();
        let unwind = std::panic::catch_unwind(|| {
            let _view = CurrentTaskView::enter();
            assert_eq!(
                CURRENT_TASK_VIEW.get(),
                Some(unsafe { libc::syscall(libc::SYS_gettid) } as u32)
            );
            panic!("controlled task-view unwind");
        });
        assert!(unwind.is_err());
        assert_eq!(CURRENT_TASK_VIEW.get(), original);
    }

    fn pin(pid: u32) -> OwnedFd {
        let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        assert!(descriptor >= 0);
        unsafe { OwnedFd::from_raw_fd(descriptor) }
    }

    fn exited(descriptor: &OwnedFd, timeout: i32) -> bool {
        let mut poll = libc::pollfd {
            fd: descriptor.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe { libc::poll(&mut poll, 1, timeout) > 0 && poll.revents & libc::POLLIN != 0 }
    }

    fn was_reaped(descriptor: &OwnedFd) -> bool {
        let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let result = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                descriptor.as_raw_fd() as libc::id_t,
                information.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
    }

    #[test]
    fn child_list_rejects_non_process_and_out_of_range_targets() {
        assert_eq!(parse_children(b"12 34\n").unwrap(), vec![12, 34]);
        assert!(parse_children(b"").unwrap().is_empty());
        for input in ["0", "1", "-1", "2147483648", "a", "12\x00"] {
            assert!(parse_children(input.as_bytes()).is_err());
        }
    }

    #[test]
    fn kernel_ownership_check_refuses_to_signal_the_test_process_itself() {
        signal_and_reap_owned_child(std::process::id(), Some(libc::SIGKILL)).unwrap();
    }

    #[test]
    fn one_owned_signal_denial_does_not_starve_later_owned_siblings() {
        use std::process::Command;
        if let Some(root) = std::env::var_os("ASB_MATERIAL_CUSTODY_ROOT") {
            let _view = CurrentTaskView::enter();
            let mut scope = DescendantScope::new().unwrap();
            // Observe these exact direct children on their actual spawning
            // libtest task. Joining another thread does not prove completed
            // reparenting onto the leader. Real gate tests retain leader view.
            let mut denied = Command::new("/bin/sleep").arg("30").spawn().unwrap();
            let mut sibling = Command::new("/bin/sleep").arg("30").spawn().unwrap();
            let sibling_identity = pin(sibling.id());
            let population = children(std::process::id()).unwrap();
            assert_eq!(population, vec![denied.id(), sibling.id()]);
            DENIED_CHILD.set(denied.id());
            let started = Instant::now();
            let error = scope.quiesce().unwrap_err();
            DENIED_CHILD.set(0);
            let elapsed = started.elapsed();
            let sibling_alive = !exited(&sibling_identity, 200);
            // Production DescendantScope, not Child::try_wait, must reap the
            // sibling. ECHILD is positive evidence here, not a fixture failure.
            let sibling_reaped = was_reaped(&sibling_identity);
            // Real cleanup before publishing observations; the outer guardian
            // still owns any orphan if a fixture assertion/unwind fails.
            denied.kill().unwrap();
            if !sibling_reaped && sibling_alive {
                sibling.kill().unwrap();
            }
            let sibling_wait = sibling.wait();
            denied.wait().unwrap();
            if sibling_reaped {
                assert_eq!(sibling_wait.unwrap_err().raw_os_error(), Some(libc::ECHILD));
            } else {
                sibling_wait.unwrap();
            }
            std::fs::write(
                std::path::PathBuf::from(root).join("sibling.json"),
                serde_json::to_vec(&serde_json::json!({
                    "error_errno":error.raw_os_error(),"sibling_alive":sibling_alive,
                    "sibling_reaped_by_scope":sibling_reaped,
                    "cleanup_millis":elapsed.as_millis(),
                    "exact_children_reaped":true
                }))
                .unwrap(),
            )
            .unwrap();
            return;
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/material50-fixtures")
            .join(crate::workloads::TaskIdentity::local("sibling-denial", None).attempt_id);
        std::fs::create_dir_all(&root).unwrap();
        let status = crate::material_guardian::run(&root,
            "process_gate::descendants::tests::one_owned_signal_denial_does_not_starve_later_owned_siblings","sibling").unwrap();
        let read = |name| -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(root.join(name)).unwrap()).unwrap()
        };
        let guardian = read("guardian.json");
        let observed = read("sibling.json");
        println!(
            "fixture={} guardian={guardian} sibling={observed}",
            root.display()
        );
        assert!(status.success());
        assert_eq!(guardian["child_status"], 0);
        assert_eq!(guardian["sentinel_survived"], true);
        assert_eq!(observed["error_errno"], libc::EPERM);
        assert_eq!(observed["exact_children_reaped"], true);
        assert_eq!(observed["sibling_reaped_by_scope"], true);
        assert!(observed["cleanup_millis"].as_u64().unwrap() < 6000);
        assert_eq!(
            observed["sibling_alive"], false,
            "first EPERM starved a separately owned sibling; all fixture cleanup already complete"
        );
    }

    #[test]
    fn already_exited_owned_child_is_reaped_without_requiring_signal_permission() {
        let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
        let identity = pin(child.id());
        assert!(exited(&identity, 3000));
        DENIED_CHILD.set(child.id());
        let result = signal_and_reap_owned_child(child.id(), Some(libc::SIGKILL));
        DENIED_CHILD.set(0);
        let reaped = was_reaped(&identity);
        // This owner must also observe ECHILD after the production pidfd reap;
        // if the implementation failed to reap, /bin/true has already exited
        // and this unconditional wait safely collects it before assertions.
        let owner_wait = child.wait();
        assert!(result.is_ok());
        assert!(reaped);
        assert_eq!(owner_wait.unwrap_err().raw_os_error(), Some(libc::ECHILD));
    }

    #[test]
    fn quiesce_reaps_a_traced_child_stopped_at_exit() {
        let _view = CurrentTaskView::enter();
        let mut scope = DescendantScope::new().unwrap();
        let child = unsafe { libc::fork() };
        if child == 0 {
            if unsafe {
                libc::ptrace(
                    libc::PTRACE_TRACEME,
                    0,
                    std::ptr::null_mut::<libc::c_void>(),
                    std::ptr::null_mut::<libc::c_void>(),
                )
            } != 0
            {
                unsafe { libc::_exit(127) };
            }
            unsafe { libc::raise(libc::SIGSTOP) };
            loop {
                unsafe { libc::pause() };
            }
        }
        assert!(child > 0);
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(child, &mut status, libc::__WALL) },
            child
        );
        assert!(libc::WIFSTOPPED(status));
        assert_eq!(
            unsafe {
                libc::ptrace(
                    libc::PTRACE_SETOPTIONS,
                    child,
                    std::ptr::null_mut::<libc::c_void>(),
                    libc::PTRACE_O_TRACEEXIT as usize as *mut libc::c_void,
                )
            },
            0
        );
        assert_eq!(
            unsafe {
                libc::ptrace(
                    libc::PTRACE_CONT,
                    child,
                    std::ptr::null_mut::<libc::c_void>(),
                    std::ptr::null_mut::<libc::c_void>(),
                )
            },
            0
        );
        let result = scope.quiesce();
        // Always release the exact child if the regression is present.
        if result.is_err() {
            unsafe {
                libc::ptrace(
                    libc::PTRACE_CONT,
                    child,
                    std::ptr::null_mut::<libc::c_void>(),
                    std::ptr::null_mut::<libc::c_void>(),
                );
                libc::kill(child, libc::SIGKILL);
                libc::waitpid(child, &mut status, libc::__WALL);
            }
        }
        assert!(
            result.is_ok(),
            "traced exit stop prevented cleanup: {result:?}"
        );
    }

    #[test]
    fn ptraced_exited_child_is_reaped_from_the_wall_wait_domain() {
        // The native lifecycle tracer creates this same ownership shape: the
        // direct child is ours, but its final wait status is delivered through
        // the ptrace wait domain.  A normal waitpid(WNOHANG) leaves it as a
        // zombie, which in turn keeps the exclusive gate's child census nonempty.
        let child = unsafe { libc::fork() };
        if child == 0 {
            if unsafe {
                libc::ptrace(
                    libc::PTRACE_TRACEME,
                    0,
                    std::ptr::null_mut::<libc::c_void>(),
                    std::ptr::null_mut::<libc::c_void>(),
                )
            } != 0
            {
                unsafe { libc::_exit(127) };
            }
            unsafe { libc::raise(libc::SIGSTOP) };
            unsafe { libc::_exit(0) };
        }
        assert!(
            child > 0,
            "fork fixture failed: {}",
            std::io::Error::last_os_error()
        );

        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(child, &mut status, libc::__WALL) },
            child,
            "could not observe ptrace stop: {}",
            std::io::Error::last_os_error()
        );
        assert!(libc::WIFSTOPPED(status));
        assert_eq!(
            unsafe {
                libc::ptrace(
                    libc::PTRACE_CONT,
                    child,
                    std::ptr::null_mut::<libc::c_void>(),
                    std::ptr::null_mut::<libc::c_void>(),
                )
            },
            0,
            "could not continue ptraced child: {}",
            std::io::Error::last_os_error()
        );
        let identity = pin(child as u32);
        assert!(exited(&identity, 3000), "ptraced child did not exit");
        signal_and_reap_owned_child(child as u32, None).unwrap();
        assert!(was_reaped(&identity));
    }

    #[test]
    fn reap_exited_continues_after_first_error_without_signalling_live_siblings() {
        use std::process::Command;
        if let Some(root) = std::env::var_os("ASB_MATERIAL_CUSTODY_ROOT") {
            let _view = CurrentTaskView::enter();
            let scope = DescendantScope::new().unwrap();
            let mut failed = Command::new("/bin/true").spawn().unwrap();
            let mut sibling = Command::new("/bin/true").spawn().unwrap();
            let mut live = Command::new("/bin/sleep").arg("30").spawn().unwrap();
            assert_eq!(
                children(std::process::id()).unwrap(),
                vec![failed.id(), sibling.id(), live.id()]
            );
            let failed_identity = pin(failed.id());
            let sibling_identity = pin(sibling.id());
            let live_identity = pin(live.id());
            assert!(exited(&failed_identity, 3000));
            assert!(exited(&sibling_identity, 3000));
            FAILED_REAP_CHILD.set(failed.id());
            let error = scope.reap_exited().unwrap_err();
            FAILED_REAP_CHILD.set(0);
            let sibling_reaped = was_reaped(&sibling_identity);
            let live_alive = !exited(&live_identity, 0);
            failed.wait().unwrap();
            let sibling_wait = sibling.wait();
            live.kill().unwrap();
            live.wait().unwrap();
            if sibling_reaped {
                assert_eq!(sibling_wait.unwrap_err().raw_os_error(), Some(libc::ECHILD));
            } else {
                sibling_wait.unwrap();
            }
            std::fs::write(
                std::path::PathBuf::from(root).join("reap.json"),
                serde_json::to_vec(&serde_json::json!({
                    "error_errno":error.raw_os_error(),
                    "sibling_reaped_by_scope":sibling_reaped,
                    "live_child_not_signalled":live_alive,
                    "exact_children_reaped":true
                }))
                .unwrap(),
            )
            .unwrap();
            return;
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/material50-fixtures")
            .join(crate::workloads::TaskIdentity::local("fair-reap", None).attempt_id);
        std::fs::create_dir_all(&root).unwrap();
        let status = crate::material_guardian::run(&root,
            "process_gate::descendants::tests::reap_exited_continues_after_first_error_without_signalling_live_siblings","reap").unwrap();
        let read = |name| -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(root.join(name)).unwrap()).unwrap()
        };
        let guardian = read("guardian.json");
        let observed = read("reap.json");
        println!(
            "fixture={} guardian={guardian} reap={observed}",
            root.display()
        );
        assert!(status.success());
        assert_eq!(guardian["child_status"], 0);
        assert_eq!(guardian["sentinel_survived"], true);
        assert_eq!(observed["error_errno"], libc::EIO);
        assert_eq!(observed["exact_children_reaped"], true);
        assert_eq!(observed["sibling_reaped_by_scope"], true);
        assert_eq!(observed["live_child_not_signalled"], true);
    }
}
