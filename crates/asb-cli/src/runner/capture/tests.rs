use super::*;
use std::process::Stdio;

fn child(script: &str) -> OwnedChild {
    OwnedChild::spawn_direct(
        Command::new("/bin/sh")
            .args(["-c", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn shared_launch_failure_aborts_peers_before_stopping_its_owned_child() {
    struct Admission {
        pid: u32,
        aborted: std::cell::Cell<bool>,
    }
    impl crate::workloads::session::LaunchReadiness for Admission {
        fn ready(&self) -> Result<(), String> {
            unreachable!("this fixture fails before admission")
        }
        fn remaining(&self) -> Result<Duration, String> {
            unreachable!("this fixture fails before admission")
        }
        fn abort(&self) {
            let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.pid)).unwrap();
            let state = stat
                .rsplit_once(") ")
                .unwrap()
                .1
                .split_whitespace()
                .next()
                .unwrap();
            assert_ne!(state, "Z", "peers must be notified before child cleanup");
            self.aborted.set(true);
        }
    }
    let mut owned = child("exec sleep 30");
    let admission = Admission {
        pid: owned.id(),
        aborted: std::cell::Cell::new(false),
    };
    let error =
        super::super::native_launch_failure(&mut owned, Some(&admission), "fixture launch failure");
    assert!(admission.aborted.get());
    assert_eq!(error.to_string(), "fixture launch failure");
    assert!(owned.try_wait().unwrap().is_some());
}

#[test]
fn dual_pipe_pressure_is_drained_with_bounded_capture_and_preserved_exit() {
    let child = OwnedChild::spawn_direct(
        Command::new("python3")
            .args([
                "-c",
                "import os\nfor i in range(600):\n os.write(1,b'x'*16384);os.write(2,b'y'*16384)\n",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .unwrap();
    let mut calls = 0;
    let result = capture_with_output_streams(
        child,
        false,
        &mut io::sink(),
        None,
        Duration::from_secs(10),
        PresentationPolicy::PreserveEvidence,
        &mut || {
            calls += 1;
            Ok(())
        },
    )
    .unwrap();
    assert!(result.output.status.success());
    assert_eq!(result.output.stdout.len(), CAPTURE_LIMIT);
    assert_eq!(result.output.stderr.len(), CAPTURE_LIMIT);
    assert!(
        result
            .capture_error
            .as_deref()
            .is_some_and(|error| error.contains("capture limit"))
    );
    assert!(result.only_transcript_truncated());
    assert_eq!(calls, 1);
}

#[test]
fn progress_receives_incremental_transcript_and_is_finished() {
    #[derive(Default)]
    struct Spy {
        updates: usize,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        finished: bool,
    }
    impl Progress for Spy {
        fn update(&mut self, stdout: &[u8], stderr: &[u8]) {
            self.updates += 1;
            self.stdout = stdout.into();
            self.stderr = stderr.into();
        }
        fn finish(&mut self) {
            self.finished = true;
        }
    }

    let mut spy = Spy::default();
    let output = capture_with_output_streams_progress(
        child("printf step; printf token >&2"),
        false,
        &mut io::sink(),
        None,
        Duration::from_secs(1),
        PresentationPolicy::PreserveEvidence,
        Some(&mut spy),
        &mut || Ok(()),
    )
    .unwrap();
    assert!(output.output.status.success());
    assert!(spy.updates > 0);
    assert_eq!(spy.stdout, b"step");
    assert_eq!(spy.stderr, b"token");
    assert!(spy.finished);
}

struct ScriptedRead {
    interrupted: bool,
    failed: bool,
}
impl Read for ScriptedRead {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if !self.interrupted {
            self.interrupted = true;
            return Err(io::ErrorKind::Interrupted.into());
        }
        if !self.failed {
            self.failed = true;
            buffer[0] = b'X';
            return Ok(1);
        }
        Err(io::Error::other("deterministic pipe failure"))
    }
}
impl Pipe for ScriptedRead {
    fn prepare(&self) -> io::Result<()> {
        Ok(())
    }
    fn available(&self) -> io::Result<Option<usize>> {
        Ok(None)
    }
}

#[test]
fn interrupted_reads_retry_but_permanent_failure_is_not_eof() {
    let mut stream = Stream::new(
        ScriptedRead {
            interrupted: false,
            failed: false,
        },
        false,
    )
    .unwrap();
    let error = stream.read_batch(|_| Ok(())).unwrap_err();
    assert!(error.to_string().contains("deterministic pipe failure"));
    assert_eq!(stream.bytes, b"X");
    assert!(stream.failed);
    assert!(!stream.eof);
}

#[test]
fn irregular_short_reads_cannot_overallocate_the_aggregate_capture_budget() {
    let mut capture = Vec::new();
    let chunk = [b'x'; 3000];
    let mut truncated = false;
    for _ in 0..4000 {
        truncated |= append_prefix(&mut capture, &chunk);
        assert!(capture.capacity() <= CAPTURE_LIMIT);
    }
    assert_eq!(capture.len(), CAPTURE_LIMIT);
    assert!(truncated);
}

#[test]
#[cfg(target_os = "linux")]
fn postexit_held_descriptor_has_a_drain_deadline_and_freezes_clock_once() {
    const FIXTURE: &str = "ASB_CAPTURE_HELD_PIPE_FIXTURE";
    if std::env::var_os(FIXTURE).is_none() {
        let result = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runner::capture::tests::postexit_held_descriptor_has_a_drain_deadline_and_freezes_clock_once", "--nocapture"])
            .env(FIXTURE, "1")
            .output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    // An exclusive fixture supervisor adopts and reaps the descriptor holder;
    // never establish process-wide descendant custody in the shared test runner.
    let mut descendants = crate::process_gate::DescendantScope::new().unwrap();
    let started = Instant::now();
    let mut ended = None;
    let result = capture_with_output_streams(
        child("sleep 30 & echo $!; exit 0"),
        false,
        &mut io::sink(),
        None,
        Duration::from_secs(3),
        PresentationPolicy::PreserveEvidence,
        &mut || {
            assert!(ended.is_none());
            ended = Some(Instant::now());
            Ok(())
        },
    )
    .unwrap();
    assert!(result.output.status.success());
    assert!(result.capture_error.unwrap().contains("drain deadline"));
    assert!(ended.unwrap().duration_since(started) < Duration::from_millis(900));
    assert!(started.elapsed() < Duration::from_secs(3));
    let pid = std::str::from_utf8(&result.output.stdout).unwrap().trim();
    descendants.quiesce().unwrap();
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
}

#[test]
fn closed_pipes_never_bypass_the_execution_deadline() {
    let started = Instant::now();
    let result = capture_with_output_streams(
        child("exec 1>&- 2>&-; exec sleep 30"),
        false,
        &mut io::sink(),
        None,
        Duration::from_millis(40),
        PresentationPolicy::PreserveEvidence,
        &mut || Ok(()),
    )
    .unwrap();
    assert!(!result.output.status.success());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        String::from_utf8(result.output.stderr)
            .unwrap()
            .contains("exceeded")
    );
}

#[test]
fn presentation_error_preserves_evidence_and_unwind_settles_owned_child() {
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("fixture presentation closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let result = capture_with_output_streams(
        child("printf first; printf second >&2"),
        true,
        &mut Broken,
        None,
        Duration::from_secs(2),
        PresentationPolicy::PreserveEvidence,
        &mut || Ok(()),
    )
    .unwrap();
    assert!(result.output_error.unwrap().contains("presentation closed"));
    assert!(result.capture_error.is_none());
    assert_eq!(result.output.stdout, b"first");
    assert_eq!(result.output.stderr, b"second");
    struct Panic;
    impl Write for Panic {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            panic!("fixture unwind")
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let owned = child("printf ready; exec sleep 30");
    let pid = owned.id();
    let mut sentinel = child("exec sleep 30");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        capture_with_output_streams(
            owned,
            true,
            &mut Panic,
            None,
            Duration::from_secs(2),
            PresentationPolicy::PreserveEvidence,
            &mut || Ok(()),
        )
    }));
    assert!(result.is_err());
    #[cfg(target_os = "linux")]
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    assert!(sentinel.try_wait().unwrap().is_none());
    sentinel.stop().unwrap();
}

#[cfg(unix)]
#[test]
fn owned_guest_transport_deadline_cancels_child_and_restores_shared_flags() {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    let (source, _blocked_peer) = UnixStream::pair().unwrap();
    let flags = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_GETFL) };
    let started = Instant::now();
    {
        let mut writer =
            transport::GuestTransport::new(&source, started + Duration::from_millis(150)).unwrap();
        let owned = child("while :; do printf 'pressurepressurepressurepressure'; done");
        let pid = owned.id();
        let error = capture_with_output_streams(
            owned,
            true,
            &mut writer,
            None,
            Duration::from_secs(5),
            PresentationPolicy::CancelOnTransportFailure,
            &mut || Ok(()),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("transport deadline"));
        #[cfg(target_os = "linux")]
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        unsafe { libc::fcntl(source.as_raw_fd(), libc::F_GETFL) },
        flags
    );
}

#[cfg(unix)]
#[test]
fn owned_transport_rejects_nonpipes_without_changing_descriptor_flags() {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::options()
        .write(true)
        .open("/dev/null")
        .unwrap();
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    let error = transport::GuestTransport::new(&file, Instant::now())
        .err()
        .unwrap();
    assert!(error.to_string().contains("controller pipes"));
    assert_eq!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) },
        flags
    );
}

#[cfg(target_os = "linux")]
#[test]
fn direct_custody_is_established_before_spawn() {
    let scope = crate::process_cleanup::test_support::Scope::new();
    crate::process_cleanup::test_support::set_creation_failure(true);
    let error = OwnedChild::spawn_direct(Command::new("/bin/sh").args(["-c", "exit 99"]))
        .err()
        .unwrap();
    assert!(error.to_string().contains("cleanup worker"));
    assert_eq!(scope.count(), 0);
    crate::process_cleanup::test_support::set_creation_failure(false);
}

#[cfg(target_os = "linux")]
#[test]
fn uncertain_direct_cleanup_retains_preestablished_custody_and_primary_error() {
    for fail in [true, false] {
        let receipts = crate::process_cleanup::test_support::Scope::new();
        let mut sentinel = child("exec sleep 30");
        let owned = child("printf ready; exec sleep 30");
        let pid = owned.id();
        let fault = test_support::Scope::stop_fault(fail);
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("fixture primary transport failure"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let started = Instant::now();
        let error = capture_with_output_streams(
            owned,
            true,
            &mut Broken,
            None,
            Duration::from_secs(10),
            PresentationPolicy::CancelOnTransportFailure,
            &mut || Ok(()),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("primary transport failure"));
        assert!(error.contains("cleanup also failed"));
        assert!(error.contains("retained in cleanup custody"));
        assert!(started.elapsed() < Duration::from_secs(8));
        drop(fault);
        assert!(sentinel.try_wait().unwrap().is_none());
        sentinel.stop().unwrap();
        drop(sentinel);
        receipts.wait_finished();
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn uncertain_drop_never_writes_to_a_full_stderr_pipe() {
    const FIXTURE: &str = "ASB_CAPTURE_FULL_STDERR_FIXTURE";
    if std::env::var_os(FIXTURE).is_none() {
        let mut fixture = OwnedChild::spawn_direct(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runner::capture::tests::uncertain_drop_never_writes_to_a_full_stderr_pipe",
                    "--nocapture",
                ])
                .env(FIXTURE, "1")
                .stdout(Stdio::null())
                .stderr(Stdio::piped()),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = fixture.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            if Instant::now() >= deadline {
                fixture.stop().unwrap();
                panic!("uncertain destructor blocked on full stderr");
            }
            std::thread::sleep(TICK);
        }
    }
    // This exclusive child's stderr is a private fixture pipe, not a terminal
    // or shared host description. Fill it, then restore blocking mode before Drop.
    let receipts = crate::process_cleanup::test_support::Scope::new();
    let owned = child("exec sleep 30");
    let flags = unsafe { libc::fcntl(2, libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(2, libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let buffer = [b'x'; 4096];
    loop {
        let size = unsafe { libc::write(2, buffer.as_ptr().cast(), buffer.len()) };
        if size < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            break;
        }
    }
    assert_eq!(unsafe { libc::fcntl(2, libc::F_SETFL, flags) }, 0);
    let fault = test_support::Scope::stop_fault(true);
    drop(owned);
    drop(fault);
    receipts.wait_finished();
    std::process::exit(0);
}
