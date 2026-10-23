//! Real owned children only, without network, providers, or privileged tools.
#![allow(unsafe_code)] // Linux identity/reaping assertions only.
use super::*;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};

struct Sentinel(Child);
impl Sentinel {
    fn new() -> Self {
        Self(Command::new("/bin/sleep").arg("30").spawn().unwrap())
    }
    fn settle(&mut self) -> bool {
        let alive = self.0.try_wait().unwrap().is_none();
        if alive {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
        alive
    }
}
impl Drop for Sentinel {
    fn drop(&mut self) {
        // Exact owned inert child only. This also covers fixture observation
        // errors/unwind before the explicit cleanup and final assertions.
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

#[test]
fn creation_failure_and_empty_scope_leave_no_waiting_worker() {
    let scope = test_support::Scope::new();
    test_support::set_creation_failure(true);
    assert!(
        OwnedReaper::new("fixture")
            .err()
            .unwrap()
            .contains("fixture resource exhaustion")
    );
    assert_eq!(scope.count(), 0);
    test_support::set_creation_failure(false);
    drop(OwnedReaper::new("fixture").unwrap());
    scope.wait_finished();
    assert_eq!(scope.count(), 1);
}

#[test]
fn poisoned_handoff_retains_and_reaps_a_real_direct_child_without_drop_panic() {
    let scope = test_support::Scope::new();
    let reaper = OwnedReaper::new("fixture").unwrap();
    let poisoned = std::panic::catch_unwind(|| {
        let _state = reaper.state.0.lock().unwrap();
        panic!("fixture handoff poison");
    });
    assert!(poisoned.is_err());
    let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let pid = child.id();
    // SAFETY: pin the directly owned child's identity before custody transfer.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    assert!(fd >= 0);
    let identity = unsafe { File::from_raw_fd(fd) };
    reaper.adopt(child);
    drop(reaper);
    scope.wait_finished();
    let mut poll = libc::pollfd {
        fd: identity.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll observes the pinned child; waitpid only checks exit custody.
    assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 1);
    assert_ne!(poll.revents & libc::POLLIN, 0);
    assert_eq!(
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn wait_only_custody_does_not_kill_a_live_supervisor() {
    let scope = test_support::Scope::new();
    let reaper = OwnedReaper::new("wait-only-fixture").unwrap();
    let mut child = Command::new("/bin/sh")
        .args(["-c", "printf ready; read release; printf quiescent; exit 7"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    let mut ready = [0; 5];
    output.read_exact(&mut ready).unwrap();
    assert_eq!(&ready, b"ready");
    reaper.adopt_wait_only(child);
    assert!(matches!(
        reaper.state.0.lock().unwrap().children[0].1,
        ReapPolicy::WaitOnly
    ));
    drop(reaper);
    input.write_all(b"release\n").unwrap();
    drop(input);
    let mut proof = String::new();
    output.read_to_string(&mut proof).unwrap();
    assert_eq!(proof, "quiescent");
    scope.wait_finished();
}

#[test]
fn controlled_wait_error_retains_live_wait_only_child_until_real_release_exit_and_reap() {
    let scope = test_support::Scope::new();
    let fault = test_support::ObservationFault::arm();
    let reaper = OwnedReaper::new("controlled-wait-error").unwrap();
    let mut sentinel = Sentinel::new();
    let (mut output, writer) = UnixStream::pair().unwrap();
    output
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    let writer: OwnedFd = writer.into();
    let mut child = Command::new("/bin/sh")
        .args(["-c", "printf ready; read release; printf quiescent; exit 7"])
        .stdin(std::process::Stdio::piped())
        .stdout(Stdio::from(writer))
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut input = child.stdin.take().unwrap();
    // Install exact child custody before any fixture observation can fail.
    reaper.adopt_wait_only(child);
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    assert!(fd >= 0);
    let identity = unsafe { File::from_raw_fd(fd) };
    let mut ready = [0; 5];
    let ready_read = output.read_exact(&mut ready);
    drop(reaper);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !fault.observed() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let injected_while_live = fault.observed() && !scope.finished();
    let mut poll = libc::pollfd {
        fd: identity.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let child_survived = unsafe { libc::poll(&mut poll, 1, 0) } == 0;
    let release = input.write_all(b"release\n");
    drop(input);
    let mut proof = String::new();
    let proof_read = output.read_to_string(&mut proof);
    let child_exited = unsafe { libc::poll(&mut poll, 1, 3000) } > 0;
    let no_premature_receipt = !scope.finished();
    fault.release();
    scope.wait_finished();
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let wait = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            identity.as_raw_fd() as libc::id_t,
            information.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    let actual_reap =
        wait == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
    let sentinel_survived = sentinel.settle();
    // Every intended assertion follows actual owned child/sentinel cleanup.
    assert!(ready_read.is_ok());
    assert_eq!(&ready, b"ready");
    assert!(injected_while_live && child_survived);
    assert!(release.is_ok() && proof_read.is_ok());
    assert_eq!(proof, "quiescent");
    assert!(child_exited && no_premature_receipt && actual_reap);
    assert!(scope.finished() && sentinel_survived);
}

#[test]
fn controlled_poll_error_retains_terminate_child_before_real_kill_exit_and_reap() {
    let scope = test_support::Scope::new();
    let fault = test_support::ObservationFault::arm();
    let reaper = OwnedReaper::new("controlled-poll-error").unwrap();
    let mut sentinel = Sentinel::new();
    let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let pid = child.id();
    reaper.adopt(child);
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    assert!(fd >= 0);
    let identity = unsafe { File::from_raw_fd(fd) };
    drop(reaper);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !fault.observed() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let mut poll = libc::pollfd {
        fd: identity.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let retained_live =
        fault.observed() && !scope.finished() && unsafe { libc::poll(&mut poll, 1, 0) } == 0;
    fault.release();
    scope.wait_finished();
    let exited = unsafe { libc::poll(&mut poll, 1, 0) } > 0;
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let wait = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            identity.as_raw_fd() as libc::id_t,
            information.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    let actual_reap =
        wait == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD);
    let sentinel_survived = sentinel.settle();
    assert!(retained_live && exited && actual_reap && sentinel_survived);
    assert!(scope.finished());
}

fn pinned(pid: u32) -> OwnedFd {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    assert!(fd >= 0);
    unsafe { OwnedFd::from_raw_fd(fd) }
}
fn identity_exited(fd: &OwnedFd) -> bool {
    let mut event = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut event, 1, 0) > 0 && event.revents & libc::POLLIN != 0 }
}
fn identity_reaped(fd: &OwnedFd) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let result = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            fd.as_raw_fd() as libc::id_t,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
}
fn bounded_until(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        if predicate() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
/// Exact held actor, with EOF as its real release on every error/unwind path.
/// The pre-established keeper gets immediate custody, before pidfd observation.
fn held_actor(
    reaper: &OwnedReaper,
    wait_only: bool,
    select: Option<&test_support::ObservationFault>,
) -> (std::process::ChildStdin, OwnedFd) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "read release; exit 0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let input = child.stdin.take().expect("configured fixture stdin");
    let pid = child.id();
    if let Some(fault) = select {
        fault.select_owned(&child);
    }
    if wait_only {
        reaper.adopt_wait_only(child);
    } else {
        reaper.adopt(child);
    }
    (input, pinned(pid))
}

#[test]
fn held_wait_only_does_not_starve_later_terminate_child() {
    let scope = test_support::Scope::new();
    let reaper = OwnedReaper::new("fair-held-first").unwrap();
    let sentinel_keeper = OwnedReaper::new("fair-independent-sentinel").unwrap();
    let (sentinel_release, sentinel) = held_actor(&sentinel_keeper, true, None);
    drop(sentinel_keeper);
    let (first_release, first) = held_actor(&reaper, true, None);
    let (later_release, later) = held_actor(&reaper, false, None);
    drop(reaper);
    let later_progress = bounded_until(|| identity_exited(&later) && identity_reaped(&later));
    let first_survived = !identity_exited(&first);
    let sentinel_survived = !identity_exited(&sentinel);
    let no_premature_completion = !scope.worker_finished(0);
    // Even old sequential source can now finish: real EOF releases every
    // held actor before the intended fairness assertion. Never kill WaitOnly.
    drop(first_release);
    drop(later_release);
    drop(sentinel_release);
    let settled = bounded_until(|| scope.finished());
    let actual_reaps = [&first, &later, &sentinel]
        .into_iter()
        .all(|fd| identity_exited(fd) && identity_reaped(fd));
    println!(
        "held-first later_progress={later_progress} first_survived={first_survived} sentinel_survived={sentinel_survived} settled={settled} actual_reaps={actual_reaps}"
    );
    assert!(
        settled && actual_reaps && first_survived && sentinel_survived && no_premature_completion
    );
    assert!(
        later_progress,
        "first held WaitOnly starved a later Terminate owner; all actors already released and reaped"
    );
}

#[test]
fn first_child_local_observation_fault_does_not_starve_later_exited_terminate_reap() {
    let scope = test_support::Scope::new();
    let fault = test_support::ObservationFault::arm_targeted();
    let reaper = OwnedReaper::new("fair-local-fault").unwrap();
    let (first_release, first) = held_actor(&reaper, false, Some(&fault));
    // The sentinel's distinct pre-established worker must not inherit the
    // targeted fault: clearing parent TLS does not change the first worker Arc.
    test_support::bind_worker_fault(None);
    let sentinel_keeper = OwnedReaper::new("fair-fault-sentinel").unwrap();
    let (sentinel_release, sentinel) = held_actor(&sentinel_keeper, true, None);
    drop(sentinel_keeper);
    let child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    reaper.adopt(child);
    let later = pinned(pid);
    // Observe actual exit without reaping. The keeper must perform that reap.
    let already_exited = bounded_until(|| identity_exited(&later));
    let not_yet_reaped = !identity_reaped(&later);
    drop(reaper);
    let injected = bounded_until(|| fault.observed());
    let later_progress = bounded_until(|| identity_reaped(&later));
    let first_survived = !identity_exited(&first);
    let sentinel_survived = !identity_exited(&sentinel);
    let no_premature_completion = !scope.worker_finished(0);
    // Controlled fault is not a kernel error. Release it AND the real actor,
    // then require positive keeper completion/reap before intended assertions.
    fault.release();
    drop(first_release);
    drop(sentinel_release);
    let settled = bounded_until(|| scope.finished());
    let actual_reaps = [&first, &later, &sentinel]
        .into_iter()
        .all(|fd| identity_exited(fd) && identity_reaped(fd));
    println!(
        "local-fault injected={injected} later_progress={later_progress} first_survived={first_survived} sentinel_survived={sentinel_survived} settled={settled} actual_reaps={actual_reaps}"
    );
    assert!(
        settled
            && actual_reaps
            && already_exited
            && not_yet_reaped
            && injected
            && first_survived
            && sentinel_survived
            && no_premature_completion
    );
    assert!(
        later_progress,
        "first-child-local controlled observation error starved a later exited Terminate owner's reap; all actors already released and reaped"
    );
}
