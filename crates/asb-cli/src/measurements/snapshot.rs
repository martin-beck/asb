//! Cancellable, bounded observations; the worker retains any uncertain custody.
use super::{ProcessIdentity, TreeSnapshotArguments, measurement_interval_ms, process_tree_pids};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{
    Arc, Condvar, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

pub(super) struct ProcessTreeSnapshot {
    stop: Arc<AtomicBool>,
    stop_deadline: Arc<Mutex<Option<Instant>>>,
    handle: Option<WorkerHandle>,
}
type WorkerTask = Box<dyn FnOnce() -> Result<(), String> + Send>;
type WorkerHandle = std::thread::JoinHandle<Result<(), String>>;

// A snapshot cancellation can first spend the gate's bounded owned-cleanup
// window. Keep a small scheduler margin so its owner does not report an
// otherwise bounded cleanup as unconfirmed at the same deadline. Dense
// cohorts cancel many pidstat-style subprocesses together, so make that
// margin proportional to the locally active snapshot population while keeping
// a firm upper bound.
const SETTLEMENT_BUDGET: Duration = Duration::from_secs(8);
const SETTLEMENT_SNAPSHOTS_PER_SECOND: usize = 4;
const MAX_SETTLEMENT_BUDGET: Duration = Duration::from_secs(60);
const MAX_CONCURRENT_SNAPSHOT_COMMANDS: usize = 16;
static ACTIVE_SNAPSHOTS: AtomicUsize = AtomicUsize::new(0);
static SNAPSHOT_COMMAND_SLOTS: OnceLock<SnapshotCommandSlots> = OnceLock::new();

struct SnapshotReservation;

impl SnapshotReservation {
    fn acquire() -> Self {
        ACTIVE_SNAPSHOTS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for SnapshotReservation {
    fn drop(&mut self) {
        ACTIVE_SNAPSHOTS.fetch_sub(1, Ordering::AcqRel);
    }
}

fn settlement_budget(active_snapshots: usize) -> Duration {
    let extra_seconds = active_snapshots.div_ceil(SETTLEMENT_SNAPSHOTS_PER_SECOND) as u64;
    SETTLEMENT_BUDGET
        .saturating_add(Duration::from_secs(extra_seconds))
        .min(MAX_SETTLEMENT_BUDGET)
}

fn observation_runtime(requested: Duration, active_snapshots: usize) -> Duration {
    requested.saturating_add(settlement_budget(active_snapshots).saturating_sub(SETTLEMENT_BUDGET))
}

struct SnapshotCommandSlots {
    used: Mutex<usize>,
    available: Condvar,
}

impl SnapshotCommandSlots {
    const fn new() -> Self {
        Self {
            used: Mutex::new(0),
            available: Condvar::new(),
        }
    }

    fn acquire(
        &self,
        cancelled: &AtomicBool,
        deadline: Instant,
    ) -> Result<Option<SnapshotCommandReservation<'_>>, String> {
        let mut used = self.used.lock().unwrap_or_else(|error| error.into_inner());
        while *used >= MAX_CONCURRENT_SNAPSHOT_COMMANDS {
            if cancelled.load(Ordering::Acquire) {
                return Ok(None);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err("process-tree snapshot scheduling deadline exceeded".into());
            }
            let wait = (deadline - now).min(Duration::from_millis(5));
            let (next, _) = self
                .available
                .wait_timeout(used, wait)
                .unwrap_or_else(|error| error.into_inner());
            used = next;
        }
        if cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        *used += 1;
        Ok(Some(SnapshotCommandReservation { slots: self }))
    }
}

struct SnapshotCommandReservation<'a> {
    slots: &'a SnapshotCommandSlots,
}

impl Drop for SnapshotCommandReservation<'_> {
    fn drop(&mut self) {
        let mut used = self
            .slots
            .used
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *used = used.saturating_sub(1);
        self.slots.available.notify_one();
    }
}

fn snapshot_command_slots() -> &'static SnapshotCommandSlots {
    SNAPSHOT_COMMAND_SLOTS.get_or_init(SnapshotCommandSlots::new)
}

fn record_stop_deadline(slot: &Mutex<Option<Instant>>, deadline: Instant) {
    let mut slot = slot.lock().unwrap_or_else(|error| error.into_inner());
    if slot.is_none_or(|current| deadline < current) {
        *slot = Some(deadline);
    }
}

fn stop_deadline(slot: Option<&Mutex<Option<Instant>>>) -> Instant {
    slot.and_then(|slot| *slot.lock().unwrap_or_else(|error| error.into_inner()))
        .unwrap_or_else(|| Instant::now() + SETTLEMENT_BUDGET)
}

fn spawn_worker(task: WorkerTask) -> io::Result<WorkerHandle> {
    std::thread::Builder::new()
        .name("asb-process-tree-snapshot".into())
        .spawn(task)
}

fn wait_for_execution_start(
    root: ProcessIdentity,
    cancelled: &AtomicBool,
    execution_started: &AtomicBool,
) -> bool {
    while !cancelled.load(Ordering::Acquire)
        && !execution_started.load(Ordering::Acquire)
        && root.matches()
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    !cancelled.load(Ordering::Acquire)
        && execution_started.load(Ordering::Acquire)
        && root.matches()
}

impl ProcessTreeSnapshot {
    pub(super) fn start(
        root: ProcessIdentity,
        executable: PathBuf,
        arguments: TreeSnapshotArguments,
        execution_started: Arc<AtomicBool>,
        stdout: File,
        stderr: File,
    ) -> Option<Self> {
        Self::start_with_spawn(
            root,
            executable,
            arguments,
            execution_started,
            stdout,
            stderr,
            spawn_worker,
        )
    }
    pub(super) fn start_with_spawn(
        root: ProcessIdentity,
        executable: PathBuf,
        arguments: TreeSnapshotArguments,
        execution_started: Arc<AtomicBool>,
        mut stdout: File,
        mut stderr: File,
        spawn: impl FnOnce(WorkerTask) -> io::Result<WorkerHandle>,
    ) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&stop);
        let stop_deadline = Arc::new(Mutex::new(None));
        let cancellation_deadline = Arc::clone(&stop_deadline);
        let reservation = SnapshotReservation::acquire();
        let handle = spawn(Box::new(move || {
            let _reservation = reservation;
            // A cohort attaches collectors to a stopped target before the
            // shared launch rendezvous. Do not let that admission interval
            // become a pidstat-style measurement sample; native execution
            // has already emitted this event before attaching, so it starts
            // immediately on the existing path.
            if !wait_for_execution_start(root, &cancelled, &execution_started) {
                return Ok(());
            }
            let started = Instant::now();
            while !cancelled.load(Ordering::Acquire) && root.matches() {
                let members = process_tree_pids(root.pid)
                    .into_iter()
                    .filter_map(ProcessIdentity::read)
                    .filter(|member| member.in_tree(root))
                    .collect::<Vec<_>>();
                if !root.matches() || members.is_empty() {
                    break;
                }
                let pids = members.iter().map(|member| member.pid).collect::<Vec<_>>();
                let request = arguments(&pids);
                let Some(observation) = observe(
                    &executable,
                    request,
                    &cancelled,
                    Some(&cancellation_deadline),
                    false,
                )?
                else {
                    break;
                };
                if !root.matches() || members.iter().any(|member| !member.in_tree(root)) {
                    continue;
                }
                write_observation(&mut stdout, &mut stderr, &observation, started.elapsed())?;
                let deadline = Instant::now() + Duration::from_millis(measurement_interval_ms());
                while !cancelled.load(Ordering::Acquire) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            Ok(())
        }))
        .ok()?;
        Some(Self {
            stop,
            stop_deadline,
            handle: Some(handle),
        })
    }
    #[cfg(test)]
    pub(super) fn stop(mut self) -> Result<(), String> {
        self.settle()
    }
    pub(super) fn request_stop(&self) {
        self.request_stop_until(Instant::now() + Self::stop_budget());
    }
    fn request_stop_until(&self, deadline: Instant) {
        record_stop_deadline(&self.stop_deadline, deadline);
        self.stop.store(true, Ordering::Release);
    }
    pub(super) fn stop_budget() -> Duration {
        settlement_budget(ACTIVE_SNAPSHOTS.load(Ordering::Acquire))
    }
    pub(super) fn stop_before_with_owner(
        mut self,
        deadline: Instant,
        owner: Option<Arc<crate::output_lock::OutputLock>>,
    ) -> Result<(), String> {
        self.settle_before(deadline, owner)
    }
    #[cfg(test)]
    pub(super) fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .is_none_or(|handle| handle.is_finished())
    }
    fn settle(&mut self) -> Result<(), String> {
        self.settle_before(Instant::now() + Self::stop_budget(), None)
    }
    fn settle_before(
        &mut self,
        deadline: Instant,
        owner: Option<Arc<crate::output_lock::OutputLock>>,
    ) -> Result<(), String> {
        self.request_stop_until(deadline);
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !handle.is_finished() {
            super::retain_owner_until_worker_exit(handle, owner);
            // The existing worker still owns its files and guardian. Do not
            // join it, publish a complete capture, or create another worker.
            return Err("process-tree snapshot settlement is unconfirmed; existing worker retains producer custody".into());
        }
        handle
            .join()
            .map_err(|_| "process-tree snapshot worker failed")?
    }
}

#[cfg(test)]
#[test]
fn process_tree_snapshot_waits_for_execution_begin() {
    let root = ProcessIdentity::read(std::process::id()).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let execution_started = Arc::new(AtomicBool::new(false));
    let notified = Arc::new(std::sync::Barrier::new(2));
    let setter = {
        let execution_started = Arc::clone(&execution_started);
        let notified = Arc::clone(&notified);
        std::thread::spawn(move || {
            notified.wait();
            std::thread::sleep(Duration::from_millis(20));
            execution_started.store(true, Ordering::Release);
        })
    };
    notified.wait();
    let started = Instant::now();
    assert!(wait_for_execution_start(
        root,
        &cancelled,
        &execution_started
    ));
    assert!(started.elapsed() >= Duration::from_millis(10));
    setter.join().unwrap();
}
impl Drop for ProcessTreeSnapshot {
    fn drop(&mut self) {
        let _ = self.settle();
    }
}

/// Generic command limits are supplied by the owning adapter, never inferred
/// from a collector ID in the measurement runtime.
pub(crate) struct SnapshotCommand {
    pub arguments: Vec<String>,
    pub runtime: Duration,
    pub stream_bytes: usize,
}

/// Bounded, owned capability discovery. Unsupported hosts return unavailable.
pub(crate) fn bounded_command(
    executable: &std::path::Path,
    request: SnapshotCommand,
) -> Option<(Vec<u8>, Vec<u8>)> {
    #[cfg(target_os = "linux")]
    {
        let result = observe(executable, request, &AtomicBool::new(false), None, true).ok()??;
        Some((result.stdout, result.stderr))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (executable, request);
        None
    }
}

pub(crate) fn bounded_version(
    executable: &std::path::Path,
    argument: &str,
    budget: Duration,
) -> Option<(Vec<u8>, Vec<u8>)> {
    #[cfg(target_os = "linux")]
    {
        let request = SnapshotCommand {
            arguments: vec![argument.to_owned()],
            runtime: budget,
            stream_bytes: 8192,
        };
        let result = observe(executable, request, &AtomicBool::new(false), None, true).ok()??;
        Some((result.stdout, result.stderr))
    }
    #[cfg(not(target_os = "linux"))]
    {
        crate::runner::portable_version_capture(executable, argument, budget)
    }
}

struct Observation {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn write_observation(
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    observation: &Observation,
    elapsed: Duration,
) -> Result<(), String> {
    writeln!(
        stdout,
        "# asb-snapshot-elapsed-seconds {:.6}",
        elapsed.as_secs_f64()
    )
    .map_err(|error| format!("cannot write process-tree snapshot timestamp: {error}"))?;
    stdout
        .write_all(&observation.stdout)
        .map_err(|error| format!("cannot write process-tree snapshot stdout: {error}"))?;
    stderr
        .write_all(&observation.stderr)
        .map_err(|error| format!("cannot write process-tree snapshot stderr: {error}"))?;
    stdout
        .flush()
        .map_err(|error| format!("cannot flush process-tree snapshot stdout: {error}"))?;
    stderr
        .flush()
        .map_err(|error| format!("cannot flush process-tree snapshot stderr: {error}"))
}

/// One fair, bounded turn. Interrupted calls consume attempts as well as
/// checking the same cancellation/deadline; they cannot monopolize the pump.
#[cfg(target_os = "linux")]
fn read_turn(
    pipe: &mut dyn Read,
    output: &mut Vec<u8>,
    eof: &mut bool,
    limit: usize,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    let mut bytes = 0;
    for _ in 0..32 {
        if *eof || bytes >= 64 * 1024 || cancelled.load(Ordering::Acquire) {
            break;
        }
        if Instant::now() >= deadline {
            return Err("process-tree snapshot output read deadline exceeded".into());
        }
        let mut buffer = [0; 4096];
        match pipe.read(&mut buffer) {
            Ok(0) => *eof = true,
            Ok(length) => {
                if output.len() + length > limit {
                    return Err("process-tree snapshot output exceeded its capture limit".into());
                }
                output.extend_from_slice(&buffer[..length]);
                bytes += length;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(format!("cannot read process-tree snapshot output: {error}")),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn runtime() -> io::Result<PathBuf> {
    #[cfg(test)]
    return Ok(std::env::current_exe()?
        .parent()
        .ok_or_else(|| io::Error::other("missing test binary parent"))?
        .parent()
        .ok_or_else(|| io::Error::other("missing actual ASB binary parent"))?
        .join("asb"));
    #[cfg(not(test))]
    crate::process_gate::executable()
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub(super) fn nonblocking(pipe: &impl std::os::fd::AsRawFd) -> io::Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn observe(
    executable: &std::path::Path,
    request: SnapshotCommand,
    cancelled: &AtomicBool,
    cancellation_deadline: Option<&Mutex<Option<Instant>>>,
    no_auto_install: bool,
) -> Result<Option<Observation>, String> {
    if request.runtime.is_zero()
        || request.runtime > Duration::from_secs(3600)
        || request.stream_bytes == 0
        || request.stream_bytes > 256 * 1024 * 1024
    {
        return Err("process-tree snapshot command has invalid capture limits".into());
    }
    if cancelled.load(Ordering::Acquire) {
        return Ok(None);
    }
    let deadline = Instant::now()
        + observation_runtime(request.runtime, ACTIVE_SNAPSHOTS.load(Ordering::Acquire));
    // Limit only the short-lived native sampling command. Waiting workers
    // remain cancellable, so a dense cohort cannot turn shutdown into hundreds
    // of simultaneous gate cancellations.
    let Some(_slot) = snapshot_command_slots().acquire(cancelled, deadline)? else {
        return Ok(None);
    };
    let mut launch = crate::launch::ProcessLaunch::new(executable);
    for argument in &request.arguments {
        launch.arg(argument);
    }
    let (mut command, route) = launch.native(Some(&runtime().map_err(|error| error.to_string())?));
    if no_auto_install {
        command.env("RUSTUP_AUTO_INSTALL", "0");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Drop the gate before its capture pipes on every return path. Its
    // shutdown may still write diagnostics while settling the owned tree.
    let (mut out, mut err);
    let mut gate = crate::process_gate::Admission::with_principal_control(
        route
            .validate_gate(&mut command)
            .map_err(|error| error.to_string())?,
    )
    .and_then(|admission| admission.spawn())
    .map_err(|error| error.to_string())?;
    out = gate.take_stdout().ok_or("missing snapshot stdout")?;
    err = gate.take_stderr().ok_or("missing snapshot stderr")?;
    nonblocking(&out)
        .and_then(|()| nonblocking(&err))
        .map_err(|error| error.to_string())?;
    gate.resume().map_err(|error| error.to_string())?;
    let mut output = [Vec::new(), Vec::new()];
    let mut eof = [false; 2];
    let mut exited = false;
    let mut drain = None;
    loop {
        if cancelled.load(Ordering::Acquire) {
            if !exited {
                let cleanup_deadline = stop_deadline(cancellation_deadline);
                let grace = std::cmp::min(
                    Instant::now() + Duration::from_millis(100),
                    cleanup_deadline,
                );
                let _ = gate.interrupt_principal(grace);
                while Instant::now() < grace {
                    if gate
                        .try_wait()
                        .map_err(|error| error.to_string())?
                        .is_some()
                    {
                        exited = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                // Delivery acknowledgement is never used as quiescence proof.
                let _acknowledged = gate.interruption_acknowledged();
                if !exited {
                    gate.stop_until(cleanup_deadline)
                        .map_err(|error| error.to_string())?;
                }
            }
            return Ok(None);
        }
        if !exited
            && gate
                .try_wait()
                .map_err(|error| error.to_string())?
                .is_some()
        {
            let status = gate
                .principal_exit()
                .ok_or("snapshot principal termination is unknown")?
                .status();
            if !status.success() {
                return Err("process-tree snapshot command failed".into());
            }
            exited = true;
            drain = Some(Instant::now() + Duration::from_secs(1));
        }
        if !exited && Instant::now() >= deadline {
            gate.stop().map_err(|error| {
                format!("process-tree snapshot command exceeded its deadline; cleanup is unconfirmed: {error}")
            })?;
            return Err("process-tree snapshot command exceeded its deadline".into());
        }
        for (index, pipe) in [
            (0, &mut out as &mut dyn Read),
            (1, &mut err as &mut dyn Read),
        ] {
            read_turn(
                pipe,
                &mut output[index],
                &mut eof[index],
                request.stream_bytes,
                drain.unwrap_or(deadline),
                cancelled,
            )?;
        }
        if cancelled.load(Ordering::Acquire) {
            continue;
        }
        if exited && eof.iter().all(|eof| *eof) {
            let [out, err] = output;
            return Ok(Some(Observation {
                stdout: out,
                stderr: err,
            }));
        }
        if drain.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(
                "process-tree snapshot output remained open after confirmed tree quiescence".into(),
            );
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod io_tests {
    use super::*;
    struct Reader<'a> {
        calls: usize,
        cancel: Option<&'a AtomicBool>,
        error: io::ErrorKind,
    }
    impl Read for Reader<'_> {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            if let Some(cancel) = self.cancel {
                cancel.store(true, Ordering::Release);
            }
            Err(io::Error::new(self.error, "fixture read failure"))
        }
    }
    #[test]
    fn interrupted_reads_yield_and_recheck_cancel_and_shared_deadline() {
        let cancelled = AtomicBool::new(false);
        let mut bytes = Vec::new();
        let mut eof = false;
        let mut reader = Reader {
            calls: 0,
            cancel: None,
            error: io::ErrorKind::Interrupted,
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        read_turn(
            &mut reader,
            &mut bytes,
            &mut eof,
            1024,
            deadline,
            &cancelled,
        )
        .unwrap();
        assert_eq!(reader.calls, 32, "Interrupted must consume a fair turn");
        assert!(!eof);
        assert!(bytes.is_empty());
        reader.cancel = Some(&cancelled);
        read_turn(
            &mut reader,
            &mut bytes,
            &mut eof,
            1024,
            deadline,
            &cancelled,
        )
        .unwrap();
        assert_eq!(reader.calls, 33, "cancellation must interrupt retry");
        cancelled.store(false, Ordering::Release);
        reader.cancel = None;
        let error = read_turn(
            &mut reader,
            &mut bytes,
            &mut eof,
            1024,
            Instant::now(),
            &cancelled,
        )
        .unwrap_err();
        assert!(error.contains("deadline"));
        assert_eq!(
            reader.calls, 33,
            "expired budget must never perform another read"
        );
    }

    #[test]
    fn cancellation_deadline_is_pinned_before_the_worker_observes_stop() {
        let deadlines = Mutex::new(None);
        let outer = Instant::now() + Duration::from_millis(40);
        record_stop_deadline(&deadlines, outer);
        record_stop_deadline(&deadlines, Instant::now() + Duration::from_secs(8));
        assert_eq!(stop_deadline(Some(&deadlines)), outer);
    }

    #[test]
    fn snapshot_settlement_budget_scales_for_dense_cohorts_and_stays_bounded() {
        assert_eq!(settlement_budget(0), Duration::from_secs(8));
        assert_eq!(settlement_budget(4), Duration::from_secs(9));
        assert_eq!(settlement_budget(128), Duration::from_secs(40));
        assert_eq!(settlement_budget(256), Duration::from_secs(60));
        assert_eq!(settlement_budget(1_000), Duration::from_secs(60));
    }

    #[test]
    fn snapshot_observation_runtime_has_the_same_bounded_dense_queue_allowance() {
        assert_eq!(
            observation_runtime(Duration::from_secs(6), 0),
            Duration::from_secs(6)
        );
        assert_eq!(
            observation_runtime(Duration::from_secs(6), 128),
            Duration::from_secs(38)
        );
        assert_eq!(
            observation_runtime(Duration::from_secs(6), 256),
            Duration::from_secs(58)
        );
    }

    #[test]
    fn queued_snapshot_command_cancels_before_acquiring_a_slot() {
        let slots = SnapshotCommandSlots::new();
        let cancelled = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_SNAPSHOT_COMMANDS {
            held.push(slots.acquire(&cancelled, deadline).unwrap().unwrap());
        }
        cancelled.store(true, Ordering::Release);
        assert!(slots.acquire(&cancelled, deadline).unwrap().is_none());
        drop(held);
    }

    #[test]
    fn timed_out_snapshot_settles_gate_before_closing_capture_pipes() {
        let error = match observe(
            std::path::Path::new("/bin/sleep"),
            SnapshotCommand {
                arguments: vec!["10".to_owned()],
                runtime: Duration::from_millis(100),
                stream_bytes: 8192,
            },
            &AtomicBool::new(false),
            None,
            true,
        ) {
            Err(error) => error,
            Ok(_) => panic!("a sleeping snapshot must time out"),
        };
        assert_eq!(error, "process-tree snapshot command exceeded its deadline");
    }

    #[test]
    fn permanent_read_error_cannot_be_complete_eof_even_after_a_valid_prefix() {
        let mut reader = Reader {
            calls: 0,
            cancel: None,
            error: io::ErrorKind::PermissionDenied,
        };
        let mut bytes = b"valid prefix\n".to_vec();
        let mut eof = false;
        let error = read_turn(
            &mut reader,
            &mut bytes,
            &mut eof,
            1024,
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.contains("fixture read failure"));
        assert!(!eof);
        assert_eq!(bytes, b"valid prefix\n");
    }
    #[test]
    fn actual_write_zero_is_a_failed_observation_not_silent_completion() {
        struct Zero;
        impl Write for Zero {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Ok(0)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let observation = Observation {
            stdout: b"valid prefix\n".to_vec(),
            stderr: Vec::new(),
        };
        let error = write_observation(&mut Zero, &mut Vec::new(), &observation, Duration::ZERO)
            .unwrap_err();
        assert!(error.contains("write"));
    }
}

#[cfg(not(target_os = "linux"))]
fn observe(
    _: &std::path::Path,
    _: SnapshotCommand,
    _: &AtomicBool,
    _: Option<&Mutex<Option<Instant>>>,
    _: bool,
) -> Result<Option<Observation>, String> {
    Err("bounded process-tree snapshot supervision requires Linux native gate support".into())
}

#[cfg(all(test, target_os = "linux"))]
mod capability_tests {
    use super::*;

    #[test]
    fn discovery_is_owned_and_rejects_partial_oversized_failed_or_slow_output() {
        let request = |script: &str, runtime| SnapshotCommand {
            arguments: vec![
                "-I".into(),
                "-S".into(),
                "-B".into(),
                "-c".into(),
                script.into(),
            ],
            runtime,
            stream_bytes: 128,
        };
        let python = std::path::Path::new("python3");
        let output = bounded_command(
            python,
            request("print('cycles\\ninstructions')", Duration::from_secs(2)),
        )
        .unwrap();
        assert_eq!(output.0, b"cycles\ninstructions\n");
        assert!(output.1.is_empty());
        for script in [
            "print('cycles'); raise SystemExit(1)",
            "print('x'*129)",
            "import sys; sys.stderr.write('x'*129)",
            "import time; print('cycles',flush=True); time.sleep(5)",
        ] {
            assert!(
                bounded_command(
                    python,
                    request(
                        script,
                        if script.contains("sleep") {
                            Duration::from_millis(200)
                        } else {
                            Duration::from_secs(2)
                        }
                    )
                )
                .is_none(),
                "{script}"
            );
        }
    }
}
