use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::Condvar;

fn root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/installer-log-core")
        .join(crate::tools::runtime::unique_id());
    fs::create_dir_all(&root).unwrap();
    fs::canonicalize(root).unwrap()
}

struct FaultWriter(Option<i32>);
impl Write for FaultWriter {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        self.0
            .map_or(Ok(0), |code| Err(io::Error::from_raw_os_error(code)))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn ancillary_zero_and_enospc_do_not_claim_retained_bytes_or_complete_logs() {
    for (errno, expected) in [
        (None, io::ErrorKind::WriteZero),
        (Some(libc::ENOSPC), io::ErrorKind::StorageFull),
    ] {
        let root = root();
        let mut sink = LogSink::new(&root, Some(Box::new(move |_| Box::new(FaultWriter(errno)))));
        sink.push(b"inert-private-secret");
        sink.queue.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = sink.report.lock().unwrap().clone();
            if state.settled {
                assert_eq!(
                    state.fault,
                    Some(LogFault {
                        phase: LogPhase::Write,
                        kind: expected
                    })
                );
                assert_eq!(state.retained, 0);
                assert_eq!(state.discarded, b"inert-private-secret".len());
                assert_eq!(fs::metadata(state.path.unwrap()).unwrap().len(), 0);
                // Later queue disconnection/loss cannot replace the first
                // actual write failure's phase or kind.
                sink.push(b"later-discarded");
                assert_eq!(
                    sink.report.lock().unwrap().fault,
                    Some(LogFault {
                        phase: LogPhase::Write,
                        kind: expected
                    })
                );
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn ordinary_log_finish_confirms_writer_completion_before_snapshot() {
    let root = root();
    let sink = LogSink::new(&root, None);
    sink.push(b"ordinary installer output");

    let report = sink.finish();

    assert!(report.settled, "ordinary writer was reported as pending");
    assert_eq!(report.fault, None);
    assert_eq!(report.retained, b"ordinary installer output".len());
    assert_eq!(report.discarded, 0);
    assert_eq!(
        fs::read(report.path.expect("private log path")).unwrap(),
        b"ordinary installer output"
    );
}

#[test]
fn log_namespace_never_overwrites_or_chmods_existing_foreign_entries() {
    let root = root();
    let foreign = root.join(".asb-install-step-foreign");
    fs::write(&foreign, "keep").unwrap();
    fs::set_permissions(&foreign, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(private_log_at(&root, ".asb-install-step-foreign").is_err());
    assert_eq!(fs::read_to_string(&foreign).unwrap(), "keep");
    assert_eq!(
        fs::metadata(&foreign).unwrap().permissions().mode() & 0o777,
        0o644
    );
    symlink(&foreign, root.join(".asb-install-step-link")).unwrap();
    assert!(private_log_at(&root, ".asb-install-step-link").is_err());
    assert!(
        fs::symlink_metadata(root.join(".asb-install-step-link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let project_link = root.join("project-link");
    symlink(&root, &project_link).unwrap();
    assert!(
        private_log(&project_link).is_err(),
        "project symlink was followed for raw logging"
    );
    assert_eq!(fs::read_to_string(foreign).unwrap(), "keep");
}

#[test]
fn private_log_modes_hold_under_umask_022_in_an_isolated_owned_process() {
    const CHILD: &str = "ASB_PRIVATE_INSTALL_LOG_TEST_CHILD";
    if let Some(root) = std::env::var_os(CHILD) {
        // SAFETY: this is an isolated one-test child with its own owned
        // guardian, not the concurrent parent libtest or another process.
        unsafe {
            libc::umask(0o022);
        }
        let (mut file, path) = private_log(Path::new(&root)).unwrap();
        file.write_all(b"inert-private-secret").unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        return;
    }
    let root = root();
    let context = OperationContext::new(&root, &ProjectConfig::default()).unwrap();
    let mut step = PreparedStep::new(
        &context,
        std::env::current_exe().unwrap(),
        Purpose::Probe,
        "private log mode fixture",
    )
    .unwrap();
    step.command_mut().args(["--exact", "install::process::native::tests::private_log_modes_hold_under_umask_022_in_an_isolated_owned_process", "--test-threads=1"])
        .env(CHILD, &root).stdin(Stdio::null());
    assert!(
        step.run(ProducerLease::default())
            .unwrap()
            .status()
            .success()
    );
}

struct PressureWriter {
    file: File,
    reached: Arc<AtomicBool>,
    release: Arc<(Mutex<bool>, Condvar)>,
}
impl Write for PressureWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.reached.store(true, Ordering::Release);
        let (lock, wake) = &*self.release;
        let mut released = lock.lock().unwrap();
        while !*released {
            released = wake.wait(released).unwrap();
        }
        self.file.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
struct ReleasePressure(Arc<(Mutex<bool>, Condvar)>);
impl Drop for ReleasePressure {
    fn drop(&mut self) {
        let (lock, wake) = &*self.0;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
}

#[test]
fn core_review_log_pressure_must_preserve_only_the_first_bytes_prefix() {
    let root = root();
    let reached = Arc::new(AtomicBool::new(false));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let guard = ReleasePressure(Arc::clone(&release));
    let held = Arc::clone(&reached);
    let unblock = Arc::clone(&release);
    let mut sink = LogSink::new(
        &root,
        Some(Box::new(move |file| {
            Box::new(PressureWriter {
                file,
                reached: held,
                release: unblock,
            })
        })),
    );
    sink.push(b"A");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !reached.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    for _ in 0..16 {
        sink.push(b"B");
    }
    sink.push(b"LOST");
    assert_eq!(sink.report.lock().unwrap().accepted, 17);
    assert_eq!(sink.report.lock().unwrap().discarded, 4);
    drop(guard);
    let deadline = Instant::now() + Duration::from_secs(5);
    while sink.report.lock().unwrap().retained != 17 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    // Queue room exists again, but bytes after the first lost byte must not
    // create a falsely contiguous log by appending a later distinct chunk.
    sink.push(b"LATE");
    sink.queue.take();
    let deadline = Instant::now() + Duration::from_secs(5);
    let report = loop {
        let report = sink.report.lock().unwrap().clone();
        if report.settled {
            break report;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    };
    let expected = [b"A".as_slice(), b"BBBBBBBBBBBBBBBB".as_slice()].concat();
    assert_eq!(
        fs::read(report.path.unwrap()).unwrap(),
        expected,
        "logging resumed after loss and no longer represents a true first-bytes prefix"
    );
    assert_eq!(report.accepted, 17);
    assert_eq!(report.retained, 17);
    assert_eq!(report.discarded, 8);
    assert_eq!(
        sink.budget.remaining.load(Ordering::Acquire),
        OPERATION_LOG_BYTES - 17,
        "only definitely rejected queue bytes may return to the quota"
    );
    assert_eq!(
        report.fault,
        Some(LogFault {
            phase: LogPhase::Queue,
            kind: io::ErrorKind::WouldBlock
        })
    );
}

struct FlushFaultWriter(File);
impl Write for FlushFaultWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOSPC))
    }
}

#[test]
fn log_fault_phases_distinguish_namespace_open_start_and_flush() {
    let root = root();
    let foreign = root.join("foreign");
    fs::write(&foreign, "keep foreign bytes").unwrap();
    let namespace = LogSink::new(&foreign, None);
    namespace.push(b"inert-not-retained");
    let namespace = namespace.finish();
    assert!(namespace.settled, "a never-started writer is not pending");
    assert_eq!(namespace.fault.unwrap().phase, LogPhase::Namespace);
    assert_eq!(fs::read(&foreign).unwrap(), b"keep foreign bytes");

    for symlink_collision in [false, true] {
        let attempt = format!(".asb-install-open-{symlink_collision}");
        let target = foreign.clone();
        let fault = private_log_prepared(
            &root,
            &attempt,
            Some(Box::new(move |path| {
                if symlink_collision {
                    symlink(target, path).unwrap();
                } else {
                    fs::write(path, "keep arriving bytes").unwrap();
                }
            })),
        )
        .unwrap_err();
        assert_eq!(
            fault,
            LogFault {
                phase: LogPhase::Open,
                kind: io::ErrorKind::AlreadyExists
            }
        );
        let log = root.join(attempt).join("output.log");
        if symlink_collision {
            assert!(fs::symlink_metadata(log).unwrap().file_type().is_symlink());
            assert_eq!(fs::read(&foreign).unwrap(), b"keep foreign bytes");
        } else {
            assert_eq!(fs::read(log).unwrap(), b"keep arriving bytes");
        }
    }
    let missing_worker = LogSink::new_inner(
        &root,
        Arc::new(LogBudget::default()),
        None,
        Some(io::ErrorKind::Other),
        true,
    );
    missing_worker.push(b"inert-not-written");
    let missing_worker = missing_worker.finish();
    assert!(missing_worker.settled);
    assert_eq!(
        missing_worker.fault,
        Some(LogFault {
            phase: LogPhase::WorkerStart,
            kind: io::ErrorKind::Other
        })
    );
    assert_eq!(missing_worker.retained, 0);

    let mut sink = LogSink::new(
        &root,
        Some(Box::new(|file| Box::new(FlushFaultWriter(file)))),
    );
    sink.push(b"inert-actually-written");
    sink.queue.take();
    let end = Instant::now() + Duration::from_secs(5);
    let report = loop {
        let report = sink.report.lock().unwrap().clone();
        if report.settled {
            break report;
        }
        assert!(Instant::now() < end, "flush fault writer did not settle");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(report.retained, b"inert-actually-written".len());
    assert_eq!(report.discarded, 0);
    assert_eq!(
        report.fault,
        Some(LogFault {
            phase: LogPhase::Flush,
            kind: io::ErrorKind::StorageFull
        })
    );
    let mut output = Vec::new();
    render_log(&report, &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains("log flush failed (StorageFull)"));
    assert!(!output.contains("log write failed"));
    assert!(!output.contains("inert-actually-written"));
}

fn await_log_settlement(report: &Arc<Mutex<LogReport>>) -> LogReport {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let report = report.lock().unwrap().clone();
        if report.settled {
            return report;
        }
        assert!(Instant::now() < end, "actual log writer did not settle");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn operation_writer_budget_survives_snapshots_until_actual_held_writers_exit() {
    let root = root();
    let budget = Arc::new(LogBudget::with_limits(4, 64));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let release_on_failure = ReleasePressure(Arc::clone(&release));
    let mut reports = Vec::new();
    for _ in 0..4 {
        let reached = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&reached);
        let held = Arc::clone(&release);
        let sink = LogSink::new_inner(
            &root,
            Arc::clone(&budget),
            Some(Box::new(move |file| {
                Box::new(PressureWriter {
                    file,
                    reached: flag,
                    release: held,
                })
            })),
            None,
            true,
        );
        sink.push(b"x");
        let end = Instant::now() + Duration::from_secs(5);
        while !reached.load(Ordering::Acquire) {
            assert!(
                Instant::now() < end,
                "held writer did not enter actual write"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        reports.push(Arc::clone(&sink.report));
        assert!(
            !sink.finish().settled,
            "snapshot invented writer completion"
        );
    }
    assert_eq!(budget.active.load(Ordering::Acquire), 4);
    let entries = fs::read_dir(&root).unwrap().count();
    let denied = LogSink::new_inner(&root, Arc::clone(&budget), None, None, true).finish();
    assert!(denied.settled);
    assert_eq!(
        denied.fault,
        Some(LogFault {
            phase: LogPhase::Budget,
            kind: io::ErrorKind::WouldBlock
        })
    );
    assert!(denied.path.is_none());
    assert_eq!(
        fs::read_dir(&root).unwrap().count(),
        entries,
        "denial created a namespace before permit acquisition"
    );
    assert_eq!(budget.active.load(Ordering::Acquire), 4);
    drop(release_on_failure);
    for report in reports {
        let report = await_log_settlement(&report);
        assert_eq!(report.retained, 1);
        assert_eq!(report.fault, None);
    }
    assert_eq!(budget.active.load(Ordering::Acquire), 0);
    let next = LogSink::new_inner(&root, Arc::clone(&budget), None, None, true);
    let report = Arc::clone(&next.report);
    next.push(b"y");
    next.finish();
    assert_eq!(await_log_settlement(&report).retained, 1);
    assert_eq!(budget.active.load(Ordering::Acquire), 0);
    assert_eq!(budget.remaining.load(Ordering::Acquire), 59);
}

#[test]
fn operation_byte_budget_keeps_only_accepted_prefix_and_never_resets_on_writer_exit() {
    let root = root();
    let budget = Arc::new(LogBudget::with_limits(1, 5));
    let first = LogSink::new_inner(&root, Arc::clone(&budget), None, None, true);
    let report = Arc::clone(&first.report);
    first.push(b"1234567");
    first.finish();
    let first = await_log_settlement(&report);
    assert_eq!(fs::read(first.path.unwrap()).unwrap(), b"12345");
    assert_eq!((first.accepted, first.retained, first.discarded), (5, 5, 2));
    assert_eq!(first.fault.unwrap().phase, LogPhase::Budget);
    assert_eq!(budget.remaining.load(Ordering::Acquire), 0);
    assert_eq!(budget.active.load(Ordering::Acquire), 0);
    let next = LogSink::new_inner(&root, Arc::clone(&budget), None, None, true);
    let report = Arc::clone(&next.report);
    next.push(b"later");
    next.finish();
    let next = await_log_settlement(&report);
    assert_eq!((next.accepted, next.retained, next.discarded), (0, 0, 5));
    assert_eq!(fs::read(next.path.unwrap()).unwrap(), b"");
    assert_eq!(next.fault.unwrap().phase, LogPhase::Budget);
    assert_eq!(budget.remaining.load(Ordering::Acquire), 0);
}
