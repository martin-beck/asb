//! Owned local subprocess regressions; never SDKs, Docker or providers.
use super::*;
use serde_json::json;
#[cfg(target_os = "linux")]
use std::process::Child;

#[cfg(unix)]
fn ready_child(script: &str) -> Controller {
    let mut command = ControllerCommand::new("python3").unwrap();
    command.args(["-I", "-S", "-B", "-u", "-c", script]);
    let mut controller = Controller::spawn(&mut command).unwrap();
    assert_eq!(
        controller
            .receive(Instant::now() + Duration::from_secs(3))
            .unwrap()["event"],
        "ready"
    );
    controller
}

#[cfg(unix)]
#[test]
fn receive_until_returns_none_for_a_presentation_poll_then_delivers_event() {
    let mut controller = ready_child(
        "import json,sys,time\nprint(json.dumps(dict(event='ready')),flush=True)\ntime.sleep(.08)\nprint(json.dumps(dict(event='output',data='progress')),flush=True)\nsys.stdin.read()\n",
    );
    assert_eq!(
        controller
            .receive_until(Instant::now() + Duration::from_millis(10))
            .unwrap(),
        None
    );
    assert_eq!(
        controller
            .receive_until(Instant::now() + Duration::from_secs(1))
            .unwrap()
            .unwrap()["event"],
        "output"
    );
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum PollFault {
    Pending,
    Error,
}
#[cfg(target_os = "linux")]
thread_local! { static POLL_FAULT: std::cell::Cell<Option<PollFault>> = const { std::cell::Cell::new(None) }; }
#[cfg(target_os = "linux")]
thread_local! { static SETUP_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
#[cfg(target_os = "linux")]
pub(super) fn setup_failure() -> bool {
    SETUP_FAILURE.get()
}
#[cfg(target_os = "linux")]
pub(super) fn cleanup_poll_override() -> Option<std::io::Result<Option<std::process::ExitStatus>>> {
    POLL_FAULT.get().map(|fault| match fault {
        PollFault::Pending => Ok(None),
        PollFault::Error => Err(std::io::Error::other("fixture polling failure")),
    })
}

#[cfg(unix)]
#[test]
fn nonreading_child_cannot_extend_request_write_past_response_budget() {
    let mut command = ControllerCommand::new("python3").unwrap();
    // The one-second fixture exit is a safety bound for the pre-fix blocking
    // write, not the behavior being qualified. It never reads the private body.
    command.args([
        "-I",
        "-S",
        "-B",
        "-u",
        "-c",
        "import json,time;print(json.dumps(dict(event='ready')),flush=True);time.sleep(1)",
    ]);
    let mut controller = Controller::spawn(&mut command).unwrap();
    controller
        .receive(Instant::now() + Duration::from_secs(3))
        .unwrap();
    let started = Instant::now();
    let error = controller
        .request(
            json!({"private":"x".repeat(768*1024)}),
            EventKind::Completed,
            Duration::from_millis(100),
        )
        .unwrap_err();
    let elapsed = started.elapsed();
    drop(controller);
    assert!(!error.contains("xxxx"));
    assert!(
        elapsed < Duration::from_millis(500),
        "request write outlived its 100ms budget: {elapsed:?}: {error}"
    );
}

#[cfg(target_os = "linux")]
fn owned_pipe_readers(identity: &std::path::Path) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .flatten()
        .filter(|entry| {
            if std::fs::read_link(entry.path()).ok().as_deref() != Some(identity) {
                return false;
            }
            let info = std::fs::read_to_string(
                std::path::Path::new("/proc/self/fdinfo").join(entry.file_name()),
            )
            .unwrap();
            let flags = info
                .lines()
                .find_map(|line| line.strip_prefix("flags:\t"))
                .unwrap();
            u32::from_str_radix(flags, 8).unwrap() & 3 != 1
        })
        .count()
}

#[cfg(target_os = "linux")]
#[test]
fn retained_stdout_writer_cannot_leave_a_background_reader_after_controller_drop() {
    let mut command = ControllerCommand::new("python3").unwrap();
    command.args([
        "-I",
        "-S",
        "-B",
        "-u",
        "-c",
        "import json,sys;print(json.dumps(dict(event='ready')),flush=True);sys.stdin.read()",
    ]);
    let mut controller = Controller::spawn(&mut command).unwrap();
    controller
        .receive(Instant::now() + Duration::from_secs(3))
        .unwrap();
    let output = std::path::PathBuf::from(format!("/proc/{}/fd/1", controller.child().id()));
    let identity = std::fs::read_link(&output).unwrap();
    assert!(identity.to_string_lossy().starts_with("pipe:"));
    // Duplicate only this fixture's writer; no orphan descendant or foreign FD.
    let retained_writer = std::fs::OpenOptions::new()
        .write(true)
        .open(output)
        .unwrap();
    drop(controller);
    let escaped_readers = owned_pipe_readers(&identity);
    drop(retained_writer);
    let deadline = Instant::now() + Duration::from_secs(2);
    while owned_pipe_readers(&identity) != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        escaped_readers, 0,
        "a detached reader retained the private stdout pipe after Drop"
    );
    assert_eq!(owned_pipe_readers(&identity), 0);
}

#[cfg(unix)]
#[test]
fn duplex_pipe_pressure_preserves_binary_request_and_bounded_ordered_events() {
    let mut controller = ready_child(
        "import fcntl,json,os,sys\nassert not fcntl.fcntl(0,fcntl.F_GETFL)&os.O_NONBLOCK\nassert not fcntl.fcntl(1,fcntl.F_GETFL)&os.O_NONBLOCK\nprint(json.dumps(dict(event='ready')),flush=True)\nprint(json.dumps(dict(event='output',data='x'*262144)),flush=True)\nrequest=json.loads(sys.stdin.readline());assert request['private']=='y'*786432\nprint(json.dumps(dict(event='completed',value=7)),flush=True)\nsys.stdin.read()\n",
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    controller
        .send(&json!({"private":"y".repeat(768*1024)}), deadline)
        .unwrap();
    let output = controller.receive(deadline).unwrap();
    assert_eq!(output["event"], "output");
    assert_eq!(output["data"].as_str().unwrap().len(), 256 * 1024);
    assert_eq!(
        controller.receive(deadline).unwrap(),
        json!({"event":"completed","value":7})
    );
    controller.complete_response(deadline).unwrap();
}

#[cfg(unix)]
#[test]
fn request_response_deadline_is_not_restarted_after_slow_write() {
    let mut controller = ready_child(
        "import json,sys,time\nprint(json.dumps(dict(event='ready')),flush=True)\ntime.sleep(.2);request=json.loads(sys.stdin.readline());time.sleep(.2)\nprint(json.dumps(dict(event='completed')),flush=True)\nsys.stdin.read()\n",
    );
    let started = Instant::now();
    let error = controller
        .request(
            json!({"private":"x".repeat(512*1024)}),
            EventKind::Completed,
            Duration::from_millis(300),
        )
        .unwrap_err();
    assert!(error.contains("response deadline"), "{error}");
    assert!(started.elapsed() < Duration::from_millis(750));
}

#[cfg(unix)]
#[test]
fn buffered_unsolicited_events_cannot_grow_or_extend_a_blocked_write() {
    let mut controller = ready_child(
        "import json,sys,time\nprint(json.dumps(dict(event='ready')),flush=True)\nfor n in range(1000): print(json.dumps(dict(event='output',data='x'*10000)),flush=True)\ntime.sleep(1)\n",
    );
    let started = Instant::now();
    assert!(
        controller
            .send(
                &json!({"private":"x".repeat(512*1024)}),
                Instant::now() + Duration::from_millis(100)
            )
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_millis(500));
    let (events, bytes) = controller.buffered_for_test();
    assert!(events <= 8);
    assert!(bytes <= MAX_MESSAGE as usize + 1);
    let _ = controller.stop_for_test(Duration::from_secs(1));
}

#[cfg(unix)]
#[test]
fn framing_bound_accepts_exact_limit_and_rejects_extra_byte_or_unterminated_line() {
    for (extra, newline) in [(0, true), (1, true), (0, false)] {
        let script = format!(
            "import sys\nprefix=b'{{\"event\":\"completed\",\"padding\":\"';suffix=b'\"}}\\n'\nbody=prefix+b'x'*(1048576-len(prefix)-len(suffix)+{extra})+suffix\nsys.stdout.buffer.write(body if {newline} else body[:-1]);sys.stdout.buffer.flush()\n",
            newline = if newline { "True" } else { "False" }
        );
        let mut command = ControllerCommand::new("python3").unwrap();
        command.args(["-I", "-S", "-B", "-u", "-c", &script]);
        let mut controller = Controller::spawn(&mut command).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let response = controller.receive(deadline);
        if extra == 0 && newline {
            assert_eq!(response.unwrap()["event"], "completed");
        } else {
            assert!(response.unwrap_err().contains("framing limit"));
        }
        controller.complete_response(deadline).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn close_and_cancellation_share_ack_exit_and_retry_cleanup_deadline() {
    for normal in [false, true] {
        let script = if normal {
            "import json,sys,time\nprint(json.dumps(dict(event='ready')),flush=True)\nassert json.loads(sys.stdin.readline())['op']=='close'\ntime.sleep(.06);print(json.dumps(dict(event='closed',cleanup='removed')),flush=True)\ntime.sleep(5)\n"
        } else {
            "import json,sys,time\nprint(json.dumps(dict(event='ready')),flush=True)\nassert sys.stdin.read()==''\ntime.sleep(.06);print(json.dumps(dict(event='closed',cleanup='stopped-preserved')),flush=True)\ntime.sleep(5)\n"
        };
        let mut controller = ready_child(script);
        let started = Instant::now();
        let result = if normal {
            controller.finish(Duration::from_millis(150)).map(|_| ())
        } else {
            controller.cancel(Duration::from_millis(150)).map(|_| ())
        };
        let error = result.unwrap_err();
        assert!(error.contains("did not exit"), "{error}");
        assert!(error.contains("retains custody"));
        assert!(started.elapsed() < Duration::from_millis(600));
        let retry = Instant::now();
        assert!(controller.cancel(Duration::from_secs(10)).is_err());
        drop(controller);
        assert!(
            retry.elapsed() < Duration::from_millis(300),
            "cleanup restarted its budget"
        );
    }
}

#[cfg(unix)]
#[test]
fn expired_cleanup_discards_partial_frame_without_stale_parser_state() {
    let mut controller = ready_child(
        "import json,sys,time\nprint(json.dumps(dict(event='ready')),flush=True)\nsys.stdout.write('{');sys.stdout.flush();time.sleep(30)\n",
    );
    let error = controller.cancel(Duration::from_millis(50)).unwrap_err();
    assert!(error.contains("retains custody"));
    assert!(
        controller
            .receive(Instant::now() + Duration::from_millis(10))
            .is_err()
    );
    assert!(
        controller
            .send(
                &json!({"private":"fixture"}),
                Instant::now() + Duration::from_millis(10)
            )
            .is_err()
    );
}

#[cfg(target_os = "linux")]
mod custody {
    #![allow(unsafe_code)] // pidfd identity/reaping assertions for owned fixtures.
    use super::*;
    use crate::process_cleanup::test_support;
    use std::fs::File;
    use std::os::fd::{AsRawFd, FromRawFd};

    struct Pinned {
        pid: u32,
        fd: File,
    }
    impl Pinned {
        fn new(pid: u32) -> Self {
            // SAFETY: pin exactly the direct child still held by its owner.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            assert!(fd >= 0);
            Self {
                pid,
                fd: unsafe { File::from_raw_fd(fd) },
            }
        }
        fn assert_reaped(&self) {
            let mut poll = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: only observes the pinned child's exit and reaping.
            assert_eq!(unsafe { libc::poll(&mut poll, 1, 0) }, 1);
            assert_ne!(poll.revents & libc::POLLIN, 0);
            assert_eq!(
                unsafe { libc::waitpid(self.pid as i32, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
    }
    struct Sentinel(Child);
    impl Sentinel {
        fn new() -> Self {
            Self(Command::new("/bin/sleep").arg("30").spawn().unwrap())
        }
        fn assert_alive(&mut self) {
            assert!(self.0.try_wait().unwrap().is_none());
        }
    }
    impl Drop for Sentinel {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn fallback_creation_failure_starts_no_controller_or_extra_worker() {
        let scope = test_support::Scope::new();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/controller-transport-fixtures")
            .join(crate::workloads::TaskIdentity::local("no-spawn", None).attempt_id);
        std::fs::create_dir_all(&root).unwrap();
        let marker = root.join("must-not-run");
        test_support::set_creation_failure(true);
        let mut command = ControllerCommand::new("python3").unwrap();
        command
            .args([
                "-I",
                "-S",
                "-B",
                "-c",
                "import pathlib,sys;pathlib.Path(sys.argv[1]).write_text('spawned')",
            ])
            .arg(&marker);
        let error = Controller::spawn(&mut command).err().unwrap();
        assert!(
            error.contains("cannot establish asb-gate-custody cleanup worker"),
            "{error}"
        );
        assert_eq!(scope.count(), 0);
        assert!(!marker.exists());
    }

    #[test]
    fn pipe_setup_failure_after_spawn_retains_precreated_cleanup_owner() {
        let scope = test_support::Scope::new();
        SETUP_FAILURE.set(true);
        let mut command = ControllerCommand::new("python3").unwrap();
        command.args(["-I", "-S", "-B", "-c", "import sys;sys.stdin.read()"]);
        let result = Controller::spawn(&mut command);
        SETUP_FAILURE.set(false);
        assert!(
            result
                .err()
                .unwrap()
                .contains("cannot configure nonblocking")
        );
        scope.wait_finished();
        assert_eq!(scope.count(), 1);
    }

    #[test]
    fn pending_or_failed_exit_poll_hands_real_child_to_precreated_reaper_without_touching_sentinel()
    {
        let scope = test_support::Scope::new();
        let mut sentinel = Sentinel::new();
        for fault in [PollFault::Pending, PollFault::Error] {
            let mut controller = ready_child(
                "import json,sys,time\nprint(json.dumps(dict(event='ready')),flush=True)\ntime.sleep(30)\n",
            );
            let pinned = Pinned::new(controller.child().id());
            POLL_FAULT.set(Some(fault));
            let started = Instant::now();
            let result = controller.stop_for_test(Duration::from_millis(40));
            POLL_FAULT.set(None);
            let error = result.unwrap_err();
            assert!(error.contains("retains custody"));
            assert!(started.elapsed() < Duration::from_millis(500));
            // The keeper runs even while the emptied Controller remains live.
            scope.wait_finished();
            pinned.assert_reaped();
            sentinel.assert_alive();
            drop(controller);
        }
        assert_eq!(scope.count(), 2);
    }

    #[test]
    fn unwind_after_admission_retains_child_custody_without_new_worker_or_wait() {
        let scope = test_support::Scope::new();
        let mut sentinel = Sentinel::new();
        let mut pinned = None;
        let started = Instant::now();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut controller = ready_child(
                "import json,sys,time\nprint(json.dumps(dict(event='ready')),flush=True)\ntime.sleep(30)\n",
            );
            pinned = Some(Pinned::new(controller.child().id()));
            controller.expire_cleanup_for_test();
            panic!("fixture unwind after controller admission");
        }));
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        scope.wait_finished();
        assert_eq!(scope.count(), 1);
        pinned.unwrap().assert_reaped();
        sentinel.assert_alive();
    }

    #[test]
    fn failed_one_shot_write_and_drop_share_total_deadline_without_extra_cleanup_grace() {
        let scope = test_support::Scope::new();
        let mut controller = ready_child(
            "import json,time\nprint(json.dumps(dict(event='ready')),flush=True)\ntime.sleep(30)\n",
        );
        let pinned = Pinned::new(controller.child().id());
        let started = Instant::now();
        let deadline = started + Duration::from_millis(100);
        controller.limit_cleanup_to(deadline);
        assert!(
            controller
                .send(&json!({"private":"x".repeat(512*1024)}), deadline)
                .is_err()
        );
        drop(controller);
        assert!(started.elapsed() < Duration::from_millis(500));
        scope.wait_finished();
        pinned.assert_reaped();
        assert_eq!(scope.count(), 1);
    }
}
